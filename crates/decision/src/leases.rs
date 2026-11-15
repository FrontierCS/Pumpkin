//! The leases a run has opened, and what each holder has used of them.
//!
//! A grant's limits count per holder, so each call of a judge starts with its grants' limits
//! whole. A lease's time counts from when its runner is ready, so the time a runner takes to
//! start costs its holder nothing.
//!
//! What a holder has used of a grant is counted from its leases when asked and never kept apart
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

#[derive(Debug, Default)]
pub(super) struct Leases(Vec<Lease>);

#[derive(Debug)]
pub(super) struct Lease {
    /// Who opened it, and alone may use it.
    pub(super) holder: Caller,
    pub(super) grant: LeaseGrantId,
    /// Since the run started.
    pub(super) opened: Duration,
    /// How long it lasts from when its runner is ready.
    pub(super) life: Duration,
    /// Which one it is among the grant's leases that have not ended, below the grant's
    /// `instances` ([`Leases::open`]).
    pub(super) instance: u32,
    pub(super) state: State,
    /// The commands and transfers accepted, even if one fails to start; they share one numbering.
    pub(super) commands: u64,
    pub(super) last: Option<Work>,
    /// The time it was busy before the work that runs began.
    busy: Duration,
    /// Since when the work that runs has run, until it ends or the lease is closed.
    working: Option<Duration>,
}

#[derive(Debug)]
pub(super) enum State {
    /// Opened; its runner has not started, and no time counts.
    Starting,
    /// Its runner has been ready since `since`, and its time counts.
    Ready { since: Duration },
    /// Closed by the core, for `end`; its runner is being destroyed, and it still holds its
    /// instance. `used` is the time it counted.
    Closing { used: Duration, end: End },
    /// Its runner is gone.
    Ended { used: Duration, end: End },
}

#[derive(Debug)]
pub(super) enum End {
    /// The holder closed it, or the core did as the holder's work ended.
    Closed,
    /// Its own life or its grant's cumulative time ran out.
    Expired,
    /// Its runner failed to start or to run, for the runtime's reason.
    Failed(String),
}

#[derive(Debug)]
/// A command or a transfer of a lease, numbered from 0 among them, with its end once the
/// runtime reports it.
pub(super) enum Work {
    Command {
        number: u64,
        ended: Option<(CommandEnd, String, String)>,
    },
    Transfer {
        number: u64,
        ended: Option<TransferEnd>,
    },
}

#[derive(Clone, Copy, Debug, Default)]
/// What a holder's leases of one grant have used.
pub(super) struct Usage {
    /// The leases that have not ended, those closing included, which still hold their instances.
    pub(super) open: u32,
    pub(super) closing: u32,
    /// Of those open, the ones whose time counts.
    pub(super) ready: u32,
    /// The time counted, the ended leases' included.
    pub(super) used: Duration,
    /// The commands and transfers accepted, the ended leases' included.
    pub(super) commands: u64,
}

impl State {
    pub(super) fn ready(&mut self, at: Duration) {
        if let Self::Starting = self {
            *self = Self::Ready { since: at };
        }
    }

    /// Closes it with `end` and returns whether it did: one closing or ended keeps its end.
    pub(super) fn close(&mut self, at: Duration, end: End) -> bool {
        let used = match *self {
            Self::Starting => Duration::ZERO,
            Self::Ready { since } => at.saturating_sub(since),
            Self::Closing { .. } | Self::Ended { .. } => return false,
        };
        *self = Self::Closing { used, end };
        true
    }

    /// Takes the runner as gone. The lease ends with `end` unless it was closing, whose end stays,
    /// and a ready lease's time counts until `at`.
    pub(super) fn report(&mut self, at: Duration, end: End) {
        self.close(at, end);
        *self = match mem::replace(self, Self::Starting) {
            Self::Closing { used, end } => Self::Ended { used, end },
            other => other,
        };
    }

    fn used(&self, now: Duration) -> Duration {
        match *self {
            Self::Starting => Duration::ZERO,
            Self::Ready { since } => now.saturating_sub(since),
            Self::Closing { used, .. } | Self::Ended { used, .. } => used,
        }
    }
}

impl Work {
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
    pub(super) fn running(&self) -> Option<&Work> {
        self.last.as_ref().filter(|work| !work.ended())
    }

    /// Takes in the work that `work` builds from the lease's next number, and returns the number.
    pub(super) fn accept(&mut self, work: impl FnOnce(u64) -> Work, now: Duration) -> u64 {
        let number = self.commands;
        self.commands += 1;
        self.last = Some(work(number));
        self.working = Some(now);
        number
    }

    // A report of any work but the last changes nothing.
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

    pub(super) fn close(&mut self, at: Duration, end: End) -> bool {
        self.rest(at);
        self.state.close(at, end)
    }

    pub(super) fn report(&mut self, at: Duration, end: End) {
        self.rest(at);
        self.state.report(at, end);
    }

    /// The time the lease was busy, from when work was accepted until its end was reported or the
    /// lease was closed, and the time it was ready and not busy, up to `now`.
    pub(super) fn busy_idle(&self, now: Duration) -> (Duration, Duration) {
        let working = (self.working).map_or(Duration::ZERO, |since| now.saturating_sub(since));
        let busy = self.busy + working;
        (busy, self.state.used(now).saturating_sub(busy))
    }

    fn rest(&mut self, at: Duration) {
        if let Some(since) = self.working.take() {
            self.busy += at.saturating_sub(since);
        }
    }
}

impl Leases {
    /// Opens a lease, under the lowest instance that no lease of the grant, not yet ended, holds.
    /// The grant's limits are the caller's to check, and so is that no other holder has a lease of
    /// the grant that has not ended: the instance is then below the grant's `instances`.
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

    pub(super) fn find(&self, number: u64) -> Option<LeaseId> {
        let index = usize::try_from(number).ok()?;
        (index < self.0.len()).then_some(LeaseId(number))
    }

    /// Closes each lease that `which` selects and that is not closing or ended, and returns them.
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

    pub(super) fn holds(&self, holder: Caller) -> bool {
        (self.0.iter())
            .any(|lease| lease.holder == holder && !matches!(lease.state, State::Ended { .. }))
    }

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
    fn lease_end(&self, lease: &Lease) -> Option<Duration> {
        match lease.state {
            State::Ready { since } => Some(self.ready_end(lease, since)),
            State::Starting | State::Closing { .. } | State::Ended { .. } => None,
        }
    }

    /// When a ready lease ends, from when its runner was ready, `since`: its life from then, or
    /// sooner if its grant's cumulative time runs out. What is left of that time is shared equally
    /// among the holder's ready leases of the grant, so the end moves as leases come and go.
    pub(super) fn ready_end(&self, lease: &Lease, since: Duration) -> Duration {
        let own = since.saturating_add(lease.life);
        let Some(total) = self.workflow.lease_grant(lease.grant).cumulative() else {
            return own;
        };
        let usage = self.leases.usage(lease.grant, lease.holder, self.now);
        let left = total.saturating_sub(usage.used);
        let share = left / usage.ready;
        // Each ready lease spends the time left at once, so it lasts a share of it, rounded up, so
        // that once the share has passed for each, the time has run out.
        let share = if share * usage.ready < left {
            share + Duration::from_nanos(1)
        } else {
            share
        };
        own.min(self.now.saturating_add(share))
    }

    pub(super) fn leases_due(&self) -> Option<Duration> {
        (self.leases.0.iter())
            .filter_map(|lease| self.lease_end(lease))
            .min()
    }

    pub(super) fn expire_leases(&mut self, actions: &mut Vec<Action>) {
        // Every end is read before any lease closes: a close changes the others' shares, and leases
        // that fall due together must expire together.
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

    pub(super) fn on_lease(&mut self, lease: LeaseId, apply: impl FnOnce(&mut Lease)) {
        if let Some(lease) = self.leases.find(lease.0) {
            apply(&mut self.leases[lease]);
        }
    }
}
