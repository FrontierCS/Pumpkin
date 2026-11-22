//! `pumpkin-admin check-members`: a team's directory of members' keys, as the controller parses
//! them.

use expect_test::expect;

use super::{Scratch, report};

#[test]
fn check_members() {
    let scratch = Scratch::new();
    let key = |fill: &str, comment: &str| {
        format!(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI{} {comment}",
            fill.repeat(43)
        )
    };
    let alice = format!(
        "{}\n\n  {}\n",
        key("A", "alice-laptop"),
        key("B", "alice-desk")
    );
    scratch.write("members/alice.pub", &alice);
    scratch.write("members/README.md", "Each member's keys.\n");
    let ran = |scratch: &Scratch| {
        let output = scratch
            .admin(&["check-members", "members"])
            .output()
            .unwrap();
        report(
            "$ pumpkin-admin check-members members".to_owned(),
            &output,
            &scratch.0,
            false,
        )
    };
    let mut lines = vec![ran(&scratch)];
    scratch.write("members/bob.pub", "ssh-rsa AAAAB3NzaC1yc2E bob\n");
    lines.push(ran(&scratch));
    scratch.write("members/bob.pub", "\n");
    lines.push(ran(&scratch));
    expect![[r#"
        $ pumpkin-admin check-members members
        exit status: Some(0)
        $ pumpkin-admin check-members members
        exit status: Some(1)
        pumpkin: members/bob.pub, line 1: member `bob`'s key is not an ed25519 public key: expected `ssh-ed25519 <68 characters of base64>` and an optional comment of printable ASCII, as `~/.config/pumpkin/ssh/key.pub` holds
        $ pumpkin-admin check-members members
        exit status: Some(1)
        pumpkin: members/bob.pub lists no key: list each of the member's keys on a line, or remove the file"#]].assert_eq(&lines.join("\n"));
}
