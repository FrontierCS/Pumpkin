use alloc::borrow::ToOwned;
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use crate::Member;

// The length of an ed25519 public key's blob in base64: 51 bytes, unpadded.
const BLOB_CHARS: usize = 68;

/// A members' list, parsed. Each key is `ssh-ed25519 <68 characters of base64>`, with no options
/// of its own, and no other line lists it. It sits under its member, and keeps its comment, if
/// any.
pub struct Members {
    keys: Vec<Key>,
}

struct Key {
    member: Member,
    blob: String,
    comment: Option<String>,
}

impl Members {
    /// Parses `list`, which has a line `<member> ssh-ed25519 <blob> [comment]` for each key. Blank
    /// lines are ignored. A member may list several keys.
    ///
    /// # Errors
    ///
    /// Fails at the first line that isn't such a line, or whose key an earlier line lists, and
    /// gives the line's number.
    pub fn parse(list: &str) -> Result<Self, String> {
        let lines = list.lines().enumerate();
        let lines = lines.filter(|(_, line)| !line.trim().is_empty());
        Self::of_keys(lines.map(|(index, line)| {
            let (name, key) = line.split_once(' ').unwrap_or((line, ""));
            (format!("line {}", index + 1), name, key)
        }))
    }

    /// Parses `keys`, as [`Members::parse`] parses each line of a list. Each item is where the key
    /// is, its member's name, and the key, `ssh-ed25519 <blob> [comment]`. A member may list
    /// several keys.
    ///
    /// # Errors
    ///
    /// Fails at the first key that isn't such a key, of a member whose name is refused, or that
    /// an earlier one lists, and leads with where it is.
    pub fn of_keys<'a>(
        keys: impl IntoIterator<Item = (String, &'a str, &'a str)>,
    ) -> Result<Self, String> {
        let mut listed = Vec::new();
        let mut blobs = BTreeSet::new();
        for (at, name, key) in keys {
            let member = Member::try_from(name.to_owned()).map_err(|why| format!("{at}: {why}"))?;
            let mut words = key.splitn(3, ' ');
            let (kind, blob, comment) = (words.next(), words.next(), words.next());
            let base64 = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/';
            // A word before the key's kind is an option. `command="sh"` would take the place of the
            // member's forced command, and `pty` would undo what `restrict` stops.
            let fits = kind == Some("ssh-ed25519")
                && blob.is_some_and(|blob| blob.len() == BLOB_CHARS && blob.bytes().all(base64))
                && comment.is_none_or(|comment| {
                    comment.bytes().all(|byte| (b' '..=b'~').contains(&byte))
                });
            let Some(blob) = blob.filter(|_| fits) else {
                return Err(format!(
                    "{at}: member `{member}`'s key is not an ed25519 public key: expected \
                     `ssh-ed25519 <{BLOB_CHARS} characters of base64>` and an optional comment \
                     of printable ASCII, as `~/.config/pumpkin/ssh/key.pub` holds"
                ));
            };
            if !blobs.insert(blob) {
                return Err(format!(
                    "{at}: member `{member}`'s key is listed before: list each key once, so that \
                     it names one member"
                ));
            }
            listed.push(Key {
                member,
                blob: blob.to_owned(),
                comment: comment.map(str::to_owned),
            });
        }
        Ok(Self { keys: listed })
    }

    /// The `authorized_keys` lines of the list's keys, one for each, in the list's order.
    ///
    /// Each line has the options `restrict` and a forced command that names the key's member. Then
    /// it has the key. Then it has the member's name, which leads the key's comment, so that
    /// `ssh-keygen -l` names each key's member. The forced command runs the command that the
    /// client asks for with the shell. The shell's `$0` and positional parameters are `pumpkin
    /// --member <member>`, which a `pumpkin` that the command runs takes as `--member <member>`.
    pub fn authorized_keys(&self) -> String {
        let mut lines = String::new();
        for Key {
            member,
            blob,
            comment,
        } in &self.keys
        {
            // `restrict` turns off forwarding, ptys and `~/.ssh/rc`. Neither a member's name nor
            // the forced command holds a `"` or a `\`, apart from the command's own quotes.
            let command = forced(member).replace('"', "\\\"");
            let comment = comment
                .as_ref()
                .map_or_else(String::new, |comment| format!(" {comment}"));
            let _ = writeln!(
                lines,
                "restrict,command=\"{command}\" ssh-ed25519 {blob} {member}{comment}"
            );
        }
        lines
    }
}

// All members share one Unix user, and a forced command names each key's member, as gitolite's
// keys do:
// https://github.com/sitaramc/gitolite/blob/f073598c5dca815a5553cebaee3d2a3f7abcbaec/src/triggers/post-compile/ssh-authkeys#L127-L141
fn forced(member: &Member) -> String {
    format!("exec /bin/sh -c \"$SSH_ORIGINAL_COMMAND\" pumpkin --member {member}")
}
