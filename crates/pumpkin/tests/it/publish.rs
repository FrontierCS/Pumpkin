//! `controller publish`, run by hand: its help, and what it refuses before it exports a run.

use expect_test::expect;

use super::{Scratch, Step, check};

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the help's snapshot")]
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
            Step::Run(scratch.pumpkin(&["controller", "publish", "--help"])),
            publish("pumpkin-public", "2026-11-15T00:00:00"),
            publish("pumpkin-public", "1969-12-31T23:59:59Z"),
            publish("Pumpkin_Public", "2026-11-15T00:00:00Z"),
            publish("pumpkin-public", "2026-11-15T00:00:00Z"),
        ],
        false,
        &[],
        &expect![[r#"
            $ pumpkin controller publish --help
            exit status: Some(0)
            printed:
              Upload the runs in --runs to an S3 bucket, from which a static copy of the
              view's page serves them: export them, as `pumpkin view --export` does, but those
              that started before --since, into public/ beside --runs, with each of the
              controller's addresses replaced too; then upload, with `aws`, AWS CLI v2, in
              --region, what changed since the last publish that uploaded all it planned, in
              an order in which no file names one not uploaded yet.
              It deletes nothing, so the credentials that `aws` finds need only list the
              bucket and put objects there. One publish at a time: this one waits for another
              that runs. A run's job on the controller publishes every minute while its
              run is live, and once more as it ends, as [publish] in the controller's settings
              says (`pumpkin help config`).

              Usage: pumpkin controller publish [OPTIONS] --bucket <name> --region <region>

              Options:
                    --runs <dir>
                        The directory of the runs to publish.
                        Default: runs/ in the repository's .pumpkin directory

                    --bucket <name>
                        The S3 bucket to upload to.

                    --region <region>
                        The bucket's AWS region.

                    --since <instant>
                        An instant, as RFC 3339 writes one, such as 2026-11-15T00:00:00Z: a run that
                        started before it, as its id says, is not published.
                        Default: every run

                    --controller-address <ipv4>
                        An IPv4 address of the controller that ran the runs, which becomes
                        <controller> in what is published; given once for each.
                        Default: none

                -h, --help
                        Print this help.

              Example, of the runs copied from a controller:
                pumpkin controller publish --runs records --bucket pumpkin-alice-public --region us-west-1 --since 2026-11-15T00:00:00Z --controller-address 172.31.5.6 --controller-address 198.51.100.9

              Exit status:
                0  Every upload ran.
                1  An error, which a `pumpkin:` line on the standard error says; the next
                   publish uploads what this one didn't.
                2  A usage error.
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
