//! Executing a prepared run through the library's entry point, [`Run::execute`], on the
//! simulated provider ([`Sim`]), and on the machine's host ([`System`]) or a simulated one
//! ([`SimHost`]). The agent's model route, where it has one, reaches a stand-in model provider
//! ([`StandIn`]).

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fmt::Write as _;
use std::io::{self, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{fs, iter};

use decision::lasts;
use expect_test::{Expect, expect};
use gateway::{Dialer, GatewayAddress, GrantListener};
use provider::{
    Console, Create, Creator, ImageRef, Listed, Meeting, Provider, Ready, Reservation, Role,
    RunTag, SandboxId, SandboxdRef, Sweep,
};
use route::Serving;
use runtime::{Ending, Host, Record, Recorded, Run, System, new_id};
use schema::ir::{self, Image, ModelName};
use schema::sandbox::HELLO_TIMEOUT;
use schema::workflow::{ByRole, Placed, Workflow};
use serde_json::{Value, json};
use stdx::Deadline;
use test_support::http::{read_request, write_chunk};
use test_support::{WORKFLOW, files, sandboxes, standard_outputs};

use crate::host::{LateTake, SimHost, SlowSync, Synced};
use crate::run::{PROVIDER, Runs, compact_outcome, placed, placed_on, providers_of, serving};
use crate::sim::{
    self, BRIEF, DAWDLING, HUNG, Journal, LAST, LATER, LOST, MUTE, REFUSED_ONCE, SLOW, Sim,
    UNDELETABLE, UNLISTABLE, UNPUBLISHED, UNSHOWN, VANISHING,
};

/// A `sandboxd` that dials.
const SANDBOXD: &str = "/sim/sandboxd";

/// The startup of a simulated provider whose tests wait for a start, or a teardown, to run out,
/// in seconds.
// Short, so that the wait takes seconds, but far longer than a simulated sandbox takes to start,
// so that a busy machine makes a start that succeeds wait longer, never fail.
const SHORT_STARTUP: u64 = 1;

/// The file that a sync of the agent's `sandbox.json` writes, and what it holds once it records
/// the sandbox's delete.
const AGENT_SANDBOX: &str = "agent/.sandbox.json.partial";
const DELETED: &str = r#""state": "deleted""#;

/// A time past the hard end, 6 s, of the runs of [`short_workflow`] with a deadline of 2 s on a
/// provider whose startup is [`SHORT_STARTUP`].
const PAST_THE_HARD_END: Duration = Duration::from_millis(6_500);

/// The seconds that what an interrupted run waits on lasts, a start's startup or a work phase's
/// deadline.
// Far past the signal's latency, yet short, so that a run that misses the signal fails its case
// within a minute or so, rather than holding `ci` up.
const WAITED: u64 = 60;

/// Where an [`Interrupting`] provider notes SIGTERM: as it publishes `sandboxd`, the first publish;
/// as it publishes an image; once it has created the sandbox of a name; as it first deletes a
/// sandbox; or, from a thread of the test's own, once the run log records that the agent started
/// ([`interrupt_once_started`]). At [`At::SwapPayloads`] it notes nothing: as it deletes a sandbox,
/// it overwrites each payload in the record instead.
#[derive(Clone, Copy, PartialEq, Eq)]
enum At {
    PublishSandboxd,
    PublishImage,
    Created(&'static str),
    Delete,
    AgentStarted,
    SwapPayloads,
}

/// What [`At::SwapPayloads`] overwrites each payload with: a size of its own, within the score
/// slot's 16 bytes.
const SWAPPED: &str = "yyyyyyyyy";

/// The simulated provider `sim`, which [interrupts](interrupt) the test's process at the step
/// `at` names, or, at [`At::SwapPayloads`], swaps the payloads in `record`, the run's.
struct Interrupting<'a> {
    sim: &'a Sim,
    at: Option<At>,
    record: &'a Path,
}

impl Interrupting<'_> {
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
        if self.at == Some(At::SwapPayloads) {
            let payloads = fs::read_dir(self.record.join("payloads")).unwrap();
            for payload in payloads {
                fs::write(payload.unwrap().path(), SWAPPED).unwrap();
            }
        }
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
            Meeting::Private(_) => Meeting::Private(self),
            Meeting::Listen(_) => Meeting::Listen(self),
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

fn named<'s, P>(providers: &'s [(&'static str, P)], name: &ir::Name) -> &'s P {
    let found = (providers.iter()).find(|(named, _)| name.as_str() == *named);
    &found.unwrap().1
}

/// Each of `providers`, by its name, [`Interrupting`] `at` a step, or swapping the payloads in
/// `record`, the run's.
fn interrupting<'a>(
    providers: &[(&'static str, &'a Sim)],
    at: Option<At>,
    record: &'a Path,
) -> Vec<(&'static str, Interrupting<'a>)> {
    (providers.iter())
        .map(|&(name, sim)| (name, Interrupting { sim, at, record }))
        .collect()
}

/// Executes `run` with `sandboxd`, a grant listener on the loopback if one of `providers` is
/// private, and a dialer that trusts the stand-in TLS edge, interrupted `at` a step, if given, by
/// the [`Interrupting`] providers it was prepared on: what `execute` returns, and for an
/// interrupted run, whether it started, whether it returned within [`WAITED`], and what it
/// published.
fn interrupted_at(
    run: Run<'_, impl Host>,
    providers: &[(&'static str, &Sim)],
    sandboxd: &str,
    at: Option<At>,
) -> Vec<String> {
    let record = run.dir().to_owned();
    let private = (providers.iter())
        .any(|(_, provider)| matches!(provider.meeting(), Meeting::Private(_)))
        .then(|| {
            let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
            let address = GatewayAddress::try_from(loopback).unwrap();
            GrantListener::bind(&address, HELLO_TIMEOUT).unwrap()
        });
    let waited = Deadline::after(Duration::from_secs(WAITED));
    let mut ran = false;
    let executed = thread::scope(|scope| {
        if at == Some(At::AgentStarted) {
            scope.spawn(|| interrupt_once_started(&record));
        }
        run.execute(
            Path::new(sandboxd),
            private.as_ref(),
            &Dialer::new(test_support::edge::client()),
            || {
                ran = true;
                Ok(())
            },
        )
    });
    let mut lines = vec![match executed {
        Ok(outcome) => format!("finished, with score {:?}", outcome.score()),
        Err(stopped) => stopped.to_string(),
    }];
    if at.is_some_and(|at| at != At::SwapPayloads) {
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

/// What `sim`'s platform shows of a run that the provider's calls don't: whether each create
/// asked for the model endpoint, if one did, and its journal, if it has one.
fn shown(sim: &Sim) -> Vec<String> {
    let mut lines = Vec::new();
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

/// The build tag of the agent's image alone, so that a record shows a role with one and one
/// without.
fn agent_tag(placed: &Placed) -> BTreeMap<ir::Image, String> {
    let workflow = placed.workflow();
    let image = workflow.sandbox(workflow.agent().role).image.clone();
    BTreeMap::from([(image, "pumpkin/agent:0123".to_owned())])
}

/// Prepares `ir` as test `name`, its agent's model route served by a [`StandIn`] if it has one, on
/// `host`, executes it on `providers`, where its roles are placed ([`placed_on`]), with
/// `sandboxd`, and checks what becomes of it ([`executed`]).
fn check(
    host: &impl Shown,
    name: &str,
    providers: &[(&'static str, &Sim)],
    ir: &Value,
    sandboxd: &str,
    expect: &Expect,
) {
    expect.assert_eq(&executed(host, name, providers, ir, sandboxd, &[]));
}

/// What a test of a run shows of it, or does to it, beyond what [`executed`] shows of each run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Also {
    /// A SIGTERM interrupts the run at a step, as each provider is [`Interrupting`]; it shows too
    /// whether the run started, whether it returned within [`WAITED`], and what it published.
    Interrupted(At),
    /// The record's files.
    Listing,
    /// The usage that `outcome.json` records.
    Usage,
    /// Each model exchange that the record keeps, as kept ([`kept_exchanges`]).
    Exchanges,
    /// What rebuilding model request 0 gives once the first item it names is changed, and what
    /// reading it gives once the log names its files outside the record.
    ChangedItem,
}

fn interruption(also: &[Also]) -> Option<At> {
    also.iter().find_map(|also| match also {
        Also::Interrupted(at) => Some(*at),
        Also::Listing | Also::Usage | Also::Exchanges | Also::ChangedItem => None,
    })
}

/// What [`check`] checks of `ir`'s run, as test `name`, on `host`, with `sandboxd`, on
/// `providers`, and what it shows `also`. It fails unless what holds of every run holds of this
/// one ([`held`], [`kept_exchanges`], [`left`], [`folds_as_whole`]), and its socket directories
/// are gone. It shows
/// what `execute` returned, the log ([`logged`]), when its last record came ([`when`]), what the
/// sandboxes printed, the stand-in's hang-ups, and what departs from the normal run. It shows the
/// socket directories, the stand-in's address, the record, as made and once made, and the runs
/// directory as `<sockets>`, `<stand-in>`, `<partial>`, `<record>` and `<runs>`.
fn executed<H: Shown>(
    host: &H,
    name: &str,
    providers: &[(&'static str, &Sim)],
    ir: &Value,
    sandboxd: &str,
    also: &[Also],
) -> String {
    let at = interruption(also);
    let runs = Runs::new(name);
    let ir: ir::Workflow = serde_json::from_value(ir.clone()).unwrap();
    let routed = ir.roles.iter().any(|role| role.egress.model.is_some());
    let stand_in = routed.then(StandIn::start);
    let ready = (providers.iter()).map(|(name, provider)| (*name, provider.ready()));
    let ready: BTreeMap<&str, &Ready> = ready.collect();
    let placed = placed_on(&ir, &ready).unwrap();
    let servings = StandIn::servings(stand_in.as_ref(), placed.workflow(), &runs.dir);
    let hard_end = lasts(&placed);
    let id = new_id(SystemTime::now()).unwrap();
    let record = runs.dir.join(id.as_str());
    let interrupting = interrupting(providers, at, &record);
    let on = providers_of(&placed, |name| named(&interrupting, name));
    let tags = agent_tag(&placed);
    let recorded = Recorded {
        args: &[],
        tags: &tags,
    };
    let run = Run::prepare(host, placed.clone(), on, &runs.dir, id, &servings, recorded);
    let run = run.unwrap();
    let (record, tag) = (run.dir().to_owned(), RunTag::try_from(run.id().to_owned()));
    let sockets = record.join("sockets");
    let partial = record.with_file_name(format!(".{}.partial", run.id()));
    let before = unix_ns();
    let mut lines = interrupted_at(run, providers, sandboxd, at);
    let started = before..=unix_ns();
    let log = read_log(&record);
    let held = held(name, &record, &log, providers, also, &lines[0]);
    lines.extend(held);
    lines.push("log.jsonl:".to_owned());
    let (log, start, last) = logged(log, &started, H::SCRIPTED);
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
    let address = stand_in.as_ref().map(|stand_in| stand_in.address.clone());
    let (received, hung_ups) = stand_in.map(StandIn::stop).unwrap_or_default();
    let exchanges = kept_exchanges(name, &record, received);
    folds_as_whole(name, &record);
    if also.contains(&Also::Exchanges) {
        lines.extend(exchanges);
    }
    lines.extend((also.contains(&Also::ChangedItem)).then(|| changed_item(&record)));
    assert!(!sockets.exists(), "{name}: its socket directories are left");
    lines.extend(left(name, providers, &tag.unwrap()));
    for hung_up in hung_ups {
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

/// What [`executed`] shows of the files in `record`, a run's, as test `name`, beside its log,
/// `log`: the listing and the usage if the test shows them `also`, each `sandbox.json` that isn't
/// of a deleted sandbox ([`undeleted`]), the payloads kept, the roles' model routes, and of a run
/// on more than one of `providers`, where it placed each role. It fails unless `provider.json`
/// holds what each declared and the routes ([`declared`]), and `outcome.json` what `returned`,
/// the line that shows what `execute` returned, shows ([`outcome_holds`], [`usage_holds`]).
fn held(
    name: &str,
    record: &Path,
    log: &[Value],
    providers: &[(&str, &Sim)],
    also: &[Also],
    returned: &str,
) -> Vec<String> {
    let mut lines = Vec::new();
    if also.contains(&Also::Listing) {
        lines.push(format!("record: {}", files(record).join(", ")));
    }
    lines.extend(undeleted(name, record, log));
    let payloads = fs::read_dir(record.join("payloads")).into_iter().flatten();
    let mut payloads: Vec<String> = payloads
        .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect();
    payloads.sort();
    if !payloads.is_empty() {
        lines.push(format!("payloads hold: {payloads:?}"));
    }
    lines.extend(declared(name, record, providers));
    let outcome = fs::read_to_string(record.join("outcome.json")).unwrap();
    let usage = outcome_holds(name, returned, &outcome, log);
    let shows = also.contains(&Also::Usage);
    usage_holds(name, record, log, &usage, providers, shows);
    if shows {
        let usage = compact_outcome(&json!({ "usage": usage }).to_string());
        let usage: Value = serde_json::from_str(&usage).unwrap();
        lines.push(format!("outcome.json's usage: {}", usage["usage"]));
    }
    lines
}

/// What `record`, a run's, keeps of each model exchange, by its number: the request, as kept, and
/// the answer's body, each line break shown as `\n`; and how many conversation items it keeps,
/// each once. It fails, as test `name`, unless the requests that the log says went upstream
/// rebuild ([`Record::model_request`]) to the bodies that the stand-in `received`, byte for byte.
fn kept_exchanges(name: &str, record: &Path, mut received: Vec<Vec<u8>>) -> Vec<String> {
    let dir = record.join("model");
    let kept: Vec<String> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    let mut numbers: Vec<u64> = (kept.iter())
        .filter_map(|name| name.split_once('.')?.0.parse().ok())
        .collect();
    numbers.sort_unstable();
    numbers.dedup();
    let opened = Record::open(record).unwrap();
    let upstream = (read_log(record).into_iter())
        .filter(|logged| logged["event"]["kind"] == "model_request_started")
        .map(|logged| logged["event"]["request"].as_u64().unwrap());
    let mut rebuilt: Vec<Vec<u8>> = upstream
        .map(|number| opened.model_request(number).unwrap())
        .collect();
    rebuilt.sort();
    received.sort();
    assert!(
        rebuilt == received,
        "{name}: the requests that went upstream rebuild to {:?}, but the stand-in received {:?}",
        rebuilt
            .iter()
            .map(|body| String::from_utf8_lossy(body))
            .collect::<Vec<_>>(),
        received
            .iter()
            .map(|body| String::from_utf8_lossy(body))
            .collect::<Vec<_>>(),
    );
    let shown = |bytes: &[u8]| String::from_utf8_lossy(bytes).replace('\n', "\\n");
    let read = |number: u64, part: &str| {
        shown(&fs::read(dir.join(format!("{number}.{part}"))).unwrap_or_default())
    };
    let mut lines: Vec<String> = (numbers.into_iter())
        .map(|number| {
            let (request, response) = (read(number, "request"), read(number, "response"));
            format!("model exchange {number} kept: request {request}; response {response}")
        })
        .collect();
    if let Ok(items) = fs::read_dir(dir.join("items")) {
        lines.push(format!("model items kept: {}", items.count()));
    }
    lines
}

/// Why model request 0 of `record`, a run's, doesn't rebuild once the first item it names is
/// changed.
fn changed_item(record: &Path) -> String {
    let request = fs::read(record.join("model/0.request")).unwrap();
    let request: Value = serde_json::from_slice(&request).unwrap();
    let item = request["input"][0]
        .as_str()
        .unwrap()
        .strip_prefix("sha256:");
    fs::write(
        record.join("model/items").join(item.unwrap()),
        r#""changed""#,
    )
    .unwrap();
    let refused = Record::open(record).unwrap().model_request(0).unwrap_err();
    let mut said = format!("request 0 rebuilt once an item is changed: {refused}");
    // A log that names kept files outside the record, as damage would.
    let log = fs::read_to_string(record.join("log.jsonl")).unwrap();
    let log = (log.replace(r#""path":"model/0.request""#, r#""path":"/etc/hostname""#)).replace(
        r#""path":"model/0.response""#,
        r#""path":"model/../../0.response""#,
    );
    fs::write(record.join("log.jsonl"), &log).unwrap();
    let opened = Record::open(record).unwrap();
    let kept = (log.lines())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|line| line["event"]["kind"] == "model_answered")
        .map(|line| line["event"]["kept"].clone())
        .unwrap();
    let refused = [
        opened.model_request(0).unwrap_err(),
        opened.kept_request(&kept["request"]).unwrap_err(),
        opened.model_response(0, &kept["response"]).unwrap_err(),
        opened.kept_request(&json!("model/0.request")).unwrap_err(),
    ];
    for refused in refused {
        let _ = write!(said, "\nrequest 0 read from a damaged log: {refused}");
    }
    said
}

/// The records of the run log in `record`, a run's, each as JSON; none for a run that failed
/// before it started, which has no log.
fn read_log(record: &Path) -> Vec<Value> {
    let log = fs::read_to_string(record.join("log.jsonl")).unwrap_or_default();
    (log.lines())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// Each `sandbox.json` in `record`, a run's, that isn't of a sandbox deleted with no report, and
/// each console that a sandbox printed ([`sandboxes`]). It fails, as test `name`, unless `log`,
/// the run's, records the delete of each sandbox that it leaves out, by its id and provider.
fn undeleted(name: &str, record: &Path, log: &[Value]) -> Vec<String> {
    let deletes: Vec<&Value> = (log.iter())
        .filter(|logged| logged["event"]["kind"] == "sandbox_deleted")
        .map(|logged| &logged["event"])
        .collect();
    let normal = |line: &str| {
        let (whose, sandbox) = line.split_once("/sandbox.json: ")?;
        let sandbox: Value = serde_json::from_str(sandbox).unwrap();
        let gone = sandbox["delete"] == json!({ "state": "deleted" });
        (gone && sandbox["report"].is_null()).then(|| {
            let (_, dir) = whose.rsplit_once(", ").unwrap();
            let named = match dir.split_once('/') {
                Some(("jobs", number)) => format!("job-{number}"),
                Some(("leases", number)) => format!("lease-{number}"),
                _ => dir.to_owned(),
            };
            let delete = json!({
                "id": sandbox["id"],
                "kind": "sandbox_deleted",
                "provider": sandbox["provider"],
                "sandbox": named,
            });
            assert!(
                deletes.contains(&&delete),
                "{name}: the log records no delete of {dir}'s sandbox: {sandbox}"
            );
        })
    };
    let lines = sandboxes(record).into_iter();
    lines.filter(|line| normal(line).is_none()).collect()
}

/// The usage that `outcome`, a run's `outcome.json`, records. It fails, as test `name`, unless
/// the rest is what `returned`, the line that shows what `execute` returned, shows, and for a
/// run that finished, what `log`, its log, records that it finished with.
fn outcome_holds(name: &str, returned: &str, outcome: &str, log: &[Value]) -> Value {
    let mut outcome: Value = serde_json::from_str(outcome).unwrap();
    let usage = outcome.as_object_mut().unwrap().remove("usage");
    let finished = (log.iter())
        .flat_map(|logged| logged["actions"].as_array().into_iter().flatten())
        .find(|action| action["kind"] == "finish");
    let expected = if let Some(reason) = returned.strip_prefix("failed: ") {
        json!({ "reason": reason, "state": "failed" })
    } else if let Some(signal) = returned.strip_prefix("was interrupted by ") {
        json!({ "signal": signal, "state": "interrupted" })
    } else {
        let mut expected = finished.unwrap()["outcome"].clone();
        expected["state"] = "finished".into();
        let score = Some(&expected["score"]).filter(|score| !score.is_null());
        let shown = format!("finished, with score {score:?}");
        assert_eq!(returned, shown, "{name}: `execute` returned another score");
        expected
    };
    assert_eq!(
        outcome, expected,
        "{name}: outcome.json holds another outcome"
    );
    usage.unwrap_or_default()
}

/// Fails, as test `name`, unless `usage`, what `outcome.json` in `record`, a run's, records,
/// counts its sandboxes as [`sandboxes_hold`] says, given `log`, the run's, and `providers`, and
/// each kept model request; and, unless the test `shows` it, records no tokens and no call's
/// usage.
fn usage_holds(
    name: &str,
    record: &Path,
    log: &[Value],
    usage: &Value,
    providers: &[(&str, &Sim)],
    shows: bool,
) {
    if usage.is_null() {
        // A run that never started records no usage.
        return;
    }
    sandboxes_hold(name, record, log, usage, providers);
    let kept = fs::read_dir(record.join("model")).into_iter().flatten();
    let kept = kept.filter(|entry| {
        let name = entry.as_ref().unwrap().file_name();
        name.to_str().unwrap().ends_with(".request")
    });
    let requests = usage["model_requests"].as_u64().unwrap();
    assert_eq!(
        requests,
        kept.count() as u64,
        "{name}: usage {usage} counts other requests"
    );
    if !shows {
        let none = json!({ "input": 0, "output": 0, "responses": 0 });
        assert_eq!(
            usage["tokens"], none,
            "{name}: usage {usage} shows no tokens"
        );
        assert!(
            usage.get("calls").is_none(),
            "{name}: usage {usage} shows no calls"
        );
    }
}

/// Fails, as test `name`, unless `usage`, what `outcome.json` in `record`, a run's, records,
/// counts each sandbox start that `log`, the run's, records, and of each of `providers` at least
/// its creates of each class; each sandbox whose `sandbox.json` records no delete, by its
/// provider; and times each class's leased use if the run leased.
fn sandboxes_hold(
    name: &str,
    record: &Path,
    log: &[Value],
    usage: &Value,
    providers: &[(&str, &Sim)],
) {
    let counted = usage["sandboxes"].as_array().unwrap();
    let leased = record.join("leases").exists();
    let (mut creates, mut undeleted) = (BTreeMap::new(), BTreeMap::new());
    for counts in counted {
        let provider = counts["provider"].as_str().unwrap().to_owned();
        let class = counts["class"].as_str().unwrap().to_owned();
        creates.insert((provider.clone(), class), counts["count"].as_u64().unwrap());
        *undeleted.entry(provider).or_insert(0) += counts["undeleted"].as_u64().unwrap_or(0);
        let timed = ["busy_ns", "charged_ns"].map(|time| counts.get(time).is_some());
        assert_eq!(
            timed, [leased; 2],
            "{name}: usage {counts} times leased use"
        );
    }
    let starts = (log.iter())
        .flat_map(|logged| logged["actions"].as_array().into_iter().flatten())
        .filter(|action| action["kind"].as_str().unwrap().starts_with("start_"));
    let started = starts.count() as u64;
    assert_eq!(
        creates.values().sum::<u64>(),
        started,
        "{name}: usage {usage} counts other starts"
    );
    let (mut made, mut left) = (BTreeMap::new(), BTreeMap::new());
    for (provider, sim) in providers {
        left.insert((*provider).to_owned(), 0);
        for class in sim.classes() {
            let key = ((*provider).to_owned(), class.as_str().to_owned());
            *made.entry(key).or_insert(0) += 1;
        }
    }
    for line in sandboxes(record) {
        let Some((_, sandbox)) = line.split_once("/sandbox.json: ") else {
            continue;
        };
        let sandbox: Value = serde_json::from_str(sandbox).unwrap();
        if sandbox["delete"]["state"] != "deleted" {
            *left.get_mut(sandbox["provider"].as_str().unwrap()).unwrap() += 1;
        }
    }
    for (key, count) in made {
        let counted = creates.get(&key).copied().unwrap_or(0);
        assert!(
            counted >= count,
            "{name}: usage {usage} counts fewer than {key:?}'s {count} creates"
        );
    }
    assert_eq!(
        undeleted, left,
        "{name}: usage {usage} counts other sandboxes undeleted"
    );
}

/// Whether `provider.json` in `record`, a run's, holds what each of `providers` declared, by its
/// name, with how its sandboxes meet the gateway, and holds `routes`, which fails test `name` if
/// not; the routes, if a role has one; and, of a run on more than one, where it places each role.
fn declared(name: &str, record: &Path, providers: &[(&str, &Sim)]) -> Vec<String> {
    let recorded = fs::read_to_string(record.join("provider.json")).unwrap();
    let recorded: Value = serde_json::from_str(&recorded).unwrap();
    let reach = |provider: &Sim| match provider.meeting() {
        Meeting::Local(_) => "local",
        Meeting::Private(_) => "private",
        Meeting::Listen(_) => "listen",
    };
    let expected = (providers.iter()).map(|(name, provider)| {
        let mut ready = json!(provider.ready());
        ready["reach"] = reach(provider).into();
        ((*name).to_owned(), ready)
    });
    let expected = Value::Object(expected.collect());
    assert_eq!(
        recorded["providers"], expected,
        "{name}: provider.json holds what no provider declared"
    );
    let routes = recorded["routes"].as_object();
    let routes = routes.unwrap_or_else(|| panic!("{name}: provider.json holds no routes"));
    let mut lines = Vec::new();
    if !routes.is_empty() {
        lines.push(format!("provider.json routes: {}", recorded["routes"]));
    }
    // A run on one provider places each role on it.
    if providers.len() > 1 {
        lines.push(format!(
            "provider.json places the roles: {}",
            recorded["roles"]
        ));
    }
    lines
}

/// What each of `providers` shows of run `tag` once it has ended: its journal ([`shown`]), and
/// what its list finds, if anything; and, of a run on more than one, by each one's name, what it
/// published, and the class of each create, too. It fails test `name` unless each socket
/// directory's directory was `700` at each create.
fn left(name: &str, providers: &[(&str, &Sim)], tag: &RunTag) -> Vec<String> {
    let listed = |provider: &Sim| {
        let listed = provider.list(tag).unwrap();
        let listed = listed.into_iter().map(|Listed { id, ended: _ }| id);
        listed.map(|id| id.as_str().to_owned()).collect::<Vec<_>>()
    };
    for (_, provider) in providers {
        let modes = provider.modes();
        assert!(
            modes.iter().all(|mode| *mode == 0o700),
            "{name}: the socket directories' directory had modes {modes:?} at its creates"
        );
    }
    if let [(_, provider)] = providers {
        let mut lines = shown(provider);
        let listed = listed(provider);
        if !listed.is_empty() {
            lines.push(format!("listed once it ended: {listed:?}"));
        }
        return lines;
    }
    let mut lines = Vec::new();
    for (name, provider) in providers {
        lines.push(format!("{name} published: {:?}", provider.published()));
        lines.extend(shown(provider).iter().map(|line| format!("{name}: {line}")));
        let classes: Vec<String> = provider.classes().into_iter().map(String::from).collect();
        lines.push(format!("{name}: the class at each create: {classes:?}"));
        let listed = listed(provider);
        if !listed.is_empty() {
            lines.push(format!("{name} listed once it ended: {listed:?}"));
        }
    }
    lines
}

/// A stand-in for a model provider, on the loopback, which answers a request whose input is
/// `stream` with a whole stream that echoes the request's line, its `authorization` and its
/// body, one whose input is a number with that number as its whole body, one whose input is
/// `stall` with a stream's head and first part, then nothing, until its client hangs up, one
/// whose input is `expired` with a 401, as a provider refuses an expired key, one whose input is
/// `forbidden` with a 403, one whose input is `down` with a 502 that names its id for the
/// request, and one whose input is `silent` with none. Each stream's head names in `openai-model`
/// the model that the request's body named, as a provider does that answers with it.
struct StandIn {
    address: String,
    stops: Sender<()>,
    /// Returns the body of each request it received, and when the client of each stream it
    /// stalled hung up, in nanoseconds since the Unix epoch.
    thread: JoinHandle<(Vec<Vec<u8>>, Vec<u64>)>,
}

impl StandIn {
    fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (stops, stopped) = mpsc::channel();
        let thread = thread::spawn(move || {
            // Each connection is answered on a thread of its own, so that a stream it stalls
            // holds no other request.
            let mut answering = Vec::new();
            for connection in listener.incoming() {
                if stopped.try_recv().is_ok() {
                    break;
                }
                let connection = connection.unwrap();
                answering.push(thread::spawn(move || answer(connection)));
            }
            let answered = answering
                .into_iter()
                .map(|answering| answering.join().unwrap());
            let (received, hung_up): (Vec<_>, Vec<_>) = answered.unzip();
            (received, hung_up.into_iter().flatten().collect())
        });
        Self {
            address,
            stops,
            thread,
        }
    }

    /// How a run serves each role's route in `workflow` through `stand_in`, which is there if a
    /// role has one, with its key in a file in `dir`.
    fn servings(stand_in: Option<&Self>, workflow: &Workflow, dir: &Path) -> ByRole<Vec<Serving>> {
        let servings = workflow.by_role(|(_, sandbox)| {
            let entries = sandbox
                .model_route
                .iter()
                .flat_map(ir::ModelRoute::as_slice);
            let served = entries.map(|entry| stand_in.unwrap().serving(dir, entry));
            Ok::<_, Infallible>(served.collect())
        });
        let Ok(servings) = servings;
        servings
    }

    /// How a run serves `entry`, a route's, through it, with its key in a file in `dir`.
    fn serving(&self, dir: &Path, entry: &ir::ApiRoute) -> Serving {
        let models = entry.models.as_slice().iter().map(ModelName::as_str);
        let models: Vec<&str> = models.collect();
        let path = match entry.api {
            ir::Api::Responses => "responses",
            ir::Api::Chat => "chat/completions",
        };
        let url = format!("http://{}/v1/{path}", self.address);
        Serving {
            api: entry.api,
            ..serving(dir, &url, &models)
        }
    }

    /// Stops it, and returns what its thread does: the body of each request it received, and
    /// when the client of each stream it stalled hung up.
    fn stop(self) -> (Vec<Vec<u8>>, Vec<u64>) {
        self.stops.send(()).unwrap();
        drop(TcpStream::connect(&self.address).unwrap());
        self.thread.join().unwrap()
    }
}

/// Answers the request on `connection` as [`StandIn`] says, and returns its body, and when its
/// client hung up, if the stand-in stalled its stream.
fn answer(mut connection: TcpStream) -> (Vec<u8>, Option<u64>) {
    let (line, headers, body) = read_request(&mut BufReader::new(&connection));
    let received = body.clone();
    let key = headers.iter().find(|(name, _)| name == "authorization");
    let input: Value = serde_json::from_slice(&body).unwrap();
    let conversation = input.get("input").unwrap_or(&input["messages"]);
    let said = (conversation.as_str()).or_else(|| conversation[1]["content"].as_str());
    let refused = match said {
        Some("silent") => return (received, None),
        Some("expired") => Some(("401 Unauthorized", "")),
        Some("forbidden") => Some(("403 Forbidden", "")),
        Some("down") => Some(("502 Bad Gateway", "x-request-id: req-502\r\n")),
        _ => None,
    };
    if let Some(number) = said.filter(|text| text.parse::<u64>().is_ok()) {
        let answer = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n{number}",
            number.len()
        );
        connection.write_all(answer.as_bytes()).unwrap();
        return (received, None);
    }
    if let Some((status, id)) = refused {
        let refused = format!(
            "HTTP/1.1 {status}\r\ncontent-type: text/plain\r\n{id}content-length: 7\r\n\
             connection: close\r\n\r\nrefused"
        );
        connection.write_all(refused.as_bytes()).unwrap();
        return (received, None);
    }
    let streams = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nopenai-model: {}\r\n\
         transfer-encoding: chunked\r\nconnection: close\r\n\r\n",
        input["model"].as_str().unwrap()
    );
    connection.write_all(streams.as_bytes()).unwrap();
    let text = String::from_utf8(body).unwrap();
    let echo = format!("data: {line} {key:?} {text}\n\n");
    if said == Some("usage") {
        let completed = r#"{"type":"response.completed","response":{"usage":{"input_tokens":12,"input_tokens_details":{"cached_tokens":8},"output_tokens":5,"output_tokens_details":{"reasoning_tokens":3}}}}"#;
        write_chunk(
            &mut connection,
            "data: {\"type\":\"response.created\",\"response\":{\"usage\":null}}\n\n\
             data: {\"type\":\"response.output_text.delta\",\"delta\":\"6\"}\n\n",
        );
        write_chunk(
            &mut connection,
            &format!("event: response.completed\ndata: {completed}\n\n"),
        );
        write_chunk(&mut connection, "");
        return (received, None);
    }
    if said == Some("limited") {
        // As a provider answered a rate limit, with a 200.
        write_chunk(
            &mut connection,
            "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"Rate limit reached\"},\"usage\":null}}\n\n\
             event: error\ndata: {\"type\":\"error\",\"code\":\"rate_limit_exceeded\",\"message\":\"Rate limit reached\",\"param\":null}\n\n",
        );
        write_chunk(&mut connection, "");
        return (received, None);
    }
    if said == Some("stream") {
        write_chunk(&mut connection, &echo);
        write_chunk(&mut connection, "");
        return (received, None);
    }
    write_chunk(&mut connection, "event: response.created\n\n");
    // What the client sends is read until it hangs up, as its end shows that it has.
    let _ = io::copy(&mut connection, &mut io::sink());
    (received, Some(unix_ns()))
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

/// The records of `log`, a run's ([`read_log`]), one per line, once [`in_order`] has checked
/// them; when the run started, in nanoseconds since the Unix epoch; and its last record's time.
/// It leaves out the `status` ops answered, alone, while their job or lease was queued, starting,
/// running or closing, or while the lease's transfer ran, whose number depends on timing, and
/// numbers the rest, and their ops, in turn. The run's start is shown only if it isn't within
/// `started`, the call to `execute`. Unless the run's times are `scripted`, a record's times are
/// shown only if 0, and the time left in a reply as `<s>`; a run of more than 64 `x`s, which a
/// command of the simulated provider's prints, as `<n x>`.
fn logged(
    log: Vec<Value>,
    started: &RangeInclusive<u64>,
    scripted: bool,
) -> (Vec<String>, Option<u64>, Option<Duration>) {
    let (mut records, mut start, mut polls) = (Vec::new(), None, 0);
    let last = in_order(&log);
    for mut logged in log {
        let actions = logged["actions"].as_array().map_or(0, Vec::len);
        let state = logged.pointer("/actions/0/reply/state");
        // Their number depends on timing.
        let kind = &logged["event"]["request"]["kind"];
        let waiting = ["queued", "running", "starting", "closing"];
        let moving = logged
            .pointer("/actions/0/reply/last")
            .is_some_and(|last| last["kind"] == "transfer" && last["end"].is_null());
        if (kind == "status" || kind == "lease_status")
            && actions == 1
            && (moving || state.is_some_and(|state| waiting.iter().any(|waiting| state == waiting)))
        {
            polls += 1;
            continue;
        }
        let ops = iter::once("/event/op".to_owned())
            .chain((0..actions).map(|action| format!("/actions/{action}/op")));
        for op in ops {
            if let Some(op) = logged.pointer_mut(&op) {
                *op = json!(op.as_u64().unwrap() - polls);
            }
        }
        let times = [
            "/at_ns",
            "/observed_ns",
            "/event/first_byte_ns",
            "/event/first_token_ns",
        ];
        for time in times {
            if let Some(ns) = logged.pointer_mut(time)
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
            if let Some(start_by) = action.get_mut("start_by_ns")
                && !scripted
            {
                *start_by = json!("<ns>");
            }
            let times = [
                "/reply/remaining_seconds",
                "/reply/elapsed_seconds",
                "/reply/busy_seconds",
                "/reply/idle_seconds",
                "/reply/remaining/instance_seconds",
                "/reply/remaining/cumulative_seconds",
            ];
            for time in times {
                if let Some(seconds) = action.pointer_mut(time)
                    && !scripted
                {
                    *seconds = json!("<s>");
                }
            }
        }
        records.push(logged);
    }
    (numbered(records), start, last)
}

/// The time of the last of `log`'s records, a run log's, once it checks that they are numbered
/// from 0 with no gap, none timed before the one before it, no event observed after its record
/// was written, and each model request's start, its answer's first byte, its first token and its
/// end in that order, where the log has them, its start before its answer in the log.
fn in_order(log: &[Value]) -> Option<Duration> {
    let time = |ns: &Value| Duration::from_nanos(nanos(ns).unwrap());
    let (mut last, mut started) = (None, BTreeMap::new());
    for (seq, logged) in log.iter().enumerate() {
        assert_eq!(logged["seq"], json!(seq), "{logged}");
        let at = Some(time(&logged["at_ns"]));
        assert!(last <= at, "{logged} comes before the record before it");
        let observed = logged.get("observed_ns").map(time);
        assert!(observed <= at, "{logged} was observed after it was written");
        let event = &logged["event"];
        match event["kind"].as_str() {
            Some("model_request_started") => {
                started.insert(event["request"].as_u64(), observed);
            }
            Some("model_answered") => {
                let start = started.remove(&event["request"].as_u64()).flatten();
                let marks = ["first_byte_ns", "first_token_ns"];
                let marks = marks.map(|field| event.get(field).map(time));
                let each = iter::once(start).chain(marks).chain([observed]).flatten();
                assert!(each.is_sorted(), "{logged}: its times are out of order");
                let timed = start.is_some() || marks == [None, None];
                assert!(timed, "{logged}: it has times, but no start");
            }
            _ => {}
        }
        last = at;
    }
    last
}

/// `records`, each numbered in turn.
fn numbered(mut records: Vec<Value>) -> Vec<String> {
    let lines = (records.iter_mut().enumerate()).map(|(seq, record)| {
        record["seq"] = json!(seq);
        elided(&record.to_string())
    });
    lines.collect()
}

/// `line`, with each run of more than 64 `x`s as `<n x>`.
fn elided(line: &str) -> String {
    let (mut elided, mut rest) = (String::new(), line);
    while let Some(start) = rest.find('x') {
        let run = rest[start..].len() - rest[start..].trim_start_matches('x').len();
        elided.push_str(&rest[..start]);
        if run > 64 {
            write!(elided, "<{run} x>").unwrap();
        } else {
            elided.push_str(&rest[start..start + run]);
        }
        rest = &rest[start + run..];
    }
    elided.push_str(rest);
    elided
}

/// Fails, as test `name`, unless the log of `record`, a run's, taken by one fold as it grows in
/// pieces, each line in halves and its newline on its own, reads after each half as before it,
/// and after each newline of the first lines, of every 16th and of the last as the log then
/// does read whole; and unless, then found shorter, it is read again from its start. It leaves
/// the log as it was, but for when it was last written.
fn folds_as_whole(name: &str, record: &Path) {
    let path = record.join("log.jsonl");
    let whole = fs::read(&path).unwrap_or_default();
    if whole.is_empty() {
        return;
    }
    let opened = Record::open(record).unwrap();
    let read_whole = || format!("{:?}", opened.progress().unwrap());
    let mut fold = opened.fold();
    let mut log = fs::File::create(&path).unwrap();
    let mut before = read_whole();
    let lines = whole.split_inclusive(|byte| *byte == b'\n');
    let last = lines.clone().count() - 1;
    for (index, line) in lines.enumerate() {
        let (head, newline) = line.split_at(line.len() - 1);
        let (first, second) = head.split_at(head.len() / 2);
        for piece in [first, second] {
            log.write_all(piece).unwrap();
            assert!(
                fold.take().unwrap().is_none(),
                "{name}: a growing log read as shorter"
            );
            let read = format!("{:?}", fold.readout().unwrap());
            assert_eq!(read, before, "{name}: a line read before its newline");
        }
        log.write_all(newline).unwrap();
        assert!(
            fold.take().unwrap().is_none(),
            "{name}: a growing log read as shorter"
        );
        before = format!("{:?}", fold.readout().unwrap());
        // A read whole costs what the log holds, so reading it at every line would cost the
        // square of its lines.
        if index < 16 || index % 16 == 0 || index == last {
            assert_eq!(before, read_whole(), "{name}: the log read in pieces");
        }
    }
    fs::write(&path, &whole[..whole.len() / 2]).unwrap();
    assert!(
        fold.take().unwrap().is_some(),
        "{name}: a shorter log read on"
    );
    let read = format!("{:?}", fold.readout().unwrap());
    assert_eq!(read, read_whole(), "{name}: a shorter log read again");
    fs::write(&path, &whole).unwrap();
    assert!(
        fold.take().unwrap().is_none(),
        "{name}: a growing log read as shorter"
    );
    let read = format!("{:?}", fold.readout().unwrap());
    assert_eq!(read, read_whole(), "{name}: the log read in pieces");
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
    let route = json!([{ "api": "responses", "models": ["m-large"] }]);
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
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"18270000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":3}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":4}
            {"at_ns":"<ns>","note":"the run failed: deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","seq":5}
            its last record came before the agent's teardown was to end"#]],
    );
}

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
            the agent's sandbox, agent/sandbox.json: {"delete":{"reason":"sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","state":"failed"},"id":"sim-1","provider":"simulated","report":null}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"18270000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"agent_started"},"observed_ns":"0","seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"0","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":2}
            {"at_ns":"0","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":3}
            {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":4}
            {"at_ns":"0","note":"the run failed: deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; <record>/agent/.sandbox.json.partial: the simulated host's sync failed","seq":5}
            its last record came before the agent's teardown was to end"#]],
    );
}

#[test]
fn unshown() {
    let mut ir = workflow(&["exit", "0"]);
    ir["roles"][0]["image"] = json!(sim::image(UNSHOWN).as_str());
    let shown = executed(
        &System,
        "unshown",
        &[(PROVIDER, &Sim::new())],
        &ir,
        UNDELETABLE,
        &[Also::Listing],
    );
    expect![[r#"
        failed: deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `simulated` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`
        record: agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
        the agent's sandbox, agent/sandbox.json: {"delete":{"reason":"sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","state":"failed"},"id":"sim-1","provider":"simulated","report":null}
        log.jsonl:
        {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"18270000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
        {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":2}
        {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":3}
        {"at_ns":"<ns>","note":"sandbox sim-1's console wasn't kept: the simulated console of sim-1 can't be read: create one with another `sandboxd`","seq":4}
        {"at_ns":"<ns>","note":"sandbox sim-1's console wasn't kept: the simulated console of sim-1 can't be read: create one with another `sandboxd`","seq":5}
        {"at_ns":"<ns>","note":"the run failed: deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `simulated` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","seq":6}
        its last record came before the agent's teardown was to end
        listed once it ended: ["sim-1"]"#]]
    .assert_eq(&shown);
}

#[test]
fn unstarted_undeletable() {
    let mut ir = workflow(&["exit", "0"]);
    ir["roles"][0]["image"] = json!(sim::image(MUTE).as_str());
    let shown = executed(
        &System,
        "unstarted-undeletable",
        &[(PROVIDER, &Sim::new())],
        &ir,
        UNDELETABLE,
        &[Also::Listing],
    );
    expect![[r#"
        failed: deleting the sandboxes the run left on provider `simulated` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`
        record: agent/console.txt, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
        the agent's sandbox, agent/sandbox.json: {"delete":{"reason":"sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","state":"failed"},"id":"sim-1","provider":"simulated","report":null}
        the agent's console, agent/console.txt: "sandboxd: a simulated one, which never dials\n"
        log.jsonl:
        {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"18270000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: sandbox sim-1: it ended before it met the gateway: expected its `sandboxd` to run until it connects; its console ends: sandboxd: a simulated one, which never dials; deleting it failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: sandbox sim-1: it ended before it met the gateway: expected its `sandboxd` to run until it connects; its console ends: sandboxd: a simulated one, which never dials; deleting it failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`"},"observed_ns":"<ns>","seq":1}
        {"at_ns":"<ns>","note":"the agent's sandbox never started: nothing was collected","seq":2}
        {"at_ns":"<ns>","note":"the run failed: deleting the sandboxes the run left on provider `simulated` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","seq":3}
        its last record came before the agent's teardown was to end
        listed once it ended: ["sim-1"]"#]]
    .assert_eq(&shown);
}

// A run whose agent's command exits: its exit ends the work phase without a submission, and
// the agent's sandbox is collected and deleted, so the run finishes without a score.
#[test]
fn exits() {
    // A deadline far later than the exit, so that an exit the run misses ends the work phase.
    let mut ir = workflow(&["exit", "3"]);
    ir["roles"][0]["runtime"]["agent"]["deadline_seconds"] = json!(30);
    // Its class priced, so that `outcome.json` prices its sandbox.
    let shown = executed(
        &System,
        "exits",
        &[(PROVIDER, &Sim::new().priced(0.25))],
        &ir,
        SANDBOXD,
        &[Also::Usage],
    );
    expect![[r#"
        finished, with score None
        outcome.json's usage: {"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"sim","count":1,"lasted_ns":"<ns>","provider":"simulated","usd":"<usd>","usd_per_hour":0.25}],"tokens":{"input":0,"output":0,"responses":0},"usd":"<usd>"}
        log.jsonl:
        {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
        {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":3}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":3}},"observed_ns":"<ns>","seq":2}
        {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":3}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":4}
        its last record came before the agent's teardown was to end"#]]
    .assert_eq(&shown);
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
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"freezing the agent's sandbox failed: the sandbox ended the session","seq":3}
            {"at_ns":"<ns>","note":"the agent's out is incomplete: collecting /pumpkin/out failed: the sandbox ended the session","seq":4}
            {"at_ns":"<ns>","note":"the agent's log is incomplete: collecting /pumpkin/log failed: the sandbox ended the session","seq":5}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, nothing; log, nothing","seq":6}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":7}
            its last record came before the agent's teardown was to end"#]],
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
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"124000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}}],"at_ns":"<ns>","event":{"kind":"tick"},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"freezing the agent's sandbox failed: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts","seq":3}
            {"at_ns":"<ns>","note":"the agent's out is incomplete: collecting /pumpkin/out failed: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts","seq":4}
            {"at_ns":"<ns>","note":"the agent's log is incomplete: collecting /pumpkin/log failed: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts","seq":5}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, nothing; log, nothing","seq":6}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":7}
            its last record came once the agent's teardown was to end, before the hard end"#]],
    );
}

// Both runs end before the startup has passed once, which a start that waited it out
// wouldn't.
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
            &[],
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
        the agent's console, agent/console.txt: "sandboxd: a simulated one, which never dials\n"
        log.jsonl:
        {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"18270000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: sandbox sim-1: it ended before it met the gateway: expected its `sandboxd` to run until it connects; its console ends: sandboxd: a simulated one, which never dials"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: sandbox sim-1: it ended before it met the gateway: expected its `sandboxd` to run until it connects; its console ends: sandboxd: a simulated one, which never dials"},"observed_ns":"<ns>","seq":1}
        {"at_ns":"<ns>","note":"the agent's sandbox never started: nothing was collected","seq":2}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":3}
        its last record came before the agent's teardown was to end"#]].assert_eq(&listed);
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
        &[Also::Interrupted(at)],
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
            log.jsonl:"#]],
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
            it published: ["sandboxd /sim/sandboxd", "image sha256:1111111111111111111111111111111111111111111111111111111111111111"]
            log.jsonl:"#]],
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
            it published: ["sandboxd /sim/hung", "image sha256:1111111111111111111111111111111111111111111111111111111111111111", "image sha256:2222222222222222222222222222222222222222222222222222222222222222"]
            the agent's console, agent/console.txt: "sandboxd: a simulated one, which never dials\n"
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"18360000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"at_ns":"<ns>","note":"the run was interrupted by SIGTERM","seq":1}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":2}
            its last record came before the agent's teardown was to end"#]],
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
            it published: ["sandboxd /sim/sandboxd", "image sha256:1111111111111111111111111111111111111111111111111111111111111111", "image sha256:2f73696d2f68756e670000000000000000000000000000000000000000000000", "image sha256:2222222222222222222222222222222222222222222222222222222222222222"]
            job 0's console, jobs/0/console.txt: "sandboxd: a simulated one, which never dials\n"
            payloads hold: ["xxxxxx"]
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"420000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"the run was interrupted by SIGTERM","seq":3}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":4}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":5}
            its last record came before the agent's teardown was to end"#]],
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
            it published: ["sandboxd /sim/sandboxd", "image sha256:1111111111111111111111111111111111111111111111111111111111111111", "image sha256:2222222222222222222222222222222222222222222222222222222222222222"]
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"330000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"at_ns":"<ns>","note":"the run was interrupted by SIGTERM","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":3}
            its last record came before the agent's teardown was to end"#]],
    );
}

#[test]
fn interrupted_lease_start() {
    // Once a lease's runner is created, of an image whose `sandboxd` never dials, while the agent
    // awaits the lease, it ends the runner's start; the run log records the runner's delete after
    // the interruption, as it does the agent's.
    let argv = ["ops", "lease_open gpu", "await_lease 0 ready"];
    let image = sim::image(HUNG);
    interrupted(
        "lease-start",
        WAITED,
        &lease_workflow(&argv, WAITED, image.as_str()),
        SANDBOXD,
        At::Created("lease-0"),
        &expect![[r#"
            was interrupted by SIGTERM
            it started: true
            it returned within 60 s: true
            it published: ["sandboxd /sim/sandboxd", "image sha256:1111111111111111111111111111111111111111111111111111111111111111", "image sha256:2222222222222222222222222222222222222222222222222222222222222222", "image sha256:2f73696d2f68756e670000000000000000000000000000000000000000000000"]
            lease 0's console, leases/0/console.txt: "sandboxd: a simulated one, which never dials\n"
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"420000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"kind":"lease","lease":0,"remaining":{"commands":4,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"grant":"gpu","holder":"agent","instance":0,"kind":"start_lease","lease":0,"start_by_ns":"<ns>"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"gpu","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"the run was interrupted by SIGTERM","seq":3}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"lease-0"},"seq":4}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":5}
            its last record came before the agent's teardown was to end"#]],
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
            it published: ["sandboxd /sim/sandboxd", "image sha256:1111111111111111111111111111111111111111111111111111111111111111", "image sha256:2222222222222222222222222222222222222222222222222222222222222222"]
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":3}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":3}},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":3}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":4}
            its last record came before the agent's teardown was to end"#]],
    );
}

// The start lasts at least as long as the deadline, which counts only from the agent's start.
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
            the agent's console, agent/console.txt: "sandboxd: a simulated one, which never dials\n"
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"184000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: sandbox sim-1: no sandbox connected to <sockets>/agent/s by the deadline: expected its sandbox to connect while the deadline lasts; the last list of its provider failed: the simulated list failed while sim-1 lasts: delete it, or create one with another `sandboxd`; its console ends: sandboxd: a simulated one, which never dials"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: sandbox sim-1: no sandbox connected to <sockets>/agent/s by the deadline: expected its sandbox to connect while the deadline lasts; the last list of its provider failed: the simulated list failed while sim-1 lasts: delete it, or create one with another `sandboxd`; its console ends: sandboxd: a simulated one, which never dials"},"observed_ns":"<ns>","seq":1}
            {"at_ns":"<ns>","note":"the agent's sandbox never started: nothing was collected","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":3}
            its last record came before the agent's teardown was to end"#]],
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
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"18270000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: sandbox sim-1: the gateway refused the session: sandboxd speaks sandbox protocol 1: expected 0, which this gateway speaks, as a sandboxd built from the same Pumpkin does"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: sandbox sim-1: the gateway refused the session: sandboxd speaks sandbox protocol 1: expected 0, which this gateway speaks, as a sandboxd built from the same Pumpkin does"},"observed_ns":"<ns>","seq":1}
            {"at_ns":"<ns>","note":"the agent's sandbox never started: nothing was collected","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":3}
            its last record came before the agent's teardown was to end"#]],
    );
}

#[test]
fn lost() {
    let shown = executed(
        &System,
        "lost",
        &[(PROVIDER, &Sim::new())],
        &workflow(&["exit", "0"]),
        LOST,
        &[Also::Listing],
    );
    expect![[r#"
        finished, with score None
        record: driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
        log.jsonl:
        {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"18270000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: the simulated create failed, and can't name what it made: expected the run's list to find it"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: the simulated create failed, and can't name what it made: expected the run's list to find it"},"observed_ns":"<ns>","seq":1}
        {"at_ns":"<ns>","note":"the agent's sandbox never started: nothing was collected","seq":2}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":null},"seq":3}
        its last record came before the agent's teardown was to end"#]]
    .assert_eq(&shown);
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
            the agent's sandbox, agent/sandbox.json: {"delete":{"reason":"sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","state":"failed"},"id":"sim-1","provider":"simulated","report":null}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"18270000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":3}
            {"at_ns":"<ns>","note":"the run failed: deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `simulated` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","seq":4}
            its last record came before the agent's teardown was to end
            listed once it ended: ["sim-1"]"#]],
    );
}

#[test]
fn unpublished() {
    let shown = executed(
        &System,
        "unpublished",
        &[(PROVIDER, &Sim::new())],
        &workflow(&["exit", "0"]),
        UNPUBLISHED,
        &[Also::Listing],
    );
    expect![[r#"
        failed: publishing `sandboxd` on provider `simulated` failed: /sim/unpublished can't be published by the simulated provider: name another path
        record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json
        log.jsonl:"#]]
    .assert_eq(&shown);
}

// A run whose judge's image the provider can't publish: the run fails before it starts, naming
// the judge, and nothing is created.
#[test]
fn unpublished_image() {
    let mut ir = workflow(&["exit", "0"]);
    ir["roles"][1]["image"] = json!(sim::image(UNPUBLISHED).as_str());
    let shown = executed(
        &System,
        "unpublished-image",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &[Also::Listing],
    );
    expect![[r#"
        failed: publishing role `train_judge`'s image on provider `simulated` failed: sha256:2f73696d2f756e7075626c697368656400000000000000000000000000000000 can't be published by the simulated provider: name another image
        record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json
        log.jsonl:"#]]
    .assert_eq(&shown);
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
            payloads hold: ["slow", "xxxxxx"]
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":"<s>","resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"kind":"ops"}},"observed_ns":"<ns>","seq":2}
            {"actions":[{"kind":"reply","op":1,"reply":{"kind":"refused","message":"you have no job 0: ask about a job that one of your calls returned"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":1,"request":{"job":"0","kind":"status"}},"observed_ns":"<ns>","seq":3}
            {"actions":[{"kind":"reply","op":2,"reply":{"kind":"refused","message":"you hold no grant named `nope`: call one that `ops` lists"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":2,"request":{"grant":"nope","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":4}
            {"actions":[{"kind":"reply","op":3,"reply":{"kind":"refused","message":"the payload is 9 bytes, over `train`'s limit of 8: send a smaller one"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":3,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:a73add1aecea03a894ca65d99bc5e91d21b58cdb728343bbcb6cf6cf38b809f5","size":9}}},"observed_ns":"<ns>","seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"op_refused","message":"the payload is 17 bytes, more than the limit of 16: send at most 16","request":{"grant":"train","kind":"judge","payload":{"over":16,"size":17}}},"seq":6}
            {"actions":[{"kind":"reply","op":4,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":4,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":7}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":8}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":9}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":10}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":11}
            {"actions":[{"kind":"reply","op":5,"reply":{"job":0,"kind":"status","remaining":{"calls":99,"seconds":null},"returned":{"raw_score":6},"scored":true,"state":"completed"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":5,"request":{"job":"0","kind":"status"}},"observed_ns":"<ns>","seq":12}
            {"actions":[{"kind":"reply","op":6,"reply":{"job":1,"kind":"judge","remaining":{"calls":98,"seconds":null}}},{"grant":"train","job":1,"kind":"start_judge","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":6,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4}}},"observed_ns":"<ns>","seq":13}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_started"},"observed_ns":"<ns>","seq":14}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_timed_out"},"observed_ns":"<ns>","seq":15}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-1"},"seq":16}
            {"actions":[],"at_ns":"<ns>","event":{"invalid":"the judge wrote no result: write it at /pumpkin/out/result.json","job":1,"kind":"judge_collected"},"observed_ns":"<ns>","seq":17}
            {"actions":[{"kind":"reply","op":7,"reply":{"failure":{"code":"judge_timeout","message":"the judge ran past its timeout"},"job":1,"kind":"status","remaining":{"calls":98,"seconds":null},"state":"errored"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":7,"request":{"job":"1","kind":"status"}},"observed_ns":"<ns>","seq":18}
            {"actions":[{"kind":"reply","op":8,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":8,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":19}
            {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":2,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":20}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":21}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":22}
            {"actions":[],"at_ns":"<ns>","event":{"job":2,"kind":"judge_started"},"observed_ns":"<ns>","seq":23}
            {"actions":[],"at_ns":"<ns>","event":{"job":2,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":24}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-4","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-2"},"seq":25}
            {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":2,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":26}
            its last record came before the agent's teardown was to end"#]],
    );
}

// The read fails, as the size and digest differ, rather than put other bytes in the judge's slot.
#[test]
fn swapped_payload() {
    let swapped = executed(
        &System,
        "swapped-payload",
        &[(PROVIDER, &Sim::new())],
        &ops_workflow(&["ops", "finalize submit 6"], 30),
        SANDBOXD,
        &[Also::Interrupted(At::SwapPayloads)],
    );
    expect![[r#"
        finished, with score None
        payloads hold: ["yyyyyyyyy"]
        log.jsonl:
        {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
        {"actions":[{"kind":"reply","op":0,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":2}
        {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":3}
        {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":4}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":5}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":6}
        {"actions":[{"kind":"finish","outcome":{"score":null,"verdict":{"failure":{"kind":"error","reason":"<record>/payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd holds 9 bytes of digest sha256:cf0da94d4b6a0de4d83a79dfe964489ba50a38aef90bc406aed5558fee383781: expected the payload's 6 bytes of digest sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd"},"kind":"no_result","submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"<record>/payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd holds 9 bytes of digest sha256:cf0da94d4b6a0de4d83a79dfe964489ba50a38aef90bc406aed5558fee383781: expected the payload's 6 bytes of digest sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd"},"observed_ns":"<ns>","seq":7}
        the final judge's standard error, as `wait` finds it: Ok("jobs/0/log/command.stderr")
        its last record came before the agent's teardown was to end"#]].assert_eq(&swapped);
}

// A run whose agent finalizes a submission whose final judge writes a result over the most a
// result may hold: the result is removed from the record, and the run finishes with no score.
#[test]
fn result_over_the_limit() {
    let shown = executed(
        &System,
        "result-over-the-limit",
        &[(PROVIDER, &Sim::new())],
        &ops_workflow(&["ops", "finalize submit large"], 30),
        SANDBOXD,
        &[Also::Listing],
    );
    expect![[r#"
        finished, with score None
        record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/console.txt, jobs/0/log/collected, jobs/0/out/collected, jobs/0/out/evidence/, jobs/0/sandbox.json, log.jsonl, outcome.json, payloads/d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9, provider.json, record.json
        payloads hold: ["large"]
        log.jsonl:
        {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
        {"actions":[{"kind":"reply","op":0,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9","size":5}}},"observed_ns":"<ns>","seq":2}
        {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9","size":5},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":3}
        {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":4}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":5}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":6}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":7}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":8}
        {"actions":[{"kind":"finish","outcome":{"score":null,"verdict":{"failure":{"kind":"invalid_result","reason":"result.json is 1048577 bytes, over the limit of 1048576: write a smaller one"},"kind":"no_result","submission":{"finalized":true,"payload":{"digest":"sha256:d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9","size":5}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"invalid":"result.json is 1048577 bytes, over the limit of 1048576: write a smaller one","job":0,"kind":"judge_collected"},"observed_ns":"<ns>","seq":9}
        the final judge's standard error, as `wait` finds it: Ok("jobs/0/log/command.stderr")
        its last record came before the agent's teardown was to end"#]]
    .assert_eq(&shown);
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
            payloads hold: ["xxxxxxxxxxxx"]
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:59ffe12a70df15109e0345955e3230a978f31ebc28d8fe3e42d306afb28b8e81","size":12},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:59ffe12a70df15109e0345955e3230a978f31ebc28d8fe3e42d306afb28b8e81","size":12}}},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"op 0's reply failed: the sandbox ended the session","seq":3}
            {"actions":[{"job":0,"kind":"stop_judge"},{"kind":"tear_down_agent"}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"},"observed_ns":"<ns>","seq":4}
            {"at_ns":"<ns>","note":"freezing the agent's sandbox failed: the sandbox ended the session","seq":5}
            {"at_ns":"<ns>","note":"the agent's out is incomplete: collecting /pumpkin/out failed: the sandbox ended the session","seq":6}
            {"at_ns":"<ns>","note":"the agent's log is incomplete: collecting /pumpkin/log failed: the sandbox ended the session","seq":7}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, nothing; log, nothing","seq":8}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":9}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":10}
            {"actions":[{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"the call was stopped before its command started"},"observed_ns":"<ns>","seq":11}
            its last record came before the agent's teardown was to end"#]],
    );
}

// An agent's call starts by the work phase's end, however long the provider's startup.
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
            payloads hold: ["xxxxxx"]
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"272000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":2}
            {"actions":[{"job":0,"kind":"stop_judge"},{"kind":"tear_down_agent"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":3}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":4}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":5}
            {"actions":[{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"the judge's sandbox didn't start: the simulated create gave up at its deadline, and can't name what it made: expected the run's list to find it"},"observed_ns":"<ns>","seq":6}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":null},"seq":7}
            its last record came before the agent's teardown was to end"#]],
    );
}

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
            job 0's console, jobs/0/console.txt: "sandboxd: a simulated one, which never dials\n"
            payloads hold: ["xxxxxx"]
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":3}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"the judge's sandbox didn't start: sandbox sim-2: no sandbox connected to <sockets>/job-0/s by the deadline: expected its sandbox to connect while the deadline lasts; its console ends: sandboxd: a simulated one, which never dials"},"observed_ns":"<ns>","seq":4}
            {"actions":[{"kind":"reply","op":1,"reply":{"failure":{"code":"judge_error","message":"the judge failed before giving a result"},"job":0,"kind":"status","remaining":{"calls":100,"seconds":null},"state":"errored"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":1,"request":{"job":"0","kind":"status"}},"observed_ns":"<ns>","seq":5}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":6}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":7}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":8}
            its last record came before the agent's teardown was to end"#]],
    );
}

/// `sim`, as provider `there` of a run on two, beside `here`: its classes offer less memory than
/// the agent needs, and more than each judge does, so the agent is placed on `here`, and each
/// judge that needs no more memory than the others on `there`.
fn there(sim: Sim) -> Sim {
    sim.named("there").offering_memory(16 << 30)
}

// `there` is private, so its sandboxes dial the run's grant listener, or it listens, so the run
// dials them, and `here`'s dial their sockets, alike either way.
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
    for reach in [Sim::private, Sim::listening] {
        let (here, there) = (Sim::new().named("here"), there(reach(Sim::new())));
        let providers = [("here", &here), ("there", &there)];
        let shown = executed(
            &System,
            "spans-providers",
            &providers,
            &ir,
            SANDBOXD,
            &[Also::Usage],
        );
        expect![[r#"
            finished, with score Some(Number(6))
            payloads hold: ["xxxxxx"]
            provider.json places the roles: {"agent":{"class":"sim","image":{"build_tag":"pumpkin/agent:0123","digest":"sha256:2222222222222222222222222222222222222222222222222222222222222222"},"provider":"here"},"final_judge":{"class":"sim","image":{"build_tag":"pumpkin/agent:0123","digest":"sha256:2222222222222222222222222222222222222222222222222222222222222222"},"provider":"there"},"train_judge":{"class":"brief","image":{"build_tag":"pumpkin/agent:0123","digest":"sha256:2222222222222222222222222222222222222222222222222222222222222222"},"provider":"there"}}
            outcome.json's usage: {"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"sim","count":1,"lasted_ns":"<ns>","provider":"here"},{"class":"brief","count":1,"lasted_ns":"<ns>","provider":"there"},{"class":"sim","count":1,"lasted_ns":"<ns>","provider":"there"}],"tokens":{"input":0,"output":0,"responses":0}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":3}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":4}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"there","sandbox":"job-0"},"seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":6}
            {"actions":[{"kind":"reply","op":1,"reply":{"job":0,"kind":"status","remaining":{"calls":99,"seconds":null},"returned":{"raw_score":6},"scored":true,"state":"completed"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":1,"request":{"job":"0","kind":"status"}},"observed_ns":"<ns>","seq":7}
            {"actions":[{"kind":"reply","op":2,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":2,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":8}
            {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":1,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":9}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":10}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"here","sandbox":"agent"},"seq":11}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_started"},"observed_ns":"<ns>","seq":12}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":13}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"there","sandbox":"job-1"},"seq":14}
            {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":1,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":15}
            its last record came before the agent's teardown was to end
            here published: ["sandboxd /sim/sandboxd", "image sha256:2222222222222222222222222222222222222222222222222222222222222222"]
            here: the class at each create: ["sim"]
            there published: ["sandboxd /sim/sandboxd", "image sha256:2222222222222222222222222222222222222222222222222222222222222222"]
            there: the class at each create: ["brief", "sim"]"#]]
        .assert_eq(&shown);
    }
}

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
            the agent's sandbox, agent/sandbox.json: {"delete":{"reason":"sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","state":"failed"},"id":"sim-1","provider":"here","report":null}
            payloads hold: ["xxxxxx"]
            provider.json places the roles: {"agent":{"class":"sim","image":{"build_tag":"pumpkin/agent:0123","digest":"sha256:2f73696d2f756e64656c657461626c6500000000000000000000000000000000"},"provider":"here"},"final_judge":{"class":"sim","image":{"digest":"sha256:2222222222222222222222222222222222222222222222222222222222222222"},"provider":"there"},"train_judge":{"class":"sim","image":{"digest":"sha256:2f73696d2f6c6f73740000000000000000000000000000000000000000000000"},"provider":"there"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"the judge's sandbox didn't start: the simulated create failed, and can't name what it made: expected the run's list to find it"},"observed_ns":"<ns>","seq":3}
            {"actions":[{"kind":"reply","op":1,"reply":{"failure":{"code":"judge_error","message":"the judge failed before giving a result"},"job":0,"kind":"status","remaining":{"calls":100,"seconds":null},"state":"errored"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":1,"request":{"job":"0","kind":"status"}},"observed_ns":"<ns>","seq":4}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":5}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":6}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"there","sandbox":null},"seq":7}
            {"at_ns":"<ns>","note":"the run failed: deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `here` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","seq":8}
            its last record came before the agent's teardown was to end
            here published: ["sandboxd /sim/sandboxd", "image sha256:2f73696d2f756e64656c657461626c6500000000000000000000000000000000"]
            here: the class at each create: ["sim"]
            here listed once it ended: ["sim-1"]
            there published: ["sandboxd /sim/sandboxd", "image sha256:2f73696d2f6c6f73740000000000000000000000000000000000000000000000", "image sha256:2222222222222222222222222222222222222222222222222222222222222222"]
            there: the class at each create: ["sim"]"#]],
    );
}

// The watcher takes each provider's list for that provider's starts alone.
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
    // judge's start is kept waiting by its class's startup, 4 s: past the end of the train
    // judge's start, a second later, at its class's 2 s. A watcher that listed `here` alone
    // while its start waits would leave the train judge's start to run out at its startup.
    let here = Sim::with_startup(4).named("here");
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
            job 0's console, jobs/0/console.txt: "sandboxd: a simulated one, which never dials\n"
            job 1's console, jobs/1/console.txt: "sandboxd: a simulated one, which never dials\n"
            payloads hold: ["xxxxxx"]
            provider.json places the roles: {"agent":{"class":"sim","image":{"build_tag":"pumpkin/agent:0123","digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111"},"provider":"here"},"final_judge":{"class":"sim","image":{"digest":"sha256:2222222222222222222222222222222222222222222222222222222222222222"},"provider":"there"},"near_judge":{"class":"sim","image":{"digest":"sha256:2f73696d2f68756e670000000000000000000000000000000000000000000000"},"provider":"here"},"train_judge":{"class":"brief","image":{"digest":"sha256:2f73696d2f6d7574650000000000000000000000000000000000000000000000"},"provider":"there"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"248000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"near","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"near","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":2}
            {"actions":[{"kind":"reply","op":1,"reply":{"job":1,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":1,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":1,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":3}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"there","sandbox":"job-1"},"seq":4}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_failed","reason":"the judge's sandbox didn't start: sandbox sim-1: it ended before it met the gateway: expected its `sandboxd` to run until it connects; its console ends: sandboxd: a simulated one, which never dials"},"observed_ns":"<ns>","seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"here","sandbox":"job-0"},"seq":6}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"the judge's sandbox didn't start: sandbox sim-2: no sandbox connected to <sockets>/job-0/s by the deadline: expected its sandbox to connect while the deadline lasts; its console ends: sandboxd: a simulated one, which never dials"},"observed_ns":"<ns>","seq":7}
            {"actions":[{"kind":"reply","op":2,"reply":{"failure":{"code":"judge_error","message":"the judge failed before giving a result"},"job":0,"kind":"status","remaining":{"calls":100,"seconds":null},"state":"errored"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":2,"request":{"job":"0","kind":"status"}},"observed_ns":"<ns>","seq":8}
            {"actions":[{"kind":"reply","op":3,"reply":{"failure":{"code":"judge_error","message":"the judge failed before giving a result"},"job":1,"kind":"status","remaining":{"calls":100,"seconds":null},"state":"errored"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":3,"request":{"job":"1","kind":"status"}},"observed_ns":"<ns>","seq":9}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":10}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":11}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"here","sandbox":"agent"},"seq":12}
            its last record came before the agent's teardown was to end
            here published: ["sandboxd /sim/sandboxd", "image sha256:1111111111111111111111111111111111111111111111111111111111111111", "image sha256:2f73696d2f68756e670000000000000000000000000000000000000000000000"]
            here: the class at each create: ["sim", "sim"]
            there published: ["sandboxd /sim/sandboxd", "image sha256:2f73696d2f6d7574650000000000000000000000000000000000000000000000", "image sha256:2222222222222222222222222222222222222222222222222222222222222222"]
            there: the class at each create: ["brief"]"#]],
    );
}

// The call's freeze runs out at the agent's teardown's end, which leaves the final judge its
// startup and timeout before the hard end.
#[test]
fn judge_stalls() {
    let argv = [
        "ops",
        "judge train stall",
        "await 0 running",
        "finalize submit 6",
    ];
    // A deadline seconds after the agent's ops, so that the teardown's end, which follows it,
    // comes soon; and a call timeout a second longer than the agent takes to finalize, so that
    // the stop ends the call, and a second shorter than the time left when the call is made, so
    // that it is the call's timeout.
    let mut ir = short_workflow(&argv, 2);
    ir["roles"][0]["grants"][0]["judge"]["call_timeout_seconds"] = json!(1);
    ir["roles"][2]["image"] = json!(sim::image(UNDELETABLE).as_str());
    check(
        &System,
        "judge-stalls",
        &[(PROVIDER, &Sim::with_startup(SHORT_STARTUP))],
        &ir,
        SANDBOXD,
        &expect![[r#"
            failed: deleting the sandbox failed: sim-3 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `simulated` failed: sim-3 can't be deleted by the simulated provider: create one with another `sandboxd`
            job 1's sandbox, jobs/1/sandbox.json: {"delete":{"reason":"sim-3 can't be deleted by the simulated provider: create one with another `sandboxd`","state":"failed"},"id":"sim-3","provider":"simulated","report":null}
            payloads hold: ["stall", "xxxxxx"]
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"6000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:e5b6a7364e8a1049a9236dacfa24768ccffd3c4b671dc8b02953625844789b97","size":5},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:e5b6a7364e8a1049a9236dacfa24768ccffd3c4b671dc8b02953625844789b97","size":5}}},"observed_ns":"<ns>","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":3}
            {"actions":[{"kind":"reply","op":1,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}},{"job":0,"kind":"stop_judge"},{"kind":"tear_down_agent"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":1,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":4}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":6}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":7}
            {"actions":[{"grant":null,"job":1,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"freezing the judge's sandbox failed: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts"},"observed_ns":"<ns>","seq":8}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_started"},"observed_ns":"<ns>","seq":9}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":10}
            {"at_ns":"<ns>","note":"the run failed: deleting the sandbox failed: sim-3 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `simulated` failed: sim-3 can't be deleted by the simulated provider: create one with another `sandboxd`","seq":11}
            its last record came once the agent's teardown was to end, before the hard end
            listed once it ended: ["sim-3"]"#]],
    );
}

// The run's clock passes the hard end as the agent's delete is recorded, while the stopped call's
// delete waits for the agent's, so the hard end's tick comes before the call's end.
#[test]
fn abandoned() {
    let argv = ["ops", "judge train slow", "await 0 running", "exit"];
    let mut ir = short_workflow(&argv, 2);
    ir["roles"][1]["image"] = json!(sim::image(LAST).as_str());
    let slow = SlowSync {
        record: Synced::Holding(AGENT_SANDBOX, DELETED),
        queued: 0,
        at: PAST_THE_HARD_END,
    };
    check(
        &SimHost::new().slow_sync(slow),
        "abandoned",
        &[(PROVIDER, &Sim::with_startup(SHORT_STARTUP))],
        &ir,
        SANDBOXD,
        &expect![[r#"
            failed: the run was still going at its hard end, 6 s after it started: expected it to end within its deadline, its finalize grace, its final judge's timeout and its sandboxes' startups
            payloads hold: ["slow"]
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"6000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"agent_started"},"observed_ns":"0","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4},"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4}}},"observed_ns":"0","seq":2}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"judge_started"},"observed_ns":"0","seq":3}
            {"actions":[{"job":0,"kind":"stop_judge"},{"kind":"tear_down_agent"}],"at_ns":"0","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":4}
            {"at_ns":"0","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":5}
            {"actions":[],"at_ns":"6500000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":6}
            {"actions":[{"kind":"abandon"}],"at_ns":"6500000000","event":{"kind":"tick"},"observed_ns":"6500000000","seq":7}
            {"actions":[],"at_ns":"6500000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":8}
            {"actions":[],"at_ns":"6500000000","event":{"job":0,"kind":"judge_failed","reason":"the call was stopped"},"observed_ns":"6500000000","seq":9}
            {"at_ns":"6500000000","note":"the run failed: the run was still going at its hard end, 6 s after it started: expected it to end within its deadline, its finalize grace, its final judge's timeout and its sandboxes' startups","seq":10}
            its last record came once the hard end had passed"#]],
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
            payloads hold: ["slow"]
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"34000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4},"timeout_ns":"20000000000"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4}}},"observed_ns":"<ns>","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":3}
            {"actions":[{"job":0,"kind":"stop_judge"},{"kind":"tear_down_agent"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":4}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":6}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":7}
            {"actions":[{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"the call was stopped"},"observed_ns":"<ns>","seq":8}
            its last record came before the agent's teardown was to end"#]],
    );
}

// The late op and the call's report each came before the hard end, at the time it carries, so the
// report ends the call in time and the run finishes rather than be abandoned.
#[test]
fn reported_before_the_hard_end() {
    // A deadline later than the agent's ops, so that the agent's exit ends the work phase, and
    // bounds the judge's start.
    let argv = ["ops", "judge train 6", "exit", "judge train 6"];
    let mut ir = short_workflow(&argv, 2);
    ir["roles"][1]["image"] = json!(sim::image(HUNG).as_str());
    // Held until the run's other threads have queued each input they send, the judge's failure,
    // stamped after its delete, last; then the clock passes the hard end, past no input's stamp.
    let slow = SlowSync {
        record: Synced::Holding(AGENT_SANDBOX, DELETED),
        queued: 4,
        at: PAST_THE_HARD_END,
    };
    check(
        &SimHost::new().slow_sync(slow),
        "reported-before-the-hard-end",
        &[(PROVIDER, &Sim::with_startup(SHORT_STARTUP))],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            job 0's console, jobs/0/console.txt: "sandboxd: a simulated one, which never dials\n"
            payloads hold: ["xxxxxx"]
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"6000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"agent_started"},"observed_ns":"0","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":2}
            {"actions":[{"job":0,"kind":"stop_judge"},{"kind":"tear_down_agent"}],"at_ns":"0","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":3}
            {"at_ns":"0","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":4}
            {"actions":[],"at_ns":"6500000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":5}
            {"actions":[{"kind":"reply","op":1,"reply":{"kind":"refused","message":"the work phase has ended: make no more ops"}}],"at_ns":"6500000000","event":{"caller":"agent","kind":"op_arrived","op":1,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":6}
            {"at_ns":"6500000000","note":"op 1's reply wasn't sent: the agent's sandbox is gone","seq":7}
            {"actions":[],"at_ns":"6500000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":8}
            {"actions":[{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"6500000000","event":{"job":0,"kind":"judge_failed","reason":"the judge's sandbox didn't start: sandbox sim-2: no sandbox connected to <sockets>/job-0/s by the deadline: expected its sandbox to connect while the deadline lasts; its console ends: sandboxd: a simulated one, which never dials"},"observed_ns":"0","seq":9}
            its last record came once the hard end had passed"#]],
    );
}

// The op reached the host before the deadline, so the core accepts it; the failure, which
// reached it after, waits for the deadline's tick.
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
            payloads hold: ["xxxxxx"]
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"agent_started"},"observed_ns":"0","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"30500000000","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":2}
            {"at_ns":"30500000000","note":"op 0's reply wasn't sent: it was due by the core's next act, which has come","seq":3}
            {"actions":[{"job":0,"kind":"stop_judge"},{"kind":"tear_down_agent"}],"at_ns":"30500000000","event":{"kind":"tick"},"observed_ns":"30500000000","seq":4}
            {"at_ns":"30500000000","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":5}
            {"actions":[],"at_ns":"30500000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":6}
            {"actions":[{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}}],"at_ns":"30500000000","event":{"job":0,"kind":"judge_failed","reason":"the judge's sandbox wasn't created: its start was due by a time that had passed"},"observed_ns":"30500000000","seq":7}
            its last record came before the agent's teardown was to end"#]],
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
            payloads hold: ["xxxxxx"]
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"agent_started"},"observed_ns":"0","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"3600000000000","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":2}
            {"at_ns":"3600000000000","note":"op 0's reply wasn't sent: it was due by the core's next act, which has come","seq":3}
            {"actions":[{"kind":"abandon"}],"at_ns":"3600000000000","event":{"kind":"tick"},"observed_ns":"3600000000000","seq":4}
            {"actions":[],"at_ns":"3600000000000","event":{"job":0,"kind":"judge_failed","reason":"the judge's sandbox wasn't created: its start was due by a time that had passed"},"observed_ns":"3600000000000","seq":5}
            {"actions":[],"at_ns":"3600000000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":6}
            {"at_ns":"3600000000000","note":"the run failed: the run was still going at its hard end, 121 s after it started: expected it to end within its deadline, its finalize grace, its final judge's timeout and its sandboxes' startups","seq":7}
            its last record came once the hard end had passed"#]],
    );
}

// The final judge's start is due by the hard end, which has passed, so its call fails with no
// sandbox created, and the hard end's tick, which comes first, abandons the run.
#[test]
fn torn_down_past_the_hard_end() {
    // The record of the agent's delete holds the loop past the hard end.
    let slow = SlowSync {
        record: Synced::Holding(AGENT_SANDBOX, DELETED),
        queued: 0,
        at: PAST_THE_HARD_END,
    };
    check(
        &SimHost::new().slow_sync(slow),
        "torn-down-past-the-hard-end",
        &[(PROVIDER, &Sim::with_startup(SHORT_STARTUP))],
        &short_workflow(&["ops", "finalize submit 6"], 2),
        SANDBOXD,
        &expect![[r#"
            failed: the run was still going at its hard end, 6 s after it started: expected it to end within its deadline, its finalize grace, its final judge's timeout and its sandboxes' startups
            payloads hold: ["xxxxxx"]
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"6000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"agent_started"},"observed_ns":"0","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"kind":"submit","remaining_seconds":0,"state":"accepted"}},{"kind":"tear_down_agent"},{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":2}
            {"at_ns":"0","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":3}
            {"actions":[],"at_ns":"6500000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":4}
            {"actions":[{"kind":"abandon"}],"at_ns":"6500000000","event":{"kind":"tick"},"observed_ns":"6500000000","seq":5}
            {"actions":[],"at_ns":"6500000000","event":{"job":0,"kind":"judge_failed","reason":"the judge's sandbox wasn't created: its start was due by a time that had passed"},"observed_ns":"6500000000","seq":6}
            {"at_ns":"6500000000","note":"the run failed: the run was still going at its hard end, 6 s after it started: expected it to end within its deadline, its finalize grace, its final judge's timeout and its sandboxes' startups","seq":7}
            its last record came once the hard end had passed"#]],
    );
}

// The agent checks that the `ops` after its 1 s sleep leaves at least 1 s less than the one before,
// however long it took to start.
#[test]
fn late_ops() {
    let argv = [
        "ops",
        "ops",
        "sleep 1",
        "ops at least 1 less",
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
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":"<s>","resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"kind":"ops"}},"observed_ns":"<ns>","seq":2}
            {"actions":[{"kind":"reply","op":1,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":"<s>","resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":1,"request":{"kind":"ops"}},"observed_ns":"<ns>","seq":3}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":4}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":6}
            {"actions":[{"kind":"reply","op":2,"reply":{"kind":"refused","message":"the work phase has ended: make no more ops"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":2,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":7}
            {"at_ns":"<ns>","note":"op 2's reply wasn't sent: the agent's sandbox is gone","seq":8}
            its last record came before the agent's teardown was to end"#]],
    );
}

// Deterministic, as one thread, the session's reader, produces both inputs, which reach the
// inbox in a fixed order.
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
            payloads hold: ["xxxxxx"]
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"agent_started"},"observed_ns":"0","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"30500000000","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":2}
            {"at_ns":"30500000000","note":"op 0's reply wasn't sent: it was due by the core's next act, which has come","seq":3}
            {"actions":[{"job":0,"kind":"stop_judge"},{"kind":"tear_down_agent"}],"at_ns":"30500000000","event":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"},"observed_ns":"0","seq":4}
            {"at_ns":"30500000000","note":"freezing the agent's sandbox failed: the sandbox ended the session","seq":5}
            {"at_ns":"30500000000","note":"the agent's out is incomplete: collecting /pumpkin/out failed: the sandbox ended the session","seq":6}
            {"at_ns":"30500000000","note":"the agent's log is incomplete: collecting /pumpkin/log failed: the sandbox ended the session","seq":7}
            {"at_ns":"30500000000","note":"collected from the agent's sandbox: out, nothing; log, nothing","seq":8}
            {"actions":[],"at_ns":"30500000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":9}
            {"actions":[{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}}],"at_ns":"30500000000","event":{"job":0,"kind":"judge_failed","reason":"the judge's sandbox wasn't created: its start was due by a time that had passed"},"observed_ns":"30500000000","seq":10}
            its last record came before the agent's teardown was to end"#]],
    );
}

// The session's end, which reached the host at 0, is decided at 0, ahead of the core's tick.
#[test]
fn held_by_a_slow_sync() {
    let slow = SlowSync {
        record: Synced::Seq(2),
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
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"agent_started"},"observed_ns":"0","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":30,"resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"0","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"kind":"ops"}},"observed_ns":"0","seq":2}
            {"at_ns":"30500000000","note":"op 0's reply wasn't sent: it was due by the core's next act, which has come","seq":3}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}}],"at_ns":"30500000000","event":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"},"observed_ns":"0","seq":4}
            {"at_ns":"30500000000","note":"freezing the agent's sandbox failed: the sandbox ended the session","seq":5}
            {"at_ns":"30500000000","note":"the agent's out is incomplete: collecting /pumpkin/out failed: the sandbox ended the session","seq":6}
            {"at_ns":"30500000000","note":"the agent's log is incomplete: collecting /pumpkin/log failed: the sandbox ended the session","seq":7}
            {"at_ns":"30500000000","note":"collected from the agent's sandbox: out, nothing; log, nothing","seq":8}
            {"actions":[],"at_ns":"30500000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":9}
            its last record came before the agent's teardown was to end"#]],
    );
}

// The deadline counts from the agent's start, the startup from the run's: held past the
// startup, the start fails.
#[test]
fn started_late() {
    let slow = SlowSync {
        record: Synced::Seq(0),
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
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"25000000000","event":{"kind":"agent_started"},"observed_ns":"25000000000","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":30,"resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"55000000000","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"kind":"ops"}},"observed_ns":"25000000000","seq":2}
            {"at_ns":"55000000000","note":"op 0's reply wasn't sent: it was due by the core's next act, which has come","seq":3}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}}],"at_ns":"55000000000","event":{"kind":"tick"},"observed_ns":"55000000000","seq":4}
            {"at_ns":"55000000000","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":5}
            {"actions":[],"at_ns":"55000000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":6}
            its last record came before the agent's teardown was to end"#]],
    );
    let slow = SlowSync {
        record: Synced::Seq(0),
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
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: the simulated create's deadline had passed: expected time left to make it"}}}],"at_ns":"35000000000","event":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: the simulated create's deadline had passed: expected time left to make it"},"observed_ns":"35000000000","seq":1}
            {"at_ns":"35000000000","note":"the agent's sandbox never started: nothing was collected","seq":2}
            its last record came before the agent's teardown was to end"#]],
    );
}

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
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"agent_started"},"observed_ns":"0","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":30,"resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"0","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"kind":"ops"}},"observed_ns":"0","seq":2}
            {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":3}
            {"at_ns":"0","note":"the run failed: <record>/log.jsonl: the simulated host's sync failed","seq":4}
            its last record came before the agent's teardown was to end"#]],
    );
}

#[test]
fn refusal_held_by_a_slow_sync() {
    let slow = SlowSync {
        record: Synced::Seq(3),
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
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"agent_started"},"observed_ns":"0","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":30,"resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"0","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"kind":"ops"}},"observed_ns":"0","seq":2}
            {"actions":[],"at_ns":"0","event":{"kind":"op_refused","message":"the payload is 17 bytes, more than the limit of 16: send at most 16","request":{"grant":"train","kind":"judge","payload":{"over":16,"size":17}}},"seq":3}
            {"at_ns":"30500000000","note":"the refusal of an op whose payload was over the agent's limit wasn't sent: it was due by the core's next act, which has come","seq":4}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}}],"at_ns":"30500000000","event":{"kind":"tick"},"observed_ns":"30500000000","seq":5}
            {"at_ns":"30500000000","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":6}
            {"actions":[],"at_ns":"30500000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":7}
            its last record came before the agent's teardown was to end"#]],
    );
}

// The agent's start leads to no act, so it is synced with the next record that does; the note of
// what its teardown collected, which no such record follows, by the run's end.
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
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"agent_started"},"observed_ns":"0","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":30,"resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"0","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"kind":"ops"}},"observed_ns":"0","seq":2}
            {"actions":[],"at_ns":"0","event":{"kind":"op_refused","message":"the payload is 17 bytes, more than the limit of 16: send at most 16","request":{"grant":"train","kind":"judge","payload":{"over":16,"size":17}}},"seq":3}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"0","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":4}
            {"at_ns":"0","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":5}
            {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":6}
            its last record came before the agent's teardown was to end
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
              fsync <record>"#]],
    );
}

// The route stays open through the grace after the finalize, until the agent's exit ends the
// work phase.
#[test]
fn model_requests() {
    // The first request's last item holds what JSON escapes, and what it writes as UTF-8, in a
    // list in an object, which its rebuild must give back byte for byte.
    let argv = [
        "ops",
        "model m-large stream \"quoted\"\\back\nline\u{1}\ttab/é→😀",
        "model m-huge stream",
        "finalize submit 6",
        "model m-large usage",
        "model m-large limited",
        "model m-large silent",
        "model m-large expired",
    ];
    let shown = executed(
        &System,
        "model-requests",
        &[(PROVIDER, &Sim::new())],
        &routed(ops_workflow(&argv, 30)),
        SANDBOXD,
        &[Also::Usage, Also::Exchanges],
    );
    expect![[r#"
        finished, with score Some(Number(6))
        payloads hold: ["xxxxxx"]
        provider.json routes: {"agent":[{"api":"responses","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"}]}
        outcome.json's usage: {"model_requests":6,"ran_ns":"<ns>","sandboxes":[{"class":"sim","count":2,"lasted_ns":"<ns>","provider":"simulated"}],"tokens":{"cached":8,"input":12,"output":5,"reasoning":3,"responses":1}}
        log.jsonl:
        {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":0},"observed_ns":"<ns>","seq":2}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":294,"body_sha256":"8aee411ba0ca2df7c885f4c4ad7c4dd467b8b23df03ff88a02519a4b0f438d48","bytes":272,"dropped_headers":["user-agent"],"items":{"count":3,"field":"input"},"path":"model/0.request"},"response":{"bytes":372,"path":"model/0.response"}},"kind":"model_answered","request":0,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":3}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"barred","message":"the request names `m-huge`, which the route doesn't allow: name `m-large`","status":400},"api":"responses","caller":"agent","kept":{"request":{"body_bytes":166,"body_sha256":"dcfd721111dd302f62bb2078dcfe197c15dda58b0c7a7d865cbb8106e6da33d4","bytes":166,"path":"model/1.request"},"response":{"bytes":153,"path":"model/1.response"}},"kind":"model_answered","request":1,"upstream":"stand-in","upstream_status":null},"observed_ns":"<ns>","seq":4}
        {"actions":[{"kind":"reply","op":0,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":5}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":2},"observed_ns":"<ns>","seq":6}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent","first_byte_ns":"<ns>","first_token_ns":"<ns>","kept":{"request":{"body_bytes":173,"body_sha256":"d78e896c5dfe6db3d4162da33a4aa93e9d38bd52a592d60f6fa718d6a1c655e9","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/2.request"},"response":{"bytes":330,"path":"model/2.response"}},"kind":"model_answered","request":2,"upstream":"stand-in","upstream_status":200,"usage":{"cached_tokens":8,"input_tokens":12,"output_tokens":5,"reasoning_tokens":3}},"observed_ns":"<ns>","seq":7}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":3},"observed_ns":"<ns>","seq":8}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent","failed":{"code":"rate_limit_exceeded"},"first_byte_ns":"<ns>","kept":{"request":{"body_bytes":175,"body_sha256":"97f26708325407223a15d9021152be9dd45fb3afd76092eeda62c06a70c2320a","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/3.request"},"response":{"bytes":281,"path":"model/3.response"}},"kind":"model_answered","request":3,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":9}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":4},"observed_ns":"<ns>","seq":10}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"refused","message":"the model provider gave no answer: try again","status":502},"api":"responses","caller":"agent","kept":{"request":{"body_bytes":174,"body_sha256":"88e3c92bf8546c69eb0ced10c5415fe43c4221081d4953cece017ee6c225e586","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/4.request"},"response":{"bytes":115,"path":"model/4.response"}},"kind":"model_answered","request":4,"upstream":"stand-in","upstream_status":null},"observed_ns":"<ns>","seq":11}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":5},"observed_ns":"<ns>","seq":12}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"refused","message":"the model provider refused the route's key, with a 401, which ends the model route: make no more model requests","status":400},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":175,"body_sha256":"845a5928a0d117a92d395f2dddb92d8073f4ace2518b1a881e88bf10a65e2d85","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/5.request"},"response":{"bytes":7,"path":"model/5.response"}},"kind":"model_answered","request":5,"upstream":"stand-in","upstream_status":401},"observed_ns":"<ns>","seq":13}
        {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":14}
        {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 2 files of 1834 bytes","seq":15}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":16}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":17}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":18}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":19}
        {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":20}
        its last record came before the agent's teardown was to end
        the agent's standard output:
        model request 0: 200 [("content-type", "text/event-stream")] data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"},{"content":[{"text":"\"quoted\"\\back\nline\u0001\ttab/é→😀","type":"input_text"}],"role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
        model request 1: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the request names `m-huge`, which the route doesn't allow: name `m-large`","param":null,"type":"invalid_request_error"}}
        model request 2: 200 [("content-type", "text/event-stream")] data: {"type":"response.created","response":{"usage":null}}\n\ndata: {"type":"response.output_text.delta","delta":"6"}\n\nevent: response.completed\ndata: {"type":"response.completed","response":{"usage":{"input_tokens":12,"input_tokens_details":{"cached_tokens":8},"output_tokens":5,"output_tokens_details":{"reasoning_tokens":3}}}}\n\n
        model request 3: 200 [("content-type", "text/event-stream")] event: response.failed\ndata: {"type":"response.failed","response":{"status":"failed","error":{"code":"rate_limit_exceeded","message":"Rate limit reached"},"usage":null}}\n\nevent: error\ndata: {"type":"error","code":"rate_limit_exceeded","message":"Rate limit reached","param":null}\n\n
        model request 4: 502 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider gave no answer: try again","param":null,"type":"server_error"}}
        model request 5: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider refused the route's key, with a 401, which ends the model route: make no more model requests","param":null,"type":"invalid_request_error"}}
        model exchange 0 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:af497cd3652d5371734347499d7dd2db68a62c28fe107f368d77cff1b78d474a","sha256:55d2a41f5ef0f1adb28a4e2d4420c19e5c857ee46121cc479a8f7bc2fdac819b"],"model":"vendor/m-large","store":false}; response data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"},{"content":[{"text":"\"quoted\"\\back\nline\u0001\ttab/é→😀","type":"input_text"}],"role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
        model exchange 1 kept: request {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"m-huge","store":false}; response {"error":{"code":null,"message":"the request names `m-huge`, which the route doesn't allow: name `m-large`","param":null,"type":"invalid_request_error"}}
        model exchange 2 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:6bc0b5dcffbd963414c779a049374557aba7edf0b7655c06cfd15b83b4901d9d"],"model":"vendor/m-large","store":false}; response data: {"type":"response.created","response":{"usage":null}}\n\ndata: {"type":"response.output_text.delta","delta":"6"}\n\nevent: response.completed\ndata: {"type":"response.completed","response":{"usage":{"input_tokens":12,"input_tokens_details":{"cached_tokens":8},"output_tokens":5,"output_tokens_details":{"reasoning_tokens":3}}}}\n\n
        model exchange 3 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:7b6c4b476ddef378a1d5d3f8d8aede1f77d78e9f85b27ee071a96779b90da7df"],"model":"vendor/m-large","store":false}; response event: response.failed\ndata: {"type":"response.failed","response":{"status":"failed","error":{"code":"rate_limit_exceeded","message":"Rate limit reached"},"usage":null}}\n\nevent: error\ndata: {"type":"error","code":"rate_limit_exceeded","message":"Rate limit reached","param":null}\n\n
        model exchange 4 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:ab7db161254d2c7471c643168516a57af7f7841c5b6613b468ba76fd09cd4ba6"],"model":"vendor/m-large","store":false}; response {"error":{"code":null,"message":"the model provider gave no answer: try again","param":null,"type":"server_error"}}
        model exchange 5 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:6752f5d96dfccaffbf0772742e4557281e807f48cc031258473eb59fb6390c57"],"model":"vendor/m-large","store":false}; response refused
        model items kept: 7
        the model endpoint at each create: [true, false]"#]]
    .assert_eq(&shown);
}

// Each API's tools, and the Responses API's instructions, are kept once whole; the Chat
// request's `instructions` is no field its API keeps whole.
#[test]
#[expect(clippy::too_many_lines, reason = "most of it is its snapshot")]
fn model_requests_kept_whole() {
    // A client's own text that names the record's first item, which a rebuild must leave as sent.
    let first = "sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8";
    let messages = format!(
        r#"[{{"role":"user","content":"x"}},{{"role":"user","content":"stream"}},"{first}"]"#
    );
    let tools = r#"{"tools":[{"type":"function","name":"exec","parameters":{"type":"object"}}],"instructions":"Be\nbrief\u00e9"}"#;
    let argv = [
        "ops",
        &format!("model m-large stream {tools}"),
        &format!("model m-large stream again {tools}"),
        r#"model m-large stream {"tools":[{"name":"exec","type":"function"},{"name":"read","type":"function"}]}"#,
        "model m-large stream",
        r#"model m-large stream {"tools":[],"instructions":null}"#,
        r#"chat m-large stream {"tools":[{"type":"function","function":{"name":"exec"}}],"instructions":"x"}"#,
        r#"model m-large stream {"input":"stream","tools":[{"name":"exec","type":"function"}]}"#,
        &format!(r#"chat m-large stream {{"tools":"{first}","messages":{messages}}}"#),
        "finalize submit 6",
    ];
    let mut ir = routed(ops_workflow(&argv, 30));
    let chat = json!({ "api": "chat", "models": ["m-large"] });
    ir["roles"][0]["egress"]["model"]
        .as_array_mut()
        .unwrap()
        .push(chat);
    let shown = executed(
        &System,
        "model-requests-kept-whole",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &[Also::Exchanges],
    );
    expect![[r#"
        finished, with score Some(Number(6))
        payloads hold: ["xxxxxx"]
        provider.json routes: {"agent":[{"api":"responses","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"},{"api":"chat","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"}]}
        log.jsonl:
        {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":0},"observed_ns":"<ns>","seq":2}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":278,"body_sha256":"4a0e2a217ab42d2918ccf10d857bb512cd2c0071f1903cfb2fc3d8069e8d6490","bytes":369,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/0.request","whole":["tools","instructions"]},"response":{"bytes":356,"path":"model/0.response"}},"kind":"model_answered","request":0,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":3}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":1},"observed_ns":"<ns>","seq":4}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":360,"body_sha256":"3f8893c949082a78b81b69e0b8c16a9cbb69a2fe2a447efc11ce3463eede3a32","bytes":443,"dropped_headers":["user-agent"],"items":{"count":3,"field":"input"},"path":"model/1.request","whole":["tools","instructions"]},"response":{"bytes":438,"path":"model/1.response"}},"kind":"model_answered","request":1,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":5}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":2},"observed_ns":"<ns>","seq":6}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":252,"body_sha256":"2e435321c933c5ff53fd926b770e3a78d618808c9e1822e1ab90a4b7c776a44d","bytes":280,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/2.request","whole":["tools"]},"response":{"bytes":330,"path":"model/2.response"}},"kind":"model_answered","request":2,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":7}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":3},"observed_ns":"<ns>","seq":8}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":174,"body_sha256":"8d6175ce51c5e9942c5037737b41cabe7120f8a38f746b122528340427199599","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/3.request"},"response":{"bytes":252,"path":"model/3.response"}},"kind":"model_answered","request":3,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":9}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":4},"observed_ns":"<ns>","seq":10}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":205,"body_sha256":"23a7b1aff92dc20f820007c4302e3e23ca538ba4ee347b37287e306dbdba6110","bytes":369,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/4.request","whole":["tools","instructions"]},"response":{"bytes":283,"path":"model/4.response"}},"kind":"model_answered","request":4,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":11}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":5},"observed_ns":"<ns>","seq":12}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"api":"chat","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":239,"body_sha256":"ceca32bd90c85a16a90d02d5fdf3cb840e5c10db5c3a050e1e0c6aa4c269b15b","bytes":288,"items":{"count":2,"field":"messages"},"path":"model/5.request","whole":["tools"]},"response":{"bytes":324,"path":"model/5.response"}},"kind":"model_answered","request":5,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":13}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":6},"observed_ns":"<ns>","seq":14}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":101,"body_sha256":"c2c8b73765a171cc435ac7ea3093cc46c6e5d9fea831d802cebb7a176232fd41","bytes":139,"dropped_headers":["user-agent"],"path":"model/6.request","whole":["tools"]},"response":{"bytes":179,"path":"model/6.response"}},"kind":"model_answered","request":6,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":15}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":7},"observed_ns":"<ns>","seq":16}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"api":"chat","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":260,"body_sha256":"557137cfb765ee826b9fbabe67c1c2bd112ef844bab0a14e281b21175d674c56","bytes":343,"items":{"count":3,"field":"messages"},"path":"model/7.request","whole":["tools"]},"response":{"bytes":345,"path":"model/7.response"}},"kind":"model_answered","request":7,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":17}
        {"actions":[{"kind":"reply","op":0,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":18}
        {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":19}
        {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 2 files of 3027 bytes","seq":20}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":21}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":22}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":23}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":24}
        {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":25}
        its last record came before the agent's teardown was to end
        the agent's standard output:
        model request 0: 200 [("content-type", "text/event-stream")] data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"instructions":"Be\nbriefé","model":"vendor/m-large","store":false,"tools":[{"name":"exec","parameters":{"type":"object"},"type":"function"}]}\n\n
        model request 1: 200 [("content-type", "text/event-stream")] data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"},{"content":[{"text":"again","type":"input_text"}],"role":"user","type":"message"}],"instructions":"Be\nbriefé","model":"vendor/m-large","store":false,"tools":[{"name":"exec","parameters":{"type":"object"},"type":"function"}]}\n\n
        model request 2: 200 [("content-type", "text/event-stream")] data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false,"tools":[{"name":"exec","type":"function"},{"name":"read","type":"function"}]}\n\n
        model request 3: 200 [("content-type", "text/event-stream")] data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
        model request 4: 200 [("content-type", "text/event-stream")] data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"instructions":null,"model":"vendor/m-large","store":false,"tools":[]}\n\n
        model request 5: 200 [("content-type", "text/event-stream")] data: POST /v1/chat/completions HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"instructions":"x","messages":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","tools":[{"function":{"name":"exec"},"type":"function"}]}\n\n
        model request 6: 200 [("content-type", "text/event-stream")] data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":"stream","model":"vendor/m-large","store":false,"tools":[{"name":"exec","type":"function"}]}\n\n
        model request 7: 200 [("content-type", "text/event-stream")] data: POST /v1/chat/completions HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"messages":[{"content":"x","role":"user"},{"content":"stream","role":"user"},"sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8"],"model":"vendor/m-large","tools":"sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8"}\n\n
        model exchange 0 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:af497cd3652d5371734347499d7dd2db68a62c28fe107f368d77cff1b78d474a"],"instructions":"sha256:583859be4b342c7b1e052c0557e4e88d2b9aba0df82b98a25a568381ad5f9e5e","model":"vendor/m-large","store":false,"tools":"sha256:9483eb23a7a4d95bd0d21b307adced4984f98543a2fd3bd651cd3ed5c8de7132"}; response data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"instructions":"Be\nbriefé","model":"vendor/m-large","store":false,"tools":[{"name":"exec","parameters":{"type":"object"},"type":"function"}]}\n\n
        model exchange 1 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:af497cd3652d5371734347499d7dd2db68a62c28fe107f368d77cff1b78d474a","sha256:a7815ca180a8c39f0e9726ab16920a802409c4043e5ebe63bbb2eceeb9555f70"],"instructions":"sha256:583859be4b342c7b1e052c0557e4e88d2b9aba0df82b98a25a568381ad5f9e5e","model":"vendor/m-large","store":false,"tools":"sha256:9483eb23a7a4d95bd0d21b307adced4984f98543a2fd3bd651cd3ed5c8de7132"}; response data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"},{"content":[{"text":"again","type":"input_text"}],"role":"user","type":"message"}],"instructions":"Be\nbriefé","model":"vendor/m-large","store":false,"tools":[{"name":"exec","parameters":{"type":"object"},"type":"function"}]}\n\n
        model exchange 2 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:af497cd3652d5371734347499d7dd2db68a62c28fe107f368d77cff1b78d474a"],"model":"vendor/m-large","store":false,"tools":"sha256:4b70428ef88f817436eed31a54cc2a1e59f859311150c3d98390fc8f15590586"}; response data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false,"tools":[{"name":"exec","type":"function"},{"name":"read","type":"function"}]}\n\n
        model exchange 3 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:af497cd3652d5371734347499d7dd2db68a62c28fe107f368d77cff1b78d474a"],"model":"vendor/m-large","store":false}; response data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
        model exchange 4 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:af497cd3652d5371734347499d7dd2db68a62c28fe107f368d77cff1b78d474a"],"instructions":"sha256:74234e98afe7498fb5daf1f36ac2d78acc339464f950703b8c019892f982b90b","model":"vendor/m-large","store":false,"tools":"sha256:4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945"}; response data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"instructions":null,"model":"vendor/m-large","store":false,"tools":[]}\n\n
        model exchange 5 kept: request {"instructions":"x","messages":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:af497cd3652d5371734347499d7dd2db68a62c28fe107f368d77cff1b78d474a"],"model":"vendor/m-large","tools":"sha256:0e9667d2a9716a8886b75add0c6589e284eece62e64126ce6a7f2e9e7fd188e7"}; response data: POST /v1/chat/completions HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"instructions":"x","messages":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","tools":[{"function":{"name":"exec"},"type":"function"}]}\n\n
        model exchange 6 kept: request {"input":"stream","model":"vendor/m-large","store":false,"tools":"sha256:958336fece1a916e68bffa23846795ce4d234f2a8310b556ca637bb95d2395f9"}; response data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":"stream","model":"vendor/m-large","store":false,"tools":[{"name":"exec","type":"function"}]}\n\n
        model exchange 7 kept: request {"messages":["sha256:4df15c38b8085060d347df44ed3a3aadb3eb6bd77f36d54e614019cbb87cbcc9","sha256:f777ce961958f1e9e82f1448192e69f6d59b271e35b904e5eb1ad6b2ed953505","sha256:bef22a4190e8fa397a4f2ec3b23e280befb118e56b8d394569e8d359060c81ed"],"model":"vendor/m-large","tools":"sha256:bef22a4190e8fa397a4f2ec3b23e280befb118e56b8d394569e8d359060c81ed"}; response data: POST /v1/chat/completions HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"messages":[{"content":"x","role":"user"},{"content":"stream","role":"user"},"sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8"],"model":"vendor/m-large","tools":"sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8"}\n\n
        model items kept: 13
        the model endpoint at each create: [true, false]"#]].assert_eq(&shown);
}

#[test]
fn model_request_rebuilt_from_changed_item() {
    let shown = executed(
        &System,
        "model-request-rebuilt-from-changed-item",
        &[(PROVIDER, &Sim::new())],
        &routed(ops_workflow(
            &["ops", "model m-large stream", "finalize submit 1"],
            30,
        )),
        SANDBOXD,
        &[Also::ChangedItem],
    );
    let rebuilt: Vec<&str> = (shown.lines())
        .filter(|line| line.starts_with("request 0 "))
        .collect();
    expect![[r#"
        request 0 rebuilt once an item is changed: model request 0: the record's items don't give back the body it sent: the rebuilt body's SHA-256 is 0207f9d9601a5a39e4cbae9cd1c4b88395f59014a0005b7116f7042d26b20920, where the run log names 8d6175ce51c5e9942c5037737b41cabe7120f8a38f746b122528340427199599
        request 0 read from a damaged log: the run log names the kept file `/etc/hostname`: expected a path below the record
        request 0 read from a damaged log: the run log names the kept file `/etc/hostname`: expected a path below the record
        request 0 read from a damaged log: the run log names the kept file `model/../../0.response`: expected a path below the record
        request 0 read from a damaged log: the run log's `kept.request` is "model/0.request": expected an object"#]].assert_eq(&rebuilt.join("\n"));
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
    let shown = executed(
        &System,
        "model-upstream-refusals",
        &[(PROVIDER, &Sim::new())],
        &routed(ops_workflow(&argv, 30)),
        SANDBOXD,
        &[Also::Exchanges],
    );
    expect![[r#"
        failed: the upstream `stand-in` refused its key, with a 401, which ended the agent's model route: give it a key that it takes, and run again
        payloads hold: ["xxxxxx"]
        provider.json routes: {"agent":[{"api":"responses","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"}]}
        log.jsonl:
        {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":0},"observed_ns":"<ns>","seq":2}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"refused","message":"the model provider refused the request, with a 403: send only requests that it allows","status":400},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":177,"body_sha256":"078fbc89df4550b236db1eb91ce291d85f3251b70680c94bd74057dcaa7f2644","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/0.request"},"response":{"bytes":7,"path":"model/0.response"}},"kind":"model_answered","request":0,"upstream":"stand-in","upstream_status":403},"observed_ns":"<ns>","seq":3}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":1},"observed_ns":"<ns>","seq":4}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":174,"body_sha256":"8d6175ce51c5e9942c5037737b41cabe7120f8a38f746b122528340427199599","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/1.request"},"response":{"bytes":252,"path":"model/1.response"}},"kind":"model_answered","request":1,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":5}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":2},"observed_ns":"<ns>","seq":6}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"refused","message":"the model provider refused the route's key, with a 401, which ends the model route: make no more model requests","status":400},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":175,"body_sha256":"845a5928a0d117a92d395f2dddb92d8073f4ace2518b1a881e88bf10a65e2d85","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/2.request"},"response":{"bytes":7,"path":"model/2.response"}},"kind":"model_answered","request":2,"upstream":"stand-in","upstream_status":401},"observed_ns":"<ns>","seq":7}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"barred","message":"the model provider refused the route's key, with a 401, which ends the model route: make no more model requests","status":400},"api":"responses","caller":"agent","kept":{"request":{"body_bytes":167,"body_sha256":"6d425404d88037d2c75db5fa6013365efa76ccf6f8ac1b5bb4f9f44fe74d0a8e","bytes":167,"path":"model/3.request"},"response":{"bytes":191,"path":"model/3.response"}},"kind":"model_answered","request":3,"upstream":"stand-in","upstream_status":null},"observed_ns":"<ns>","seq":8}
        {"actions":[{"kind":"reply","op":0,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":9}
        {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":10}
        {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 2 files of 1054 bytes","seq":11}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":12}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":13}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":14}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":15}
        {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":16}
        {"at_ns":"<ns>","note":"the run failed: the upstream `stand-in` refused its key, with a 401, which ended the agent's model route: give it a key that it takes, and run again","seq":17}
        its last record came before the agent's teardown was to end
        the agent's standard output:
        model request 0: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider refused the request, with a 403: send only requests that it allows","param":null,"type":"invalid_request_error"}}
        model request 1: 200 [("content-type", "text/event-stream")] data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
        model request 2: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider refused the route's key, with a 401, which ends the model route: make no more model requests","param":null,"type":"invalid_request_error"}}
        model request 3: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider refused the route's key, with a 401, which ends the model route: make no more model requests","param":null,"type":"invalid_request_error"}}
        model exchange 0 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:8b4bb900d130b1b6642e6f25e0464280105b882a02c27bd9e008556adbe2d34f"],"model":"vendor/m-large","store":false}; response refused
        model exchange 1 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:af497cd3652d5371734347499d7dd2db68a62c28fe107f368d77cff1b78d474a"],"model":"vendor/m-large","store":false}; response data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
        model exchange 2 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:6752f5d96dfccaffbf0772742e4557281e807f48cc031258473eb59fb6390c57"],"model":"vendor/m-large","store":false}; response refused
        model exchange 3 kept: request {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"m-large","store":false}; response {"error":{"code":null,"message":"the model provider refused the route's key, with a 401, which ends the model route: make no more model requests","param":null,"type":"invalid_request_error"}}
        model items kept: 4
        the model endpoint at each create: [true, false]"#]]
    .assert_eq(&shown);
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
    let shown = executed(
        &System,
        "model-provider-down",
        &[(PROVIDER, &Sim::new())],
        &routed(ops_workflow(&argv, 30)),
        SANDBOXD,
        &[Also::Exchanges],
    );
    expect![[r#"
        failed: the upstream `stand-in` failed the agent's last model request, 2 (502; failing since request 1), and the agent exited: a deployment fault, not the agent's result; run it again
        provider.json routes: {"agent":[{"api":"responses","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"}]}
        log.jsonl:
        {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":0},"observed_ns":"<ns>","seq":2}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":174,"body_sha256":"8d6175ce51c5e9942c5037737b41cabe7120f8a38f746b122528340427199599","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/0.request"},"response":{"bytes":252,"path":"model/0.response"}},"kind":"model_answered","request":0,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":3}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":1},"observed_ns":"<ns>","seq":4}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":172,"body_sha256":"dc978248f6a10cf2405c2f1144b54026073188a2ef8072f5b52280326be5f330","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/1.request"},"response":{"bytes":7,"path":"model/1.response"}},"kind":"model_answered","request":1,"request_id":"req-502","upstream":"stand-in","upstream_status":502},"observed_ns":"<ns>","seq":5}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":2},"observed_ns":"<ns>","seq":6}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":172,"body_sha256":"dc978248f6a10cf2405c2f1144b54026073188a2ef8072f5b52280326be5f330","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/2.request"},"response":{"bytes":7,"path":"model/2.response"}},"kind":"model_answered","request":2,"request_id":"req-502","upstream":"stand-in","upstream_status":502},"observed_ns":"<ns>","seq":7}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"barred","message":"the request names `m-huge`, which the route doesn't allow: name `m-large`","status":400},"api":"responses","caller":"agent","kept":{"request":{"body_bytes":166,"body_sha256":"dcfd721111dd302f62bb2078dcfe197c15dda58b0c7a7d865cbb8106e6da33d4","bytes":166,"path":"model/3.request"},"response":{"bytes":153,"path":"model/3.response"}},"kind":"model_answered","request":3,"upstream":"stand-in","upstream_status":null},"observed_ns":"<ns>","seq":8}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":4},"observed_ns":"<ns>","seq":9}
        {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":10}
        {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 2 files of 809 bytes","seq":11}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":12}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"cut"},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":173,"body_sha256":"47bf6d5b1bdebfa49b0689b8f69a5f19e3d3ae113fde67c00dd2d2edb81ec513","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/4.request"},"response":{"bytes":25,"path":"model/4.response"}},"kind":"model_answered","request":4,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":13}
        {"at_ns":"<ns>","note":"the run failed: the upstream `stand-in` failed the agent's last model request, 2 (502; failing since request 1), and the agent exited: a deployment fault, not the agent's result; run it again","seq":14}
        its last record came before the agent's teardown was to end
        the agent's standard output:
        model request 0: 200 [("content-type", "text/event-stream")] data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
        model request 1: 502 [("content-type", "text/plain"), ("x-request-id", "req-502")] refused
        model request 2: 502 [("content-type", "text/plain"), ("x-request-id", "req-502")] refused
        model request 3: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the request names `m-huge`, which the route doesn't allow: name `m-large`","param":null,"type":"invalid_request_error"}}
        model request 4: 200 [("content-type", "text/event-stream")] event: response.created\n\n
        model exchange 0 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:af497cd3652d5371734347499d7dd2db68a62c28fe107f368d77cff1b78d474a"],"model":"vendor/m-large","store":false}; response data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
        model exchange 1 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:ebd80fcd992f7a45a6c58098f79ce103ace84aef8b2530a94481e55202c29f67"],"model":"vendor/m-large","store":false}; response refused
        model exchange 2 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:ebd80fcd992f7a45a6c58098f79ce103ace84aef8b2530a94481e55202c29f67"],"model":"vendor/m-large","store":false}; response refused
        model exchange 3 kept: request {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"m-huge","store":false}; response {"error":{"code":null,"message":"the request names `m-huge`, which the route doesn't allow: name `m-large`","param":null,"type":"invalid_request_error"}}
        model exchange 4 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:8b740cd5d26befcc0dbb835cfb3849a3cdd15f134987d273e263503033017963"],"model":"vendor/m-large","store":false}; response event: response.created\n\n
        model items kept: 4
        the model endpoint at each create: [true]
        a stalled stream's client hung up before the agent's teardown was to end"#]]
    .assert_eq(&shown);
}

#[test]
fn model_barred_is_no_failure() {
    let argv = ["ops", "model m-large stream", "model m-huge stream", "exit"];
    let shown = executed(
        &System,
        "model-barred",
        &[(PROVIDER, &Sim::new())],
        &routed(ops_workflow(&argv, 30)),
        SANDBOXD,
        &[Also::Exchanges],
    );
    expect![[r#"
        finished, with score None
        provider.json routes: {"agent":[{"api":"responses","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"}]}
        log.jsonl:
        {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":0},"observed_ns":"<ns>","seq":2}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":174,"body_sha256":"8d6175ce51c5e9942c5037737b41cabe7120f8a38f746b122528340427199599","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/0.request"},"response":{"bytes":252,"path":"model/0.response"}},"kind":"model_answered","request":0,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":3}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"barred","message":"the request names `m-huge`, which the route doesn't allow: name `m-large`","status":400},"api":"responses","caller":"agent","kept":{"request":{"body_bytes":166,"body_sha256":"dcfd721111dd302f62bb2078dcfe197c15dda58b0c7a7d865cbb8106e6da33d4","bytes":166,"path":"model/1.request"},"response":{"bytes":153,"path":"model/1.response"}},"kind":"model_answered","request":1,"upstream":"stand-in","upstream_status":null},"observed_ns":"<ns>","seq":4}
        {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":5}
        {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 2 files of 538 bytes","seq":6}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":7}
        its last record came before the agent's teardown was to end
        the agent's standard output:
        model request 0: 200 [("content-type", "text/event-stream")] data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
        model request 1: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the request names `m-huge`, which the route doesn't allow: name `m-large`","param":null,"type":"invalid_request_error"}}
        model exchange 0 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:af497cd3652d5371734347499d7dd2db68a62c28fe107f368d77cff1b78d474a"],"model":"vendor/m-large","store":false}; response data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
        model exchange 1 kept: request {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"m-huge","store":false}; response {"error":{"code":null,"message":"the request names `m-huge`, which the route doesn't allow: name `m-large`","param":null,"type":"invalid_request_error"}}
        model items kept: 2
        the model endpoint at each create: [true]"#]]
    .assert_eq(&shown);
}

#[test]
fn model_provider_recovers() {
    let argv = ["ops", "model m-large down", "model m-large stream"];
    let shown = executed(
        &System,
        "model-provider-recovers",
        &[(PROVIDER, &Sim::new())],
        &routed(ops_workflow(&argv, 30)),
        SANDBOXD,
        &[Also::Exchanges],
    );
    expect![[r#"
        finished, with score None
        provider.json routes: {"agent":[{"api":"responses","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"}]}
        log.jsonl:
        {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":0},"observed_ns":"<ns>","seq":2}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":172,"body_sha256":"dc978248f6a10cf2405c2f1144b54026073188a2ef8072f5b52280326be5f330","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/0.request"},"response":{"bytes":7,"path":"model/0.response"}},"kind":"model_answered","request":0,"request_id":"req-502","upstream":"stand-in","upstream_status":502},"observed_ns":"<ns>","seq":3}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":1},"observed_ns":"<ns>","seq":4}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":174,"body_sha256":"8d6175ce51c5e9942c5037737b41cabe7120f8a38f746b122528340427199599","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/1.request"},"response":{"bytes":252,"path":"model/1.response"}},"kind":"model_answered","request":1,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":5}
        {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":6}
        {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 2 files of 415 bytes","seq":7}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":8}
        its last record came before the agent's teardown was to end
        the agent's standard output:
        model request 0: 502 [("content-type", "text/plain"), ("x-request-id", "req-502")] refused
        model request 1: 200 [("content-type", "text/event-stream")] data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
        model exchange 0 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:ebd80fcd992f7a45a6c58098f79ce103ace84aef8b2530a94481e55202c29f67"],"model":"vendor/m-large","store":false}; response refused
        model exchange 1 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:af497cd3652d5371734347499d7dd2db68a62c28fe107f368d77cff1b78d474a"],"model":"vendor/m-large","store":false}; response data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
        model items kept: 3
        the model endpoint at each create: [true]"#]]
    .assert_eq(&shown);
}

#[test]
fn model_provider_down_at_the_deadline() {
    let argv = [
        "ops",
        "submit submit 6",
        "model m-large down",
        "model m-large stall",
    ];
    let shown = executed(
        &System,
        "model-provider-down-at-the-deadline",
        &[(PROVIDER, &Sim::new())],
        &routed(short_workflow(&argv, 2)),
        SANDBOXD,
        &[Also::Exchanges],
    );
    expect![[r#"
        finished, with score Some(Number(6))
        payloads hold: ["xxxxxx"]
        provider.json routes: {"agent":[{"api":"responses","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"}]}
        log.jsonl:
        {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"93000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
        {"actions":[{"kind":"reply","op":0,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"stored"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"finalize":false,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":2}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":0},"observed_ns":"<ns>","seq":3}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":172,"body_sha256":"dc978248f6a10cf2405c2f1144b54026073188a2ef8072f5b52280326be5f330","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/0.request"},"response":{"bytes":7,"path":"model/0.response"}},"kind":"model_answered","request":0,"request_id":"req-502","upstream":"stand-in","upstream_status":502},"observed_ns":"<ns>","seq":4}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":1},"observed_ns":"<ns>","seq":5}
        {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"tick"},"observed_ns":"<ns>","seq":6}
        {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 2 files of 188 bytes","seq":7}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":8}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"cut"},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":173,"body_sha256":"47bf6d5b1bdebfa49b0689b8f69a5f19e3d3ae113fde67c00dd2d2edb81ec513","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/1.request"},"response":{"bytes":25,"path":"model/1.response"}},"kind":"model_answered","request":1,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":9}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":10}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":11}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":12}
        {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":false,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"deadline"}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":13}
        its last record came before the agent's teardown was to end
        the agent's standard output:
        model request 0: 502 [("content-type", "text/plain"), ("x-request-id", "req-502")] refused
        model request 1: 200 [("content-type", "text/event-stream")] event: response.created\n\n
        model exchange 0 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:ebd80fcd992f7a45a6c58098f79ce103ace84aef8b2530a94481e55202c29f67"],"model":"vendor/m-large","store":false}; response refused
        model exchange 1 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:8b740cd5d26befcc0dbb835cfb3849a3cdd15f134987d273e263503033017963"],"model":"vendor/m-large","store":false}; response event: response.created\n\n
        model items kept: 3
        the model endpoint at each create: [true, false]
        a stalled stream's client hung up before the agent's teardown was to end"#]]
    .assert_eq(&shown);
}

#[test]
fn model_stream_cut() {
    let argv = ["ops", "submit submit slow", "model m-large stall"];
    let shown = executed(
        &System,
        "model-stream-cut",
        &[(PROVIDER, &Sim::new())],
        &routed(short_workflow(&argv, 1)),
        SANDBOXD,
        &[Also::Exchanges],
    );
    expect![[r#"
        finished, with score None
        payloads hold: ["slow"]
        provider.json routes: {"agent":[{"api":"responses","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"}]}
        log.jsonl:
        {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"92000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
        {"actions":[{"kind":"reply","op":0,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"stored"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"finalize":false,"grant":"submit","kind":"submit","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4}}},"observed_ns":"<ns>","seq":2}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":0},"observed_ns":"<ns>","seq":3}
        {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"tick"},"observed_ns":"<ns>","seq":4}
        {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 2 files of 97 bytes","seq":5}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":6}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"cut"},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":173,"body_sha256":"47bf6d5b1bdebfa49b0689b8f69a5f19e3d3ae113fde67c00dd2d2edb81ec513","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/0.request"},"response":{"bytes":25,"path":"model/0.response"}},"kind":"model_answered","request":0,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":7}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":8}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_timed_out"},"observed_ns":"<ns>","seq":9}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":10}
        {"actions":[{"kind":"finish","outcome":{"score":null,"verdict":{"failure":{"kind":"timeout"},"kind":"no_result","submission":{"finalized":false,"payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4}}},"work_end":{"kind":"deadline"}}}],"at_ns":"<ns>","event":{"invalid":"the judge wrote no result: write it at /pumpkin/out/result.json","job":0,"kind":"judge_collected"},"observed_ns":"<ns>","seq":11}
        the final judge's standard error, as `wait` finds it: Ok("jobs/0/log/command.stderr")
        its last record came before the agent's teardown was to end
        the agent's standard output:
        model request 0: 200 [("content-type", "text/event-stream")] event: response.created\n\n
        model exchange 0 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:8b740cd5d26befcc0dbb835cfb3849a3cdd15f134987d273e263503033017963"],"model":"vendor/m-large","store":false}; response event: response.created\n\n
        model items kept: 2
        the model endpoint at each create: [true, false]
        a stalled stream's client hung up before the agent's teardown was to end"#]]
    .assert_eq(&shown);
}

#[test]
fn model_grace_cut() {
    let argv = ["ops", "ask m-large stall", "finalize submit 6"];
    let mut ir = routed(ops_workflow(&argv, 30));
    // So that the grace runs out while the agent still waits on the stalled stream.
    ir["roles"][0]["runtime"]["agent"]["finalize_grace_seconds"] = json!(1);
    let shown = executed(
        &System,
        "model-grace-cut",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &[Also::Exchanges],
    );
    expect![[r#"
        finished, with score Some(Number(6))
        payloads hold: ["xxxxxx"]
        provider.json routes: {"agent":[{"api":"responses","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"}]}
        log.jsonl:
        {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"241000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":0},"observed_ns":"<ns>","seq":2}
        {"actions":[{"kind":"reply","op":0,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":3}
        {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"tick"},"observed_ns":"<ns>","seq":4}
        {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 2 files of 97 bytes","seq":5}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":6}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"cut"},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":173,"body_sha256":"47bf6d5b1bdebfa49b0689b8f69a5f19e3d3ae113fde67c00dd2d2edb81ec513","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/0.request"},"response":{"bytes":25,"path":"model/0.response"}},"kind":"model_answered","request":0,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":7}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":8}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":9}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":10}
        {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"finalized"}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":11}
        its last record came before the agent's teardown was to end
        the agent's standard output:
        model request 0: 200 [("content-type", "text/event-stream")] event: response.created\n\n
        model exchange 0 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:8b740cd5d26befcc0dbb835cfb3849a3cdd15f134987d273e263503033017963"],"model":"vendor/m-large","store":false}; response event: response.created\n\n
        model items kept: 2
        the model endpoint at each create: [true, false]
        a stalled stream's client hung up before the agent's teardown was to end"#]]
    .assert_eq(&shown);
}

/// `ir`, with a model route for judge `judge`, 1 for the train judge or 2 for the score's, that
/// allows `m-small`, whose command takes `steps` (`ops`), and a timeout of 20 s for each of the
/// train judge's calls, shorter than what the agent's deadline leaves any call.
fn judge_routed(mut ir: Value, judge: usize, steps: &[&str]) -> Value {
    let route = json!([{ "api": "responses", "models": ["m-small"] }]);
    ir["roles"][judge]["egress"] = json!({ "model": route });
    let argv: Vec<&str> = iter::once("ops").chain(steps.iter().copied()).collect();
    ir["roles"][judge]["runtime"]["judge"]["command"]["argv"] = json!(argv);
    ir["roles"][0]["grants"][0]["judge"]["call_timeout_seconds"] = json!(20);
    ir
}

#[test]
fn judge_model() {
    let argv = ["ops", "model m-large stream", "finalize submit 6"];
    let ir = judge_routed(
        routed(ops_workflow(&argv, 30)),
        2,
        &["model m-small 7", "result"],
    );
    let shown = executed(
        &System,
        "judge-model",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &[Also::Usage, Also::Exchanges],
    );
    expect![[r#"
        finished, with score Some(Number(7))
        payloads hold: ["xxxxxx"]
        provider.json routes: {"agent":[{"api":"responses","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"}],"final_judge":[{"api":"responses","models":{"m-small":{"upstream_name":"vendor/m-small"}},"upstream":"stand-in"}]}
        outcome.json's usage: {"calls":[{"grant":null,"job":0,"model_requests":1,"tokens":{"input":0,"output":0,"responses":0}}],"model_requests":2,"ran_ns":"<ns>","sandboxes":[{"class":"sim","count":2,"lasted_ns":"<ns>","provider":"simulated"}],"tokens":{"input":0,"output":0,"responses":0}}
        log.jsonl:
        {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":0},"observed_ns":"<ns>","seq":2}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":174,"body_sha256":"8d6175ce51c5e9942c5037737b41cabe7120f8a38f746b122528340427199599","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/0.request"},"response":{"bytes":252,"path":"model/0.response"}},"kind":"model_answered","request":0,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":3}
        {"actions":[{"kind":"reply","op":0,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":4}
        {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":5}
        {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 2 files of 324 bytes","seq":6}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":7}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":8}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"job-0","kind":"model_request_started","request":1},"observed_ns":"<ns>","seq":9}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"job-0","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":169,"body_sha256":"c538063991a5eaec390bb3b71b775b030d5b5fa2cafef2f66f043eedfdc8297f","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/1.request"},"response":{"bytes":1,"path":"model/1.response"}},"kind":"model_answered","request":1,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":10}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":11}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":12}
        {"actions":[{"kind":"finish","outcome":{"score":7,"verdict":{"kind":"judged","result":{"returned":{"raw_score":7},"score":7},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"returned":{"raw_score":7},"score":7}},"observed_ns":"<ns>","seq":13}
        its last record came before the agent's teardown was to end
        the agent's standard output:
        model request 0: 200 [("content-type", "text/event-stream")] data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
        job 0's standard output:
        model request 0: 200 [("content-type", "text/plain")] 7
        model exchange 0 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:af497cd3652d5371734347499d7dd2db68a62c28fe107f368d77cff1b78d474a"],"model":"vendor/m-large","store":false}; response data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
        model exchange 1 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:5559c1b7a436c81115685858ea5a05bca66397f56a0d93101977bad52de50aea"],"model":"vendor/m-small","store":false}; response 7
        model items kept: 3
        the model endpoint at each create: [true, true]"#]]
    .assert_eq(&shown);
}

// The call's route ends with its command, so its request in flight is cut short, while the
// agent's stream goes on.
#[test]
fn judge_and_agent_model() {
    let argv = [
        "ops",
        "ask m-large stall",
        "judge train 3",
        "await 0 completed",
        "submit submit 6",
        "exit",
    ];
    let steps = ["model m-small 5", "result", "ask m-small stall", "exit"];
    let ir = judge_routed(routed(ops_workflow(&argv, 30)), 1, &steps);
    let shown = executed(
        &System,
        "judge-and-agent-model",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &[Also::Usage, Also::Exchanges],
    );
    expect![[r#"
        finished, with score Some(Number(6))
        payloads hold: ["xxx", "xxxxxx"]
        provider.json routes: {"agent":[{"api":"responses","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"}],"train_judge":[{"api":"responses","models":{"m-small":{"upstream_name":"vendor/m-small"}},"upstream":"stand-in"}]}
        outcome.json's usage: {"calls":[{"grant":"train","job":0,"model_requests":2,"tokens":{"input":0,"output":0,"responses":0}}],"model_requests":3,"ran_ns":"<ns>","sandboxes":[{"class":"sim","count":3,"lasted_ns":"<ns>","provider":"simulated"}],"tokens":{"input":0,"output":0,"responses":0}}
        log.jsonl:
        {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent","kind":"model_request_started","request":0},"observed_ns":"<ns>","seq":2}
        {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:cd2eb0837c9b4c962c22d2ff8b5441b7b45805887f051d39bf133b583baf6860","size":3},"timeout_ns":"20000000000"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:cd2eb0837c9b4c962c22d2ff8b5441b7b45805887f051d39bf133b583baf6860","size":3}}},"observed_ns":"<ns>","seq":3}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":4}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"job-0","kind":"model_request_started","request":1},"observed_ns":"<ns>","seq":5}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"job-0","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":169,"body_sha256":"0687d2985455096f15d6a4404ef14584b391c7b3d31d6355df3fbfda36a3a57a","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/1.request"},"response":{"bytes":1,"path":"model/1.response"}},"kind":"model_answered","request":1,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":6}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"job-0","kind":"model_request_started","request":2},"observed_ns":"<ns>","seq":7}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":8}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"cut"},"api":"responses","caller":"job-0","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":173,"body_sha256":"ec0d3ba6df672edb7aa80d93ee65128ce79f7ebf9cd6691b1adefef48fed5c2b","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/2.request"},"response":{"bytes":25,"path":"model/2.response"}},"kind":"model_answered","request":2,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":9}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":10}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"returned":{"raw_score":5},"score":5}},"observed_ns":"<ns>","seq":11}
        {"actions":[{"kind":"reply","op":1,"reply":{"job":0,"kind":"status","remaining":{"calls":99,"seconds":null},"returned":{"raw_score":5},"scored":true,"state":"completed"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":1,"request":{"job":"0","kind":"status"}},"observed_ns":"<ns>","seq":12}
        {"actions":[{"kind":"reply","op":2,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"stored"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":2,"request":{"finalize":false,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":13}
        {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":1,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":14}
        {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 2 files of 97 bytes","seq":15}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":16}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"cut"},"api":"responses","caller":"agent","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":173,"body_sha256":"47bf6d5b1bdebfa49b0689b8f69a5f19e3d3ae113fde67c00dd2d2edb81ec513","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/0.request"},"response":{"bytes":25,"path":"model/0.response"}},"kind":"model_answered","request":0,"upstream":"stand-in","upstream_status":200},"observed_ns":"<ns>","seq":17}
        {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_started"},"observed_ns":"<ns>","seq":18}
        {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":19}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-1"},"seq":20}
        {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":false,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":1,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":21}
        its last record came before the agent's teardown was to end
        the agent's standard output:
        model request 0: 200 [("content-type", "text/event-stream")] event: response.created\n\n
        job 0's standard output:
        model request 0: 200 [("content-type", "text/plain")] 5
        model request 1: 200 [("content-type", "text/event-stream")] event: response.created\n\n
        model exchange 0 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:8b740cd5d26befcc0dbb835cfb3849a3cdd15f134987d273e263503033017963"],"model":"vendor/m-large","store":false}; response event: response.created\n\n
        model exchange 1 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:3fbcedef56e9b2dbf2b7bf88532bd068ca2537a4aed6e1e9ea74a494abba2fc2"],"model":"vendor/m-small","store":false}; response 5
        model exchange 2 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:8b740cd5d26befcc0dbb835cfb3849a3cdd15f134987d273e263503033017963"],"model":"vendor/m-small","store":false}; response event: response.created\n\n
        model items kept: 3
        the model endpoint at each create: [true, true, false]
        a stalled stream's client hung up before the agent's teardown was to end
        a stalled stream's client hung up before the agent's teardown was to end"#]]
    .assert_eq(&shown);
}

// The model provider's refusal of the call's key is the deployment's fault, never the agent's,
// so the run fails though its submission is judged.
#[test]
fn judge_key_refused() {
    let argv = [
        "ops",
        "judge train 3",
        "await 0 errored",
        "finalize submit 6",
    ];
    let ir = judge_routed(ops_workflow(&argv, 30), 1, &["model m-small expired"]);
    let shown = executed(
        &System,
        "judge-key-refused",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &[Also::Usage, Also::Exchanges],
    );
    expect![[r#"
        failed: the upstream `stand-in` refused its key, with a 401, which ended job 0's model route: give it a key that it takes, and run again
        payloads hold: ["xxx", "xxxxxx"]
        provider.json routes: {"train_judge":[{"api":"responses","models":{"m-small":{"upstream_name":"vendor/m-small"}},"upstream":"stand-in"}]}
        outcome.json's usage: {"calls":[{"grant":"train","job":0,"model_requests":1,"tokens":{"input":0,"output":0,"responses":0}}],"model_requests":1,"ran_ns":"<ns>","sandboxes":[{"class":"sim","count":3,"lasted_ns":"<ns>","provider":"simulated"}],"tokens":{"input":0,"output":0,"responses":0}}
        log.jsonl:
        {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
        {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:cd2eb0837c9b4c962c22d2ff8b5441b7b45805887f051d39bf133b583baf6860","size":3},"timeout_ns":"20000000000"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:cd2eb0837c9b4c962c22d2ff8b5441b7b45805887f051d39bf133b583baf6860","size":3}}},"observed_ns":"<ns>","seq":2}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":3}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"job-0","kind":"model_request_started","request":0},"observed_ns":"<ns>","seq":4}
        {"actions":[],"at_ns":"<ns>","event":{"answer":{"kind":"refused","message":"the model provider refused the route's key, with a 401, which ends the model route: make no more model requests","status":400},"api":"responses","caller":"job-0","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":175,"body_sha256":"3c0cdf32d3c30701e3f2321e9bdb0d6bf020435c0f7bb528cc43be1139395b72","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"path":"model/0.request"},"response":{"bytes":7,"path":"model/0.response"}},"kind":"model_answered","request":0,"upstream":"stand-in","upstream_status":401},"observed_ns":"<ns>","seq":5}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":6}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":7}
        {"actions":[],"at_ns":"<ns>","event":{"invalid":"the judge wrote no result: write it at /pumpkin/out/result.json","job":0,"kind":"judge_collected"},"observed_ns":"<ns>","seq":8}
        {"actions":[{"kind":"reply","op":1,"reply":{"failure":{"code":"invalid_result","message":"the judge wrote no valid result"},"job":0,"kind":"status","remaining":{"calls":99,"seconds":null},"state":"errored"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":1,"request":{"job":"0","kind":"status"}},"observed_ns":"<ns>","seq":9}
        {"actions":[{"kind":"reply","op":2,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":2,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":10}
        {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":1,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":11}
        {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":12}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":13}
        {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_started"},"observed_ns":"<ns>","seq":14}
        {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":15}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-1"},"seq":16}
        {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":1,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":17}
        {"at_ns":"<ns>","note":"the run failed: the upstream `stand-in` refused its key, with a 401, which ended job 0's model route: give it a key that it takes, and run again","seq":18}
        its last record came before the agent's teardown was to end
        job 0's standard output:
        model request 0: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider refused the route's key, with a 401, which ends the model route: make no more model requests","param":null,"type":"invalid_request_error"}}
        model exchange 0 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:6752f5d96dfccaffbf0772742e4557281e807f48cc031258473eb59fb6390c57"],"model":"vendor/m-small","store":false}; response refused
        model items kept: 2
        the model endpoint at each create: [false, true, false]"#]]
    .assert_eq(&shown);
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

// A run holds what it reserved until its end, interrupted or not: a signal ends the second before
// it publishes, and a third is then prepared.
#[test]
fn holds_until_its_end() {
    let runs = Runs::new("holds-until-its-end");
    let sim = Sim::new().with_cpus(16);
    let ir: ir::Workflow = serde_json::from_value(workflow(&["exit", "0"])).unwrap();
    let no_tags = BTreeMap::new();
    let prepare = |name: &str| {
        let placed = placed(&ir, sim.ready()).unwrap();
        let dir = runs.dir.join(name);
        let Ok(unrouted) = placed
            .workflow()
            .by_role(|_| Ok::<_, Infallible>(Vec::new()));
        let on = providers_of(&placed, |_| &sim);
        let recorded = Recorded {
            args: &[],
            tags: &no_tags,
        };
        let id = new_id(SystemTime::now()).unwrap();
        Run::prepare(&System, placed, on, &dir, id, &unrouted, recorded)
    };
    let prepared = |run: &Result<Run<'_, System>, String>| match run {
        Ok(_) => "prepared".to_owned(),
        Err(error) => format!("refused: {error}"),
    };
    let execute = |run: Run<'_, System>| {
        let executed = run.execute(
            Path::new(SANDBOXD),
            None,
            &Dialer::new(test_support::edge::client()),
            || Ok(()),
        );
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

/// [`ops_workflow`]'s workflow, with `argv` as the agent's command and `deadline` as its
/// deadline, in seconds, and a runner, `gpu_runner`, of image `image`, which the agent leases
/// under grant `gpu`: 2 at once, each for at most 600 s, with no bound on its time in all, 4
/// commands in all, and transfers of 64 bytes each.
fn lease_workflow(argv: &[&str], deadline: u64, image: &str) -> Value {
    let mut ir = ops_workflow(argv, deadline);
    let mut runner = ir["roles"][1].clone();
    runner["name"] = json!("gpu_runner");
    runner["image"] = json!(image);
    runner["runtime"] = json!({ "runner": { "cwd": "/work", "env": ["CUDA=1"] } });
    ir["roles"].as_array_mut().unwrap().push(runner);
    let grant = json!({ "lease": {
        "name": "gpu", "role": "gpu_runner", "instances": 2,
        "instance_seconds": 600, "commands": 4, "transfer_bytes": 64,
    } });
    ir["roles"][0]["grants"]
        .as_array_mut()
        .unwrap()
        .insert(1, grant);
    ir
}

/// An image of the simulated provider's that no fault names, for a runner.
const RUNNER: &str = "sha256:3333333333333333333333333333333333333333333333333333333333333333";

#[test]
fn leases() {
    let argv = [
        "ops",
        "lease_open gpu",
        // Once its runner is ready, which a thread of its own reports, so that the ready comes
        // before these ops in the log, not as the threads happen to run.
        "await_lease 0 ready",
        "lease_status 7",
        "lease_open train",
        "lease_run 0 - print one",
        "await_lease 0 ready",
        "lease_run 0 - fill 30000 two",
        "await_lease 0 ready",
        "lease_run 0 1 sleep",
        "await_lease 0 ready",
        "lease_close 0",
        "lease_run 0 - print three",
        "finalize submit 6",
    ];
    check(
        &System,
        "leases",
        &[(PROVIDER, &Sim::new())],
        &lease_workflow(&argv, 30, RUNNER),
        SANDBOXD,
        &expect![[r#"
            finished, with score Some(Number(6))
            payloads hold: ["xxxxxx"]
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"kind":"lease","lease":0,"remaining":{"commands":4,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"grant":"gpu","holder":"agent","instance":0,"kind":"start_lease","lease":0,"start_by_ns":"<ns>"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"gpu","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"<ns>","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"lease_ready","lease":0},"observed_ns":"<ns>","seq":3}
            {"actions":[{"kind":"reply","op":1,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":null,"lease":0,"remaining":{"commands":4,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":1,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":4}
            {"actions":[{"kind":"reply","op":2,"reply":{"kind":"refused","message":"you have no lease 7: name one that `lease open` returned"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":2,"request":{"kind":"lease_status","lease":7}},"observed_ns":"<ns>","seq":5}
            {"actions":[{"kind":"reply","op":3,"reply":{"kind":"refused","message":"`train` is a judge grant, not a lease grant: judge with it instead"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":3,"request":{"grant":"train","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"<ns>","seq":6}
            {"actions":[{"kind":"reply","op":4,"reply":{"kind":"lease","lease":0,"remaining":{"commands":3,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"argv":["print","one"],"command":0,"kind":"run_command","lease":0,"timeout_ns":null}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":4,"request":{"argv":["print","one"],"kind":"lease_run","lease":0,"timeout_seconds":null}},"observed_ns":"<ns>","seq":7}
            {"actions":[],"at_ns":"<ns>","event":{"command":0,"end":{"exited":{"code":0}},"kind":"command_ended","lease":0,"stderr":"","stdout":"one"},"observed_ns":"<ns>","seq":8}
            {"actions":[{"kind":"reply","op":5,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":{"command":0,"end":{"exited":{"code":0}},"kind":"command","stderr":"","stdout":"one"},"lease":0,"remaining":{"commands":3,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":5,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":9}
            {"actions":[{"kind":"reply","op":6,"reply":{"kind":"lease","lease":0,"remaining":{"commands":2,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"argv":["fill","30000","two"],"command":1,"kind":"run_command","lease":0,"timeout_ns":null}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":6,"request":{"argv":["fill","30000","two"],"kind":"lease_run","lease":0,"timeout_seconds":null}},"observed_ns":"<ns>","seq":10}
            {"actions":[],"at_ns":"<ns>","event":{"command":1,"end":{"exited":{"code":0}},"kind":"command_ended","lease":0,"stderr":"","stdout":"<21501 x>two"},"observed_ns":"<ns>","seq":11}
            {"actions":[{"kind":"reply","op":7,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":{"command":1,"end":{"exited":{"code":0}},"kind":"command","stderr":"","stdout":"<21501 x>two"},"lease":0,"remaining":{"commands":2,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":7,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":12}
            {"actions":[{"kind":"reply","op":8,"reply":{"kind":"lease","lease":0,"remaining":{"commands":1,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"argv":["sleep"],"command":2,"kind":"run_command","lease":0,"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":8,"request":{"argv":["sleep"],"kind":"lease_run","lease":0,"timeout_seconds":1}},"observed_ns":"<ns>","seq":13}
            {"actions":[],"at_ns":"<ns>","event":{"command":2,"end":"timed_out","kind":"command_ended","lease":0,"stderr":"","stdout":""},"observed_ns":"<ns>","seq":14}
            {"actions":[{"kind":"reply","op":9,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":{"command":2,"end":"timed_out","kind":"command","stderr":"","stdout":""},"lease":0,"remaining":{"commands":1,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":9,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":15}
            {"actions":[{"kind":"reply","op":10,"reply":{"kind":"lease","lease":0,"remaining":{"commands":1,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"kind":"close_lease","lease":0}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":10,"request":{"kind":"lease_close","lease":0}},"observed_ns":"<ns>","seq":16}
            {"actions":[{"kind":"reply","op":11,"reply":{"kind":"refused","message":"lease 0 has ended: open another"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":11,"request":{"argv":["print","three"],"kind":"lease_run","lease":0,"timeout_seconds":null}},"observed_ns":"<ns>","seq":17}
            {"actions":[{"kind":"reply","op":12,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":12,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":18}
            {"actions":[{"kind":"tear_down_agent"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":19}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":20}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":21}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"lease-0"},"seq":22}
            {"actions":[{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"lease_ended","lease":0},"observed_ns":"<ns>","seq":23}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":24}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":25}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":26}
            {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":27}
            its last record came before the agent's teardown was to end"#]],
    );
}

#[test]
fn lease_ends() {
    let argv = [
        "ops",
        "lease_open gpu 1",
        "await_lease 0 ready",
        "lease_run 0 - sleep",
        // A lease is `expired` only once its runner is gone, so its end comes before the next
        // lease's events.
        "await_lease 0 expired",
        "lease_open gpu",
        "await_lease 1 ready",
        "lease_run 1 - sleep",
        "submit submit 6",
        "exit",
    ];
    check(
        &System,
        "lease-ends",
        &[(PROVIDER, &Sim::new())],
        &lease_workflow(&argv, 30, RUNNER),
        SANDBOXD,
        &expect![[r#"
            finished, with score Some(Number(6))
            payloads hold: ["xxxxxx"]
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"kind":"lease","lease":0,"remaining":{"commands":4,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"grant":"gpu","holder":"agent","instance":0,"kind":"start_lease","lease":0,"start_by_ns":"<ns>"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"gpu","instance_seconds":1,"kind":"lease_open"}},"observed_ns":"<ns>","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"lease_ready","lease":0},"observed_ns":"<ns>","seq":3}
            {"actions":[{"kind":"reply","op":1,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":null,"lease":0,"remaining":{"commands":4,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":1,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":4}
            {"actions":[{"kind":"reply","op":2,"reply":{"kind":"lease","lease":0,"remaining":{"commands":3,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"argv":["sleep"],"command":0,"kind":"run_command","lease":0,"timeout_ns":null}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":2,"request":{"argv":["sleep"],"kind":"lease_run","lease":0,"timeout_seconds":null}},"observed_ns":"<ns>","seq":5}
            {"actions":[{"kind":"close_lease","lease":0}],"at_ns":"<ns>","event":{"kind":"tick"},"observed_ns":"<ns>","seq":6}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"lease-0"},"seq":7}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"lease_ended","lease":0},"observed_ns":"<ns>","seq":8}
            {"actions":[{"kind":"reply","op":3,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":{"command":0,"end":null,"kind":"command","stderr":"","stdout":""},"lease":0,"remaining":{"commands":3,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":2},"state":"expired"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":3,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":9}
            {"actions":[{"kind":"reply","op":4,"reply":{"kind":"lease","lease":1,"remaining":{"commands":3,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"grant":"gpu","holder":"agent","instance":0,"kind":"start_lease","lease":1,"start_by_ns":"<ns>"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":4,"request":{"grant":"gpu","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"<ns>","seq":10}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"lease_ready","lease":1},"observed_ns":"<ns>","seq":11}
            {"actions":[{"kind":"reply","op":5,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":null,"lease":1,"remaining":{"commands":3,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":5,"request":{"kind":"lease_status","lease":1}},"observed_ns":"<ns>","seq":12}
            {"actions":[{"kind":"reply","op":6,"reply":{"kind":"lease","lease":1,"remaining":{"commands":2,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"argv":["sleep"],"command":0,"kind":"run_command","lease":1,"timeout_ns":null}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":6,"request":{"argv":["sleep"],"kind":"lease_run","lease":1,"timeout_seconds":null}},"observed_ns":"<ns>","seq":13}
            {"actions":[{"kind":"reply","op":7,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"stored"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":7,"request":{"finalize":false,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":14}
            {"actions":[{"kind":"close_lease","lease":1},{"kind":"tear_down_agent"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":15}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":16}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":17}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"lease-1"},"seq":18}
            {"actions":[{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"lease_ended","lease":1},"observed_ns":"<ns>","seq":19}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":20}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":21}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-4","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":22}
            {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":false,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":23}
            its last record came before the agent's teardown was to end"#]],
    );
}

// The runner's create fails, as a provider with no room refuses one: the lease fails at no cost to
// its grant.
#[test]
fn lease_unstarted() {
    let argv = [
        "ops",
        "lease_open gpu",
        "await_lease 0 failed",
        "lease_status 0",
        "lease_open gpu",
        "await_lease 1 failed",
        "finalize submit 6",
    ];
    let image = sim::image(LOST);
    check(
        &System,
        "lease-unstarted",
        &[(PROVIDER, &Sim::new())],
        &lease_workflow(&argv, 30, image.as_str()),
        SANDBOXD,
        &expect![[r#"
            finished, with score Some(Number(6))
            payloads hold: ["xxxxxx"]
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"kind":"lease","lease":0,"remaining":{"commands":4,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"grant":"gpu","holder":"agent","instance":0,"kind":"start_lease","lease":0,"start_by_ns":"<ns>"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"gpu","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"<ns>","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"lease_failed","lease":0,"reason":"the runner's sandbox didn't start: the simulated create failed, and can't name what it made: expected the run's list to find it"},"observed_ns":"<ns>","seq":3}
            {"actions":[{"kind":"reply","op":1,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":"the runner's sandbox didn't start: the simulated create failed, and can't name what it made: expected the run's list to find it","idle_seconds":"<s>","kind":"lease_status","last":null,"lease":0,"remaining":{"commands":4,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":2},"state":"failed"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":1,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":4}
            {"actions":[{"kind":"reply","op":2,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":"the runner's sandbox didn't start: the simulated create failed, and can't name what it made: expected the run's list to find it","idle_seconds":"<s>","kind":"lease_status","last":null,"lease":0,"remaining":{"commands":4,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":2},"state":"failed"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":2,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":5}
            {"actions":[{"kind":"reply","op":3,"reply":{"kind":"lease","lease":1,"remaining":{"commands":4,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"grant":"gpu","holder":"agent","instance":0,"kind":"start_lease","lease":1,"start_by_ns":"<ns>"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":3,"request":{"grant":"gpu","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"<ns>","seq":6}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"lease_failed","lease":1,"reason":"the runner's sandbox didn't start: the simulated create failed, and can't name what it made: expected the run's list to find it"},"observed_ns":"<ns>","seq":7}
            {"actions":[{"kind":"reply","op":4,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":"the runner's sandbox didn't start: the simulated create failed, and can't name what it made: expected the run's list to find it","idle_seconds":"<s>","kind":"lease_status","last":null,"lease":1,"remaining":{"commands":4,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":2},"state":"failed"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":4,"request":{"kind":"lease_status","lease":1}},"observed_ns":"<ns>","seq":8}
            {"actions":[{"kind":"reply","op":5,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":5,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":9}
            {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":10}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":11}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":12}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":13}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":14}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-4","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":15}
            {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":16}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":null},"seq":17}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":null},"seq":18}
            its last record came before the agent's teardown was to end"#]],
    );
}

// Each entry keeps the mode it had in the agent's sandbox, whatever the host's umask; a transfer
// counts as a command.
#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the transcript")]
fn lease_transfers() {
    let argv = [
        "ops",
        "lease_open gpu",
        "await_lease 0 ready",
        "lease_put 0 /workspace/.codex /work/in",
        "await_transfer 0 moved",
        "lease_run 0 - list /work/in",
        "await_lease 0 ready",
        "lease_put 0 /workspace/.codex /work/in",
        "await_transfer 0 failed",
        "lease_get 0 /pumpkin/log/1 /pumpkin/out/got",
        "await_transfer 0 moved",
        "lease_get 0 /work/in /pumpkin/out/back",
        "await_transfer 0 moved",
        "lease_put 0 /pumpkin/log /work/in replace",
        "await_transfer 0 moved",
        "lease_get 0 /work/in /pumpkin/out/replaced",
        "await_transfer 0 moved",
        "lease_put 0 /workspace/big /work/big",
        "await_transfer 0 failed",
        "finalize submit 6",
    ];
    let mut ir = lease_workflow(&argv, 30, RUNNER);
    let lease = &mut ir["roles"][0]["grants"][1]["lease"];
    lease["commands"] = json!(8);
    lease["transfer_bytes"] = json!(256);
    let blob = json!({ "path": "/workspace/big/blob", "mode": "0644", "content": "x".repeat(256) });
    let tool = json!({ "path": "/workspace/.codex/tool", "mode": "0750", "content": "true" });
    let read_only = json!({ "path": "/workspace/.codex/ro", "mode": "0444", "content": "r" });
    let shared = json!({ "path": "/workspace/.codex/shared", "mode": "0666", "content": "s" });
    let key = json!({ "path": "/workspace/.codex/shut/key", "mode": "0600", "content": "k" });
    let agent = &mut ir["roles"][0]["runtime"]["agent"];
    let shut = json!({ "path": "/workspace/.codex/shut", "mode": "0551" });
    agent["dirs"].as_array_mut().unwrap().push(shut);
    let files = agent["files"].as_array_mut().unwrap();
    files.extend([blob, tool, read_only, shared, key]);
    let shown = executed(
        &System,
        "lease-transfers",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &[Also::Listing],
    );
    expect![[r#"
        finished, with score Some(Number(6))
        record: agent/console.txt, agent/log/collected, agent/out/back/collected, agent/out/back/config.toml, agent/out/back/ro, agent/out/back/shared, agent/out/back/shut/key, agent/out/back/tool*, agent/out/collected, agent/out/got/collected, agent/out/got/command.stdout, agent/out/replaced/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/console.txt, jobs/0/log/collected, jobs/0/out/collected, jobs/0/out/evidence/, jobs/0/out/result.json, jobs/0/sandbox.json, leases/0/console.txt, leases/0/log/1/collected, leases/0/log/1/command.stdout, leases/0/out/collected, leases/0/sandbox.json, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
        payloads hold: ["xxxxxx"]
        log.jsonl:
        {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
        {"actions":[{"kind":"reply","op":0,"reply":{"kind":"lease","lease":0,"remaining":{"commands":8,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"grant":"gpu","holder":"agent","instance":0,"kind":"start_lease","lease":0,"start_by_ns":"<ns>"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"gpu","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"<ns>","seq":2}
        {"actions":[],"at_ns":"<ns>","event":{"kind":"lease_ready","lease":0},"observed_ns":"<ns>","seq":3}
        {"actions":[{"kind":"reply","op":1,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":null,"lease":0,"remaining":{"commands":8,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":1,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":4}
        {"actions":[{"kind":"reply","op":2,"reply":{"kind":"lease","lease":0,"remaining":{"commands":7,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"direction":"put","from":"/workspace/.codex","kind":"transfer","lease":0,"max_bytes":256,"to":"/work/in","transfer":0}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":2,"request":{"kind":"lease_put","lease":0,"replace":false,"to":"/work/in","tree":"/workspace/.codex"}},"observed_ns":"<ns>","seq":5}
        {"actions":[],"at_ns":"<ns>","event":{"end":{"moved":{"bytes":38,"entries":7}},"kind":"transfer_ended","lease":0,"transfer":0},"observed_ns":"<ns>","seq":6}
        {"actions":[{"kind":"reply","op":3,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":{"end":{"moved":{"bytes":38,"entries":7}},"kind":"transfer","transfer":0},"lease":0,"remaining":{"commands":7,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":3,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":7}
        {"actions":[{"kind":"reply","op":4,"reply":{"kind":"lease","lease":0,"remaining":{"commands":6,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"argv":["list","/work/in"],"command":1,"kind":"run_command","lease":0,"timeout_ns":null}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":4,"request":{"argv":["list","/work/in"],"kind":"lease_run","lease":0,"timeout_seconds":null}},"observed_ns":"<ns>","seq":8}
        {"actions":[],"at_ns":"<ns>","event":{"command":1,"end":{"exited":{"code":0}},"kind":"command_ended","lease":0,"stderr":"","stdout":"0644 collected: collected\n0600 config.toml: model = \"gpt-6-astra\"\n\n0444 ro: r\n0666 shared: s\n0551 shut/\n0600 shut/key: k\n0750 tool: true"},"observed_ns":"<ns>","seq":9}
        {"actions":[{"kind":"reply","op":5,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":{"command":1,"end":{"exited":{"code":0}},"kind":"command","stderr":"","stdout":"0644 collected: collected\n0600 config.toml: model = \"gpt-6-astra\"\n\n0444 ro: r\n0666 shared: s\n0551 shut/\n0600 shut/key: k\n0750 tool: true"},"lease":0,"remaining":{"commands":6,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":5,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":10}
        {"actions":[{"kind":"reply","op":6,"reply":{"kind":"lease","lease":0,"remaining":{"commands":5,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"direction":"put","from":"/workspace/.codex","kind":"transfer","lease":0,"max_bytes":256,"to":"/work/in","transfer":2}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":6,"request":{"kind":"lease_put","lease":0,"replace":false,"to":"/work/in","tree":"/workspace/.codex"}},"observed_ns":"<ns>","seq":11}
        {"actions":[],"at_ns":"<ns>","event":{"end":{"failed":"putting the tree at /work/in in the runner failed: /work/in: File exists (os error 17): the tree's root becomes /work/in, which must not exist: give `--replace` to replace it"},"kind":"transfer_ended","lease":0,"transfer":2},"observed_ns":"<ns>","seq":12}
        {"actions":[{"kind":"reply","op":7,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":{"end":{"failed":"putting the tree at /work/in in the runner failed: /work/in: File exists (os error 17): the tree's root becomes /work/in, which must not exist: give `--replace` to replace it"},"kind":"transfer","transfer":2},"lease":0,"remaining":{"commands":5,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":7,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":13}
        {"actions":[{"kind":"reply","op":8,"reply":{"kind":"lease","lease":0,"remaining":{"commands":4,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"direction":"get","from":"/pumpkin/log/1","kind":"transfer","lease":0,"max_bytes":256,"to":"/pumpkin/out/got","transfer":3}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":8,"request":{"from":"/pumpkin/log/1","kind":"lease_get","lease":0,"to":"/pumpkin/out/got"}},"observed_ns":"<ns>","seq":14}
        {"actions":[],"at_ns":"<ns>","event":{"end":{"moved":{"bytes":145,"entries":2}},"kind":"transfer_ended","lease":0,"transfer":3},"observed_ns":"<ns>","seq":15}
        {"actions":[{"kind":"reply","op":9,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":{"end":{"moved":{"bytes":145,"entries":2}},"kind":"transfer","transfer":3},"lease":0,"remaining":{"commands":4,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":9,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":16}
        {"actions":[{"kind":"reply","op":10,"reply":{"kind":"lease","lease":0,"remaining":{"commands":3,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"direction":"get","from":"/work/in","kind":"transfer","lease":0,"max_bytes":256,"to":"/pumpkin/out/back","transfer":4}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":10,"request":{"from":"/work/in","kind":"lease_get","lease":0,"to":"/pumpkin/out/back"}},"observed_ns":"<ns>","seq":17}
        {"actions":[],"at_ns":"<ns>","event":{"end":{"moved":{"bytes":38,"entries":7}},"kind":"transfer_ended","lease":0,"transfer":4},"observed_ns":"<ns>","seq":18}
        {"actions":[{"kind":"reply","op":11,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":{"end":{"moved":{"bytes":38,"entries":7}},"kind":"transfer","transfer":4},"lease":0,"remaining":{"commands":3,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":11,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":19}
        {"actions":[{"kind":"reply","op":12,"reply":{"kind":"lease","lease":0,"remaining":{"commands":2,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"direction":"replace","from":"/pumpkin/log","kind":"transfer","lease":0,"max_bytes":256,"to":"/work/in","transfer":5}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":12,"request":{"kind":"lease_put","lease":0,"replace":true,"to":"/work/in","tree":"/pumpkin/log"}},"observed_ns":"<ns>","seq":20}
        {"actions":[],"at_ns":"<ns>","event":{"end":{"moved":{"bytes":9,"entries":1}},"kind":"transfer_ended","lease":0,"transfer":5},"observed_ns":"<ns>","seq":21}
        {"actions":[{"kind":"reply","op":13,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":{"end":{"moved":{"bytes":9,"entries":1}},"kind":"transfer","transfer":5},"lease":0,"remaining":{"commands":2,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":13,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":22}
        {"actions":[{"kind":"reply","op":14,"reply":{"kind":"lease","lease":0,"remaining":{"commands":1,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"direction":"get","from":"/work/in","kind":"transfer","lease":0,"max_bytes":256,"to":"/pumpkin/out/replaced","transfer":6}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":14,"request":{"from":"/work/in","kind":"lease_get","lease":0,"to":"/pumpkin/out/replaced"}},"observed_ns":"<ns>","seq":23}
        {"actions":[],"at_ns":"<ns>","event":{"end":{"moved":{"bytes":9,"entries":1}},"kind":"transfer_ended","lease":0,"transfer":6},"observed_ns":"<ns>","seq":24}
        {"actions":[{"kind":"reply","op":15,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":{"end":{"moved":{"bytes":9,"entries":1}},"kind":"transfer","transfer":6},"lease":0,"remaining":{"commands":1,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":15,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":25}
        {"actions":[{"kind":"reply","op":16,"reply":{"kind":"lease","lease":0,"remaining":{"commands":0,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"direction":"put","from":"/workspace/big","kind":"transfer","lease":0,"max_bytes":256,"to":"/work/big","transfer":7}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":16,"request":{"kind":"lease_put","lease":0,"replace":false,"to":"/work/big","tree":"/workspace/big"}},"observed_ns":"<ns>","seq":26}
        {"actions":[],"at_ns":"<ns>","event":{"end":{"failed":"collecting /workspace/big failed: the tree's files hold more than 256 bytes"},"kind":"transfer_ended","lease":0,"transfer":7},"observed_ns":"<ns>","seq":27}
        {"actions":[{"kind":"reply","op":17,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":{"end":{"failed":"collecting /workspace/big failed: the tree's files hold more than 256 bytes"},"kind":"transfer","transfer":7},"lease":0,"remaining":{"commands":0,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":17,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":28}
        {"actions":[{"kind":"reply","op":18,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":18,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":29}
        {"actions":[{"kind":"close_lease","lease":0},{"kind":"tear_down_agent"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":30}
        {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 10 files of 201 bytes; log, 1 file of 9 bytes","seq":31}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":32}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"lease-0"},"seq":33}
        {"actions":[{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"lease_ended","lease":0},"observed_ns":"<ns>","seq":34}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":35}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":36}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":37}
        {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":38}
        its last record came before the agent's teardown was to end"#]]
    .assert_eq(&shown);
}

// The collect from the agent's sandbox waits, so the run goes on deciding meanwhile, answering
// the agent's ops.
#[test]
fn transfer_cut() {
    let argv = [
        "ops",
        "lease_open gpu",
        "await_lease 0 ready",
        "lease_put 0 /workspace/held /work/in",
        "ops",
        "lease_close 0",
        // The close's reply comes before its stop reaches the lease, and an op's reply after the
        // actions of the op before, so that the tree arrives only once the stop has, not as the
        // threads happen to run.
        "lease_status 0",
        "release",
        "await_transfer 0 failed",
        "finalize submit 6",
    ];
    let shown = executed(
        &System,
        "transfer-cut",
        &[(PROVIDER, &Sim::new())],
        &lease_workflow(&argv, 30, RUNNER),
        SANDBOXD,
        &[Also::Listing],
    );
    expect![[r#"
        finished, with score Some(Number(6))
        record: agent/console.txt, agent/log/collected, agent/out/collected, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/console.txt, jobs/0/log/collected, jobs/0/out/collected, jobs/0/out/evidence/, jobs/0/out/result.json, jobs/0/sandbox.json, leases/0/console.txt, leases/0/log/, leases/0/out/collected, leases/0/sandbox.json, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
        payloads hold: ["xxxxxx"]
        log.jsonl:
        {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
        {"actions":[{"kind":"reply","op":0,"reply":{"kind":"lease","lease":0,"remaining":{"commands":4,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"grant":"gpu","holder":"agent","instance":0,"kind":"start_lease","lease":0,"start_by_ns":"<ns>"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"gpu","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"<ns>","seq":2}
        {"actions":[],"at_ns":"<ns>","event":{"kind":"lease_ready","lease":0},"observed_ns":"<ns>","seq":3}
        {"actions":[{"kind":"reply","op":1,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":null,"lease":0,"remaining":{"commands":4,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":1,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":4}
        {"actions":[{"kind":"reply","op":2,"reply":{"kind":"lease","lease":0,"remaining":{"commands":3,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"direction":"put","from":"/workspace/held","kind":"transfer","lease":0,"max_bytes":64,"to":"/work/in","transfer":0}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":2,"request":{"kind":"lease_put","lease":0,"replace":false,"to":"/work/in","tree":"/workspace/held"}},"observed_ns":"<ns>","seq":5}
        {"actions":[{"kind":"reply","op":3,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"judge","max_bytes":8,"name":"train"},{"commands":4,"cumulative_seconds":null,"instance_seconds":600,"instances":2,"kind":"lease","name":"gpu","remaining":{"commands":3,"cumulative_seconds":null,"instance_seconds":600,"instances":1},"transfer_bytes":64},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":"<s>","resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":3,"request":{"kind":"ops"}},"observed_ns":"<ns>","seq":6}
        {"actions":[{"kind":"reply","op":4,"reply":{"kind":"lease","lease":0,"remaining":{"commands":3,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":1}}},{"kind":"close_lease","lease":0}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":4,"request":{"kind":"lease_close","lease":0}},"observed_ns":"<ns>","seq":7}
        {"actions":[],"at_ns":"<ns>","event":{"end":{"failed":"the lease's end cut the transfer short: collecting /workspace/held failed: stopped before the tree was whole"},"kind":"transfer_ended","lease":0,"transfer":0},"observed_ns":"<ns>","seq":8}
        {"actions":[{"kind":"reply","op":5,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":5,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":9}
        {"actions":[{"kind":"tear_down_agent"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":10}
        {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":11}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":12}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"lease-0"},"seq":13}
        {"actions":[{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"lease_ended","lease":0},"observed_ns":"<ns>","seq":14}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":15}
        {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":16}
        {"actions":[],"at_ns":"<ns>","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":17}
        {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":18}
        its last record came before the agent's teardown was to end"#]]
    .assert_eq(&shown);
}

/// [`ops_workflow`]'s workflow, with `argv` as the agent's command, and a deadline of 30 s, whose
/// judge `judge`, 1 for the train judge and 2 for the score's, runs `judge_argv`, and leases a
/// runner, `box`, under its grant `box`, 1 at a time, each for 60 s, whose image is [`DAWDLING`],
/// so that a lease that ends with its call ends once the call has; and whose train calls time out
/// after 20 s.
fn judge_lease_workflow(argv: &[&str], judge: usize, judge_argv: &[&str]) -> Value {
    let mut ir = ops_workflow(argv, 30);
    ir["roles"][0]["grants"][0]["judge"]["call_timeout_seconds"] = json!(20);
    ir["roles"][judge]["runtime"]["judge"]["command"]["argv"] = json!(judge_argv);
    let mut runner = ir["roles"][1].clone();
    runner["name"] = json!("box");
    runner["image"] = json!(sim::image(DAWDLING));
    runner["runtime"] = json!({ "runner": { "cwd": "/work" } });
    ir["roles"].as_array_mut().unwrap().push(runner);
    ir["roles"][judge]["grants"] = json!([{ "lease": {
        "name": "box", "role": "box", "instances": 1, "instance_seconds": 60,
        "transfer_bytes": 64,
    } }]);
    ir
}

/// What a judge that leases runs: it opens a lease, puts its slot's payload there, runs a command
/// there, which prints 7, reads the command's end from the lease's status, and scores what it
/// printed.
const LEASING_JUDGE: [&str; 8] = [
    "ops",
    "lease_open box",
    "await_lease 0 ready",
    "lease_put 0 /pumpkin/in/payload /work/in",
    "await_transfer 0 moved",
    "lease_run 0 - print 7",
    "await_lease 0 ready",
    "result",
];

// The agent, which names the call's lease, is refused, as no lease of its own has that number.
#[test]
fn judge_leases() {
    let argv = [
        "ops",
        "judge train 6",
        "await 0 completed",
        "lease_status 0",
        "finalize submit 6",
    ];
    check(
        &System,
        "judge-leases",
        &[(PROVIDER, &Sim::new())],
        &judge_lease_workflow(&argv, 1, &LEASING_JUDGE),
        SANDBOXD,
        &expect![[r#"
            finished, with score Some(Number(6))
            payloads hold: ["xxxxxx"]
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"20000000000"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":3}
            {"actions":[{"kind":"reply","op":1,"reply":{"kind":"lease","lease":0,"remaining":{"commands":null,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":0}}},{"grant":"box","holder":"job-0","instance":0,"kind":"start_lease","lease":0,"start_by_ns":"<ns>"}],"at_ns":"<ns>","event":{"caller":"job-0","kind":"op_arrived","op":1,"request":{"grant":"box","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"<ns>","seq":4}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"lease_ready","lease":0},"observed_ns":"<ns>","seq":5}
            {"actions":[{"kind":"reply","op":2,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":null,"lease":0,"remaining":{"commands":null,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":0},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"job-0","kind":"op_arrived","op":2,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":6}
            {"actions":[{"kind":"reply","op":3,"reply":{"kind":"lease","lease":0,"remaining":{"commands":null,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":0}}},{"direction":"put","from":"/pumpkin/in/payload","kind":"transfer","lease":0,"max_bytes":64,"to":"/work/in","transfer":0}],"at_ns":"<ns>","event":{"caller":"job-0","kind":"op_arrived","op":3,"request":{"kind":"lease_put","lease":0,"replace":false,"to":"/work/in","tree":"/pumpkin/in/payload"}},"observed_ns":"<ns>","seq":7}
            {"actions":[],"at_ns":"<ns>","event":{"end":{"moved":{"bytes":15,"entries":2}},"kind":"transfer_ended","lease":0,"transfer":0},"observed_ns":"<ns>","seq":8}
            {"actions":[{"kind":"reply","op":4,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":{"end":{"moved":{"bytes":15,"entries":2}},"kind":"transfer","transfer":0},"lease":0,"remaining":{"commands":null,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":0},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"job-0","kind":"op_arrived","op":4,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":9}
            {"actions":[{"kind":"reply","op":5,"reply":{"kind":"lease","lease":0,"remaining":{"commands":null,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":0}}},{"argv":["print","7"],"command":1,"kind":"run_command","lease":0,"timeout_ns":null}],"at_ns":"<ns>","event":{"caller":"job-0","kind":"op_arrived","op":5,"request":{"argv":["print","7"],"kind":"lease_run","lease":0,"timeout_seconds":null}},"observed_ns":"<ns>","seq":10}
            {"actions":[],"at_ns":"<ns>","event":{"command":1,"end":{"exited":{"code":0}},"kind":"command_ended","lease":0,"stderr":"","stdout":"7"},"observed_ns":"<ns>","seq":11}
            {"actions":[{"kind":"reply","op":6,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":{"command":1,"end":{"exited":{"code":0}},"kind":"command","stderr":"","stdout":"7"},"lease":0,"remaining":{"commands":null,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":0},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"job-0","kind":"op_arrived","op":6,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":12}
            {"actions":[{"kind":"close_lease","lease":0}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":13}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":14}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"returned":{"raw_score":7},"score":7}},"observed_ns":"<ns>","seq":15}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"lease-0"},"seq":16}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"lease_ended","lease":0},"observed_ns":"<ns>","seq":17}
            {"actions":[{"kind":"reply","op":7,"reply":{"job":0,"kind":"status","remaining":{"calls":99,"seconds":null},"returned":{"raw_score":7},"scored":true,"state":"completed"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":7,"request":{"job":"0","kind":"status"}},"observed_ns":"<ns>","seq":18}
            {"actions":[{"kind":"reply","op":8,"reply":{"kind":"refused","message":"you have no lease 0: name one that `lease open` returned"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":8,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":19}
            {"actions":[{"kind":"reply","op":9,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":9,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":20}
            {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":1,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":21}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":22}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":23}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_started"},"observed_ns":"<ns>","seq":24}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":25}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-4","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-1"},"seq":26}
            {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":1,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":27}
            its last record came before the agent's teardown was to end"#]],
    );
}

// A run whose score's judge leases a runner in the final phase, as [`LEASING_JUDGE`] does, and
// scores what its command printed; the run ends once its lease has, after the call.
#[test]
fn score_leases() {
    check(
        &System,
        "score-leases",
        &[(PROVIDER, &Sim::new())],
        &judge_lease_workflow(&["ops", "finalize submit 6"], 2, &LEASING_JUDGE),
        SANDBOXD,
        &expect![[r#"
            finished, with score Some(Number(7))
            payloads hold: ["xxxxxx"]
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":2}
            {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":3}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":4}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":6}
            {"actions":[{"kind":"reply","op":1,"reply":{"kind":"lease","lease":0,"remaining":{"commands":null,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":0}}},{"grant":"box","holder":"job-0","instance":0,"kind":"start_lease","lease":0,"start_by_ns":"<ns>"}],"at_ns":"<ns>","event":{"caller":"job-0","kind":"op_arrived","op":1,"request":{"grant":"box","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"<ns>","seq":7}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"lease_ready","lease":0},"observed_ns":"<ns>","seq":8}
            {"actions":[{"kind":"reply","op":2,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":null,"lease":0,"remaining":{"commands":null,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":0},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"job-0","kind":"op_arrived","op":2,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":9}
            {"actions":[{"kind":"reply","op":3,"reply":{"kind":"lease","lease":0,"remaining":{"commands":null,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":0}}},{"direction":"put","from":"/pumpkin/in/payload","kind":"transfer","lease":0,"max_bytes":64,"to":"/work/in","transfer":0}],"at_ns":"<ns>","event":{"caller":"job-0","kind":"op_arrived","op":3,"request":{"kind":"lease_put","lease":0,"replace":false,"to":"/work/in","tree":"/pumpkin/in/payload"}},"observed_ns":"<ns>","seq":10}
            {"actions":[],"at_ns":"<ns>","event":{"end":{"moved":{"bytes":15,"entries":2}},"kind":"transfer_ended","lease":0,"transfer":0},"observed_ns":"<ns>","seq":11}
            {"actions":[{"kind":"reply","op":4,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":{"end":{"moved":{"bytes":15,"entries":2}},"kind":"transfer","transfer":0},"lease":0,"remaining":{"commands":null,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":0},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"job-0","kind":"op_arrived","op":4,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":12}
            {"actions":[{"kind":"reply","op":5,"reply":{"kind":"lease","lease":0,"remaining":{"commands":null,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":0}}},{"argv":["print","7"],"command":1,"kind":"run_command","lease":0,"timeout_ns":null}],"at_ns":"<ns>","event":{"caller":"job-0","kind":"op_arrived","op":5,"request":{"argv":["print","7"],"kind":"lease_run","lease":0,"timeout_seconds":null}},"observed_ns":"<ns>","seq":13}
            {"actions":[],"at_ns":"<ns>","event":{"command":1,"end":{"exited":{"code":0}},"kind":"command_ended","lease":0,"stderr":"","stdout":"7"},"observed_ns":"<ns>","seq":14}
            {"actions":[{"kind":"reply","op":6,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":{"command":1,"end":{"exited":{"code":0}},"kind":"command","stderr":"","stdout":"7"},"lease":0,"remaining":{"commands":null,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":0},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"job-0","kind":"op_arrived","op":6,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":15}
            {"actions":[{"kind":"close_lease","lease":0}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":16}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":17}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"returned":{"raw_score":7},"score":7}},"observed_ns":"<ns>","seq":18}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"lease-0"},"seq":19}
            {"actions":[{"kind":"finish","outcome":{"score":7,"verdict":{"kind":"judged","result":{"returned":{"raw_score":7},"score":7},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"lease_ended","lease":0},"observed_ns":"<ns>","seq":20}
            its last record came before the agent's teardown was to end"#]],
    );
}

// The lease starts by the call's timeout, not by its class's startup, so its create gives up
// then.
#[test]
fn lease_starts_by_its_holders_end() {
    let judge = ["ops", "lease_open box"];
    let argv = [
        "ops",
        "judge train 6",
        "await 0 errored",
        "finalize submit 6",
    ];
    let mut ir = judge_lease_workflow(&argv, 1, &judge);
    ir["roles"][0]["grants"][0]["judge"]["call_timeout_seconds"] = json!(1);
    ir["roles"][3]["image"] = json!(sim::image(SLOW).as_str());
    check(
        &System,
        "lease-starts-by-its-holders-end",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score Some(Number(6))
            payloads hold: ["xxxxxx"]
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":3}
            {"actions":[{"kind":"reply","op":1,"reply":{"kind":"lease","lease":0,"remaining":{"commands":null,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":0}}},{"grant":"box","holder":"job-0","instance":0,"kind":"start_lease","lease":0,"start_by_ns":"<ns>"}],"at_ns":"<ns>","event":{"caller":"job-0","kind":"op_arrived","op":1,"request":{"grant":"box","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"<ns>","seq":4}
            {"actions":[{"kind":"close_lease","lease":0}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":6}
            {"actions":[],"at_ns":"<ns>","event":{"invalid":"the judge wrote no result: write it at /pumpkin/out/result.json","job":0,"kind":"judge_collected"},"observed_ns":"<ns>","seq":7}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"lease_failed","lease":0,"reason":"the runner's sandbox didn't start: the simulated create gave up at its deadline, and can't name what it made: expected the run's list to find it"},"observed_ns":"<ns>","seq":8}
            {"actions":[{"kind":"reply","op":2,"reply":{"failure":{"code":"invalid_result","message":"the judge wrote no valid result"},"job":0,"kind":"status","remaining":{"calls":99,"seconds":null},"state":"errored"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":2,"request":{"job":"0","kind":"status"}},"observed_ns":"<ns>","seq":9}
            {"actions":[{"kind":"reply","op":3,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":3,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":10}
            {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":1,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":11}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":12}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":13}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_started"},"observed_ns":"<ns>","seq":14}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":15}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-4","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-1"},"seq":16}
            {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":1,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":17}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":null},"seq":18}
            its last record came before the agent's teardown was to end"#]],
    );
}

// A run whose train judge exits as a command it started in its lease still runs: the lease
// closes as the call's command ends, with the command in it, and the call ends without a result.
#[test]
fn judge_exits_leasing() {
    let judge = [
        "ops",
        "lease_open box",
        "await_lease 0 ready",
        "lease_run 0 - sleep",
    ];
    let argv = [
        "ops",
        "judge train 6",
        "await 0 errored",
        "finalize submit 6",
    ];
    check(
        &System,
        "judge-exits-leasing",
        &[(PROVIDER, &Sim::new())],
        &judge_lease_workflow(&argv, 1, &judge),
        SANDBOXD,
        &expect![[r#"
            finished, with score Some(Number(6))
            payloads hold: ["xxxxxx"]
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"<ns>","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"20000000000"}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"observed_ns":"<ns>","seq":3}
            {"actions":[{"kind":"reply","op":1,"reply":{"kind":"lease","lease":0,"remaining":{"commands":null,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":0}}},{"grant":"box","holder":"job-0","instance":0,"kind":"start_lease","lease":0,"start_by_ns":"<ns>"}],"at_ns":"<ns>","event":{"caller":"job-0","kind":"op_arrived","op":1,"request":{"grant":"box","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"<ns>","seq":4}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"lease_ready","lease":0},"observed_ns":"<ns>","seq":5}
            {"actions":[{"kind":"reply","op":2,"reply":{"busy_seconds":"<s>","elapsed_seconds":"<s>","failure":null,"idle_seconds":"<s>","kind":"lease_status","last":null,"lease":0,"remaining":{"commands":null,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":0},"state":"ready"}}],"at_ns":"<ns>","event":{"caller":"job-0","kind":"op_arrived","op":2,"request":{"kind":"lease_status","lease":0}},"observed_ns":"<ns>","seq":6}
            {"actions":[{"kind":"reply","op":3,"reply":{"kind":"lease","lease":0,"remaining":{"commands":null,"cumulative_seconds":"<s>","instance_seconds":"<s>","instances":0}}},{"argv":["sleep"],"command":0,"kind":"run_command","lease":0,"timeout_ns":null}],"at_ns":"<ns>","event":{"caller":"job-0","kind":"op_arrived","op":3,"request":{"argv":["sleep"],"kind":"lease_run","lease":0,"timeout_seconds":null}},"observed_ns":"<ns>","seq":7}
            {"actions":[{"kind":"close_lease","lease":0}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":8}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":9}
            {"actions":[],"at_ns":"<ns>","event":{"invalid":"the judge wrote no result: write it at /pumpkin/out/result.json","job":0,"kind":"judge_collected"},"observed_ns":"<ns>","seq":10}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"lease-0"},"seq":11}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"lease_ended","lease":0},"observed_ns":"<ns>","seq":12}
            {"actions":[{"kind":"reply","op":4,"reply":{"failure":{"code":"invalid_result","message":"the judge wrote no valid result"},"job":0,"kind":"status","remaining":{"calls":99,"seconds":null},"state":"errored"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":4,"request":{"job":"0","kind":"status"}},"observed_ns":"<ns>","seq":13}
            {"actions":[{"kind":"reply","op":5,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"caller":"agent","kind":"op_arrived","op":5,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":14}
            {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":1,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"observed_ns":"<ns>","seq":15}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":16}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent"},"seq":17}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_started"},"observed_ns":"<ns>","seq":18}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_exited","status":{"code":0}},"observed_ns":"<ns>","seq":19}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-4","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-1"},"seq":20}
            {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":1,"kind":"judge_collected","result":{"returned":{"raw_score":6},"score":6}},"observed_ns":"<ns>","seq":21}
            its last record came before the agent's teardown was to end"#]],
    );
}
