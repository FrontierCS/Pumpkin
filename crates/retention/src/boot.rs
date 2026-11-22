//! Whether the controller's boot has ended.
//!
//! The boot reaps the runs of each install's build, and a reap may need what a prune would remove.
//! So the prunes of the records and of the installs wait until the boot has ended.

/// The boot unit's properties, as `systemctl show` gives them. Each is `None` if `systemctl` gave
/// none, as when it can't say.
#[derive(Clone, Copy, Debug)]
pub struct Unit<'s> {
    /// `ActiveState`.
    pub active_state: Option<&'s str>,
    /// `Job`: the job queued for the unit, empty if none is.
    pub job: Option<&'s str>,
    /// `ExecMainExitTimestamp`: when the unit's run exited, empty if it hasn't.
    pub exit_timestamp: Option<&'s str>,
}

impl Unit<'_> {
    /// Whether the unit has run to its end since the machine booted. It has no job queued, and it
    /// isn't activating again.
    pub fn ended(self) -> bool {
        // systemd keeps a unit's times for the boot alone, so an exit time belongs to this boot's
        // run.
        let exited = self.exit_timestamp.is_some_and(|at| !at.is_empty());
        let idle = self.job == Some("") && self.active_state != Some("activating");
        exited && idle
    }
}
