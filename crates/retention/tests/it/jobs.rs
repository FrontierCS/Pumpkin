use std::time::{Duration, SystemTime};

use expect_test::{Expect, expect};
use retention::KEPT_DAYS;
use retention::jobs::{self, Job};

use crate::{ago, days, now};

fn check(cases: &[(&str, Option<SystemTime>, bool)], expect: &Expect) {
    let observed: Vec<Job> = (cases.iter())
        .map(|&(id, started, recorded)| Job {
            id: id.to_owned(),
            started,
            recorded,
        })
        .collect();
    let removed = jobs::removed(&observed, now());
    let lines: Vec<String> = (observed.iter())
        .map(|job| {
            let tree = if jobs::expired(job.started, now()) {
                "tree removed"
            } else {
                "tree kept"
            };
            let gone = removed.iter().any(|gone| gone.id == job.id);
            let dir = if gone {
                "directory removed"
            } else {
                "directory kept"
            };
            format!("{}: {tree}, {dir}", job.id)
        })
        .collect();
    expect.assert_eq(&lines.join("\n"));
}

#[test]
fn trees_and_directories_last_kept() {
    let second = Duration::from_secs(1);
    let old = Some(ago(days(15)));
    let unrecorded = false;
    check(
        &[
            ("a run of 15 days ago, its record gone", old, unrecorded),
            (
                "a run just past KEPT ago, its record gone",
                Some(ago(days(KEPT_DAYS) + second)),
                unrecorded,
            ),
            (
                "a run KEPT ago, its record gone",
                Some(ago(days(KEPT_DAYS))),
                unrecorded,
            ),
            ("a run of 15 days ago, its record there", old, true),
            (
                "a run of yesterday, its record gone",
                Some(ago(days(1))),
                unrecorded,
            ),
            ("a run after now", Some(now() + days(1)), unrecorded),
            ("a run whose id says no time", None, unrecorded),
        ],
        &expect![[r#"
            a run of 15 days ago, its record gone: tree removed, directory removed
            a run just past KEPT ago, its record gone: tree removed, directory removed
            a run KEPT ago, its record gone: tree kept, directory kept
            a run of 15 days ago, its record there: tree removed, directory kept
            a run of yesterday, its record gone: tree kept, directory kept
            a run after now: tree kept, directory kept
            a run whose id says no time: tree kept, directory kept"#]],
    );
}
