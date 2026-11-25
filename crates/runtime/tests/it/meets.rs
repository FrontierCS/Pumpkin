//! Which failure is a start's, of those of its sandbox's containers' meetings: `gathered`.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::time::Duration;

use expect_test::{Expect, expect};
use schema::ir::CompanionName;

// The runtime's own module, which does no I/O and reads no clock, so a test calls it on values.
// The runtime exports none of it.
#[path = "../../src/sandbox/meets/gathered.rs"]
mod gathered;

use gathered::{Failure, gathered};

/// How a container's meeting ended: with its session, or at a second for a reason that no stop
/// caused.
type Ended = Option<(u64, &'static str)>;

const OPENED: Ended = None;

/// Shows, for each case, each failure with its time, then what the start comes to. A case is how
/// `main`, `scheduler` and `swe-0` met, and the second that the sandbox ended, if it did.
fn check(cases: &[([Ended; 3], Option<u64>)], expect: &Expect) {
    let failure = |(at, reason): (u64, &str)| Failure {
        reason: reason.to_owned(),
        at: Duration::from_secs(at),
        stopped: false,
    };
    let session = |name, ended: Ended| ended.map_or(Ok(name), |ended| Err(failure(ended)));
    let lines = cases.iter().map(|&([main, scheduler, swe], ended)| {
        let ended = ended.map(|at| (at, "it ended"));
        let named = [("main", main), ("scheduler", scheduler), ("swe-0", swe)];
        let failed = (named.into_iter().chain([("the sandbox", ended)]))
            .filter_map(|(name, ended)| Some(format!("{name} at {}s", ended?.0)));
        let companions = named[1..].iter().map(|&(name, ended)| {
            let companion = CompanionName::try_from(name.to_owned()).unwrap();
            (companion, session(name, ended))
        });
        let gathered = gathered(
            session("main", main),
            companions.collect(),
            ended.map(failure),
        );
        let outcome = gathered.map_or_else(|reason| reason, |_| "started".to_owned());
        format!("{}: {outcome}", failed.collect::<Vec<_>>().join(", "))
    });
    expect.assert_eq(&lines.collect::<Vec<_>>().join("\n"));
}

#[test]
fn fails_by_its_first_failure() {
    let untrusted = |at| Some((at, "the gateway can't trust it"));
    check(
        &[
            ([OPENED, untrusted(2), untrusted(2)], None),
            ([OPENED, untrusted(3), untrusted(2)], None),
            ([OPENED, untrusted(1), OPENED], Some(1)),
        ],
        &expect![[r#"
            scheduler at 2s, swe-0 at 2s: its container `scheduler`: the gateway can't trust it
            scheduler at 3s, swe-0 at 2s: its container `swe-0`: the gateway can't trust it
            scheduler at 1s, the sandbox at 1s: its container `scheduler`: the gateway can't trust it"#]],
    );
}
