//! What the controller is short of, and what the unit of each job that runs uses. `serve` reads
//! them from Linux as it answers, and reads nothing between requests.
//!
//! The controller's own cgroup v2 root says how long its tasks stalled. That root is the
//! machine's, or that of a Docker controller's container. Memory is the machine's, as
//! `/proc/meminfo` gives it. A job's figures come from its unit's cgroup, which systemd names, as
//! the path of a unit's cgroup is no interface of systemd's:
//! <https://github.com/systemd/systemd/blob/0966fe6734417ce07d5eb82a245fdec3789714da/docs/CGROUP_DELEGATION.md#L484-L492>.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;
use std::{fs, io};

use provider::RunTag;
use stdx::Context as _;
use stdx::process::{Bound, Echo, Run};

use crate::driver::{CGROUPS, cgroup_cpu};
use crate::job::SYSTEMD;
use crate::link::{Pressure, StalledPercent};
use crate::progress::driver::UnitUse;

const MEMINFO: &str = "/proc/meminfo";

// The bound of `systemctl show`, which reads the units' properties from the user's systemd.
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(10);

/// What the controller is short of now.
pub(crate) fn pressure() -> Result<Pressure, String> {
    let meminfo = fs::read_to_string(MEMINFO).context(MEMINFO)?;
    // A line such as `MemTotal:       64771480 kB`.
    let bytes = |key: &str| {
        let kib = (meminfo.lines())
            .find_map(|line| {
                line.strip_prefix(key)?
                    .strip_prefix(':')?
                    .strip_suffix(" kB")
            })
            .and_then(|kib| kib.trim().parse::<u64>().ok());
        kib.and_then(|kib| kib.checked_mul(1024)).ok_or_else(|| {
            format!("{MEMINFO} has no `{key}:` in kB: expected one, as Linux writes it")
        })
    };
    Ok(Pressure {
        memory_size_bytes: bytes("MemTotal")?,
        memory_available_bytes: bytes("MemAvailable")?,
        stalled_percent: stalled_percent()?,
    })
}

fn stalled_percent() -> Result<Option<StalledPercent>, String> {
    let some = |resource: &str| {
        let path = Path::new(CGROUPS).join(format!("{resource}.pressure"));
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            // A kernel that keeps none has no such file, or refuses its read:
            // https://github.com/torvalds/linux/blob/06090c9b622a7e1f797e775db4c035e0d779b76e/kernel/sched/psi.c#L1240-L1241
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::Unsupported
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error).context(path.display()),
        };
        // A line such as `some avg10=0.00 avg60=4.92 avg300=2.15 total=28115963`.
        let some = text.lines().find_map(|line| line.strip_prefix("some "));
        let avg60 = some.and_then(|some| some.split(' ').find_map(|f| f.strip_prefix("avg60=")));
        match avg60.and_then(|avg60| avg60.parse().ok()) {
            Some(percent) => Ok(Some(percent)),
            None => Err(format!(
                "{} holds `{}`: expected a line `some avg10=… avg60=… avg300=… total=…`, as \
                 Linux writes it",
                path.display(),
                text.trim().escape_debug()
            )),
        }
    };
    let (Some(cpu), Some(memory), Some(io)) = (some("cpu")?, some("memory")?, some("io")?) else {
        return Ok(None);
    };
    Ok(Some(StalledPercent { cpu, memory, io }))
}

/// What the unit of each job of `ids` that runs uses now, by the job's id. A job runs while its
/// directory in tmpfs is there (`crate::job::jobs`). A job whose cgroup systemd doesn't name, as
/// when `systemctl` fails, has no figure read. With no such job, nothing runs.
pub(crate) fn units<'i>(ids: impl IntoIterator<Item = &'i str>) -> BTreeMap<String, UnitUse> {
    let running = crate::job::jobs().and_then(|jobs| crate::job::ids(&jobs));
    let running: BTreeSet<String> = running.unwrap_or_default().into_iter().collect();
    let tags: Vec<RunTag> = (ids.into_iter())
        .filter(|id| running.contains(*id))
        .filter_map(|id| RunTag::try_from(id.to_owned()).ok())
        .collect();
    if tags.is_empty() {
        return BTreeMap::new();
    }
    let mut systemctl = stdx::process::command("systemctl", SYSTEMD);
    systemctl.args([
        "--user",
        "show",
        "--property=Id",
        "--property=ControlGroup",
        "--",
    ]);
    systemctl.args(tags.iter().map(crate::job::unit_name));
    let how = Run::new(Bound::Within(SYSTEMCTL_TIMEOUT), Echo::Silent);
    let shown = stdx::process::run(&mut systemctl, how)
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    // A block of `Name=value` lines for each unit, the blocks apart by an empty line. A unit that
    // isn't loaded, as once it has ended, has an empty `ControlGroup`.
    let mut cgroups = BTreeMap::new();
    for block in shown.split("\n\n") {
        let value = |name: &str| {
            (block.lines()).find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
        };
        let cgroup = value("ControlGroup").filter(|cgroup| !cgroup.is_empty());
        if let (Some(unit), Some(cgroup)) = (value("Id"), cgroup) {
            cgroups.insert(
                unit,
                Path::new(CGROUPS).join(cgroup.trim_start_matches('/')),
            );
        }
    }
    let used = |tag: &RunTag| {
        // As systemd names a unit that `systemd-run --unit` names without a suffix.
        let unit = format!("{}.service", crate::job::unit_name(tag));
        cgroups
            .get(unit.as_str())
            .map_or_else(UnitUse::default, |dir| UnitUse {
                cpu: cgroup_cpu(dir),
                memory_bytes: working_set(dir),
            })
    };
    tags.iter()
        .map(|tag| (tag.as_str().to_owned(), used(tag)))
        .collect()
}

/// The working set of the cgroup v2 directory `dir`: its `memory.current` less the `inactive_file`
/// of its `memory.stat`. That is the page cache that the kernel reclaims first, as cadvisor says:
/// <https://github.com/google/cadvisor/blob/68f0a593946d47c91398292b47a5ab43c952beef/container/libcontainer/handler.go#L832-L845>.
fn working_set(dir: &Path) -> Option<u64> {
    let read = |file: &str| fs::read_to_string(dir.join(file)).ok();
    let current: u64 = read("memory.current")?.trim().parse().ok()?;
    let stat = read("memory.stat")?;
    let inactive = stat
        .lines()
        .find_map(|line| line.strip_prefix("inactive_file "))?;
    Some(current.saturating_sub(inactive.trim().parse().ok()?))
}
