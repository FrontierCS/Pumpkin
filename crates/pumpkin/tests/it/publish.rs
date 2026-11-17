//! `controller publish`, run by hand: which `aws s3` commands upload a runs directory's export,
//! in which order, and with which headers, on a stand-in `aws` that notes each command it is
//! given and uploads nothing ([`AWS`]), so each publish plans what changed since the last.

use std::collections::BTreeMap;
use std::fs;

use expect_test::expect;
use serde_json::Value;

use super::export::{EXPORTED, exported, reports, write_exported};
use super::{Prices, Scratch, Step, asked, check, lease_places, returned, whole_log, write_run};

/// The stand-in `aws`, which answers `--version`, ends every other command at once, as if it ran,
/// and notes each in `calls` beside it, its arguments a line each, ended by an empty line.
const AWS: &str = r#"#!/bin/sh
{ printf '%s\n' "$@"; echo; } >> "${0%/*}/calls"
[ "$*" != --version ] || echo aws-cli/2.38.1 Python/3.14.6 Linux/7.0.0 exe/x86_64.stand-in
"#;

/// A run that started a day before [`EXPORTED`].
const OLDER: &str = "20261109-170000-0000000b";

/// What `pumpkin controller publish` of `scratch`'s default runs directory, of the runs that
/// started at `since` or later, said, and the commands that it gave the stand-in `aws` ([`AWS`])
/// but `--version`, each on a line.
fn publish(scratch: &Scratch, since: &str) -> String {
    let mut publish = scratch.pumpkin(&[
        "controller",
        "publish",
        "--bucket",
        "pumpkin-public",
        "--region",
        "us-west-1",
        "--since",
        since,
        "--controller-address",
        "172.31.5.6",
    ]);
    let path = format!("{}/bin:/usr/bin:/bin", scratch.0.display());
    publish.env("PATH", path);
    let output = publish.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let mut lines = vec![String::from_utf8(output.stderr).unwrap()];
    let calls = scratch.0.join("bin/calls");
    let noted = fs::read_to_string(&calls).unwrap();
    fs::remove_file(calls).unwrap();
    let noted = noted
        .split_terminator("\n\n")
        .map(|call| call.replace('\n', " "));
    let common = "--no-cli-pager --output json --region us-west-1 s3 ";
    let uploads = noted.filter_map(|call| call.strip_prefix(common).map(str::to_owned));
    lines.extend(uploads.map(|upload| format!("aws s3 {upload}")));
    lines.join("\n")
}

#[test]
fn uploads_what_changed() {
    let scratch = Scratch::new("publish-uploads-what-changed");
    scratch.write_executable("bin/aws", AWS.as_bytes());
    let outcome = Some(returned(49.36));
    let mut places = lease_places(Prices::Both);
    for (provider, report) in reports().as_object().unwrap() {
        places["providers"][provider]["report"] = report.clone();
    }
    write_run(
        &scratch,
        OLDER,
        asked(OLDER, 0),
        &whole_log(),
        outcome,
        &places,
    );
    write_exported(&scratch, true, "list the files on 172.31.5.6", &reports());
    let mut said = vec![publish(&scratch, "2026-11-10T00:00:00Z")];
    let files = exported(&scratch.0.join(".pumpkin/public"));
    let named =
        (files.iter()).filter(|(_, body)| String::from_utf8_lossy(body).contains("172.31.5.6"));
    let named: Vec<&String> = named.map(|(key, _)| key).collect();
    assert!(named.is_empty(), "the controller's address is in {named:?}");
    let listed = |files: &BTreeMap<String, Vec<u8>>| {
        let list: Value = serde_json::from_slice(&files["runs.json"]).unwrap();
        let ids = list["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|run| run["id"].to_string());
        format!("runs.json lists: {}", ids.collect::<Vec<_>>().join(", "))
    };
    said.push(listed(&files));
    // Of the page's build, which the page's files name.
    let page = files
        .keys()
        .find_map(|key| key.split_once('/').filter(|(dir, _)| *dir != "runs"));
    let digest = page.unwrap().0.to_owned();
    said.push("again, with nothing changed:".to_owned());
    said.push(publish(&scratch, "2026-11-10T00:00:00Z"));
    said.push("again, since a day before:".to_owned());
    said.push(publish(&scratch, "2026-11-09T00:00:00Z"));
    said.push(listed(&exported(&scratch.0.join(".pumpkin/public"))));
    check(
        &scratch,
        said.into_iter().map(Step::Ran),
        false,
        &[
            (EXPORTED, "<id>"),
            (OLDER, "<older>"),
            (&digest, "<digest>"),
        ],
        &expect![[r#"
            pumpkin: published 1 run to s3://pumpkin-public, and left out 0: the export in <scratch>/.pumpkin/public wrote 98 files, and 7 `aws` commands uploaded what changed

            aws s3 sync <scratch>/.pumpkin/public/runs/<id> s3://pumpkin-public/runs/<id> --exclude * --include exchange/* --include diff/* --include kept/* --exclude *.part --cache-control public, max-age=31536000, immutable --content-encoding deflate --only-show-errors
            aws s3 sync <scratch>/.pumpkin/public/<digest> s3://pumpkin-public/<digest> --exclude *.part --cache-control public, max-age=31536000, immutable --only-show-errors
            aws s3 sync <scratch>/.pumpkin/public/runs/<id> s3://pumpkin-public/runs/<id> --exclude exchange/* --exclude diff/* --exclude kept/* --exclude run.json --exclude *.part --cache-control public, max-age=30 --content-encoding deflate --only-show-errors
            aws s3 cp <scratch>/.pumpkin/public/runs/<id>/run.json s3://pumpkin-public/runs/<id>/run.json --cache-control public, max-age=30 --content-encoding deflate --only-show-errors
            aws s3 cp <scratch>/.pumpkin/public/index.html s3://pumpkin-public/index.html --cache-control public, max-age=30 --only-show-errors
            aws s3 cp <scratch>/.pumpkin/public/robots.txt s3://pumpkin-public/robots.txt --cache-control public, max-age=30 --only-show-errors
            aws s3 cp <scratch>/.pumpkin/public/runs.json s3://pumpkin-public/runs.json --cache-control public, max-age=30 --content-encoding deflate --only-show-errors
            runs.json lists: "<id>"
            again, with nothing changed:
            pumpkin: published 1 run to s3://pumpkin-public, and left out 0: the export in <scratch>/.pumpkin/public wrote 0 files, and 0 `aws` commands uploaded what changed

            again, since a day before:
            pumpkin: published 2 runs to s3://pumpkin-public, and left out 0: the export in <scratch>/.pumpkin/public wrote 47 files, and 4 `aws` commands uploaded what changed

            aws s3 sync <scratch>/.pumpkin/public/runs/<older> s3://pumpkin-public/runs/<older> --exclude * --include exchange/* --include diff/* --include kept/* --exclude *.part --cache-control public, max-age=31536000, immutable --content-encoding deflate --only-show-errors
            aws s3 sync <scratch>/.pumpkin/public/runs/<older> s3://pumpkin-public/runs/<older> --exclude exchange/* --exclude diff/* --exclude kept/* --exclude run.json --exclude *.part --cache-control public, max-age=30 --content-encoding deflate --only-show-errors
            aws s3 cp <scratch>/.pumpkin/public/runs/<older>/run.json s3://pumpkin-public/runs/<older>/run.json --cache-control public, max-age=30 --content-encoding deflate --only-show-errors
            aws s3 cp <scratch>/.pumpkin/public/runs.json s3://pumpkin-public/runs.json --cache-control public, max-age=30 --content-encoding deflate --only-show-errors
            runs.json lists: "<id>", "<older>""#]],
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the help's snapshot")]
fn usage() {
    let scratch = Scratch::new("publish-usage");
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
                        Default: runs/ in the .pumpkin directory that this pumpkin is in

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
