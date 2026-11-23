use alloc::format;
use alloc::string::String;
use core::fmt;

use serde::{Deserialize, Serialize};

/// A member's name: 1 to 32 of a-z, 0-9 and `-`, starting with a letter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Member(String);

impl TryFrom<String> for Member {
    type Error = String;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        let bytes = name.as_bytes();
        let allowed =
            |byte: &u8| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-';
        let fits = (1..=32).contains(&bytes.len())
            && bytes.first().is_some_and(u8::is_ascii_lowercase)
            && bytes.iter().all(allowed);
        if !fits {
            return Err(format!(
                "`{}` is not a member's name: give one of 1 to 32 of a-z, 0-9 and `-`, starting \
                 with a letter",
                name.escape_debug()
            ));
        }
        Ok(Self(name))
    }
}

impl From<Member> for String {
    fn from(member: Member) -> Self {
        member.0
    }
}

impl fmt::Display for Member {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
