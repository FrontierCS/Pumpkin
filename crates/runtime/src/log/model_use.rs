//! A run's model requests, as its log tells them.
//!
//! A `Duration` here is a time since the run started, by the run's clock, but for the lengths in
//! [`ModelUse::took`] and [`ModelUse::first_tokens`].

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use schema::ir::Api;

use super::logged;

/// The model requests of a role, or of the whole run: each has been answered, or is in flight.
#[derive(Clone, Debug, Default)]
pub struct ModelUse {
    /// How many were answered.
    pub requests: u64,
    /// How many answers ended each way. An answer ends with the upstream's status, such as `200`,
    /// or `null` if it gave none. Or it ends as `<status> cut`, `barred`, `refused` or `abandoned`.
    pub answers: BTreeMap<String, u64>,
    /// How many responses stated a failure, by the code they gave, or `None` if they gave none.
    pub failed: BTreeMap<Option<String>, u64>,
    /// How many answers were not a whole response with a 2xx status.
    pub not_2xx: u64,
    /// When the good answer that ended last was answered: a whole response with a 2xx status, whose
    /// stream stated no failure.
    pub last_good: Option<Duration>,
    /// The requests begun and not yet answered, by number, with when each began.
    pub in_flight: BTreeMap<u64, Duration>,
    /// How long each good answer whose start the log records took, from going upstream to its end,
    /// in the order they were answered.
    pub took: Vec<Duration>,
    /// How long each good answer took to its first token of output, if it had one.
    pub first_tokens: Vec<Duration>,
    /// What the responses reported.
    pub tokens: Tokens,
    /// What the answers that name their model reported, by their entry's API and model.
    pub by_model: BTreeMap<(Api, String), Tokens>,
    /// How many responses went with tokens unreported, in whole or in part, keyed as `by_model`.
    /// These are the responses with a 2xx status cut short, whose usage, if any, is only the
    /// stream's so far. They are also those with a 2xx status that came whole and reported none,
    /// and the requests abandoned once handed to the upstream. An error status and the route's own
    /// barred and refused answers are not counted.
    pub unreported: BTreeMap<(Api, String), u64>,
    /// The names of request headers that the route left behind, at most
    /// `route::MAX_DROPPED_HEADERS`.
    pub dropped_headers: BTreeSet<String>,
    /// Whether more names were left behind than `dropped_headers` holds.
    pub more_dropped_headers: bool,
}

/// Tokens summed over the responses that reported them.
///
/// `reported` is how many did, and each `Option` count is `None` if none of those gave it.
/// `cached` and `cache_write` count input tokens read from and written to the provider's cache,
/// which `input` includes.
#[derive(Clone, Debug, Default)]
pub struct Tokens {
    /// How many responses reported their usage.
    pub reported: u64,
    pub input: u64,
    pub output: u64,
    pub cached: Option<u64>,
    pub cache_write: Option<u64>,
    pub reasoning: Option<u64>,
}

pub(super) struct Answered<'e> {
    pub(super) request: u64,
    pub(super) model: Option<(Api, &'e str)>,
    pub(super) dropped_headers: &'e [String],
    pub(super) first_token: Option<Duration>,
    pub(super) upstream_status: Option<u16>,
    pub(super) answer: &'e logged::Answer,
    pub(super) failed: Option<&'e logged::Failed>,
    pub(super) usage: Option<route::Usage>,
}

impl ModelUse {
    pub(super) fn take(&mut self, at: Duration, answered: &Answered<'_>) {
        let Answered {
            request,
            model,
            dropped_headers,
            first_token,
            upstream_status,
            answer,
            failed,
            usage,
        } = answered;
        self.requests += 1;
        (self.dropped_headers).extend(dropped_headers.iter().cloned());
        while self.dropped_headers.len() > route::MAX_DROPPED_HEADERS {
            self.dropped_headers.pop_last();
            self.more_dropped_headers = true;
        }
        let started = self.in_flight.remove(request);
        if let Some(logged::Failed { code }) = failed {
            *self.failed.entry(code.clone()).or_default() += 1;
        }
        let whole_2xx = matches!(answer, logged::Answer::Forwarded)
            && upstream_status.is_some_and(|status| (200..300).contains(&status));
        if !whole_2xx {
            self.not_2xx += 1;
        } else if failed.is_none() {
            // Threads write their answers as they end, which needn't be in the order they ended.
            self.last_good = self.last_good.max(Some(at));
            if let Some(started) = started {
                self.took.push(at.saturating_sub(started));
                if let Some(first_token) = first_token {
                    self.first_tokens.push(first_token.saturating_sub(started));
                }
            }
        }
        // An upstream bills a 2xx response's tokens whether it reported them or not, and an
        // error's not at all. A request abandoned before the head may have reached the provider
        // whole, which may bill its response.
        let two_xx = upstream_status.is_some_and(|status| (200..300).contains(&status));
        let unreported = match answer {
            logged::Answer::Cut => two_xx,
            logged::Answer::Forwarded => two_xx && usage.is_none(),
            logged::Answer::Abandoned => true,
            logged::Answer::Barred | logged::Answer::Refused => false,
        };
        if unreported && let Some((api, model)) = model {
            *self.unreported.entry((*api, (*model).into())).or_default() += 1;
        }
        // As the log gives the status: `null` for none.
        let status = upstream_status.map_or_else(|| "null".to_owned(), |status| status.to_string());
        let answer = match answer {
            logged::Answer::Forwarded => status,
            logged::Answer::Cut => format!("{status} cut"),
            logged::Answer::Barred => "barred".to_owned(),
            logged::Answer::Refused => "refused".to_owned(),
            logged::Answer::Abandoned => "abandoned".to_owned(),
        };
        *self.answers.entry(answer).or_default() += 1;
        if let Some(usage) = usage {
            self.tokens.add(usage);
            if let Some((api, model)) = model {
                let key = (*api, (*model).to_owned());
                self.by_model.entry(key).or_default().add(usage);
            }
        }
    }
}

impl Tokens {
    pub(crate) fn add(&mut self, usage: &route::Usage) {
        self.reported += 1;
        self.input += usage.input_tokens;
        self.output += usage.output_tokens;
        if let Some(cached) = usage.cached_tokens {
            *self.cached.get_or_insert(0) += cached;
        }
        if let Some(written) = usage.cache_write_tokens {
            *self.cache_write.get_or_insert(0) += written;
        }
        if let Some(reasoning) = usage.reasoning_tokens {
            *self.reasoning.get_or_insert(0) += reasoning;
        }
    }
}
