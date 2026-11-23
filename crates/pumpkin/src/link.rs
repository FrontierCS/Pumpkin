//! The link between a client and its controller: `pumpkin controller serve` ([`crate::serve`]),
//! of the client's own build, on the standard input and output of one `ssh`, in
//! [`schema::frame`]'s frames.
//!
//! `serve` speaks first, a [`Hello`], whose form never changes. Then the client asks one
//! [`Request`] at a time, and each gets its own [`Request::Answer`], whose body, if it has one,
//! follows in frames of its own, or a refusal. A group of uploads gets its answers in order. A
//! follow, a cancel and a reap get theirs streamed, item by item, as each happens, and a refusal
//! may end the stream at any item.
//! A start names the job it starts, so a client whose link ends before an answer is whole may
//! ask again over another.
//!
//! The `ssh` runs [`command`], which runs the `serve` installed below the controller user's home
//! by the SHA-256 of the manifest of the client's executable and the files beside it, so that
//! both ends are always one build, as Bazel keys its install by a digest of what it ships
//! (<https://github.com/bazelbuild/bazel/blob/a765dc177f48efb4c9e1b980609b838c9ba27f42/src/main/cpp/startup_options.cc#L754-L762>).
//! With none there, the command says [`Hello::Absent`], and the client uploads its build
//! ([`installing`]) and links again.

pub(crate) mod copy;
pub(crate) mod store;

use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use members::Member;
use schema::frame::{self, MAX_BODY_BYTES};
use schema::ir::Sha256Digest;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::progress::{Listing, Unlisted};
use crate::reaping::{Reaping, RunReaped};
use crate::{config, manifest, workspace};

// Well past the largest answer: a listing of a few thousand runs is a few MiB, and a body that a
// record keeps is at most the 64 MiB that the gateway takes of a request.
const MAX_ANSWER_BODY_BYTES: u64 = 256 * 1024 * 1024;

// Room for a job's keys and configuration. Copy uses the same budget for its optional file stamps.
pub(crate) const MAX_REQUEST_BODY_BYTES: usize = 16 << 20;

/// What `serve` says first, in a frame with no body.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Hello {
    /// It answers what follows.
    Serving,
    /// The controller has no `serve` of the client's build, so [`command`] printed this.
    Absent,
}

/// A request, which `serve` answers with its [`Request::Answer`], or refuses.
pub(crate) trait Request {
    type Answer;
    /// The answer's head: an enum tagged by `type`, as [`Head`] is, by tags of its own, so that no
    /// other request's answer reads as one.
    type Head: Serialize + DeserializeOwned;

    /// The request as it crosses.
    fn ask(self) -> Ask;

    /// The head of `answer`, and its body, of the length that the head gives.
    fn sent(answer: Self::Answer) -> io::Result<(Self::Head, Vec<u8>)>;

    /// The answer that `head` begins, with its body, if it has one, read from `link`.
    fn received(head: Self::Head, link: &mut Link) -> Result<Self::Answer, Unanswered>;
}

/// A request's head, followed by its body in frames of their own.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Ask {
    Copy(copy::CopyRequest),
    Copied(copy::CopiedRequest),
    Follow {
        id: String,
        log_from_bytes: u64,
        after_cut: bool,
    },
    Health(HealthRequest),
    Jobs(JobsRequest),
    List(ListRequest),
    View(ViewRequest),
    Start(StartRequest),
    Cancel(CancelRequest),
    Reap(ReapRequest),
    Lacks(store::Lacks),
    Put(store::Put),
    Puts {
        blobs: Vec<store::Put>,
    },
}

impl Ask {
    fn buffered_bytes(&self) -> io::Result<usize> {
        let bytes = match self {
            Self::Copy(copy) => copy.held_bytes,
            Self::Start(start) => start.body_bytes,
            Self::Cancel(request) => request.modal_bytes.unwrap_or(0),
            Self::Reap(request) => request.modal_bytes.unwrap_or(0),
            Self::Lacks(request) => request.body_bytes,
            _ => 0,
        };
        if bytes > MAX_REQUEST_BODY_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a request's body holds at most 16 MiB",
            ));
        }
        Ok(bytes)
    }

    /// Reads the buffered body before the request takes effect. A `Put` streams its own body.
    pub(crate) fn read_buffered_body(&self, input: &mut impl Read) -> io::Result<Vec<u8>> {
        let bytes = self.buffered_bytes()?;
        let mut body = Vec::new();
        store::Parts::new(input, bytes as u64).read_to_end(&mut body)?;
        Ok(body)
    }
}

/// A job's log in frame bodies, milestones, then its ending.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Followed {
    Logged,
    Said { body_bytes: u64 },
    Ran { writer: Option<String> },
    Unstarted,
}

/// The head of an answer, or of a refusal.
#[derive(Serialize, Deserialize)]
// The whole of serde's message for a head that is neither, which would otherwise name this type.
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(expecting = "it is neither an answer to the request nor a refusal")]
enum Head<H> {
    /// Nothing took effect, or, in a streamed answer, nothing after the items before it, for the
    /// reason that `message` gives, with what to do.
    Refused { message: String },
    /// The answer's head, which carries its tag itself.
    #[serde(untagged)]
    Answered(H),
}

#[derive(Serialize, Deserialize)]
pub(crate) struct HealthRequest {}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum HealthHead {
    Health(Health),
}

impl Request for HealthRequest {
    type Answer = Health;
    type Head = HealthHead;

    fn ask(self) -> Ask {
        Ask::Health(self)
    }

    fn sent(health: Health) -> io::Result<(HealthHead, Vec<u8>)> {
        Ok((HealthHead::Health(health), Vec::new()))
    }

    fn received(head: HealthHead, _: &mut Link) -> Result<Health, Unanswered> {
        let HealthHead::Health(health) = head;
        Ok(health)
    }
}

/// What the controller's machine reads of itself: how long it has been up, since its PID 1
/// started in a container, its disk and its boot.
/// It judges nothing: it reads the same whether or not the controller can run a job.
#[derive(Serialize, Deserialize)]
pub(crate) struct Health {
    pub(crate) up_seconds: u64,
    /// The size of the disk that holds the runs, as `job::disk` gives it.
    pub(crate) disk_size_bytes: u64,
    /// What an ordinary user may still write of that disk, as `job::disk` gives it.
    pub(crate) disk_free_bytes: u64,
    pub(crate) boot: Boot,
}

/// The state of the controller's boot, the unit `pumpkin-boot.service`, as `systemctl` gives it.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Boot {
    Done,
    Running,
    /// It failed: its main process exited with `status`, at `at`, as `systemctl` words the time,
    /// if it gives one.
    Failed {
        status: i32,
        at: Option<String>,
        log: String,
    },
    /// `systemctl` gave no state.
    Unknown,
}

/// The runs, and the jobs yet to start one, whose ids start with one of `prefixes`, or all with
/// none, as `ls` lists them, with those that ended before [`crate::progress::listed_since`] by
/// `serve`'s clock named only as older.
#[derive(Serialize, Deserialize)]
pub(crate) struct ListRequest {
    pub(crate) prefixes: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ListedHead {
    /// The [`Listed`]'s JSON, in a body of `body_bytes`.
    Listed { body_bytes: u64 },
}

impl Request for ListRequest {
    type Answer = Listed;
    type Head = ListedHead;

    fn ask(self) -> Ask {
        Ask::List(self)
    }

    fn sent(listed: Listed) -> io::Result<(ListedHead, Vec<u8>)> {
        let body = serde_json::to_vec(&listed)?;
        let body_bytes = body.len() as u64;
        Ok((ListedHead::Listed { body_bytes }, body))
    }

    fn received(head: ListedHead, link: &mut Link) -> Result<Listed, Unanswered> {
        let ListedHead::Listed { body_bytes } = head;
        link.json(body_bytes, "its listing")
    }
}

/// The runs that `serve` listed, and those that it couldn't list.
#[derive(Serialize, Deserialize)]
pub(crate) struct Listed {
    pub(crate) listing: Listing,
    pub(crate) unlisted: Vec<Unlisted>,
}

/// The runs' jobs there, as `crate::job::ids` lists them, each with its member.
#[derive(Serialize, Deserialize)]
pub(crate) struct JobsRequest {}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum JobsHead {
    /// The [`Jobs`]' JSON, in a body of `body_bytes`, as it may outgrow a frame's message.
    Jobs { body_bytes: u64 },
}

/// The jobs there, and the member that `serve` acts for.
#[derive(Serialize, Deserialize)]
pub(crate) struct Jobs {
    pub(crate) member: Member,
    pub(crate) jobs: Vec<Job>,
}

/// A run's job, and whom it names as the member who started it.
#[derive(Serialize, Deserialize)]
pub(crate) struct Job {
    pub(crate) id: String,
    pub(crate) owner: Owner,
}

/// The member that a job names.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Owner {
    Member {
        member: Member,
    },
    /// What names it can't be read, for the reason that `error` gives.
    Unread {
        error: String,
    },
}

impl From<Result<Member, String>> for Owner {
    fn from(member: Result<Member, String>) -> Self {
        match member {
            Ok(member) => Self::Member { member },
            Err(error) => Self::Unread { error },
        }
    }
}

impl Request for JobsRequest {
    type Answer = Jobs;
    type Head = JobsHead;

    fn ask(self) -> Ask {
        Ask::Jobs(self)
    }

    fn sent(jobs: Jobs) -> io::Result<(JobsHead, Vec<u8>)> {
        let body = serde_json::to_vec(&jobs)?;
        let body_bytes = body.len() as u64;
        Ok((JobsHead::Jobs { body_bytes }, body))
    }

    fn received(head: JobsHead, link: &mut Link) -> Result<Jobs, Unanswered> {
        let JobsHead::Jobs { body_bytes } = head;
        serde_json::from_slice(&link.body(body_bytes)?)
            .map_err(|error| Unanswered::Failed(format!("its jobs don't read: {error}")))
    }
}

/// The body holds one key per upstream, in name order, the optional Modal file, then the job's
/// JSON. A repeat of the same id leaves the first job alone.
#[derive(Clone, Copy, Serialize, Deserialize)]
pub(crate) struct StartRequest {
    body_bytes: usize,
    credential_bytes: usize,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct JobStart {
    pub(crate) id: String,
    pub(crate) repo: String,
    pub(crate) tree: Sha256Digest,
    pub(crate) install: Sha256Digest,
    pub(crate) words: Vec<String>,
    pub(crate) sent: config::Sent,
    pub(crate) key_bytes: Vec<usize>,
    pub(crate) modal_bytes: Option<usize>,
}

impl StartRequest {
    pub(crate) fn encode(start: &JobStart, body: &mut Vec<u8>) -> Result<Self, String> {
        let credential_bytes = body.len();
        serde_json::to_writer(&mut *body, start)
            .map_err(|error| format!("writing the job's start: {error}"))?;
        Ok(Self {
            body_bytes: body.len(),
            credential_bytes,
        })
    }

    pub(crate) fn decode(self, body: &[u8]) -> Result<(JobStart, &[u8]), String> {
        let (credentials, job) = body
            .split_at_checked(self.credential_bytes)
            .ok_or("the job's credentials exceed its request body")?;
        let start: JobStart = serde_json::from_slice(job)
            .map_err(|error| format!("reading the job's start: {error}"))?;
        let bytes = start
            .key_bytes
            .iter()
            .try_fold(start.modal_bytes.unwrap_or(0), |sum, key| {
                sum.checked_add(*key)
            });
        if bytes != Some(credentials.len()) {
            return Err("the job gives a different length for its credentials".to_owned());
        }
        Ok((start, credentials))
    }
}

pub(crate) enum StartAnswer {
    Started { unrecorded: Option<String> },
    AlreadyStarted,
    Unconfirmed { message: String },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum StartHead {
    /// The job started, though the controller couldn't record that it did, for the reason that
    /// `unrecorded` gives, if any.
    Started {
        unrecorded: Option<String>,
    },
    AlreadyStarted,
    Unconfirmed {
        message: String,
    },
}

impl Request for StartRequest {
    type Answer = StartAnswer;
    type Head = StartHead;

    fn ask(self) -> Ask {
        Ask::Start(self)
    }

    fn sent(answer: StartAnswer) -> io::Result<(StartHead, Vec<u8>)> {
        Ok(match answer {
            StartAnswer::Started { unrecorded } => (StartHead::Started { unrecorded }, Vec::new()),
            StartAnswer::AlreadyStarted => (StartHead::AlreadyStarted, Vec::new()),
            StartAnswer::Unconfirmed { message } => {
                (StartHead::Unconfirmed { message }, Vec::new())
            }
        })
    }

    fn received(head: StartHead, _: &mut Link) -> Result<StartAnswer, Unanswered> {
        match head {
            StartHead::Started { unrecorded } => Ok(StartAnswer::Started { unrecorded }),
            StartHead::AlreadyStarted => Ok(StartAnswer::AlreadyStarted),
            StartHead::Unconfirmed { message } => Ok(StartAnswer::Unconfirmed { message }),
        }
    }
}

/// Stops a job, with the optional Modal credentials needed to reap its run. Unless
/// `any_member`, a job of another member than the one `serve` acts for is refused.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct CancelRequest {
    pub(crate) id: String,
    pub(crate) any_member: bool,
    pub(crate) modal_bytes: Option<usize>,
}

/// What `serve` streams in answer to a [`CancelRequest`]: [`Self::Stopping`] if it signalled the
/// run's driver, then one of the rest, which ends the answer.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum CancelHead {
    /// The run's driver is signalled: the run ends once its sandboxes are deleted.
    Stopping,
    /// The run has ended, and its reap, a [`RunReaped`]'s JSON, follows in a body of
    /// `body_bytes`.
    Ended {
        body_bytes: u64,
    },
    /// The run has ended, but its reap failed before it reached a provider: why.
    EndedUnreaped {
        message: String,
    },
    Unstarted,
    Stopped {
        writer: Option<String>,
    },
}

/// How a cancel ended, as the last [`CancelHead`] of its answer gives it.
pub(crate) enum CancelAnswer {
    Ended { reaped: RunReaped },
    EndedUnreaped { message: String },
    Unstarted,
    Stopped { writer: Option<String> },
}

impl CancelAnswer {
    /// Writes the answer's end, which `serve` follows a [`CancelHead::Stopping`] with, if any.
    pub(crate) fn write(self, output: &mut impl Write) -> io::Result<()> {
        let head = match self {
            Self::Ended { reaped } => {
                return write_json(
                    output,
                    |body_bytes| CancelHead::Ended { body_bytes },
                    &reaped,
                );
            }
            Self::EndedUnreaped { message } => CancelHead::EndedUnreaped { message },
            Self::Unstarted => CancelHead::Unstarted,
            Self::Stopped { writer } => CancelHead::Stopped { writer },
        };
        frame::write(output, &head, &[])
    }

    /// Reads the answer to a cancel, once sent, calling `stopping` if `serve` says that the run
    /// stops; or the message of its refusal.
    pub(crate) fn read(
        link: &mut Link,
        mut stopping: impl FnMut(),
    ) -> Result<Result<Self, String>, Unanswered> {
        loop {
            let head = match link.item_head::<CancelHead>()? {
                Ok(head) => head,
                Err(message) => return Ok(Err(message)),
            };
            let answer = match head {
                CancelHead::Stopping => {
                    stopping();
                    continue;
                }
                CancelHead::Ended { body_bytes } => Self::Ended {
                    reaped: link.json(body_bytes, "its reap of the run")?,
                },
                CancelHead::EndedUnreaped { message } => Self::EndedUnreaped { message },
                CancelHead::Unstarted => Self::Unstarted,
                CancelHead::Stopped { writer } => Self::Stopped { writer },
            };
            return Ok(Ok(answer));
        }
    }
}

/// Reaps the ended or lost runs that this build wrote there, with the optional Modal credentials
/// needed to reach their providers.
#[derive(Clone, Copy, Serialize, Deserialize)]
pub(crate) struct ReapRequest {
    pub(crate) modal_bytes: Option<usize>,
}

/// What `serve` streams in answer to a [`ReapRequest`]: a [`Self::Run`] for each run, as it is
/// reaped, then [`Self::Reaped`].
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ReapHead {
    /// What came of a run, a [`Reaping`]'s JSON, in a body of `body_bytes`.
    Run { body_bytes: u64 },
    /// The reap is whole.
    Reaped,
}

impl ReapRequest {
    /// Writes what came of a run, as the answer's next [`ReapHead::Run`].
    pub(crate) fn write_run(output: &mut impl Write, reaping: &Reaping) -> io::Result<()> {
        write_json(output, |body_bytes| ReapHead::Run { body_bytes }, reaping)
    }

    /// Reads the answer to a reap, once sent, giving `reported` what came of each run as it
    /// arrives; or the message of its refusal, or `reported`'s error.
    pub(crate) fn read(
        link: &mut Link,
        mut reported: impl FnMut(Reaping) -> Result<(), String>,
    ) -> Result<Result<(), String>, Unanswered> {
        loop {
            match link.item_head::<ReapHead>()? {
                Ok(ReapHead::Run { body_bytes }) => {
                    let reaping = link.json(body_bytes, "its reap of a run")?;
                    if let Err(error) = reported(reaping) {
                        return Ok(Err(error));
                    }
                }
                Ok(ReapHead::Reaped) => return Ok(Ok(())),
                Err(message) => return Ok(Err(message)),
            }
        }
    }
}

/// A `GET` of `target`, a path with an optional query, of the view's data of the runs there
/// ([`crate::view`]), whose body may come deflated if `deflate`. What fails, such as a run that
/// isn't there, is answered with the view's status for it, never refused.
#[derive(Serialize, Deserialize)]
pub(crate) struct ViewRequest {
    pub(crate) target: String,
    pub(crate) deflate: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ViewedHead {
    /// The view's reply, whose body is of `body_bytes`.
    Viewed {
        status: String,
        content_type: String,
        deflated: bool,
        body_bytes: u64,
    },
}

impl Request for ViewRequest {
    type Answer = Viewed;
    type Head = ViewedHead;

    fn ask(self) -> Ask {
        Ask::View(self)
    }

    fn sent(viewed: Viewed) -> io::Result<(ViewedHead, Vec<u8>)> {
        let Viewed {
            status,
            content_type,
            deflated,
            body,
        } = viewed;
        let body_bytes = body.len() as u64;
        let head = ViewedHead::Viewed {
            status,
            content_type,
            deflated,
            body_bytes,
        };
        Ok((head, body))
    }

    fn received(head: ViewedHead, link: &mut Link) -> Result<Viewed, Unanswered> {
        let ViewedHead::Viewed {
            status,
            content_type,
            deflated,
            body_bytes,
        } = head;
        let body = link.body(body_bytes)?;
        Ok(Viewed {
            status,
            content_type,
            deflated,
            body,
        })
    }
}

/// The view's reply, as its HTTP gives it.
pub(crate) struct Viewed {
    /// The status line's code and reason.
    pub(crate) status: String,
    pub(crate) content_type: String,
    /// Whether `body` is deflated.
    pub(crate) deflated: bool,
    pub(crate) body: Vec<u8>,
}

/// The message of each frame of an answer's body: the frame's body is the next part of it.
#[derive(Serialize, Deserialize)]
pub(crate) struct Part;

/// Writes the answer to `request` that `answering` gives, or the refusal of the message it
/// returns.
pub(crate) fn answer<R: Request>(
    output: &mut impl Write,
    request: R,
    answering: impl FnOnce(R) -> Result<R::Answer, String>,
) -> io::Result<()> {
    let (head, body) = match answering(request) {
        Ok(answer) => {
            let (head, body) = R::sent(answer)?;
            (Head::Answered(head), body)
        }
        Err(message) => (Head::Refused { message }, Vec::new()),
    };
    frame::write(output, &head, &[])?;
    write_parts(output, &body)
}

pub(crate) fn write_parts(output: &mut impl Write, body: &[u8]) -> io::Result<()> {
    for part in body.chunks(MAX_BODY_BYTES) {
        frame::write(output, &Part, part)?;
    }
    Ok(())
}

/// Writes `head`, which `value`'s JSON is the body of, given its length, then that body.
fn write_json<H: Serialize>(
    output: &mut impl Write,
    head: impl FnOnce(u64) -> H,
    value: &impl Serialize,
) -> io::Result<()> {
    let body = serde_json::to_vec(value)?;
    frame::write(output, &head(body.len() as u64), &[])?;
    write_parts(output, &body)
}

/// Writes the refusal of `message`, which answers no request.
pub(crate) fn refuse(output: &mut impl Write, message: String) -> io::Result<()> {
    frame::write(output, &Head::<()>::Refused { message }, &[])
}

/// The install of the build whose manifest has the SHA-256 `digest`, relative to the controller
/// user's home. Its `bin/` holds the build's files whole. Its `pumpkin` is the build's `serve`, and
/// each job of the build runs it.
pub(crate) fn install(digest: &Sha256Digest) -> String {
    format!("{}/{}", workspace::DIR, digest.hex())
}

/// The command that a link's `ssh` runs on the controller: the `serve` installed there for the
/// build whose manifest has the SHA-256 `digest`, or the frame of [`Hello::Absent`] if it is
/// missing. `serve` takes the shell's positional parameters, which the controller's forced
/// command gives ([`members::Members::authorized_keys`]), as `--member <name>`.
pub(crate) fn command(digest: &Sha256Digest) -> String {
    let mut absent = Vec::new();
    if let Err(error) = frame::write(&mut absent, &Hello::Absent, &[]) {
        unreachable!("a `Vec` takes every byte of a frame with no body: {error}");
    }
    // Each byte as the octal escape that `printf` reads in its format, so that none is the shell's.
    let escaped: Vec<String> = absent.iter().map(|byte| format!("\\{byte:03o}")).collect();
    format!(
        "serve={}/bin/pumpkin\ncd && [ -x \"$serve\" ] && exec \"$serve\" controller serve \"$@\"\nprintf '{}'\n",
        install(digest),
        escaped.concat()
    )
}

/// The script that installs the `tar` archive on standard input as the `serve` that [`command`]
/// runs. Its files must match `manifest`. The script writes them into a directory of its own in
/// the install and checks them. It makes them 0555 and renames the directory into place whole as
/// the install's `bin/`, so that no `serve` or job runs a part of an install. An install that
/// another client renamed there first is as good.
pub(crate) fn installing(manifest: &manifest::Manifest) -> String {
    let install = install(&manifest.name());
    let listing: String = manifest
        .entries()
        .iter()
        .map(|entry| [entry.digest.hex(), "  ", &entry.path, "\n"].concat())
        .collect();
    let names: Vec<&str> = manifest
        .entries()
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    let names = names.join(" ");
    // The files are made in `.made.*` inside the install. That renews the time that the prune of
    // installs reads, and the prune removes what a cut script leaves there, with the install.
    format!(
        r#"install={install}
cd && mkdir --parents "$install" && cd "$install" || exit 1
made=$(mktemp --directory .made.XXXXXX) || exit 1
trap 'rm --recursive --force "$made"' EXIT
tar --extract --directory="$made" -- {names} || exit 1
if ! printf %s '{listing}' | (cd "$made" && sha256sum --check --status); then
    echo "the files that arrived differ from those that this pumpkin hashed, as one changed while they were sent: run the command again" >&2
    exit 1
fi
(cd "$made" && chmod 0555 -- {names}) || exit 1
mv --no-target-directory "$made" bin 2>/dev/null || [ -x bin/pumpkin ] || {{
    echo "moving the files that arrived to ~/$install/bin failed: see that the controller's user may write there, and run the command again" >&2
    exit 1
}}
"#
    )
}

/// Why a link gave no answer.
pub(crate) enum Unanswered {
    /// The frames ended before what was asked for was whole, as when the connection is cut: how.
    /// A repeatable request may be asked again over another link.
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
    /// Runs `ssh`, an `ssh` that runs [`command`], with `stderr` as its standard error, which
    /// `serve`'s goes to as well, and reads its hello: `None` if it is [`Hello::Absent`].
    pub(crate) fn open(mut ssh: Command, stderr: File) -> Result<Option<Self>, Unanswered> {
        let child = stdx::process::spawn(&mut ssh, Stdio::piped(), Stdio::piped(), stderr.into());
        let mut child = child.map_err(Unanswered::Failed)?;
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

    /// Sends one request and its body.
    pub(crate) fn send(&mut self, ask: &Ask, body: &[u8]) -> Result<(), Unanswered> {
        let mut write = || -> io::Result<()> {
            if ask.buffered_bytes()? != body.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the request gives a different length for its body",
                ));
            }
            frame::write(&mut self.stdin, ask, &[])?;
            write_parts(&mut self.stdin, body)
        };
        write().map_err(|error| match error.kind() {
            io::ErrorKind::BrokenPipe => {
                Unanswered::Unreached("ended before the request".to_owned())
            }
            _ => Unanswered::Failed(format!("writing to `ssh` failed: {error}")),
        })
    }

    /// Reads the answer once the whole request has been sent, or the message of its refusal.
    pub(crate) fn answer<R: Request>(&mut self) -> Result<Result<R::Answer, String>, Unanswered> {
        match self.received("the answer")? {
            Head::Refused { message } => Ok(Err(message)),
            Head::Answered(head) => R::received(head, self).map(Ok),
        }
    }

    /// The next frame of a streamed answer, or the peer's refusal at that point.
    pub(crate) fn item<H: DeserializeOwned>(
        &mut self,
    ) -> Result<Result<(H, Vec<u8>), String>, Unanswered> {
        let (head, body) = self.read("the answer")?;
        match head {
            Head::Answered(item) => Ok(Ok((item, body))),
            Head::Refused { message } if body.is_empty() => Ok(Err(message)),
            Head::Refused { .. } => Err(Unanswered::Failed("a refusal has a body".to_owned())),
        }
    }

    /// The next frame of a streamed answer, which has no body, or the peer's refusal at that
    /// point.
    fn item_head<H: DeserializeOwned>(&mut self) -> Result<Result<H, String>, Unanswered> {
        let item = self.item()?;
        match item {
            Ok((head, body)) if body.is_empty() => Ok(Ok(head)),
            Ok((_, body)) => Err(Unanswered::Failed(format!(
                "an item of its answer has a {}-byte body: expected none",
                body.len()
            ))),
            Err(message) => Ok(Err(message)),
        }
    }

    /// The value whose JSON is the body of `body_bytes` that follows, which `what` names.
    fn json<T: DeserializeOwned>(&mut self, body_bytes: u64, what: &str) -> Result<T, Unanswered> {
        let body = self.body(body_bytes)?;
        serde_json::from_slice(&body)
            .map_err(|error| Unanswered::Failed(format!("{what} doesn't read: {error}")))
    }

    /// The body of an answer whose head gives it as of `body_bytes`.
    pub(crate) fn body(&mut self, body_bytes: u64) -> Result<Vec<u8>, Unanswered> {
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

impl Drop for Link {
    fn drop(&mut self) {
        // What it was asked is answered or given up on, and a `serve` still there ends with it.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
