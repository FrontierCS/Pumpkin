//! `controller publish`, run by hand: what it refuses before it exports a run.

use expect_test::expect;

use super::{Scratch, Step, check};

#[test]
fn usage() {
    let scratch = Scratch::new();
    let publish = |bucket: &str, since: &str| {
        let args = [
            "controller",
            "publish",
            "--bucket",
            bucket,
            "--region",
            "us-west-1",
        ];
        Step::Run(scratch.pumpkin(&[&args[..], &["--runs", "runs", "--since", since]].concat()))
    };
    check(
        &scratch,
        [
            publish("pumpkin-public", "2026-11-15T00:00:00"),
            publish("pumpkin-public", "1969-12-31T23:59:59Z"),
            publish("Pumpkin_Public", "2026-11-15T00:00:00Z"),
            publish("pumpkin-public", "2026-11-15T00:00:00Z"),
        ],
        false,
        &[],
        &expect![[r#"
            $ pumpkin controller publish --bucket pumpkin-public --region us-west-1 --runs runs --since 2026-11-15T00:00:00
            exit status: Some(2)
            error: invalid value '2026-11-15T00:00:00' for '--since <instant>': `2026-11-15T00:00:00` is no instant: give a date, a time and an offset, as `2026-11-15T00:00:00Z`
            pumpkin: to read how to use it: pumpkin controller publish --help
            $ pumpkin controller publish --bucket pumpkin-public --region us-west-1 --runs runs --since 1969-12-31T23:59:59Z
            exit status: Some(2)
            error: invalid value '1969-12-31T23:59:59Z' for '--since <instant>': `1969-12-31T23:59:59Z` is before 1970: give an instant after it
            pumpkin: to read how to use it: pumpkin controller publish --help
            $ pumpkin controller publish --bucket Pumpkin_Public --region us-west-1 --runs runs --since 2026-11-15T00:00:00Z
            exit status: Some(2)
            error: invalid value 'Pumpkin_Public' for '--bucket <name>': `Pumpkin_Public` is no S3 bucket's name: name one of 3 to 63 lowercase letters, digits, `.` and `-`, which starts and ends with a letter or a digit
            pumpkin: to read how to use it: pumpkin controller publish --help
            $ pumpkin controller publish --bucket pumpkin-public --region us-west-1 --runs runs --since 2026-11-15T00:00:00Z
            exit status: Some(1)
            pumpkin: the runs directory <scratch>/runs isn't there: name the directory of the runs to publish with --runs"#]],
    );
}
