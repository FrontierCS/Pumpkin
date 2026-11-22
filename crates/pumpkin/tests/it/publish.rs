//! `controller publish`, run by hand: its help, and what it refuses before it exports a run.

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
              Upload the runs to the bucket of the public page.

              `publish` exports the runs into `public/` beside `--runs`, as `pumpkin view --export` does. It
              leaves out runs that started before `--since`, and replaces each controller address. It then uploads
              with `aws` what changed since the last publish that uploaded all it planned. It deletes nothing, so
              its credentials need only list the bucket and put objects. One publish runs at a time, and another
              waits for it.

              Usage: pumpkin controller publish [OPTIONS] --bucket <name> --region <region>

              Options:
                    --runs <dir>
                        The directory of the runs to publish (default: `.pumpkin/runs`)

                    --bucket <name>
                        The S3 bucket to upload to

                    --region <region>
                        The bucket's AWS region

                    --since <instant>
                        Leave out the runs that started before this RFC 3339 instant, such as 2026-11-15T00:00:00Z

                    --controller-address <ipv4>
                        An IPv4 address of the controller, which becomes `<controller>` in what is published

                -h, --help
                        Print help

              Example, of the runs copied from a controller:
                pumpkin controller publish --runs records --bucket pumpkin-alice-public \
                  --region us-west-1 --since 2026-11-15T00:00:00Z \
                  --controller-address 172.31.5.6 --controller-address 198.51.100.9
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
