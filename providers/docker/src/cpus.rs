//! The machine's CPUs, which the provider shares out between runs ([`Cpus`]): each run holds
//! whole cores of its own from its [reservation](provider::Provider::reserve) until its end, and
//! each instance of each of its roles a share of them, which the container of that instance is
//! confined to.
//!
//! What runs hold is kept in the engine's ledger, a directory that every run on the engine
//! shares, whoever starts it, as each machine's configuration names one engine, with an entry for
//! each run that has reserved, locked by the process that holds the reservation: so the kernel
//! releases a run's CPUs once that process has ended, however it ends, and no process runs to
//! keep the ledger. A reserve takes the ledger's own lock while it reads what the locked entries
//! hold and writes its own, so two reserves never choose the same CPUs. An instance's share is
//! whole cores, a core's CPUs together, as many as hold its role's CPU limit rounded up, laid out
//! by phase, so that no two sandboxes that run at once share a core, nor any two runs.
//!
//! Its limits: only runs of a Pumpkin that keeps the ledger hold CPUs, so other containers, as
//! of an older Pumpkin, run on any of them; a container that outlives its run's process, while
//! the engine removes it, may run on CPUs that a later run holds; two engines on one machine, as
//! a rootful and a rootless one, each share out all its CPUs; and any user of the machine may add
//! to a ledger, or hold an entry of their own, so it keeps runs apart, not users.

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

/// The directory of the ledgers of the CPUs that runs hold, one for each engine, by its ID.
// Not in /dev/shm, where its entries would be lost: logind, with `RemoveIPC=yes`, its default,
// unlinks a user's files there once their last session ends, though a driver started outside a
// session, as from cron, still holds its entry's lock. An entry that outlives its holder, as a
// reboot leaves it, is unlocked, so the next reserve removes it.
const LEDGERS: &str = "/var/tmp/pumpkin-cpus";

/// The mode of the ledgers and their directory: anyone may add an entry, and only its owner may
/// remove it.
const LEDGER_MODE: u32 = 0o1777;

/// The mode of an entry: whatever the umask, every user's reserve can read and lock it.
const ENTRY_MODE: u32 = 0o644;

/// Where the kernel lists the CPUs that are online.
const ONLINE: &str = "/sys/devices/system/cpu/online";

/// Where the kernel describes each CPU, its flags among it.
const CPUINFO: &str = "/proc/cpuinfo";

/// A set of CPUs, by their numbers, written as the kernel writes one, such as `0-3,8-11`.
///
/// It is numbers and ranges of them, apart by commas, each range's first at most its last; made
/// from a `String`, it holds one CPU at least.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct CpuList(BTreeSet<u32>);

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
    /// The set that `text`, as the kernel writes one, names.
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

/// The CPUs that the provider gives sandboxes, each core's together, in order of their first,
/// and the ledger of what runs on the engine hold of them.
#[derive(Debug)]
pub(crate) struct Cpus {
    cores: Vec<Vec<u32>>,
    /// All of them, in thousandths of a CPU.
    cpu_millis: NonZeroU64,
    ledger: PathBuf,
}

impl Cpus {
    /// The machine's online CPUs, or those of `given` if given, each with the CPUs that share
    /// its core, as the kernel lists them, which runs on the engine of ID `engine`, as `docker
    /// info` gives it, hold.
    ///
    /// # Errors
    ///
    /// Fails if the engine's ID can't name a directory, if none is online, or if `given` names a
    /// CPU that isn't.
    pub(crate) fn of_machine(engine: &str, given: Option<&CpuList>) -> Result<Self, String> {
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
        let mut online = read(Path::new(ONLINE))?;
        if let Some(given) = given {
            let offline = CpuList(given.0.difference(&online.0).copied().collect());
            if !offline.0.is_empty() {
                return Err(format!(
                    "the CPUs to give sandboxes, {given}, name {offline}, which aren't online: \
                     name only CPUs of {online}"
                ));
            }
            online = given.clone();
        }
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
            // A core by its first CPU, of those online.
            let first = (siblings.0.iter()).find(|sibling| online.0.contains(sibling));
            cores.entry(*first.unwrap_or(&cpu)).or_default().push(cpu);
        }
        Ok(Self {
            cores: cores.into_values().collect(),
            cpu_millis,
            ledger: Path::new(LEDGERS).join(engine),
        })
    }

    /// All the CPUs it gives sandboxes, in thousandths of a CPU.
    pub(crate) fn cpu_millis(&self) -> NonZeroU64 {
        self.cpu_millis
    }
}

/// The flags that every CPU of the machine has, as the kernel lists each CPU's in `/proc/cpuinfo`.
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
        common = Some(match common {
            Some(common) => common.intersection(&flags).cloned().collect(),
            None => flags,
        });
    }
    common.ok_or_else(|| {
        format!("{CPUINFO} lists no CPU's flags: expected a `flags` line for each, as x86's has")
    })
}

/// The CPUs that each instance of each role of `phases` is given of `cores`, a core's CPUs
/// together, by its key ([`instance_key`]), and how many of `cores`, from the first, the run
/// takes; or `None` if a phase's roles need more. Each phase is laid out from the first core, each
/// instance taking whole cores until they hold as many CPUs as its role needs, so no two sandboxes
/// of a phase share a core.
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

/// The key of instance `instance` of role `role` in a run's entry: `<role>/<instance>`, which no
/// role's name is, as a name holds no `/`.
fn instance_key(role: &Name, instance: u32) -> String {
    format!("{role}/{instance}")
}

/// How many whole CPUs a role of `resources` is given: its CPU limit, rounded up.
fn of_role(resources: &Resources) -> u64 {
    resources.cpu_millis.get().div_ceil(MILLIS_PER_CPU)
}

/// A run's entry in the ledger, as JSON: each instance's share, by its key ([`instance_key`]). A
/// field it doesn't name is ignored, so that an older Pumpkin reads what a newer one adds; and a
/// reserve sums the shares whatever their keys, so that it reads an older Pumpkin's entry, whose
/// keys are its roles' names.
#[derive(Serialize, Deserialize)]
struct EntryJson {
    roles: BTreeMap<String, CpuList>,
}

/// Holds for `run` the CPUs that its `phases` need at once, of `cpus`, beside those every
/// other run holds, as [`Provider::reserve`](provider::Provider::reserve) says, and shares them
/// out between its roles.
pub(crate) fn reserve(
    cpus: &Cpus,
    run: &RunTag,
    phases: &[Vec<Role>],
) -> Result<Reservation, String> {
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
    // The ledger's lock goes with it, once the entry is locked and whole.
    drop(ledger);
    Ok(Reservation::new(entry))
}

/// The CPUs each role of `phases` needs, as an error shows them: its name, and its CPUs, for each
/// of its instances.
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

/// The share of instance `instance` of role `role` of `run`, whose entry the ledger holds. It
/// fails if the run holds no CPUs, or none for the instance.
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

/// A ledger, its lock held until it is dropped.
struct Ledger<'p> {
    path: &'p Path,
    _locked: File,
}

/// The ledger at `path`, made if need be, locked.
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
            // The umask would narrow the mode `create_dir` makes it with.
            Ok(()) => {
                fs::set_permissions(dir, Permissions::from_mode(LEDGER_MODE)).map_err(unmade)?;
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(unmade(error)),
        }
    }
    let dir = File::open(path).context(path.display())?;
    // Each holder of the lock reads and writes only a few small files in memory.
    dir.lock().context(path.display())?;
    Ok(Ledger { path, _locked: dir })
}

/// The CPUs that each run holds, by its tag, of the entries in `ledger` whose locks are held;
/// those whose aren't are removed, where this user may.
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
        match file.try_lock_shared() {
            Ok(()) => {
                // Its holder has ended. Another user's entry stays, as the ledger's mode keeps
                // it, and counts for nothing.
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

/// Writes `run`'s entry, `json`, into `ledger`, locked by the file returned.
fn write_entry(ledger: &Ledger<'_>, run: &RunTag, json: &EntryJson) -> Result<File, String> {
    let path = entry_path(ledger.path, run);
    let file = (OpenOptions::new().write(true).create_new(true))
        .mode(ENTRY_MODE)
        .open(&path)
        .context(path.display())?;
    // The umask may have narrowed the mode it was made with.
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

/// The path of `run`'s entry in the ledger: a run tag holds no `/`, and starts with no `.`.
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
