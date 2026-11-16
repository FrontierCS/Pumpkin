//! The words for a role's or a run's model requests, made of a [`runtime::ModelUse`]: how they
//! were answered, what they used and cost, and how long the good answers took.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use super::{cost, span};

pub(super) fn model(
    model: &runtime::ModelUse,
    prices: Option<&BTreeMap<String, runtime::TokenPrices>>,
) -> String {
    let requests = crate::counted(
        usize::try_from(model.requests).unwrap_or(usize::MAX),
        "request",
    );
    let mut line = match answered(model) {
        Some(answered) => format!("{requests}, {answered}"),
        None => requests,
    };
    if let Some(failed) = failed_upstream(model) {
        let _ = write!(line, "; {failed}");
    }
    if !model.dropped_headers.is_empty() {
        let dropped: Vec<&str> = model.dropped_headers.iter().map(String::as_str).collect();
        let _ = write!(line, "; dropped request headers: {}", dropped.join(", "));
        if model.more_dropped_headers {
            line.push_str(", and more");
        }
    }
    let runtime::Tokens {
        reported,
        input,
        output,
        cached,
        cache_write,
        reasoning,
    } = &model.tokens;
    if *reported == 0 {
        if model.requests > 0 {
            line.push_str("; no response reported its tokens");
        }
        line.push_str(&timed(model));
        return line;
    }
    let _ = write!(line, "; tokens {input} in");
    let input_parts: Vec<String> = [
        cached.map(|cached| format!("{cached} cached")),
        cache_write.map(|written| format!("{written} written to the cache")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !input_parts.is_empty() {
        let _ = write!(line, " ({})", input_parts.join(", "));
    }
    let _ = write!(line, ", {output} out");
    if let Some(reasoning) = reasoning {
        let _ = write!(line, " ({reasoning} reasoning)");
    }
    if *reported < model.requests {
        let responses = crate::counted(*reported, "response");
        let _ = write!(line, ", as {responses} reported them");
    }
    if let Some(prices) = prices {
        line.push_str(&cost::tokens(model, prices));
    }
    line.push_str(&timed(model));
    line
}

fn timed(model: &runtime::ModelUse) -> String {
    let Some(took) = median(&model.took) else {
        return String::new();
    };
    let first_token = median(&model.first_tokens).map_or(String::new(), |first_token| {
        let count = model.first_tokens.len();
        format!(
            "{} to the first token (of {count}) and ",
            seconds(first_token)
        )
    });
    let good = crate::counted(model.took.len(), "good answer");
    format!(
        "; of {good}, by the median, {first_token}{} to the end",
        seconds(took)
    )
}

/// The median of `spans`, the mean of the middle two if there are two, or `None` if there are
/// none.
pub(crate) fn median(spans: &[Duration]) -> Option<Duration> {
    if spans.is_empty() {
        return None;
    }
    let mut sorted = spans.to_vec();
    sorted.sort_unstable();
    let middle = sorted.len() / 2;
    Some(if sorted.len().is_multiple_of(2) {
        (sorted[middle - 1] + sorted[middle]) / 2
    } else {
        sorted[middle]
    })
}

/// `span` to the hundredth of a second, as `0.85s`, below a minute, where a model's times fall,
/// and else as [`span`] says it.
fn seconds(span: Duration) -> String {
    if span < Duration::from_mins(1) {
        format!("{:.2}s", span.as_secs_f64())
    } else {
        self::span(span)
    }
}

pub(super) fn answered(model: &runtime::ModelUse) -> Option<String> {
    let answers: Vec<String> = (model.answers.iter())
        .map(|(answer, count)| format!("{count} {answer}"))
        .collect();
    match (model.answers.keys().next(), model.answers.len()) {
        (None, _) => None,
        (Some(answer), 1) => Some(format!("all answered {answer}")),
        _ => Some(format!("answered {}", answers.join(", "))),
    }
}

pub(super) fn failed_upstream(model: &runtime::ModelUse) -> Option<String> {
    let count: u64 = model.failed.values().sum();
    let failed: Vec<(&Option<String>, &u64)> = model.failed.iter().collect();
    match failed[..] {
        [] => None,
        [(Some(code), _)] => Some(format!("{count} failed upstream: {code}")),
        [(None, _)] => Some(format!("{count} failed upstream, with no code")),
        _ => {
            let codes = failed.iter().map(|(code, count)| match code {
                Some(code) => format!("{count} {code}"),
                None => format!("{count} with no code"),
            });
            let codes: Vec<String> = codes.collect();
            Some(format!("{count} failed upstream: {}", codes.join(", ")))
        }
    }
}
