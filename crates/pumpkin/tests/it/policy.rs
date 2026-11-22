//! `pumpkin-admin policy` prints each principal's IAM policy, as the calls of this build's code on
//! AWS state it. A deployment's review reads it. `deploy/aws` renders each policy from
//! `deploy/aws/policies/<principal>.json.tftpl`, which these tests keep equal to it.

use expect_test::{ExpectFile, expect_file};

use super::Scratch;

/// What `pumpkin-admin policy <arg>` prints. It says nothing else, and succeeds.
fn output(arg: &str) -> String {
    let scratch = Scratch::new();
    let output = (scratch.admin(&["policy", arg])).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

/// Compares `principal`'s policy with `expect`, the module's file of it.
fn check(principal: &str, expect: &ExpectFile) {
    expect.assert_eq(&output(principal));
}

#[test]
fn controller() {
    check(
        "controller",
        &expect_file!["../../../../deploy/aws/policies/controller.json.tftpl"],
    );
}

#[test]
fn timer() {
    check(
        "timer",
        &expect_file!["../../../../deploy/aws/policies/timer.json.tftpl"],
    );
}

#[test]
fn sandbox() {
    check(
        "sandbox",
        &expect_file!["../../../../deploy/aws/policies/sandbox.json.tftpl"],
    );
}

#[test]
fn modal_pull() {
    check(
        "modal-pull",
        &expect_file!["../../../../deploy/aws/policies/modal-pull.json.tftpl"],
    );
}

#[test]
fn client() {
    check(
        "client",
        &expect_file!["../../../../deploy/aws/policies/client.json.tftpl"],
    );
}
