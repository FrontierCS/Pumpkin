//! The leases that lease grants open, the meter they keep, and when the core ends them.
//!
//! Each lease has a holder ([`Caller`]), the agent or a judge's call, whose grant's limits it
//! counts against: a grant's meter is its holder's, so each call of a judge starts with its
//! grants' limits whole.
//!
//! A lease counts against its grant's `instances` from when it is accepted until its sandbox is
//! gone. Its time runs from when its runner is ready, as the runtime observed it, until the core
//! closes it, or the runtime reports its runner failed: the core closes it at its holder's close,
//! at the end of its own time (its grant's
//! [`instance_seconds`](schema::ir::LeaseGrant::instance_seconds), or less as its holder asked),
//! at the end of its grant's
//! ([`cumulative_seconds`](schema::ir::LeaseGrant::cumulative_seconds)), at the work phase's
//! end, or, for a call's, once the call's command has ended. The grant's time left is what it
//! allows less what its holder's leases have used, which each of them that is ready spends at
//! once, so the core closes them all once it has run out, and its tick comes then
//! ([`Core::tick_due`]).
//!
//! A lease runs one command or transfer at a time, and each counts against its grant's `commands`
//! once accepted, even if it fails to start. Of its time, it is busy from when the core accepts a
//! command or transfer until the runtime reports its end, or the core closes the lease, and idle
//! otherwise.
//!
//! The meter is not kept apart: what each grant has used is counted from its leases
//! ([`Leases::usage`]), so the two can't disagree.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::mem;
use core::ops::{Index, IndexMut};
use core::time::Duration;

use schema::sandbox::op::{CommandEnd, TransferEnd};
use schema::workflow::LeaseGrantId;

use super::{Action, Caller, Core, LeaseId};

/// The run's leases, each numbered by its index, in the order they were opened.
#[derive(Debug, Default)]
pub(super) struct Leases(Vec<Lease>);

#[derive(Debug)]
pub(super) struct Lease {
    /// Who opened it, and alone may use it.
    pub(super) holder: Caller,
    /// The holder's grant the lease was opened under.
    pub(super) grant: LeaseGrantId,
    /// When it was opened.
    pub(super) opened: Duration,
    /// How long it lasts once its runner is ready.
    pub(super) life: Duration,
    /// Which of its grant's instances it is, below the grant's `instances`.
    pub(super) instance: u32,
    pub(super) state: State,
    /// The commands and transfers accepted.
    pub(super) commands: u64,
    /// The last command or transfer accepted, if one was.
    pub(super) last: Option<Work>,
    /// How long it was busy, until the command or transfer that runs, if one does, began.
    busy: Duration,
    /// When the command or transfer that runs began, until it ends, or the lease is closed.
    working: Option<Duration>,
}

/// Where a lease is.
#[derive(Debug)]
pub(super) enum State {
    /// Accepted; its runner is starting.
    Starting,
    /// Its runner has been ready since `since`.
    Ready { since: Duration },
    /// The core closed it, for `end`; its sandbox is being destroyed. `used` is how long its
    /// runner was ready.
    Closing { used: Duration, end: End },
    /// Its sandbox is gone.
    Ended { used: Duration, end: End },
}

/// Why a lease ended.
#[derive(Debug)]
pub(super) enum End {
    /// Its holder closed it, or its holder's work ended.
    Closed,
    /// Its time, or its grant's, ran out.
    Expired,
    /// Its runner failed, for this reason.
    Failed(String),
}

/// A command or a transfer a lease ran, each with its number among its lease's commands and
/// transfers, from 0, and how it ended, once the runtime says.
#[derive(Debug)]
pub(super) enum Work {
    /// A command, which ended with the ends of its standard output and error.
    Command {
        number: u64,
        ended: Option<(CommandEnd, String, String)>,
    },
    Transfer {
        number: u64,
        ended: Option<TransferEnd>,
    },
}

/// What a holder's leases of a grant have used.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Usage {
    /// The leases that haven't ended.
    pub(super) open: u32,
    /// Of them, those the core has closed, whose sandboxes are being removed.
    pub(super) closing: u32,
    /// The leases whose runners are ready, each of which spends the grant's time.
    pub(super) ready: u32,
    /// How long its leases' runners have been ready, until the time asked about.
    pub(super) used: Duration,
    /// The commands and transfers accepted.
    pub(super) commands: u64,
}

impl State {
    /// Notes that a starting lease's runner is ready.
    pub(super) fn ready(&mut self, at: Duration) {
        if let Self::Starting = self {
            *self = Self::Ready { since: at };
        }
    }

    /// Closes a lease that hasn't been closed, for `end`, and returns whether it did.
    pub(super) fn close(&mut self, at: Duration, end: End) -> bool {
        let used = match *self {
            Self::Starting => Duration::ZERO,
            Self::Ready { since } => at.saturating_sub(since),
            Self::Closing { .. } | Self::Ended { .. } => return false,
        };
        *self = Self::Closing { used, end };
        true
    }

    /// Ends a lease whose sandbox is gone: for the end it was closed for, if the core closed it,
    /// and else for `end`, its time running until then if its runner was ready.
    pub(super) fn report(&mut self, at: Duration, end: End) {
        self.close(at, end);
        *self = match mem::replace(self, Self::Starting) {
            Self::Closing { used, end } => Self::Ended { used, end },
            other => other,
        };
    }

    /// How long its runner has been ready, until `now`.
    fn used(&self, now: Duration) -> Duration {
        match *self {
            Self::Starting => Duration::ZERO,
            Self::Ready { since } => now.saturating_sub(since),
            Self::Closing { used, .. } | Self::Ended { used, .. } => used,
        }
    }
}

impl Work {
    /// What it is, by its number, as a refusal names it.
    pub(super) fn name(&self) -> String {
        match self {
            Self::Command { number, .. } => format!("command {number}"),
            Self::Transfer { number, .. } => format!("transfer {number}"),
        }
    }

    fn ended(&self) -> bool {
        match self {
            Self::Command { ended, .. } => ended.is_some(),
            Self::Transfer { ended, .. } => ended.is_some(),
        }
    }
}

impl Lease {
    /// The command or transfer it runs, if one runs: accepted, and its end not yet reported.
    pub(super) fn running(&self) -> Option<&Work> {
        self.last.as_ref().filter(|work| !work.ended())
    }

    /// Accepts `work`, at `now`, as its next command or transfer, and returns its number.
    pub(super) fn accept(&mut self, work: impl FnOnce(u64) -> Work, now: Duration) -> u64 {
        let number = self.commands;
        self.commands += 1;
        self.last = Some(work(number));
        self.working = Some(now);
        number
    }

    /// Notes that its `number`th command ended at `at`, as `ended` says, if that is its last.
    pub(super) fn command_ended(
        &mut self,
        number: u64,
        ended: (CommandEnd, String, String),
        at: Duration,
    ) {
        if let Some(Work::Command {
            number: last,
            ended: end,
        }) = &mut self.last
            && *last == number
        {
            *end = Some(ended);
            self.rest(at);
        }
    }

    /// Notes that its `number`th transfer ended at `at`, as `ended` says, if that is its last.
    pub(super) fn transfer_ended(&mut self, number: u64, ended: TransferEnd, at: Duration) {
        if let Some(Work::Transfer {
            number: last,
            ended: end,
        }) = &mut self.last
            && *last == number
        {
            *end = Some(ended);
            self.rest(at);
        }
    }

    /// Closes it, as [`State::close`] does, and returns whether it did.
    pub(super) fn close(&mut self, at: Duration, end: End) -> bool {
        self.rest(at);
        self.state.close(at, end)
    }

    /// Ends it, as [`State::report`] does.
    pub(super) fn report(&mut self, at: Duration, end: End) {
        self.rest(at);
        self.state.report(at, end);
    }

    /// How long it has been busy, and idle, of the time its runner has been ready, until `now`.
    pub(super) fn busy_idle(&self, now: Duration) -> (Duration, Duration) {
        let working = (self.working).map_or(Duration::ZERO, |since| now.saturating_sub(since));
        let busy = self.busy + working;
        (busy, self.state.used(now).saturating_sub(busy))
    }

    /// Ends its busy time at `at`, if it is busy.
    fn rest(&mut self, at: Duration) {
        if let Some(since) = self.working.take() {
            self.busy += at.saturating_sub(since);
        }
    }
}

impl Leases {
    /// Opens a lease for `holder`, starting, at `now`, under `grant`, to last `life` once ready,
    /// as the first of its instances that no other open lease of `grant` is.
    pub(super) fn open(
        &mut self,
        (holder, grant): (Caller, LeaseGrantId),
        life: Duration,
        now: Duration,
    ) -> LeaseId {
        let taken = |instance: u32| {
            (self.0.iter()).any(|lease| {
                lease.grant == grant
                    && lease.instance == instance
                    && !matches!(lease.state, State::Ended { .. })
            })
        };
        let mut instance = 0;
        while taken(instance) {
            instance += 1;
        }
        let lease = LeaseId(self.0.len() as u64);
        self.0.push(Lease {
            holder,
            grant,
            opened: now,
            life,
            instance,
            state: State::Starting,
            commands: 0,
            last: None,
            busy: Duration::ZERO,
            working: None,
        });
        lease
    }

    /// The lease numbered `number`, if one is.
    pub(super) fn find(&self, number: u64) -> Option<LeaseId> {
        let index = usize::try_from(number).ok()?;
        (index < self.0.len()).then_some(LeaseId(number))
    }

    /// Closes every lease `which` picks that hasn't been closed, at `now`, and returns them.
    pub(super) fn close_where(
        &mut self,
        now: Duration,
        which: impl Fn(&Lease) -> bool,
    ) -> Vec<LeaseId> {
        let mut closed = Vec::new();
        for (index, lease) in self.0.iter_mut().enumerate() {
            if which(lease) && lease.close(now, End::Closed) {
                closed.push(LeaseId(index as u64));
            }
        }
        closed
    }

    pub(super) fn all_ended(&self) -> bool {
        (self.0.iter()).all(|lease| matches!(lease.state, State::Ended { .. }))
    }

    /// Whether `holder` holds a lease that hasn't ended.
    pub(super) fn holds(&self, holder: Caller) -> bool {
        (self.0.iter())
            .any(|lease| lease.holder == holder && !matches!(lease.state, State::Ended { .. }))
    }

    /// What `holder`'s leases of `grant` have used by `now`.
    pub(super) fn usage(&self, grant: LeaseGrantId, holder: Caller, now: Duration) -> Usage {
        let mut usage = Usage::default();
        let held = |lease: &&Lease| lease.grant == grant && lease.holder == holder;
        for lease in self.0.iter().filter(held) {
            match lease.state {
                State::Starting => usage.open += 1,
                State::Closing { .. } => {
                    usage.open += 1;
                    usage.closing += 1;
                }
                State::Ready { .. } => {
                    usage.open += 1;
                    usage.ready += 1;
                }
                State::Ended { .. } => {}
            }
            usage.used += lease.state.used(now);
            usage.commands += lease.commands;
        }
        usage
    }
}

impl Index<LeaseId> for Leases {
    type Output = Lease;

    /// The lease `lease`, which the core opened, as [`Leases::open`] or [`Leases::find`] gave it.
    fn index(&self, lease: LeaseId) -> &Lease {
        &self.0[usize::try_from(lease.0).unwrap_or(usize::MAX)]
    }
}

impl IndexMut<LeaseId> for Leases {
    fn index_mut(&mut self, lease: LeaseId) -> &mut Lease {
        &mut self.0[usize::try_from(lease.0).unwrap_or(usize::MAX)]
    }
}

impl Core<'_> {
    /// When lease `lease` ends, unless it is closed first, if its runner is ready
    /// ([`Core::ready_end`]).
    fn lease_end(&self, lease: &Lease) -> Option<Duration> {
        match lease.state {
            State::Ready { since } => Some(self.ready_end(lease, since)),
            State::Starting | State::Closing { .. } | State::Ended { .. } => None,
        }
    }

    /// When `lease`, whose runner has been ready since `since`, ends, unless it is closed first:
    /// at its own time's end, or its grant's time's for its holder, whichever comes first.
    pub(super) fn ready_end(&self, lease: &Lease, since: Duration) -> Duration {
        let own = since.saturating_add(lease.life);
        let Some(total) = self.workflow.lease_grant(lease.grant).cumulative() else {
            return own;
        };
        let usage = self.leases.usage(lease.grant, lease.holder, self.now);
        let left = total.saturating_sub(usage.used);
        // Each ready lease spends the time left at once, so it lasts a share of it, rounded up,
        // so that once the share has passed for each, the time has run out.
        let share = left / usage.ready;
        let share = if share * usage.ready < left {
            share + Duration::from_nanos(1)
        } else {
            share
        };
        own.min(self.now.saturating_add(share))
    }

    /// When the first of the leases ends, unless it is closed first.
    pub(super) fn leases_due(&self) -> Option<Duration> {
        (self.leases.0.iter())
            .filter_map(|lease| self.lease_end(lease))
            .min()
    }

    /// Closes each lease whose time, or whose grant's, has run out by now.
    pub(super) fn expire_leases(&mut self, actions: &mut Vec<Action>) {
        let ends: Vec<Option<Duration>> = (self.leases.0.iter())
            .map(|lease| self.lease_end(lease))
            .collect();
        let (now, leases) = (self.now, self.leases.0.iter_mut());
        for (index, (lease, end)) in leases.zip(ends).enumerate() {
            if end.is_some_and(|end| now >= end) && lease.close(now, End::Expired) {
                actions.push(Action::CloseLease {
                    lease: LeaseId(index as u64),
                });
            }
        }
    }

    /// Applies a report on lease `lease` to it, if the run has a lease by that number.
    pub(super) fn on_lease(&mut self, lease: LeaseId, apply: impl FnOnce(&mut Lease)) {
        if let Some(lease) = self.leases.find(lease.0) {
            apply(&mut self.leases[lease]);
        }
    }
}
