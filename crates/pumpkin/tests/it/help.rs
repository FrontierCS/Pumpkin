//! The help of each command and each topic, one test each: what `pumpkin` prints is compared
//! with a file in `help/`, which `cargo xtask refill` rewrites.

use expect_test::{ExpectFile, expect_file};

use super::Scratch;

// Each command and each topic has a file of its own, as in cargo and rustup. A test asserts what
// both assert: a success, and an empty standard error.
// https://github.com/rust-lang/cargo/blob/111443f3dc4d82a65e2306a93028f122406401c1/tests/testsuite/cargo_build/help/mod.rs#L5-L14
// https://github.com/rust-lang/rustup/blob/08f60b0a9f445bf25153692139700a2cbc1a869b/tests/suite/cli_rustup_ui.rs#L6-L26
fn check(args: &[&str], expect: &ExpectFile) {
    let scratch = Scratch::new();
    let output = scratch.pumpkin(args).output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8(output.stderr).unwrap(), "");
    expect.assert_eq(&String::from_utf8(output.stdout).unwrap());
}

#[test]
fn pumpkin_help() {
    check(&["--help"], &expect_file!["help/pumpkin.txt"]);
}

#[test]
fn check_help() {
    check(&["check", "--help"], &expect_file!["help/check.txt"]);
}

#[test]
fn fmt_help() {
    check(&["fmt", "--help"], &expect_file!["help/fmt.txt"]);
}

#[test]
fn run_help() {
    check(&["run", "--help"], &expect_file!["help/run.txt"]);
}

#[test]
fn wait_help() {
    check(&["wait", "--help"], &expect_file!["help/wait.txt"]);
}

#[test]
fn ls_help() {
    check(&["ls", "--help"], &expect_file!["help/ls.txt"]);
}

#[test]
fn cancel_help() {
    check(&["cancel", "--help"], &expect_file!["help/cancel.txt"]);
}

#[test]
fn view_help() {
    check(&["view", "--help"], &expect_file!["help/view.txt"]);
}

#[test]
fn help_help() {
    check(&["help", "--help"], &expect_file!["help/help.txt"]);
}

#[test]
fn build_help() {
    check(&["build", "--help"], &expect_file!["help/build.txt"]);
}

#[test]
fn reap_help() {
    check(&["reap", "--help"], &expect_file!["help/reap.txt"]);
}

#[test]
fn controller_help() {
    check(
        &["controller", "--help"],
        &expect_file!["help/controller.txt"],
    );
}

#[test]
fn controller_job_help() {
    let expect = expect_file!["help/controller_job.txt"];
    check(&["controller", "job", "--help"], &expect);
}

#[test]
fn controller_boot_help() {
    let expect = expect_file!["help/controller_boot.txt"];
    check(&["controller", "boot", "--help"], &expect);
}

#[test]
fn controller_cancel_help() {
    let expect = expect_file!["help/controller_cancel.txt"];
    check(&["controller", "cancel", "--help"], &expect);
}

#[test]
fn controller_serve_help() {
    let expect = expect_file!["help/controller_serve.txt"];
    check(&["controller", "serve", "--help"], &expect);
}

#[test]
fn config_topic() {
    check(&["help", "config"], &expect_file!["help/config.txt"]);
}
