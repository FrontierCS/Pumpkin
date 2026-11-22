use std::path::PathBuf;
use std::time::Duration;

use expect_test::{Expect, expect};
use retention::store::{self, Blob};

use crate::{ago, now};

/// Checks which of `blobs` the prune removes. Each is `(digit, links count, what it stands for)`.
/// Each of `manifests` is the digits that it names, or why it can't be read.
fn check(manifests: &[Result<&str, &str>], blobs: &[(char, u64, &str)], expect: &Expect) {
    let read = (manifests.iter()).map(|manifest| manifest.map(str::chars));
    let blobs: Vec<(Blob<char>, &str)> = (blobs.iter())
        .map(|&(digest, links_count, shown)| {
            let path = PathBuf::from(format!("store/{digest}"));
            (
                Blob {
                    path,
                    digest,
                    links_count,
                },
                shown,
            )
        })
        .collect();
    let (observed, shown): (Vec<Blob<char>>, Vec<&str>) = blobs.into_iter().unzip();
    let lines: Vec<String> = match store::named(read) {
        Err(unread) => vec![format!(
            "every blob kept, as a manifest can't be read: {unread}"
        )],
        Ok(named) => {
            let removed = store::unlinked(&observed, &named);
            (observed.iter().zip(shown))
                .map(|(blob, shown)| {
                    let gone = removed.iter().any(|gone| gone.path == blob.path);
                    format!("{shown}: {}", if gone { "removed" } else { "kept" })
                })
                .collect()
        }
    };
    expect.assert_eq(&lines.join("\n"));
}

#[test]
fn blobs_last_while_linked_or_named() {
    let blobs = [
        ('a', 1, "named by a manifest"),
        ('b', 1, "named by the other manifest"),
        ('c', 2, "linked by a tree"),
        ('d', 1, "neither"),
        ('e', 3, "linked by two trees and named"),
    ];
    check(
        &[Ok("a"), Ok("be")],
        &blobs,
        &expect![[r#"
        named by a manifest: kept
        named by the other manifest: kept
        linked by a tree: kept
        neither: removed
        linked by two trees and named: kept"#]],
    );
    check(
        &[Ok("a"), Err("first"), Ok("be"), Err("second")],
        &blobs,
        &expect!["every blob kept, as a manifest can't be read: second"],
    );
    check(
        &[],
        &blobs,
        &expect![[r#"
        named by a manifest: removed
        named by the other manifest: removed
        linked by a tree: kept
        neither: removed
        linked by two trees and named: kept"#]],
    );
}

#[test]
fn manifests_and_partials_last_a_day() {
    let second = Duration::from_secs(1);
    let day = Duration::from_hours(24);
    let lines: Vec<String> = [
        ("written two days ago", ago(day * 2)),
        ("written just past a day ago", ago(day + second)),
        ("written a day ago", ago(day)),
        ("written now", now()),
        ("written after now", now() + day),
    ]
    .iter()
    .map(|&(shown, written)| {
        let fate = if store::expired(written, now()) {
            "removed"
        } else {
            "kept"
        };
        format!("{shown}: {fate}")
    })
    .collect();
    expect![[r#"
        written two days ago: removed
        written just past a day ago: removed
        written a day ago: kept
        written now: kept
        written after now: kept"#]]
    .assert_eq(&lines.join("\n"));
}
