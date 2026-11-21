//! Why a command failed, in the cases that callers branch on. A failure's message says what went
//! wrong and, where the command's own report says it, what to fix; what the failure means to the
//! caller's operation is the caller's to add.

use std::borrow::Cow;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::{fmt, io};

use super::{Bound, Stream};

// How much of the end of a failed command's standard error its failure reports: the end, where a
// program, such as Python with its traceback, says what failed last.
const REPORTED_BYTES: usize = 4096;

/// Why a [`run`](super::run) failed. Each case names the command as [`Run::named`] says.
///
/// [`Run::named`]: super::Run::named
#[derive(Debug)]
pub enum Failure {
    /// It didn't start, for want of what `missing` names, where that is why.
    Unstarted {
        name: String,
        missing: Option<Missing>,
        error: io::Error,
    },
    /// It started but couldn't be followed to its end, as when its output couldn't be read, or
    /// passed what a run captures, and it was killed if it hadn't ended.
    Unwatched { name: String, error: io::Error },
    /// It hadn't ended at `bound`, and was killed then, or never started, as its deadline had
    /// passed.
    Unended { name: String, bound: Bound },
    /// It exited with a status other than 0, or a signal ended it: `output` holds what it wrote to
    /// each stream that was captured, and `stderr` says where its standard error went.
    Exited {
        name: String,
        output: Output,
        stderr: Stream,
    },
}

/// What a command lacked that didn't start.
#[derive(Debug)]
pub enum Missing {
    /// Its program, which isn't on `PATH`, or at the path it names, or can't be executed.
    Program(String),
    /// Its working directory.
    Directory(PathBuf),
}

impl Failure {
    /// The failure of `command`, named `name`, which didn't start for `error`.
    // A spawn fails so when it finds no program, or one it can't execute, and also when the
    // working directory is missing, which xshell tells apart in the same way:
    // https://github.com/matklad/xshell/blob/9a405946ecbbfe50fe4a7229bd764194154e4ce6/src/lib.rs#L1020-L1029
    pub(super) fn unstarted(command: &Command, name: String, error: io::Error) -> Self {
        let lacking = matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
        );
        let missing = lacking.then(|| match command.get_current_dir() {
            Some(dir) if !dir.is_dir() => Missing::Directory(dir.to_owned()),
            Some(_) | None => {
                Missing::Program(command.get_program().to_string_lossy().into_owned())
            }
        });
        Self::Unstarted {
            name,
            missing,
            error,
        }
    }

    /// The end of what an [`Exited`](Self::Exited) command wrote to its standard error, if that
    /// was captured, as a message quotes it: its last 4096 bytes, trimmed, less one trailing
    /// period, which the message's own punctuation follows. Empty for any other failure.
    pub fn reported(&self) -> Cow<'_, str> {
        let Self::Exited {
            output,
            stderr: Stream::Captured,
            ..
        } = self
        else {
            return Cow::Borrowed("");
        };
        let tail = &output.stderr[output.stderr.len().saturating_sub(REPORTED_BYTES)..];
        // From the first whole character, where the cut splits one.
        let split = (tail.iter().take(3))
            .take_while(|&&byte| byte & 0xC0 == 0x80)
            .count();
        match String::from_utf8_lossy(&tail[split..]) {
            Cow::Borrowed(text) => Cow::Borrowed(trimmed(text)),
            Cow::Owned(text) => Cow::Owned(trimmed(&text).to_owned()),
        }
    }
}

fn trimmed(text: &str) -> &str {
    let text = text.trim();
    text.strip_suffix('.').unwrap_or(text)
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unstarted {
                name,
                missing,
                error,
            } => match missing {
                Some(Missing::Program(program)) if program.contains('/') => write!(
                    f,
                    "`{name}` didn't start: {error}: expected a program at `{program}`"
                ),
                Some(Missing::Program(program)) => write!(
                    f,
                    "`{name}` didn't start: {error}: install `{program}` on `PATH`"
                ),
                Some(Missing::Directory(dir)) => write!(
                    f,
                    "`{name}` didn't start in `{}`: {error}: expected that directory to exist",
                    dir.display()
                ),
                None => write!(f, "`{name}` didn't start: {error}"),
            },
            Self::Unwatched { name, error } => write!(f, "running `{name}` failed: {error}"),
            Self::Unended {
                name,
                bound: Bound::Within(timeout),
            } => {
                let secs = timeout.as_secs_f64();
                write!(f, "`{name}` didn't end within {secs} s, so it was killed")
            }
            Self::Unended {
                name,
                bound: Bound::By(_),
            } => write!(f, "the deadline passed before `{name}` ended"),
            Self::Exited {
                name,
                output,
                stderr: Stream::Captured,
            } => {
                let status = output.status;
                match self.reported() {
                    reported if reported.is_empty() => write!(
                        f,
                        "`{name}` failed with {status} and wrote nothing to standard error"
                    ),
                    reported => write!(
                        f,
                        "`{name}` failed with {status}: {reported}: fix what it reports"
                    ),
                }
            }
            Self::Exited {
                name,
                output,
                stderr: Stream::Inherited,
            } => write!(
                f,
                "`{name}` failed with {}: fix what it printed above",
                output.status
            ),
            Self::Exited {
                name,
                output,
                stderr: Stream::Discarded,
            } => write!(f, "`{name}` failed with {}", output.status),
        }
    }
}
