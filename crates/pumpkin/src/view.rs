//! `pumpkin view`: the runs' records served to a browser, from this machine or through the view on
//! a controller.
//!
//! A [`View`] holds each run it has been asked about between requests, as a fold of its log that
//! is read further on each request, so that a page that polls reads only what the log has grown
//! by. Its replies are JSON, built here and in [`json`](mod@json), [`talk`] and [`trace`]. The view
//! that asks over a link gets the same replies ([`Answering`]).
//!
//! A run that has ended is listed from its record, read once, as an ended record doesn't change;
//! one that is lost, whose driver ended without an outcome, is read again until a reap writes it.
//! The view names a record only by its id as a [`RunTag`], below the runs directory, so that no
//! path a request gives reaches the disk.
//!
//! A request is answered only if it names a loopback `Host` ([`http`]) and carries the token that
//! `pumpkin view` printed, bar the page's own files; a run's cancel and the controller's start,
//! the requests that change anything, are also refused unless they come from the page's own
//! origin.

mod asked;
mod cancel;
mod failure;
mod http;
mod json;
mod page;
mod talk;
mod trace;

use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::fs::File;
use std::io::{self, Read as _};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use provider::RunTag;
use runtime::{Ending, LogFold, ReadError, Readout, Record};
use rustix::event::{self, PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use serde_json::{Value, json};
use stdx::Context;

use self::asked::{Asked, START, allowed, asked};
use self::failure::Failures;
use self::http::{Method, Request, Response, Sent};
use self::json::{listed_json, live_row, record_json, run_json};
use self::talk::Talk;
use crate::cli::Exit;
use crate::controller::{Unviewed, Viewing};
use crate::link::{ViewRequest, Viewed};
use crate::progress::{self, Listed};
use crate::{cli, config, next, say};

/// The most runs in one reply of the run list; `next` says where the following reply begins.
const LIST_PAGE_RUNS: usize = 100;

/// The most bytes of log in one window of a run's log, but for a single line longer than that.
///
/// A page reads a log one window at a time, each a round trip, so the window is large enough to
/// read a log in few trips over a slow link, and small enough that the JSON of one answer stays a
/// few MiB.
const LOG_WINDOW_BYTES: u64 = 1024 * 1024;

/// How many of a run's latest log lines are kept in memory, to answer a poll without reading the
/// file: a live run's log gains a few between two polls.
const RECENT_LINES: usize = 100;

/// How many runs that have ended `View` keeps held; the one read longest ago goes first. A run's
/// page is read again and again while it is open, and a person opens a few at a time.
const HELD_ENDED_RUNS: usize = 8;

/// Serves the runs in the directory `flags` name, or else, with `[controllers.<name>]` in the
/// configuration, a controller's runs, until the process is stopped.
pub(crate) fn view(flags: cli::View) -> Result<Exit, String> {
    let cli::View {
        runs,
        controller,
        port,
    } = flags;
    let (source, serving) = if runs.is_none()
        && let Some(client) = config::client(None, controller.as_ref())?
    {
        let viewing = Viewing::reach(&client)?;
        let serving = format!("controller `{}`'s runs", viewing.name());
        (Source::Controller(Box::new(viewing)), serving)
    } else {
        let (runs, named) = crate::runs_dir(runs)?;
        let serving = format!("the runs in {runs}");
        let view = Box::new(View::new(
            PathBuf::from(&runs),
            next::At::runs(named.as_deref()),
        ));
        let runs_given = runs;
        (Source::Here { view, runs_given }, serving)
    };
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port.unwrap_or(0)))
        .context("binding the view's port on 127.0.0.1 failed")?;
    let port = listener.local_addr().context("the view's port")?.port();
    let token = token()?;
    // The address alone on its line, as a next command is, so that a word-wise selection or a
    // double-click takes it whole, as Jupyter's server prints its own:
    // https://github.com/jupyter-server/jupyter_server/blob/0d1a13b2f5212a9f9da0122a9e7dbaf23f6cfcb7/jupyter_server/serverapp.py#L3202-L3206
    say(format_args!(
        "serving {serving} to this machine alone, until stopped, at\n    \
         http://127.0.0.1:{port}/?t={token}"
    ));
    let mut local = Local {
        source,
        token,
        port,
        cancels: cancel::Cancels::default(),
        page: page::Page::new(),
    };
    match serve(&listener, &mut local)? {}
}

/// The view of the runs below a controller's home that `serve` keeps for a link's life, which
/// holds what it has read of the runs that are live, and lists each run with its member.
pub(crate) struct Answering(View);

impl Answering {
    pub(crate) fn new(home: &Path) -> Self {
        // The command that runs a run again from here would name the controller's paths.
        Self(View {
            again: false,
            jobs: Some(home.join(crate::job::JOBS)),
            ..View::new(home.join(crate::job::runs()), next::At::Default)
        })
    }

    /// The answer to `request`, a `GET` of its `target`, as the page's requests are answered, with
    /// its body deflated if it asks to `deflate` and that is shorter, and its commands to run next
    /// naming the controller as its `controller`.
    pub(crate) fn answer(&mut self, request: ViewRequest) -> Viewed {
        let ViewRequest {
            target,
            deflate,
            controller,
        } = request;
        let target = &target[..];
        self.0.at = next::At::Controller(controller);
        let response = match Request::get(target, deflate) {
            Ok(request) => api(&request, &mut self.0),
            Err(refused) => refused,
        };
        // A page's file, the one answer a browser may keep, is never asked for here.
        let Sent {
            status,
            content_type,
            body,
            deflated,
            immutable: _,
        } = response.sent(deflate);
        Viewed {
            status: status.into_owned(),
            content_type: content_type.into_owned(),
            deflated,
            body,
        }
    }
}

struct Local {
    source: Source,
    token: String,
    /// The port the view is served at, which its page's `Origin` names.
    port: u16,
    cancels: cancel::Cancels,
    page: page::Page,
}

enum Source {
    /// The runs in a directory here, which a cancel names as `runs_given`, as it was given, or
    /// found by default.
    Here { view: Box<View>, runs_given: String },
    /// The controller's runs, which its `serve` answers requests for.
    Controller(Box<Viewing>),
}

/// The runs of one directory, as the view reads them.
struct View {
    runs: PathBuf,
    /// Where the commands to run next find the runs.
    at: next::At,
    /// Whether a run's reply carries the command that runs it again.
    again: bool,
    /// The directory of the runs' jobs, on a controller. A job names its run's member, and is
    /// listed as `ls` lists it while its run has no record.
    jobs: Option<PathBuf>,
    /// The list row of each run that has ended, or is lost.
    ended: BTreeMap<String, Row>,
    /// Each run held as a fold of its log.
    held: BTreeMap<String, Held>,
    /// Each run's log epoch, kept after its `Held` is dropped.
    log_epochs: BTreeMap<String, u64>,
    /// How many runs have been read, in order.
    reads: u64,
    /// How many `Held`s have been made.
    made: u64,
    /// The traces of runs' callers, the last read last, as [`trace::TRACE_PASSES`] bounds them.
    trace: Vec<trace::Trace>,
}

struct Row {
    record: Record,
    /// Whether the run ended as lost, which a record that later ends otherwise supersedes.
    lost: bool,
    json: JsonRow,
}

/// A complete JSON value shared by the cache and the list page being assembled.
#[derive(Clone)]
struct JsonRow(Rc<str>);

impl From<Value> for JsonRow {
    fn from(row: Value) -> Self {
        Self(row.to_string().into())
    }
}

/// A run read so far: its record and the fold of its log, with what the replies are made from.
struct Held {
    record: Record,
    fold: LogFold,
    /// What the lines taken say of the run, read again only once a take gains a line or the
    /// run's ending changes, and shared by every answer until then.
    readout: Readout,
    talk: Option<Talk>,
    failures: Failures,
    task: Option<String>,
    /// The byte at which each line of the log starts.
    lines: Vec<u64>,
    /// The last [`RECENT_LINES`] lines, each with its byte.
    recent: VecDeque<(u64, Vec<u8>)>,
    ending: Option<Ending>,
    /// How many times the log has started over: a byte of an earlier epoch means nothing now.
    log_epoch: u64,
    /// What the view found of the log when it last started over, which the run's row says.
    started_over: Option<String>,
    /// The value of `View::reads` when it was last read.
    read: u64,
    exchanges: trace::Exchanges,
    ops: trace::Ops,
    /// The value of `View::made` when it was made, which a trace compares.
    made: u64,
}

impl Held {
    fn new(record: Record, log_epoch: u64, made: u64) -> Result<Self, ReadError> {
        let fold = record.fold();
        Ok(Self {
            readout: fold.readout()?,
            fold,
            record,
            talk: None,
            failures: Failures::default(),
            task: None,
            lines: Vec::new(),
            recent: VecDeque::new(),
            ending: None,
            log_epoch,
            started_over: None,
            read: 0,
            exchanges: trace::Exchanges::default(),
            ops: trace::Ops::default(),
            made,
        })
    }

    fn look(&mut self) -> Result<(), ReadError> {
        let (ended, log_epoch) = (self.ending.is_some(), self.log_epoch);
        // How it ended first, so that the log taken after holds every record before the end.
        let ending = self.record.ending()?;
        let changed = ending != self.ending;
        self.ending = ending;
        let taken = self.fold.taken();
        if !ended {
            self.take()?;
        }
        if changed || self.fold.taken() != taken || self.log_epoch != log_epoch {
            self.readout = self.fold.readout()?;
        }
        Ok(())
    }

    fn take(&mut self) -> Result<(), ReadError> {
        if self.talk.is_none() {
            // A run writes both before it creates its log, so a log without them has no line.
            let (Some(ir), Some(places)) = (self.record.ir()?, self.record.places()?) else {
                return Ok(());
            };
            let routes = self.record.routes()?;
            self.talk = Some(Talk::new(&ir, &places, routes.as_ref()));
            self.task = Some(ir.name.to_string());
        }
        let Self {
            fold,
            talk,
            failures,
            lines,
            recent,
            exchanges,
            ops,
            ..
        } = self;
        let started_over = fold.take_each(|line| {
            lines.push(line.byte);
            if recent.len() == RECENT_LINES {
                recent.pop_front();
            }
            recent.push_back((line.byte, line.bytes.to_vec()));
            if let Ok(record) = serde_json::from_slice(line.bytes) {
                if let Some(talk) = talk.as_mut() {
                    talk.take(&record);
                    failures.take(line.byte, &record, talk);
                }
                exchanges.take(line.byte, &record);
                ops.take(&record);
            }
        })?;
        if let Some(started_over) = started_over {
            let said = format!("run {}: {started_over}", self.record.id());
            // What was derived of the log before is not of this one.
            let record = Record::open(self.record.dir())?;
            let ending = self.ending.take();
            *self = Self::new(record, self.log_epoch + 1, self.made)?;
            self.ending = ending;
            self.started_over = Some(said);
            return self.take();
        }
        Ok(())
    }
}

fn token() -> Result<String, String> {
    let mut random = [0; 16];
    File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut random))
        .context("reading /dev/urandom for the view's token failed")?;
    Ok(format!("{:032x}", u128::from_be_bytes(random)))
}

/// An accepted connection that has sent nothing yet, as a browser opens one to use later, or
/// never.
struct Silent {
    stream: TcpStream,
    accepted: Instant,
}

fn serve(listener: &TcpListener, local: &mut Local) -> Result<Infallible, String> {
    let mut silent: Vec<Silent> = Vec::new();
    loop {
        // A connection waits here until it sends a byte, so that one that never does holds up no
        // other, and is dropped after `http::IDLE`.
        let (incoming, sent) = wait(listener, &silent)?;
        // All that had sent when the wait ended are answered, first accepted first, before the
        // next wait, so none waits on a connection accepted after it.
        let mut still = Vec::with_capacity(silent.len());
        for (connection, sent) in silent.into_iter().zip(sent) {
            if sent {
                answer_sent(connection.stream, local);
            } else if connection.accepted.elapsed() < http::IDLE {
                still.push(connection);
            }
        }
        silent = still;
        if incoming {
            match listener.accept() {
                Ok((stream, _)) => silent.push(Silent {
                    stream,
                    accepted: Instant::now(),
                }),
                Err(error) => say(format_args!("accepting a connection failed: {error}")),
            }
        }
    }
}

fn wait(listener: &TcpListener, silent: &[Silent]) -> Result<(bool, Vec<bool>), String> {
    let mut polled = vec![PollFd::new(listener, PollFlags::IN)];
    polled.extend(
        silent
            .iter()
            .map(|connection| PollFd::new(&connection.stream, PollFlags::IN)),
    );
    // `silent` is in the order accepted, so its first is the first to reach `IDLE`.
    let timeout = silent.first().map(|first| {
        let left = (first.accepted + http::IDLE).saturating_duration_since(Instant::now());
        let Ok(left) = Timespec::try_from(left) else {
            unreachable!("{left:?}, at most {:?}, fits a `timespec`", http::IDLE)
        };
        left
    });
    match event::poll(&mut polled, timeout.as_ref()) {
        // A signal ends the wait early, with nothing ready.
        Ok(_) | Err(Errno::INTR) => {}
        Err(error) => return Err(format!("waiting on the view's connections failed: {error}")),
    }
    let ready = |polled: &PollFd<'_>| !polled.revents().is_empty();
    Ok((ready(&polled[0]), polled[1..].iter().map(ready).collect()))
}

fn answer_sent(stream: TcpStream, local: &mut Local) {
    // One that ends, or fails, before its first byte asked nothing: a browser closes its spare
    // connections so.
    if !stream.peek(&mut [0]).is_ok_and(|bytes| bytes > 0) {
        return;
    }
    if let Err(error) = answer(stream, local) {
        say(format_args!("a connection to the view failed: {error}"));
    }
}

fn answer(stream: TcpStream, local: &mut Local) -> std::io::Result<()> {
    // A request sent in part holds back every other connection until `STALL` after its first
    // byte; a browser sends each request whole.
    let read = http::read(&stream, Instant::now() + http::STALL)?;
    // No deadline for making the answer: it is made of this machine's records, which
    // no client can slow, or relayed from the view on the controller, over an `ssh` whose
    // `ServerAliveInterval` ends a dead connection; and one cut off would be thrown away once its
    // cost was paid. The other connections wait while it is made, as the view answers one at a
    // time.
    let (response, deflate) = match read {
        Ok(request) => (route(&request, local), request.deflate),
        Err(refused) => (refused, false),
    };
    http::write(stream, response, deflate)
}

/// Why a reply is not a `200`.
enum Failed {
    NotFound(String),
    Unreadable(String),
    Refused(Response),
}

impl From<String> for Failed {
    fn from(error: String) -> Self {
        Self::Unreadable(error)
    }
}

impl From<ReadError> for Failed {
    fn from(error: ReadError) -> Self {
        Self::Unreadable(error.into())
    }
}

impl Failed {
    fn response(self) -> Response {
        match self {
            Self::NotFound(error) => Response::refused("404 Not Found", &error),
            Self::Unreadable(error) => Response::refused("500 Internal Server Error", &error),
            Self::Refused(refused) => refused,
        }
    }
}

fn route(request: &Request, local: &mut Local) -> Response {
    // The browser's own requests for the page's files can't carry the token header.
    if request.method == Method::Get
        && let Some(file) = local.page.file(&request.path)
    {
        return file;
    }
    if request.token.as_deref() != Some(local.token.as_str()) {
        let carries = if request.token.is_some() {
            "a wrong"
        } else {
            "no"
        };
        return Response::refused(
            "401 Unauthorized",
            &format!(
                "the request carries {carries} token in `{}`: give the one in the address that \
                 `pumpkin view` printed as it started",
                http::TOKEN_HEADER
            ),
        );
    }
    let asked = match asked(request) {
        Ok(asked) => asked,
        Err(refused) => return refused,
    };
    let at = match &local.source {
        Source::Here { view, .. } => view.at.clone(),
        Source::Controller(viewing) => next::At::Controller(viewing.name().clone()),
    };
    if let Err(refused) = allowed(&asked, request, local.port, &at) {
        return refused;
    }
    match (asked, &mut local.source) {
        (Asked::Cancel { id }, _) => local.cancel(&id),
        (Asked::Cancelling { id }, _) => local.cancels.said(&id),
        (Asked::Start, Source::Here { runs_given, .. }) => Response::refused(
            "404 Not Found",
            &format!(
                "this view serves the runs of a directory, `{runs_given}`, not a controller's: \
                 start a controller by a command that reaches it, which starts it: pumpkin ls"
            ),
        ),
        (Asked::Start, Source::Controller(viewing)) => match viewing.start() {
            Ok(()) => {
                let said = format!("controller `{}` is starting", viewing.name());
                Response::json(&json!({ "said": said })).with_status("202 Accepted")
            }
            Err(error) => Response::refused("502 Bad Gateway", &error),
        },
        (asked, Source::Here { view, .. }) => answered(asked, view),
        (_, Source::Controller(viewing)) => asked_there(viewing, request),
    }
}

/// The view on the controller's reply to `request`, a `GET` of the runs' data.
fn asked_there(viewing: &mut Viewing, request: &Request) -> Response {
    match viewing.ask(&request.target, request.deflate) {
        Ok(viewed) => {
            let Viewed {
                status,
                content_type,
                deflated,
                body,
            } = viewed;
            Response::relayed(status, content_type, body, deflated)
        }
        Err(Unviewed::Stopped) => {
            let stopped = format!(
                "controller `{}` is stopped: start it by `POST {START}`, as the page's \
                 \"Start it\" does, or run a command that reaches it, which starts it: {}",
                viewing.name(),
                next::of_runs("ls", &next::At::Controller(viewing.name().clone()))
            );
            let stopped = json!({ "error": stopped, "start": START });
            Response::json(&stopped).with_status("503 Service Unavailable")
        }
        Err(Unviewed::Down(down)) => Response::refused("503 Service Unavailable", &down),
        Err(Unviewed::Failed(error)) => Response::refused("502 Bad Gateway", &error),
    }
}

/// The answer to `request`, a `GET` of the runs' data, with none of `route`'s checks.
fn api(request: &Request, view: &mut View) -> Response {
    match asked(request) {
        Ok(asked) => answered(asked, view),
        Err(refused) => refused,
    }
}

fn answered(asked: Asked, view: &mut View) -> Response {
    let answered = match asked {
        Asked::Runs { before } => {
            let reply = view.runs_json(before.as_deref());
            view.drop_ended();
            return reply.unwrap_or_else(Failed::response);
        }
        Asked::Run { id } => view.run_json(&id),
        Asked::Since { id, log, log_epoch } => view.since_json(&id, log, log_epoch),
        Asked::Log { id, at, log_epoch } => view.log_json(&id, at, log_epoch),
        // Never asked of a view on a controller, as the view that asks answers these itself.
        Asked::Start => {
            return Response::refused(
                "404 Not Found",
                "the view on a controller starts nothing: start it from the page that `pumpkin \
                 view` serves",
            );
        }
        Asked::Cancel { id } | Asked::Cancelling { id } => {
            return Response::refused(
                "404 Not Found",
                &format!(
                    "the view on a controller answers no cancel: cancel run {} from the page \
                     that `pumpkin view` serves, or run: {}",
                    id.as_str(),
                    next::of_run("cancel", &view.at, id.as_str())
                ),
            );
        }
        Asked::Trace { id, asked } => match view.trace(&id, asked) {
            Ok(trace::Answer::Json(json)) => Ok(json),
            Ok(trace::Answer::Bytes(bytes)) => return bytes,
            Err(failed) => Err(failed),
        },
    };
    view.drop_ended();
    match answered {
        Ok(json) => Response::json(&json),
        Err(failed) => failed.response(),
    }
}

impl View {
    fn new(runs: PathBuf, at: next::At) -> Self {
        Self {
            runs,
            at,
            again: true,
            jobs: None,
            ended: BTreeMap::new(),
            held: BTreeMap::new(),
            log_epochs: BTreeMap::new(),
            reads: 0,
            made: 0,
            trace: Vec::new(),
        }
    }

    fn runs_json(&mut self, before: Option<&str>) -> Result<Response, Failed> {
        let mut names =
            if self.at.dir().is_none() && !self.runs.try_exists().context(self.runs.display())? {
                Vec::new()
            } else {
                Record::names(&self.runs)?
            };
        if let Some(jobs) = &self.jobs {
            names.extend(crate::serve::job_ids(jobs)?);
            names.sort_unstable();
            names.dedup();
        }
        let older = names.iter().rev();
        let mut page = older.filter(|name| before.is_none_or(|before| name.as_str() < before));
        let mut rows = Vec::new();
        let mut last = None;
        for name in page.by_ref() {
            last = Some(name.clone());
            let row = self
                .row(name)
                .or_else(|error| self.unread_row(name, error).map(Some))
                .unwrap_or_else(|error| Some(json!({ "id": name, "error": error }).into()));
            rows.extend(row);
            if rows.len() == LIST_PAGE_RUNS {
                break;
            }
        }
        let next = if page.next().is_some() { last } else { None };
        // Append completed JSON values as Git's json_writer does. A cached row is already encoded.
        // https://github.com/git/git/blob/5d826e972970a784bd7a7bdf587512510097b8c7/json-writer.c#L350-L355
        let mut body = format!(r#"{{"next":{},"runs":["#, json!(next));
        body.reserve(rows.iter().map(|row| row.0.len() + 1).sum::<usize>() + 2);
        for (at, row) in rows.iter().enumerate() {
            if at > 0 {
                body.push(',');
            }
            body.push_str(&row.0);
        }
        body.push_str("]}");
        Ok(Response::json_text(body))
    }

    /// The row of run `id`, whose record failed to read with `error`, as `ls` lists it when
    /// another build made the record; else `error`.
    fn unread_row(&self, id: &str, error: ReadError) -> Result<JsonRow, String> {
        let listed = Listed::unread(&self.runs.join(id), error)?;
        let row = progress::Row::of(id, &listed, crate::unix_now(), &self.at);
        Ok(listed_json(&self.membered(row)?, None).into())
    }

    /// The row of job `id`, whose run has no record, as `ls` lists it, or `None` if no job is `id`.
    fn job_row(&self, id: &str) -> Result<Option<JsonRow>, String> {
        let Some(jobs) = &self.jobs else {
            return Ok(None);
        };
        let job = jobs.join(id);
        if !job.try_exists().context(job.display())? {
            return Ok(None);
        }
        let listed = crate::serve::job_listed(id)?;
        let row = progress::Row::of(id, &listed, crate::unix_now(), &self.at);
        Ok(Some(listed_json(&self.membered(row)?, None).into()))
    }

    /// `row` with its run's member, if a job started the run.
    fn membered(&self, mut row: progress::Row) -> Result<progress::Row, String> {
        if let Some(jobs) = &self.jobs {
            row.member = crate::job::run_member(&jobs.join(&row.id))?;
        }
        Ok(row)
    }

    fn row(&mut self, id: &str) -> Result<Option<JsonRow>, ReadError> {
        if let Some(row) = self.ended.get(id) {
            if !row.lost || row.record.ending()? == Some(Ending::Lost) {
                return Ok(Some(row.json.clone()));
            }
            self.ended.remove(id);
        }
        let record = if self.held.contains_key(id) {
            None
        } else {
            let dir = self.runs.join(id);
            let has = dir.join("record.json").try_exists();
            let has = has.map_err(|error| ReadError::Failed(format!("{}: {error}", dir.display())));
            if !has? {
                return self.job_row(id).map_err(ReadError::Failed);
            }
            let record = Record::open(&dir)?;
            if let Some(ending) = record.ending()? {
                return self.ended_row(record, ending).map(Some);
            }
            Some(record)
        };
        let at = self.at.clone();
        let held = self.looked(id, record)?;
        match &held.ending {
            None => {
                let row = live_row(held, &at);
                let started_over = held.started_over.clone();
                let row = self.membered(row).map_err(ReadError::Failed)?;
                Ok(Some(listed_json(&row, started_over.as_deref()).into()))
            }
            Some(ending) => {
                let ending = ending.clone();
                let record = Record::open(held.record.dir())?;
                self.ended_row(record, ending).map(Some)
            }
        }
    }

    fn ended_row(&mut self, record: Record, ending: Ending) -> Result<JsonRow, ReadError> {
        let at = self.at.clone();
        let now = crate::unix_now();
        let lost = ending == Ending::Lost;
        let used = record.used()?;
        // A lost run, or a reap's record of one, says what it used only in its log, which a row
        // reads only then.
        let progress = match used {
            Some(_) => None,
            None => record.progress()?.progress,
        };
        let listed = Listed::of(&record, record.task()?, Some(ending), progress, used);
        let row = progress::Row::of(record.id(), &listed, now, &at);
        let id = record.id().to_owned();
        let row = self.membered(row).map_err(ReadError::Failed)?;
        let json = JsonRow::from(listed_json(&row, None));
        let row = Row {
            record,
            lost,
            json: json.clone(),
        };
        self.ended.insert(id, row);
        Ok(json)
    }

    fn held(&mut self, id: &RunTag) -> Result<&mut Held, Failed> {
        self.reads += 1;
        let id = id.as_str();
        let record = if self.held.contains_key(id) {
            None
        } else {
            Some(self.record(id)?)
        };
        let reads = self.reads;
        let held = self.looked(id, record)?;
        held.read = reads;
        Ok(held)
    }

    fn record(&self, id: &str) -> Result<Record, Failed> {
        let dir = self.runs.join(id);
        let path = dir.join("record.json");
        let has = match path.try_exists() {
            // A file is no run, as the list shows directories alone.
            Err(error) if error.kind() == io::ErrorKind::NotADirectory => Ok(false),
            has => has,
        };
        if !has.context(path.display())? {
            // Said without the runs directory, which is the server's, not the page's.
            return Err(Failed::NotFound(format!(
                "no run that this view serves matches `{id}`: name the id of one that its list \
                 shows"
            )));
        }
        Record::open(&dir).map_err(|error| match error {
            ReadError::Form(error) => Failed::NotFound(error),
            ReadError::Failed(error) => Failed::Unreadable(error),
        })
    }

    /// Run `id`, held, or else of `record`, looked at again. Its fold is dropped if the look
    /// fails, as what a take that failed part-way leaves can't be trusted.
    fn looked(&mut self, id: &str, record: Option<Record>) -> Result<&mut Held, ReadError> {
        let log_epoch = self.log_epochs.get(id).copied().unwrap_or(0);
        let made = self.made + 1;
        let mut held = match (self.held.remove(id), record) {
            (Some(held), _) => held,
            (None, Some(record)) => Held::new(record, log_epoch, made)?,
            (None, None) => Held::new(Record::open(&self.runs.join(id))?, log_epoch, made)?,
        };
        self.made = self.made.max(held.made);
        held.look()?;
        if held.log_epoch != log_epoch {
            self.log_epochs.insert(id.to_owned(), held.log_epoch);
        }
        Ok(self.held.entry(id.to_owned()).or_insert(held))
    }

    fn run_json(&mut self, id: &RunTag) -> Result<Value, Failed> {
        let (at, again) = (self.at.clone(), self.again);
        let held = self.held(id)?;
        Ok(run_json(held, &at, again))
    }

    /// What run `id`'s log gained past byte `log`, a byte that a reply of `log_epoch` gave: `end`
    /// and `live`, with the run if it is live or the log gained, and, if it gained, the records
    /// from `log` on in a window, and `next`, the byte to ask from next.
    fn since_json(
        &mut self,
        id: &RunTag,
        log: u64,
        log_epoch: Option<u64>,
    ) -> Result<Value, Failed> {
        let (at, again) = (self.at.clone(), self.again);
        let held = self.held(id)?;
        let end = held.fold.taken();
        let live = held.ending.is_none();
        let from = line_at(held, log, log_epoch)?;
        if log == end && !live {
            return Ok(json!({ "end": end, "live": live }));
        }
        let mut reply = if log == end {
            // A live run's clock goes on while its log is quiet, as on a long call.
            json!({ "end": end })
        } else {
            window_json(held, from)?
        };
        reply["live"] = json!(live);
        reply["run"] = run_json(held, &at, again);
        Ok(reply)
    }

    fn log_json(&mut self, id: &RunTag, at: u64, log_epoch: Option<u64>) -> Result<Value, Failed> {
        let held = self.held(id)?;
        let at = line_at(held, at, log_epoch)?;
        Ok(window_json(held, at)?)
    }

    /// Drops the folds of the runs that have ended, but the [`HELD_ENDED_RUNS`] read last.
    fn drop_ended(&mut self) {
        let mut ended: Vec<(u64, String)> = (self.held.iter())
            .filter(|(_, held)| held.ending.is_some())
            .map(|(id, held)| (held.read, id.clone()))
            .collect();
        ended.sort_unstable();
        let dropped = ended.len().saturating_sub(HELD_ENDED_RUNS);
        for (_, id) in &ended[..dropped] {
            self.held.remove(id);
        }
    }
}

impl Local {
    fn cancel(&mut self, id: &RunTag) -> Response {
        match &self.source {
            Source::Here { view, runs_given } => {
                if let Err(failed) = view.record(id.as_str()) {
                    return failed.response();
                }
                self.cancels.start(id, &next::At::Runs(runs_given.clone()))
            }
            Source::Controller(viewing) => {
                let at = next::At::Controller(viewing.name().clone());
                self.cancels.start(id, &at)
            }
        }
    }
}

/// The index of the line that starts at `byte`, refused if `log_epoch` is not the log's, or if
/// `byte` starts no line and is not the log's end.
fn line_at(held: &Held, byte: u64, log_epoch: Option<u64>) -> Result<usize, Failed> {
    if log_epoch.is_some_and(|log_epoch| log_epoch != held.log_epoch) {
        let error = format!(
            "run {}'s log started over since the reply that gave byte {byte}: read the run \
             again, and its log from byte 0",
            held.record.id()
        );
        let refused = json!({ "error": error, "log_epoch": held.log_epoch });
        return Err(Failed::Refused(
            Response::json(&refused).with_status("409 Conflict"),
        ));
    }
    let end = held.fold.taken();
    if byte == end {
        return Ok(held.lines.len());
    }
    held.lines.binary_search(&byte).map_err(|_| {
        Failed::Refused(Response::refused(
            "400 Bad Request",
            &format!(
                "byte {byte} starts no line of run {}'s log, which is read to byte {end}: \
                 expected 0, or a byte that an answer gave",
                held.record.id()
            ),
        ))
    })
}

fn window_json(held: &Held, from: usize) -> Result<Value, String> {
    let end = held.fold.taken();
    let starts = &held.lines[from..];
    let mut read = from..from;
    if let Some(&start) = starts.first() {
        let ends = starts[1..].iter().copied().chain([end]);
        let within = ends.take_while(|end| end - start <= LOG_WINDOW_BYTES);
        read.end += within.count().max(1);
    }
    let next = held.lines.get(read.end).copied().unwrap_or(end);
    let mut reply = json!({ "end": end, "next": next });
    // Move the parsed records into the reply: `json!` would serialize each into a second tree.
    // https://github.com/serde-rs/json/blob/44d9c53e2507636c0c2afee0c9c132095dddb7df/src/macros.rs#L277-L279
    reply["records"] = Value::Array(lines(held, read)?);
    Ok(reply)
}

fn lines(held: &Held, read: std::ops::Range<usize>) -> Result<Vec<Value>, String> {
    let past = held
        .lines
        .get(read.end)
        .copied()
        .unwrap_or(held.fold.taken());
    let starts = &held.lines[read];
    let Some(&start) = starts.first() else {
        return Ok(Vec::new());
    };
    // The lines held in memory answer it if they hold all of it; else the log is read.
    let held_from = held.recent.front().map(|(byte, _)| *byte);
    if held_from.is_some_and(|held_from| held_from <= start) {
        let recent = held.recent.iter().skip_while(|(byte, _)| *byte < start);
        let records = recent.take(starts.len());
        let records = records.map(|(byte, line)| record_json(*byte, line));
        return Ok(records.collect());
    }
    let lines = held.record.log_lines(start..past)?;
    let records = lines.iter().map(|(byte, line)| record_json(*byte, line));
    Ok(records.collect())
}

/// When `record`, a record of a run's log, happened, in seconds since the run started: when the
/// runtime observed it, if it says, else when the log wrote it.
fn when(record: &Value) -> Option<f64> {
    let nanos = |name: &str| record[name].as_str()?.parse::<u64>().ok();
    let nanos = nanos("observed_ns").or_else(|| nanos("at_ns"))?;
    Some(Duration::from_nanos(nanos).as_secs_f64())
}
