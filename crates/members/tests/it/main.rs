//! The members' list as the admin writes it. The tests check what the controller's `sshd` gets for
//! it, which is each key's `authorized_keys` line, or else the refusal of the whole list.

use std::fmt::Write as _;

use expect_test::{Expect, expect};
use members::Members;

// An ed25519 public key's blob in base64, its last 43 characters `fill`.
fn blob(fill: char) -> String {
    format!("AAAAC3NzaC1lZDI1NTE5AAAAI{}", fill.to_string().repeat(43))
}

const FORCED: &str = concat!(
    r#"restrict,command="exec /bin/sh -c \"$SSH_ORIGINAL_COMMAND\""#,
    " pumpkin --member ",
);

fn check(lists: &[String], expect: &Expect) {
    let outcomes: Vec<String> = (lists.iter())
        .map(|list| {
            let outcome = match Members::parse(list) {
                Ok(members) => {
                    let keys = members.authorized_keys();
                    assert!(keys.lines().all(|key| key.starts_with(FORCED)), "{keys}");
                    keys
                }
                Err(why) => format!("refused: {why}\n"),
            };
            let mut shown = String::new();
            for line in list.lines() {
                let _ = writeln!(shown, "{line:?}");
            }
            shown + &outcome
        })
        .collect();
    expect.assert_eq(&outcomes.join("\n"));
}

#[test]
fn each_key_forced_to_its_member() {
    let (a, b, c, d) = (blob('a'), blob('+'), blob('c'), blob('/'));
    let longest = "z".repeat(32);
    check(
        &[
            format!(
                "alice ssh-ed25519 {a} alice@laptop\n\n  \nbob ssh-ed25519 {b}\nalice ssh-ed25519 {c} command=\"sh\" no-pty\ncarol-2 ssh-ed25519 {d}\n"
            ),
            format!("alice ssh-ed25519 {a}\r\n{longest} ssh-ed25519 {b} written on Windows\r\n"),
        ],
        &expect![[r#"
            "alice ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa alice@laptop"
            ""
            "  "
            "bob ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI+++++++++++++++++++++++++++++++++++++++++++"
            "alice ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIccccccccccccccccccccccccccccccccccccccccccc command=\"sh\" no-pty"
            "carol-2 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI///////////////////////////////////////////"
            restrict,command="exec /bin/sh -c \"$SSH_ORIGINAL_COMMAND\" pumpkin --member alice" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa alice alice@laptop
            restrict,command="exec /bin/sh -c \"$SSH_ORIGINAL_COMMAND\" pumpkin --member bob" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI+++++++++++++++++++++++++++++++++++++++++++ bob
            restrict,command="exec /bin/sh -c \"$SSH_ORIGINAL_COMMAND\" pumpkin --member alice" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIccccccccccccccccccccccccccccccccccccccccccc alice command="sh" no-pty
            restrict,command="exec /bin/sh -c \"$SSH_ORIGINAL_COMMAND\" pumpkin --member carol-2" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI/////////////////////////////////////////// carol-2

            "alice ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI+++++++++++++++++++++++++++++++++++++++++++ written on Windows"
            restrict,command="exec /bin/sh -c \"$SSH_ORIGINAL_COMMAND\" pumpkin --member alice" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa alice
            restrict,command="exec /bin/sh -c \"$SSH_ORIGINAL_COMMAND\" pumpkin --member zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI+++++++++++++++++++++++++++++++++++++++++++ zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz written on Windows
        "#]],
    );
}

#[test]
fn key_with_options_refused() {
    let a = blob('a');
    check(
        &[
            format!("bob command=\"sh\" ssh-ed25519 {a}"),
            format!("bob restrict ssh-ed25519 {a}"),
            format!("bob no-pty,command=\"sh\" ssh-ed25519 {a} bob@laptop"),
        ],
        &expect![[r#"
            "bob command=\"sh\" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            refused: line 1: member `bob`'s key is not an ed25519 public key: expected `ssh-ed25519 <68 characters of base64>` and an optional comment of printable ASCII, as `~/.config/pumpkin/ssh/key.pub` holds

            "bob restrict ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            refused: line 1: member `bob`'s key is not an ed25519 public key: expected `ssh-ed25519 <68 characters of base64>` and an optional comment of printable ASCII, as `~/.config/pumpkin/ssh/key.pub` holds

            "bob no-pty,command=\"sh\" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bob@laptop"
            refused: line 1: member `bob`'s key is not an ed25519 public key: expected `ssh-ed25519 <68 characters of base64>` and an optional comment of printable ASCII, as `~/.config/pumpkin/ssh/key.pub` holds
        "#]],
    );
}

#[test]
fn key_not_ed25519_refused() {
    let a = blob('a');
    check(
        &[
            "alice".to_owned(),
            "alice ssh-ed25519".to_owned(),
            format!("ssh-ed25519 {a} alice@laptop"),
            format!("alice ssh-ed25519 {a}\r"),
            format!("alice  ssh-ed25519 {a}"),
            format!("alice ssh-rsa {a}"),
            format!("alice ssh-ed25519 {}", &a[1..]),
            format!("alice ssh-ed25519 {a}a"),
            format!("alice ssh-ed25519 {}=", &a[1..]),
            format!("alice ssh-ed25519 {a} laptop\tkey"),
            format!("alice ssh-ed25519 {a} café"),
        ],
        &expect![[r#"
            "alice"
            refused: line 1: member `alice`'s key is not an ed25519 public key: expected `ssh-ed25519 <68 characters of base64>` and an optional comment of printable ASCII, as `~/.config/pumpkin/ssh/key.pub` holds

            "alice ssh-ed25519"
            refused: line 1: member `alice`'s key is not an ed25519 public key: expected `ssh-ed25519 <68 characters of base64>` and an optional comment of printable ASCII, as `~/.config/pumpkin/ssh/key.pub` holds

            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa alice@laptop"
            refused: line 1: member `ssh-ed25519`'s key is not an ed25519 public key: expected `ssh-ed25519 <68 characters of base64>` and an optional comment of printable ASCII, as `~/.config/pumpkin/ssh/key.pub` holds

            "alice ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r"
            refused: line 1: member `alice`'s key is not an ed25519 public key: expected `ssh-ed25519 <68 characters of base64>` and an optional comment of printable ASCII, as `~/.config/pumpkin/ssh/key.pub` holds

            "alice  ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            refused: line 1: member `alice`'s key is not an ed25519 public key: expected `ssh-ed25519 <68 characters of base64>` and an optional comment of printable ASCII, as `~/.config/pumpkin/ssh/key.pub` holds

            "alice ssh-rsa AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            refused: line 1: member `alice`'s key is not an ed25519 public key: expected `ssh-ed25519 <68 characters of base64>` and an optional comment of printable ASCII, as `~/.config/pumpkin/ssh/key.pub` holds

            "alice ssh-ed25519 AAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            refused: line 1: member `alice`'s key is not an ed25519 public key: expected `ssh-ed25519 <68 characters of base64>` and an optional comment of printable ASCII, as `~/.config/pumpkin/ssh/key.pub` holds

            "alice ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            refused: line 1: member `alice`'s key is not an ed25519 public key: expected `ssh-ed25519 <68 characters of base64>` and an optional comment of printable ASCII, as `~/.config/pumpkin/ssh/key.pub` holds

            "alice ssh-ed25519 AAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa="
            refused: line 1: member `alice`'s key is not an ed25519 public key: expected `ssh-ed25519 <68 characters of base64>` and an optional comment of printable ASCII, as `~/.config/pumpkin/ssh/key.pub` holds

            "alice ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa laptop\tkey"
            refused: line 1: member `alice`'s key is not an ed25519 public key: expected `ssh-ed25519 <68 characters of base64>` and an optional comment of printable ASCII, as `~/.config/pumpkin/ssh/key.pub` holds

            "alice ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa café"
            refused: line 1: member `alice`'s key is not an ed25519 public key: expected `ssh-ed25519 <68 characters of base64>` and an optional comment of printable ASCII, as `~/.config/pumpkin/ssh/key.pub` holds
        "#]],
    );
}

#[test]
fn bad_member_name_refused() {
    let a = blob('a');
    check(
        &[
            format!(" alice ssh-ed25519 {a}"),
            "# the team's keys".to_owned(),
            format!("Alice ssh-ed25519 {a}"),
            format!("1alice ssh-ed25519 {a}"),
            format!("al_ice ssh-ed25519 {a}"),
            format!("{} ssh-ed25519 {a}", "a".repeat(33)),
            format!("alice\tssh-ed25519 {a}"),
        ],
        &expect![[r##"
            " alice ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            refused: line 1: `` is not a member's name: give one of 1 to 32 of a-z, 0-9 and `-`, starting with a letter

            "# the team's keys"
            refused: line 1: `#` is not a member's name: give one of 1 to 32 of a-z, 0-9 and `-`, starting with a letter

            "Alice ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            refused: line 1: `Alice` is not a member's name: give one of 1 to 32 of a-z, 0-9 and `-`, starting with a letter

            "1alice ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            refused: line 1: `1alice` is not a member's name: give one of 1 to 32 of a-z, 0-9 and `-`, starting with a letter

            "al_ice ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            refused: line 1: `al_ice` is not a member's name: give one of 1 to 32 of a-z, 0-9 and `-`, starting with a letter

            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            refused: line 1: `aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa` is not a member's name: give one of 1 to 32 of a-z, 0-9 and `-`, starting with a letter

            "alice\tssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            refused: line 1: `alice\tssh-ed25519` is not a member's name: give one of 1 to 32 of a-z, 0-9 and `-`, starting with a letter
        "##]],
    );
}

#[test]
fn key_listed_twice_refused() {
    let (a, b) = (blob('a'), blob('b'));
    check(
        &[
            format!(
                "alice ssh-ed25519 {a}\nbob ssh-ed25519 {b}\n\ncarol ssh-ed25519 {a} carol@laptop\n"
            ),
            format!("alice ssh-ed25519 {a}\nalice ssh-ed25519 {a}\n"),
        ],
        &expect![[r#"
            "alice ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            "bob ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            ""
            "carol ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa carol@laptop"
            refused: line 4: member `carol`'s key is listed before: list each key once, so that it names one member

            "alice ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            "alice ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            refused: line 2: member `alice`'s key is listed before: list each key once, so that it names one member
        "#]],
    );
}
