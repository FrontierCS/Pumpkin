//! A run as `ls` lists it, made where the run's record is: the text that `ls` prints for it, and
//! the typed fields that the totals and a reader of a listing need.

use provider::UnixTime;
use runtime::Ending;
use serde::{Deserialize, Serialize};

use super::{Columns, Listed, State, columns, under};

/// A run in a listing, rendered at the time and with the runs directory that it was made with.
#[derive(Serialize, Deserialize)]
pub(crate) struct Row {
    /// What `ls` prints in each column of the run's line.
    pub(crate) cells: Columns,
    /// The lines that `ls` prints under the run's line, each ending in a newline: one for each
    /// call and lease of a live run that hasn't ended.
    pub(super) under: String,
    pub(crate) state: State,
    /// Whether the run finished without a result.
    pub(super) without_result: bool,
    /// When a live run ends at the latest, by its hard end, in seconds since the Unix epoch.
    pub(super) ends_unix_secs: Option<u64>,
    /// What the run cost in US dollars: nothing if no part of it has a price.
    pub(super) cost_usd: Option<f64>,
    /// Whether a part of the run's cost has no price, or its cost isn't known.
    pub(super) unpriced: bool,
    /// How many responses went with tokens unreported ([`runtime::Cost::unreported`]).
    pub(super) unreported_responses: u64,
}

impl Row {
    /// The row of run `id`, `listed`, at `now`, with its commands naming `runs` as its runs
    /// directory.
    pub(crate) fn of(id: &str, listed: &Listed, now: UnixTime, runs: Option<&str>) -> Self {
        let mut row = Self {
            cells: columns(id, listed, now, runs),
            under: under(id, listed, now, runs),
            state: listed.state(),
            without_result: false,
            ends_unix_secs: None,
            cost_usd: None,
            unpriced: false,
            unreported_responses: 0,
        };
        let Listed::Run(run) = listed else {
            return row;
        };
        row.without_result = matches!(run.ending, Some(Ending::NoResult(_)));
        let Some(progress) = &run.progress else {
            return row;
        };
        if run.ending.is_none() {
            let ends = progress.started.saturating_add(progress.hard_end);
            row.ends_unix_secs = Some(ends.since_epoch().as_secs());
        }
        match progress.cost() {
            Some(cost) => {
                row.cost_usd = Some(cost.usd());
                row.unpriced = !cost.unpriced_models.is_empty() || cost.unpriced_classes;
                row.unreported_responses = cost.unreported;
            }
            None => row.unpriced = true,
        }
        row
    }

    /// Whether the run is live, or its job builds it.
    pub(crate) fn live(&self) -> bool {
        match self.state {
            State::Waiting | State::Building | State::Running => true,
            State::Finished
            | State::Failed
            | State::Interrupted
            | State::Lost
            | State::Unstarted => false,
        }
    }

    /// Whether the run has ended with an `outcome.json`, which no later look changes: a lost run
    /// has none, so it is neither live nor ended.
    pub(crate) fn ended(&self) -> bool {
        match self.state {
            State::Finished | State::Failed | State::Interrupted => true,
            State::Waiting | State::Building | State::Running | State::Lost | State::Unstarted => {
                false
            }
        }
    }

    pub(crate) fn has_result(&self) -> bool {
        self.state == State::Finished && !self.without_result
    }
}
