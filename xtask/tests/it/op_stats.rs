use std::fs;

use expect_test::{Expect, expect};
use stdx::process::{self, Environment};
use test_support::TestDir;

fn check(log: &str, flags: &[&str], expect: &Expect) {
    let run = TestDir::new();
    fs::write(run.path().join("log.jsonl"), log).unwrap();
    let output = process::command(env!("CARGO_BIN_EXE_xtask"), Environment::Only(&[]))
        .arg("op-stats")
        .arg(run.path())
        .args(flags)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stderr, [] as [u8; 0]);
    expect.assert_eq(&String::from_utf8(output.stdout).unwrap());
}

#[test]
fn counts_whole_incoming_lines() {
    check(
        concat!(
            r#"{"at_ns":"1","event":{"kind":"op_arrived","op":1,"caller":"job-17","#,
            r#""request":{"kind":"lease_run","argv":["echo","雪"]}}}"#,
            "\r\n",
            r#"{"at_ns":"2","event":{"kind":"op_arrived","op":2,"caller":"agent","#,
            r#""request":{"kind":"lease_status"}}}"#,
            "\n",
            r#"{"at_ns":"3","event":{"kind":"tick"},"actions":[{"kind":"reply","#,
            r#""reply":{"kind":"lease_status"}}]}"#,
            "\n",
            "malformed\n",
            r#"{"at_ns":"4","event":{"kind":"op_arrived","caller":"job-17"}}"#,
            "\n",
            r#"{"at_ns":"5","event":{"kind":"op_arrived","op":3,"caller":"job-17","#,
            r#""request":{"kind":"lease_status"}}}"#,
        ),
        &["--caller", "job-17"],
        &expect![[r#"
            Log size: 498 bytes
            Unread complete lines: 2 (72 bytes)
            Partial final line: 102 bytes
            Incoming op requests from "job-17"
            Request kind                  Count    Log bytes
            "lease_run"                       1          123
            Total                             1          123
        "#]],
    );
}
