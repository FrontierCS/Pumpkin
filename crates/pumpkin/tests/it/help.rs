//! The help of each command and each topic, one test each: what `pumpkin` prints with its colour
//! forced is drawn as an SVG and compared with a file in `help/`, which `cargo xtask refill`
//! rewrites.

use std::process::Command;

use expect_test::{ExpectFile, expect_file};

use super::Scratch;

// Each command and each topic has a file of its own, as in cargo and rustup. A test asserts what
// both assert: a success, and an empty standard error.
// https://github.com/rust-lang/cargo/blob/111443f3dc4d82a65e2306a93028f122406401c1/tests/testsuite/cargo_build/help/mod.rs#L5-L14
// https://github.com/rust-lang/rustup/blob/08f60b0a9f445bf25153692139700a2cbc1a869b/tests/suite/cli_rustup_ui.rs#L6-L26
//
// The help's colour is forced, as cargo's tests force cargo's. The help is drawn as an SVG with
// anstyle-svg, which snapbox uses for cargo's `.term.svg` files. A pipe gets the text unstyled.
// https://github.com/rust-lang/cargo/blob/111443f3dc4d82a65e2306a93028f122406401c1/tests/testsuite/utils/ext.rs#L41-L48
// https://github.com/rust-lang/cargo/blob/111443f3dc4d82a65e2306a93028f122406401c1/tests/testsuite/cargo_build/help/mod.rs#L12
// https://github.com/assert-rs/snapbox/blob/62b0cda401c0d8a49bedb4f1f727eb762775f354/crates/snapbox/src/data/mod.rs#L735-L738
fn check(args: &[&str], expect: &ExpectFile) {
    drawn(Scratch::pumpkin, args, expect);
}

fn admin(args: &[&str], expect: &ExpectFile) {
    drawn(Scratch::admin, args, expect);
}

fn drawn(program: fn(&Scratch, &[&str]) -> Command, args: &[&str], expect: &ExpectFile) {
    let scratch = Scratch::new();
    let print = |colour: Option<&str>| {
        let mut pumpkin = program(&scratch, args);
        pumpkin.envs(colour.map(|forced| ("CLICOLOR_FORCE", forced)));
        let output = pumpkin.output().unwrap();
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(String::from_utf8(output.stderr).unwrap(), "");
        String::from_utf8(output.stdout).unwrap()
    };
    let styled = print(Some("1"));
    let plain = print(None);
    assert_eq!(plain, anstream::adapter::strip_str(&styled).to_string());
    expect.assert_eq(&anstyle_svg::Term::new().render_svg(&styled));
}

#[test]
fn pumpkin_help() {
    check(&["--help"], &expect_file!["help/pumpkin.svg"]);
}

#[test]
fn check_help() {
    check(&["check", "--help"], &expect_file!["help/check.svg"]);
}

#[test]
fn fmt_help() {
    check(&["fmt", "--help"], &expect_file!["help/fmt.svg"]);
}

#[test]
fn run_help() {
    check(&["run", "--help"], &expect_file!["help/run.svg"]);
}

#[test]
fn wait_help() {
    check(&["wait", "--help"], &expect_file!["help/wait.svg"]);
}

#[test]
fn ls_help() {
    check(&["ls", "--help"], &expect_file!["help/ls.svg"]);
}

#[test]
fn cancel_help() {
    check(&["cancel", "--help"], &expect_file!["help/cancel.svg"]);
}

#[test]
fn view_help() {
    check(&["view", "--help"], &expect_file!["help/view.svg"]);
}

#[test]
fn help_help() {
    check(&["help", "--help"], &expect_file!["help/help.svg"]);
}

#[test]
fn reap_help() {
    check(&["reap", "--help"], &expect_file!["help/reap.svg"]);
}

#[test]
fn controller_help() {
    check(
        &["controller", "--help"],
        &expect_file!["help/controller.svg"],
    );
}

#[test]
fn admin_help() {
    admin(&["--help"], &expect_file!["help/admin.svg"]);
}

#[test]
fn admin_install_help() {
    let expect = expect_file!["help/admin_install.svg"];
    admin(&["install", "--help"], &expect);
}

#[test]
fn admin_check_help() {
    let expect = expect_file!["help/admin_check.svg"];
    admin(&["check", "--help"], &expect);
}

#[test]
fn admin_policy_help() {
    let expect = expect_file!["help/admin_policy.svg"];
    admin(&["policy", "--help"], &expect);
}

#[test]
fn admin_check_members_help() {
    let expect = expect_file!["help/admin_check_members.svg"];
    admin(&["check-members", "--help"], &expect);
}

#[test]
fn config_topic() {
    check(&["help", "config"], &expect_file!["help/config.svg"]);
}
