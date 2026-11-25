//! What the meetings of a sandbox's containers come to, once each has ended. It takes the time of
//! each failure and reads no clock, so the same meetings always come to the same outcome.

use std::collections::BTreeMap;
use std::time::Duration;

use schema::ir::{CompanionName, Container};

pub(super) struct Failure {
    pub(super) reason: String,
    pub(super) at: Duration,
    /// Whether a stop ended the wait that failed, as a signal or another failure does.
    pub(super) stopped: bool,
}

/// The sessions of `main` and of each companion, in the order declared, if each opened one and
/// the sandbox didn't end (`ended`). Otherwise the start's failure, by its container where the
/// sandbox holds companions, and every session that opened is dropped, which ends it.
///
/// The failure is the earliest that no stop caused, or, where a stop caused each, the earliest.
/// Of two at one time, the container declared first gives it, and the sandbox's end comes last.
pub(super) fn gathered<S>(
    main: Result<S, Failure>,
    companions: Vec<(CompanionName, Result<S, Failure>)>,
    ended: Option<Failure>,
) -> Result<(S, BTreeMap<CompanionName, S>), String> {
    let alone = companions.is_empty();
    let rest = (companions.into_iter())
        .map(|(name, session)| match session {
            Ok(session) => Ok((name, session)),
            Err(failure) => Err((Some(Container::Companion(name)), failure)),
        })
        .chain(ended.map(|failure| Err((None, failure))));
    let mut gathered = main
        .map(|main| (main, BTreeMap::new()))
        .map_err(|failure| (Some(Container::Main), failure));
    let rank = |failure: &Failure| (failure.stopped, failure.at);
    for next in rest {
        gathered = match (gathered, next) {
            (Ok((main, mut companions)), Ok((name, session))) => {
                companions.insert(name, session);
                Ok((main, companions))
            }
            (Ok(_), Err(failed)) => Err(failed),
            (Err(kept), Err(failed)) if rank(&failed.1) < rank(&kept.1) => Err(failed),
            (Err(kept), Ok(_) | Err(_)) => Err(kept),
        };
    }
    gathered.map_err(|(container, failure)| match container {
        Some(container) if !alone => format!("its container `{container}`: {}", failure.reason),
        Some(_) | None => failure.reason,
    })
}
