//! The link between a client and its controller: `pumpkin controller serve` ([`crate::serve`]),
//! of the client's own build, on the standard input and output of one `ssh`, in
//! [`schema::frame`]'s frames.
//!
//! `serve` speaks first, a [`Hello`], whose form never changes. Then the client asks one request
//! at a time ([`Ask`]), and each gets one [`Answer`], whose body, if it has one, follows in frames
//! of its own. A request only reads, so a client whose link ends before an answer is whole may
//! ask again over another.
//!
//! The `ssh` runs [`command`], which runs the `serve` installed below the controller user's home
//! by the SHA-256 of the client's own executable, so that both ends are always one build, as
//! Bazel keys its install by a digest of what it ships
//! (<https://github.com/bazelbuild/bazel/blob/a765dc177f48efb4c9e1b980609b838c9ba27f42/src/main/cpp/startup_options.cc#L754-L762>).
//! With none there, the command says [`Hello::Absent`], and the client uploads its executable
//! ([`installing`]) and links again.

use std::io::{self, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use schema::frame::{self, MAX_BODY_BYTES};
use schema::ir::Sha256Digest;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use stdx::Context as _;

use crate::progress::Listing;
use crate::workspace;

// Well past the largest answer: a listing of a few thousand runs is a few MiB, and a body that a
// record keeps is at most the 64 MiB that the gateway takes of a request.
const MAX_ANSWER_BODY_BYTES: u64 = 256 * 1024 * 1024;

/// What `serve` says first, in a frame with no body.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Hello {
    /// It answers what follows.
    Serving,
    /// The controller has no `serve` of the client's build, so [`command`] printed this.
    Absent,
}

/// A request, in a frame with no body.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Ask {
    Health,
    /// The runs, and the jobs yet to start one, whose ids start with one of `prefixes`, or all
    /// with none, as `ls` lists them, with those that ended before
    /// [`crate::progress::listed_since`] by `serve`'s clock named only as older.
    List {
        prefixes: Vec<String>,
    },
    /// A `GET` of `target`, a path with an optional query, of the view's data of the runs there
    /// ([`crate::view`]), whose body may come deflated if `deflate`.
    View {
        target: String,
        deflate: bool,
    },
}

/// The answer to a request.
pub(crate) enum Answer {
    Health(Health),
    Listed(Listed),
    /// The view's reply, as its HTTP gives it: `status` is a status line's code and reason, and
    /// `body` is deflated if `deflated`.
    Viewed {
        status: String,
        content_type: String,
        deflated: bool,
        body: Vec<u8>,
    },
    /// Nothing took effect, for the reason that `message` gives, with what to do.
    Refused {
        message: String,
    },
}

/// The runs that `serve` listed, and how many it couldn't list, as it said on its standard error.
pub(crate) struct Listed {
    pub(crate) listing: Listing,
    pub(crate) unlisted_count: u64,
}

/// An [`Answer`] as it crosses, in a frame with no body: what of it is a body, if anything, follows
/// in frames of its own.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Head {
    Health(Health),
    /// The listing, a [`Listing`]'s JSON, in a body of `body_bytes`.
    Listed {
        body_bytes: u64,
        unlisted_count: u64,
    },
    /// The view's reply, whose body is of `body_bytes`.
    Viewed {
        status: String,
        content_type: String,
        deflated: bool,
        body_bytes: u64,
    },
    Refused {
        message: String,
    },
}

/// The controller's own health.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Health {
    pub(crate) up_seconds: u64,
    /// The size of the disk that holds the runs, as `job::disk` gives it.
    pub(crate) disk_size_bytes: u64,
    /// What an ordinary user may still write of that disk, as `job::disk` gives it.
    pub(crate) disk_free_bytes: u64,
    pub(crate) boot: Boot,
}

/// The state of the controller's boot, the unit `pumpkin-boot.service`, as `systemctl` gives it.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Boot {
    Done,
    Running,
    /// It failed: its main process exited with `status`, at `at`, as `systemctl` words the time,
    /// if it gives one.
    Failed {
        status: i32,
        at: Option<String>,
    },
    /// `systemctl` gave no state.
    Unknown,
}

/// The message of each frame of an answer's body: the frame's body is the next part of it.
#[derive(Serialize, Deserialize)]
struct Part;

/// Writes `answer`: its head, then its body, if it has one, in frames of at most
/// [`MAX_BODY_BYTES`].
pub(crate) fn send(output: &mut impl Write, answer: &Answer) -> io::Result<()> {
    let listing_json;
    let (head, body): (Head, &[u8]) = match answer {
        Answer::Health(health) => (Head::Health(health.clone()), &[]),
        Answer::Listed(Listed {
            listing,
            unlisted_count,
        }) => {
            listing_json = serde_json::to_vec(listing).unwrap_or_else(|error| {
                unreachable!("a listing, whose maps are keyed by strings, is JSON: {error}")
            });
            let head = Head::Listed {
                body_bytes: listing_json.len() as u64,
                unlisted_count: *unlisted_count,
            };
            (head, &listing_json)
        }
        Answer::Viewed {
            status,
            content_type,
            deflated,
            body,
        } => {
            let head = Head::Viewed {
                status: status.clone(),
                content_type: content_type.clone(),
                deflated: *deflated,
                body_bytes: body.len() as u64,
            };
            (head, body)
        }
        Answer::Refused { message } => {
            let head = Head::Refused {
                message: message.clone(),
            };
            (head, &[])
        }
    };
    frame::write(output, &head, &[])?;
    for part in body.chunks(MAX_BODY_BYTES) {
        frame::write(output, &Part, part)?;
    }
    Ok(())
}

/// The directory of `serve`'s installs, relative to the controller user's home: each is
/// `<sha256>/pumpkin`, by the SHA-256 of the executable.
pub(crate) fn installs() -> String {
    format!("{}/serve", workspace::DIR)
}

fn installed(digest: &Sha256Digest) -> String {
    format!("{}/{}/pumpkin", installs(), digest.hex())
}

/// The command that a link's `ssh` runs on the controller: the `serve` installed there for the
/// executable whose SHA-256 is `digest`, or, if there is none, one that prints the frame of
/// [`Hello::Absent`].
pub(crate) fn command(digest: &Sha256Digest) -> String {
    let mut absent = Vec::new();
    if let Err(error) = frame::write(&mut absent, &Hello::Absent, &[]) {
        unreachable!("a `Vec` takes every byte of a frame with no body: {error}");
    }
    // Each byte as the octal escape that `printf` reads in its format, so that none is the shell's.
    let escaped: Vec<String> = absent.iter().map(|byte| format!("\\{byte:03o}")).collect();
    format!(
        "serve={}\ncd && [ -x \"$serve\" ] && exec \"$serve\" controller serve\nprintf '{}'\n",
        installed(digest),
        escaped.concat()
    )
}

/// The script that installs what its standard input brings, the executable whose SHA-256 is
/// `digest`, as the `serve` that [`command`] runs: written into a directory of its own beside the
/// installs, checked, made 0555 and renamed into place whole, so that no `serve` runs a part of
/// one. An install that another client renamed there first is as good.
pub(crate) fn installing(digest: &Sha256Digest) -> String {
    let (installs, hex) = (installs(), digest.hex());
    format!(
        r#"installs={installs}
cd && mkdir --parents "$installs" && cd "$installs" || exit 1
made=$(mktemp --directory .made.XXXXXX) || exit 1
trap 'rm --recursive --force "$made"' EXIT
cat > "$made/pumpkin" || exit 1
if ! printf '%s  %s\n' {hex} "$made/pumpkin" | sha256sum --check --status; then
    echo "the executable that arrived has a SHA-256 other than {hex}, as it changed while it was sent: run the command again" >&2
    exit 1
fi
chmod 0555 "$made/pumpkin" || exit 1
mv --no-target-directory "$made" {hex} 2>/dev/null || [ -x {hex}/pumpkin ] || {{
    echo "moving the executable that arrived to ~/$installs/{hex} failed: see that the controller's user may write there, and run the command again" >&2
    exit 1
}}
"#
    )
}

/// Why a link gave no answer.
pub(crate) enum Unanswered {
    /// The frames ended before what was asked for was whole, as when the connection is cut: how.
    /// What only reads may be asked again, over another link.
    Unreached(String),
    /// `ssh` didn't run, or what arrived is no frame that `serve` sends, which asking again would
    /// meet again: why.
    Failed(String),
}

/// An `ssh` that runs [`command`], and its standard input and output, until the link is dropped,
/// which ends the `ssh`.
pub(crate) struct Link {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Link {
    /// Runs `ssh`, an `ssh` that runs [`command`], and reads its hello: `None` if it is
    /// [`Hello::Absent`]. A session that `ssh` goes over is the caller's to hold until the link is
    /// dropped.
    pub(crate) fn open(mut ssh: Command) -> Result<Option<Self>, Unanswered> {
        let child = ssh.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn();
        let mut child = (child.context("running `ssh` failed")).map_err(Unanswered::Failed)?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            unreachable!(
                "the child's standard input or output is missing, though both are piped just above"
            )
        };
        let mut link = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        };
        match link.received("its hello")? {
            Hello::Serving => Ok(Some(link)),
            Hello::Absent => Ok(None),
        }
    }

    /// The controller's health, or the message of its refusal.
    pub(crate) fn health(&mut self) -> Result<Result<Health, String>, Unanswered> {
        match self.ask(&Ask::Health)? {
            Answer::Health(health) => Ok(Ok(health)),
            Answer::Refused { message } => Ok(Err(message)),
            Answer::Listed(_) | Answer::Viewed { .. } => Err(other("health")),
        }
    }

    /// The runs whose ids start with one of `prefixes`, as [`Ask::List`] says, or the message of
    /// the refusal.
    pub(crate) fn list(
        &mut self,
        prefixes: &[String],
    ) -> Result<Result<Listed, String>, Unanswered> {
        let ask = Ask::List {
            prefixes: prefixes.to_vec(),
        };
        match self.ask(&ask)? {
            Answer::Listed(listed) => Ok(Ok(listed)),
            Answer::Refused { message } => Ok(Err(message)),
            Answer::Health(_) | Answer::Viewed { .. } => Err(other("list")),
        }
    }

    /// The answer to `ask`.
    pub(crate) fn ask(&mut self, ask: &Ask) -> Result<Answer, Unanswered> {
        if let Err(error) = frame::write(&mut self.stdin, ask, &[]) {
            return Err(match error.kind() {
                io::ErrorKind::BrokenPipe => {
                    Unanswered::Unreached("ended before the request".to_owned())
                }
                _ => Unanswered::Failed(format!("writing to `ssh` failed: {error}")),
            });
        }
        let (body_bytes, unlisted_count) = match self.received("the answer")? {
            Head::Health(health) => return Ok(Answer::Health(health)),
            Head::Refused { message } => return Ok(Answer::Refused { message }),
            Head::Viewed {
                status,
                content_type,
                deflated,
                body_bytes,
            } => {
                return Ok(Answer::Viewed {
                    status,
                    content_type,
                    deflated,
                    body: self.body(body_bytes)?,
                });
            }
            Head::Listed {
                body_bytes,
                unlisted_count,
            } => (body_bytes, unlisted_count),
        };
        let body = self.body(body_bytes)?;
        let listing = serde_json::from_slice(&body)
            .map_err(|error| Unanswered::Failed(format!("its listing doesn't read: {error}")))?;
        Ok(Answer::Listed(Listed {
            listing,
            unlisted_count,
        }))
    }

    /// The body of an answer whose head gives it as of `body_bytes`.
    fn body(&mut self, body_bytes: u64) -> Result<Vec<u8>, Unanswered> {
        if body_bytes > MAX_ANSWER_BODY_BYTES {
            return Err(Unanswered::Failed(format!(
                "its answer's body is of {body_bytes} bytes: expected {} MiB at most",
                MAX_ANSWER_BODY_BYTES / 1024 / 1024
            )));
        }
        let mut body = Vec::new();
        while (body.len() as u64) < body_bytes {
            let (Part, part) = self.read("the answer's body")?;
            let left = body_bytes - body.len() as u64;
            if part.is_empty() || part.len() as u64 > left {
                return Err(Unanswered::Failed(format!(
                    "a part of its answer's body is of {} bytes: expected 1 to {left}, what is \
                     left of the body that the answer gives",
                    part.len()
                )));
            }
            body.extend_from_slice(&part);
        }
        Ok(body)
    }

    /// The message of the next frame, which has no body; `what` names it.
    fn received<T: DeserializeOwned>(&mut self, what: &str) -> Result<T, Unanswered> {
        let (message, body) = self.read(what)?;
        if !body.is_empty() {
            return Err(Unanswered::Failed(format!(
                "{what} has a {}-byte body: expected none",
                body.len()
            )));
        }
        Ok(message)
    }

    fn read<T: DeserializeOwned>(&mut self, what: &str) -> Result<(T, Vec<u8>), Unanswered> {
        match frame::read(&mut self.stdout) {
            Ok(Some(read)) => Ok(read),
            Ok(None) => Err(Unanswered::Unreached(format!("ended before {what}"))),
            Err(error) => Err(match error.kind() {
                io::ErrorKind::UnexpectedEof => {
                    Unanswered::Unreached(format!("ended within {what}"))
                }
                io::ErrorKind::InvalidData => Unanswered::Failed(format!(
                    "{what} is no frame that `pumpkin controller serve` sends: {error}: expected \
                     its frames alone on the standard output there, which a shell's startup file \
                     may write to"
                )),
                _ => Unanswered::Failed(format!("reading {what} from `ssh` failed: {error}")),
            }),
        }
    }
}

fn other(asked: &str) -> Unanswered {
    Unanswered::Failed(format!(
        "it answered `{asked}` with the answer to another request: expected its own, as this \
         pumpkin's `serve` gives it"
    ))
}

impl Drop for Link {
    fn drop(&mut self) {
        // What it was asked is answered or given up on, and a `serve` still there ends with it.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
