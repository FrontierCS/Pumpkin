//! The values that a run and its providers exchange.
//!
//! Every sandbox of a run carries the run's [`RunTag`], by which a provider lists the run's
//! sandboxes, whatever made them.

/// The tag that every sandbox of a run carries: an ASCII letter or digit, then any of ASCII
/// letters, digits, `_`, `.` and `-`.
#[derive(Clone, Debug)]
pub struct RunTag(String);

impl RunTag {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RunTag {
    type Error = String;

    fn try_from(tag: String) -> Result<Self, Self::Error> {
        // A provider finds a run's sandboxes by the tag, through command lines that may trim or
        // split what they are given, as `docker` trims a filter's value: these pass unchanged,
        // and a tag that starts with a letter or digit can't be read as an option.
        let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-');
        let expected = "use only ASCII letters, digits, `_`, `.` and `-`";
        let Some(first) = tag.chars().next() else {
            return Err(format!("the run tag is empty: {expected}, at least one"));
        };
        if let Some(c) = tag.chars().find(|&c| !allowed(c)) {
            let (tag, c) = (tag.escape_debug(), c.escape_debug());
            return Err(format!("the run tag `{tag}` holds `{c}`: {expected}"));
        }
        if !first.is_ascii_alphanumeric() {
            return Err(format!(
                "the run tag `{tag}` starts with `{first}`: start it with an ASCII letter or digit"
            ));
        }
        Ok(Self(tag))
    }
}
