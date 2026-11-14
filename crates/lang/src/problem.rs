//! The problems an evaluation reports, which read as [`Problems`] says.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::ops::Range;

use crate::Label;
use crate::value::{FileId, Span};

/// `names`, each in backticks, separated by commas, for a message.
pub(crate) fn list(names: &[&str]) -> String {
    let names: Vec<String> = names.iter().map(|name| format!("`{name}`")).collect();
    names.join(", ")
}

/// `n` of `noun`, as `1 name` or `2 names`.
pub(crate) fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// The message that refuses `what`, which Starlark has and the subset leaves out, saying what
/// to write `instead`: every refusal of the subset's is this one's, so that each says it is one.
/// Any other problem is Starlark's own error, a limit the evaluation keeps to, such as how deep
/// it nests, or one of Pumpkin's own, as of a label or a Pumpkin function.
pub(crate) fn refusal(what: impl fmt::Display, instead: impl fmt::Display) -> String {
    format!("{what} is not in Pumpkin's Starlark: {instead}")
}

/// What a check finds wrong at a span, before it reads as a [`Problem`].
#[derive(Debug)]
pub(crate) struct Fault(
    // Boxed, so that the many results that may hold one, on the parser's and the evaluator's
    // deepest paths, stay small.
    Box<Found>,
);

/// What a [`Fault`] holds.
#[derive(Debug)]
struct Found {
    at: Span,
    message: String,
    notes: Vec<Note>,
}

/// A line that says more about a fault: its text, then a place, if it names one.
#[derive(Debug)]
pub(crate) struct Note {
    pub(crate) text: String,
    pub(crate) at: Option<Span>,
}

impl Fault {
    pub(crate) fn new(at: Span, message: impl Into<String>) -> Self {
        Self(Box::new(Found {
            at,
            message: message.into(),
            notes: Vec::new(),
        }))
    }

    pub(crate) fn at(&self) -> Span {
        self.0.at
    }

    #[must_use]
    pub(crate) fn note(mut self, text: impl Into<String>, at: Option<Span>) -> Self {
        self.0.notes.push(Note {
            text: text.into(),
            at,
        });
        self
    }
}

/// What a check of a file's text apart from the evaluation, such as a linter, finds wrong in it.
///
/// It is a message about a span of the text, in bytes, each end on a character's boundary, which
/// [`Plan::lint`](crate::Plan::lint) reads as a problem.
#[derive(Debug)]
pub struct Finding {
    pub span: Range<usize>,
    pub message: String,
}

/// Something wrong in the sources an evaluation read: a fault at a span, or a problem with a
/// whole file.
#[derive(Debug)]
pub(crate) enum Problem {
    At(Fault),
    File { label: Label, message: String },
}

impl Problem {
    /// A problem with the whole file at `label`.
    pub(crate) fn file(label: &Label, message: impl Into<String>) -> Self {
        Self::File {
            label: label.clone(),
            message: message.into(),
        }
    }
}

impl From<Fault> for Vec<Problem> {
    fn from(fault: Fault) -> Self {
        Self::from([Problem::At(fault)])
    }
}

/// Each file an evaluation has read, by its [`FileId`].
#[derive(Clone, Debug, Default)]
pub(crate) struct Sources(Vec<Source>);

#[derive(Clone, Debug)]
struct Source {
    label: Label,
    text: String,
    /// Where each line starts, in bytes, in order: the first at 0.
    lines: Vec<usize>,
}

impl Sources {
    pub(crate) fn add(&mut self, label: Label, text: String) -> FileId {
        let starts = text.match_indices('\n').map(|(newline, _)| newline + 1);
        let lines = core::iter::once(0).chain(starts).collect();
        self.0.push(Source { label, text, lines });
        FileId(self.0.len() - 1)
    }

    pub(crate) fn text(&self, file: FileId) -> &str {
        &self.0[file.0].text
    }

    pub(crate) fn label(&self, file: FileId) -> &Label {
        &self.0[file.0].label
    }

    /// Each file, with its label and text, in the order read.
    pub(crate) fn files(&self) -> impl Iterator<Item = (FileId, &Label, &str)> {
        let files = self.0.iter().enumerate();
        files.map(|(index, source)| (FileId(index), &source.label, source.text.as_str()))
    }

    /// Where `at` starts: `<file>:<line>:<column>`.
    fn place(&self, at: Span) -> String {
        let Location { line, column, .. } = self.locate(at);
        format!("{}:{line}:{column}", self.0[at.file.0].label.path())
    }

    fn locate(&self, at: Span) -> Location<'_> {
        let Source { text, lines, .. } = &self.0[at.file.0];
        // The span's line is the last that starts at or before it; the first starts at 0.
        let line = lines.partition_point(|&start| start <= at.start);
        let start = lines[line - 1];
        let end = lines.get(line).map_or(text.len(), |next| next - 1);
        // Slicing checks that the span lies inside the text, on characters' boundaries.
        let (before, after) = (&text[start..at.start], &text[at.start..at.end]);
        let line_text = &text[start..end];
        Location {
            line,
            column: before.chars().count() + 1,
            before,
            span: after.find('\n').map_or(after, |end| &after[..end]),
            text: line_text.strip_suffix('\r').unwrap_or(line_text),
        }
    }

    /// Writes `fault` as it reads.
    fn write(&self, f: &mut fmt::Formatter<'_>, fault: &Fault) -> fmt::Result {
        let Found { at, message, notes } = &*fault.0;
        let Location {
            line,
            column,
            before,
            span,
            text,
        } = self.locate(*at);
        let file = self.0[at.file.0].label.path();
        write!(f, "{file}:{line}:{column}: {message}")?;
        let width = format!("{line}").len();
        write!(f, "\n    {line} | {text}\n    {:width$} | ", "")?;
        // The marks keep each tab before the span, so that they line up under it.
        let indent = before
            .chars()
            .map(|char| if char == '\t' { '\t' } else { ' ' });
        let carets = core::iter::repeat_n('^', span.chars().count().max(1));
        f.write_str(&indent.chain(carets).collect::<String>())?;
        for Note { text, at } in notes {
            write!(f, "\n    {text}")?;
            if let Some(at) = at {
                write!(f, " {}", self.place(*at))?;
            }
        }
        Ok(())
    }
}

/// Where a span starts in its file's text.
struct Location<'t> {
    line: usize,
    column: usize,
    /// The text of the span's first line before it.
    before: &'t str,
    /// The span's text on its first line.
    span: &'t str,
    /// The text of the span's first line, without its end.
    text: &'t str,
}

/// How many of its problems [`Problems`] shows: the first, then a line that counts the rest,
/// which it still holds.
// Each problem shows its source line, so showing every one could take text quadratic in a line's
// length, and this many keep it linear in the sources.
const MAX_SHOWN: usize = 20;

/// The problems an evaluation found, in the order it found them, never empty.
///
/// Each reads as GNU's form has it: `<file>:<line>:<column>: <message>`, the file being its path
/// below the root, the line and the column counted from 1, the column in Unicode scalar values.
/// Then come the source line, with carets under the span, and notes that say more, such as the
/// place in the IR. A problem with a whole file has its path alone. How many are shown is
/// `MAX_SHOWN`'s.
#[derive(Debug)]
pub struct Problems {
    /// The files that the problems are in.
    sources: Sources,
    problems: Vec<Problem>,
}

impl Problems {
    pub(crate) fn new(sources: Sources, problems: Vec<Problem>) -> Self {
        assert!(
            !problems.is_empty(),
            "an evaluation reports a problem it found"
        );
        Self { sources, problems }
    }

    /// How many problems there are, those that its display counts past the ones it shows
    /// included.
    pub fn count(&self) -> usize {
        self.problems.len()
    }
}

impl fmt::Display for Problems {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, problem) in self.problems.iter().take(MAX_SHOWN).enumerate() {
            if index > 0 {
                f.write_str("\n")?;
            }
            match problem {
                Problem::At(fault) => self.sources.write(f, fault)?,
                Problem::File { label, message } => write!(f, "{}: {message}", label.path())?,
            }
        }
        match self.problems.len().saturating_sub(MAX_SHOWN) {
            0 => Ok(()),
            more => write!(
                f,
                "\nand {}: fix these first, then run again",
                count(more, "more problem")
            ),
        }
    }
}

impl core::error::Error for Problems {}
