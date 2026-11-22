//! The machine's CPUs, which the provider shares out between runs: each run holds whole cores of
//! its own from its reservation until its end, and each instance of each of its roles a share of
//! them, which the instance's container is confined to.
//!
//! What runs hold is kept in the engine's ledger, a directory that every run on the engine
//! shares, whoever starts it, with an entry for each run that has reserved, locked by the process
//! that holds the reservation. The kernel releases a run's CPUs once that process has ended,
//! however it ends, and no process runs to keep the ledger. A reserve holds the ledger's own lock
//! while it reads what the locked entries hold and writes its own, so two reserves never choose
//! the same CPUs.
//!
//! Its limits: only runs that keep the ledger hold CPUs, so other containers run on any of them; a
//! container that outlives its run's process, while the engine removes it, may run on CPUs that a
//! later run holds; two engines on one machine, as a rootful and a rootless one, each share out
//! all its CPUs; and any user of the machine may add to a ledger, or hold an entry of their own,
//! so it keeps runs apart, not users.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions, Permissions, TryLockError};
use std::io::ErrorKind;
use std::num::NonZeroU64;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use provider::{Reservation, Role, RunTag};
use schema::ir::{CpuFlag, MILLIS_PER_CPU, Name, Resources};
use serde::{Deserialize, Serialize};
use stdx::Context;

// A directory for each engine, by its ID, holding an entry for each run on it.
//
// Not /dev/shm, where logind, with `RemoveIPC=yes`, its default, unlinks a user's files once their
// last session ends, though a driver started outside a session, as from cron, still holds its
// entry's lock. An entry that outlives its holder, as a reboot leaves it, is unlocked, so the next
// reserve removes it:
// https://www.freedesktop.org/software/systemd/man/latest/logind.conf.html#RemoveIPC=
const LEDGERS: &str = "/var/tmp/pumpkin-cpus";

// Every user may make an entry, and the sticky bit keeps one user from removing another's.
const LEDGER_MODE: u32 = 0o1777;

// Whatever the umask, every user's reserve can read and lock an entry, and only its owner writes
// it.
const ENTRY_MODE: u32 = 0o644;

const ONLINE: &str = "/sys/devices/system/cpu/online";

const CPUINFO: &str = "/proc/cpuinfo";

/// A set of CPUs, which is written as Linux does: `0-3,8`. One read from a ledger's entry names at
/// least one CPU.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub(crate) struct CpuList(BTreeSet<u32>);

impl TryFrom<String> for CpuList {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        let cpus = Self::parse(&text)?;
        if cpus.0.is_empty() {
            return Err("the CPU list is empty: name one CPU at least, such as `0`".to_owned());
        }
        Ok(cpus)
    }
}

impl CpuList {
    fn parse(text: &str) -> Result<Self, String> {
        let mut cpus = BTreeSet::new();
        let unread = |text: &str| format!("`{text}` is no CPU list: expected one such as `0-3,8`");
        for part in text.trim().split(',').filter(|part| !part.is_empty()) {
            let (first, last) = part.split_once('-').unwrap_or((part, part));
            let (Ok(first), Ok(last)) = (first.parse::<u32>(), last.parse::<u32>()) else {
                return Err(unread(text.trim()));
            };
            if first > last {
                return Err(unread(text.trim()));
            }
            cpus.extend(first..=last);
        }
        Ok(Self(cpus))
    }
}

impl Serialize for CpuList {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl fmt::Display for CpuList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut cpus = self.0.iter().copied().peekable();
        let mut first = true;
        while let Some(start) = cpus.next() {
            let mut end = start;
            while cpus
                .next_if(|&next| Some(next) == end.checked_add(1))
                .is_some()
            {
                end += 1;
            }
            let comma = if first { "" } else { "," };
            first = false;
            if end == start {
                write!(f, "{comma}{start}")?;
            } else {
                write!(f, "{comma}{start}-{end}")?;
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct Cpus {
    // The online CPUs that sandboxes may use, grouped by the core they are threads of, as a run is
    // given whole cores.
    cores: Vec<Vec<u32>>,
    cpu_millis: NonZeroU64,
    ledger: PathBuf,
}

impl Cpus {
    pub(crate) fn of_machine(engine: &str) -> Result<Self, String> {
        // The ID is a directory's name in the ledgers.
        let named = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | ':' | '_');
        if engine.is_empty() || !engine.chars().all(named) {
            return Err(format!(
                "the engine's ID, `{}`, isn't of the form expected: use an engine whose `docker \
                 info` gives one of letters, digits, `-`, `:` and `_`",
                engine.escape_debug()
            ));
        }
        let read = |path: &Path| {
            let list = fs::read_to_string(path).context(path.display())?;
            CpuList::parse(&list).context(path.display())
        };
        let online = read(Path::new(ONLINE))?;
        let count = u64::try_from(online.0.len()).unwrap_or(u64::MAX);
        let Some(cpu_millis) = NonZeroU64::new(count.saturating_mul(MILLIS_PER_CPU)) else {
            return Err(format!(
                "{ONLINE} lists no CPU: expected at least one online"
            ));
        };
        let mut cores: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for &cpu in &online.0 {
            let siblings =
                format!("/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list");
            let siblings = read(Path::new(&siblings))?;
            // Threads of one core list the same siblings, so the lowest online one names the core.
            let first = (siblings.0.iter()).find(|sibling| online.0.contains(sibling));
            cores.entry(*first.unwrap_or(&cpu)).or_default().push(cpu);
        }
        Ok(Self {
            cores: cores.into_values().collect(),
            cpu_millis,
            ledger: Path::new(LEDGERS).join(engine),
        })
    }

    pub(crate) fn cpu_millis(&self) -> NonZeroU64 {
        self.cpu_millis
    }
}

pub(crate) fn flags() -> Result<BTreeSet<CpuFlag>, String> {
    let text = fs::read_to_string(CPUINFO).context(CPUINFO)?;
    let lines = text.lines().filter_map(|line| line.split_once(':'));
    let listed = lines.filter(|(key, _)| key.trim_end() == "flags");
    let mut common: Option<BTreeSet<CpuFlag>> = None;
    for (_, flags) in listed {
        let flags = flags
            .split_whitespace()
            .map(|flag| CpuFlag::try_from(flag.to_owned()));
        let flags: BTreeSet<CpuFlag> = flags.collect::<Result<_, _>>().context(CPUINFO)?;
        // A sandbox may run on any CPU, so only a flag that every CPU lists can be relied on.
        common = Some(match common {
            Some(common) => common.intersection(&flags).cloned().collect(),
            None => flags,
        });
    }
    common.ok_or_else(|| {
        format!("{CPUINFO} lists no CPU's flags: expected a `flags` line for each, as x86's has")
    })
}

// Gives each instance of each role the cores it needs, taken in order from `cores`, and returns
// those shares with the number of cores the most demanding phase takes. Roles of one phase get
// cores of their own; a later phase starts again from the first, as phases don't overlap. Gives
// `None` if `cores` run out.
fn lay_out(cores: &[&Vec<u32>], phases: &[Vec<Role>]) -> Option<(Vec<(String, CpuList)>, usize)> {
    let mut shares = Vec::new();
    let mut taken = 0;
    for phase in phases {
        let mut at = 0;
        for Role {
            name,
            resources,
            instances,
            ..
        } in phase
        {
            for instance in 0..instances.get() {
                let mut share = CpuList::default();
                while u64::try_from(share.0.len()).unwrap_or(u64::MAX) < of_role(resources) {
                    share.0.extend(cores.get(at)?.iter());
                    at += 1;
                }
                shares.push((instance_key(name, instance), share));
            }
        }
        taken = taken.max(at);
    }
    Some((shares, taken))
}

fn instance_key(role: &Name, instance: u32) -> String {
    format!("{role}/{instance}")
}

fn of_role(resources: &Resources) -> u64 {
    resources.cpu_millis.get().div_ceil(MILLIS_PER_CPU)
}

// A run's entry, which each instance's share is in, by its key. Fields it doesn't name are
// ignored, as processes of other versions of Pumpkin share the ledger.
#[derive(Serialize, Deserialize)]
struct EntryJson {
    roles: BTreeMap<String, CpuList>,
}

pub(crate) fn reserve(
    cpus: &Cpus,
    run: &RunTag,
    phases: &[Vec<Role>],
) -> Result<Reservation, String> {
    // The run is laid out on every core first, to tell a run that can never fit this machine from
    // one that waits for another's cores.
    let all: Vec<&Vec<u32>> = cpus.cores.iter().collect();
    let count = all.len();
    let Some((_, needed)) = lay_out(&all, phases) else {
        return Err(format!(
            "the run's roles need more whole cores at once than the {count} that this machine \
             gives sandboxes, to hold their CPUs ({}): give them fewer CPUs",
            needs(phases)
        ));
    };
    let ledger = open_ledger(&cpus.ledger)?;
    let held = held(&ledger)?;
    let in_use: BTreeSet<u32> = held
        .values()
        .flat_map(|cpus| cpus.0.iter().copied())
        .collect();
    let free: Vec<&Vec<u32>> = (all.iter().copied())
        .filter(|core| core.iter().all(|cpu| !in_use.contains(cpu)))
        .collect();
    let Some((laid_out, _)) = lay_out(&free, phases) else {
        let runs: Vec<&str> = held.keys().map(String::as_str).collect();
        let (s, them) = if runs.len() == 1 {
            ("", "it ends")
        } else {
            ("s", "they end")
        };
        return Err(format!(
            "the run's roles need {needed} whole cores at once, to hold their CPUs ({}), and \
             run{s} {} hold {} of the {count} that this machine gives sandboxes: run it once \
             {them}, or give its roles fewer CPUs",
            needs(phases),
            runs.join(", "),
            count - free.len()
        ));
    };
    let mut shares = BTreeMap::new();
    for (key, cpus) in laid_out {
        let twice = shares.insert(key.clone(), cpus);
        assert!(twice.is_none(), "instance `{key}` is named twice");
    }
    let entry = write_entry(&ledger, run, &EntryJson { roles: shares })?;
    // The entry's own lock holds the cores from here, so the ledger is free to the next run.
    drop(ledger);
    Ok(Reservation::new(entry))
}

fn needs(phases: &[Vec<Role>]) -> String {
    let needs: Vec<String> = (phases.iter().flatten())
        .map(|role| {
            let (cpus, instances) = (of_role(&role.resources), role.instances);
            match instances.get() {
                1 => format!("`{}` {cpus}", role.name),
                _ => format!("`{}` {cpus} for each of {instances}", role.name),
            }
        })
        .collect();
    needs.join(", ")
}

pub(crate) fn share(
    cpus: &Cpus,
    run: &RunTag,
    role: &Name,
    instance: u32,
) -> Result<CpuList, String> {
    let path = entry_path(&cpus.ledger, run);
    let text = fs::read_to_string(&path).map_err(|error| match error.kind() {
        ErrorKind::NotFound => format!(
            "run {} holds no CPUs, as {} has no entry for it: reserve them before creating its \
             sandboxes",
            run.as_str(),
            cpus.ledger.display()
        ),
        _ => format!("{}: {error}", path.display()),
    })?;
    let EntryJson { mut roles } = parse_entry(&path, &text)?;
    let Some(cpus) = roles.remove(&instance_key(role, instance)) else {
        return Err(format!(
            "run {} holds no CPUs for instance {instance} of role `{role}`: reserve them for as \
             many instances of each role as it creates sandboxes of at once",
            run.as_str()
        ));
    };
    Ok(cpus)
}

struct Ledger<'p> {
    path: &'p Path,
    // The directory, locked exclusively until this drops, so that one process at a time decides
    // which cores are free.
    _locked: File,
}

fn open_ledger(path: &Path) -> Result<Ledger<'_>, String> {
    for dir in [Path::new(LEDGERS), path] {
        let unmade = |error| {
            format!(
                "the CPU ledger {} can't be made: {error}: expected /var/tmp to be a directory \
                 every user may write, as Linux makes it",
                dir.display()
            )
        };
        match fs::create_dir(dir) {
            Ok(()) => {
                // Set apart from the creation, which the umask would narrow.
                fs::set_permissions(dir, Permissions::from_mode(LEDGER_MODE)).map_err(unmade)?;
            }
            // Another process may have made it first.
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(unmade(error)),
        }
    }
    let dir = File::open(path).context(path.display())?;
    // This waits for the other holders, which is brief: each reads and writes a few small files.
    dir.lock().context(path.display())?;
    Ok(Ledger { path, _locked: dir })
}

fn held(ledger: &Ledger<'_>) -> Result<BTreeMap<String, CpuList>, String> {
    let mut held = BTreeMap::new();
    let at = ledger.path.display();
    for entry in fs::read_dir(ledger.path).context(&at)? {
        let path = entry.context(&at)?.path();
        let Some(run) = (path.file_name().and_then(|name| name.to_str()))
            .and_then(|name| name.strip_suffix(".json"))
            .map(str::to_owned)
        else {
            return Err(format!(
                "{} is no entry of the CPU ledger, which holds only `<run>.json`s: remove it",
                path.display()
            ));
        };
        let file = File::open(&path).context(path.display())?;
        // A run holds its entry locked exclusively for as long as it lives, so an entry that a
        // shared lock can take is of a run that has ended. Another user's entry stays, as the
        // ledger's sticky bit keeps it, and counts for nothing.
        match file.try_lock_shared() {
            Ok(()) => {
                let _ = fs::remove_file(&path);
            }
            Err(TryLockError::WouldBlock) => {
                let text = fs::read_to_string(&path).context(path.display())?;
                let EntryJson { roles } = parse_entry(&path, &text)?;
                let cpus = roles.into_values().flat_map(|share| share.0).collect();
                held.insert(run, CpuList(cpus));
            }
            Err(TryLockError::Error(error)) => return Err(error).context(path.display()),
        }
    }
    Ok(held)
}

fn write_entry(ledger: &Ledger<'_>, run: &RunTag, json: &EntryJson) -> Result<File, String> {
    let path = entry_path(ledger.path, run);
    let file = (OpenOptions::new().write(true).create_new(true))
        .mode(ENTRY_MODE)
        .open(&path)
        .context(path.display())?;
    // Set apart from the creation, which the umask would narrow.
    file.set_permissions(Permissions::from_mode(ENTRY_MODE))
        .context(path.display())?;
    let locked = file.lock().map_err(|error| error.to_string());
    let written =
        locked.and_then(|()| serde_json::to_writer(&file, json).map_err(|error| error.to_string()));
    if let Err(error) = written {
        let _ = fs::remove_file(&path);
        return Err(format!("{}: {error}", path.display()));
    }
    Ok(file)
}

// A run tag holds no `/` and starts with no `.`, so the path is in the ledger.
fn entry_path(ledger: &Path, run: &RunTag) -> PathBuf {
    ledger.join(format!("{}.json", run.as_str()))
}

fn parse_entry(path: &Path, text: &str) -> Result<EntryJson, String> {
    serde_json::from_str(text).map_err(|error| {
        format!(
            "{} isn't an entry of the CPU ledger of the form expected: {error}: remove it once \
             the run it names has ended",
            path.display()
        )
    })
}
