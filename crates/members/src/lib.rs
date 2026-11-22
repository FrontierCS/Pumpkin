//! A team's members and their keys. The team's admin keeps the list, a line `<member> <key>` for
//! each key. The controller's `sshd` admits each key by an `authorized_keys` line, which has a
//! forced command that names the key's member. A list is parsed whole or refused whole, so a list
//! with a line that this crate refuses gives no line to `sshd`. The caller decides where the list
//! is kept and which file `sshd` reads.

mod list;
mod member;

pub use list::Members;
pub use member::Member;
