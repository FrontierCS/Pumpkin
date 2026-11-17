//! Labels: how the workflow language names a file, below the task's root or the standard
//! library's.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A place below a [`Root`], which names no place outside it.
///
/// It is `//`, the task's root, or `@pumpkin//`, the standard library's, then `/`-separated
/// components of ASCII letters, digits, `.`, `_` and `-`, none of them empty, `.` or `..`.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize
)]
#[serde(try_from = "String", into = "String")]
pub struct Label {
    root: Root,
    path: String,
}

/// The root that a [`Label`] names a place below.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Root {
    /// The root of the task's repository, which the entry is below.
    Task,
    /// Pumpkin's standard library, whose files load only files of its own, so that it depends
    /// on no task.
    Library,
}

/// What starts a label of the standard library, [`Root::Library`].
// As Bazel names a repository's root, `@rules_oci//` (https://bazel.build/concepts/labels), and
// Buck2 the cell of its bundled prelude, `prelude//`:
// https://github.com/facebook/buck2/blob/e3dc98f84acf5f70c40b857819556baa7ec9c929/docs/users/advanced/external_cells.md#L50-L56
const LIBRARY: &str = "@pumpkin//";

impl Label {
    /// Parses a label, such as `//lib/bench.star` or `@pumpkin//harness/codex/codex.star`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (root, path) = if let Some(path) = text.strip_prefix(LIBRARY) {
            (Root::Library, path)
        } else if let Some(path) = text.strip_prefix("//") {
            (Root::Task, path)
        } else if text.starts_with('@') && text.contains("//") {
            return Err(format!(
                "`{}` names a root Pumpkin has none of: start it with `//`, the task's root, or \
                 `{LIBRARY}`, the standard library's",
                text.escape_debug()
            ));
        } else {
            return Err(format!(
                "`{}` is no label: start it with `//`, the root",
                text.escape_debug()
            ));
        };
        Self::below(root, String::from(path)).ok_or_else(|| {
            let start = &text[..text.len() - path.len()];
            format!(
                "`{}` is no label: after `{start}`, write `/`-separated components of ASCII \
                 letters, digits, `.`, `_` and `-`, none of them empty, `.` or `..`",
                text.escape_debug()
            )
        })
    }

    /// The label of `path` below `root`, if it [is a path](Self::is_path) a label may have.
    fn below(root: Root, path: String) -> Option<Self> {
        Self::is_path(&path).then_some(Self { root, path })
    }

    /// Whether `path` is one a label may have below its root: `/`-separated components of ASCII
    /// letters, digits, `.`, `_` and `-`, none of them empty, `.` or `..`.
    pub fn is_path(path: &str) -> bool {
        path.split('/').all(is_component)
    }

    pub fn root(&self) -> Root {
        self.root
    }

    /// The path below its root, such as `lib/bench.star`.
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let start = match self.root {
            Root::Task => "//",
            Root::Library => LIBRARY,
        };
        write!(f, "{start}{}", self.path)
    }
}

fn is_component(component: &str) -> bool {
    !matches!(component, "" | "." | "..")
        && (component.chars())
            .all(|char| char.is_ascii_alphanumeric() || matches!(char, '.' | '_' | '-'))
}

impl TryFrom<String> for Label {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<Label> for String {
    fn from(label: Label) -> Self {
        label.to_string()
    }
}
