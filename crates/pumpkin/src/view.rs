//! `pumpkin view`: the runs in a runs directory, as JSON, served on this machine's loopback
//! interface ([`http`]) to the person who started it, whose requests carry the token in the
//! address that it prints, in [`http::TOKEN_HEADER`]:
//!
//! - `GET /api/runs?before=<id>`: a page of the runs, newest first, each as `ls` lists it
//!   ([`progress::columns`]), those before run `before` if given;
//! - `GET /api/runs/<id>`: the run, its phase or ending, lanes, who talks to whom ([`talk`]),
//!   model requests, and `end`, the byte of its log that it was read to;
//! - `GET /api/runs/<id>/since?log=<byte>`: only `end` and `live` if the log has no line past
//!   `byte`; else the run as above, and the log's records from `byte` on;
//! - `GET /api/runs/<id>/log?at=<byte>`: the log's records from `byte` on;
//! - `POST /api/runs/<id>/cancel`, from the view's page, and `GET …/cancelling` ([`cancel`]).
//!
//! What it reads, and when. An ended run, whose `outcome.json` is there, is listed from its
//! `record.json`, `ir.json`'s name and `outcome.json`, read once, as an ended record doesn't
//! change; a lost run, whose driver ended without one, from its log, read once, until a reap
//! writes it. A live run, whose driver holds its lock, is held as a fold of its log ([`LogFold`]),
//! which reads only what the log gained since it last read, and hands each new line to what the
//! view derives from it; so is a run whose page is read, of which the view holds the
//! [`HELD_ENDED_RUNS`] read last once they have ended. A poll's answer sends lines the fold holds;
//! only a window further back reads the log again, the lines it sends.
//!
//! It names a record only by its id, as a run's tag ([`RunTag`]), below the runs directory, and
//! reads only what the record holds there: no path a request gives reaches the disk. The JSON
//! form of what it answers with is a module's of its own ([`json`](mod@json)).

mod cancel;
mod frame;
mod http;
mod json;
mod page;
mod talk;
mod trace;

use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::fs::File;
use std::io::Read as _;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use provider::RunTag;
use runtime::{Ending, LogFold, Readout, Record};
use rustix::event::{self, PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use serde_json::{Value, json};
use stdx::Context;

use self::http::{Method, Request, Response};
use self::json::{listed_json, live_row, record_json, run_json, scored, tokens_json};
use self::talk::Talk;
use crate::progress::{self, Listed};
use crate::{cli, next, say};

/// The most runs that a page of the list holds.
const LIST_PAGE_RUNS: usize = 100;

/// The most bytes of a run's log whose records one answer holds ([`window`]).
// A page reads a log one window at a time, asking for each once the last has arrived, so each
// costs a round trip. A link of under 1 MB a second with round trips of a tenth of a second
// carries under 100 KB in one; at ten times that the round trips add a tenth or less to the read,
// and the JSON one answer builds stays a few MiB.
const LOG_WINDOW_BYTES: u64 = 1024 * 1024;

/// How many of a log's last lines a held run keeps, which a poll's answer sends without reading
/// the log again: a live run's log gains a few between two polls.
const RECENT_LINES: usize = 100;

/// How many runs that have ended the view holds the folds of, those read last: a run's page is
/// read again and again while it is open, and a person opens a few at a time.
const HELD_ENDED_RUNS: usize = 8;

/// Serves the runs in the directory `flags` name, until the process is stopped.
pub(crate) fn view(flags: cli::View) -> Result<ExitCode, String> {
    let cli::View { runs, port } = flags;
    let (runs, named) = crate::runs_dir(runs)?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port.unwrap_or(0)))
        .context("binding the view's port on 127.0.0.1 failed")?;
    let port = listener.local_addr().context("the view's port")?.port();
    let token = token()?;
    // The address alone on its line, as a next command is, so that a word-wise selection or a
    // double-click takes it whole, as Jupyter's server prints its own:
    // https://github.com/jupyter-server/jupyter_server/blob/0d1a13b2f5212a9f9da0122a9e7dbaf23f6cfcb7/jupyter_server/serverapp.py#L3202-L3206
    say(format_args!(
        "serving the runs in {runs} to this machine alone, until stopped, at\n    \
         http://127.0.0.1:{port}/?t={token}"
    ));
    let mut local = Local {
        view: View::new(PathBuf::from(&runs), named),
        runs_given: runs,
        token,
        port,
        cancels: cancel::Cancels::default(),
        page: page::Page::new(),
    };
    match serve(&listener, &mut local)? {}
}

/// Answers, on the standard output, each request on the standard input for the runs' data in the
/// default runs directory, in the frames of [`frame`], until the input ends or [`frame::IDLE`]
/// passes with none.
pub(crate) fn over_pipe() -> Result<ExitCode, String> {
    let view = View::new(PathBuf::from(crate::default_runs()?), None);
    // The command that runs a run again from here would name the controller's paths.
    let mut view = View {
        again: false,
        ..view
    };
    frame::answer(&mut view)?;
    Ok(ExitCode::SUCCESS)
}

/// What `pumpkin view` holds beside the runs' data ([`View`]).
struct Local {
    view: View,
    /// The runs directory as it was given, or found by default, which a cancel names.
    runs_given: String,
    /// What each request of the view's data carries ([`token`]).
    token: String,
    /// The port that the view is served at, which its page's `Origin` names.
    port: u16,
    cancels: cancel::Cancels,
    page: page::Page,
}

/// What the view holds of the runs' data between requests.
struct View {
    runs: PathBuf,
    /// The runs directory as commands name it, if not by default.
    named: Option<String>,
    /// Whether a run's reply holds the command that runs it again ([`run_json`]).
    again: bool,
    /// The list's row of each run that has ended, or is lost, by its id.
    ended: BTreeMap<String, Row>,
    /// Each run held as a fold of its log, by its id.
    held: BTreeMap<String, Held>,
    /// How many times the view found each run's log started over ([`Held::log_epoch`]), by its
    /// id, kept as a run is dropped and held again.
    log_epochs: BTreeMap<String, u64>,
    /// How many requests have read a held run, which orders them by the last read.
    reads: u64,
    /// How many folds of a run the view has made ([`Held::made`]).
    made: u64,
    /// The traces of runs' callers, the last read last, as [`trace::TRACE_PASSES`] bounds them
    /// ([`trace::Trace`]).
    trace: Vec<trace::Trace>,
}

/// The list's row of a run that has ended, or is lost, kept as the module's doc says.
struct Row {
    record: Record,
    lost: bool,
    json: Value,
}

/// A run held as a fold of its log, with what the view derives from each of its lines.
struct Held {
    record: Record,
    fold: LogFold,
    /// What the lines taken say of the run, read again only once a take gains a line or the
    /// run's ending changes, and shared by every answer until then.
    readout: Readout,
    /// Who talks to whom, once the record has its `ir.json` and `provider.json`.
    talk: Option<Talk>,
    /// The run's task, as its `ir.json` names it, once it has one.
    task: Option<String>,
    /// The byte at which each line taken starts, in the log's order.
    lines: Vec<u64>,
    /// The last [`RECENT_LINES`] lines taken, by the byte each starts at, without their newlines.
    recent: VecDeque<(u64, Vec<u8>)>,
    /// How the run ended, as of the last look; `None` while it is live.
    ending: Option<Ending>,
    /// How many times the view found the run's log started over, shorter or another file: each
    /// byte that an answer gave before is of another log.
    log_epoch: u64,
    /// The request that read it last ([`View::reads`]).
    read: u64,
    /// The model exchanges its log names.
    exchanges: trace::Exchanges,
    /// The ops its log names.
    ops: trace::Ops,
    /// Which of the view's folds of a run it is ([`View::made`]).
    made: u64,
}

impl Held {
    /// The run of `record`, of whose log the view found `log_epoch` starts over before, with
    /// nothing of its log taken, as the view's fold `made`.
    fn new(record: Record, log_epoch: u64, made: u64) -> Result<Self, String> {
        let fold = record.fold();
        Ok(Self {
            readout: fold.readout()?,
            fold,
            record,
            talk: None,
            task: None,
            lines: Vec::new(),
            recent: VecDeque::new(),
            ending: None,
            log_epoch,
            read: 0,
            exchanges: trace::Exchanges::default(),
            ops: trace::Ops::default(),
            made,
        })
    }

    /// Looks at the record again: how the run ended, if it has, then what its log gained, unless
    /// the last look knew its ending already, as an ended run's log doesn't grow.
    fn look(&mut self) -> Result<(), String> {
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

    /// Takes what the log gained, and all of it again once it started over
    /// ([`runtime::StartedOver`]).
    fn take(&mut self) -> Result<(), String> {
        if self.talk.is_none() {
            // A run writes both before it creates its log, so a log without them has no line.
            let (Some(ir), Some(places)) = (self.record.ir()?, self.record.places()?) else {
                return Ok(());
            };
            let routes = self.record.routes()?;
            self.talk = Some(Talk::new(&ir, &places, routes.as_ref()));
            self.task = ir["name"].as_str().map(str::to_owned);
        }
        let Self {
            fold,
            talk,
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
                }
                exchanges.take(line.byte, &record);
                ops.take(&record);
            }
        })?;
        if let Some(started_over) = started_over {
            say(format_args!("run {}: {started_over}", self.record.id()));
            // What was derived of the log before is not of this one.
            let record = Record::open(self.record.dir())?;
            let ending = self.ending.take();
            *self = Self::new(record, self.log_epoch + 1, self.made)?;
            self.ending = ending;
            return self.take();
        }
        Ok(())
    }
}

/// A token of 128 random bits, in hexadecimal.
fn token() -> Result<String, String> {
    let mut random = [0; 16];
    File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut random))
        .context("reading /dev/urandom for the view's token failed")?;
    Ok(format!("{:032x}", u128::from_be_bytes(random)))
}

/// A connection that has sent nothing yet, as a browser opens one to use later, or never.
struct Silent {
    stream: TcpStream,
    accepted: Instant,
}

/// Answers the connections to `listener` from `local`, one at a time, first accepted first. A
/// connection that has sent nothing holds back no other, and is closed once silent for
/// [`http::IDLE`].
fn serve(listener: &TcpListener, local: &mut Local) -> Result<Infallible, String> {
    // In the order they were accepted.
    let mut silent: Vec<Silent> = Vec::new();
    loop {
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

/// Waits until `listener` has a connection, one of `silent` has sent something or ended, or
/// [`http::IDLE`] has passed since the first of `silent` was accepted. Gives whether `listener`
/// has a connection, and whether each of `silent` has sent something or ended.
fn wait(listener: &TcpListener, silent: &[Silent]) -> Result<(bool, Vec<bool>), String> {
    let mut polled = vec![PollFd::new(listener, PollFlags::IN)];
    polled.extend(
        silent
            .iter()
            .map(|connection| PollFd::new(&connection.stream, PollFlags::IN)),
    );
    // As `silent` is in the order accepted, its first is the first to reach `IDLE`.
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

/// Answers `stream`, which has sent something or ended, and says why if that fails.
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

/// Reads the request on `stream`, which has sent its first byte, and answers it.
fn answer(stream: TcpStream, local: &mut Local) -> std::io::Result<()> {
    // A request sent in part holds back every other connection until `STALL` after its first
    // byte; a browser sends each request whole.
    let read = http::read(&stream, Instant::now() + http::STALL)?;
    // Making the answer has no deadline: it is made of this machine's records, which no client
    // can slow, and one cut off while it was made would waste its cost.
    let (response, deflate) = match read {
        Ok(request) => (route(&request, local), request.deflate),
        Err(refused) => (refused, false),
    };
    http::write(stream, response, deflate)
}

/// What a request asks for, as the module's doc says.
enum Asked {
    Runs {
        before: Option<String>,
    },
    Run {
        id: RunTag,
    },
    Since {
        id: RunTag,
        log: u64,
        log_epoch: Option<u64>,
    },
    Log {
        id: RunTag,
        at: u64,
        log_epoch: Option<u64>,
    },
    Cancel {
        id: RunTag,
    },
    Cancelling {
        id: RunTag,
    },
    Trace {
        id: RunTag,
        asked: trace::Asked,
    },
}

/// A failure to answer: no such run, or one that can't be read.
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

/// The answer to `request`, of `local`: a page's file, or, for a request that carries the token
/// and passes [`allowed`], a cancel or the runs' data.
fn route(request: &Request, local: &mut Local) -> Response {
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
    if let Err(refused) = allowed(&asked, request, local.port, local.view.named.as_deref()) {
        return refused;
    }
    match asked {
        Asked::Cancel { id } => local.cancel(&id),
        Asked::Cancelling { id } => local.cancels.said(&id),
        asked => answered(asked, &mut local.view),
    }
}

/// The answer to `request`, a `GET` of the runs' data, of `view`, with none of the checks of
/// [`route()`].
fn api(request: &Request, view: &mut View) -> Response {
    match asked(request) {
        Ok(asked) => answered(asked, view),
        Err(refused) => refused,
    }
}

/// The answer to `asked`, of the runs' data, of `view`.
fn answered(asked: Asked, view: &mut View) -> Response {
    let answered = match asked {
        Asked::Runs { before } => view.runs_json(before.as_deref()),
        Asked::Run { id } => view.run_json(&id),
        Asked::Since { id, log, log_epoch } => view.since_json(&id, log, log_epoch),
        Asked::Log { id, at, log_epoch } => view.log_json(&id, at, log_epoch),
        Asked::Cancel { id } | Asked::Cancelling { id } => {
            return Response::refused(
                "404 Not Found",
                &format!(
                    "the view on a controller answers no cancel: cancel run {} from the page \
                     that `pumpkin view` serves, or run: {}",
                    id.as_str(),
                    next::of_run("cancel", None, id.as_str())
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
        Err(Failed::NotFound(error)) => Response::refused("404 Not Found", &error),
        Err(Failed::Unreadable(error)) => Response::refused("500 Internal Server Error", &error),
        Err(Failed::Refused(refused)) => refused,
    }
}

/// What `request` asks for, or its refusal: not found, for a path that names nothing, or a bad
/// request, for a run's id or a number that is none.
fn asked(request: &Request) -> Result<Asked, Response> {
    let path = request.path.as_str();
    let not_found = || {
        let message = format!(
            "nothing is at `{path}`: expected the page at `/`, or the runs at `/api/runs` and \
             below it"
        );
        Response::refused("404 Not Found", &message)
    };
    let bad = |message: String| Response::refused("400 Bad Request", &message);
    let Some(api) = path.strip_prefix("/api/runs") else {
        return Err(not_found());
    };
    let parts: Vec<&str> = api.split('/').collect();
    let id = || match parts.get(1) {
        Some(id) => RunTag::try_from((*id).to_owned()).map_err(bad),
        None => Err(not_found()),
    };
    let byte = |name: &str| {
        request.number(name)?.ok_or_else(|| {
            bad(format!(
                "give the byte of the log: `?{name}=<byte>`, 0 for its start"
            ))
        })
    };
    match parts[..] {
        [""] => Ok(Asked::Runs {
            before: request.query("before").map(str::to_owned),
        }),
        ["", _] => Ok(Asked::Run { id: id()? }),
        ["", _, "since"] => Ok(Asked::Since {
            id: id()?,
            log: byte("log")?,
            log_epoch: request.number("log_epoch")?,
        }),
        ["", _, "log"] => Ok(Asked::Log {
            id: id()?,
            at: byte("at")?,
            log_epoch: request.number("log_epoch")?,
        }),
        ["", _, "cancel"] => Ok(Asked::Cancel { id: id()? }),
        ["", _, "cancelling"] => Ok(Asked::Cancelling { id: id()? }),
        ["", _, "trace" | "exchange" | "item" | "diff", ..] => Ok(Asked::Trace {
            id: id()?,
            asked: trace::asked(&parts[2..], request)?,
        }),
        _ => Err(not_found()),
    }
}

/// Refuses `request` unless its method is the one that what it asks for, `asked`, takes: `POST`
/// for a cancel, from the view's own page, its `Origin`, as served at `port`, else `GET`; its runs
/// directory is `named`, as commands name it, if not by default.
fn allowed(
    asked: &Asked,
    request: &Request,
    port: u16,
    named: Option<&str>,
) -> Result<(), Response> {
    let refused = |status, message: String| Err(Response::refused(status, &message));
    match (asked, request.method) {
        (Asked::Cancel { id }, Method::Post) => {
            let page = http::HOSTS.map(|host| format!("http://{host}:{port}"));
            let from = match &request.origin {
                Some(origin) if page.contains(origin) => return Ok(()),
                Some(origin) => format!("comes from `{origin}`, a page of another site"),
                None => "carries no `Origin`, which a browser sends".to_owned(),
            };
            refused(
                "403 Forbidden",
                format!(
                    "the cancel of run {} {from}: cancel it from the page that `pumpkin view` \
                     serves, or run: {}",
                    id.as_str(),
                    next::of_run("cancel", named, id.as_str())
                ),
            )
        }
        (Asked::Cancel { id }, Method::Get) => refused(
            "405 Method Not Allowed",
            format!(
                "run {} is cancelled by `POST`, which the page's cancel sends: cancel it there, \
                 or run: {}",
                id.as_str(),
                next::of_run("cancel", named, id.as_str())
            ),
        ),
        (_, Method::Post) => refused(
            "405 Method Not Allowed",
            format!(
                "`{}` is read by `GET`: only a run's cancel, `/api/runs/<id>/cancel`, takes \
                 `POST`",
                request.path
            ),
        ),
        (_, Method::Get) => Ok(()),
    }
}

impl View {
    /// The view of the runs in `runs`, which commands name as `named`, if not by default, holding
    /// nothing yet.
    fn new(runs: PathBuf, named: Option<String>) -> Self {
        Self {
            runs,
            named,
            again: true,
            ended: BTreeMap::new(),
            held: BTreeMap::new(),
            log_epochs: BTreeMap::new(),
            reads: 0,
            made: 0,
            trace: Vec::new(),
        }
    }

    /// A page of the runs, newest first, before run `before` if given, as JSON:
    /// `{"runs": [...], "next": <id or null>}`, where `next` is the `before` of the next page, if
    /// there is one.
    fn runs_json(&mut self, before: Option<&str>) -> Result<Value, Failed> {
        let names =
            if self.named.is_none() && !self.runs.try_exists().context(self.runs.display())? {
                Vec::new()
            } else {
                Record::names(&self.runs)?
            };
        let older = names.iter().rev();
        let mut page = older.filter(|name| before.is_none_or(|before| name.as_str() < before));
        let mut rows = Vec::new();
        let mut last = None;
        for name in page.by_ref() {
            last = Some(name.clone());
            let row = self
                .row(name)
                .unwrap_or_else(|error| Some(json!({ "id": name, "error": error })));
            rows.extend(row);
            if rows.len() == LIST_PAGE_RUNS {
                break;
            }
        }
        let next = if page.next().is_some() { last } else { None };
        Ok(json!({ "runs": rows, "next": next }))
    }

    /// The list's row of run `id`, as JSON; `None` if its directory holds no record.
    fn row(&mut self, id: &str) -> Result<Option<Value>, String> {
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
            if !has.context(dir.display())? {
                return Ok(None);
            }
            let record = Record::open(&dir)?;
            if let Some(ending) = record.ending()? {
                return self.ended_row(record, ending).map(Some);
            }
            Some(record)
        };
        let named = self.named.clone();
        let held = self.looked(id, record)?;
        match &held.ending {
            None => Ok(Some(live_row(held, named.as_deref()))),
            Some(ending) => {
                let ending = ending.clone();
                let record = Record::open(held.record.dir())?;
                self.ended_row(record, ending).map(Some)
            }
        }
    }

    /// The list's row of the run of `record`, which ended as `ending` says, or is lost, as JSON,
    /// kept: read from its `outcome.json`, which says what it used, or else from its log.
    fn ended_row(&mut self, record: Record, ending: Ending) -> Result<Value, String> {
        let named = self.named.as_deref();
        let now = crate::unix_now();
        let lost = ending == Ending::Lost;
        let scored = scored(Some(&ending));
        let json = if let Some(used) = record.used()? {
            let listed = Listed::of(&record, record.task()?, Some(ending), None);
            let mut row = listed_json(record.id(), &listed, scored, now, named);
            row["time"] = json!(progress::column_span(used.ran));
            row["usd"] = json!(
                used.usd
                    .map_or("-".to_owned(), |usd| { progress::usd(usd, used.unpriced) })
            );
            row["requests"] = json!(used.model_requests);
            row["tokens"] = tokens_json(&used.tokens, used.model_requests);
            row
        } else {
            // A lost run, or a reap's record of one, says what it used only in its log.
            let progress = record.progress()?.progress;
            let model = progress.as_ref().map(|progress| progress.model.clone());
            let listed = Listed::of(&record, record.task()?, Some(ending), progress);
            let mut row = listed_json(record.id(), &listed, scored, now, named);
            row["requests"] = json!(model.as_ref().map(|model| model.requests));
            row["tokens"] = json!(
                model
                    .as_ref()
                    .map(|model| tokens_json(&model.tokens, model.requests))
            );
            row
        };
        let id = record.id().to_owned();
        let row = Row {
            record,
            lost,
            json: json.clone(),
        };
        self.ended.insert(id, row);
        Ok(json)
    }

    /// Run `id`, held, and looked at again, as read by this request.
    fn held(&mut self, id: &RunTag) -> Result<&mut Held, Failed> {
        self.reads += 1;
        let id = id.as_str();
        let record = if self.held.contains_key(id) {
            None
        } else {
            Some(Record::open(&self.runs.join(id)).map_err(Failed::NotFound)?)
        };
        let reads = self.reads;
        let held = self.looked(id, record)?;
        held.read = reads;
        Ok(held)
    }

    /// Run `id`, held, or else of `record`, looked at again ([`Held::look`]); dropped if the
    /// look fails, as what a take that failed part-way leaves can't be trusted.
    fn looked(&mut self, id: &str, record: Option<Record>) -> Result<&mut Held, String> {
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

    /// Run `id`, as JSON, as the module's doc says.
    fn run_json(&mut self, id: &RunTag) -> Result<Value, Failed> {
        let (named, again) = (self.named.clone(), self.again);
        let held = self.held(id)?;
        Ok(run_json(held, named.as_deref(), again))
    }

    /// What run `id`'s log gained past byte `log`, a byte that a reply of `log_epoch` gave
    /// ([`line_at`]), as JSON: `{"end": e, "live": l}` if nothing; else with `"run"`, the run,
    /// and `"records"`, the records from `log` on, a window as [`window`] bounds it, and `"next"`,
    /// the byte past the last of them, to ask from next.
    fn since_json(
        &mut self,
        id: &RunTag,
        log: u64,
        log_epoch: Option<u64>,
    ) -> Result<Value, Failed> {
        let (named, again) = (self.named.clone(), self.again);
        let held = self.held(id)?;
        let end = held.fold.taken();
        let live = held.ending.is_none();
        let from = line_at(held, log, log_epoch)?;
        if log == end {
            return Ok(json!({ "end": end, "live": live }));
        }
        let Window { records, past } = window(held, from)?;
        Ok(json!({
            "end": end,
            "live": live,
            "run": run_json(held, named.as_deref(), again),
            "records": records,
            "next": past,
        }))
    }

    /// The records of run `id`'s log from byte `at` on, a byte that a reply of `log_epoch` gave
    /// ([`line_at`]), a window as [`window`] bounds it, as JSON: `{"records": [...], "next": n,
    /// "end": e}`, where `n` is the byte past the last.
    fn log_json(&mut self, id: &RunTag, at: u64, log_epoch: Option<u64>) -> Result<Value, Failed> {
        let held = self.held(id)?;
        let at = line_at(held, at, log_epoch)?;
        let Window { records, past } = window(held, at)?;
        Ok(json!({
            "records": records,
            "next": past,
            "end": held.fold.taken(),
        }))
    }

    /// Drops the folds of the runs that have ended, or are lost, but the [`HELD_ENDED_RUNS`] read
    /// last.
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
    /// Starts `pumpkin cancel` of run `id` ([`cancel::Cancels::start`]); not found if the runs
    /// directory holds no record of it.
    fn cancel(&mut self, id: &RunTag) -> Response {
        if let Err(error) = Record::open(&self.view.runs.join(id.as_str())) {
            return Response::refused("404 Not Found", &error);
        }
        self.cancels.start(id, &self.runs_given)
    }
}

/// The index, among the lines that `held` took, of the one that starts at byte `byte`, or the
/// count of them if `byte` is where its log was taken to; refused if it is neither, or, with
/// `409 Conflict` and the log's `"log_epoch"`, if a reply gave `byte` of `log_epoch`, another
/// than the log's: its log started over since, and the byte is of the log before.
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

/// Records of a run's log, as JSON, of the lines up to byte `past`.
struct Window {
    records: Vec<Value>,
    past: u64,
}

/// The records of `held`'s log from its line `from` on, those of its lines that end within
/// [`LOG_WINDOW_BYTES`] of the start of line `from`, and that line at least.
fn window(held: &Held, from: usize) -> Result<Window, String> {
    let starts = &held.lines[from..];
    let mut read = from..from;
    if let Some(&start) = starts.first() {
        let ends = starts[1..].iter().copied().chain([held.fold.taken()]);
        let within = ends.take_while(|end| end - start <= LOG_WINDOW_BYTES);
        read.end += within.count().max(1);
    }
    Ok(Window {
        past: held
            .lines
            .get(read.end)
            .copied()
            .unwrap_or(held.fold.taken()),
        records: lines(held, read)?,
    })
}

/// The records of `held`'s log of its lines `read`, as JSON: from those it holds, if it holds
/// them all, else read from the log now.
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
/// runtime observed it, if it says, else when the log wrote it, as the log's doc says.
fn when(record: &Value) -> Option<f64> {
    let nanos = |name: &str| record[name].as_str()?.parse::<u64>().ok();
    let nanos = nanos("observed_ns").or_else(|| nanos("at_ns"))?;
    Some(Duration::from_nanos(nanos).as_secs_f64())
}
