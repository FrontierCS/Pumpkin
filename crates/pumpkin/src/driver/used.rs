//! What the driver used, as it reads itself from Linux as the run ends. Each figure is read on its
//! own. One that can't be read is `None`, and why it couldn't is not kept, so a failed read never
//! fails the run.

use std::fs;
use std::path::Path;
use std::time::Duration;

use runtime::{DriverUse, JobUse};
use rustix::time::{ClockId, clock_gettime};

const STAT: &str = "/proc/self/stat";

const STATUS: &str = "/proc/self/status";

const CGROUP: &str = "/proc/self/cgroup";

// Where systemd mounts the cgroup v2 hierarchy on a host whose hierarchy is unified, v2 alone.
// `/proc/self/cgroup` names a cgroup from the root of the process's cgroup namespace, which is
// what a namespace's own mount shows here.
const CGROUPS: &str = "/sys/fs/cgroup";

// The file that each cgroup v2 directory has, and none of a v1 hierarchy.
const CONTROLLERS: &str = "cgroup.controllers";

/// What this process has used so far. It holds what the unit `job_unit` used if this process's
/// cgroup is that unit's, in a unified cgroup v2 hierarchy at [`CGROUPS`].
pub(crate) fn used(job_unit: Option<&str>) -> DriverUse {
    let stat = fs::read_to_string(STAT).ok();
    let status = fs::read_to_string(STATUS).ok();
    DriverUse {
        cpu: Duration::try_from(clock_gettime(ClockId::ProcessCPUTime)).ok(),
        children_cpu: stat.as_deref().and_then(children_cpu),
        peak_memory_bytes: status.as_deref().and_then(peak_memory_bytes),
        job: job_unit.and_then(job),
    }
}

/// `cutime` and `cstime` of `stat`, `/proc/self/stat`, as proc(5) lays it out. They are its 16th
/// and 17th fields, in clock ticks.
fn children_cpu(stat: &str) -> Option<Duration> {
    // The 2nd field is the command's name in parentheses, which may hold spaces and parentheses.
    // So the fields are counted from the 3rd, after the last `)`.
    let (_, after) = stat.rsplit_once(')')?;
    let mut fields = after.split_whitespace().skip(16 - 3);
    let user: u64 = fields.next()?.parse().ok()?;
    let system: u64 = fields.next()?.parse().ok()?;
    let ticks = u128::from(user) + u128::from(system);
    let per_second = u128::from(rustix::param::clock_ticks_per_second());
    let nanos = (ticks * 1_000_000_000).checked_div(per_second)?;
    Some(Duration::from_nanos(u64::try_from(nanos).ok()?))
}

/// `VmHWM` of `status`, `/proc/self/status`, as proc(5) lays it out: `VmHWM:   21284 kB`.
fn peak_memory_bytes(status: &str) -> Option<u64> {
    let peak = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))?;
    let kib: u64 = peak.trim().strip_suffix(" kB")?.trim_end().parse().ok()?;
    kib.checked_mul(1024)
}

/// What the unit `unit` used, if this process's cgroup v2, as `/proc/self/cgroup` names it, is the
/// unit's, and its directory is below [`CGROUPS`]. The figures are those of its `cpu.stat`,
/// `memory.peak` and `pids.peak`.
fn job(unit: &str) -> Option<JobUse> {
    let cgroup = fs::read_to_string(CGROUP).ok()?;
    // The v2 line, `0::<path>`, which a host with v1 hierarchies too lists among theirs.
    let path = cgroup.lines().find_map(|line| line.strip_prefix("0::"))?;
    if Path::new(path).file_name()? != unit {
        return None;
    }
    let dir = Path::new(CGROUPS).join(path.trim_start_matches('/'));
    // A host whose v2 hierarchy is mounted elsewhere, as one with v1 hierarchies too, has no such
    // directory below `CGROUPS`.
    if !dir.join(CONTROLLERS).is_file() {
        return None;
    }
    let read = |file: &str| fs::read_to_string(dir.join(file)).ok();
    let number = |file: &str| read(file)?.trim().parse().ok();
    let cpu = read("cpu.stat").and_then(|stat| {
        let micros = stat
            .lines()
            .find_map(|line| line.strip_prefix("usage_usec "))?;
        Some(Duration::from_micros(micros.trim().parse().ok()?))
    });
    Some(JobUse {
        unit: unit.to_owned(),
        cpu,
        peak_memory_bytes: number("memory.peak"),
        peak_tasks_count: number("pids.peak"),
    })
}
