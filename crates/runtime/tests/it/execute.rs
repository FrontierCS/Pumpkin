//! Executing a prepared run through the library's entry point, [`Run::execute`], on the
//! simulated provider ([`Sim`]), and on the machine's host ([`System`]) or a simulated one
//! ([`SimHost`]): what it returns, its record, its run log, and what it leaves behind. The
//! agent's model route, where it has one, reaches a stand-in model provider ([`StandIn`]).

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]

use std::collections::BTreeMap;
use std::io::{self, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{fs, iter};

use decision::lasts;
use expect_test::{Expect, expect};
use provider::{
    Console, Create, Creator, ImageRef, Listed, Meeting, Provider, Ready, Reservation, Role,
    RunTag, SandboxId, SandboxdRef, Sweep,
};
use route::Serving;
use runtime::{Ending, Host, Record, Recorded, Run, System, new_id};
use schema::ir::{self, Image, ModelName};
use schema::workflow::Placed;
use serde_json::{Value, json};
use stdx::Deadline;
use test_support::http::{read_request, write_chunk};
use test_support::{WORKFLOW, files, sandboxes, standard_outputs};

use crate::host::{LateTake, SimHost, SlowSync};
use crate::run::{PROVIDER, Runs, compact_outcome, placed, placed_on, providers_of, serving};
use crate::sim::{
    self, BRIEF, HUNG, Journal, LAST, LATER, LINGERING, LOST, MUTE, REFUSED_ONCE, SLOW, Sim,
    UNDELETABLE, UNLISTABLE, UNPUBLISHED, UNSHOWN, VANISHING,
};

/// A `sandboxd` that dials.
const SANDBOXD: &str = "/sim/sandboxd";

/// The startup of a simulated provider whose tests wait for a start, or a teardown, to run out:
/// short, so that the wait takes seconds, but still far longer than a simulated sandbox takes
/// to start, so that a busy machine makes a start that succeeds wait longer, never fail.
const SHORT_STARTUP: u64 = 1;

/// The seconds that what an interrupted run waits on lasts, a start's startup or a work phase's
/// deadline: far past the signal's latency, yet short, so that a run that misses the signal
/// fails its case within a minute or so, rather than holding `ci` up.
const WAITED: u64 = 60;

/// Where an [`Interrupting`] provider notes SIGTERM: as it publishes `sandboxd`, the first publish;
/// as it publishes an image; once it has created the sandbox of a name; as it first deletes a
/// sandbox; or, from a thread of the test's own, once the run log records that the agent started
/// ([`interrupt_once_started`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum At {
    PublishSandboxd,
    PublishImage,
    Created(&'static str),
    Delete,
    AgentStarted,
}

/// The simulated provider `sim`, which [interrupts](interrupt) the test's process at the step
/// `at` names.
struct Interrupting<'a> {
    sim: &'a Sim,
    at: Option<At>,
}

impl Interrupting<'_> {
    /// Interrupts the process if the run has come to `step`.
    fn at(&self, step: At) {
        if self.at == Some(step) {
            interrupt();
        }
    }
}

/// Sends the test's process SIGTERM, which it notes ([`stdx::signal::catch`]), so that its run
/// stops, and returns once it is noted.
fn interrupt() {
    use rustix::process::{Signal, getpid, kill_process};
    stdx::signal::catch(&[Signal::TERM]).unwrap();
    kill_process(getpid(), Signal::TERM).unwrap();
    // Any thread may take it, after the kill returns.
    while stdx::signal::caught().is_none() {
        thread::yield_now();
    }
}

impl Sweep for Interrupting<'_> {
    fn delete(&self, run: &RunTag, ids: &[SandboxId]) -> Result<(), String> {
        self.at(At::Delete);
        self.sim.delete(run, ids)
    }

    fn list(&self, run: &RunTag) -> Result<Vec<Listed>, String> {
        self.sim.list(run)
    }
}

impl Provider for Interrupting<'_> {
    fn ready(&self) -> &Ready {
        self.sim.ready()
    }

    fn reserve(&self, run: &RunTag, phases: &[Vec<Role>]) -> Result<Reservation, String> {
        self.sim.reserve(run, phases)
    }

    fn publish_image(&self, image: &Image) -> Result<ImageRef, String> {
        self.at(At::PublishImage);
        self.sim.publish_image(image)
    }

    fn publish_sandboxd(&self, path: &Path) -> Result<SandboxdRef, String> {
        self.at(At::PublishSandboxd);
        self.sim.publish_sandboxd(path)
    }

    fn meeting(&self) -> Meeting<'_> {
        match self.sim.meeting() {
            Meeting::Local(_) => Meeting::Local(self),
        }
    }

    fn console(&self, run: &RunTag, id: &SandboxId) -> Result<Console, String> {
        self.sim.console(run, id)
    }
}

impl<D, C> Creator<D, C> for Interrupting<'_>
where
    Sim: Creator<D, C>,
{
    fn create(&self, create: &Create, dial: D) -> Result<C, String> {
        let created = Creator::<D, C>::create(self.sim, create, dial);
        if let Some(At::Created(name)) = self.at
            && create.name.as_str() == name
        {
            interrupt();
        }
        created
    }
}

/// The one of `providers` that `name` names.
fn named<'s, P>(providers: &'s [(&'static str, P)], name: &ir::Name) -> &'s P {
    let found = (providers.iter()).find(|(named, _)| name.as_str() == *named);
    &found.unwrap().1
}

/// Each of `providers`, by its name, [`Interrupting`] `at` a step.
fn interrupting<'a>(
    providers: &[(&'static str, &'a Sim)],
    at: Option<At>,
) -> Vec<(&'static str, Interrupting<'a>)> {
    (providers.iter())
        .map(|&(name, sim)| (name, Interrupting { sim, at }))
        .collect()
}

/// Executes `run` with `sandboxd`, interrupted `at` a step, if given, by the [`Interrupting`]
/// providers it was prepared on: what `execute` returns, and for an interrupted run, whether it
/// started, whether it returned within [`WAITED`], and what it published.
fn interrupted_at(
    run: Run<'_, impl Host>,
    providers: &[(&'static str, &Sim)],
    sandboxd: &str,
    at: Option<At>,
) -> Vec<String> {
    let record = run.dir().to_owned();
    let waited = Deadline::after(Duration::from_secs(WAITED));
    let mut ran = false;
    let executed = thread::scope(|scope| {
        if at == Some(At::AgentStarted) {
            scope.spawn(|| interrupt_once_started(&record));
        }
        run.execute(Path::new(sandboxd), || {
            ran = true;
            Ok(())
        })
    });
    let mut lines = vec![match executed {
        Ok(outcome) => format!("finished, with score {:?}", outcome.score()),
        Err(stopped) => stopped.to_string(),
    }];
    if at.is_some() {
        lines.push(format!("it started: {ran}"));
        let within = waited.remaining().is_some();
        lines.push(format!("it returned within {WAITED} s: {within}"));
        let published = providers.iter().flat_map(|(_, sim)| sim.published());
        lines.push(format!("it published: {:?}", published.collect::<Vec<_>>()));
    }
    lines
}

/// [Interrupts](interrupt) the process once the run log in `record` records that the agent
/// started, which it looks for every 10 ms, for a minute at most.
fn interrupt_once_started(record: &Path) {
    for _ in 0..6000 {
        let log = fs::read_to_string(record.join("log.jsonl")).unwrap_or_default();
        if log.contains(r#""kind":"agent_started""#) {
            interrupt();
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("the agent didn't start in a minute");
}

/// What `sim`'s platform shows of a run that the provider's calls don't: the mode of the
/// directory that held each create's socket directory, whether each create asked for the
/// model endpoint, if one did, and its journal, if it has one.
fn shown(sim: &Sim) -> Vec<String> {
    let modes = sim.modes().into_iter().map(|mode| format!("{mode:o}"));
    let modes: Vec<String> = modes.collect();
    let mut lines = vec![format!("their directory's mode at each create: {modes:?}")];
    let endpoints = sim.models();
    if endpoints.contains(&true) {
        lines.push(format!("the model endpoint at each create: {endpoints:?}"));
    }
    if let Some(journal) = sim.journal() {
        lines.push("the journal:".to_owned());
        lines.extend(
            journal
                .entries()
                .into_iter()
                .map(|entry| format!("  {entry}")),
        );
    }
    lines
}

/// Prepares `ir` as test `name`, its agent's model route served by a [`StandIn`] if it has one, on
/// `host`, executes it on `providers`, where its roles are placed ([`placed_on`]), with
/// `sandboxd`, and checks what becomes of it ([`executed`]): what `execute` returns, the record,
/// its log ([`logged`]), when its last record came ([`when`]), what the sandboxes printed, what
/// the platform shows ([`shown`]) and what the run's list finds once it has ended. It shows the
/// socket directories, the stand-in's address, the record, as made and once made, and the runs
/// directory as `<sockets>`, `<stand-in>`, `<partial>`, `<record>` and `<runs>`.
fn check(
    host: &impl Shown,
    name: &str,
    providers: &[(&'static str, &Sim)],
    ir: &Value,
    sandboxd: &str,
    expect: &Expect,
) {
    expect.assert_eq(&executed(host, name, providers, ir, sandboxd, None));
}

/// What [`check`] checks of `ir`'s run, as test `name`, on `host`, with `sandboxd`, on
/// `providers`; for a run that a SIGTERM
/// interrupts `at` a step, each provider is [`Interrupting`], and it shows too whether the run
/// started, and whether it returned within [`WAITED`], and what it published.
fn executed<H: Shown>(
    host: &H,
    name: &str,
    providers: &[(&'static str, &Sim)],
    ir: &Value,
    sandboxd: &str,
    at: Option<At>,
) -> String {
    let runs = Runs::new(name);
    let ir: ir::Workflow = serde_json::from_value(ir.clone()).unwrap();
    let route = ir.roles.iter().find_map(|role| role.egress.model.as_ref());
    let stand_in = route.map(|_| StandIn::start());
    let serving =
        (route.zip(stand_in.as_ref())).map(|(route, stand_in)| stand_in.serving(&runs.dir, route));
    let ready = (providers.iter()).map(|(name, provider)| (*name, provider.ready()));
    let ready: BTreeMap<&str, &Ready> = ready.collect();
    let placed = placed_on(&ir, &ready).unwrap();
    let hard_end = lasts(&placed);
    let id = new_id(SystemTime::now()).unwrap();
    let interrupting = interrupting(providers, at);
    let on = providers_of(&placed, |name| named(&interrupting, name));
    let recorded = Recorded { args: &[] };
    let run = Run::prepare(host, placed.clone(), on, &runs.dir, id, serving, recorded);
    let run = run.unwrap();
    let (record, tag) = (run.dir().to_owned(), RunTag::try_from(run.id().to_owned()));
    let sockets = record.join("sockets");
    let partial = record.with_file_name(format!(".{}.partial", run.id()));
    let before = unix_ns();
    let mut lines = interrupted_at(run, providers, sandboxd, at);
    let started = before..=unix_ns();
    lines.push(format!("record: {}", files(&record).join(", ")));
    lines.extend(sandboxes(&record));
    let payloads = fs::read_dir(record.join("payloads")).into_iter().flatten();
    let mut payloads: Vec<String> = payloads
        .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect();
    payloads.sort();
    lines.push(format!("payloads hold: {payloads:?}"));
    lines.extend(declared(&record, providers));
    let outcome = fs::read_to_string(record.join("outcome.json")).unwrap();
    lines.push(format!("outcome.json: {}", compact_outcome(&outcome)));
    lines.push("log.jsonl:".to_owned());
    let (log, start, last) = logged(&record, &started, H::SCRIPTED);
    lines.extend(log);
    lines.extend(final_judge_stderr(&record));
    if let Some(last) = last {
        let last = when(&placed, last);
        lines.push(format!("its last record came {last}"));
    }
    if written(&record, start).is_some_and(|written| written >= hard_end) {
        lines.push("its log's last record was written once the hard end had passed".to_owned());
    }
    lines.extend(standard_outputs(&record));
    lines.push(format!("socket directories left: {}", sockets.exists()));
    lines.extend(left(providers, &tag.unwrap()));
    let address = stand_in.as_ref().map(|stand_in| stand_in.address.clone());
    for hung_up in stand_in.into_iter().flat_map(StandIn::stop) {
        // A run whose stream stalled has started. The stand-in reads the clock after the
        // hang-up, so this errs late.
        let hung_up = Duration::from_nanos(hung_up - start.unwrap());
        // Its place among the run's records isn't shown: a `model_answered` may come after the
        // run's last record, and so may a hang-up that its answer follows.
        let at = when(&placed, hung_up);
        lines.push(format!("a stalled stream's client hung up {at}"));
    }
    let runs_dir = record.parent().unwrap().display().to_string();
    let mut names = vec![
        (sockets.display().to_string(), "<sockets>"),
        (record.display().to_string(), "<record>"),
        (partial.display().to_string(), "<partial>"),
        (runs_dir, "<runs>"),
    ];
    names.extend(address.map(|address| (address, "<stand-in>")));
    (names.into_iter()).fold(lines.join("\n"), |shown, (path, name)| {
        shown.replace(&path, name)
    })
}

/// Whether `provider.json` in `record`, a run's, holds what each of `providers` declared, by its
/// name, with how its sandboxes meet the gateway; and, of a run on more than one, where it places
/// each role.
fn declared(record: &Path, providers: &[(&str, &Sim)]) -> Vec<String> {
    let recorded = fs::read_to_string(record.join("provider.json")).unwrap();
    let recorded: Value = serde_json::from_str(&recorded).unwrap();
    let reach = |provider: &Sim| match provider.meeting() {
        Meeting::Local(_) => "local",
    };
    let expected = (providers.iter()).map(|(name, provider)| {
        let mut ready = json!(provider.ready());
        ready["reach"] = reach(provider).into();
        ((*name).to_owned(), ready)
    });
    let declared = recorded["providers"] == Value::Object(expected.collect());
    // A run on one provider shows what it shows of it by no name.
    if providers.len() == 1 {
        return vec![format!(
            "provider.json is what the provider declared: {declared}"
        )];
    }
    vec![
        format!("provider.json is what each provider declared: {declared}"),
        format!("provider.json places the roles: {}", recorded["roles"]),
    ]
}

/// What each of `providers` shows of run `tag` once it has ended: what its platform shows
/// ([`shown`]), and what its list finds; and, of a run on more than one, by each one's name, what
/// it published, and the class of each create, too.
fn left(providers: &[(&str, &Sim)], tag: &RunTag) -> Vec<String> {
    let listed = |provider: &Sim| {
        let listed = provider.list(tag).unwrap();
        let listed = listed.into_iter().map(|Listed { id, ended: _ }| id);
        listed.map(|id| id.as_str().to_owned()).collect::<Vec<_>>()
    };
    if let [(_, provider)] = providers {
        let mut lines = shown(provider);
        lines.push(format!("listed once it ended: {:?}", listed(provider)));
        return lines;
    }
    let mut lines = Vec::new();
    for (name, provider) in providers {
        lines.push(format!("{name} published: {:?}", provider.published()));
        lines.extend(shown(provider).iter().map(|line| format!("{name}: {line}")));
        let classes: Vec<String> = provider.classes().into_iter().map(String::from).collect();
        lines.push(format!("{name}: the class at each create: {classes:?}"));
        lines.push(format!(
            "{name} listed once it ended: {:?}",
            listed(provider)
        ));
    }
    lines
}

/// A stand-in for a model provider, on the loopback, which answers a request whose input is
/// `stream` with a whole stream that echoes the request's line, its `authorization` and its
/// body, one whose input is `stall` with a stream's head and first part, then nothing, until
/// its client hangs up, one whose input is `expired` with a 401, as a provider refuses an
/// expired key, one whose input is `forbidden` with a 403, and one whose input is `down` with a
/// 502 that names its id for the request. Each stream's head names in
/// `openai-model` the model that the request's body named, as a provider does that answers with
/// the model asked for.
struct StandIn {
    address: String,
    stops: Sender<()>,
    /// Returns when the client of each stream it stalled hung up, in nanoseconds since the Unix
    /// epoch.
    thread: JoinHandle<Vec<u64>>,
}

impl StandIn {
    fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (stops, stopped) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut hung_up = Vec::new();
            for connection in listener.incoming() {
                if stopped.try_recv().is_ok() {
                    break;
                }
                hung_up.extend(answer(connection.unwrap()));
            }
            hung_up
        });
        Self {
            address,
            stops,
            thread,
        }
    }

    /// How a run serves `route` through it, with its key in a file in `dir`.
    fn serving(&self, dir: &Path, route: &ir::ModelRoute) -> Serving {
        let models = route.models.as_slice().iter().map(ModelName::as_str);
        let models: Vec<&str> = models.collect();
        serving(
            dir,
            &format!("http://{}/v1/responses", self.address),
            &models,
        )
    }

    /// Stops it, and returns when the client of each stream it stalled hung up, as its thread
    /// does.
    fn stop(self) -> Vec<u64> {
        self.stops.send(()).unwrap();
        drop(TcpStream::connect(&self.address).unwrap());
        self.thread.join().unwrap()
    }
}

/// Answers the request on `connection` as [`StandIn`] says, and returns when its client hung
/// up, if the stand-in stalled its stream.
fn answer(mut connection: TcpStream) -> Option<u64> {
    let (line, headers, body) = read_request(&mut BufReader::new(&connection));
    let key = headers.iter().find(|(name, _)| name == "authorization");
    let input: Value = serde_json::from_slice(&body).unwrap();
    let said = input["input"][1]["content"].as_str();
    let refused = match said {
        Some("expired") => Some(("401 Unauthorized", "")),
        Some("forbidden") => Some(("403 Forbidden", "")),
        Some("down") => Some(("502 Bad Gateway", "x-request-id: req-502\r\n")),
        _ => None,
    };
    if let Some((status, id)) = refused {
        let refused = format!(
            "HTTP/1.1 {status}\r\ncontent-type: text/plain\r\n{id}content-length: 7\r\n\
             connection: close\r\n\r\nrefused"
        );
        connection.write_all(refused.as_bytes()).unwrap();
        return None;
    }
    let streams = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nopenai-model: {}\r\n\
         transfer-encoding: chunked\r\nconnection: close\r\n\r\n",
        input["model"].as_str().unwrap()
    );
    connection.write_all(streams.as_bytes()).unwrap();
    let echo = format!(
        "data: {line} {key:?} {}\n\n",
        String::from_utf8(body).unwrap()
    );
    if said == Some("usage") {
        let completed = r#"{"type":"response.completed","response":{"usage":{"input_tokens":12,"input_tokens_details":{"cached_tokens":8},"output_tokens":5,"output_tokens_details":{"reasoning_tokens":3}}}}"#;
        write_chunk(
            &mut connection,
            "data: {\"type\":\"response.created\",\"response\":{\"usage\":null}}\n\n",
        );
        write_chunk(
            &mut connection,
            &format!("event: response.completed\ndata: {completed}\n\n"),
        );
        write_chunk(&mut connection, "");
        return None;
    }
    if said == Some("stream") {
        write_chunk(&mut connection, &echo);
        write_chunk(&mut connection, "");
        return None;
    }
    write_chunk(&mut connection, "event: response.created\n\n");
    // What the client sends is read until it hangs up, as its end shows that it has.
    let _ = io::copy(&mut connection, &mut io::sink());
    Some(unix_ns())
}

/// A host as a test shows its run: whether its clock reads only the times its script sets, so
/// that the run's times are the same each time it runs.
trait Shown: Host {
    const SCRIPTED: bool;
}

impl Shown for System {
    const SCRIPTED: bool = false;
}

impl Shown for SimHost {
    const SCRIPTED: bool = true;
}

/// The records of the run log in `record`, a run's, one per line, once [`in_order`] has checked
/// them; when the run started, in nanoseconds since the Unix epoch; and its last record's time.
/// It leaves out the `status` ops answered, alone, while their job was queued or running, whose
/// number depends on timing, and numbers the rest, and their ops, in turn. The run's start is
/// shown only if it isn't within `started`, the call to `execute`. Unless the run's times are
/// `scripted`, a record's times are shown only if 0, and the time left in a reply as `<s>`.
fn logged(
    record: &Path,
    started: &RangeInclusive<u64>,
    scripted: bool,
) -> (Vec<String>, Option<u64>, Option<Duration>) {
    let (mut lines, mut start, mut polls) = (Vec::new(), None, 0);
    // A run that fails before it starts has no log.
    let log = fs::read_to_string(record.join("log.jsonl")).unwrap_or_default();
    let log: Vec<Value> = (log.lines())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let last = in_order(&log);
    for mut logged in log {
        let actions = logged["actions"].as_array().map_or(0, Vec::len);
        let state = logged.pointer("/actions/0/reply/state");
        // Their number depends on timing.
        if logged["event"]["request"]["kind"] == "status"
            && actions == 1
            && state.is_some_and(|state| state == "queued" || state == "running")
        {
            polls += 1;
            continue;
        }
        logged["seq"] = json!(lines.len());
        let ops = iter::once("/event/op".to_owned())
            .chain((0..actions).map(|action| format!("/actions/{action}/op")));
        for op in ops {
            if let Some(op) = logged.pointer_mut(&op) {
                *op = json!(op.as_u64().unwrap() - polls);
            }
        }
        for time in ["at_ns", "observed_ns"] {
            if let Some(ns) = logged.get_mut(time)
                && nanos(ns) != Some(0)
                && !scripted
            {
                *ns = json!("<ns>");
            }
        }
        if let Some(unix_ns) = logged.pointer_mut("/event/unix_ns") {
            start = nanos(unix_ns);
            if start.is_some_and(|ns| started.contains(&ns)) {
                *unix_ns = json!("<during execute>");
            }
        }
        let actions = logged.get_mut("actions").and_then(Value::as_array_mut);
        for action in actions.into_iter().flatten() {
            if let Some(seconds) = action.pointer_mut("/reply/remaining_seconds")
                && !scripted
            {
                *seconds = json!("<s>");
            }
        }
        lines.push(logged.to_string());
    }
    (lines, start, last)
}

/// The time of the last of `log`'s records, a run log's, once it checks that they are numbered
/// from 0 with no gap, none timed before the one before it, and no event observed after its
/// record was written.
fn in_order(log: &[Value]) -> Option<Duration> {
    let time = |ns: &Value| Duration::from_nanos(nanos(ns).unwrap());
    let mut last = None;
    for (seq, logged) in log.iter().enumerate() {
        assert_eq!(logged["seq"], json!(seq), "{logged}");
        let at = Some(time(&logged["at_ns"]));
        assert!(last <= at, "{logged} comes before the record before it");
        let observed = logged.get("observed_ns").map(time);
        assert!(observed <= at, "{logged} was observed after it was written");
        last = at;
    }
    last
}

/// How long after its start, `start` nanoseconds since the Unix epoch, the log in `record`, a
/// run's, was last written, if the run started.
fn written(record: &Path, start: Option<u64>) -> Option<Duration> {
    // By the record alone: the call to `execute` also publishes before the run's clock starts,
    // and removes its sockets after its log ends.
    let start = Duration::from_nanos(start?);
    let modified = fs::metadata(record.join("log.jsonl")).and_then(|log| log.modified());
    let written = modified.unwrap().duration_since(UNIX_EPOCH).unwrap();
    Some(written.saturating_sub(start))
}

fn final_judge_stderr(record: &Path) -> Option<String> {
    let opened = Record::open(record).unwrap();
    let Ending::NoResult {
        judge_stderr: found,
        ..
    } = opened.wait(|| {}).unwrap()
    else {
        return None;
    };
    let found = found.map(|path| path.strip_prefix(opened.dir()).unwrap().to_owned());
    Some(format!(
        "the final judge's standard error, as `wait` finds it: {found:?}"
    ))
}

/// When `last`, a time since a run of `placed` started, came: before the agent's teardown was
/// to end, at most the hard end ([`lasts`]) less the final judge's startup and timeout; before
/// the hard end; or after it.
fn when(placed: &Placed, last: Duration) -> &'static str {
    let score = placed.workflow().score();
    let startup = placed.startup(score.slot.judge().role());
    let hard_end = lasts(placed);
    let teardown_end = hard_end.saturating_sub(startup + score.timeout);
    if last < teardown_end {
        "before the agent's teardown was to end"
    } else if last < hard_end {
        "once the agent's teardown was to end, before the hard end"
    } else {
        "once the hard end had passed"
    }
}

/// The fixture's workflow, without the agent's model route ([`routed`] gives one), and with
/// `argv` as the agent's command.
fn workflow(argv: &[&str]) -> Value {
    let mut ir: Value = serde_json::from_str(WORKFLOW).unwrap();
    let agent = &mut ir["roles"][0];
    agent.as_object_mut().unwrap().remove("egress");
    agent["runtime"]["agent"]["command"]["argv"] = json!(argv);
    ir
}

/// The workflow of the runs whose agents make ops: [`workflow`]'s, with `argv` as the agent's
/// command and `deadline` as the agent's deadline, in seconds; both judges judging the payload
/// in their slot as the simulated provider's `judge` command does; room in the train judge's
/// slot for 8 bytes and in the score's for 16, the most an op of the agent's may carry; and a
/// call timeout of 1 s, shorter than the time left in any run that makes its calls soon after it
/// starts, without a cumulative limit, so that it is each call's timeout, and what a reply has
/// left depends on no command's time.
fn ops_workflow(argv: &[&str], deadline: u64) -> Value {
    let mut ir = workflow(argv);
    let grant = &mut ir["roles"][0]["grants"][0]["judge"];
    grant["call_timeout_seconds"] = json!(1);
    grant.as_object_mut().unwrap().remove("cumulative_seconds");
    for role in [1, 2] {
        let judge = &mut ir["roles"][role]["runtime"]["judge"];
        judge["command"]["argv"] = json!(["judge", "/pumpkin/in/payload/raw"]);
    }
    ir["roles"][1]["runtime"]["judge"]["slots"][0]["max_bytes"] = json!(8);
    ir["roles"][2]["runtime"]["judge"]["slots"][0]["max_bytes"] = json!(16);
    ir["roles"][0]["runtime"]["agent"]["deadline_seconds"] = json!(deadline);
    ir
}

/// `ir`, with a model route for its agent that allows `m-large`.
fn routed(mut ir: Value) -> Value {
    let route = json!({ "models": ["m-large"] });
    ir["roles"][0]["egress"] = json!({ "model": route });
    ir
}

/// [`ops_workflow`]'s workflow, with the run's other shares of its hard end as short as they
/// go: no finalize grace, and a score timeout of 1 s, so that a run that waits for its
/// teardown's end or its hard end waits seconds.
fn short_workflow(argv: &[&str], deadline: u64) -> Value {
    let mut ir = ops_workflow(argv, deadline);
    ir["roles"][0]["runtime"]["agent"]["finalize_grace_seconds"] = json!(0);
    ir["score"]["timeout_seconds"] = json!(1);
    ir
}

// A run whose agent's sandbox its provider refuses to delete once: the agent's teardown fails to
// delete it, and the run's sweep, which finds it again, deletes it, which its record says last;
// the run fails for the first delete.
#[test]
fn deleted_by_the_sweep() {
    check(
        &System,
        "deleted-by-the-sweep",
        &[(PROVIDER, &Sim::new())],
        &workflow(&["exit", "0"]),
        REFUSED_ONCE,
        &expect![[r#"
            failed: deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`
            record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"reason":"deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","state":"failed"}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"18270000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":3}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":4}
            {"at_ns":"<ns>","note":"the run failed: deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","seq":5}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

// A run whose agent's sandbox its provider refuses to delete once, as in
// [`deleted_by_the_sweep`], on a host whose sync of its record of the sweep's delete fails: the
// run fails for the first delete and for that sync, and the record keeps the first delete's
// outcome.
#[test]
fn sweep_unrecorded() {
    check(
        &SimHost::new().failed_sync_of(".sandbox.json.partial", 2),
        "sweep-unrecorded",
        &[(PROVIDER, &Sim::new())],
        &workflow(&["exit", "0"]),
        REFUSED_ONCE,
        &expect![[r#"
            failed: deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; <record>/agent/.sandbox.json.partial: the simulated host's sync failed
            record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"reason":"sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","state":"failed"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"reason":"deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; <record>/agent/.sandbox.json.partial: the simulated host's sync failed","state":"failed"}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"18270000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"agent_started"},"observed_ns":"0","seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"0","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":2}
            {"at_ns":"0","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":3}
            {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":4}
            {"at_ns":"0","note":"the run failed: deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; <record>/agent/.sandbox.json.partial: the simulated host's sync failed","seq":5}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

// A run whose agent's sandbox can't be deleted, and whose console its provider can't read: the
// agent's teardown notes the console it didn't keep, and so does the run's sweep, which finds
// the sandbox again; the run fails for the delete alone.
#[test]
fn unshown() {
    let mut ir = workflow(&["exit", "0"]);
    ir["roles"][0]["image"] = json!(sim::image(UNSHOWN).as_str());
    check(
        &System,
        "unshown",
        &[(PROVIDER, &Sim::new())],
        &ir,
        UNDELETABLE,
        &expect![[r#"
            failed: deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `simulated` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`
            record: agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"reason":"sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","state":"failed"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"reason":"deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `simulated` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","state":"failed"}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"18270000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":3}
            {"at_ns":"<ns>","note":"sandbox sim-1's console wasn't kept: the simulated console of sim-1 can't be read: create one with another `sandboxd`","seq":4}
            {"at_ns":"<ns>","note":"sandbox sim-1's console wasn't kept: the simulated console of sim-1 can't be read: create one with another `sandboxd`","seq":5}
            {"at_ns":"<ns>","note":"the run failed: deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `simulated` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","seq":6}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: ["sim-1"]"#]],
    );
}

// A run whose agent's `sandboxd` exits without dialing, and whose sandbox can't be deleted, after
// which its console shows nothing: the record keeps the console its failed start read, which the
// run's sweep, finding the sandbox again, doesn't read again.
#[test]
fn unstarted_undeletable() {
    let mut ir = workflow(&["exit", "0"]);
    ir["roles"][0]["image"] = json!(sim::image(MUTE).as_str());
    check(
        &System,
        "unstarted-undeletable",
        &[(PROVIDER, &Sim::new())],
        &ir,
        UNDELETABLE,
        &expect![[r#"
            failed: deleting the sandboxes the run left on provider `simulated` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`
            record: agent/console.txt, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"reason":"sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","state":"failed"},"id":"sim-1","provider":"simulated","report":null}
            the agent's console, agent/console.txt: "sandboxd: a simulated one, which never dials\n"
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"reason":"deleting the sandboxes the run left on provider `simulated` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","state":"failed"}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"18270000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: sandbox sim-1: it ended before it met the gateway: expected its `sandboxd` to run until it connects; its console ends: sandboxd: a simulated one, which never dials; deleting it failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: sandbox sim-1: it ended before it met the gateway: expected its `sandboxd` to run until it connects; its console ends: sandboxd: a simulated one, which never dials; deleting it failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`"},"observed_ns":"<ns>","seq":1}
            {"at_ns":"<ns>","note":"the agent's sandbox never started: nothing was collected","seq":2}
            {"at_ns":"<ns>","note":"the run failed: deleting the sandboxes the run left on provider `simulated` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","seq":3}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: ["sim-1"]"#]],
    );
}

// A run whose agent's command exits: its exit ends the work phase without a submission, and
// the agent's sandbox is collected and deleted, so the run finishes without a score.
#[test]
fn exits() {
    // A deadline far later than the exit, so that an exit the run misses ends the work phase.
    let mut ir = workflow(&["exit", "3"]);
    ir["roles"][0]["runtime"]["agent"]["deadline_seconds"] = json!(30);
    check(
        &System,
        "exits",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":3}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":3}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":3}},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":3}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":4}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

#[test]
fn crashes() {
    // A deadline far later than the session's end, so that an end the run misses ends the work
    // phase.
    let mut ir = workflow(&["crash"]);
    ir["roles"][0]["runtime"]["agent"]["deadline_seconds"] = json!(30);
    check(
        &System,
        "crashes",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/console.txt, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"freezing the agent's sandbox failed: the sandbox ended the session","seq":3}
            {"at_ns":"<ns>","note":"the agent's out is incomplete: collecting /pumpkin/out failed: the sandbox ended the session","seq":4}
            {"at_ns":"<ns>","note":"the agent's log is incomplete: collecting /pumpkin/log failed: the sandbox ended the session","seq":5}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, nothing; log, nothing","seq":6}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":7}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

#[test]
fn stalls() {
    let mut ir = workflow(&["stall"]);
    let agent = &mut ir["roles"][0]["runtime"]["agent"];
    agent["deadline_seconds"] = json!(1);
    agent["finalize_grace_seconds"] = json!(0);
    check(
        &System,
        "stalls",
        &[(PROVIDER, &Sim::with_startup(SHORT_STARTUP))],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/console.txt, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"124000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}}],"at_ns":"<ns>","event":{"kind":"tick"},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"freezing the agent's sandbox failed: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts","seq":3}
            {"at_ns":"<ns>","note":"the agent's out is incomplete: collecting /pumpkin/out failed: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts","seq":4}
            {"at_ns":"<ns>","note":"the agent's log is incomplete: collecting /pumpkin/log failed: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts","seq":5}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, nothing; log, nothing","seq":6}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":7}
            its last record came once the agent's teardown was to end, before the hard end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

// A run whose agent's `sandboxd` exits without dialing: the run's watcher finds its sandbox
// ended, whether its list then finds it so or leaves it out, long before its class's
// startup, which ends its start then, and the sandbox is deleted, with its console quoted in
// the agent's failure. Both runs end before the startup has passed once, which a start that
// waited it out wouldn't.
#[test]
fn unstarted() {
    let ir = workflow(&["exit", "0"]);
    let parsed: ir::Workflow = serde_json::from_value(ir.clone()).unwrap();
    let placed = placed(&parsed, Sim::new().ready()).unwrap();
    let startup = placed.startup(placed.workflow().agent().role);
    let begun = Instant::now();
    let on = |name, sandboxd| {
        executed(
            &System,
            name,
            &[(PROVIDER, &Sim::new())],
            &ir,
            sandboxd,
            None,
        )
    };
    let (listed, left_out) = (on("unstarted", MUTE), on("unstarted-left-out", VANISHING));
    let took = begun.elapsed();
    assert!(
        took < startup,
        "the runs took {took:?}, the startup {startup:?}"
    );
    assert_eq!(
        left_out, listed,
        "left out of the list, against listed as ended"
    );
    expect![[r#"
        finished, with score None
        record: agent/console.txt, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
        the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
        the agent's console, agent/console.txt: "sandboxd: a simulated one, which never dials\n"
        payloads hold: []
        provider.json is what the provider declared: true
        outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: sandbox sim-1: it ended before it met the gateway: expected its `sandboxd` to run until it connects; its console ends: sandboxd: a simulated one, which never dials"}}
        log.jsonl:
        {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"18270000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: sandbox sim-1: it ended before it met the gateway: expected its `sandboxd` to run until it connects; its console ends: sandboxd: a simulated one, which never dials"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: sandbox sim-1: it ended before it met the gateway: expected its `sandboxd` to run until it connects; its console ends: sandboxd: a simulated one, which never dials"},"observed_ns":"<ns>","seq":1}
        {"at_ns":"<ns>","note":"the agent's sandbox never started: nothing was collected","seq":2}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":3}
        its last record came before the agent's teardown was to end
        socket directories left: false
        their directory's mode at each create: ["700"]
        listed once it ended: []"#]].assert_eq(&listed);
}

// A run that SIGTERM interrupts stops as one that fails does, deleting its sandboxes, and records
// the signal, long before what it waits on would end (`WAITED`). Each case is a test of its own,
// as a signal interrupts every later run of the process.

/// Checks the run of `ir`, as test `name`, on a simulated provider whose startup is `startup`
/// seconds, with `sandboxd`, interrupted `at` a step.
fn interrupted(name: &str, startup: u64, ir: &Value, sandboxd: &str, at: At, expect: &Expect) {
    let provider = Sim::with_startup(startup);
    let test = format!("interrupted-{name}");
    let providers = [(PROVIDER, &provider)];
    expect.assert_eq(&executed(
        &System,
        &test,
        &providers,
        ir,
        sandboxd,
        Some(at),
    ));
}

#[test]
fn interrupted_publishing_sandboxd() {
    // One interrupted as it publishes never starts, nor publishes more.
    interrupted(
        "publishing-sandboxd",
        30,
        &workflow(&["exit", "0"]),
        SANDBOXD,
        At::PublishSandboxd,
        &expect![[r#"
        was interrupted by SIGTERM
        it started: false
        it returned within 60 s: true
        it published: ["sandboxd /sim/sandboxd"]
        record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json
        payloads hold: []
        provider.json is what the provider declared: true
        outcome.json: {"signal":"SIGTERM","state":"interrupted"}
        log.jsonl:
        socket directories left: false
        their directory's mode at each create: []
        listed once it ended: []"#]],
    );
}

#[test]
fn interrupted_publishing_image() {
    // Nor does one interrupted once its last publish is under way.
    let mut one_image = workflow(&["exit", "0"]);
    for judge in [1, 2] {
        one_image["roles"][judge]["image"] = one_image["roles"][0]["image"].clone();
    }
    interrupted(
        "publishing-image",
        30,
        &one_image,
        SANDBOXD,
        At::PublishImage,
        &expect![[r#"
            was interrupted by SIGTERM
            it started: false
            it returned within 60 s: true
            it published: ["sandboxd /sim/sandboxd", "image example/agent@sha256:1111111111111111111111111111111111111111111111111111111111111111"]
            record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"signal":"SIGTERM","state":"interrupted"}
            log.jsonl:
            socket directories left: false
            their directory's mode at each create: []
            listed once it ended: []"#]],
    );
}

#[test]
fn interrupted_agent_start() {
    // Once the agent's sandbox is created, whose `sandboxd` never dials, it ends the agent's start.
    interrupted(
        "agent-start",
        WAITED,
        &workflow(&["exit", "0"]),
        HUNG,
        At::Created("agent"),
        &expect![[r#"
            was interrupted by SIGTERM
            it started: true
            it returned within 60 s: true
            it published: ["sandboxd /sim/hung", "image example/agent@sha256:1111111111111111111111111111111111111111111111111111111111111111", "image example/judge@sha256:2222222222222222222222222222222222222222222222222222222222222222"]
            record: agent/console.txt, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            the agent's console, agent/console.txt: "sandboxd: a simulated one, which never dials\n"
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"signal":"SIGTERM","state":"interrupted"}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"18360000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"at_ns":"<ns>","note":"the run was interrupted by SIGTERM","seq":1}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":2}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

#[test]
fn interrupted_judge_start() {
    // Once a judge's sandbox is created, of an image whose `sandboxd` never dials, while the agent
    // awaits the call, it ends the judge's start.
    let mut judged = ops_workflow(&["ops", "judge train 6", "await 0 completed"], WAITED);
    judged["roles"][1]["image"] = json!(sim::image(HUNG).as_str());
    interrupted(
        "judge-start",
        WAITED,
        &judged,
        SANDBOXD,
        At::Created("job-0"),
        &expect![[r#"
            was interrupted by SIGTERM
            it started: true
            it returned within 60 s: true
            it published: ["sandboxd /sim/sandboxd", "image example/agent@sha256:1111111111111111111111111111111111111111111111111111111111111111", "image sha256:2f73696d2f68756e670000000000000000000000000000000000000000000000", "image example/judge@sha256:2222222222222222222222222222222222222222222222222222222222222222"]
            record: agent/console.txt, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/console.txt, jobs/0/sandbox.json, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-2","provider":"simulated","report":null}
            job 0's console, jobs/0/console.txt: "sandboxd: a simulated one, which never dials\n"
            payloads hold: ["xxxxxx"]
            provider.json is what the provider declared: true
            outcome.json: {"signal":"SIGTERM","state":"interrupted"}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"420000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"the run was interrupted by SIGTERM","seq":3}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":4}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":5}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700", "700"]
            listed once it ended: []"#]],
    );
}

#[test]
fn interrupted_working() {
    // Once the agent has started, while it works and makes no op, it ends the loop's wait for its
    // next event.
    let mut works = workflow(&["work"]);
    works["roles"][0]["runtime"]["agent"]["deadline_seconds"] = json!(WAITED);
    interrupted(
        "working",
        30,
        &works,
        SANDBOXD,
        At::AgentStarted,
        &expect![[r#"
            was interrupted by SIGTERM
            it started: true
            it returned within 60 s: true
            it published: ["sandboxd /sim/sandboxd", "image example/agent@sha256:1111111111111111111111111111111111111111111111111111111111111111", "image example/judge@sha256:2222222222222222222222222222222222222222222222222222222222222222"]
            record: agent/console.txt, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"signal":"SIGTERM","state":"interrupted"}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"330000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"at_ns":"<ns>","note":"the run was interrupted by SIGTERM","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":3}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

#[test]
fn interrupted_last_batch() {
    // One interrupted in the core's last batch, as it deletes the agent's sandbox, finishes as it
    // would have.
    let mut last = workflow(&["exit", "3"]);
    last["roles"][0]["runtime"]["agent"]["deadline_seconds"] = json!(30);
    interrupted(
        "last-batch",
        30,
        &last,
        SANDBOXD,
        At::Delete,
        &expect![[r#"
            finished, with score None
            it started: true
            it returned within 60 s: true
            it published: ["sandboxd /sim/sandboxd", "image example/agent@sha256:1111111111111111111111111111111111111111111111111111111111111111", "image example/judge@sha256:2222222222222222222222222222222222222222222222222222222222222222"]
            record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":3}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":3}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":3}},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":3}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":4}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

// A run whose agent's `sandboxd` hangs without dialing while the run's list fails: its start
// runs out at the provider's startup, and the sandbox is deleted, with the last list's failure
// and its console quoted in the agent's failure, which ends the work phase: the start lasts at
// least as long as the deadline, which counts only from the agent's start.
#[test]
fn unlistable() {
    let mut ir = workflow(&["exit", "0"]);
    // Outlasted by the start, so that the core hears of its failure before the deadline's tick.
    ir["roles"][0]["runtime"]["agent"]["deadline_seconds"] = json!(SHORT_STARTUP);
    check(
        &System,
        "unlistable",
        &[(PROVIDER, &Sim::with_startup(SHORT_STARTUP))],
        &ir,
        UNLISTABLE,
        &expect![[r#"
            finished, with score None
            record: agent/console.txt, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            the agent's console, agent/console.txt: "sandboxd: a simulated one, which never dials\n"
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: sandbox sim-1: no sandbox connected to <sockets>/agent/s by the deadline: expected its sandbox to connect while the deadline lasts; the last list of its provider failed: the simulated list failed while sim-1 lasts: delete it, or create one with another `sandboxd`; its console ends: sandboxd: a simulated one, which never dials"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"184000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: sandbox sim-1: no sandbox connected to <sockets>/agent/s by the deadline: expected its sandbox to connect while the deadline lasts; the last list of its provider failed: the simulated list failed while sim-1 lasts: delete it, or create one with another `sandboxd`; its console ends: sandboxd: a simulated one, which never dials"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: sandbox sim-1: no sandbox connected to <sockets>/agent/s by the deadline: expected its sandbox to connect while the deadline lasts; the last list of its provider failed: the simulated list failed while sim-1 lasts: delete it, or create one with another `sandboxd`; its console ends: sandboxd: a simulated one, which never dials"},"observed_ns":"<ns>","seq":1}
            {"at_ns":"<ns>","note":"the agent's sandbox never started: nothing was collected","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":3}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

// A run whose agent's `sandboxd` speaks the sandbox protocol's next version: the gateway refuses
// its hello at once, and the agent fails, saying which version each end speaks.
#[test]
fn mismatched() {
    let provider = Sim::new();
    check(
        &System,
        "mismatched",
        &[(PROVIDER, &provider)],
        &workflow(&["exit", "0"]),
        LATER,
        &expect![[r#"
            finished, with score None
            record: agent/console.txt, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: sandbox sim-1: the gateway refused the session: sandboxd speaks sandbox protocol 1: expected 0, which this gateway speaks, as a sandboxd built from the same Pumpkin does"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"18270000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: sandbox sim-1: the gateway refused the session: sandboxd speaks sandbox protocol 1: expected 0, which this gateway speaks, as a sandboxd built from the same Pumpkin does"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: sandbox sim-1: the gateway refused the session: sandboxd speaks sandbox protocol 1: expected 0, which this gateway speaks, as a sandboxd built from the same Pumpkin does"},"observed_ns":"<ns>","seq":1}
            {"at_ns":"<ns>","note":"the agent's sandbox never started: nothing was collected","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":3}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

#[test]
fn lost() {
    check(
        &System,
        "lost",
        &[(PROVIDER, &Sim::new())],
        &workflow(&["exit", "0"]),
        LOST,
        &expect![[r#"
            finished, with score None
            record: driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: the simulated create failed, and can't name what it made: expected the run's list to find it"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"18270000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: the simulated create failed, and can't name what it made: expected the run's list to find it"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: the simulated create failed, and can't name what it made: expected the run's list to find it"},"observed_ns":"<ns>","seq":1}
            {"at_ns":"<ns>","note":"the agent's sandbox never started: nothing was collected","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":null},"seq":3}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

#[test]
fn undeletable() {
    check(
        &System,
        "undeletable",
        &[(PROVIDER, &Sim::new())],
        &workflow(&["exit", "0"]),
        UNDELETABLE,
        &expect![[r#"
            failed: deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `simulated` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`
            record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"reason":"sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","state":"failed"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"reason":"deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `simulated` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","state":"failed"}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"18270000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":3}
            {"at_ns":"<ns>","note":"the run failed: deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `simulated` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","seq":4}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: ["sim-1"]"#]],
    );
}

#[test]
fn unpublished() {
    check(
        &System,
        "unpublished",
        &[(PROVIDER, &Sim::new())],
        &workflow(&["exit", "0"]),
        UNPUBLISHED,
        &expect![[r#"
            failed: publishing `sandboxd` on provider `simulated` failed: /sim/unpublished can't be published by the simulated provider: name another path
            record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"reason":"publishing `sandboxd` on provider `simulated` failed: /sim/unpublished can't be published by the simulated provider: name another path","state":"failed"}
            log.jsonl:
            socket directories left: false
            their directory's mode at each create: []
            listed once it ended: []"#]],
    );
}

// A run whose judge's image the provider can't publish: the run fails before it starts, naming
// the judge, and nothing is created.
#[test]
fn unpublished_image() {
    let mut ir = workflow(&["exit", "0"]);
    ir["roles"][1]["image"] = json!(sim::image(UNPUBLISHED).as_str());
    check(
        &System,
        "unpublished-image",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &expect![[r#"
            failed: publishing role `train_judge`'s image on provider `simulated` failed: sha256:2f73696d2f756e7075626c697368656400000000000000000000000000000000 can't be published by the simulated provider: name another image
            record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"reason":"publishing role `train_judge`'s image on provider `simulated` failed: sha256:2f73696d2f756e7075626c697368656400000000000000000000000000000000 can't be published by the simulated provider: name another image","state":"failed"}
            log.jsonl:
            socket directories left: false
            their directory's mode at each create: []
            listed once it ended: []"#]],
    );
}

#[test]
fn ops() {
    let argv = [
        "ops",
        "ops",
        "status 0",
        "judge nope 6",
        "judge train 9",
        "judge train 17",
        "judge train 6",
        "await 0 completed",
        "judge train slow",
        "await 1 errored",
        "finalize submit 6",
    ];
    // A deadline far later than the agent's last op, so that a reply the agent misses ends the
    // work phase.
    check(
        &System,
        "ops",
        &[(PROVIDER, &Sim::new())],
        &ops_workflow(&argv, 30),
        SANDBOXD,
        &expect![[r#"
            finished, with score Some(Number(6))
            record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/console.txt, jobs/0/log/collected, jobs/0/out/collected, jobs/0/out/result.json, jobs/0/sandbox.json, jobs/1/console.txt, jobs/1/log/collected, jobs/1/out/collected, jobs/1/sandbox.json, jobs/2/console.txt, jobs/2/log/collected, jobs/2/out/collected, jobs/2/out/result.json, jobs/2/sandbox.json, log.jsonl, outcome.json, payloads/5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-2","provider":"simulated","report":null}
            job 1's sandbox, jobs/1/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-3","provider":"simulated","report":null}
            job 2's sandbox, jobs/2/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-4","provider":"simulated","report":null}
            payloads hold: ["slow", "xxxxxx"]
            provider.json is what the provider declared: true
            outcome.json: {"score":6,"state":"finished","verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":"<s>","resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"kind":"ops"}},"observed_ns":"<ns>","seq":2}
            {"actions":[{"kind":"reply","op":1,"reply":{"kind":"refused","message":"you have no job 0: ask about a job that one of your calls returned"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":1,"request":{"job":"0","kind":"status"}},"observed_ns":"<ns>","seq":3}
            {"actions":[{"kind":"reply","op":2,"reply":{"kind":"refused","message":"you hold no grant named `nope`: call one that `ops` lists"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":2,"request":{"grant":"nope","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":4}
            {"actions":[{"kind":"reply","op":3,"reply":{"kind":"refused","message":"the payload is 9 bytes, over `train`'s limit of 8: send a smaller one"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":3,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:a73add1aecea03a894ca65d99bc5e91d21b58cdb728343bbcb6cf6cf38b809f5","size":9}}},"observed_ns":"<ns>","seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"op_refused","message":"the payload is 17 bytes, more than the limit of 16: send at most 16","request":{"grant":"train","kind":"judge","payload":{"over":16,"size":17}}},"seq":6}
            {"actions":[{"kind":"reply","op":4,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":4,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":7}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":8}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":9}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":10}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":11}
            {"actions":[{"kind":"reply","op":5,"reply":{"job":0,"kind":"status","remaining":{"calls":99,"seconds":null},"returned":{"raw_score":6},"scored":true,"state":"completed"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":5,"request":{"job":"0","kind":"status"}},"observed_ns":"<ns>","seq":12}
            {"actions":[{"kind":"reply","op":6,"reply":{"job":1,"kind":"judge","remaining":{"calls":98,"seconds":null}}},{"grant":"train","job":1,"kind":"start_judge","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":6,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4}}},"observed_ns":"<ns>","seq":13}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_started"},"observed_ns":"<ns>","seq":14}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_timed_out"},"observed_ns":"<ns>","seq":15}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-1"},"seq":16}
            {"actions":[],"at_ns":"<ns>","event":{"invalid":"the judge wrote no result: write it at /pumpkin/out/result.json","job":1,"kind":"judge_collected"},"observed_ns":"<ns>","seq":17}
            {"actions":[{"kind":"reply","op":7,"reply":{"failure":{"code":"judge_timeout","message":"the judge ran past its timeout"},"job":1,"kind":"status","remaining":{"calls":98,"seconds":null},"state":"errored"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":7,"request":{"job":"1","kind":"status"}},"observed_ns":"<ns>","seq":18}
            {"actions":[{"kind":"reply","op":8,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":8,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":19}
            {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":2,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":20}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":21}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":22}
            {"actions":[],"at_ns":"<ns>","event":{"job":2,"kind":"judge_started"},"observed_ns":"<ns>","seq":23}
            {"actions":[],"at_ns":"<ns>","event":{"job":2,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":24}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-4","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-2"},"seq":25}
            {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":2,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":26}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700", "700", "700", "700"]
            listed once it ended: []"#]],
    );
}

// A run whose agent finalizes a submission whose final judge writes a result over the most a
// result may hold: the result is removed from the record, and the run finishes with no score.
#[test]
fn result_over_the_limit() {
    check(
        &System,
        "result-over-the-limit",
        &[(PROVIDER, &Sim::new())],
        &ops_workflow(&["ops", "finalize submit large"], 30),
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/console.txt, jobs/0/log/collected, jobs/0/out/collected, jobs/0/sandbox.json, log.jsonl, outcome.json, payloads/d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-2","provider":"simulated","report":null}
            payloads hold: ["large"]
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"failure":{"kind":"invalid_result","reason":"result.json is 1048577 bytes, over the limit of 1048576: write a smaller one"},"kind":"no_result","submission":{"finalized":true,"payload":{"digest":"sha256:d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9","size":5}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9","size":5}}},"observed_ns":"<ns>","seq":2}
            {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9","size":5},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":3}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":4}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":6}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":7}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":8}
            {"actions":[{"kind":"finish","outcome":{"score":null,"verdict":{"failure":{"kind":"invalid_result","reason":"result.json is 1048577 bytes, over the limit of 1048576: write a smaller one"},"kind":"no_result","submission":{"finalized":true,"payload":{"digest":"sha256:d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9","size":5}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"invalid":"result.json is 1048577 bytes, over the limit of 1048576: write a smaller one","job":0,"kind":"judge_collected"},"observed_ns":"<ns>","seq":9}
            the final judge's standard error, as `wait` finds it: Ok("jobs/0/log/command.stderr")
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700", "700"]
            listed once it ended: []"#]],
    );
}

#[test]
fn crashes_with_an_op_in_flight() {
    let mut ir = ops_workflow(&["ops", "judge train 12", "crash"], 30);
    // Slow to create, so that the agent's end stops the call before the judge's command starts.
    ir["roles"][1]["image"] = json!(sim::image(SLOW).as_str());
    // Over the score's slot but within the judge's, so within the most an op may carry.
    ir["roles"][1]["runtime"]["judge"]["slots"][0]["max_bytes"] = json!(16);
    ir["roles"][2]["runtime"]["judge"]["slots"][0]["max_bytes"] = json!(8);
    check(
        &System,
        "crashes-with-an-op",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/console.txt, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/console.txt, jobs/0/sandbox.json, log.jsonl, outcome.json, payloads/59ffe12a70df15109e0345955e3230a978f31ebc28d8fe3e42d306afb28b8e81, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-2","provider":"simulated","report":null}
            payloads hold: ["xxxxxxxxxxxx"]
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:59ffe12a70df15109e0345955e3230a978f31ebc28d8fe3e42d306afb28b8e81","size":12},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:59ffe12a70df15109e0345955e3230a978f31ebc28d8fe3e42d306afb28b8e81","size":12}}},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"op 0's reply failed: the sandbox ended the session","seq":3}
            {"actions":[{"job":0,"kind":"stop_judge"},{"kind":"tear_down_agent"}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"},"observed_ns":"<ns>","seq":4}
            {"at_ns":"<ns>","note":"freezing the agent's sandbox failed: the sandbox ended the session","seq":5}
            {"at_ns":"<ns>","note":"the agent's out is incomplete: collecting /pumpkin/out failed: the sandbox ended the session","seq":6}
            {"at_ns":"<ns>","note":"the agent's log is incomplete: collecting /pumpkin/log failed: the sandbox ended the session","seq":7}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, nothing; log, nothing","seq":8}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":9}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":10}
            {"actions":[{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"the call was stopped before its command started"},"observed_ns":"<ns>","seq":11}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700", "700"]
            listed once it ended: []"#]],
    );
}

// A run whose agent makes a judge call, then exits, and whose judge's sandbox takes seconds to
// create, past the deadline: an agent's call starts by the work phase's end, which comes by the
// deadline, however long the provider's startup, so the create gives up then, leaving the
// sandbox for the run's end to delete, and the run finishes without a submission.
#[test]
fn judge_starts_past_the_deadline() {
    // A deadline that the create, which begins once the agent has started, outlasts, and that
    // leaves the call its whole timeout.
    let mut ir = ops_workflow(&["ops", "judge train 6", "exit"], 2);
    ir["roles"][1]["image"] = json!(sim::image(SLOW).as_str());
    check(
        &System,
        "judge-starts-past-the-deadline",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: ["xxxxxx"]
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"272000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":2}
            {"actions":[{"job":0,"kind":"stop_judge"},{"kind":"tear_down_agent"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":3}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":4}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":5}
            {"actions":[{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"the judge's sandbox didn't start: the simulated create gave up at its deadline, and can't name what it made: expected the run's list to find it"},"observed_ns":"<ns>","seq":6}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":null},"seq":7}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700", "700"]
            listed once it ended: []"#]],
    );
}

// A run whose train judge needs the CPU model [`BRIEF`], so is placed in that class, whose
// sandboxes start in seconds, and whose `sandboxd` hangs without dialing: its start runs out at
// its own class's startup, though the agent's class gives 30 s, so the agent, which awaits the
// call's end, sees it errored, and exits long before the deadline.
#[test]
fn judge_placed_apart() {
    // The judge's start runs out at its own class's startup, though the agent's class gives
    // 30 s, so the agent sees the call errored long before the deadline.
    let mut ir = ops_workflow(&["ops", "judge train 6", "await 0 errored"], 30);
    ir["roles"][1]["image"] = json!(sim::image(HUNG).as_str());
    ir["roles"][1]["needs"]["cpu_model"] = json!(BRIEF);
    check(
        &System,
        "judge-placed-apart",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/console.txt, jobs/0/sandbox.json, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-2","provider":"simulated","report":null}
            job 0's console, jobs/0/console.txt: "sandboxd: a simulated one, which never dials\n"
            payloads hold: ["xxxxxx"]
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":3}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"the judge's sandbox didn't start: sandbox sim-2: no sandbox connected to <sockets>/job-0/s by the deadline: expected its sandbox to connect while the deadline lasts; its console ends: sandboxd: a simulated one, which never dials"},"observed_ns":"<ns>","seq":4}
            {"actions":[{"kind":"reply","op":1,"reply":{"failure":{"code":"judge_error","message":"the judge failed before giving a result"},"job":0,"kind":"status","remaining":{"calls":99,"seconds":null},"state":"errored"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":1,"request":{"job":"0","kind":"status"}},"observed_ns":"<ns>","seq":5}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":6}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":7}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":8}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700", "700"]
            listed once it ended: []"#]],
    );
}

/// `sim`, as provider `there` of a run on two, beside `here`: its classes offer less memory than
/// the agent needs, and more than each judge does, so the agent is placed on `here`, and each
/// judge that needs no more memory than the others on `there`.
fn there(sim: Sim) -> Sim {
    sim.named("there").offering_memory(16 << 30)
}

// A run on two simulated providers, the agent on `here` and the judges on [`there`], the train
// judge, which needs the CPU model [`BRIEF`], in that class, whose agent and judges share
// an image, which each provider publishes itself, and each refuses a reference that the other
// published: the agent makes a judge call and a final submission, which the final judge scores,
// each sandbox is created in its role's class, and each provider is left with none of the run's
// sandboxes; the record says where each role is placed, and each provider's reach.
#[test]
fn spans_providers() {
    let argv = [
        "ops",
        "judge train 6",
        "await 0 completed",
        "finalize submit 6",
    ];
    let mut ir = ops_workflow(&argv, 30);
    ir["roles"][1]["needs"]["cpu_model"] = json!(BRIEF);
    ir["roles"][0]["image"] = ir["roles"][1]["image"].clone();
    let (here, there) = (Sim::new().named("here"), there(Sim::new()));
    let providers = [("here", &here), ("there", &there)];
    check(
        &System,
        "spans-providers",
        &providers,
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score Some(Number(6))
            record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/console.txt, jobs/0/log/collected, jobs/0/out/collected, jobs/0/out/result.json, jobs/0/sandbox.json, jobs/1/console.txt, jobs/1/log/collected, jobs/1/out/collected, jobs/1/out/result.json, jobs/1/sandbox.json, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"here","report":null}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"there","report":null}
            job 1's sandbox, jobs/1/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-2","provider":"there","report":null}
            payloads hold: ["xxxxxx"]
            provider.json is what each provider declared: true
            provider.json places the roles: {"agent":{"class":"sim","provider":"here"},"final_judge":{"class":"sim","provider":"there"},"train_judge":{"class":"brief","provider":"there"}}
            outcome.json: {"score":6,"state":"finished","verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":3}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":4}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"there","sandbox":"job-0"},"seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":6}
            {"actions":[{"kind":"reply","op":1,"reply":{"job":0,"kind":"status","remaining":{"calls":99,"seconds":null},"returned":{"raw_score":6},"scored":true,"state":"completed"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":1,"request":{"job":"0","kind":"status"}},"observed_ns":"<ns>","seq":7}
            {"actions":[{"kind":"reply","op":2,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":2,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":8}
            {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":1,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":9}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":10}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"here","sandbox":"agent"},"seq":11}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_started"},"observed_ns":"<ns>","seq":12}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":13}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"there","sandbox":"job-1"},"seq":14}
            {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":1,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":15}
            its last record came before the agent's teardown was to end
            socket directories left: false
            here published: ["sandboxd /sim/sandboxd", "image example/judge@sha256:2222222222222222222222222222222222222222222222222222222222222222"]
            here: their directory's mode at each create: ["700"]
            here: the class at each create: ["sim"]
            here listed once it ended: []
            there published: ["sandboxd /sim/sandboxd", "image example/judge@sha256:2222222222222222222222222222222222222222222222222222222222222222"]
            there: their directory's mode at each create: ["700", "700"]
            there: the class at each create: ["brief", "sim"]
            there listed once it ended: []"#]],
    );
}

// A run on two simulated providers, `here` and [`there`], whose agent's
// sandbox, on `here`, can't be deleted, and whose train judge's create, on `there`, fails,
// leaving a sandbox: `here`'s sweep fails, and `there`'s still deletes what it finds, so the run
// fails, naming `here`.
#[test]
fn sweeps_each_provider() {
    let mut ir = ops_workflow(&["ops", "judge train 6", "await 0 errored"], 30);
    ir["roles"][0]["image"] = json!(sim::image(UNDELETABLE).as_str());
    ir["roles"][1]["image"] = json!(sim::image(LOST).as_str());
    let (here, there) = (Sim::new().named("here"), there(Sim::new()));
    let providers = [("here", &here), ("there", &there)];
    check(
        &System,
        "sweeps-each-provider",
        &providers,
        &ir,
        SANDBOXD,
        &expect![[r#"
            failed: deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `here` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`
            record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"reason":"sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","state":"failed"},"id":"sim-1","provider":"here","report":null}
            payloads hold: ["xxxxxx"]
            provider.json is what each provider declared: true
            provider.json places the roles: {"agent":{"class":"sim","provider":"here"},"final_judge":{"class":"sim","provider":"there"},"train_judge":{"class":"sim","provider":"there"}}
            outcome.json: {"reason":"deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `here` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","state":"failed"}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"the judge's sandbox didn't start: the simulated create failed, and can't name what it made: expected the run's list to find it"},"observed_ns":"<ns>","seq":3}
            {"actions":[{"kind":"reply","op":1,"reply":{"failure":{"code":"judge_error","message":"the judge failed before giving a result"},"job":0,"kind":"status","remaining":{"calls":99,"seconds":null},"state":"errored"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":1,"request":{"job":"0","kind":"status"}},"observed_ns":"<ns>","seq":4}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":5}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":6}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"there","sandbox":null},"seq":7}
            {"at_ns":"<ns>","note":"the run failed: deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `here` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","seq":8}
            its last record came before the agent's teardown was to end
            socket directories left: false
            here published: ["sandboxd /sim/sandboxd", "image sha256:2f73696d2f756e64656c657461626c6500000000000000000000000000000000"]
            here: their directory's mode at each create: ["700"]
            here: the class at each create: ["sim"]
            here listed once it ended: ["sim-1"]
            there published: ["sandboxd /sim/sandboxd", "image sha256:2f73696d2f6c6f73740000000000000000000000000000000000000000000000", "image example/judge@sha256:2222222222222222222222222222222222222222222222222222222222222222"]
            there: their directory's mode at each create: ["700"]
            there: the class at each create: ["sim"]
            there listed once it ended: []"#]],
    );
}

// A run whose agent makes two judge calls, of judges placed on two providers, the second a
// second after the first: the near judge's, on `here`, as it needs what the agent does, whose
// `sandboxd` hangs without dialing; then the train judge's, on `there`, in class [`BRIEF`], whose
// model it needs, whose `sandboxd` ends at once without dialing. The run's watcher lists each
// provider where a start waits, and takes each list for that provider's starts alone: so the
// train judge's start is found ended on its own provider while the near judge's start still
// waits on `here`, and the near judge's start runs out at its class's startup. The agent, which
// awaits both calls' ends, sees each errored.
#[test]
fn watches_each_provider() {
    // The agent can't tell when a start is handed to the watcher, so a second's sleep orders the
    // two: `here`'s start waits as `there`'s begins.
    let argv = [
        "ops",
        "judge near 6",
        "sleep 1",
        "judge train 6",
        "await 0 errored",
        "await 1 errored",
    ];
    let mut ir = ops_workflow(&argv, 30);
    ir["roles"][1]["needs"]["cpu_model"] = json!(BRIEF);
    ir["roles"][1]["image"] = json!(sim::image(MUTE).as_str());
    let mut near = ir["roles"][1].clone();
    near["name"] = json!("near_judge");
    near["needs"] = ir["roles"][0]["needs"].clone();
    near["image"] = json!(sim::image(HUNG).as_str());
    ir["roles"].as_array_mut().unwrap().push(near);
    let grants = &mut ir["roles"][0]["grants"];
    let mut grant = grants[0].clone();
    grant["judge"]["name"] = json!("near");
    grant["judge"]["role"] = json!("near_judge");
    grants.as_array_mut().unwrap().push(grant);
    // A simulated sandbox that never dials waits on nothing but its start's deadline, so the near
    // judge's start is kept waiting by its class's startup, 5 s: past the end of the train
    // judge's start, a second later, at its class's 2 s. A watcher that listed `here` alone
    // while its start waits would leave the train judge's start to run out at its startup.
    let here = Sim::with_startup(5).named("here");
    let there = there(Sim::new());
    let providers = [("here", &here), ("there", &there)];
    check(
        &System,
        "watches-each-provider",
        &providers,
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/console.txt, jobs/0/sandbox.json, jobs/1/console.txt, jobs/1/sandbox.json, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"here","report":null}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-2","provider":"here","report":null}
            job 0's console, jobs/0/console.txt: "sandboxd: a simulated one, which never dials\n"
            job 1's sandbox, jobs/1/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"there","report":null}
            job 1's console, jobs/1/console.txt: "sandboxd: a simulated one, which never dials\n"
            payloads hold: ["xxxxxx"]
            provider.json is what each provider declared: true
            provider.json places the roles: {"agent":{"class":"sim","provider":"here"},"final_judge":{"class":"sim","provider":"there"},"near_judge":{"class":"sim","provider":"here"},"train_judge":{"class":"brief","provider":"there"}}
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"250000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"near","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"grant":"near","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":2}
            {"actions":[{"kind":"reply","op":1,"reply":{"job":1,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":1,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":1,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":3}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"there","sandbox":"job-1"},"seq":4}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_failed","reason":"the judge's sandbox didn't start: sandbox sim-1: it ended before it met the gateway: expected its `sandboxd` to run until it connects; its console ends: sandboxd: a simulated one, which never dials"},"observed_ns":"<ns>","seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"here","sandbox":"job-0"},"seq":6}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"the judge's sandbox didn't start: sandbox sim-2: no sandbox connected to <sockets>/job-0/s by the deadline: expected its sandbox to connect while the deadline lasts; its console ends: sandboxd: a simulated one, which never dials"},"observed_ns":"<ns>","seq":7}
            {"actions":[{"kind":"reply","op":2,"reply":{"failure":{"code":"judge_error","message":"the judge failed before giving a result"},"job":0,"kind":"status","remaining":{"calls":99,"seconds":null},"state":"errored"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":2,"request":{"job":"0","kind":"status"}},"observed_ns":"<ns>","seq":8}
            {"actions":[{"kind":"reply","op":3,"reply":{"failure":{"code":"judge_error","message":"the judge failed before giving a result"},"job":1,"kind":"status","remaining":{"calls":99,"seconds":null},"state":"errored"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":3,"request":{"job":"1","kind":"status"}},"observed_ns":"<ns>","seq":9}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":10}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":11}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"here","sandbox":"agent"},"seq":12}
            its last record came before the agent's teardown was to end
            socket directories left: false
            here published: ["sandboxd /sim/sandboxd", "image example/agent@sha256:1111111111111111111111111111111111111111111111111111111111111111", "image sha256:2f73696d2f68756e670000000000000000000000000000000000000000000000"]
            here: their directory's mode at each create: ["700", "700"]
            here: the class at each create: ["sim", "sim"]
            here listed once it ended: []
            there published: ["sandboxd /sim/sandboxd", "image sha256:2f73696d2f6d7574650000000000000000000000000000000000000000000000", "image example/judge@sha256:2222222222222222222222222222222222222222222222222222222222222222"]
            there: their directory's mode at each create: ["700"]
            there: the class at each create: ["brief"]
            there listed once it ended: []"#]],
    );
}

// A run whose train judge stalls its sandbox once its command has started, which the agent
// awaits before it finalizes: the work phase ends at once, so the core stops the call, whose
// sandbox's freeze runs out at the agent's teardown's end, which leaves the final judge its
// startup and timeout before the hard end. The final judge scores the submission, but its
// sandbox can't be deleted, which fails the run, and the run's end can't delete it either.
#[test]
fn judge_stalls() {
    let argv = [
        "ops",
        "judge train stall",
        "await 0 running",
        "finalize submit 6",
    ];
    // A deadline seconds after the agent's ops, so that the teardown's end, which follows it,
    // comes soon; and a call timeout seconds longer than the agent takes to finalize, so that
    // the stop ends the call, and seconds shorter than the time left when the call is made, so
    // that it is the call's timeout.
    let mut ir = short_workflow(&argv, 4);
    ir["roles"][0]["grants"][0]["judge"]["call_timeout_seconds"] = json!(2);
    ir["roles"][2]["image"] = json!(sim::image(UNDELETABLE).as_str());
    check(
        &System,
        "judge-stalls",
        &[(PROVIDER, &Sim::with_startup(SHORT_STARTUP))],
        &ir,
        SANDBOXD,
        &expect![[r#"
            failed: deleting the sandbox failed: sim-3 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `simulated` failed: sim-3 can't be deleted by the simulated provider: create one with another `sandboxd`
            record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/console.txt, jobs/0/sandbox.json, jobs/1/console.txt, jobs/1/log/collected, jobs/1/out/collected, jobs/1/out/result.json, jobs/1/sandbox.json, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, payloads/e5b6a7364e8a1049a9236dacfa24768ccffd3c4b671dc8b02953625844789b97, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-2","provider":"simulated","report":null}
            job 1's sandbox, jobs/1/sandbox.json: {"delete":{"reason":"sim-3 can't be deleted by the simulated provider: create one with another `sandboxd`","state":"failed"},"id":"sim-3","provider":"simulated","report":null}
            payloads hold: ["stall", "xxxxxx"]
            provider.json is what the provider declared: true
            outcome.json: {"reason":"deleting the sandbox failed: sim-3 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `simulated` failed: sim-3 can't be deleted by the simulated provider: create one with another `sandboxd`","state":"failed"}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"8000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:e5b6a7364e8a1049a9236dacfa24768ccffd3c4b671dc8b02953625844789b97","size":5},"timeout_ns":"2000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:e5b6a7364e8a1049a9236dacfa24768ccffd3c4b671dc8b02953625844789b97","size":5}}},"observed_ns":"<ns>","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":3}
            {"actions":[{"kind":"reply","op":1,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}},{"job":0,"kind":"stop_judge"},{"kind":"tear_down_agent"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":1,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":4}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":6}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":7}
            {"actions":[{"grant":null,"job":1,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"freezing the judge's sandbox failed: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts"},"observed_ns":"<ns>","seq":8}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_started"},"observed_ns":"<ns>","seq":9}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":10}
            {"at_ns":"<ns>","note":"the run failed: deleting the sandbox failed: sim-3 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `simulated` failed: sim-3 can't be deleted by the simulated provider: create one with another `sandboxd`","seq":11}
            its last record came once the agent's teardown was to end, before the hard end
            socket directories left: false
            their directory's mode at each create: ["700", "700", "700"]
            listed once it ended: ["sim-3"]"#]],
    );
}

#[test]
fn abandoned() {
    let argv = ["ops", "judge train slow", "await 0 running", "exit"];
    // A deadline later than the agent's ops, so that the agent's exit ends the work phase.
    let mut ir = short_workflow(&argv, 2);
    ir["roles"][1]["image"] = json!(sim::image(LINGERING).as_str());
    check(
        &System,
        "abandoned",
        &[(PROVIDER, &Sim::with_startup(SHORT_STARTUP))],
        &ir,
        SANDBOXD,
        &expect![[r#"
            failed: the run was still going at its hard end, 6 s after it started: expected it to end within its deadline, its finalize grace, its final judge's timeout and its sandboxes' startups
            record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/console.txt, jobs/0/log/collected, jobs/0/out/collected, jobs/0/sandbox.json, log.jsonl, outcome.json, payloads/5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-2","provider":"simulated","report":null}
            payloads hold: ["slow"]
            provider.json is what the provider declared: true
            outcome.json: {"reason":"the run was still going at its hard end, 6 s after it started: expected it to end within its deadline, its finalize grace, its final judge's timeout and its sandboxes' startups","state":"failed"}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"6000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4}}},"observed_ns":"<ns>","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":3}
            {"actions":[{"job":0,"kind":"stop_judge"},{"kind":"tear_down_agent"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":4}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":6}
            {"actions":[{"kind":"abandon"}],"at_ns":"<ns>","event":{"kind":"tick"},"observed_ns":"<ns>","seq":7}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":8}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"the call was stopped"},"observed_ns":"<ns>","seq":9}
            {"at_ns":"<ns>","note":"the run failed: the run was still going at its hard end, 6 s after it started: expected it to end within its deadline, its finalize grace, its final judge's timeout and its sandboxes' startups","seq":10}
            its last record came once the hard end had passed
            its log's last record was written once the hard end had passed
            socket directories left: false
            their directory's mode at each create: ["700", "700"]
            listed once it ended: []"#]],
    );
}

// The provider finishes the agent's delete only after the stopped call's, whose record reaches the
// loop only once the agent's teardown, which writes its own, is done.
#[test]
fn deleted_while_torn_down() {
    let argv = ["ops", "judge train slow", "await 0 running", "exit"];
    // A call timeout far longer than the agent takes to exit, so that the stop ends the call.
    let mut ir = short_workflow(&argv, 30);
    ir["roles"][0]["grants"][0]["judge"]["call_timeout_seconds"] = json!(20);
    ir["roles"][0]["image"] = json!(sim::image(LAST).as_str());
    check(
        &System,
        "deleted-while-torn-down",
        &[(PROVIDER, &Sim::with_startup(SHORT_STARTUP))],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/console.txt, jobs/0/log/collected, jobs/0/out/collected, jobs/0/sandbox.json, log.jsonl, outcome.json, payloads/5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-2","provider":"simulated","report":null}
            payloads hold: ["slow"]
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"34000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4},"timeout_ns":"20000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4}}},"observed_ns":"<ns>","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":3}
            {"actions":[{"job":0,"kind":"stop_judge"},{"kind":"tear_down_agent"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":4}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":6}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":7}
            {"actions":[{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"the call was stopped"},"observed_ns":"<ns>","seq":8}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700", "700"]
            listed once it ended: []"#]],
    );
}

// A run whose agent's judge call fails once its start runs out, since its judge hangs without
// dialing, and whose agent's sandbox's delete, as the agent exits, outlasts the run's hard
// end, so that the loop takes the call's last report only after it, and the judge call that
// the agent's sandbox makes after the exit, as a process the agent left would, which reaches
// the host before the report. Each came before the hard end, at the time it carries: the core
// refuses the op, whose refusal can't be sent, and the report ends the call in time, so the
// run finishes without a submission rather than be abandoned.
#[test]
fn reported_before_the_hard_end() {
    // A deadline later than the agent's ops, so that the agent's exit ends the work phase, and
    // bounds the judge's start.
    let argv = ["ops", "judge train 6", "exit", "judge train 6"];
    let mut ir = short_workflow(&argv, 2);
    ir["roles"][0]["image"] = json!(sim::image(LINGERING).as_str());
    ir["roles"][1]["image"] = json!(sim::image(HUNG).as_str());
    check(
        &System,
        "reported-before-the-hard-end",
        &[(PROVIDER, &Sim::with_startup(SHORT_STARTUP))],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/console.txt, jobs/0/sandbox.json, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-2","provider":"simulated","report":null}
            job 0's console, jobs/0/console.txt: "sandboxd: a simulated one, which never dials\n"
            payloads hold: ["xxxxxx"]
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"6000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":2}
            {"actions":[{"job":0,"kind":"stop_judge"},{"kind":"tear_down_agent"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":3}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":4}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":5}
            {"actions":[{"kind":"reply","op":1,"reply":{"kind":"refused","message":"the work phase has ended: make no more ops"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":1,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":6}
            {"at_ns":"<ns>","note":"op 1's reply wasn't sent: the agent's sandbox is gone","seq":7}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":8}
            {"actions":[{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"the judge's sandbox didn't start: sandbox sim-2: no sandbox connected to <sockets>/job-0/s by the deadline: expected its sandbox to connect while the deadline lasts; its console ends: sandboxd: a simulated one, which never dials"},"observed_ns":"<ns>","seq":9}
            its last record came once the hard end had passed
            its log's last record was written once the hard end had passed
            socket directories left: false
            their directory's mode at each create: ["700", "700"]
            listed once it ended: []"#]],
    );
}

// A run on a simulated host, whose agent makes a judge call as the run starts, and whose loop
// takes it 0.5 s past the deadline, then finds its inbox empty until the call's thread, which
// finds the call's start due by the deadline, reports the call's failure, at that time: the op
// reached the host before the deadline, so the core accepts it, and the failure, which reached
// it after, waits while the deadline's tick ends the work phase, then ends the call, which
// finishes the run.
#[test]
fn reported_past_the_deadline() {
    let late = LateTake {
        behind: 0,
        at: Duration::from_millis(30_500),
        then: 1,
    };
    check(
        &SimHost::new().late_take(late),
        "reported-past-the-deadline",
        &[(PROVIDER, &Sim::new())],
        &short_workflow(&["ops", "judge train 6"], 30),
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: ["xxxxxx"]
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"agent_started"},"observed_ns":"0","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"30500000000","event":{"kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":2}
            {"at_ns":"30500000000","note":"op 0's reply wasn't sent: it was due by the core's next act, which has come","seq":3}
            {"actions":[{"job":0,"kind":"stop_judge"},{"kind":"tear_down_agent"}],"at_ns":"30500000000","event":{"kind":"tick"},"observed_ns":"30500000000","seq":4}
            {"at_ns":"30500000000","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":5}
            {"actions":[],"at_ns":"30500000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":6}
            {"actions":[{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}}],"at_ns":"30500000000","event":{"job":0,"kind":"judge_failed","reason":"the judge's sandbox wasn't created: the call's start was due by a time that had passed"},"observed_ns":"30500000000","seq":7}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

// [`reported_past_the_deadline`]'s run, with its loop's take an hour in, past the run's hard
// end: the call's failure waits while the tick abandons the run, and is recorded after it.
#[test]
fn reported_past_the_hard_end() {
    let late = LateTake {
        behind: 0,
        at: Duration::from_secs(3600),
        then: 1,
    };
    check(
        &SimHost::new().late_take(late),
        "reported-past-the-hard-end",
        &[(PROVIDER, &Sim::new())],
        &short_workflow(&["ops", "judge train 6"], 30),
        SANDBOXD,
        &expect![[r#"
            failed: the run was still going at its hard end, 121 s after it started: expected it to end within its deadline, its finalize grace, its final judge's timeout and its sandboxes' startups
            record: agent/console.txt, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: ["xxxxxx"]
            provider.json is what the provider declared: true
            outcome.json: {"reason":"the run was still going at its hard end, 121 s after it started: expected it to end within its deadline, its finalize grace, its final judge's timeout and its sandboxes' startups","state":"failed"}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"agent_started"},"observed_ns":"0","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"3600000000000","event":{"kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":2}
            {"at_ns":"3600000000000","note":"op 0's reply wasn't sent: it was due by the core's next act, which has come","seq":3}
            {"actions":[{"kind":"abandon"}],"at_ns":"3600000000000","event":{"kind":"tick"},"observed_ns":"3600000000000","seq":4}
            {"actions":[],"at_ns":"3600000000000","event":{"job":0,"kind":"judge_failed","reason":"the judge's sandbox wasn't created: the call's start was due by a time that had passed"},"observed_ns":"3600000000000","seq":5}
            {"actions":[],"at_ns":"3600000000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":6}
            {"at_ns":"3600000000000","note":"the run failed: the run was still going at its hard end, 121 s after it started: expected it to end within its deadline, its finalize grace, its final judge's timeout and its sandboxes' startups","seq":7}
            its last record came once the hard end had passed
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

// A run whose agent finalizes a submission, and whose agent's sandbox's delete, in the teardown
// that follows, outlasts the run's hard end: the final judge's start is due by then, so its
// call fails with no sandbox created, once the hard end has passed, and the hard end's tick,
// which comes first, abandons the run.
#[test]
fn torn_down_past_the_hard_end() {
    check(
        &System,
        "torn-down-past-the-hard-end",
        &[(PROVIDER, &Sim::with_startup(SHORT_STARTUP))],
        &short_workflow(&["ops", "finalize submit 6"], 2),
        LINGERING,
        &expect![[r#"
            failed: the run was still going at its hard end, 6 s after it started: expected it to end within its deadline, its finalize grace, its final judge's timeout and its sandboxes' startups
            record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: ["xxxxxx"]
            provider.json is what the provider declared: true
            outcome.json: {"reason":"the run was still going at its hard end, 6 s after it started: expected it to end within its deadline, its finalize grace, its final judge's timeout and its sandboxes' startups","state":"failed"}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"6000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}},{"kind":"tear_down_agent"},{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":3}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":4}
            {"actions":[{"kind":"abandon"}],"at_ns":"<ns>","event":{"kind":"tick"},"observed_ns":"<ns>","seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"the judge's sandbox wasn't created: the call's start was due by a time that had passed"},"observed_ns":"<ns>","seq":6}
            {"at_ns":"<ns>","note":"the run failed: the run was still going at its hard end, 6 s after it started: expected it to end within its deadline, its finalize grace, its final judge's timeout and its sandboxes' startups","seq":7}
            its last record came once the hard end had passed
            its log's last record was written once the hard end had passed
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

// A run whose agent waits 2 s once its first `ops` is answered before it makes its second,
// timed as it reached the host, so that its reply leaves at least 2 s less than the first's,
// however long the agent took to start, which the agent checks, exiting with 1 if not. It then
// exits, and its sandbox makes a judge call after the exit, as a process the agent left would:
// the call arrives once the core has finished the run, so the core refuses it, and the refusal
// can't be sent, which the run log notes.
#[test]
fn late_ops() {
    let argv = [
        "ops",
        "ops",
        "sleep 2",
        "ops at least 2 less",
        "exit",
        "judge train 6",
    ];
    check(
        &System,
        "late-ops",
        &[(PROVIDER, &Sim::new())],
        &ops_workflow(&argv, 30),
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":"<s>","resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"kind":"ops"}},"observed_ns":"<ns>","seq":2}
            {"actions":[{"kind":"reply","op":1,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":"<s>","resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":1,"request":{"kind":"ops"}},"observed_ns":"<ns>","seq":3}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":4}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":6}
            {"actions":[{"kind":"reply","op":2,"reply":{"kind":"refused","message":"the work phase has ended: make no more ops"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":2,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":7}
            {"at_ns":"<ns>","note":"op 2's reply wasn't sent: the agent's sandbox is gone","seq":8}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

// A run on a simulated host, whose agent makes a judge call, then at once ends its session by
// closing the connection, both as the run starts, and whose loop takes the op 0.5 s past the
// deadline, as a slow sync of the run log would hold it, with the session's end queued behind
// it: each reached the host before the deadline, and counts then, however late the loop takes
// it, so the core accepts the op, whose reply isn't sent, as it was due by the deadline, and the
// session's end ends the work phase as the agent's failure, ahead of the call's failure, which
// its thread reports as the call's start was due by then, and which then finishes the run.
//
// The test is deterministic because one thread produces its inputs, the session's reader, so
// they reach the inbox in a fixed order; the order between producers on different threads, such
// as a judge's report and the agent's reader, isn't scripted yet.
#[test]
fn taken_past_the_deadline() {
    let late = LateTake {
        behind: 1,
        at: Duration::from_millis(30_500),
        then: 0,
    };
    check(
        &SimHost::new().late_take(late),
        "taken-past-the-deadline",
        &[(PROVIDER, &Sim::new())],
        &short_workflow(&["ops", "judge train 6", "close"], 30),
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/console.txt, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: ["xxxxxx"]
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"agent_started"},"observed_ns":"0","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"30500000000","event":{"kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":2}
            {"at_ns":"30500000000","note":"op 0's reply wasn't sent: it was due by the core's next act, which has come","seq":3}
            {"actions":[{"job":0,"kind":"stop_judge"},{"kind":"tear_down_agent"}],"at_ns":"30500000000","event":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"},"observed_ns":"0","seq":4}
            {"at_ns":"30500000000","note":"freezing the agent's sandbox failed: the sandbox ended the session","seq":5}
            {"at_ns":"30500000000","note":"the agent's out is incomplete: collecting /pumpkin/out failed: the sandbox ended the session","seq":6}
            {"at_ns":"30500000000","note":"the agent's log is incomplete: collecting /pumpkin/log failed: the sandbox ended the session","seq":7}
            {"at_ns":"30500000000","note":"collected from the agent's sandbox: out, nothing; log, nothing","seq":8}
            {"actions":[],"at_ns":"30500000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":9}
            {"actions":[{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}}],"at_ns":"30500000000","event":{"job":0,"kind":"judge_failed","reason":"the judge's sandbox wasn't created: the call's start was due by a time that had passed"},"observed_ns":"30500000000","seq":10}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

// A run on a simulated host, whose agent makes an `ops`, then at once ends its session by
// closing the connection, both as the run starts, and whose loop takes the op at once, but
// whose sync of the op's record lasts until the session's end is queued behind it, and ends
// 0.5 s past the deadline: the reply is due by the deadline, so it isn't sent; and the
// session's end, which reached the host at 0, is decided at 0, ahead of the core's tick, so
// the agent fails.
#[test]
fn held_by_a_slow_sync() {
    let slow = SlowSync {
        record: 2,
        queued: 1,
        at: Duration::from_millis(30_500),
    };
    check(
        &SimHost::new().slow_sync(slow),
        "held-by-a-slow-sync",
        &[(PROVIDER, &Sim::new())],
        &short_workflow(&["ops", "ops", "close"], 30),
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/console.txt, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"agent_started"},"observed_ns":"0","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":30,"resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"0","event":{"kind":"op_arrived","op":0,"request":{"kind":"ops"}},"observed_ns":"0","seq":2}
            {"at_ns":"30500000000","note":"op 0's reply wasn't sent: it was due by the core's next act, which has come","seq":3}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}}],"at_ns":"30500000000","event":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"},"observed_ns":"0","seq":4}
            {"at_ns":"30500000000","note":"freezing the agent's sandbox failed: the sandbox ended the session","seq":5}
            {"at_ns":"30500000000","note":"the agent's out is incomplete: collecting /pumpkin/out failed: the sandbox ended the session","seq":6}
            {"at_ns":"30500000000","note":"the agent's log is incomplete: collecting /pumpkin/log failed: the sandbox ended the session","seq":7}
            {"at_ns":"30500000000","note":"collected from the agent's sandbox: out, nothing; log, nothing","seq":8}
            {"actions":[],"at_ns":"30500000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":9}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

// A run on a simulated host whose agent starts 25 s in, within its startup of 30 s, as a slow
// sync of the run's first record holds its start back, and whose loop takes the agent's `ops`
// 55 s in: the deadline of 30 s counts from the agent's start, so the reply leaves the agent
// the whole 30 s, though it isn't sent, as it was due by the deadline, 55 s in, whose tick
// then ends the work phase. Held to 35 s, past its startup, which counts from the run's start,
// the start fails, rather than leaving the agent less than its deadline.
#[test]
fn started_late() {
    let slow = SlowSync {
        record: 0,
        queued: 0,
        at: Duration::from_secs(25),
    };
    let late = LateTake {
        behind: 0,
        at: Duration::from_secs(55),
        then: 0,
    };
    check(
        &SimHost::new().slow_sync(slow).late_take(late),
        "started-late",
        &[(PROVIDER, &Sim::new())],
        &short_workflow(&["ops", "ops"], 30),
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"25000000000","event":{"kind":"agent_started"},"observed_ns":"25000000000","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":30,"resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"55000000000","event":{"kind":"op_arrived","op":0,"request":{"kind":"ops"}},"observed_ns":"25000000000","seq":2}
            {"at_ns":"55000000000","note":"op 0's reply wasn't sent: it was due by the core's next act, which has come","seq":3}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}}],"at_ns":"55000000000","event":{"kind":"tick"},"observed_ns":"55000000000","seq":4}
            {"at_ns":"55000000000","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":5}
            {"actions":[],"at_ns":"55000000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":6}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
    let slow = SlowSync {
        record: 0,
        queued: 0,
        at: Duration::from_secs(35),
    };
    check(
        &SimHost::new().slow_sync(slow),
        "started-past-the-startup",
        &[(PROVIDER, &Sim::new())],
        &short_workflow(&["ops", "ops"], 30),
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: the simulated create's deadline had passed: expected time left to make it"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: the simulated create's deadline had passed: expected time left to make it"}}}],"at_ns":"35000000000","event":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: the simulated create's deadline had passed: expected time left to make it"},"observed_ns":"35000000000","seq":1}
            {"at_ns":"35000000000","note":"the agent's sandbox never started: nothing was collected","seq":2}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: []
            listed once it ended: []"#]],
    );
}

// A run on a simulated host whose sync of the record of the agent's first op fails: the run
// fails with the sync's error as its reason, and sends no reply; the run's end deletes the
// agent's sandbox, which the run log notes, with why the run failed.
#[test]
fn sync_fails() {
    check(
        &SimHost::new().failed_sync(2),
        "sync-fails",
        &[(PROVIDER, &Sim::new())],
        &ops_workflow(&["ops", "ops", "exit"], 30),
        SANDBOXD,
        &expect![[r#"
            failed: <record>/log.jsonl: the simulated host's sync failed
            record: agent/console.txt, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"reason":"<record>/log.jsonl: the simulated host's sync failed","state":"failed"}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"agent_started"},"observed_ns":"0","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":30,"resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"0","event":{"kind":"op_arrived","op":0,"request":{"kind":"ops"}},"observed_ns":"0","seq":2}
            {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":3}
            {"at_ns":"0","note":"the run failed: <record>/log.jsonl: the simulated host's sync failed","seq":4}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

// A run on a simulated host whose agent makes an `ops`, then a judge call whose payload is over
// the agent's limit, which the gateway refuses, and whose sync of the refused op's record ends
// 0.5 s past the deadline: the refusal, due by then, comes only after that sync, so it isn't
// sent, and the core's tick ends the work phase at its deadline.
#[test]
fn refusal_held_by_a_slow_sync() {
    let slow = SlowSync {
        record: 3,
        queued: 0,
        at: Duration::from_millis(30_500),
    };
    check(
        &SimHost::new().slow_sync(slow),
        "refusal-held-by-a-slow-sync",
        &[(PROVIDER, &Sim::new())],
        &short_workflow(&["ops", "ops", "judge train 17", "exit"], 30),
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"agent_started"},"observed_ns":"0","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":30,"resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"0","event":{"kind":"op_arrived","op":0,"request":{"kind":"ops"}},"observed_ns":"0","seq":2}
            {"actions":[],"at_ns":"0","event":{"kind":"op_refused","message":"the payload is 17 bytes, more than the limit of 16: send at most 16","request":{"grant":"train","kind":"judge","payload":{"over":16,"size":17}}},"seq":3}
            {"at_ns":"30500000000","note":"the refusal of an op whose payload was over the agent's limit wasn't sent: it was due by the core's next act, which has come","seq":4}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}}],"at_ns":"30500000000","event":{"kind":"tick"},"observed_ns":"30500000000","seq":5}
            {"at_ns":"30500000000","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":6}
            {"actions":[],"at_ns":"30500000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":7}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

// A run on a simulated host whose journal shows each sync of its record in order with what the
// simulated provider sees of the acts that follow: its agent makes an `ops`, then a judge call
// whose payload is over the agent's limit, which the gateway refuses, then exits. Each record of
// the run log that leads to an act is synced before it, with every record before it: the run's
// start before the agent's create, the `ops`'s arrival before its reply reaches the agent's
// session, the refused op's record before its refusal does, and the agent's exit before its
// sandbox's delete. The agent's start and the note of what its teardown collected lead to no act,
// and are synced with the next record that does, or by the run's end, before `outcome.json`.
#[test]
fn syncs() {
    let journal = Journal::default();
    check(
        &SimHost::new().journaled(&journal),
        "syncs",
        &[(PROVIDER, &Sim::new().journaled(&journal))],
        &ops_workflow(&["ops", "ops", "judge train 17", "exit"], 30),
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"agent_started"},"observed_ns":"0","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":30,"resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"0","event":{"kind":"op_arrived","op":0,"request":{"kind":"ops"}},"observed_ns":"0","seq":2}
            {"actions":[],"at_ns":"0","event":{"kind":"op_refused","message":"the payload is 17 bytes, more than the limit of 16: send at most 16","request":{"grant":"train","kind":"judge","payload":{"over":16,"size":17}}},"seq":3}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"0","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":4}
            {"at_ns":"0","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":5}
            {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":6}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            the journal:
              fsync <partial>/.record.json.partial
              fsync <partial>
              fsync <partial>/driver.pid
              fsync <partial>
              fsync <runs>
              fsync <record>/.ir.json.partial
              fsync <record>
              fsync <record>/.provider.json.partial
              fsync <record>
              fsync <record>
              fsync <record>
              fdatasync <record>/log.jsonl, through record 0
              created sim-1
              fsync <record>/agent/.sandbox.json.partial
              fsync <record>/agent
              fdatasync <record>/log.jsonl, through record 2
              a session read op 0's reply: ops
              fdatasync <record>/log.jsonl, through record 3
              a session read op 1's reply: refused
              fdatasync <record>/log.jsonl, through record 4
              fsync <record>/agent/.console.txt.partial
              fsync <record>/agent
              deleted sim-1
              fsync <record>/agent/.sandbox.json.partial
              fsync <record>/agent
              fdatasync <record>/log.jsonl, through record 6
              fsync <record>/.outcome.json.partial
              fsync <record>
            listed once it ended: []"#]],
    );
}

// A run whose agent makes model requests through its route, each once the one before it is
// answered, which the run log says goes to the stand-in, each model under the name it takes:
// one goes upstream, its model named as the upstream takes it, with the upstream's key, and its
// stream reaches the agent, naming the model that answered as the agent named it; one that
// names a model the route doesn't allow is refused; and once the agent has finalized a
// submission, one goes upstream too, as the route stays open through the grace after the
// finalize, until the agent's exit ends the work phase, and so does one whose key the provider
// refuses, with a 401, which the run log keeps while the run is scored as usual, since the
// finalize had locked its selection. The run log records how each was answered, before the
// agent goes on.
#[test]
fn model_requests() {
    let argv = [
        "ops",
        "model m-large stream",
        "model m-huge stream",
        "finalize submit 6",
        "model m-large usage",
        "model m-large expired",
    ];
    check(
        &System,
        "model-requests",
        &[(PROVIDER, &Sim::new())],
        &routed(ops_workflow(&argv, 30)),
        SANDBOXD,
        &expect![[r#"
            finished, with score Some(Number(6))
            record: agent/console.txt, agent/log/collected, agent/log/command.stdout, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/console.txt, jobs/0/log/collected, jobs/0/out/collected, jobs/0/out/result.json, jobs/0/sandbox.json, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-2","provider":"simulated","report":null}
            payloads hold: ["xxxxxx"]
            provider.json is what the provider declared: true
            outcome.json: {"score":6,"state":"finished","verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"at_ns":"<ns>","note":"the agent's model route goes to the upstream `stand-in`, which takes `m-large` as `vendor/m-large`","seq":1}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"model request 0 goes upstream","seq":3}
            {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"kind":"model_answered","request":0,"upstream_status":200},"seq":4}
            {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"barred","message":"the request names `m-huge`, which the route doesn't allow: name `m-large`","status":400},"kind":"model_answered","request":1,"upstream_status":null},"seq":5}
            {"actions":[{"kind":"reply","op":0,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":6}
            {"at_ns":"<ns>","note":"model request 2 goes upstream","seq":7}
            {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"kind":"model_answered","request":2,"upstream_status":200},"seq":8}
            {"at_ns":"<ns>","note":"model request 3 goes upstream","seq":9}
            {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"refused","message":"the model provider refused the route's key, with a 401, which ends the model route: make no more model requests","status":400},"kind":"model_answered","request":3,"upstream_status":401},"seq":10}
            {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":11}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 2 files of 1188 bytes","seq":12}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":13}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":14}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":15}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":16}
            {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":17}
            its last record came before the agent's teardown was to end
            the agent's standard output:
            model request 0: 200 [("content-type", "text/event-stream"), ("openai-model", "m-large")] data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
            model request 1: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the request names `m-huge`, which the route doesn't allow: name `m-large`","param":null,"type":"invalid_request_error"}}
            model request 2: 200 [("content-type", "text/event-stream"), ("openai-model", "m-large")] data: {"type":"response.created","response":{"usage":null}}\n\nevent: response.completed\ndata: {"type":"response.completed","response":{"usage":{"input_tokens":12,"input_tokens_details":{"cached_tokens":8},"output_tokens":5,"output_tokens_details":{"reasoning_tokens":3}}}}\n\n
            model request 3: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider refused the route's key, with a 401, which ends the model route: make no more model requests","param":null,"type":"invalid_request_error"}}
            socket directories left: false
            their directory's mode at each create: ["700", "700"]
            the model endpoint at each create: [true, false]
            listed once it ended: []"#]],
    );
}

#[test]
fn model_upstream_refusals() {
    let argv = [
        "ops",
        "model m-large forbidden",
        "model m-large stream",
        "model m-large expired",
        "model m-large stream",
        "finalize submit 6",
    ];
    check(
        &System,
        "model-upstream-refusals",
        &[(PROVIDER, &Sim::new())],
        &routed(ops_workflow(&argv, 30)),
        SANDBOXD,
        &expect![[r#"
            failed: the upstream `stand-in` refused its key, with a 401, which ended the agent's model route: give it a key that it takes, and run again
            record: agent/console.txt, agent/log/collected, agent/log/command.stdout, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/console.txt, jobs/0/log/collected, jobs/0/out/collected, jobs/0/out/result.json, jobs/0/sandbox.json, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-2","provider":"simulated","report":null}
            payloads hold: ["xxxxxx"]
            provider.json is what the provider declared: true
            outcome.json: {"reason":"the upstream `stand-in` refused its key, with a 401, which ended the agent's model route: give it a key that it takes, and run again","state":"failed"}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"at_ns":"<ns>","note":"the agent's model route goes to the upstream `stand-in`, which takes `m-large` as `vendor/m-large`","seq":1}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"model request 0 goes upstream","seq":3}
            {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"refused","message":"the model provider refused the request, with a 403: send only requests that it allows","status":400},"kind":"model_answered","request":0,"upstream_status":403},"seq":4}
            {"at_ns":"<ns>","note":"model request 1 goes upstream","seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"kind":"model_answered","request":1,"upstream_status":200},"seq":6}
            {"at_ns":"<ns>","note":"model request 2 goes upstream","seq":7}
            {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"refused","message":"the model provider refused the route's key, with a 401, which ends the model route: make no more model requests","status":400},"kind":"model_answered","request":2,"upstream_status":401},"seq":8}
            {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"barred","message":"the model provider refused the route's key, with a 401, which ends the model route: make no more model requests","status":400},"kind":"model_answered","request":3,"upstream_status":null},"seq":9}
            {"actions":[{"kind":"reply","op":0,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":10}
            {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":11}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 2 files of 1083 bytes","seq":12}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":13}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":14}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":15}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":16}
            {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":17}
            {"at_ns":"<ns>","note":"the run failed: the upstream `stand-in` refused its key, with a 401, which ended the agent's model route: give it a key that it takes, and run again","seq":18}
            its last record came before the agent's teardown was to end
            the agent's standard output:
            model request 0: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider refused the request, with a 403: send only requests that it allows","param":null,"type":"invalid_request_error"}}
            model request 1: 200 [("content-type", "text/event-stream"), ("openai-model", "m-large")] data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
            model request 2: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider refused the route's key, with a 401, which ends the model route: make no more model requests","param":null,"type":"invalid_request_error"}}
            model request 3: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider refused the route's key, with a 401, which ends the model route: make no more model requests","param":null,"type":"invalid_request_error"}}
            socket directories left: false
            their directory's mode at each create: ["700", "700"]
            the model endpoint at each create: [true, false]
            listed once it ended: []"#]],
    );
}

#[test]
fn model_provider_down() {
    let argv = [
        "ops",
        "model m-large stream",
        "model m-large down",
        "model m-large down",
        "model m-huge stream",
        "ask m-large stall",
        "exit",
    ];
    check(
        &System,
        "model-provider-down",
        &[(PROVIDER, &Sim::new())],
        &routed(ops_workflow(&argv, 30)),
        SANDBOXD,
        &expect![[r#"
            failed: the upstream `stand-in` failed the agent's last model request, 2 (502; failing since request 1), and the agent exited: a deployment fault, not the agent's result; run it again
            record: agent/console.txt, agent/log/collected, agent/log/command.stdout, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"reason":"the upstream `stand-in` failed the agent's last model request, 2 (502; failing since request 1), and the agent exited: a deployment fault, not the agent's result; run it again","state":"failed"}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"at_ns":"<ns>","note":"the agent's model route goes to the upstream `stand-in`, which takes `m-large` as `vendor/m-large`","seq":1}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"model request 0 goes upstream","seq":3}
            {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"kind":"model_answered","request":0,"upstream_status":200},"seq":4}
            {"at_ns":"<ns>","note":"model request 1 goes upstream","seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"kind":"model_answered","request":1,"request_id":"req-502","upstream_status":502},"seq":6}
            {"at_ns":"<ns>","note":"model request 2 goes upstream","seq":7}
            {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"kind":"model_answered","request":2,"request_id":"req-502","upstream_status":502},"seq":8}
            {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"barred","message":"the request names `m-huge`, which the route doesn't allow: name `m-large`","status":400},"kind":"model_answered","request":3,"upstream_status":null},"seq":9}
            {"at_ns":"<ns>","note":"model request 4 goes upstream","seq":10}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":11}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 2 files of 867 bytes","seq":12}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":13}
            {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"cut"},"kind":"model_answered","request":4,"upstream_status":200},"seq":14}
            {"at_ns":"<ns>","note":"the run failed: the upstream `stand-in` failed the agent's last model request, 2 (502; failing since request 1), and the agent exited: a deployment fault, not the agent's result; run it again","seq":15}
            its last record came before the agent's teardown was to end
            the agent's standard output:
            model request 0: 200 [("content-type", "text/event-stream"), ("openai-model", "m-large")] data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
            model request 1: 502 [("content-type", "text/plain"), ("x-request-id", "req-502")] refused
            model request 2: 502 [("content-type", "text/plain"), ("x-request-id", "req-502")] refused
            model request 3: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the request names `m-huge`, which the route doesn't allow: name `m-large`","param":null,"type":"invalid_request_error"}}
            model request 4: 200 [("content-type", "text/event-stream"), ("openai-model", "m-large")] event: response.created\n\n
            socket directories left: false
            their directory's mode at each create: ["700"]
            the model endpoint at each create: [true]
            listed once it ended: []
            a stalled stream's client hung up before the agent's teardown was to end"#]],
    );
}

#[test]
fn model_barred_is_no_failure() {
    let argv = ["ops", "model m-large stream", "model m-huge stream", "exit"];
    check(
        &System,
        "model-barred",
        &[(PROVIDER, &Sim::new())],
        &routed(ops_workflow(&argv, 30)),
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/console.txt, agent/log/collected, agent/log/command.stdout, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"at_ns":"<ns>","note":"the agent's model route goes to the upstream `stand-in`, which takes `m-large` as `vendor/m-large`","seq":1}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"model request 0 goes upstream","seq":3}
            {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"kind":"model_answered","request":0,"upstream_status":200},"seq":4}
            {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"barred","message":"the request names `m-huge`, which the route doesn't allow: name `m-large`","status":400},"kind":"model_answered","request":1,"upstream_status":null},"seq":5}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":6}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 2 files of 567 bytes","seq":7}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":8}
            its last record came before the agent's teardown was to end
            the agent's standard output:
            model request 0: 200 [("content-type", "text/event-stream"), ("openai-model", "m-large")] data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
            model request 1: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the request names `m-huge`, which the route doesn't allow: name `m-large`","param":null,"type":"invalid_request_error"}}
            socket directories left: false
            their directory's mode at each create: ["700"]
            the model endpoint at each create: [true]
            listed once it ended: []"#]],
    );
}

#[test]
fn model_provider_recovers() {
    let argv = ["ops", "model m-large down", "model m-large stream"];
    check(
        &System,
        "model-provider-recovers",
        &[(PROVIDER, &Sim::new())],
        &routed(ops_workflow(&argv, 30)),
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/console.txt, agent/log/collected, agent/log/command.stdout, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"at_ns":"<ns>","note":"the agent's model route goes to the upstream `stand-in`, which takes `m-large` as `vendor/m-large`","seq":1}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"model request 0 goes upstream","seq":3}
            {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"kind":"model_answered","request":0,"request_id":"req-502","upstream_status":502},"seq":4}
            {"at_ns":"<ns>","note":"model request 1 goes upstream","seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"kind":"model_answered","request":1,"upstream_status":200},"seq":6}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":7}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 2 files of 444 bytes","seq":8}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":9}
            its last record came before the agent's teardown was to end
            the agent's standard output:
            model request 0: 502 [("content-type", "text/plain"), ("x-request-id", "req-502")] refused
            model request 1: 200 [("content-type", "text/event-stream"), ("openai-model", "m-large")] data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
            socket directories left: false
            their directory's mode at each create: ["700"]
            the model endpoint at each create: [true]
            listed once it ended: []"#]],
    );
}

#[test]
fn model_provider_down_at_the_deadline() {
    let argv = [
        "ops",
        "submit submit 6",
        "model m-large down",
        "model m-large stall",
    ];
    check(
        &System,
        "model-provider-down-at-the-deadline",
        &[(PROVIDER, &Sim::new())],
        &routed(short_workflow(&argv, 2)),
        SANDBOXD,
        &expect![[r#"
            finished, with score Some(Number(6))
            record: agent/console.txt, agent/log/collected, agent/log/command.stdout, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/console.txt, jobs/0/log/collected, jobs/0/out/collected, jobs/0/out/result.json, jobs/0/sandbox.json, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-2","provider":"simulated","report":null}
            payloads hold: ["xxxxxx"]
            provider.json is what the provider declared: true
            outcome.json: {"score":6,"state":"finished","verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":false,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"deadline"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"93000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"at_ns":"<ns>","note":"the agent's model route goes to the upstream `stand-in`, which takes `m-large` as `vendor/m-large`","seq":1}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":2}
            {"actions":[{"kind":"reply","op":0,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"stored"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"finalize":false,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":3}
            {"at_ns":"<ns>","note":"model request 0 goes upstream","seq":4}
            {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"kind":"model_answered","request":0,"request_id":"req-502","upstream_status":502},"seq":5}
            {"at_ns":"<ns>","note":"model request 1 goes upstream","seq":6}
            {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"tick"},"observed_ns":"<ns>","seq":7}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 2 files of 217 bytes","seq":8}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":9}
            {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"cut"},"kind":"model_answered","request":1,"upstream_status":200},"seq":10}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":11}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":12}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":13}
            {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":false,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"deadline"}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":14}
            its last record came before the agent's teardown was to end
            the agent's standard output:
            model request 0: 502 [("content-type", "text/plain"), ("x-request-id", "req-502")] refused
            model request 1: 200 [("content-type", "text/event-stream"), ("openai-model", "m-large")] event: response.created\n\n
            socket directories left: false
            their directory's mode at each create: ["700", "700"]
            the model endpoint at each create: [true, false]
            listed once it ended: []
            a stalled stream's client hung up before the agent's teardown was to end"#]],
    );
}

#[test]
fn model_stream_cut() {
    let argv = ["ops", "submit submit slow", "model m-large stall"];
    check(
        &System,
        "model-stream-cut",
        &[(PROVIDER, &Sim::new())],
        &routed(short_workflow(&argv, 2)),
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/console.txt, agent/log/collected, agent/log/command.stdout, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/console.txt, jobs/0/log/collected, jobs/0/out/collected, jobs/0/sandbox.json, log.jsonl, outcome.json, payloads/5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-2","provider":"simulated","report":null}
            payloads hold: ["slow"]
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"failure":{"kind":"timeout"},"kind":"no_result","submission":{"finalized":false,"payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4}}},"work_end":{"kind":"deadline"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"93000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"at_ns":"<ns>","note":"the agent's model route goes to the upstream `stand-in`, which takes `m-large` as `vendor/m-large`","seq":1}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":2}
            {"actions":[{"kind":"reply","op":0,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"stored"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"finalize":false,"grant":"submit","kind":"submit","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4}}},"observed_ns":"<ns>","seq":3}
            {"at_ns":"<ns>","note":"model request 0 goes upstream","seq":4}
            {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"tick"},"observed_ns":"<ns>","seq":5}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 2 files of 126 bytes","seq":6}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":7}
            {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"cut"},"kind":"model_answered","request":0,"upstream_status":200},"seq":8}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":9}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_timed_out"},"observed_ns":"<ns>","seq":10}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":11}
            {"actions":[{"kind":"finish","outcome":{"score":null,"verdict":{"failure":{"kind":"timeout"},"kind":"no_result","submission":{"finalized":false,"payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4}}},"work_end":{"kind":"deadline"}}}],"at_ns":"<ns>","event":{"invalid":"the judge wrote no result: write it at /pumpkin/out/result.json","job":0,"kind":"judge_collected"},"observed_ns":"<ns>","seq":12}
            the final judge's standard error, as `wait` finds it: Ok("jobs/0/log/command.stderr")
            its last record came before the agent's teardown was to end
            the agent's standard output:
            model request 0: 200 [("content-type", "text/event-stream"), ("openai-model", "m-large")] event: response.created\n\n
            socket directories left: false
            their directory's mode at each create: ["700", "700"]
            the model endpoint at each create: [true, false]
            listed once it ended: []
            a stalled stream's client hung up before the agent's teardown was to end"#]],
    );
}

#[test]
fn model_grace_cut() {
    let argv = ["ops", "ask m-large stall", "finalize submit 6"];
    let mut ir = routed(ops_workflow(&argv, 30));
    // So that the grace runs out while the agent still waits on the stalled stream.
    ir["roles"][0]["runtime"]["agent"]["finalize_grace_seconds"] = json!(1);
    check(
        &System,
        "model-grace-cut",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score Some(Number(6))
            record: agent/console.txt, agent/log/collected, agent/log/command.stdout, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/console.txt, jobs/0/log/collected, jobs/0/out/collected, jobs/0/out/result.json, jobs/0/sandbox.json, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-1","provider":"simulated","report":null}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"sim-2","provider":"simulated","report":null}
            payloads hold: ["xxxxxx"]
            provider.json is what the provider declared: true
            outcome.json: {"score":6,"state":"finished","verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"finalized"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"241000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"at_ns":"<ns>","note":"the agent's model route goes to the upstream `stand-in`, which takes `m-large` as `vendor/m-large`","seq":1}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"model request 0 goes upstream","seq":3}
            {"actions":[{"kind":"reply","op":0,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":4}
            {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"tick"},"observed_ns":"<ns>","seq":5}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 2 files of 126 bytes","seq":6}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":7}
            {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"cut"},"kind":"model_answered","request":0,"upstream_status":200},"seq":8}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":9}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":10}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":11}
            {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"finalized"}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":12}
            its last record came before the agent's teardown was to end
            the agent's standard output:
            model request 0: 200 [("content-type", "text/event-stream"), ("openai-model", "m-large")] event: response.created\n\n
            socket directories left: false
            their directory's mode at each create: ["700", "700"]
            the model endpoint at each create: [true, false]
            listed once it ended: []
            a stalled stream's client hung up before the agent's teardown was to end"#]],
    );
}

/// Nanoseconds since the Unix epoch.
fn unix_ns() -> u64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    u64::try_from(since.as_nanos()).unwrap()
}

/// A time or duration of the log's, a decimal string of nanoseconds, as a number.
fn nanos(value: &Value) -> Option<u64> {
    value.as_str()?.parse().ok()
}

// A run holds what its provider reserved for it until its end, interrupted or not: while one run
// holds all the provider's CPUs, another is refused before its record; once the first has
// ended, as a signal ends it before it publishes, the other is prepared, and once that one has
// finished, a third.
#[test]
fn holds_until_its_end() {
    let runs = Runs::new("holds-until-its-end");
    let sim = Sim::new().with_cpus(16);
    let ir: ir::Workflow = serde_json::from_value(workflow(&["exit", "0"])).unwrap();
    let prepare = |name: &str| {
        let placed = placed(&ir, sim.ready()).unwrap();
        let dir = runs.dir.join(name);
        let on = providers_of(&placed, |_| &sim);
        let recorded = Recorded { args: &[] };
        let id = new_id(SystemTime::now()).unwrap();
        Run::prepare(&System, placed, on, &dir, id, None, recorded)
    };
    let prepared = |run: &Result<Run<'_, System>, String>| match run {
        Ok(_) => "prepared".to_owned(),
        Err(error) => format!("refused: {error}"),
    };
    let execute = |run: Run<'_, System>| {
        let executed = run.execute(Path::new(SANDBOXD), || Ok(()));
        match executed {
            Ok(outcome) => format!("finished, with score {:?}", outcome.score()),
            Err(stopped) => stopped.to_string(),
        }
    };
    let first = prepare("first").unwrap();
    let mut lines = vec![format!(
        "a second, beside it: {}",
        prepared(&prepare("second"))
    )];
    lines.push(format!("the first: {}", execute(first)));
    let second = prepare("second");
    lines.push(format!("then the second: {}", prepared(&second)));
    // A signal interrupts every later run of the process.
    interrupt();
    lines.push(format!("the second: {}", execute(second.unwrap())));
    lines.push(format!("then a third: {}", prepared(&prepare("third"))));
    expect![[r#"
        a second, beside it: refused: reserving on provider `simulated`: the run needs 16 simulated CPUs, and 16 of the 16 are held: run it once others end
        the first: finished, with score None
        then the second: prepared
        the second: was interrupted by SIGTERM
        then a third: prepared"#]].assert_eq(&lines.join("\n"));
}
