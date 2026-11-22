use std::time::{Duration, SystemTime};

use expect_test::{Expect, expect};
use retention::KEPT_DAYS;
use retention::records::{self, Record};

use crate::{ago, days, now};

fn check(cases: &[(&str, Option<SystemTime>, bool, bool)], expect: &Expect) {
    let records: Vec<Record> = (cases.iter())
        .map(|&(id, copied, running, recorded)| Record {
            id: id.to_owned(),
            copied,
            running,
            recorded,
        })
        .collect();
    let removed = records::removed(&records, now());
    let uncopied = records::uncopied(&records);
    for &(id, copied) in &removed {
        let named = |record: &Record| record.id == id && record.copied == Some(copied);
        assert!(
            records.iter().any(named),
            "{id} is removed as of another copy"
        );
    }
    assert!(
        uncopied
            .iter()
            .all(|id| removed.iter().all(|(gone, _)| gone != id)),
        "a record is both uncopied and removed"
    );
    let lines: Vec<String> = (records.iter())
        .map(|record| {
            let id = record.id.as_str();
            let fate = if removed.iter().any(|&(gone, _)| gone == id) {
                "removed"
            } else if uncopied.contains(&id) {
                "kept, uncopied"
            } else {
                "kept"
            };
            format!("{id}: {fate}")
        })
        .collect();
    expect.assert_eq(&lines.join("\n"));
}

#[test]
fn records_last_until_copied_then_kept() {
    let second = Duration::from_secs(1);
    check(
        &[
            ("copied 15 days ago", Some(ago(days(15))), false, true),
            (
                "copied just past KEPT ago",
                Some(ago(days(KEPT_DAYS) + second)),
                false,
                true,
            ),
            ("copied KEPT ago", Some(ago(days(KEPT_DAYS))), false, true),
            ("copied again yesterday", Some(ago(days(1))), false, true),
            (
                "copied 15 days ago, its job runs",
                Some(ago(days(15))),
                true,
                true,
            ),
            (
                "copied 15 days ago, gone",
                Some(ago(days(15))),
                false,
                false,
            ),
            ("copied after now", Some(now() + days(1)), false, true),
            ("never copied", None, false, true),
            ("never copied, its job runs", None, true, true),
            ("never copied, gone", None, false, false),
        ],
        &expect![[r#"
            copied 15 days ago: removed
            copied just past KEPT ago: removed
            copied KEPT ago: kept
            copied again yesterday: kept
            copied 15 days ago, its job runs: kept
            copied 15 days ago, gone: kept
            copied after now: kept
            never copied: kept, uncopied
            never copied, its job runs: kept
            never copied, gone: kept"#]],
    );
}
