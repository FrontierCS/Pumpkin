use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use expect_test::{Expect, expect};
use retention::KEPT_DAYS;
use retention::installs::{Install, Installs};

use crate::{ago, days, now};

fn name(digit: char) -> String {
    digit.to_string().repeat(64)
}

fn link(digit: char) -> PathBuf {
    PathBuf::from(format!(
        "/home/pumpkin/.pumpkin/{}/bin/pumpkin",
        name(digit)
    ))
}

/// Checks which of `installs` the prune removes. Each is `(digit of its name, whether it is a
/// directory, when it was last used, what it stands for)`. `own` is the digit of the pruning
/// `pumpkin`'s install, and `links` are the links of the jobs that run.
fn check(
    own: char,
    links: Vec<PathBuf>,
    installs: &[(char, bool, SystemTime, &str)],
    expect: &Expect,
) {
    let snapshot = Installs {
        own: Some(name(own)),
        links,
        installs: (installs.iter())
            .map(|&(digit, directory, used, _)| Install {
                name: name(digit),
                directory,
                used,
            })
            .collect(),
    };
    let removed = snapshot.removed(now());
    let fates: Vec<String> = (installs.iter().zip(&snapshot.installs))
        .map(|(&(.., shown), install)| {
            let gone = removed.iter().any(|gone| gone.name == install.name);
            format!("{shown}: {}", if gone { "removed" } else { "kept" })
        })
        .collect();
    expect.assert_eq(&fates.join("\n"));
}

#[test]
fn installs_held_or_used_within_kept() {
    let second = Duration::from_secs(1);
    let idle = ago(days(15));
    check(
        'f',
        vec![
            link('a'),
            link('b'),
            // Links that name no install of the workspace: one points at another directory, and
            // one has no directory part.
            PathBuf::from("/opt/pumpkin/bin/pumpkin"),
            PathBuf::from("pumpkin"),
        ],
        &[
            ('a', true, idle, "run by a job that runs"),
            ('b', true, idle, "run by another job that runs"),
            ('c', true, idle, "idle 15 days"),
            (
                'd',
                true,
                ago(days(KEPT_DAYS) + second),
                "idle just past KEPT",
            ),
            ('e', true, ago(days(KEPT_DAYS)), "idle KEPT"),
            ('f', true, idle, "the pruning pumpkin's own"),
            (
                '1',
                true,
                ago(Duration::ZERO),
                "its mark renewed by a start now",
            ),
            ('2', false, idle, "a decoy, a file named as an install"),
            ('3', true, now() + days(1), "used after now"),
        ],
        &expect![[r#"
            run by a job that runs: kept
            run by another job that runs: kept
            idle 15 days: removed
            idle just past KEPT: removed
            idle KEPT: kept
            the pruning pumpkin's own: kept
            its mark renewed by a start now: kept
            a decoy, a file named as an install: kept
            used after now: kept"#]],
    );
}
