//! A run as `ls` lists it, made where the run's record is: the text that `ls` prints for it, and
//! the typed fields that the totals and a reader of a listing need.

use members::Member;
use provider::UnixTime;
use runtime::{Ending, Stopped};
use serde::{Deserialize, Serialize};

use super::{Columns, Listed, State, columns, under};

/// A run in a listing, rendered at the time and with the runs directory that it was made with.
#[derive(Serialize, Deserialize)]
pub(crate) struct Row {
    /// What `ls` prints in each column of the run's line.
    pub(crate) cells: Columns,
    pub(super) stage: Stage,
    pub(super) spent: Spent,
    /// The member whose job the run is, on a controller whose job names one.
    pub(crate) member: Option<Member>,
    /// What of the run's log this `pumpkin` can't read, as `progress::unread` says it, if any.
    pub(crate) unread: Option<String>,
}

/// What a run is doing or how it ended, with what only that stage has.
#[derive(Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(super) enum Stage {
    /// A job that waits for a build slot.
    Waiting,
    /// A job that builds the run.
    Building,
    /// A job that ended before its run started.
    Unstarted,
    Failed,
    Interrupted,
    /// A run that ended with no `outcome.json`.
    Lost,
    /// A run whose record another build made, which this one can't read.
    Unread,
    /// A run that publishes its images, before its log has begun, which the totals count as
    /// running.
    Publishing,
    Running {
        /// When the run ends at the latest, by its hard end, in seconds since the Unix epoch.
        ends_unix_seconds: u64,
        /// The lines that `ls` prints under the run's line, each ending in a newline: one for
        /// each call and lease that hasn't ended.
        under: String,
    },
    Finished {
        /// Whether the run returned a result.
        result: bool,
    },
}

/// What a run cost in US dollars, as far as its log says.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Spent {
    /// Nothing of the run's log has been read.
    Unread,
    /// No part of the run's cost has a price.
    Unpriced,
    Priced {
        usd: f64,
        /// Whether a part of the run's cost has no price.
        partly_unpriced: bool,
        /// How many responses went with tokens unreported ([`runtime::Cost::unreported`]).
        unreported_responses: u64,
    },
}

impl Row {
    /// The row of run `id`, `listed`, at `now`, with its commands naming `runs` as its runs
    /// directory.
    pub(crate) fn of(id: &str, listed: &Listed, now: UnixTime, runs: Option<&str>) -> Self {
        let stage = Stage::of(id, listed, now, runs);
        Self {
            cells: columns(id, listed, stage.state(), now, runs),
            stage,
            spent: Spent::of(listed),
            member: None,
            unread: None,
        }
    }

    pub(crate) fn state(&self) -> State {
        self.stage.state()
    }

    /// The lines that `ls` prints under the run's line: none unless it is running.
    pub(super) fn under(&self) -> &str {
        match &self.stage {
            Stage::Running { under, .. } => under,
            Stage::Waiting
            | Stage::Building
            | Stage::Unstarted
            | Stage::Failed
            | Stage::Interrupted
            | Stage::Lost
            | Stage::Unread
            | Stage::Publishing
            | Stage::Finished { .. } => "",
        }
    }

    /// Whether the run is live, or its job builds it.
    pub(crate) fn live(&self) -> bool {
        match self.stage {
            Stage::Waiting | Stage::Building | Stage::Publishing | Stage::Running { .. } => true,
            Stage::Finished { .. }
            | Stage::Failed
            | Stage::Interrupted
            | Stage::Lost
            | Stage::Unstarted
            | Stage::Unread => false,
        }
    }

    pub(crate) fn has_result(&self) -> bool {
        match self.stage {
            Stage::Finished { result } => result,
            Stage::Waiting
            | Stage::Building
            | Stage::Publishing
            | Stage::Running { .. }
            | Stage::Failed
            | Stage::Interrupted
            | Stage::Lost
            | Stage::Unstarted
            | Stage::Unread => false,
        }
    }
}

impl Stage {
    fn of(id: &str, listed: &Listed, now: UnixTime, runs: Option<&str>) -> Self {
        let run = match listed {
            Listed::Waiting { .. } => return Self::Waiting,
            Listed::Building { .. } => return Self::Building,
            Listed::Unstarted => return Self::Unstarted,
            Listed::Unread(_) => return Self::Unread,
            Listed::Run(run) => run,
        };
        match &run.ending {
            None => {
                let Some(progress) = &run.progress else {
                    return Self::Publishing;
                };
                let ends = progress.started.saturating_add(progress.hard_end);
                Self::Running {
                    ends_unix_seconds: ends.since_epoch().as_secs(),
                    under: under(id, progress, now, runs),
                }
            }
            Some(Ending::Returned { .. }) => Self::Finished { result: true },
            Some(Ending::NoResult(_)) => Self::Finished { result: false },
            Some(Ending::Stopped(Stopped::Failed(_))) => Self::Failed,
            Some(Ending::Stopped(Stopped::Interrupted(_))) => Self::Interrupted,
            Some(Ending::Lost) => Self::Lost,
        }
    }

    pub(super) fn state(&self) -> State {
        match self {
            Self::Waiting => State::Waiting,
            Self::Building => State::Building,
            Self::Unstarted => State::Unstarted,
            Self::Publishing | Self::Running { .. } => State::Running,
            Self::Finished { .. } => State::Finished,
            Self::Failed => State::Failed,
            Self::Interrupted => State::Interrupted,
            Self::Lost => State::Lost,
            Self::Unread => State::Unread,
        }
    }
}

impl Spent {
    fn of(listed: &Listed) -> Self {
        let Listed::Run(run) = listed else {
            return Self::Unread;
        };
        let Some(progress) = &run.progress else {
            return Self::Unread;
        };
        match progress.cost() {
            None => Self::Unpriced,
            Some(cost) => Self::Priced {
                usd: cost.usd(),
                partly_unpriced: !cost.unpriced_models.is_empty() || cost.unpriced_classes,
                unreported_responses: cost.unreported,
            },
        }
    }
}
