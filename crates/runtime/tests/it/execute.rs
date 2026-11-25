#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::fmt::Write as _;
use std::io::{self, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, iter};

use decision::{Outcome, lasts};
use expect_test::{Expect, expect};
use gateway::Dialer;
use provider::{
    Console, Create, Created, Creator, ImageRef, Listed, ListenDials, Listened, Listens, Meeting,
    Provider, Ready, Reservation, Role, RunTag, SandboxId, SandboxdRef, Sweep, Undeleted,
};
use route::Serving;
use runtime::{
    DriverUse, Host, JobUse, ModelRoutes, Reaped, Record, Recorded, Run, System, TokenPrices,
    new_id,
};
use schema::ir::{self, Image, ModelName};
use schema::sandbox::{RESULT_MAX_DEPTH, SocketDial};
use schema::workflow::{ByRole, Placed, Workflow};
use serde_json::{Value, json};
use stdx::Deadline;
use test_support::http::{read_request, write_chunk};
use test_support::{TestDir, files, sandboxes, standard_outputs};

use crate::body::Bodies;
use crate::host::{LateTake, SimHost, SimTime, SlowSync, Synced};
use crate::run::{PROVIDER, compact_outcome, placed, placed_on, providers_of, serving};
use crate::sim::{
    self, BRIEF, HUNG, Journal, LAST, LOST, MUTE, REFUSED_ONCE, SCRIBBLED, SLOW, Sim, UNDELETABLE,
    UNDELETABLE_LAST, UNFREEZABLE, UNLISTABLE, UNPUBLISHED, VANISHING,
};

const SANDBOXD: &str = "/sim/sandboxd";

// Short, so that waiting it out takes seconds, but far longer than a simulated sandbox takes to
// start, so that a busy machine makes a start that succeeds wait longer, never fail.
const SHORT_STARTUP: u64 = 1;

const AGENT_SANDBOX: &str = "agents/agent/.sandbox.json.partial";
const DELETED: &str = r#""state": "deleted""#;

// Half a second past the 6 s hard end of a run of `SHORT_STARTUP` and a deadline of 2 s.
const PAST_THE_HARD_END: Duration = Duration::from_millis(6_500);

// Far past the signal's latency, yet short, so that a run that misses the signal fails its case
// within a minute or so, rather than holding `ci` up.
const WAITED: u64 = 60;

#[derive(Clone, Copy, PartialEq, Eq)]
enum At {
    Created(&'static str),
    Delete,
    AgentStarted,
    SwapPayloads,
    Logged(&'static str),
    FinalSweep,
}

// A size of its own, within the score slot's 16 bytes.
const SWAPPED: &str = "yyyyyyyyy";

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

fn interrupt() {
    use rustix::process::{Signal, getpid, kill_process};
    stdx::signal::catch(&[Signal::TERM]).unwrap();
    kill_process(getpid(), Signal::TERM).unwrap();
    // Any thread may take the signal, after `kill` returns.
    while stdx::signal::caught().is_none() {
        thread::yield_now();
    }
}

impl Sweep for Interrupting<'_> {
    fn delete(&self, run: &RunTag, ids: &[SandboxId]) -> Result<(), Undeleted> {
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
        if self.at == Some(At::FinalSweep)
            && fs::read_to_string(self.record.join("log.jsonl"))
                .unwrap_or_default()
                .contains(r#""kind":"start_final_call""#)
        {
            interrupt();
        }
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
        self.sim.publish_image(image)
    }

    fn publish_sandboxd(&self, path: &Path) -> Result<SandboxdRef, String> {
        self.sim.publish_sandboxd(path)
    }

    fn meeting(&self) -> Meeting<'_> {
        match self.sim.meeting() {
            Meeting::Local(_) => Meeting::Local(self),
            Meeting::Listen(_) => Meeting::Listen(self),
            Meeting::Private(_) => {
                panic!("the simulated provider's sandboxes dial no private address")
            }
        }
    }

    fn console(&self, run: &RunTag, id: &SandboxId) -> Result<Console, String> {
        self.sim.console(run, id)
    }
}

impl Creator<SocketDial> for Interrupting<'_> {
    fn create(&self, create: &Create, dial: SocketDial) -> Result<Created, String> {
        self.created(create, || self.sim.create(create, dial))
    }
}

impl Creator<ListenDials, Listened> for Interrupting<'_> {
    fn create(&self, create: &Create, dials: ListenDials) -> Result<Listened, String> {
        self.created(create, || self.sim.create(create, dials))
    }
}

impl Listens for Interrupting<'_> {
    fn publish_companion_images(
        &self,
        images: &[Image],
        sandboxd: &SandboxdRef,
    ) -> Result<Vec<ImageRef>, String> {
        self.sim.publish_companion_images(images, sandboxd)
    }
}

impl Interrupting<'_> {
    // The simulated provider's `create`, after which the signal comes if `at` names it.
    fn created<C>(
        &self,
        create: &Create,
        sim: impl FnOnce() -> Result<C, String>,
    ) -> Result<C, String> {
        if create.name.as_str().starts_with("shared-") {
            assert!(!create.ops && !create.model);
        }
        let created = sim();
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

fn interrupting<'a>(
    providers: &[(&'static str, &'a Sim)],
    at: Option<At>,
    record: &'a Path,
) -> Vec<(&'static str, Interrupting<'a>)> {
    (providers.iter())
        .map(|&(name, sim)| (name, Interrupting { sim, at, record }))
        .collect()
}

fn interrupted_at(run: Run<'_, impl Host>, sandboxd: &str, also: &[Also]) -> Vec<String> {
    let record = run.dir().to_owned();
    let at = interruption(also);
    let waited = Deadline::after(Duration::from_secs(WAITED));
    let mut ran = false;
    let executed = thread::scope(|scope| {
        if at == Some(At::AgentStarted) {
            scope.spawn(|| interrupt_once_started(&record));
        }
        run.execute(
            Path::new(sandboxd),
            None,
            &Dialer::new(test_support::edge::client()),
            &Bodies { record: &record },
            || {
                ran = true;
                Ok(())
            },
            driver_used,
        )
    });
    let mut lines = vec![match executed {
        Ok(outcome) => finished(&outcome),
        Err(stopped) => stopped.to_string(),
    }];
    if at.is_some_and(|at| at != At::SwapPayloads) {
        lines.push(format!("it started: {ran}"));
        let within = waited.remaining().is_some();
        lines.push(format!("it returned within {WAITED} s: {within}"));
    }
    lines
}

/// What the tests' driver read of itself: figures of its own and its job's, one of them unread.
fn driver_used() -> DriverUse {
    DriverUse {
        cpu: Some(Duration::from_millis(41_200)),
        children_cpu: Some(Duration::from_millis(3_100)),
        peak_memory_bytes: Some(212 << 20),
        job: Some(JobUse {
            unit: "pumpkin-job-20261125-120000-00000001.service".to_owned(),
            cpu: Some(Duration::from_secs(46)),
            peak_memory_bytes: None,
            peak_tasks_count: Some(1204),
        }),
    }
}

fn interrupt_once_started(record: &Path) {
    sim::logged(record, r#""kind":"agent_started""#);
    interrupt();
}

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

// What `case` shows on the simulated provider, which shows the same where its sandboxes listen.
fn listening_too(case: impl Fn(Sim) -> String) -> String {
    let (dialing, listening) = (case(Sim::new()), case(Sim::new().listening()));
    assert_eq!(
        listening, dialing,
        "where sandboxes listen, against where they dial"
    );
    dialing
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Also {
    Interrupted(At),
    Listing,
    Usage,
    /// The `driver` of `outcome.json`, and what [`Record::driver_use`] reads of it.
    Driver,
    Exchanges,
    ChangedItem,
    Polls,
    Reaped,
    /// Each sandbox's hard end, after the run's start.
    HardEnds,
    /// Each sandbox's `sandbox_created` and `sandbox_met`, which the log otherwise drops.
    Starts,
    /// The meetings of the named sandbox's containers, each with when it was observed.
    Met(&'static str),
    /// A part of the records that nothing orders among the others, which the log shows apart
    /// from them, sorted.
    Racing(&'static str),
    /// A file of the record, which the output shows whole.
    File(&'static str),
}

fn interruption(also: &[Also]) -> Option<At> {
    also.iter().find_map(|also| match also {
        Also::Interrupted(at) => Some(*at),
        Also::Listing
        | Also::Usage
        | Also::Driver
        | Also::Exchanges
        | Also::ChangedItem
        | Also::Polls
        | Also::Reaped
        | Also::HardEnds
        | Also::Starts
        | Also::Met(_)
        | Also::Racing(_)
        | Also::File(_) => None,
    })
}

fn executed<H: Shown>(
    host: &H,
    name: &str,
    providers: &[(&'static str, &Sim)],
    ir: &Value,
    sandboxd: &str,
    also: &[Also],
) -> String {
    let ir = serde_json::from_value(ir.clone()).unwrap();
    executed_ir(host, name, providers, &ir, sandboxd, also)
}

fn model_routed(ir: &ir::Workflow) -> bool {
    ir.roles.iter().any(|role| match &role.runtime {
        ir::Runtime::Agent(agent) => agent.egress.model.is_some(),
        ir::Runtime::Target(target) => target.egress.model.is_some(),
        ir::Runtime::Runner(_) => false,
    })
}

fn executed_ir<H: Shown>(
    host: &H,
    name: &str,
    providers: &[(&'static str, &Sim)],
    ir: &ir::Workflow,
    sandboxd: &str,
    also: &[Also],
) -> String {
    let at = interruption(also);
    let runs = TestDir::new();
    let stand_in = model_routed(ir).then(|| StandIn::start(host.time()));
    let ready = (providers.iter()).map(|(name, provider)| (*name, provider.ready()));
    let ready: BTreeMap<&str, &Ready> = ready.collect();
    let placed = placed_on(ir, &ready).unwrap();
    let servings = StandIn::servings(stand_in.as_ref(), placed.workflow(), runs.path());
    let hard_end = lasts(&placed);
    let id = new_id(SystemTime::now()).unwrap();
    let record = runs.path().join(id.as_str());
    for (_, sim) in providers {
        sim.records(&record, host.time());
    }
    let interrupting = interrupting(providers, at, &record);
    let on = providers_of(&placed, |name| named(&interrupting, name));
    let prices = prices();
    let (models, recorded) = kept(&placed, &servings, &prices);
    let run = Run::prepare(host, placed.clone(), on, runs.path(), id, models, recorded).unwrap();
    let (record, tag) = (run.dir().to_owned(), RunTag::try_from(run.id().to_owned()));
    let sockets = record.join("sockets");
    let partial = record.with_file_name(format!(".{}.partial", run.id()));
    let before = unix_ns();
    let mut lines = interrupted_at(run, sandboxd, also);
    let started = before..=unix_ns();
    let log = read_log(&record);
    let held = held(name, &record, &log, providers, also, &lines[0]);
    lines.extend(held);
    if also.contains(&Also::Polls) {
        lines.splice(1..1, polls(&log));
    }
    lines.extend(also.iter().flat_map(|also| met(&log, also)));
    lines.push("log.jsonl:".to_owned());
    let (log, start, last) = logged(log, &started, H::SCRIPTED, also);
    lines.extend(log);
    lines.extend(last.map(|last| format!("its last record came {}", when(&placed, last))));
    // A scripted host's last record shows its time above, which the log's file doesn't follow.
    if !H::SCRIPTED && written(&record, start).is_some_and(|written| written >= hard_end) {
        lines.push("its log's last record was written once the hard end had passed".to_owned());
    }
    lines.extend(standard_outputs(&record));
    lines.extend(also.iter().flat_map(|also| shown_file(&record, also)));
    let address = stand_in.as_ref().map(|stand_in| stand_in.address.clone());
    let (received, hung_ups) = stand_in.map(StandIn::stop).unwrap_or_default();
    let exchanges = kept_exchanges(name, &record, received);
    folds_as_whole(name, &record);
    if also.contains(&Also::Exchanges) {
        lines.extend(exchanges);
    }
    lines.extend((also.contains(&Also::ChangedItem)).then(|| changed_item(&record)));
    assert!(!sockets.exists(), "{name}: its socket directories are left");
    if also.contains(&Also::Reaped) {
        lines.extend(reaped(&record, providers));
    }
    lines.extend(left(name, providers, &tag.unwrap()));
    for hung_up in hung_ups {
        // A run whose stream stalled has started. The stand-in reads the clock after the hang-up,
        // so this errs late.
        let hung_up = Duration::from_nanos(hung_up - start.unwrap());
        let at = when(&placed, hung_up);
        // Its place among the run's records isn't shown: a `model_answered` may come after the
        // run's last record, and so may a hang-up that its answer follows.
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

// The lines of the file of `record` that `also` names, if it names one, under its path.
fn shown_file(record: &Path, also: &Also) -> Vec<String> {
    let Also::File(path) = also else {
        return Vec::new();
    };
    let text = fs::read_to_string(record.join(path)).unwrap();
    iter::once(format!("{path}:"))
        .chain(text.lines().map(str::to_owned))
        .collect()
}

fn prices() -> BTreeMap<ModelName, TokenPrices> {
    // Only `m-small` is priced, so that a record shows a model with a price beside one without.
    let small = ModelName::try_from("m-small".to_owned()).unwrap();
    let prices = json!({ "input": 3, "cached": 0.3, "cache_write": 3.75, "output": 15 });
    BTreeMap::from([(small, serde_json::from_value(prices).unwrap())])
}

fn kept<'p>(
    placed: &Placed,
    servings: &'p ByRole<Vec<Serving>>,
    prices: &'p BTreeMap<ModelName, TokenPrices>,
) -> (ModelRoutes<'p>, Recorded<'p>) {
    let budgets = runtime::budgets(placed.workflow(), prices).unwrap();
    (
        ModelRoutes { servings, budgets },
        Recorded {
            args: &[],
            prices,
            sources: &NO_SOURCES,
        },
    )
}

// The tests' workflows are IR, which no evaluation read from a file.
static NO_SOURCES: BTreeMap<ir::Label, String> = BTreeMap::new();

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
    let hard_ends = sandboxes_hard_ends(name, record, log, providers);
    if also.contains(&Also::HardEnds) {
        lines.extend(hard_ends);
    }
    let outcome = fs::read_to_string(record.join("outcome.json")).unwrap();
    let (usage, finished) = outcome_holds(name, returned, &outcome, log);
    if let Some(finished) = finished {
        lines.push(format!("outcome.json: {finished}"));
    }
    let shows = also.contains(&Also::Usage);
    usage_holds(name, record, log, &usage, providers, shows);
    if shows {
        let usage = compact_outcome(&json!({ "usage": usage }).to_string());
        let usage: Value = serde_json::from_str(&usage).unwrap();
        lines.push(format!("outcome.json's usage: {}", usage["usage"]));
    }
    let driver = Record::open(record).unwrap().driver_use().unwrap();
    assert_eq!(
        driver,
        Some(driver_used()),
        "{name}: outcome.json holds another driver's use"
    );
    if also.contains(&Also::Driver) {
        let outcome: Value = serde_json::from_str(&outcome).unwrap();
        lines.push(format!("outcome.json's driver: {}", outcome["driver"]));
        lines.push(format!("read back: {driver:?}"));
    }
    lines
}

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
    // Kept files that lead outside the record, as damage would make them.
    for part in ["request", "response"] {
        let kept = record.join(format!("model/0.{part}"));
        fs::remove_file(&kept).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", &kept).unwrap();
    }
    let opened = Record::open(record).unwrap();
    let kept = (read_log(record).into_iter())
        .find(|line| line["event"]["kind"] == "model_answered")
        .map(|line| line["event"]["kept"].clone())
        .unwrap();
    let refused = [
        opened.model_request(0).unwrap_err(),
        opened.kept_request(0, &kept["request"]).unwrap_err(),
        opened.model_response(0).unwrap_err(),
        opened
            .kept_request(0, &json!("model/0.request"))
            .unwrap_err(),
    ];
    for refused in refused {
        let _ = write!(said, "\nrequest 0 read from a damaged log: {refused}");
    }
    said
}

fn read_log(record: &Path) -> Vec<Value> {
    let log = fs::read_to_string(record.join("log.jsonl")).unwrap_or_default();
    (log.lines())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn undeleted(name: &str, record: &Path, log: &[Value]) -> Vec<String> {
    let deletes: Vec<&Value> = (log.iter())
        .filter(|logged| logged["event"]["kind"] == "sandbox_deleted")
        .map(|logged| &logged["event"])
        .collect();
    let ir: Value = serde_json::from_slice(&fs::read(record.join("ir.json")).unwrap()).unwrap();
    let index = |role: &str| {
        let mut roles = ir["roles"].as_array().unwrap().iter();
        roles
            .position(|candidate| candidate["name"] == role)
            .unwrap()
    };
    let named = |dir: &str| match dir.split_once('/') {
        Some(("agents", role)) => format!("agent-{}", index(role)),
        Some(("shared", role)) => format!("shared-{}", index(role)),
        Some(("jobs", number)) => format!("job-{number}"),
        Some(("leases", number)) => format!("lease-{number}"),
        _ => dir.to_owned(),
    };
    // The sandboxes whose records keep them undeleted, by their names.
    let mut kept = BTreeSet::new();
    let shared = fs::read_dir(record.join("shared")).into_iter().flatten();
    for role in shared {
        let role = role.unwrap().file_name().into_string().unwrap();
        let sandbox = fs::read(record.join("shared").join(&role).join("sandbox.json"));
        let sandbox: Option<Value> = sandbox
            .ok()
            .map(|json| serde_json::from_slice(&json).unwrap());
        if sandbox.is_some_and(|sandbox| sandbox["delete"] != json!({ "state": "deleted" })) {
            kept.insert(named(&format!("shared/{role}")));
        }
    }
    let mut normal = |line: &str| {
        let (whose, sandbox) = line.split_once("/sandbox.json: ")?;
        let sandbox: Value = serde_json::from_str(sandbox).unwrap();
        let gone = sandbox["delete"] == json!({ "state": "deleted" });
        let (_, dir) = whose.rsplit_once(", ").unwrap();
        let named = named(dir);
        if !gone {
            kept.insert(named.clone());
        }
        gone.then(|| {
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
    let lines = lines.filter(|line| normal(line).is_none()).collect();
    let created = log.iter().map(|logged| &logged["event"]);
    let created = created.filter(|event| event["kind"] == "sandbox_created");
    for event in created {
        let sandbox = &event["sandbox"];
        let gone = deletes.iter().any(|delete| delete["sandbox"] == *sandbox);
        let left = sandbox
            .as_str()
            .is_some_and(|sandbox| kept.contains(sandbox));
        assert!(
            gone || left,
            "{name}: the log records no delete of {sandbox}, though its record keeps it as deleted"
        );
    }
    lines
}

fn outcome_holds(
    name: &str,
    returned: &str,
    outcome: &str,
    log: &[Value],
) -> (Value, Option<Value>) {
    let mut outcome: Value = serde_json::from_str(outcome).unwrap();
    let fields = outcome.as_object_mut().unwrap();
    let usage = fields.remove("usage").unwrap_or_default();
    // Shown apart, as `Also::Driver` asks.
    fields.remove("driver");
    let ended = if let Some(reason) = returned.strip_prefix("failed: ") {
        json!({ "reason": reason, "state": "failed" })
    } else if let Some(signal) = returned.strip_prefix("was interrupted by ") {
        json!({ "signal": signal, "state": "interrupted" })
    } else {
        let finished = (log.iter())
            .flat_map(|logged| logged["actions"].as_array().into_iter().flatten())
            .any(|action| action["kind"] == "finish");
        assert!(finished, "{name}: the log records no finish");
        assert_eq!(fields.remove("state"), Some(json!("finished")), "{name}");
        let verdict = &outcome["verdict"];
        let shown = if verdict["kind"] == "returned" {
            let values = verdict["result"]["values"].as_array().unwrap().iter();
            let values: Vec<String> = values
                .map(|shown| format!("{}={}", shown["name"].as_str().unwrap(), shown["value"]))
                .collect();
            format!("finished, showing [{}]", values.join(" "))
        } else {
            "finished without a result".to_owned()
        };
        assert_eq!(
            returned, shown,
            "{name}: `execute` returned another verdict"
        );
        return (usage, Some(outcome));
    };
    assert_eq!(outcome, ended, "{name}: outcome.json holds another outcome");
    (usage, None)
}

fn usage_holds(
    name: &str,
    record: &Path,
    log: &[Value],
    usage: &Value,
    providers: &[(&str, &Sim)],
    shows: bool,
) {
    if usage.is_null() {
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

fn sandboxes_hold(
    name: &str,
    record: &Path,
    log: &[Value],
    usage: &Value,
    providers: &[(&str, &Sim)],
) {
    let counted = usage["sandboxes"].as_array().unwrap();
    // A place times leased use once a lease's runner is placed there, whether created or not.
    let leased: BTreeSet<_> = (opened(record, log).into_iter())
        .map(|opened| opened.place)
        .collect();
    let (mut creates, mut undeleted) = (BTreeMap::new(), BTreeMap::new());
    for counts in counted {
        let provider = counts["provider"].as_str().unwrap().to_owned();
        let class = counts["class"].as_str().unwrap().to_owned();
        let leased = leased.contains(&(provider.clone(), class.clone()));
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
    // A shared role's sandbox starts with the work, by no action of its own.
    let shared = fs::read_dir(record.join("shared")).into_iter().flatten();
    let shared =
        shared.filter(|entry| entry.as_ref().unwrap().path().join("sandbox.json").exists());
    let started = (starts.count() + shared.count()) as u64;
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

/// A lease that the run's log opened, as its record places it.
struct Opened {
    /// Its runner's name as a sandbox, `lease-<number>`.
    runner: String,
    /// The name of its holder's sandbox: the agent's, a call's own target's or the shared one's.
    holder: String,
    /// Its runner's provider and class.
    place: (String, String),
    /// How long it lasts from when its runner is ready, as the open's reply gives it.
    life: Duration,
    /// When its runner was ready, since the run's start, if it was.
    ready: Option<Duration>,
}

// The leases of the log's `start_lease` actions, each with the runner that its holder's grant
// names in `ir.json`, placed as `provider.json` says.
fn opened(record: &Path, log: &[Value]) -> Vec<Opened> {
    let read = |file: &str| -> Value {
        serde_json::from_str(&fs::read_to_string(record.join(file)).unwrap()).unwrap()
    };
    let (ir, placed) = (read("ir.json"), read("provider.json"));
    let roles = ir["roles"].as_array().unwrap();
    let role = |name: &Value| roles.iter().position(|role| role["name"] == *name).unwrap();
    let agent = |caller: &Value| {
        let index = caller.as_str().unwrap().strip_prefix("agent-")?;
        Some(&roles[index.parse::<usize>().unwrap()])
    };
    // Each job's target's sessions, and the sandbox the job runs on, by the job's name.
    let mut targets = BTreeMap::new();
    let mut opened: Vec<Opened> = Vec::new();
    for line in log {
        if line["event"]["kind"] == "lease_ready" {
            let lease = usize::try_from(line["event"]["lease"].as_u64().unwrap()).unwrap();
            opened[lease].ready = Some(Duration::from_nanos(nanos(&line["at_ns"]).unwrap()));
        }
        let actions = line["actions"].as_array().into_iter().flatten();
        for action in actions.clone() {
            let job = || format!("job-{}", action["job"]);
            match action["kind"].as_str().unwrap() {
                "start_call" => {
                    let caller = agent(&action["caller"]).unwrap();
                    let function = granted(&caller["runtime"]["agent"]["grants"], &action["grant"]);
                    let target = role(&function["job"]["target"]);
                    let runtime = &roles[target]["runtime"]["target"];
                    let sandbox = if runtime["lifetime"] == "work_phase" {
                        format!("shared-{target}")
                    } else {
                        job()
                    };
                    targets.insert(job(), (&runtime["sessions"], sandbox));
                }
                "start_final_call" => {
                    let target = role(&ir["final"]["target"]);
                    targets.insert(
                        job(),
                        (&roles[target]["runtime"]["target"]["sessions"], job()),
                    );
                }
                "start_lease" => {
                    let holder = action["holder"].as_str().unwrap();
                    let (sessions, sandbox) = match agent(&action["holder"]) {
                        Some(agent) => (&agent["runtime"]["agent"]["grants"], holder.to_owned()),
                        None => targets[holder].clone(),
                    };
                    let runner = &granted(sessions, &action["grant"])["session"]["runner"];
                    let at = &placed["roles"][runner.as_str().unwrap()];
                    let field = |key: &str| at[key].as_str().unwrap().to_owned();
                    let mut replies = actions.clone().map(|action| &action["reply"]);
                    let reply = replies
                        .find(|reply| reply["lease"] == action["lease"])
                        .unwrap();
                    let life = reply["remaining"]["instance_seconds"].as_u64().unwrap();
                    assert_eq!(action["lease"], json!(opened.len()), "leases count from 0");
                    opened.push(Opened {
                        runner: format!("lease-{}", action["lease"]),
                        holder: sandbox,
                        place: (field("provider"), field("class")),
                        life: Duration::from_secs(life),
                        ready: None,
                    });
                }
                _ => {}
            }
        }
    }
    opened
}

// The grant named `name` among `grants`, as the IR lists a role's.
fn granted<'g>(grants: &'g Value, name: &Value) -> &'g Value {
    let grants = grants.as_array().unwrap();
    grants.iter().find(|grant| grant["name"] == *name).unwrap()
}

// Each sandbox's hard end, as its create took it, after the run's start, which the log's first
// record gives with the run's hard end. None is past the run's. A runner's is at most its
// holder's, and at least the earlier of that and its life after its ready.
fn sandboxes_hard_ends(
    name: &str,
    record: &Path,
    log: &[Value],
    providers: &[(&str, &Sim)],
) -> Vec<String> {
    let Some(started) = log.first().map(|first| &first["event"]) else {
        return Vec::new();
    };
    let start = Duration::from_nanos(nanos(&started["unix_ns"]).unwrap());
    let run_hard_end = Duration::from_nanos(nanos(&started["hard_end_ns"]).unwrap());
    let mut lines = Vec::new();
    let mut hard_ends = BTreeMap::new();
    for (_, provider) in providers {
        for (sandbox, hard_end) in provider.hard_ends() {
            let after = hard_end.since_epoch().saturating_sub(start);
            assert!(
                after <= run_hard_end,
                "{name}: {sandbox}'s hard end is {after:?} after the run's start, past the run's \
                 hard end, {run_hard_end:?}"
            );
            let seconds = after.as_secs();
            lines.push(format!(
                "{sandbox}'s hard end: {seconds} s after the run's start"
            ));
            hard_ends.insert(sandbox, after);
        }
    }
    for lease in opened(record, log) {
        let Opened {
            runner,
            holder,
            life,
            ready,
            ..
        } = lease;
        let Some(&leased) = hard_ends.get(&runner) else {
            continue;
        };
        let held = hard_ends[&holder];
        assert!(
            leased <= held,
            "{name}: {runner}'s hard end is {leased:?} after the run's start, past its holder \
             {holder}'s, {held:?}"
        );
        if let Some(ready) = ready {
            let due = held.min(ready + life);
            assert!(
                leased >= due,
                "{name}: {runner}'s hard end is {leased:?} after the run's start, before \
                 {due:?}, its holder's or its life of {life:?} after its ready at {ready:?}"
            );
        }
    }
    lines
}

fn declared(name: &str, record: &Path, providers: &[(&str, &Sim)]) -> Vec<String> {
    let recorded = fs::read_to_string(record.join("provider.json")).unwrap();
    let recorded: Value = serde_json::from_str(&recorded).unwrap();
    let expected = (providers.iter()).map(|(name, provider)| {
        let ready = provider.ready();
        let classes = (ready.classes.iter()).map(|(class, declared)| {
            let mut class_json = json!({ "startup_seconds": declared.startup_seconds });
            if let Some(price) = declared.usd_per_hour {
                class_json["usd_per_hour"] = json!(price);
            }
            (class.to_string(), class_json)
        });
        let classes = Value::Object(classes.collect());
        let declared = json!({ "report": ready.report, "classes": classes });
        ((*name).to_owned(), declared)
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
    if providers.len() > 1 {
        lines.push(format!(
            "provider.json places the roles: {}",
            recorded["roles"]
        ));
    }
    lines
}

// What a reap of the ended run on `providers` returns, and each line of its `reaps.jsonl`.
fn reaped(record: &Path, providers: &[(&str, &Sim)]) -> Vec<String> {
    let names = providers.iter().map(|(name, _)| (*name).to_owned());
    let names: Vec<ir::Name> = names
        .map(|name| ir::Name::try_from(name).unwrap())
        .collect();
    let sweeps = (names.iter().zip(providers)).map(|(name, (_, sim))| (name, *sim as &dyn Sweep));
    let reaped = Record::open(record)
        .unwrap()
        .reap(&sweeps.collect::<Vec<_>>());
    let Reaped {
        sandboxes,
        ended,
        sockets: _,
        lost: _,
        failed,
    } = reaped.unwrap();
    let removed = (sandboxes.iter()).flat_map(|(name, ids)| ids.iter().map(move |id| (name, id)));
    let ended = ended.iter().map(|(name, id)| (name, id));
    let shown = removed
        .chain(ended)
        .map(|(name, id)| format!("{name}/{}", id.as_str()));
    let mut lines = vec![format!("reaped: {:?}", shown.collect::<Vec<_>>())];
    lines.extend(failed.map(|failed| format!("the reap failed: {failed}")));
    let reaps = fs::read_to_string(record.join("reaps.jsonl")).unwrap_or_default();
    for line in reaps.lines() {
        let mut line: Value = serde_json::from_str(line).unwrap();
        line["at_ns"] = json!("<ns>");
        lines.push(format!("reaps.jsonl: {line}"));
    }
    lines
}

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

struct StandIn {
    address: String,
    stops: Sender<()>,
    thread: JoinHandle<(Vec<Vec<u8>>, Vec<u64>)>,
}

impl StandIn {
    // On a simulated host, `time` times each hang-up.
    fn start(time: Option<SimTime>) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (stops, stopped) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut answering = Vec::new();
            for connection in listener.incoming() {
                if stopped.try_recv().is_ok() {
                    break;
                }
                let connection = connection.unwrap();
                // Each connection is answered on a thread of its own, so that a stalled stream
                // holds no other request.
                let time = time.clone();
                answering.push(thread::spawn(move || answer(connection, time.as_ref())));
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

    fn servings(stand_in: Option<&Self>, workflow: &Workflow, dir: &Path) -> ByRole<Vec<Serving>> {
        let servings = workflow.by_role(|(role, _)| {
            let entries = (workflow.model(role).into_iter()).flat_map(ir::ModelRoute::as_slice);
            let served = entries.map(|entry| stand_in.unwrap().serving(dir, entry));
            Ok::<_, Infallible>(served.collect())
        });
        let Ok(servings) = servings;
        servings
    }

    fn serving(&self, dir: &Path, entry: &ir::ApiRoute) -> Serving {
        let models = entry.models.as_slice().iter().map(ModelName::as_str);
        let models: Vec<&str> = models.collect();
        let path = match entry.api {
            ir::Api::Responses => "responses",
            ir::Api::Chat => "chat/completions",
            ir::Api::Messages => "messages",
            ir::Api::GenerateContent => "models",
        };
        let url = format!("http://{}/v1/{path}", self.address);
        Serving {
            api: entry.api,
            ..serving(dir, &url, &models)
        }
    }

    fn stop(self) -> (Vec<Vec<u8>>, Vec<u64>) {
        self.stops.send(()).unwrap();
        // The accept loop blocks in `incoming`: a connection wakes it to see the stop.
        drop(TcpStream::connect(&self.address).unwrap());
        self.thread.join().unwrap()
    }
}

fn answer(mut connection: TcpStream, time: Option<&SimTime>) -> (Vec<u8>, Option<u64>) {
    let (line, headers, body) = read_request(&mut BufReader::new(&connection));
    let received = body.clone();
    let key = headers.iter().find(|(name, _)| name == "authorization");
    let input: Value = serde_json::from_slice(&body).unwrap();
    let conversation = input.get("input").unwrap_or(&input["messages"]);
    let said = (conversation.as_str()).or_else(|| conversation[1]["content"].as_str());
    let refused = match said {
        Some("expired") => Some(("401 Unauthorized", "")),
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
    if let Some(said @ ("usage" | "cut")) = said {
        completed(&mut connection, said == "usage");
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
    (received, Some(time.map_or_else(unix_ns, sim_unix_ns)))
}

fn completed(connection: &mut TcpStream, ended: bool) {
    let completed = r#"{"type":"response.completed","response":{"usage":{"input_tokens":12,"input_tokens_details":{"cached_tokens":8},"output_tokens":5,"output_tokens_details":{"reasoning_tokens":3}}}}"#;
    write_chunk(
        connection,
        "data: {\"type\":\"response.created\",\"response\":{\"usage\":null}}\n\n\
         data: {\"type\":\"response.output_text.delta\",\"delta\":\"6\"}\n\n",
    );
    write_chunk(
        connection,
        &format!("event: response.completed\ndata: {completed}\n\n"),
    );
    if ended {
        write_chunk(connection, "");
    }
}

trait Shown: Host {
    const SCRIPTED: bool;

    /// The clock that the simulated provider waits on, if the host's is simulated.
    fn time(&self) -> Option<SimTime>;
}

impl Shown for System {
    const SCRIPTED: bool = false;

    fn time(&self) -> Option<SimTime> {
        None
    }
}

impl Shown for SimHost {
    const SCRIPTED: bool = true;

    fn time(&self) -> Option<SimTime> {
        Some(Self::time(self))
    }
}

fn logged(
    log: Vec<Value>,
    started: &RangeInclusive<u64>,
    scripted: bool,
    also: &[Also],
) -> (Vec<String>, Option<u64>, Option<Duration>) {
    let (mut records, mut start, mut dropped) = (Vec::new(), None, 0);
    let last = in_order(&log);
    for mut logged in log {
        // A poll answered by its own kind, and a call refused as busy, which the agent makes
        // again, are dropped, as how many an `await` or a call makes depends on timing, and the
        // later op numbers drop with them.
        // A sandbox's start, which `in_order` checks, is dropped too, as its place depends on
        // timing.
        let event = &logged["event"]["kind"];
        let starts = ["sandbox_created", "sandbox_met", "container_met"];
        if starts.iter().any(|kind| event == kind) && !also.contains(&Also::Starts) {
            continue;
        }
        let kind = &logged["event"]["request"]["kind"];
        if kind == "call" && logged.pointer("/actions/0/reply/busy") == Some(&json!(true)) {
            dropped += 1;
            continue;
        }
        if (kind == "status" || kind == "lease_status")
            && logged.pointer("/actions/0/reply/kind") == Some(kind)
        {
            let reply = json!([{ "kind": "reply", "reply": { "kind": kind } }]);
            assert_eq!(
                logged["actions"], reply,
                "a poll's line holds its reply's body"
            );
            dropped += 1;
            continue;
        }
        if let Some(op) = logged.pointer_mut("/event/op") {
            *op = json!(op.as_u64().unwrap() - dropped);
        }
        // A scripted host's clock gives exact times, which stay; a real clock's are hidden.
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
    (numbered_apart(records, also), start, last)
}

// The meetings of the containers of the sandbox that `also` names, if it names one. They are
// each `container_met` and the `sandbox_met`, in the log's order, with when each was observed.
fn met(log: &[Value], also: &Also) -> Vec<String> {
    let Also::Met(sandbox) = also else {
        return Vec::new();
    };
    let met = log.iter().filter_map(|logged| {
        let event = &logged["event"];
        let met = event["kind"] == "container_met" || event["kind"] == "sandbox_met";
        let observed = Duration::from_nanos(nanos(&logged["observed_ns"])?);
        (met && event["sandbox"] == *sandbox)
            .then(|| format!("  {event}, observed at {observed:?}"))
    });
    let mut lines = vec![format!("the meetings of {sandbox}'s containers:")];
    lines.extend(met);
    lines
}

// The records numbered in order, then those of a racing kind, sorted, with no place.
fn numbered_apart(records: Vec<Value>, also: &[Also]) -> Vec<String> {
    let races = |logged: &Value| {
        let line = logged.to_string();
        (also.iter()).any(|also| matches!(also, Also::Racing(part) if line.contains(part)))
    };
    let (racing, records): (Vec<Value>, _) = records.into_iter().partition(races);
    let mut lines = numbered(records);
    let mut racing: Vec<String> = (racing.into_iter())
        .map(|mut logged| {
            logged.as_object_mut().unwrap().remove("seq");
            elided(&logged.to_string())
        })
        .collect();
    if !racing.is_empty() {
        racing.sort_unstable();
        lines.push("log.jsonl, at no fixed place:".to_owned());
        lines.extend(racing);
    }
    lines
}

fn polls(log: &[Value]) -> Vec<String> {
    let mut polls = BTreeSet::new();
    for logged in log {
        let request = &logged["event"]["request"];
        let (kind, reply) = (&request["kind"], &logged["actions"][0]["reply"]);
        if (kind == "status" || kind == "lease_status") && reply["kind"] == *kind {
            polls.insert(format!("a poll, {request}, is answered {reply}"));
        }
    }
    polls.into_iter().collect()
}

fn in_order(log: &[Value]) -> Option<Duration> {
    let time = |ns: &Value| Duration::from_nanos(nanos(ns).unwrap());
    let (mut last, mut started, mut created) = (None, BTreeMap::new(), BTreeMap::new());
    // When the last container of each sandbox to meet did.
    let mut containers = BTreeMap::new();
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
            Some("sandbox_created") => {
                let began = Some(time(&event["began_ns"]));
                assert!(began <= observed, "{logged}: it began after it returned");
                for phase in event["phases"].as_array().into_iter().flatten() {
                    let start = Some(time(&phase["began_ns"]));
                    let end = start.map(|start| start + time(&phase["took_ns"]));
                    let within = began <= start && end <= observed;
                    assert!(within, "{logged}: a phase lies outside its create");
                }
                let once = created.insert(event["sandbox"].as_str(), observed);
                assert!(once.is_none(), "{logged}: its sandbox was created before");
            }
            Some("container_met") => {
                let created = created.get(&event["sandbox"].as_str());
                let met = created.is_some_and(|created| *created <= observed);
                assert!(met, "{logged}: no create of its sandbox came before");
                let before = containers.insert(event["sandbox"].as_str(), observed);
                assert!(
                    before <= Some(observed),
                    "{logged}: a container met after it"
                );
            }
            Some("sandbox_met") => {
                let created = created.get(&event["sandbox"].as_str());
                let met = created.is_some_and(|created| *created <= observed);
                assert!(met, "{logged}: no create of its sandbox came before");
                // A sandbox that holds companions met as the last of its containers did.
                let last = containers.get(&event["sandbox"].as_str());
                assert!(last.is_none_or(|last| *last == observed), "{logged}");
            }
            Some("sandbox_deleted") if event["sandbox"].is_string() => {
                let known = created.contains_key(&event["sandbox"].as_str());
                assert!(known, "{logged}: no create of its sandbox came before");
            }
            _ => {}
        }
        last = at;
    }
    last
}

fn numbered(mut records: Vec<Value>) -> Vec<String> {
    let lines = (records.iter_mut().enumerate()).map(|(seq, record)| {
        record["seq"] = json!(seq);
        elided(&record.to_string())
    });
    lines.collect()
}

fn elided(line: &str) -> String {
    // A payload is a run of `x`; a run past 64 shows as its length, so a snapshot stays readable.
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
        // Reading the whole log at every line is quadratic: compare at the first lines, at every
        // 16th and at the last.
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

fn written(record: &Path, start: Option<u64>) -> Option<Duration> {
    let start = Duration::from_nanos(start?);
    // By the record alone: the call to `execute` also publishes before the run's clock starts, and
    // removes its sockets after its log ends.
    let modified = fs::metadata(record.join("log.jsonl")).and_then(|log| log.modified());
    let written = modified.unwrap().duration_since(UNIX_EPOCH).unwrap();
    Some(written.saturating_sub(start))
}

fn when(placed: &Placed, last: Duration) -> &'static str {
    let final_call = placed.workflow().final_call();
    let startup = placed.startup(final_call.target.role());
    let hard_end = lasts(placed);
    let teardown_end = hard_end.saturating_sub(startup + final_call.timeout);
    if last < teardown_end {
        "before the agent's teardown was to end"
    } else if last < hard_end {
        "once the agent's teardown was to end, before the hard end"
    } else {
        "once the hard end had passed"
    }
}

fn workflow(argv: &[&str]) -> Value {
    let mut ir = test_support::unrouted();
    ir["roles"][0]["runtime"]["agent"]["command"]["argv"] = json!(argv);
    ir
}

fn ops_workflow(argv: &[&str], deadline: u64) -> Value {
    let mut ir = workflow(argv);
    let train = &mut ir["roles"][0]["runtime"]["agent"]["grants"][0]["job"];
    // A call timeout shorter than the time left in any run that makes its calls soon after it
    // starts, with no cumulative limit, so that it is each call's timeout and what a reply has left
    // depends on no command's time.
    train["call_timeout_seconds"] = json!(1);
    train.as_object_mut().unwrap().remove("cumulative_seconds");
    // `train` takes 8 bytes, and the now function 16, the most an op of the agent's may carry: a
    // payload of 9 or 17 is over its limit, one of 6 is not. The final call takes what it selects.
    train["payload_max_bytes"] = json!(8);
    ir["roles"][0]["runtime"]["agent"]["grants"][1]["now"]["payload_max_bytes"] = json!(16);
    ir["final"]["payload_max_bytes"] = json!(16);
    ir["deadline_seconds"] = json!(deadline);
    ir
}

fn routed(mut ir: Value) -> Value {
    let route = json!([{ "api": "responses", "models": ["m-large"] }]);
    ir["roles"][0]["runtime"]["agent"]["egress"] = json!({ "model": route });
    ir
}

fn short_workflow(argv: &[&str], deadline: u64) -> Value {
    let mut ir = ops_workflow(argv, deadline);
    // The run's other shares of its hard end as short as they go: no finalize grace and a final
    // call's timeout of 1 s, so that a wait for the teardown's end or the hard end takes seconds.
    ir["roles"][0]["runtime"]["agent"]["finalize_grace_seconds"] = json!(0);
    ir["final"]["timeout_seconds"] = json!(1);
    ir
}

#[test]
fn deleted_by_the_sweep() {
    check(
        &SimHost::new(),
        "deleted-by-the-sweep",
        &[(PROVIDER, &Sim::new())],
        &workflow(&["exit", "0"]),
        REFUSED_ONCE,
        &expect![[r#"
            failed: deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"18270000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":3}
            {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":4}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":5}
            {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":6}
            {"at_ns":"0","note":"the run failed: deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","seq":7}
            its last record came before the agent's teardown was to end"#]],
    );
}

#[test]
fn unstarted_undeletable() {
    let mut ir = workflow(&["exit", "0"]);
    ir["roles"][0]["image"] = json!(sim::image(MUTE).as_str());
    // Once the start has read the clock past the create's record, the clock moves to its first
    // look, which finds the sandbox ended.
    let created = Synced::Holding(AGENT_SANDBOX, "sim-1");
    let shown = executed(
        &SimHost::new().moving_after_read(created, Duration::from_secs(1)),
        "unstarted-undeletable",
        &[(PROVIDER, &Sim::new())],
        &ir,
        UNDELETABLE,
        &[Also::Listing],
    );
    expect![[r#"
        failed: sandbox sim-1: it ended before it met the gateway: expected its `sandboxd` to run until it connects; its console ends: sandboxd: a simulated one, which never dials; deleting it failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `simulated` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`
        record: agents/agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
        agent `agent`'s sandbox, agents/agent/sandbox.json: {"delete":{"state":"failed"},"id":"sim-1","provider":"simulated"}
        log.jsonl:
        {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"18270000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"1000000000","event":{"kind":"work_started"},"observed_ns":"1000000000","seq":1}
        {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"agent_failed","reason":"the agent's sandbox didn't start: it ended before it met the gateway: expected its `sandboxd` to run until it connects"},"observed_ns":"1000000000","seq":2}
        {"at_ns":"1000000000","note":"the run failed: sandbox sim-1: it ended before it met the gateway: expected its `sandboxd` to run until it connects; its console ends: sandboxd: a simulated one, which never dials; deleting it failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `simulated` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","seq":3}
        its last record came before the agent's teardown was to end
        listed once it ended: ["sim-1"]"#]]
    .assert_eq(&shown);
}

#[test]
fn stalls() {
    // Once it has started, the agent's sleep moves the clock to the 1 s deadline. Then its sandbox
    // stops answering, and holds its teardown's freeze until the clock reaches its end, 3 s.
    let argv = ["ops", "await_log agent_started", "sleep 1", "stall 3"];
    let mut ir = workflow(&argv);
    ir["deadline_seconds"] = json!(1);
    ir["roles"][0]["runtime"]["agent"]["finalize_grace_seconds"] = json!(0);
    check(
        &SimHost::new(),
        "stalls",
        &[(PROVIDER, &Sim::with_startup(SHORT_STARTUP))],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, showing [score=0]
            outcome.json: {"agents":[{"role":"agent","submission":null,"work_end":{"kind":"deadline"}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":0}]}}}
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"124000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"1000000000","event":{"kind":"tick"},"observed_ns":"1000000000","seq":3}
            {"at_ns":"3000000000","note":"freezing the agent's sandbox failed: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts","seq":4}
            {"at_ns":"3000000000","note":"the agent's out is incomplete: collecting /pumpkin/out failed: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts","seq":5}
            {"at_ns":"3000000000","note":"the agent's log is incomplete: collecting /pumpkin/log failed: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts","seq":6}
            {"at_ns":"3000000000","note":"collected from agent `agent`'s sandbox: out, nothing; log, nothing","seq":7}
            {"actions":[],"at_ns":"3000000000","event":{"caller":"agent-0","kind":"agent_collected"},"seq":8}
            {"actions":[],"at_ns":"3000000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":9}
            {"actions":[{"job":0,"kind":"start_final_call","selections":[{"role":"agent","selection":null}],"timeout_ns":"120000000000"}],"at_ns":"3000000000","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"3000000000","seq":10}
            {"actions":[],"at_ns":"3000000000","event":{"job":0,"kind":"call_started"},"observed_ns":"3000000000","seq":11}
            {"actions":[],"at_ns":"3000000000","event":{"job":0,"kind":"call_returned","returned":[{"name":"score","value":0}]},"observed_ns":"3000000000","seq":12}
            {"actions":[],"at_ns":"3000000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":13}
            {"actions":[{"kind":"finish"}],"at_ns":"3000000000","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"3000000000","seq":14}
            its last record came once the agent's teardown was to end, before the hard end"#]],
    );
}

// Each start ends at its first look, which finds its sandbox ended, long before its 30 s startup:
// the clock moves past the look's interval once the start reads it past its create's record.
#[test]
fn unstarted() {
    let ir = workflow(&["exit", "0"]);
    let on = |name, sandboxd| {
        listening_too(|sim| {
            let looked = |record, id| Synced::Holding(record, id);
            let host = SimHost::new()
                .moving_after_read(looked(AGENT_SANDBOX, "sim-1"), Duration::from_secs(1))
                .moving_after_read(
                    looked("jobs/0/.sandbox.json.partial", "sim-2"),
                    Duration::from_secs(2),
                );
            executed(&host, name, &[(PROVIDER, &sim)], &ir, sandboxd, &[])
        })
    };
    let (listed, left_out) = (on("unstarted", MUTE), on("unstarted-left-out", VANISHING));
    assert_eq!(
        left_out, listed,
        "left out of the list, against listed as ended"
    );
    expect![[r#"
        finished without a result
        outcome.json: {"agents":[{"role":"agent","submission":null,"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: it ended before it met the gateway: expected its `sandboxd` to run until it connects"}}],"verdict":{"failure":{"kind":"error","reason":"the job's sandbox didn't start: sandbox sim-2: it ended before it met the gateway: expected its `sandboxd` to run until it connects; its console ends: sandboxd: a simulated one, which never dials"},"kind":"no_result"}}
        log.jsonl:
        {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"18270000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"1000000000","event":{"kind":"work_started"},"observed_ns":"1000000000","seq":1}
        {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"agent_failed","reason":"the agent's sandbox didn't start: it ended before it met the gateway: expected its `sandboxd` to run until it connects"},"observed_ns":"1000000000","seq":2}
        {"actions":[],"at_ns":"1000000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":3}
        {"at_ns":"1000000000","note":"agent `agent`: the agent's sandbox didn't start: sandbox sim-1: it ended before it met the gateway: expected its `sandboxd` to run until it connects; its console ends: sandboxd: a simulated one, which never dials","seq":4}
        {"at_ns":"1000000000","note":"the agent's sandbox never started: nothing was collected","seq":5}
        {"actions":[{"job":0,"kind":"start_final_call","selections":[{"role":"agent","selection":null}],"timeout_ns":"120000000000"}],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"1000000000","seq":6}
        {"actions":[],"at_ns":"2000000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":7}
        {"actions":[],"at_ns":"2000000000","event":{"job":0,"kind":"call_failed","reason":"the job's sandbox didn't start: sandbox sim-2: it ended before it met the gateway: expected its `sandboxd` to run until it connects; its console ends: sandboxd: a simulated one, which never dials"},"observed_ns":"2000000000","seq":8}
        {"actions":[{"kind":"finish"}],"at_ns":"2000000000","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"2000000000","seq":9}
        its last record came before the agent's teardown was to end"#]].assert_eq(&listed);
}

// A run that a signal interrupts stops as one that fails does, long before what it waits on
// (`WAITED`) would end. Each case is a test of its own: a signal interrupts every later run.
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
fn interrupted_agent_start() {
    interrupted(
        "agent-start",
        WAITED,
        &workflow(&["exit", "0"]),
        HUNG,
        At::Created("agent-0"),
        &expect![[r#"
            was interrupted by SIGTERM
            it started: true
            it returned within 60 s: true
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"<ns>","event":{"hard_end_ns":"18360000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"at_ns":"<ns>","note":"the run was interrupted by SIGTERM","seq":1}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":2}
            its last record came before the agent's teardown was to end"#]],
    );
}

#[test]
fn interrupted_call_start() {
    let mut judged = ops_workflow(&["ops", "call train 6", "await 0 completed"], WAITED);
    judged["roles"][1]["image"] = json!(sim::image(HUNG).as_str());
    // The sweep's delete of the agent's sandbox is recorded once the call's start has deleted
    // its own, which the two threads would otherwise log in either order.
    let deleted = Synced::Holding(AGENT_SANDBOX, DELETED);
    let host = SimHost::new().held_sync(deleted, r#""sandbox":"job-0""#);
    let shown = executed(
        &host,
        "interrupted-call-start",
        &[(PROVIDER, &Sim::with_startup(WAITED))],
        &judged,
        SANDBOXD,
        &[Also::Interrupted(At::Created("job-0"))],
    );
    expect![[r#"
        was interrupted by SIGTERM
        it started: true
        it returned within 60 s: true
        payloads hold: ["xxxxxx"]
        log.jsonl:
        {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"420000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
        {"actions":[{"kind":"reply","reply":{"job":0,"kind":"call","remaining":{"calls":99,"seconds":null}}},{"caller":"agent-0","grant":"train","job":0,"kind":"start_call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":3}
        {"at_ns":"0","note":"the run was interrupted by SIGTERM","seq":4}
        {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":5}
        {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":6}
        its last record came before the agent's teardown was to end"#]]
    .assert_eq(&shown);
}

#[test]
fn interrupted_with_one_undeletable() {
    // The run's sweep deletes both sandboxes at once, and fails only the agent's.
    let mut judged = ops_workflow(&["ops", "call train 6", "await 0 completed"], WAITED);
    judged["roles"][0]["image"] = json!(sim::image(UNDELETABLE).as_str());
    judged["roles"][1]["image"] = json!(sim::image(HUNG).as_str());
    interrupted(
        "one-undeletable",
        WAITED,
        &judged,
        SANDBOXD,
        At::Created("job-0"),
        &expect![[r#"
            failed: SIGTERM interrupted the run; deleting the sandboxes the run left on provider `simulated` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`
            it started: true
            it returned within 60 s: true
            agent `agent`'s sandbox, agents/agent/sandbox.json: {"delete":{"state":"failed"},"id":"sim-1","provider":"simulated"}
            payloads hold: ["xxxxxx"]
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"<ns>","event":{"hard_end_ns":"420000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"work_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[],"at_ns":"<ns>","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"<ns>","seq":2}
            {"actions":[{"kind":"reply","reply":{"job":0,"kind":"call","remaining":{"calls":99,"seconds":null}}},{"caller":"agent-0","grant":"train","job":0,"kind":"start_call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":3}
            {"at_ns":"<ns>","note":"the run was interrupted by SIGTERM","seq":4}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":5}
            {"at_ns":"<ns>","note":"the run failed: SIGTERM interrupted the run; deleting the sandboxes the run left on provider `simulated` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","seq":6}
            its last record came before the agent's teardown was to end
            listed once it ended: ["sim-1"]"#]],
    );
}

#[test]
fn reaped_past_a_failing_provider() {
    // The reap fails job 0's sandbox on `there`, its first provider, and still deletes the
    // agent's on `here`.
    let mut judged = ops_workflow(&["ops", "call train 6", "await 0 completed"], WAITED);
    judged["roles"][0]["image"] = json!(sim::image(REFUSED_ONCE).as_str());
    judged["roles"][1]["image"] = json!(sim::image(UNDELETABLE).as_str());
    let (here, there) = (
        Sim::with_startup(WAITED).named("here"),
        there(Sim::with_startup(WAITED)),
    );
    let shown = executed(
        &System,
        "reaped-past-a-failing-provider",
        &[("there", &there), ("here", &here)],
        &judged,
        SANDBOXD,
        &[Also::Interrupted(At::Created("job-0")), Also::Reaped],
    );
    expect![[r#"
        failed: SIGTERM interrupted the run; sandbox sim-1: the accept was stopped before a sandbox connected to <sockets>/job-0/s: expected its sandbox to connect before the stop; deleting it failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `here` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `there` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`
        it started: true
        it returned within 60 s: true
        agent `agent`'s sandbox, agents/agent/sandbox.json: {"delete":{"state":"failed"},"id":"sim-1","provider":"here"}
        job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"failed"},"id":"sim-1","provider":"there"}
        payloads hold: ["xxxxxx"]
        provider.json places the roles: {"agent":{"class":"sim","provider":"here"},"final_judge":{"class":"sim","provider":"there"},"trainer":{"class":"sim","provider":"there"}}
        log.jsonl:
        {"actions":[{"kind":"start_agents"}],"at_ns":"<ns>","event":{"hard_end_ns":"420000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"<ns>","event":{"kind":"work_started"},"observed_ns":"<ns>","seq":1}
        {"actions":[],"at_ns":"<ns>","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"<ns>","seq":2}
        {"actions":[{"kind":"reply","reply":{"job":0,"kind":"call","remaining":{"calls":99,"seconds":null}}},{"caller":"agent-0","grant":"train","job":0,"kind":"start_call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"<ns>","seq":3}
        {"at_ns":"<ns>","note":"the run was interrupted by SIGTERM","seq":4}
        {"at_ns":"<ns>","note":"the run failed: SIGTERM interrupted the run; sandbox sim-1: the accept was stopped before a sandbox connected to <sockets>/job-0/s: expected its sandbox to connect before the stop; deleting it failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `here` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `there` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","seq":5}
        its last record came before the agent's teardown was to end
        reaped: ["here/sim-1"]
        the reap failed: deleting the sandboxes the run left on provider `there` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`
        reaps.jsonl: {"at_ns":"<ns>","id":"sim-1","provider":"here","reaped":"removed"}
        there published: ["sandboxd /sim/sandboxd", "image sha256:2f73696d2f756e64656c657461626c6500000000000000000000000000000000", "image sha256:2222222222222222222222222222222222222222222222222222222222222222"]
        there: the class at each create: ["sim"]
        there listed once it ended: ["sim-1"]
        here published: ["sandboxd /sim/sandboxd", "image sha256:2f73696d2f726566757365642d6f6e6365000000000000000000000000000000"]
        here: the class at each create: ["sim"]"#]].assert_eq(&shown);
}

#[test]
fn interrupted_working() {
    let mut works = workflow(&["work"]);
    works["deadline_seconds"] = json!(WAITED);
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
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"<ns>","event":{"hard_end_ns":"330000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"work_started"},"observed_ns":"<ns>","seq":1}
            {"actions":[],"at_ns":"<ns>","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"<ns>","seq":2}
            {"at_ns":"<ns>","note":"the run was interrupted by SIGTERM","seq":3}
            {"actions":[],"at_ns":"<ns>","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":4}
            its last record came before the agent's teardown was to end"#]],
    );
}

#[test]
fn interrupted_during_teardown() {
    let mut ir = workflow(&["exit", "3"]);
    ir["deadline_seconds"] = json!(30);
    let shown = executed(
        &System,
        "interrupted-during-teardown",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &[Also::Interrupted(At::Delete)],
    );
    assert!(shown.starts_with("was interrupted by SIGTERM"));
    assert!(shown.contains(r#""kind":"agent_collected""#));
    assert!(shown.contains(r#""kind":"sandbox_deleted""#));
    assert!(!shown.contains(r#""kind":"start_final_call""#));
}

// The final call's start runs out at its startup, past a look whose list fails. An agent's start
// that ran out then would race the setup's wait, which ends at the startup too.
#[test]
fn unlistable() {
    let mut ir = workflow(&["exit", "0"]);
    ir["roles"][2]["image"] = json!(sim::image(UNLISTABLE).as_str());
    let created = Synced::Holding("jobs/0/.sandbox.json.partial", "sim-2");
    let startup = Duration::from_secs(SHORT_STARTUP);
    check(
        &SimHost::new().moving_after_read(created, startup),
        "unlistable",
        &[(PROVIDER, &Sim::with_startup(SHORT_STARTUP))],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished without a result
            outcome.json: {"agents":[{"role":"agent","submission":null,"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"failure":{"kind":"error","reason":"the job's sandbox didn't start: sandbox sim-2: no sandbox connected to <sockets>/job-0/s by the deadline: expected its sandbox to connect while the deadline lasts; the last list of its provider failed: the simulated list failed while sim-2 lasts: delete it, or create one with another `sandboxd`; its console ends: sandboxd: a simulated one, which never dials"},"kind":"no_result"}}
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"18183000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":3}
            {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":4}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":5}
            {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":6}
            {"actions":[{"job":0,"kind":"start_final_call","selections":[{"role":"agent","selection":null}],"timeout_ns":"120000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":7}
            {"actions":[],"at_ns":"1000000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":8}
            {"actions":[],"at_ns":"1000000000","event":{"job":0,"kind":"call_failed","reason":"the job's sandbox didn't start: sandbox sim-2: no sandbox connected to <sockets>/job-0/s by the deadline: expected its sandbox to connect while the deadline lasts; the last list of its provider failed: the simulated list failed while sim-2 lasts: delete it, or create one with another `sandboxd`; its console ends: sandboxd: a simulated one, which never dials"},"observed_ns":"1000000000","seq":9}
            {"actions":[{"kind":"finish"}],"at_ns":"1000000000","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"1000000000","seq":10}
            its last record came before the agent's teardown was to end"#]],
    );
}

#[test]
fn unpublished() {
    let shown = executed(
        &SimHost::new(),
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

#[test]
fn unpublished_image() {
    let mut ir = workflow(&["exit", "0"]);
    ir["roles"][1]["image"] = json!(sim::image(UNPUBLISHED).as_str());
    let shown = executed(
        &SimHost::new(),
        "unpublished-image",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &[Also::Listing],
    );
    expect![[r#"
        failed: publishing role `trainer`'s image on provider `simulated` failed: sha256:2f73696d2f756e7075626c697368656400000000000000000000000000000000 can't be published by the simulated provider: name another image
        record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json
        log.jsonl:"#]]
    .assert_eq(&shown);
}

// The read fails, as the size and digest differ, rather than put other bytes on the target.
#[test]
fn swapped_payload() {
    let swapped = executed(
        &SimHost::new(),
        "swapped-payload",
        &[(PROVIDER, &Sim::new())],
        &ops_workflow(&["ops", "call submit 6 finalize=true"], 30),
        SANDBOXD,
        &[Also::Interrupted(At::SwapPayloads)],
    );
    expect![[r#"
        finished without a result
        payloads hold: ["yyyyyyyyy"]
        outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"failure":{"kind":"error","reason":"<record>/payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd holds 9 bytes of digest sha256:cf0da94d4b6a0de4d83a79dfe964489ba50a38aef90bc406aed5558fee383781: expected the payload's 6 bytes of digest sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd"},"kind":"no_result"}}
        log.jsonl:
        {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
        {"actions":[{"kind":"reply","reply":{"kind":"call","remaining_seconds":30,"returned":{"state":"accepted"}}}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"evaluated":{"effects":["set_submission","end_work"],"returned":[{"name":"state","value":"accepted"}]},"grant":"submit","kind":"now","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":3}
        {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":4}
        {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":5}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":6}
        {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":7}
        {"actions":[{"job":0,"kind":"start_final_call","selections":[{"role":"agent","selection":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}],"timeout_ns":"120000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":8}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":9}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_failed","reason":"<record>/payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd holds 9 bytes of digest sha256:cf0da94d4b6a0de4d83a79dfe964489ba50a38aef90bc406aed5558fee383781: expected the payload's 6 bytes of digest sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd"},"observed_ns":"0","seq":10}
        {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":11}
        {"actions":[{"kind":"finish"}],"at_ns":"0","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"0","seq":12}
        its last record came before the agent's teardown was to end"#]].assert_eq(&swapped);
}

#[test]
fn result_over_the_limit() {
    let shown = executed(
        &SimHost::new(),
        "result-over-the-limit",
        &[(PROVIDER, &Sim::new())],
        &ops_workflow(&["ops", "call submit large finalize=true"], 30),
        SANDBOXD,
        &[Also::Listing],
    );
    expect![[r#"
        finished without a result
        record: agents/agent/log/collected, agents/agent/out/collected, agents/agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/got/1/, jobs/0/log/0/collected, jobs/0/out/collected, jobs/0/out/result.json, jobs/0/sandbox.json, log.jsonl, outcome.json, payloads/d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9, provider.json, record.json
        payloads hold: ["large"]
        outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9","size":5}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"failure":{"kind":"error","reason":"getting /pumpkin/out/result.json failed: the tree's files hold more than 1048576 bytes"},"kind":"no_result"}}
        log.jsonl:
        {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
        {"actions":[{"kind":"reply","reply":{"kind":"call","remaining_seconds":30,"returned":{"state":"accepted"}}}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"evaluated":{"effects":["set_submission","end_work"],"returned":[{"name":"state","value":"accepted"}]},"grant":"submit","kind":"now","payload":{"digest":"sha256:d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9","size":5}}},"observed_ns":"0","seq":3}
        {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":4}
        {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":5}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":6}
        {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":7}
        {"actions":[{"job":0,"kind":"start_final_call","selections":[{"role":"agent","selection":{"digest":"sha256:d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9","size":5}}],"timeout_ns":"120000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":8}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":9}
        {"actions":[],"at_ns":"0","event":{"bytes":5,"digest":"sha256:d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9","job":0,"kind":"target_put","path":"/pumpkin/in/payload/raw"},"observed_ns":"0","seq":10}
        {"actions":[],"at_ns":"0","event":{"argv":["judge","/pumpkin/in/payload/raw"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"0","seq":11}
        {"actions":[],"at_ns":"0","event":{"command":0,"end":{"exited":{"code":0}},"job":0,"kind":"command_ended"},"observed_ns":"0","seq":12}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_failed","reason":"getting /pumpkin/out/result.json failed: the tree's files hold more than 1048576 bytes"},"observed_ns":"0","seq":13}
        {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":14}
        {"actions":[{"kind":"finish"}],"at_ns":"0","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"0","seq":15}
        its last record came before the agent's teardown was to end"#]]
    .assert_eq(&shown);
}

#[test]
fn crashes_with_an_op_in_flight() {
    let mut ir = ops_workflow(&["ops", "call train 12", "crash"], 30);
    // Over the final call's payload but within `train`'s, so within the most an op may carry.
    ir["roles"][0]["runtime"]["agent"]["grants"][0]["job"]["payload_max_bytes"] = json!(16);
    ir["final"]["payload_max_bytes"] = json!(8);
    // The call's sandbox is recorded once the agent's teardown is, so that the agent's end stops
    // the call before its body starts.
    let created = Synced::Holding("jobs/0/.sandbox.json.partial", "sim-2");
    check(
        &SimHost::new().held_sync(created, r#""kind":"agent_torn_down""#),
        "crashes-with-an-op",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, showing [score=0]
            payloads hold: ["xxxxxxxxxxxx"]
            outcome.json: {"agents":[{"role":"agent","submission":null,"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":0}]}}}
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"kind":"reply","reply":{"job":0,"kind":"call","remaining":{"calls":99,"seconds":null}}},{"caller":"agent-0","grant":"train","job":0,"kind":"start_call","payload":{"digest":"sha256:59ffe12a70df15109e0345955e3230a978f31ebc28d8fe3e42d306afb28b8e81","size":12},"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"call","payload":{"digest":"sha256:59ffe12a70df15109e0345955e3230a978f31ebc28d8fe3e42d306afb28b8e81","size":12}}},"observed_ns":"0","seq":3}
            {"at_ns":"0","note":"op 0's reply failed: the sandbox ended the session","seq":4}
            {"actions":[{"job":0,"kind":"stop_call"},{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"},"observed_ns":"0","seq":5}
            {"at_ns":"0","note":"freezing the agent's sandbox failed: the sandbox ended the session","seq":6}
            {"at_ns":"0","note":"the agent's out is incomplete: collecting /pumpkin/out failed: the sandbox ended the session","seq":7}
            {"at_ns":"0","note":"the agent's log is incomplete: collecting /pumpkin/log failed: the sandbox ended the session","seq":8}
            {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, nothing; log, nothing","seq":9}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":10}
            {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":11}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":12}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_failed","reason":"the call was stopped"},"observed_ns":"0","seq":13}
            {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":14}
            {"actions":[{"job":1,"kind":"start_final_call","selections":[{"role":"agent","selection":null}],"timeout_ns":"120000000000"}],"at_ns":"0","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"0","seq":15}
            {"actions":[],"at_ns":"0","event":{"job":1,"kind":"call_started"},"observed_ns":"0","seq":16}
            {"actions":[],"at_ns":"0","event":{"job":1,"kind":"call_returned","returned":[{"name":"score","value":0}]},"observed_ns":"0","seq":17}
            {"actions":[],"at_ns":"0","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-1"},"seq":18}
            {"actions":[{"kind":"finish"}],"at_ns":"0","event":{"job":1,"kind":"call_torn_down"},"observed_ns":"0","seq":19}
            its last record came before the agent's teardown was to end"#]],
    );
}

// An agent's call starts by the work phase's end, however long the provider's startup.
#[test]
fn call_starts_past_the_deadline() {
    // A deadline that the create, which begins once the agent has started, outlasts, and that
    // leaves the call its whole timeout.
    let mut ir = ops_workflow(&["ops", "call train 6", "exit"], 2);
    ir["roles"][1]["image"] = json!(sim::image(SLOW).as_str());
    // The clock reaches the deadline, where the create gives up, once the agent's teardown is
    // logged.
    let host = SimHost::new();
    let sim = Sim::new().once_logged(
        &[r#""kind":"agent_torn_down""#],
        host.moving_to(Duration::from_secs(2)),
    );
    check(
        &host,
        "call-starts-past-the-deadline",
        &[(PROVIDER, &sim)],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, showing [score=0]
            payloads hold: ["xxxxxx"]
            outcome.json: {"agents":[{"role":"agent","submission":null,"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":0}]}}}
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"272000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"kind":"reply","reply":{"job":0,"kind":"call","remaining":{"calls":99,"seconds":null}}},{"caller":"agent-0","grant":"train","job":0,"kind":"start_call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":3}
            {"actions":[{"job":0,"kind":"stop_call"},{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":4}
            {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":5}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":6}
            {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":7}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":8}
            {"actions":[],"at_ns":"2000000000","event":{"job":0,"kind":"call_failed","reason":"the job's sandbox didn't start: the simulated create gave up at its deadline, and can't name what it made: expected the run's list to find it"},"observed_ns":"2000000000","seq":9}
            {"actions":[{"job":1,"kind":"start_final_call","selections":[{"role":"agent","selection":null}],"timeout_ns":"120000000000"}],"at_ns":"2000000000","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"2000000000","seq":10}
            {"actions":[],"at_ns":"2000000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":null},"seq":11}
            {"actions":[],"at_ns":"2000000000","event":{"job":1,"kind":"call_started"},"observed_ns":"2000000000","seq":12}
            {"actions":[],"at_ns":"2000000000","event":{"job":1,"kind":"call_returned","returned":[{"name":"score","value":0}]},"observed_ns":"2000000000","seq":13}
            {"actions":[],"at_ns":"2000000000","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-1"},"seq":14}
            {"actions":[{"kind":"finish"}],"at_ns":"2000000000","event":{"job":1,"kind":"call_torn_down"},"observed_ns":"2000000000","seq":15}
            its last record came before the agent's teardown was to end"#]],
    );
}

#[test]
fn call_placed_apart() {
    // The call's start runs out at its target's class's startup, though the agent's class gives
    // 30 s, so the agent sees the call errored long before the deadline. The create's record
    // lasts until that startup.
    let mut ir = ops_workflow(&["ops", "call train 6", "await 0 errored"], 30);
    ir["roles"][1]["image"] = json!(sim::image(HUNG).as_str());
    ir["roles"][1]["needs"]["cpu_model"] = json!(BRIEF);
    let startup = SlowSync {
        record: Synced::Holding("jobs/0/.sandbox.json.partial", "sim-2"),
        queued: 0,
        at: Duration::from_secs(1),
    };
    check(
        &SimHost::new().slow_sync(startup),
        "call-placed-apart",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, showing [score=0]
            payloads hold: ["xxxxxx"]
            outcome.json: {"agents":[{"role":"agent","submission":null,"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":0}]}}}
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"kind":"reply","reply":{"job":0,"kind":"call","remaining":{"calls":99,"seconds":null}}},{"caller":"agent-0","grant":"train","job":0,"kind":"start_call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":3}
            {"actions":[],"at_ns":"1000000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":4}
            {"actions":[],"at_ns":"1000000000","event":{"job":0,"kind":"call_failed","reason":"the job's sandbox didn't start: sandbox sim-2: no sandbox connected to <sockets>/job-0/s by the deadline: expected its sandbox to connect while the deadline lasts; its console ends: sandboxd: a simulated one, which never dials"},"observed_ns":"1000000000","seq":5}
            {"actions":[],"at_ns":"1000000000","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"1000000000","seq":6}
            {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"1000000000","seq":7}
            {"at_ns":"1000000000","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":8}
            {"actions":[],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"agent_collected"},"seq":9}
            {"actions":[],"at_ns":"1000000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":10}
            {"actions":[{"job":1,"kind":"start_final_call","selections":[{"role":"agent","selection":null}],"timeout_ns":"120000000000"}],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"1000000000","seq":11}
            {"actions":[],"at_ns":"1000000000","event":{"job":1,"kind":"call_started"},"observed_ns":"1000000000","seq":12}
            {"actions":[],"at_ns":"1000000000","event":{"job":1,"kind":"call_returned","returned":[{"name":"score","value":0}]},"observed_ns":"1000000000","seq":13}
            {"actions":[],"at_ns":"1000000000","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-1"},"seq":14}
            {"actions":[{"kind":"finish"}],"at_ns":"1000000000","event":{"job":1,"kind":"call_torn_down"},"observed_ns":"1000000000","seq":15}
            its last record came before the agent's teardown was to end"#]],
    );
}

fn there(sim: Sim) -> Sim {
    // Less memory than the agent needs, so that a role with the agent's needs is placed on `here`,
    // and more than the trainer and the final call's target need, so that they are placed on
    // `there`.
    sim.named("there").offering_memory(16 << 30)
}

#[test]
fn sweeps_each_provider() {
    let mut ir = ops_workflow(&["ops", "call train 6", "await 0 errored"], 30);
    ir["roles"][0]["image"] = json!(sim::image(UNDELETABLE).as_str());
    ir["roles"][1]["image"] = json!(sim::image(LOST).as_str());
    let (here, there) = (Sim::new().named("here"), there(Sim::new()));
    let providers = [("here", &here), ("there", &there)];
    check(
        &SimHost::new(),
        "sweeps-each-provider",
        &providers,
        &ir,
        SANDBOXD,
        &expect![[r#"
            failed: deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `here` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`
            agent `agent`'s sandbox, agents/agent/sandbox.json: {"delete":{"state":"failed"},"id":"sim-1","provider":"here"}
            payloads hold: ["xxxxxx"]
            provider.json places the roles: {"agent":{"class":"sim","provider":"here"},"final_judge":{"class":"sim","provider":"there"},"trainer":{"class":"sim","provider":"there"}}
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"kind":"reply","reply":{"job":0,"kind":"call","remaining":{"calls":99,"seconds":null}}},{"caller":"agent-0","grant":"train","job":0,"kind":"start_call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":3}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_failed","reason":"the job's sandbox didn't start: the simulated create failed, and can't name what it made: expected the run's list to find it"},"observed_ns":"0","seq":4}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"0","seq":5}
            {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":6}
            {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":7}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":8}
            {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"there","sandbox":null},"seq":9}
            {"at_ns":"0","note":"the run failed: deleting the sandbox failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `here` failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","seq":10}
            its last record came before the agent's teardown was to end
            here published: ["sandboxd /sim/sandboxd", "image sha256:2f73696d2f756e64656c657461626c6500000000000000000000000000000000"]
            here: the class at each create: ["sim"]
            here listed once it ended: ["sim-1"]
            there published: ["sandboxd /sim/sandboxd", "image sha256:2f73696d2f6c6f73740000000000000000000000000000000000000000000000", "image sha256:2222222222222222222222222222222222222222222222222222222222222222"]
            there: the class at each create: ["sim"]"#]],
    );
}

// Each start looks at its own provider's list.
#[test]
fn watches_each_provider() {
    let argv = [
        "ops",
        "call near 6",
        "call train 6",
        "await 0 errored",
        "await 1 errored",
    ];
    let mut ir = ops_workflow(&argv, 30);
    ir["roles"][1]["needs"]["cpu_model"] = json!(BRIEF);
    ir["roles"][1]["image"] = json!(sim::image(MUTE).as_str());
    let mut near = ir["roles"][1].clone();
    near["name"] = json!("nearer");
    near["needs"] = ir["roles"][0]["needs"].clone();
    near["image"] = json!(sim::image(HUNG).as_str());
    ir["roles"].as_array_mut().unwrap().push(near);
    let grants = &mut ir["roles"][0]["runtime"]["agent"]["grants"];
    let mut grant = grants[0].clone();
    grant["name"] = json!("near");
    grant["job"]["target"] = json!("nearer");
    grants.as_array_mut().unwrap().push(grant);
    // At half the train call's startup, once its start reads the clock past its create's record,
    // its look at `there` finds its sandbox ended, where one at `here` would leave it waiting.
    let created = Synced::Holding("jobs/1/.sandbox.json.partial", "sim-1");
    // The run log's next record after the train call's end lasts until the near call's startup.
    let startup = SlowSync {
        record: Synced::Holding("log.jsonl", r#""event":{"job":1,"kind":"call_torn_down"}"#),
        queued: 0,
        at: Duration::from_secs(4),
    };
    let here = Sim::with_startup(4).named("here");
    let there = there(Sim::new());
    let providers = [("here", &here), ("there", &there)];
    check(
        &SimHost::new()
            .moving_after_read(created, Duration::from_millis(500))
            .slow_sync(startup),
        "watches-each-provider",
        &providers,
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, showing [score=0]
            payloads hold: ["xxxxxx"]
            provider.json places the roles: {"agent":{"class":"sim","provider":"here"},"final_judge":{"class":"sim","provider":"there"},"nearer":{"class":"sim","provider":"here"},"trainer":{"class":"brief","provider":"there"}}
            outcome.json: {"agents":[{"role":"agent","submission":null,"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":0}]}}}
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"248000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"kind":"reply","reply":{"job":0,"kind":"call","remaining":{"calls":99,"seconds":null}}},{"caller":"agent-0","grant":"near","job":0,"kind":"start_call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"near","kind":"call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":3}
            {"actions":[{"kind":"reply","reply":{"job":1,"kind":"call","remaining":{"calls":99,"seconds":null}}},{"caller":"agent-0","grant":"train","job":1,"kind":"start_call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":1,"request":{"grant":"train","kind":"call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":4}
            {"actions":[],"at_ns":"500000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"there","sandbox":"job-1"},"seq":5}
            {"actions":[],"at_ns":"500000000","event":{"job":1,"kind":"call_failed","reason":"the job's sandbox didn't start: sandbox sim-1: it ended before it met the gateway: expected its `sandboxd` to run until it connects; its console ends: sandboxd: a simulated one, which never dials"},"observed_ns":"500000000","seq":6}
            {"actions":[],"at_ns":"500000000","event":{"job":1,"kind":"call_torn_down"},"observed_ns":"500000000","seq":7}
            {"actions":[],"at_ns":"4000000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"here","sandbox":"job-0"},"seq":8}
            {"actions":[],"at_ns":"4000000000","event":{"job":0,"kind":"call_failed","reason":"the job's sandbox didn't start: sandbox sim-2: no sandbox connected to <sockets>/job-0/s by the deadline: expected its sandbox to connect while the deadline lasts; its console ends: sandboxd: a simulated one, which never dials"},"observed_ns":"4000000000","seq":9}
            {"actions":[],"at_ns":"4000000000","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"4000000000","seq":10}
            {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"4000000000","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"4000000000","seq":11}
            {"at_ns":"4000000000","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":12}
            {"actions":[],"at_ns":"4000000000","event":{"caller":"agent-0","kind":"agent_collected"},"seq":13}
            {"actions":[],"at_ns":"4000000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"here","sandbox":"agent-0"},"seq":14}
            {"actions":[{"job":2,"kind":"start_final_call","selections":[{"role":"agent","selection":null}],"timeout_ns":"120000000000"}],"at_ns":"4000000000","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"4000000000","seq":15}
            {"actions":[],"at_ns":"4000000000","event":{"job":2,"kind":"call_started"},"observed_ns":"4000000000","seq":16}
            {"actions":[],"at_ns":"4000000000","event":{"job":2,"kind":"call_returned","returned":[{"name":"score","value":0}]},"observed_ns":"4000000000","seq":17}
            {"actions":[],"at_ns":"4000000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"there","sandbox":"job-2"},"seq":18}
            {"actions":[{"kind":"finish"}],"at_ns":"4000000000","event":{"job":2,"kind":"call_torn_down"},"observed_ns":"4000000000","seq":19}
            its last record came before the agent's teardown was to end
            here published: ["sandboxd /sim/sandboxd", "image sha256:1111111111111111111111111111111111111111111111111111111111111111", "image sha256:2f73696d2f68756e670000000000000000000000000000000000000000000000"]
            here: the class at each create: ["sim", "sim"]
            there published: ["sandboxd /sim/sandboxd", "image sha256:2f73696d2f6d7574650000000000000000000000000000000000000000000000", "image sha256:2222222222222222222222222222222222222222222222222222222222222222"]
            there: the class at each create: ["brief", "sim"]"#]],
    );
}

// The call's delete waits for the agent's, whose record moves the clock past the hard end: its
// tick abandons the run before the call's teardown, which the core waits on.
#[test]
fn abandoned() {
    let argv = ["ops", "call train 6", "await_result 0 completed", "exit"];
    let mut ir = short_workflow(&argv, 2);
    ir["roles"][1]["image"] = json!(sim::image(LAST).as_str());
    let slow = SlowSync {
        record: Synced::Holding(AGENT_SANDBOX, DELETED),
        queued: 0,
        at: PAST_THE_HARD_END,
    };
    // The move at the agent's delete wakes the tick beside the agent's last records, and past the
    // abandon nothing orders the agent's teardown among the call's reports.
    let racing = [
        Also::Racing(r#""kind":"agent_torn_down""#),
        Also::Racing(r#""sandbox":"agent-0""#),
    ];
    let shown = executed(
        &SimHost::new().slow_sync(slow),
        "abandoned",
        &[(PROVIDER, &Sim::with_startup(SHORT_STARTUP))],
        &ir,
        SANDBOXD,
        &racing,
    );
    expect![[r#"
        failed: the run was still going at its hard end, 6 s after it started: expected it to end within its deadline, its finalize grace, its final call's timeout and its sandboxes' startups
        payloads hold: ["xxxxxx"]
        log.jsonl:
        {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"6000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
        {"actions":[{"kind":"reply","reply":{"job":0,"kind":"call","remaining":{"calls":99,"seconds":null}}},{"caller":"agent-0","grant":"train","job":0,"kind":"start_call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":3}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":4}
        {"actions":[],"at_ns":"0","event":{"bytes":6,"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","job":0,"kind":"target_put","path":"/work/payload"},"observed_ns":"0","seq":5}
        {"actions":[],"at_ns":"0","event":{"argv":["judge","/work/payload","/work/result.json"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"0","seq":6}
        {"actions":[],"at_ns":"0","event":{"command":0,"end":{"exited":{"code":0}},"job":0,"kind":"command_ended"},"observed_ns":"0","seq":7}
        {"actions":[],"at_ns":"0","event":{"bytes":15,"digest":"sha256:68e3ea1c52db57d6e28c6d873ef07678bc738b836a1ec8ae5f831875083035e9","job":0,"kind":"target_get","path":"/work/result.json"},"observed_ns":"0","seq":8}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_returned","returned":[{"name":"raw_score","value":6}]},"observed_ns":"0","seq":9}
        {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":10}
        {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":11}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":12}
        {"actions":[{"kind":"abandon"}],"at_ns":"6500000000","event":{"kind":"tick"},"observed_ns":"6500000000","seq":13}
        {"actions":[],"at_ns":"6500000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":14}
        {"actions":[],"at_ns":"6500000000","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"6500000000","seq":15}
        {"at_ns":"6500000000","note":"the run failed: the run was still going at its hard end, 6 s after it started: expected it to end within its deadline, its finalize grace, its final call's timeout and its sandboxes' startups","seq":16}
        log.jsonl, at no fixed place:
        {"actions":[],"at_ns":"6500000000","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"6500000000"}
        {"actions":[],"at_ns":"6500000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"}}
        its last record came once the hard end had passed"#]].assert_eq(&shown);
}

// The clock passes the hard end once the call has taken its stop, at the agent's exit: the call's
// failure, observed past the hard end, comes after the tick that abandons the run.
#[test]
fn stopped_past_the_hard_end() {
    let argv = ["ops", "call train 6", "await_log target_run", "exit"];
    let mut ir = short_workflow(&argv, 2);
    // The command stalls its sandbox, which moves the clock past the hard end as it takes the
    // freeze that follows the call's stop, once the stop alone has cut the call.
    let train = &mut ir["roles"][0]["runtime"]["agent"]["grants"][0]["job"];
    train["body"]["def"] = json!("run");
    train["keywords"] = json!({ "argv": ["stall", "7"] });
    let dir = TestDir::new();
    let host = SimHost::new();
    let (record, outcome) = execute_agents(&host, SHORT_STARTUP, ir, &dir, None);
    let Err(runtime::Stopped::Failed(reason)) = outcome else {
        panic!("expected the run to be abandoned: {outcome:?}")
    };
    assert!(
        reason.starts_with("the run was still going at its hard end"),
        "{reason}"
    );
    let log = read_log(&record);
    let abandon = |line: &Value| line["actions"][0]["kind"] == "abandon";
    let abandoned = log.iter().position(abandon).unwrap();
    let failed = |line: &Value| line["event"]["kind"] == "call_failed";
    let failed = log.iter().position(failed).unwrap();
    assert!(abandoned < failed);
    assert_eq!(log[failed]["event"]["reason"], "the call was stopped");
    assert_eq!(log[failed]["observed_ns"], "7000000000");
}

// The call's delete waits for the agent's, which begins once the agent has heard the result: the
// result comes before the delete, which the final call waits for, as the finish waits for its.
#[test]
fn returns_before_its_delete() {
    let argv = ["ops", "call train 6", "await_result 0 completed", "exit"];
    let mut ir = short_workflow(&argv, 30);
    ir["roles"][1]["image"] = json!(sim::image(LAST).as_str());
    // The call's delete is recorded once the agent's teardown is.
    let deleted = Synced::Holding("jobs/0/.sandbox.json.partial", DELETED);
    check(
        &SimHost::new().held_sync(deleted, r#""kind":"agent_torn_down""#),
        "returns-before-its-delete",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, showing [score=0]
            payloads hold: ["xxxxxx"]
            outcome.json: {"agents":[{"role":"agent","submission":null,"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":0}]}}}
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"kind":"reply","reply":{"job":0,"kind":"call","remaining":{"calls":99,"seconds":null}}},{"caller":"agent-0","grant":"train","job":0,"kind":"start_call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":3}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":4}
            {"actions":[],"at_ns":"0","event":{"bytes":6,"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","job":0,"kind":"target_put","path":"/work/payload"},"observed_ns":"0","seq":5}
            {"actions":[],"at_ns":"0","event":{"argv":["judge","/work/payload","/work/result.json"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"0","seq":6}
            {"actions":[],"at_ns":"0","event":{"command":0,"end":{"exited":{"code":0}},"job":0,"kind":"command_ended"},"observed_ns":"0","seq":7}
            {"actions":[],"at_ns":"0","event":{"bytes":15,"digest":"sha256:68e3ea1c52db57d6e28c6d873ef07678bc738b836a1ec8ae5f831875083035e9","job":0,"kind":"target_get","path":"/work/result.json"},"observed_ns":"0","seq":8}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_returned","returned":[{"name":"raw_score","value":6}]},"observed_ns":"0","seq":9}
            {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":10}
            {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":11}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":12}
            {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":13}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":14}
            {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":15}
            {"actions":[{"job":1,"kind":"start_final_call","selections":[{"role":"agent","selection":null}],"timeout_ns":"1000000000"}],"at_ns":"0","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"0","seq":16}
            {"actions":[],"at_ns":"0","event":{"job":1,"kind":"call_started"},"observed_ns":"0","seq":17}
            {"actions":[],"at_ns":"0","event":{"job":1,"kind":"call_returned","returned":[{"name":"score","value":0}]},"observed_ns":"0","seq":18}
            {"actions":[],"at_ns":"0","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-1"},"seq":19}
            {"actions":[{"kind":"finish"}],"at_ns":"0","event":{"job":1,"kind":"call_torn_down"},"observed_ns":"0","seq":20}
            its last record came before the agent's teardown was to end"#]],
    );
}

// As `returns_before_its_delete`, but the call's delete fails, which fails the run with its final
// call unstarted.
#[test]
fn undeleted_after_its_return() {
    let argv = ["ops", "call train 6", "await_result 0 completed", "exit"];
    let mut ir = short_workflow(&argv, 30);
    ir["roles"][1]["image"] = json!(sim::image(UNDELETABLE_LAST).as_str());
    let providers = [(PROVIDER, &Sim::new())];
    // The call's failed delete is recorded once the agent's teardown is: past the run's failure,
    // the log would take no teardown.
    let failed = Synced::Holding("jobs/0/.sandbox.json.partial", r#""state": "failed""#);
    let shown = executed(
        &SimHost::new().held_sync(failed, r#""kind":"agent_torn_down""#),
        "undeleted-after-its-return",
        &providers,
        &ir,
        SANDBOXD,
        &[],
    );
    expect![[r#"
        failed: deleting the sandbox failed: sim-2 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `simulated` failed: sim-2 can't be deleted by the simulated provider: create one with another `sandboxd`
        job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"failed"},"id":"sim-2","provider":"simulated"}
        payloads hold: ["xxxxxx"]
        log.jsonl:
        {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
        {"actions":[{"kind":"reply","reply":{"job":0,"kind":"call","remaining":{"calls":99,"seconds":null}}},{"caller":"agent-0","grant":"train","job":0,"kind":"start_call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":3}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":4}
        {"actions":[],"at_ns":"0","event":{"bytes":6,"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","job":0,"kind":"target_put","path":"/work/payload"},"observed_ns":"0","seq":5}
        {"actions":[],"at_ns":"0","event":{"argv":["judge","/work/payload","/work/result.json"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"0","seq":6}
        {"actions":[],"at_ns":"0","event":{"command":0,"end":{"exited":{"code":0}},"job":0,"kind":"command_ended"},"observed_ns":"0","seq":7}
        {"actions":[],"at_ns":"0","event":{"bytes":15,"digest":"sha256:68e3ea1c52db57d6e28c6d873ef07678bc738b836a1ec8ae5f831875083035e9","job":0,"kind":"target_get","path":"/work/result.json"},"observed_ns":"0","seq":8}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_returned","returned":[{"name":"raw_score","value":6}]},"observed_ns":"0","seq":9}
        {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":10}
        {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":11}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":12}
        {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":13}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":14}
        {"at_ns":"0","note":"the run failed: deleting the sandbox failed: sim-2 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left on provider `simulated` failed: sim-2 can't be deleted by the simulated provider: create one with another `sandboxd`","seq":15}
        its last record came before the agent's teardown was to end
        listed once it ended: ["sim-2"]"#]].assert_eq(&shown);
}

// The provider finishes the agent's delete only after the stopped call's, whose reports come as
// the agent's teardown goes on, in either order: the final call waits for both teardowns.
#[test]
fn deleted_while_torn_down() {
    let argv = ["ops", "call train slow", "await_log target_run", "exit"];
    let mut ir = short_workflow(&argv, 30);
    ir["roles"][0]["image"] = json!(sim::image(LAST).as_str());
    // The agent's delete is recorded once the call's is logged: its `LAST` sandbox orders only
    // the provider's deletes, not what each thread reports after its delete.
    let deleted = Synced::Holding(AGENT_SANDBOX, DELETED);
    let host = SimHost::new().held_sync(deleted, r#""provider":"simulated","sandbox":"job-0""#);
    let dir = TestDir::new();
    let (record, outcome) = execute_agents(&host, SHORT_STARTUP, ir, &dir, None);
    outcome.unwrap();
    let recorded: Value =
        serde_json::from_slice(&fs::read(record.join("outcome.json")).unwrap()).unwrap();
    let score =
        json!({ "kind": "returned", "result": { "values": [{ "name": "score", "value": 0 }] } });
    assert_eq!(recorded["verdict"], score);
    let exited = json!({ "kind": "agent_exited", "status": { "code": 0 } });
    assert_eq!(recorded["agents"][0]["work_end"], exited);
    let log = read_log(&record);
    let line = |kind: &str, of: (&str, Value)| {
        let event = |line: &Value| line["event"]["kind"] == kind && line["event"][of.0] == of.1;
        log.iter().position(event).unwrap()
    };
    let exited = line("agent_exited", ("caller", json!("agent-0")));
    let stop = json!({ "kind": "stop_call", "job": 0 });
    assert_eq!(log[exited]["actions"][0], stop);
    let ended = line("call_failed", ("job", json!(0)));
    assert_eq!(log[ended]["event"]["reason"], "the call was stopped");
    let deleted = line("sandbox_deleted", ("sandbox", json!("job-0")));
    let torn_down = line("call_torn_down", ("job", json!(0)));
    let agent_deleted = line("sandbox_deleted", ("sandbox", json!("agent-0")));
    let agent_torn_down = line("agent_torn_down", ("caller", json!("agent-0")));
    let starts = |line: &Value| line["actions"][0]["kind"] == "start_final_call";
    let final_call = log.iter().position(starts).unwrap();
    assert!(ended < deleted && deleted < torn_down && deleted < agent_deleted);
    assert_eq!(final_call, torn_down.max(agent_torn_down));
}

// The late op and the call's report each came before the hard end, at the time it carries, so
// the report ends the call in time and the run finishes rather than be abandoned.
#[test]
fn reported_before_the_hard_end() {
    // A deadline later than the agent's ops, so that the agent's exit ends the work phase, and
    // bounds the call's start.
    let deadline = 2;
    // The agent exits once its call's sandbox is recorded. So what the call's start hands the
    // driver comes first, and the hold below counts the next op alone.
    let argv = [
        "ops",
        // The launch is recorded before the first op, so only the agent's session sends what the
        // hold counts.
        "await_log agent_started",
        "call train 6",
        "await_file jobs/0/sandbox.json",
        "exit",
        "call train 6",
    ];
    let mut ir = short_workflow(&argv, deadline);
    ir["roles"][1]["image"] = json!(sim::image(HUNG).as_str());
    // Hold the driver as it records the exit. The command already sent its next request:
    // even after the hard end passes the driver must take what arrived in time.
    let slow = SlowSync {
        record: Synced::Holding("log.jsonl", r#""kind":"stop_call""#),
        queued: 1,
        at: PAST_THE_HARD_END,
    };
    let dir = TestDir::new();
    let (record, result) = execute_agents(
        &SimHost::new().slow_sync(slow),
        SHORT_STARTUP,
        ir,
        &dir,
        None,
    );
    assert!(result.unwrap_err().to_string().contains("hard end"));
    let log = read_log(&record);
    let op = log
        .iter()
        .position(|line| line["event"]["op"] == 1)
        .unwrap();
    let abandon = log
        .iter()
        .position(|line| line["actions"][0]["kind"] == "abandon")
        .unwrap();
    assert!(op < abandon);
    assert_eq!(log[op]["observed_ns"], "0");
    assert_eq!(log[op]["at_ns"], "6500000000");
    assert_eq!(log[op]["actions"][0]["reply"]["kind"], "refused");
    // The call's sandbox was created, and never met the gateway, as it never dials.
    let call = |kind: &str| {
        let event = |line: &Value| line["event"]["kind"] == kind;
        log.iter()
            .any(|line| event(line) && line["event"]["sandbox"] == "job-0")
    };
    assert!(call("sandbox_created") && !call("sandbox_met"));
}

// The final call's start is due by the hard end, which has passed, so its call fails with no
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
        &short_workflow(&["ops", "call submit 6 finalize=true"], 2),
        SANDBOXD,
        &expect![[r#"
            failed: the run was still going at its hard end, 6 s after it started: expected it to end within its deadline, its finalize grace, its final call's timeout and its sandboxes' startups
            payloads hold: ["xxxxxx"]
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"6000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"kind":"reply","reply":{"kind":"call","remaining_seconds":0,"returned":{"state":"accepted"}}},{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"evaluated":{"effects":["set_submission","end_work"],"returned":[{"name":"state","value":"accepted"}]},"grant":"submit","kind":"now","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":3}
            {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":4}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":5}
            {"actions":[],"at_ns":"6500000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":6}
            {"actions":[{"kind":"abandon"}],"at_ns":"6500000000","event":{"kind":"tick"},"observed_ns":"6500000000","seq":7}
            {"actions":[],"at_ns":"6500000000","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"6500000000","seq":8}
            {"at_ns":"6500000000","note":"the run failed: the run was still going at its hard end, 6 s after it started: expected it to end within its deadline, its finalize grace, its final call's timeout and its sandboxes' startups","seq":9}
            its last record came once the hard end had passed"#]],
    );
}

// Once it has started, the agent's sleep moves the clock past the hard end. Its model request then
// comes at the tick's time, so the loop takes it after the tick that abandons the run.
#[test]
fn model_request_after_abandon() {
    let argv = [
        "ops",
        "await_log agent_started",
        "sleep 7",
        "model m-large 7",
    ];
    check(
        &SimHost::new(),
        "model-request-after-abandon",
        &[(PROVIDER, &Sim::with_startup(SHORT_STARTUP))],
        &routed(short_workflow(&argv, 2)),
        SANDBOXD,
        &expect![[r#"
            failed: the run was still going at its hard end, 6 s after it started: expected it to end within its deadline, its finalize grace, its final call's timeout and its sandboxes' startups
            provider.json routes: {"agent":[{"api":"responses","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"}]}
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"6000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"kind":"abandon"}],"at_ns":"7000000000","event":{"kind":"tick"},"observed_ns":"7000000000","seq":3}
            {"at_ns":"7000000000","note":"model request 0 wasn't answered: the run has ended","seq":4}
            {"actions":[],"at_ns":"7000000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":5}
            {"at_ns":"7000000000","note":"the run failed: the run was still going at its hard end, 6 s after it started: expected it to end within its deadline, its finalize grace, its final call's timeout and its sandboxes' startups","seq":6}
            its last record came once the hard end had passed
            the model endpoint at each create: [true]"#]],
    );
}

#[test]
fn taken_past_the_deadline() {
    let late = LateTake {
        behind: 1,
        at: Duration::from_millis(30_500),
    };
    let ir = short_workflow(
        &[
            "ops",
            "await_log agent_started",
            "ops",
            "call train 6",
            "close",
        ],
        30,
    );
    let dir = TestDir::new();
    let (record, outcome) = execute_agents(&SimHost::new().late_take(late), 30, ir, &dir, None);
    let outcome = outcome.unwrap();
    assert!(matches!(
        outcome.agents[0].work_end,
        decision::WorkEnd::AgentFailed(_)
    ));
    assert!(outcome.result.is_ok());

    let log = read_log(&record);
    let op = log
        .iter()
        .position(|line| line["event"]["op"] == 1)
        .unwrap();
    let ended = log
        .iter()
        .position(|line| line["event"]["kind"] == "agent_failed")
        .unwrap();
    assert!(op < ended);
    assert_eq!(log[op]["observed_ns"], "0");
    assert_eq!(log[op]["at_ns"], "30500000000");
    assert_eq!(log[op]["actions"][1]["kind"], "start_call");
    assert_eq!(log[ended]["observed_ns"], "0");
    assert!(log.iter().any(|line| line["note"]
        == "op 1's reply wasn't sent: it was due by the core's next act, which has come"));
    assert!(
        log.iter()
            .any(|line| line["event"]["kind"] == "call_failed" && line["event"]["job"] == 0)
    );
    assert!(
        log.iter()
            .any(|line| line["event"]["kind"] == "agent_torn_down")
    );
}

// The session's end, which reached the host at 0, is decided at 0, ahead of the core's tick.
#[test]
fn held_by_a_slow_sync() {
    let argv = [
        "ops",
        // The launch is recorded before the first op, so only the agent's session sends what the
        // hold counts.
        "await_log agent_started",
        "ops",
        "close",
    ];
    let slow = SlowSync {
        record: Synced::Holding("log.jsonl", r#""kind":"agent_started""#),
        queued: 1,
        at: Duration::from_millis(30_500),
    };
    check(
        &SimHost::new().slow_sync(slow),
        "held-by-a-slow-sync",
        &[(PROVIDER, &Sim::new())],
        &short_workflow(&argv, 30),
        SANDBOXD,
        &expect![[r#"
            finished, showing [score=0]
            outcome.json: {"agents":[{"role":"agent","submission":null,"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":0}]}}}
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"kind":"reply","reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"job","name":"train","payload_max_bytes":8},{"arguments":{"finalize":{"default":false,"type":"bool"}},"kind":"now","name":"submit","payload_max_bytes":16}],"kind":"ops","remaining_seconds":30,"resources":{"cpu_millis":8000,"memory_bytes":34359738368},"role":"agent"}}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"kind":"ops"}},"observed_ns":"0","seq":3}
            {"at_ns":"30500000000","note":"op 0's reply wasn't sent: it was due by the core's next act, which has come","seq":4}
            {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"30500000000","event":{"caller":"agent-0","kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"},"observed_ns":"0","seq":5}
            {"at_ns":"30500000000","note":"freezing the agent's sandbox failed: the sandbox ended the session","seq":6}
            {"at_ns":"30500000000","note":"the agent's out is incomplete: collecting /pumpkin/out failed: the sandbox ended the session","seq":7}
            {"at_ns":"30500000000","note":"the agent's log is incomplete: collecting /pumpkin/log failed: the sandbox ended the session","seq":8}
            {"at_ns":"30500000000","note":"collected from agent `agent`'s sandbox: out, nothing; log, nothing","seq":9}
            {"actions":[],"at_ns":"30500000000","event":{"caller":"agent-0","kind":"agent_collected"},"seq":10}
            {"actions":[],"at_ns":"30500000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":11}
            {"actions":[{"job":0,"kind":"start_final_call","selections":[{"role":"agent","selection":null}],"timeout_ns":"1000000000"}],"at_ns":"30500000000","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"30500000000","seq":12}
            {"actions":[],"at_ns":"30500000000","event":{"job":0,"kind":"call_started"},"observed_ns":"30500000000","seq":13}
            {"actions":[],"at_ns":"30500000000","event":{"job":0,"kind":"call_returned","returned":[{"name":"score","value":0}]},"observed_ns":"30500000000","seq":14}
            {"actions":[],"at_ns":"30500000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":15}
            {"actions":[{"kind":"finish"}],"at_ns":"30500000000","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"30500000000","seq":16}
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
    };
    check(
        &SimHost::new().slow_sync(slow).late_take(late),
        "started-late",
        &[(PROVIDER, &Sim::new())],
        &short_workflow(&["ops", "await_log agent_started", "ops", "ops"], 30),
        SANDBOXD,
        &expect![[r#"
            finished, showing [score=0]
            outcome.json: {"agents":[{"role":"agent","submission":null,"work_end":{"kind":"deadline"}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":0}]}}}
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"25000000000","event":{"kind":"work_started"},"observed_ns":"25000000000","seq":1}
            {"actions":[],"at_ns":"25000000000","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"25000000000","seq":2}
            {"actions":[{"kind":"reply","reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"job","name":"train","payload_max_bytes":8},{"arguments":{"finalize":{"default":false,"type":"bool"}},"kind":"now","name":"submit","payload_max_bytes":16}],"kind":"ops","remaining_seconds":30,"resources":{"cpu_millis":8000,"memory_bytes":34359738368},"role":"agent"}}],"at_ns":"25000000000","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"kind":"ops"}},"observed_ns":"25000000000","seq":3}
            {"actions":[{"kind":"reply","reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"job","name":"train","payload_max_bytes":8},{"arguments":{"finalize":{"default":false,"type":"bool"}},"kind":"now","name":"submit","payload_max_bytes":16}],"kind":"ops","remaining_seconds":30,"resources":{"cpu_millis":8000,"memory_bytes":34359738368},"role":"agent"}}],"at_ns":"55000000000","event":{"caller":"agent-0","kind":"op_arrived","op":1,"request":{"kind":"ops"}},"observed_ns":"25000000000","seq":4}
            {"at_ns":"55000000000","note":"op 1's reply wasn't sent: it was due by the core's next act, which has come","seq":5}
            {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"55000000000","event":{"kind":"tick"},"observed_ns":"55000000000","seq":6}
            {"at_ns":"55000000000","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":7}
            {"actions":[],"at_ns":"55000000000","event":{"caller":"agent-0","kind":"agent_collected"},"seq":8}
            {"actions":[],"at_ns":"55000000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":9}
            {"actions":[{"job":0,"kind":"start_final_call","selections":[{"role":"agent","selection":null}],"timeout_ns":"1000000000"}],"at_ns":"55000000000","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"55000000000","seq":10}
            {"actions":[],"at_ns":"55000000000","event":{"job":0,"kind":"call_started"},"observed_ns":"55000000000","seq":11}
            {"actions":[],"at_ns":"55000000000","event":{"job":0,"kind":"call_returned","returned":[{"name":"score","value":0}]},"observed_ns":"55000000000","seq":12}
            {"actions":[],"at_ns":"55000000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":13}
            {"actions":[{"kind":"finish"}],"at_ns":"55000000000","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"55000000000","seq":14}
            its last record came before the agent's teardown was to end"#]],
    );
}

#[test]
fn setup_started_past_its_deadline() {
    let slow = SlowSync {
        record: Synced::Seq(0),
        queued: 0,
        at: Duration::from_secs(35),
    };
    let ir = short_workflow(&["ops", "ops"], 30);
    let dir = TestDir::new();
    let (record, outcome) = execute_agents(&SimHost::new().slow_sync(slow), 30, ir, &dir, None);
    let outcome = outcome.unwrap();
    assert!(matches!(
        outcome.agents[0].work_end,
        decision::WorkEnd::AgentFailed(_)
    ));
    assert!(outcome.result.is_ok());
    let log = read_log(&record);
    assert!(
        log.iter()
            .all(|line| line["event"]["kind"] != "agent_started")
    );
    assert!(log.iter().all(|line| line["event"]["kind"] != "op_arrived"));
    assert!(
        log.iter()
            .any(|line| line["event"]["kind"] == "agent_torn_down")
    );
}

#[test]
fn sync_fails() {
    check(
        &SimHost::new().failed_sync(Synced::Holding("log.jsonl", r#""kind":"agent_started""#)),
        "sync-fails",
        &[(PROVIDER, &Sim::new())],
        &ops_workflow(&["ops", "ops", "exit"], 30),
        SANDBOXD,
        &expect![[r#"
            failed: <record>/log.jsonl: the simulated host's sync failed
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"kind":"reply","reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"job","name":"train","payload_max_bytes":8},{"arguments":{"finalize":{"default":false,"type":"bool"}},"kind":"now","name":"submit","payload_max_bytes":16}],"kind":"ops","remaining_seconds":30,"resources":{"cpu_millis":8000,"memory_bytes":34359738368},"role":"agent"}}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"kind":"ops"}},"observed_ns":"0","seq":3}
            {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":4}
            {"at_ns":"0","note":"the run failed: <record>/log.jsonl: the simulated host's sync failed","seq":5}
            its last record came before the agent's teardown was to end"#]],
    );
}

#[test]
fn refusal_held_by_a_slow_sync() {
    let slow = SlowSync {
        record: Synced::Holding("log.jsonl", r#""request":{"kind":"ops"}"#),
        queued: 0,
        at: Duration::from_millis(30_500),
    };
    check(
        &SimHost::new().slow_sync(slow),
        "refusal-held-by-a-slow-sync",
        &[(PROVIDER, &Sim::new())],
        &short_workflow(&["ops", "ops", "call train 17", "exit"], 30),
        SANDBOXD,
        &expect![[r#"
            finished, showing [score=0]
            outcome.json: {"agents":[{"role":"agent","submission":null,"work_end":{"kind":"deadline"}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":0}]}}}
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"kind":"reply","reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"job","name":"train","payload_max_bytes":8},{"arguments":{"finalize":{"default":false,"type":"bool"}},"kind":"now","name":"submit","payload_max_bytes":16}],"kind":"ops","remaining_seconds":30,"resources":{"cpu_millis":8000,"memory_bytes":34359738368},"role":"agent"}}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"kind":"ops"}},"observed_ns":"0","seq":3}
            {"at_ns":"30500000000","note":"op 0's reply wasn't sent: it was due by the core's next act, which has come","seq":4}
            {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"30500000000","event":{"kind":"tick"},"observed_ns":"30500000000","seq":5}
            {"at_ns":"30500000000","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":6}
            {"actions":[],"at_ns":"30500000000","event":{"caller":"agent-0","kind":"agent_collected"},"seq":7}
            {"actions":[],"at_ns":"30500000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":8}
            {"actions":[{"job":0,"kind":"start_final_call","selections":[{"role":"agent","selection":null}],"timeout_ns":"1000000000"}],"at_ns":"30500000000","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"30500000000","seq":9}
            {"actions":[],"at_ns":"30500000000","event":{"job":0,"kind":"call_started"},"observed_ns":"30500000000","seq":10}
            {"actions":[],"at_ns":"30500000000","event":{"job":0,"kind":"call_returned","returned":[{"name":"score","value":0}]},"observed_ns":"30500000000","seq":11}
            {"actions":[],"at_ns":"30500000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":12}
            {"actions":[{"kind":"finish"}],"at_ns":"30500000000","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"30500000000","seq":13}
            its last record came before the agent's teardown was to end"#]],
    );
}

// Each create takes 3 s, and its sandbox meets the gateway a second after it returns. The
// agent's create begins at 0, and the final call's target's at the agent's exit, at 4 s.
#[test]
fn start_times() {
    let target = || Synced::Holding("jobs/0/.sandbox.json.partial", "sim-2");
    let shown = listening_too(|sim| {
        // The loop logs the agent's create only once the agent is set up. The target's record
        // waits for its create's line, so that the loop logs it before the clock moves.
        let host = SimHost::new()
            .moving_after_read(
                Synced::Holding(AGENT_SANDBOX, "sim-1"),
                Duration::from_secs(4),
            )
            .moving_after_read(target(), Duration::from_secs(8))
            .held_sync(target(), r#""sandbox":"job-0""#);
        executed(
            &host,
            "start-times",
            &[(PROVIDER, &sim.taking(Duration::from_secs(1)))],
            &ops_workflow(&["ops", "exit"], 30),
            SANDBOXD,
            &[Also::Starts],
        )
    });
    let starts = (shown.lines())
        .filter(|line| line.contains("sandbox_created") || line.contains("sandbox_met"));
    expect![[r#"
        {"actions":[],"at_ns":"4000000000","event":{"began_ns":"0","kind":"sandbox_created","phases":[{"began_ns":"1000000000","name":"make","took_ns":"1000000000"}],"sandbox":"agent-0"},"observed_ns":"3000000000","seq":2}
        {"actions":[],"at_ns":"4000000000","event":{"kind":"sandbox_met","sandbox":"agent-0"},"observed_ns":"4000000000","seq":3}
        {"actions":[],"at_ns":"7000000000","event":{"began_ns":"4000000000","kind":"sandbox_created","phases":[{"began_ns":"5000000000","name":"make","took_ns":"1000000000"}],"sandbox":"job-0"},"observed_ns":"7000000000","seq":10}
        {"actions":[],"at_ns":"8000000000","event":{"kind":"sandbox_met","sandbox":"job-0"},"observed_ns":"8000000000","seq":11}"#]].assert_eq(&starts.collect::<Vec<_>>().join("\n"));
}

// The record keeps what the driver read of itself, with what its job's unit used. A figure that
// it couldn't read is `null`.
#[test]
fn records_what_the_driver_used() {
    let shown = executed(
        &SimHost::new(),
        "driver-used",
        &[(PROVIDER, &Sim::new())],
        &workflow(&["exit", "0"]),
        SANDBOXD,
        &[Also::Driver],
    );
    expect![[r#"
        finished, showing [score=0]
        outcome.json: {"agents":[{"role":"agent","submission":null,"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":0}]}}}
        outcome.json's driver: {"children_cpu_ns":"3100000000","cpu_ns":"41200000000","job":{"cpu_ns":"46000000000","peak_memory_bytes":null,"peak_tasks_count":1204,"unit":"pumpkin-job-20261125-120000-00000001.service"},"peak_memory_bytes":222298112}
        read back: Some(DriverUse { cpu: Some(41.2s), children_cpu: Some(3.1s), peak_memory_bytes: Some(222298112), job: Some(JobUse { unit: "pumpkin-job-20261125-120000-00000001.service", cpu: Some(46s), peak_memory_bytes: None, peak_tasks_count: Some(1204) }) })
        log.jsonl:
        {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"18270000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
        {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":3}
        {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":4}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":5}
        {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":6}
        {"actions":[{"job":0,"kind":"start_final_call","selections":[{"role":"agent","selection":null}],"timeout_ns":"120000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":7}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":8}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_returned","returned":[{"name":"score","value":0}]},"observed_ns":"0","seq":9}
        {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":10}
        {"actions":[{"kind":"finish"}],"at_ns":"0","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"0","seq":11}
        its last record came before the agent's teardown was to end"#]].assert_eq(&shown);
}

// The agent's start leads to no act, so it is synced with the next record that does; the note
// of what its teardown collected, which no such record follows, by the run's end.
#[test]
fn syncs() {
    let journal = Journal::default();
    let shown = executed(
        &SimHost::new().journaled(&journal),
        "syncs",
        &[(PROVIDER, &Sim::new().journaled(&journal))],
        &ops_workflow(&["ops", "ops", "call train 17", "exit"], 30),
        SANDBOXD,
        // The journal numbers each line of the log, a start's too.
        &[Also::Starts],
    );
    expect![[r#"
        finished, showing [score=0]
        outcome.json: {"agents":[{"role":"agent","submission":null,"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":0}]}}}
        log.jsonl:
        {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
        {"actions":[],"at_ns":"0","event":{"began_ns":"0","kind":"sandbox_created","phases":[{"began_ns":"0","name":"make","took_ns":"0"}],"sandbox":"agent-0"},"observed_ns":"0","seq":2}
        {"actions":[],"at_ns":"0","event":{"kind":"sandbox_met","sandbox":"agent-0"},"observed_ns":"0","seq":3}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":4}
        {"actions":[{"kind":"reply","reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"job","name":"train","payload_max_bytes":8},{"arguments":{"finalize":{"default":false,"type":"bool"}},"kind":"now","name":"submit","payload_max_bytes":16}],"kind":"ops","remaining_seconds":30,"resources":{"cpu_millis":8000,"memory_bytes":34359738368},"role":"agent"}}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"kind":"ops"}},"observed_ns":"0","seq":5}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"op_refused","message":"the payload is 17 bytes, more than the limit of 16: send at most 16","request":{"grant":"train","kind":"call","payload":{"over":16,"size":17}}},"seq":6}
        {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":7}
        {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":8}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":9}
        {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":10}
        {"actions":[{"job":0,"kind":"start_final_call","selections":[{"role":"agent","selection":null}],"timeout_ns":"120000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":11}
        {"actions":[],"at_ns":"0","event":{"began_ns":"0","kind":"sandbox_created","phases":[{"began_ns":"0","name":"make","took_ns":"0"}],"sandbox":"job-0"},"observed_ns":"0","seq":12}
        {"actions":[],"at_ns":"0","event":{"kind":"sandbox_met","sandbox":"job-0"},"observed_ns":"0","seq":13}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":14}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_returned","returned":[{"name":"score","value":0}]},"observed_ns":"0","seq":15}
        {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":16}
        {"actions":[{"kind":"finish"}],"at_ns":"0","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"0","seq":17}
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
          fsync <record>/agents/agent/.sandbox.json.partial
          fsync <record>/agents/agent
          fdatasync <record>/log.jsonl, through record 5
          a session read op 0's reply: ops
          fdatasync <record>/log.jsonl, through record 6
          a session read op 1's reply: refused
          fdatasync <record>/log.jsonl, through record 7
          deleted sim-1
          fsync <record>/agents/agent/.sandbox.json.partial
          fsync <record>/agents/agent
          fdatasync <record>/log.jsonl, through record 11
          created sim-2
          fsync <record>/jobs/0/.sandbox.json.partial
          fsync <record>/jobs/0
          deleted sim-2
          fsync <record>/jobs/0/.sandbox.json.partial
          fsync <record>/jobs/0
          fdatasync <record>/log.jsonl, through record 17
          fdatasync <record>/log.jsonl, through record 17
          fsync <record>/.outcome.json.partial
          fsync <record>"#]]
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
        "call submit 6 finalize=true",
    ];
    let mut ir = routed(ops_workflow(&argv, 30));
    let chat = json!({ "api": "chat", "models": ["m-large"] });
    ir["roles"][0]["runtime"]["agent"]["egress"]["model"]
        .as_array_mut()
        .unwrap()
        .push(chat);
    let shown = executed(
        &SimHost::new(),
        "model-requests-kept-whole",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &[Also::Exchanges],
    );
    expect![[r#"
        finished, showing [score=6]
        payloads hold: ["xxxxxx"]
        provider.json routes: {"agent":[{"api":"responses","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"},{"api":"chat","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"}]}
        outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":6}]}}}
        log.jsonl:
        {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"model_request_started","request":0},"observed_ns":"0","seq":3}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent-0","first_byte_ns":"0","kept":{"request":{"body_bytes":278,"body_sha256":"4a0e2a217ab42d2918ccf10d857bb512cd2c0071f1903cfb2fc3d8069e8d6490","bytes":369,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"whole":["tools","instructions"]},"response":{"bytes":356}},"kind":"model_answered","model":"m-large","request":0,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"0","seq":4}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"model_request_started","request":1},"observed_ns":"0","seq":5}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent-0","first_byte_ns":"0","kept":{"request":{"body_bytes":360,"body_sha256":"3f8893c949082a78b81b69e0b8c16a9cbb69a2fe2a447efc11ce3463eede3a32","bytes":443,"dropped_headers":["user-agent"],"items":{"count":3,"field":"input"},"whole":["tools","instructions"]},"response":{"bytes":438}},"kind":"model_answered","model":"m-large","request":1,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"0","seq":6}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"model_request_started","request":2},"observed_ns":"0","seq":7}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent-0","first_byte_ns":"0","kept":{"request":{"body_bytes":252,"body_sha256":"2e435321c933c5ff53fd926b770e3a78d618808c9e1822e1ab90a4b7c776a44d","bytes":280,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"whole":["tools"]},"response":{"bytes":330}},"kind":"model_answered","model":"m-large","request":2,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"0","seq":8}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"model_request_started","request":3},"observed_ns":"0","seq":9}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent-0","first_byte_ns":"0","kept":{"request":{"body_bytes":174,"body_sha256":"8d6175ce51c5e9942c5037737b41cabe7120f8a38f746b122528340427199599","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":252}},"kind":"model_answered","model":"m-large","request":3,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"0","seq":10}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"model_request_started","request":4},"observed_ns":"0","seq":11}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent-0","first_byte_ns":"0","kept":{"request":{"body_bytes":205,"body_sha256":"23a7b1aff92dc20f820007c4302e3e23ca538ba4ee347b37287e306dbdba6110","bytes":369,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"},"whole":["tools","instructions"]},"response":{"bytes":283}},"kind":"model_answered","model":"m-large","request":4,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"0","seq":12}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"model_request_started","request":5},"observed_ns":"0","seq":13}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"chat","caller":"agent-0","first_byte_ns":"0","kept":{"request":{"body_bytes":239,"body_sha256":"ceca32bd90c85a16a90d02d5fdf3cb840e5c10db5c3a050e1e0c6aa4c269b15b","bytes":288,"items":{"count":2,"field":"messages"},"whole":["tools"]},"response":{"bytes":324}},"kind":"model_answered","model":"m-large","request":5,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"0","seq":14}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"model_request_started","request":6},"observed_ns":"0","seq":15}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent-0","first_byte_ns":"0","kept":{"request":{"body_bytes":101,"body_sha256":"c2c8b73765a171cc435ac7ea3093cc46c6e5d9fea831d802cebb7a176232fd41","bytes":139,"dropped_headers":["user-agent"],"whole":["tools"]},"response":{"bytes":179}},"kind":"model_answered","model":"m-large","request":6,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"0","seq":16}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"model_request_started","request":7},"observed_ns":"0","seq":17}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"chat","caller":"agent-0","first_byte_ns":"0","kept":{"request":{"body_bytes":260,"body_sha256":"557137cfb765ee826b9fbabe67c1c2bd112ef844bab0a14e281b21175d674c56","bytes":343,"items":{"count":3,"field":"messages"},"whole":["tools"]},"response":{"bytes":345}},"kind":"model_answered","model":"m-large","request":7,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"0","seq":18}
        {"actions":[{"kind":"reply","reply":{"kind":"call","remaining_seconds":30,"returned":{"state":"accepted"}}}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"evaluated":{"effects":["set_submission","end_work"],"returned":[{"name":"state","value":"accepted"}]},"grant":"submit","kind":"now","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":19}
        {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":20}
        {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 2 files of 3027 bytes","seq":21}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":22}
        {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":23}
        {"actions":[{"job":0,"kind":"start_final_call","selections":[{"role":"agent","selection":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}],"timeout_ns":"120000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":24}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":25}
        {"actions":[],"at_ns":"0","event":{"bytes":6,"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","job":0,"kind":"target_put","path":"/pumpkin/in/payload/raw"},"observed_ns":"0","seq":26}
        {"actions":[],"at_ns":"0","event":{"argv":["judge","/pumpkin/in/payload/raw"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"0","seq":27}
        {"actions":[],"at_ns":"0","event":{"command":0,"end":{"exited":{"code":0}},"job":0,"kind":"command_ended"},"observed_ns":"0","seq":28}
        {"actions":[],"at_ns":"0","event":{"bytes":15,"digest":"sha256:68e3ea1c52db57d6e28c6d873ef07678bc738b836a1ec8ae5f831875083035e9","job":0,"kind":"target_get","path":"/pumpkin/out/result.json"},"observed_ns":"0","seq":29}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_returned","returned":[{"name":"score","value":6}]},"observed_ns":"0","seq":30}
        {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":31}
        {"actions":[{"kind":"finish"}],"at_ns":"0","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"0","seq":32}
        its last record came before the agent's teardown was to end
        agent `agent`'s standard output:
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
        &SimHost::new(),
        "model-request-rebuilt-from-changed-item",
        &[(PROVIDER, &Sim::new())],
        &routed(ops_workflow(
            &["ops", "model m-large stream", "call submit 1 finalize=true"],
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
        request 0 read from a damaged log: `<record>/model/0.request` is a symbolic link: expected a file of the record's own
        request 0 read from a damaged log: `<record>/model/0.request` is a symbolic link: expected a file of the record's own
        request 0 read from a damaged log: `<record>/model/0.response` is a symbolic link: expected a file of the record's own
        request 0 read from a damaged log: the run log's `kept.request` is "model/0.request": expected an object"#]].assert_eq(&rebuilt.join("\n"));
}

// The upstream fails, answers, then fails until the agent exits: its failure counts from the
// first request past the answer, and a request the route bars, or cuts at its end, leaves it.
#[test]
fn model_provider_down() {
    let argv = [
        "ops",
        "model m-large down",
        "model m-large stream",
        "model m-large down",
        "model m-large down",
        "model m-huge stream",
        "ask m-large stall",
        "exit",
    ];
    let shown = executed(
        &SimHost::new(),
        "model-provider-down",
        &[(PROVIDER, &Sim::new())],
        &routed(ops_workflow(&argv, 30)),
        SANDBOXD,
        &[Also::Exchanges],
    );
    expect![[r#"
        finished, showing [score=0]
        provider.json routes: {"agent":[{"api":"responses","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"}]}
        outcome.json: {"agents":[{"role":"agent","route_failure":"the upstream `stand-in` failed the agent's last model request, 3 (502; failing since request 2), and the agent exited: a deployment fault, not the agent's result; run it again","submission":null,"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":0}]}}}
        log.jsonl:
        {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"model_request_started","request":0},"observed_ns":"0","seq":3}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent-0","first_byte_ns":"0","kept":{"request":{"body_bytes":172,"body_sha256":"dc978248f6a10cf2405c2f1144b54026073188a2ef8072f5b52280326be5f330","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":7}},"kind":"model_answered","model":"m-large","request":0,"request_id":"req-502","stream":false,"upstream":"stand-in","upstream_status":502},"observed_ns":"0","seq":4}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"model_request_started","request":1},"observed_ns":"0","seq":5}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent-0","first_byte_ns":"0","kept":{"request":{"body_bytes":174,"body_sha256":"8d6175ce51c5e9942c5037737b41cabe7120f8a38f746b122528340427199599","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":252}},"kind":"model_answered","model":"m-large","request":1,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"0","seq":6}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"model_request_started","request":2},"observed_ns":"0","seq":7}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent-0","first_byte_ns":"0","kept":{"request":{"body_bytes":172,"body_sha256":"dc978248f6a10cf2405c2f1144b54026073188a2ef8072f5b52280326be5f330","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":7}},"kind":"model_answered","model":"m-large","request":2,"request_id":"req-502","stream":false,"upstream":"stand-in","upstream_status":502},"observed_ns":"0","seq":8}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"model_request_started","request":3},"observed_ns":"0","seq":9}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent-0","first_byte_ns":"0","kept":{"request":{"body_bytes":172,"body_sha256":"dc978248f6a10cf2405c2f1144b54026073188a2ef8072f5b52280326be5f330","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":7}},"kind":"model_answered","model":"m-large","request":3,"request_id":"req-502","stream":false,"upstream":"stand-in","upstream_status":502},"observed_ns":"0","seq":10}
        {"actions":[],"at_ns":"0","event":{"answer":{"bar":"rules","kind":"barred","message":"the request names `m-huge`, which the route doesn't allow: name `m-large`","status":400},"api":"responses","caller":"agent-0","kept":{"request":{"body_bytes":166,"body_sha256":"dcfd721111dd302f62bb2078dcfe197c15dda58b0c7a7d865cbb8106e6da33d4","bytes":166},"response":{"bytes":153}},"kind":"model_answered","request":4,"upstream":"stand-in","upstream_status":null},"observed_ns":"0","seq":11}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"model_request_started","request":5},"observed_ns":"0","seq":12}
        {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":13}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"cut"},"api":"responses","caller":"agent-0","first_byte_ns":"0","kept":{"request":{"body_bytes":173,"body_sha256":"47bf6d5b1bdebfa49b0689b8f69a5f19e3d3ae113fde67c00dd2d2edb81ec513","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":25}},"kind":"model_answered","model":"m-large","request":5,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"0","seq":14}
        {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 2 files of 900 bytes","seq":15}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":16}
        {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":17}
        {"actions":[{"job":0,"kind":"start_final_call","selections":[{"role":"agent","selection":null}],"timeout_ns":"120000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":18}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":19}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_returned","returned":[{"name":"score","value":0}]},"observed_ns":"0","seq":20}
        {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":21}
        {"actions":[{"kind":"finish"}],"at_ns":"0","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"0","seq":22}
        its last record came before the agent's teardown was to end
        agent `agent`'s standard output:
        model request 0: 502 [("content-type", "text/plain"), ("x-request-id", "req-502")] refused
        model request 1: 200 [("content-type", "text/event-stream")] data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
        model request 2: 502 [("content-type", "text/plain"), ("x-request-id", "req-502")] refused
        model request 3: 502 [("content-type", "text/plain"), ("x-request-id", "req-502")] refused
        model request 4: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the request names `m-huge`, which the route doesn't allow: name `m-large`","param":null,"type":"invalid_request_error"}}
        model request 5: 200 [("content-type", "text/event-stream")] event: response.created\n\n
        model exchange 0 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:ebd80fcd992f7a45a6c58098f79ce103ace84aef8b2530a94481e55202c29f67"],"model":"vendor/m-large","store":false}; response refused
        model exchange 1 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:af497cd3652d5371734347499d7dd2db68a62c28fe107f368d77cff1b78d474a"],"model":"vendor/m-large","store":false}; response data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
        model exchange 2 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:ebd80fcd992f7a45a6c58098f79ce103ace84aef8b2530a94481e55202c29f67"],"model":"vendor/m-large","store":false}; response refused
        model exchange 3 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:ebd80fcd992f7a45a6c58098f79ce103ace84aef8b2530a94481e55202c29f67"],"model":"vendor/m-large","store":false}; response refused
        model exchange 4 kept: request {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"m-huge","store":false}; response {"error":{"code":null,"message":"the request names `m-huge`, which the route doesn't allow: name `m-large`","param":null,"type":"invalid_request_error"}}
        model exchange 5 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:8b740cd5d26befcc0dbb835cfb3849a3cdd15f134987d273e263503033017963"],"model":"vendor/m-large","store":false}; response event: response.created\n\n
        model items kept: 4
        the model endpoint at each create: [true, false]
        a stalled stream's client hung up before the agent's teardown was to end"#]]
    .assert_eq(&shown);
}

#[test]
fn model_provider_down_at_the_deadline() {
    let argv = [
        "ops",
        "call submit 6",
        "model m-large down",
        // Its stream's first event has come, so the request is under way, once the sleep moves
        // the clock to the 2 s deadline.
        "ask m-large stall",
        "await_log model_request_started request=1",
        "sleep 2",
    ];
    // The stalled request ends as the deadline ends the agent's route, beside the agent's
    // teardown, which nothing orders it against.
    let shown = executed(
        &SimHost::new(),
        "model-provider-down-at-the-deadline",
        &[(PROVIDER, &Sim::new())],
        &routed(short_workflow(&argv, 2)),
        SANDBOXD,
        &[Also::Exchanges, Also::Racing(r#""answer":{"kind":"cut"}"#)],
    );
    expect![[r#"
        finished, showing [score=6]
        payloads hold: ["xxxxxx"]
        provider.json routes: {"agent":[{"api":"responses","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"}]}
        outcome.json: {"agents":[{"role":"agent","submission":{"finalized":false,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}},"work_end":{"kind":"deadline"}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":6}]}}}
        log.jsonl:
        {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"93000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
        {"actions":[{"kind":"reply","reply":{"kind":"call","remaining_seconds":2,"returned":{"state":"stored"}}}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"evaluated":{"effects":["set_submission"],"returned":[{"name":"state","value":"stored"}]},"grant":"submit","kind":"now","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":3}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"model_request_started","request":0},"observed_ns":"0","seq":4}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent-0","first_byte_ns":"0","kept":{"request":{"body_bytes":172,"body_sha256":"dc978248f6a10cf2405c2f1144b54026073188a2ef8072f5b52280326be5f330","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":7}},"kind":"model_answered","model":"m-large","request":0,"request_id":"req-502","stream":false,"upstream":"stand-in","upstream_status":502},"observed_ns":"0","seq":5}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"model_request_started","request":1},"observed_ns":"0","seq":6}
        {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"2000000000","event":{"kind":"tick"},"observed_ns":"2000000000","seq":7}
        {"at_ns":"2000000000","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 2 files of 188 bytes","seq":8}
        {"actions":[],"at_ns":"2000000000","event":{"caller":"agent-0","kind":"agent_collected"},"seq":9}
        {"actions":[],"at_ns":"2000000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":10}
        {"actions":[{"job":0,"kind":"start_final_call","selections":[{"role":"agent","selection":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}],"timeout_ns":"1000000000"}],"at_ns":"2000000000","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"2000000000","seq":11}
        {"actions":[],"at_ns":"2000000000","event":{"job":0,"kind":"call_started"},"observed_ns":"2000000000","seq":12}
        {"actions":[],"at_ns":"2000000000","event":{"bytes":6,"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","job":0,"kind":"target_put","path":"/pumpkin/in/payload/raw"},"observed_ns":"2000000000","seq":13}
        {"actions":[],"at_ns":"2000000000","event":{"argv":["judge","/pumpkin/in/payload/raw"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"2000000000","seq":14}
        {"actions":[],"at_ns":"2000000000","event":{"command":0,"end":{"exited":{"code":0}},"job":0,"kind":"command_ended"},"observed_ns":"2000000000","seq":15}
        {"actions":[],"at_ns":"2000000000","event":{"bytes":15,"digest":"sha256:68e3ea1c52db57d6e28c6d873ef07678bc738b836a1ec8ae5f831875083035e9","job":0,"kind":"target_get","path":"/pumpkin/out/result.json"},"observed_ns":"2000000000","seq":16}
        {"actions":[],"at_ns":"2000000000","event":{"job":0,"kind":"call_returned","returned":[{"name":"score","value":6}]},"observed_ns":"2000000000","seq":17}
        {"actions":[],"at_ns":"2000000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":18}
        {"actions":[{"kind":"finish"}],"at_ns":"2000000000","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"2000000000","seq":19}
        log.jsonl, at no fixed place:
        {"actions":[],"at_ns":"2000000000","event":{"answer":{"kind":"cut"},"api":"responses","caller":"agent-0","first_byte_ns":"0","kept":{"request":{"body_bytes":173,"body_sha256":"47bf6d5b1bdebfa49b0689b8f69a5f19e3d3ae113fde67c00dd2d2edb81ec513","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":25}},"kind":"model_answered","model":"m-large","request":1,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"2000000000"}
        its last record came before the agent's teardown was to end
        agent `agent`'s standard output:
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
    // The sleep moves the clock to the 1 s deadline once the stream's first event has come and
    // the request's start is logged.
    let argv = [
        "ops",
        "call submit slow",
        "ask m-large stall",
        "await_log model_request_started",
        "sleep 1",
    ];
    // The clock reaches the slow judge's 1 s timeout once its command has started, and the cut is
    // logged, so that the cut's times are the deadline's.
    let host = SimHost::new();
    let judging = &[
        r#""job":0,"kind":"target_run""#,
        r#""answer":{"kind":"cut"}"#,
    ];
    let sim = Sim::new().once_logged(judging, host.moving_to(Duration::from_secs(2)));
    // The stalled request ends as the deadline ends the agent's route, beside the agent's
    // teardown, which nothing orders it against.
    let also = [Also::Exchanges, Also::Racing(r#""answer":{"kind":"cut"}"#)];
    let shown = executed(
        &host,
        "model-stream-cut",
        &[(PROVIDER, &sim)],
        &routed(short_workflow(&argv, 1)),
        SANDBOXD,
        &also,
    );
    expect![[r#"
        finished without a result
        payloads hold: ["slow"]
        provider.json routes: {"agent":[{"api":"responses","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"}]}
        outcome.json: {"agents":[{"role":"agent","submission":{"finalized":false,"payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4}},"work_end":{"kind":"deadline"}}],"verdict":{"failure":{"kind":"timeout"},"kind":"no_result"}}
        log.jsonl:
        {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"92000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
        {"actions":[{"kind":"reply","reply":{"kind":"call","remaining_seconds":1,"returned":{"state":"stored"}}}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"evaluated":{"effects":["set_submission"],"returned":[{"name":"state","value":"stored"}]},"grant":"submit","kind":"now","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4}}},"observed_ns":"0","seq":3}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"model_request_started","request":0},"observed_ns":"0","seq":4}
        {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"1000000000","event":{"kind":"tick"},"observed_ns":"1000000000","seq":5}
        {"at_ns":"1000000000","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 2 files of 97 bytes","seq":6}
        {"actions":[],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"agent_collected"},"seq":7}
        {"actions":[],"at_ns":"1000000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":8}
        {"actions":[{"job":0,"kind":"start_final_call","selections":[{"role":"agent","selection":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4}}],"timeout_ns":"1000000000"}],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"1000000000","seq":9}
        {"actions":[],"at_ns":"1000000000","event":{"job":0,"kind":"call_started"},"observed_ns":"1000000000","seq":10}
        {"actions":[],"at_ns":"1000000000","event":{"bytes":4,"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","job":0,"kind":"target_put","path":"/pumpkin/in/payload/raw"},"observed_ns":"1000000000","seq":11}
        {"actions":[],"at_ns":"1000000000","event":{"argv":["judge","/pumpkin/in/payload/raw"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"1000000000","seq":12}
        {"actions":[],"at_ns":"2000000000","event":{"job":0,"kind":"call_timed_out"},"observed_ns":"2000000000","seq":13}
        {"actions":[],"at_ns":"2000000000","event":{"job":0,"kind":"call_failed","reason":"the call's timeout passed"},"observed_ns":"2000000000","seq":14}
        {"actions":[],"at_ns":"2000000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":15}
        {"actions":[{"kind":"finish"}],"at_ns":"2000000000","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"2000000000","seq":16}
        log.jsonl, at no fixed place:
        {"actions":[],"at_ns":"1000000000","event":{"answer":{"kind":"cut"},"api":"responses","caller":"agent-0","first_byte_ns":"0","kept":{"request":{"body_bytes":173,"body_sha256":"47bf6d5b1bdebfa49b0689b8f69a5f19e3d3ae113fde67c00dd2d2edb81ec513","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":25}},"kind":"model_answered","model":"m-large","request":0,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"1000000000"}
        its last record came before the agent's teardown was to end
        agent `agent`'s standard output:
        model request 0: 200 [("content-type", "text/event-stream")] event: response.created\n\n
        model exchange 0 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:8b740cd5d26befcc0dbb835cfb3849a3cdd15f134987d273e263503033017963"],"model":"vendor/m-large","store":false}; response event: response.created\n\n
        model items kept: 2
        the model endpoint at each create: [true, false]
        a stalled stream's client hung up before the agent's teardown was to end"#]]
    .assert_eq(&shown);
}

#[test]
fn model_grace_cut() {
    // The sleep moves the clock to the grace's end.
    let argv = [
        "ops",
        "ask m-large stall",
        "call submit 6 finalize=true",
        "sleep 1",
    ];
    let mut ir = routed(ops_workflow(&argv, 30));
    // So that the grace runs out while the agent still waits on the stalled stream.
    ir["roles"][0]["runtime"]["agent"]["finalize_grace_seconds"] = json!(1);
    // The stalled request ends as the grace's end ends the agent's route, beside the agent's
    // teardown, which nothing orders it against.
    let also = [Also::Exchanges, Also::Racing(r#""answer":{"kind":"cut"}"#)];
    let shown = executed(
        &SimHost::new(),
        "model-grace-cut",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &also,
    );
    expect![[r#"
        finished, showing [score=6]
        payloads hold: ["xxxxxx"]
        provider.json routes: {"agent":[{"api":"responses","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"}]}
        outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}},"work_end":{"kind":"finalized"}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":6}]}}}
        log.jsonl:
        {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"241000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"model_request_started","request":0},"observed_ns":"0","seq":3}
        {"actions":[{"kind":"reply","reply":{"kind":"call","remaining_seconds":1,"returned":{"state":"accepted"}}}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"evaluated":{"effects":["set_submission","end_work"],"returned":[{"name":"state","value":"accepted"}]},"grant":"submit","kind":"now","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":4}
        {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"1000000000","event":{"kind":"tick"},"observed_ns":"1000000000","seq":5}
        {"at_ns":"1000000000","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 2 files of 97 bytes","seq":6}
        {"actions":[],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"agent_collected"},"seq":7}
        {"actions":[],"at_ns":"1000000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":8}
        {"actions":[{"job":0,"kind":"start_final_call","selections":[{"role":"agent","selection":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}],"timeout_ns":"120000000000"}],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"1000000000","seq":9}
        {"actions":[],"at_ns":"1000000000","event":{"job":0,"kind":"call_started"},"observed_ns":"1000000000","seq":10}
        {"actions":[],"at_ns":"1000000000","event":{"bytes":6,"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","job":0,"kind":"target_put","path":"/pumpkin/in/payload/raw"},"observed_ns":"1000000000","seq":11}
        {"actions":[],"at_ns":"1000000000","event":{"argv":["judge","/pumpkin/in/payload/raw"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"1000000000","seq":12}
        {"actions":[],"at_ns":"1000000000","event":{"command":0,"end":{"exited":{"code":0}},"job":0,"kind":"command_ended"},"observed_ns":"1000000000","seq":13}
        {"actions":[],"at_ns":"1000000000","event":{"bytes":15,"digest":"sha256:68e3ea1c52db57d6e28c6d873ef07678bc738b836a1ec8ae5f831875083035e9","job":0,"kind":"target_get","path":"/pumpkin/out/result.json"},"observed_ns":"1000000000","seq":14}
        {"actions":[],"at_ns":"1000000000","event":{"job":0,"kind":"call_returned","returned":[{"name":"score","value":6}]},"observed_ns":"1000000000","seq":15}
        {"actions":[],"at_ns":"1000000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":16}
        {"actions":[{"kind":"finish"}],"at_ns":"1000000000","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"1000000000","seq":17}
        log.jsonl, at no fixed place:
        {"actions":[],"at_ns":"1000000000","event":{"answer":{"kind":"cut"},"api":"responses","caller":"agent-0","first_byte_ns":"0","kept":{"request":{"body_bytes":173,"body_sha256":"47bf6d5b1bdebfa49b0689b8f69a5f19e3d3ae113fde67c00dd2d2edb81ec513","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":25}},"kind":"model_answered","model":"m-large","request":0,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"1000000000"}
        its last record came before the agent's teardown was to end
        agent `agent`'s standard output:
        model request 0: 200 [("content-type", "text/event-stream")] event: response.created\n\n
        model exchange 0 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:8b740cd5d26befcc0dbb835cfb3849a3cdd15f134987d273e263503033017963"],"model":"vendor/m-large","store":false}; response event: response.created\n\n
        model items kept: 2
        the model endpoint at each create: [true, false]
        a stalled stream's client hung up before the agent's teardown was to end"#]]
    .assert_eq(&shown);
}

fn route() -> Value {
    json!({ "model": [{ "api": "responses", "models": ["m-small"] }] })
}

/// `ir` with its trainer routed to `m-small`, where `train` runs `ops` with `steps`.
fn trainer_routed(ir: Value, steps: &[&str]) -> Value {
    let argv: Vec<&str> = iter::once("ops").chain(steps.iter().copied()).collect();
    let mut ir = train_runs(ir, &argv);
    ir["roles"][1]["runtime"]["target"]["egress"] = route();
    ir
}

/// `ir` whose `train` runs `argv` on its target, through the tests' body `run`, with a call
/// timeout of 20 s, longer than what `argv` does takes.
fn train_runs(mut ir: Value, argv: &[&str]) -> Value {
    let train = &mut ir["roles"][0]["runtime"]["agent"]["grants"][0]["job"];
    train["body"]["def"] = json!("run");
    train["keywords"] = json!({ "argv": argv });
    train["call_timeout_seconds"] = json!(20);
    ir
}

// The final judge's budget for `m-small` is under its first response's cost, so its second
// request is barred, and the score comes from `m-large`.
#[test]
fn final_call_model() {
    let argv = ["ops", "model m-large stream", "call submit 6 finalize=true"];
    let mut ir = routed(ops_workflow(&argv, 30));
    ir["roles"][2]["runtime"]["target"]["egress"]["model"] = json!([
        { "api": "responses", "models": ["m-small"], "budget": { "microusd": 50 } },
        { "api": "responses", "models": ["m-large"] },
    ]);
    ir["final"]["body"]["def"] = json!("routed");
    let argv = [
        "ops",
        "model m-small usage",
        "model m-small 5",
        "model m-large 7",
        "result",
    ];
    ir["final"]["keywords"] = json!({ "argv": argv });
    let shown = executed(
        &SimHost::new(),
        "final-call-model",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &[Also::Usage, Also::Exchanges],
    );
    expect![[r#"
        finished, showing [score=7 raw=7]
        payloads hold: ["xxxxxx"]
        provider.json routes: {"agent":[{"api":"responses","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"}],"final_judge":[{"api":"responses","models":{"m-small":{"upstream_name":"vendor/m-small","usd_per_mtok":{"cache_write":3.75,"cached":0.3,"input":3.0,"output":15.0}}},"upstream":"stand-in"},{"api":"responses","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"}]}
        outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":7},{"name":"raw","value":7}]}}}
        outcome.json's usage: {"calls":[{"grant":null,"job":0,"model_requests":3,"tokens":{"cached":8,"input":12,"output":5,"reasoning":3,"responses":1}}],"model_requests":4,"ran_ns":"<ns>","sandboxes":[{"class":"sim","count":2,"lasted_ns":"<ns>","provider":"simulated"}],"tokens":{"cached":8,"input":12,"output":5,"reasoning":3,"responses":1},"unpriced_models":["m-large"],"usd":"<usd>"}
        log.jsonl:
        {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"model_request_started","request":0},"observed_ns":"0","seq":3}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent-0","first_byte_ns":"0","kept":{"request":{"body_bytes":174,"body_sha256":"8d6175ce51c5e9942c5037737b41cabe7120f8a38f746b122528340427199599","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":252}},"kind":"model_answered","model":"m-large","request":0,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"0","seq":4}
        {"actions":[{"kind":"reply","reply":{"kind":"call","remaining_seconds":30,"returned":{"state":"accepted"}}}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"evaluated":{"effects":["set_submission","end_work"],"returned":[{"name":"state","value":"accepted"}]},"grant":"submit","kind":"now","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":5}
        {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":6}
        {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 2 files of 324 bytes","seq":7}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":8}
        {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":9}
        {"actions":[{"job":0,"kind":"start_final_call","selections":[{"role":"agent","selection":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}],"timeout_ns":"120000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":10}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":11}
        {"actions":[],"at_ns":"0","event":{"argv":["ops","model m-small usage","model m-small 5","model m-large 7","result"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"0","seq":12}
        {"actions":[],"at_ns":"0","event":{"caller":"job-0","kind":"model_request_started","request":1},"observed_ns":"0","seq":13}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"job-0","first_byte_ns":"0","first_token_ns":"0","kept":{"request":{"body_bytes":173,"body_sha256":"adf416133003fe506e3c6c79d89b48d93a2fab8fa6183b4146e91bfcb0cdf293","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":330}},"kind":"model_answered","model":"m-small","request":1,"stream":false,"upstream":"stand-in","upstream_status":200,"usage":{"cached_tokens":8,"input_tokens":12,"output_tokens":5,"reasoning_tokens":3}},"observed_ns":"0","seq":14}
        {"actions":[],"at_ns":"0","event":{"answer":{"bar":"budget","kind":"barred","message":"the model budget of the role's entry for `m-small`, $0.00005 at the configured prices, is exhausted: make no more requests for its models","status":400},"api":"responses","caller":"job-0","kept":{"request":{"body_bytes":162,"body_sha256":"fbad949ecafca6d36e3c4e8eb7a799e56b2d6e36bc2cdab84f9326a5cdf05b4f","bytes":162},"response":{"bytes":217}},"kind":"model_answered","request":2,"upstream":"stand-in","upstream_status":null},"observed_ns":"0","seq":15}
        {"actions":[],"at_ns":"0","event":{"caller":"job-0","kind":"model_request_started","request":3},"observed_ns":"0","seq":16}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"job-0","first_byte_ns":"0","kept":{"request":{"body_bytes":169,"body_sha256":"cc92fd8a18f8a014cf749e2c1d5e30186fd25dad8e510b8101c8c4c16eed5839","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":1}},"kind":"model_answered","model":"m-large","request":3,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"0","seq":17}
        {"actions":[],"at_ns":"0","event":{"command":0,"end":{"exited":{"code":0}},"job":0,"kind":"command_ended"},"observed_ns":"0","seq":18}
        {"actions":[],"at_ns":"0","event":{"bytes":15,"digest":"sha256:0e20615f5979c6452a398779743c38e097227a573e33c6adaa59a74dc8f3c385","job":0,"kind":"target_get","path":"/pumpkin/out/result.json"},"observed_ns":"0","seq":19}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_returned","model_requests_over_budget":1,"returned":[{"name":"score","value":7},{"name":"raw","value":7}]},"observed_ns":"0","seq":20}
        {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":21}
        {"actions":[{"kind":"finish"}],"at_ns":"0","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"0","seq":22}
        its last record came before the agent's teardown was to end
        agent `agent`'s standard output:
        model request 0: 200 [("content-type", "text/event-stream")] data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
        model exchange 0 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:af497cd3652d5371734347499d7dd2db68a62c28fe107f368d77cff1b78d474a"],"model":"vendor/m-large","store":false}; response data: POST /v1/responses HTTP/1.1 Some(("authorization", "Bearer sk-test")) {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"stream","role":"user","type":"message"}],"model":"vendor/m-large","store":false}\n\n
        model exchange 1 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:6bc0b5dcffbd963414c779a049374557aba7edf0b7655c06cfd15b83b4901d9d"],"model":"vendor/m-small","store":false}; response data: {"type":"response.created","response":{"usage":null}}\n\ndata: {"type":"response.output_text.delta","delta":"6"}\n\nevent: response.completed\ndata: {"type":"response.completed","response":{"usage":{"input_tokens":12,"input_tokens_details":{"cached_tokens":8},"output_tokens":5,"output_tokens_details":{"reasoning_tokens":3}}}}\n\n
        model exchange 2 kept: request {"input":[{"content":"You are a simulated agent.","role":"user","type":"message"},{"content":"5","role":"user","type":"message"}],"model":"m-small","store":false}; response {"error":{"code":null,"message":"the model budget of the role's entry for `m-small`, $0.00005 at the configured prices, is exhausted: make no more requests for its models","param":null,"type":"invalid_request_error"}}
        model exchange 3 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:5559c1b7a436c81115685858ea5a05bca66397f56a0d93101977bad52de50aea"],"model":"vendor/m-large","store":false}; response 7
        model items kept: 4
        the model endpoint at each create: [true, true]"#]]
    .assert_eq(&shown);
}

// The call's route ends with the call, so its request in flight is cut short, while the agent's
// stream goes on.
#[test]
#[expect(clippy::too_many_lines, reason = "most of it is its snapshot")]
fn call_and_agent_model() {
    let argv = [
        "ops",
        "ask m-large stall",
        "call train 3",
        "await 0 completed",
        "call submit 6",
        "exit",
        // Each route's end cuts its stream, beside the teardown of its caller's sandbox. The
        // agent's sandbox answers its teardown's freeze once its cut is logged.
        r#"await_log model_answered model="m-large""#,
    ];
    let steps = ["model m-small 5", "ask m-small stall", "exit"];
    let mut ir = trainer_routed(routed(ops_workflow(&argv, 30)), &steps);
    ir["roles"][0]["name"] = json!("solver");
    ir["roles"].as_array_mut().unwrap().reverse();
    // The call's delete is recorded once its cut is.
    let deleted = Synced::Holding("jobs/0/.sandbox.json.partial", DELETED);
    let cut = r#""answer":{"kind":"cut"},"api":"responses","caller":"job-0""#;
    let shown = executed(
        &SimHost::new().held_sync(deleted, cut),
        "call-and-agent-model",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &[Also::Usage, Also::Exchanges],
    );
    expect![[r#"
        finished, showing [score=6]
        payloads hold: ["xxx", "xxxxxx"]
        provider.json routes: {"solver":[{"api":"responses","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"}],"trainer":[{"api":"responses","models":{"m-small":{"upstream_name":"vendor/m-small","usd_per_mtok":{"cache_write":3.75,"cached":0.3,"input":3.0,"output":15.0}}},"upstream":"stand-in"}]}
        outcome.json: {"agents":[{"role":"solver","submission":{"finalized":false,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":6}]}}}
        outcome.json's usage: {"calls":[{"grant":"train","job":0,"model_requests":2,"tokens":{"input":0,"output":0,"responses":0}}],"model_requests":3,"ran_ns":"<ns>","sandboxes":[{"class":"sim","count":3,"lasted_ns":"<ns>","provider":"simulated"}],"tokens":{"input":0,"output":0,"responses":0}}
        log.jsonl:
        {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-2","kind":"agent_started"},"observed_ns":"0","seq":2}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-2","kind":"model_request_started","request":0},"observed_ns":"0","seq":3}
        {"actions":[{"kind":"reply","reply":{"job":0,"kind":"call","remaining":{"calls":99,"seconds":null}}},{"caller":"agent-2","grant":"train","job":0,"kind":"start_call","payload":{"digest":"sha256:cd2eb0837c9b4c962c22d2ff8b5441b7b45805887f051d39bf133b583baf6860","size":3},"timeout_ns":"20000000000"}],"at_ns":"0","event":{"caller":"agent-2","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"call","payload":{"digest":"sha256:cd2eb0837c9b4c962c22d2ff8b5441b7b45805887f051d39bf133b583baf6860","size":3}}},"observed_ns":"0","seq":4}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":5}
        {"actions":[],"at_ns":"0","event":{"bytes":3,"digest":"sha256:cd2eb0837c9b4c962c22d2ff8b5441b7b45805887f051d39bf133b583baf6860","job":0,"kind":"target_put","path":"/pumpkin/in/payload/raw"},"observed_ns":"0","seq":6}
        {"actions":[],"at_ns":"0","event":{"argv":["ops","model m-small 5","ask m-small stall","exit"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"0","seq":7}
        {"actions":[],"at_ns":"0","event":{"caller":"job-0","kind":"model_request_started","request":1},"observed_ns":"0","seq":8}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"job-0","first_byte_ns":"0","kept":{"request":{"body_bytes":169,"body_sha256":"0687d2985455096f15d6a4404ef14584b391c7b3d31d6355df3fbfda36a3a57a","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":1}},"kind":"model_answered","model":"m-small","request":1,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"0","seq":9}
        {"actions":[],"at_ns":"0","event":{"caller":"job-0","kind":"model_request_started","request":2},"observed_ns":"0","seq":10}
        {"actions":[],"at_ns":"0","event":{"command":0,"end":{"exited":{"code":0}},"job":0,"kind":"command_ended"},"observed_ns":"0","seq":11}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_returned","returned":[]},"observed_ns":"0","seq":12}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"cut"},"api":"responses","caller":"job-0","first_byte_ns":"0","kept":{"request":{"body_bytes":173,"body_sha256":"ec0d3ba6df672edb7aa80d93ee65128ce79f7ebf9cd6691b1adefef48fed5c2b","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":25}},"kind":"model_answered","model":"m-small","request":2,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"0","seq":13}
        {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":14}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"0","seq":15}
        {"actions":[{"kind":"reply","reply":{"kind":"call","remaining_seconds":30,"returned":{"state":"stored"}}}],"at_ns":"0","event":{"caller":"agent-2","kind":"op_arrived","op":1,"request":{"evaluated":{"effects":["set_submission"],"returned":[{"name":"state","value":"stored"}]},"grant":"submit","kind":"now","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":16}
        {"actions":[{"caller":"agent-2","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-2","kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":17}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"cut"},"api":"responses","caller":"agent-2","first_byte_ns":"0","kept":{"request":{"body_bytes":173,"body_sha256":"47bf6d5b1bdebfa49b0689b8f69a5f19e3d3ae113fde67c00dd2d2edb81ec513","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":25}},"kind":"model_answered","model":"m-large","request":0,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"0","seq":18}
        {"at_ns":"0","note":"collected from agent `solver`'s sandbox: out, 1 file of 9 bytes; log, 2 files of 97 bytes","seq":19}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-2","kind":"agent_collected"},"seq":20}
        {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-2"},"seq":21}
        {"actions":[{"job":1,"kind":"start_final_call","selections":[{"role":"solver","selection":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}],"timeout_ns":"120000000000"}],"at_ns":"0","event":{"caller":"agent-2","kind":"agent_torn_down"},"observed_ns":"0","seq":22}
        {"actions":[],"at_ns":"0","event":{"job":1,"kind":"call_started"},"observed_ns":"0","seq":23}
        {"actions":[],"at_ns":"0","event":{"bytes":6,"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","job":1,"kind":"target_put","path":"/pumpkin/in/payload/raw"},"observed_ns":"0","seq":24}
        {"actions":[],"at_ns":"0","event":{"argv":["judge","/pumpkin/in/payload/raw"],"command":0,"job":1,"kind":"target_run"},"observed_ns":"0","seq":25}
        {"actions":[],"at_ns":"0","event":{"command":0,"end":{"exited":{"code":0}},"job":1,"kind":"command_ended"},"observed_ns":"0","seq":26}
        {"actions":[],"at_ns":"0","event":{"bytes":15,"digest":"sha256:68e3ea1c52db57d6e28c6d873ef07678bc738b836a1ec8ae5f831875083035e9","job":1,"kind":"target_get","path":"/pumpkin/out/result.json"},"observed_ns":"0","seq":27}
        {"actions":[],"at_ns":"0","event":{"job":1,"kind":"call_returned","returned":[{"name":"score","value":6}]},"observed_ns":"0","seq":28}
        {"actions":[],"at_ns":"0","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-1"},"seq":29}
        {"actions":[{"kind":"finish"}],"at_ns":"0","event":{"job":1,"kind":"call_torn_down"},"observed_ns":"0","seq":30}
        its last record came before the agent's teardown was to end
        agent `solver`'s standard output:
        model request 0: 200 [("content-type", "text/event-stream")] event: response.created\n\n
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
fn call_key_refused() {
    let argv = [
        "ops",
        "call train 3",
        "await 0 completed",
        "call submit 6 finalize=true",
    ];
    let ir = trainer_routed(ops_workflow(&argv, 30), &["model m-small expired"]);
    let shown = executed(
        &SimHost::new(),
        "call-key-refused",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &[Also::Usage, Also::Exchanges],
    );
    expect![[r#"
        finished, showing [score=6]
        payloads hold: ["xxx", "xxxxxx"]
        provider.json routes: {"trainer":[{"api":"responses","models":{"m-small":{"upstream_name":"vendor/m-small","usd_per_mtok":{"cache_write":3.75,"cached":0.3,"input":3.0,"output":15.0}}},"upstream":"stand-in"}]}
        outcome.json: {"agents":[{"role":"agent","route_failure":"the upstream `stand-in` refused its key, which ended job 0's model route: give it a key that it takes, and run again","submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":6}]}}}
        outcome.json's usage: {"calls":[{"grant":"train","job":0,"model_requests":1,"tokens":{"input":0,"output":0,"responses":0}}],"model_requests":1,"ran_ns":"<ns>","sandboxes":[{"class":"sim","count":3,"lasted_ns":"<ns>","provider":"simulated"}],"tokens":{"input":0,"output":0,"responses":0}}
        log.jsonl:
        {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
        {"actions":[{"kind":"reply","reply":{"job":0,"kind":"call","remaining":{"calls":99,"seconds":null}}},{"caller":"agent-0","grant":"train","job":0,"kind":"start_call","payload":{"digest":"sha256:cd2eb0837c9b4c962c22d2ff8b5441b7b45805887f051d39bf133b583baf6860","size":3},"timeout_ns":"20000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"call","payload":{"digest":"sha256:cd2eb0837c9b4c962c22d2ff8b5441b7b45805887f051d39bf133b583baf6860","size":3}}},"observed_ns":"0","seq":3}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":4}
        {"actions":[],"at_ns":"0","event":{"bytes":3,"digest":"sha256:cd2eb0837c9b4c962c22d2ff8b5441b7b45805887f051d39bf133b583baf6860","job":0,"kind":"target_put","path":"/pumpkin/in/payload/raw"},"observed_ns":"0","seq":5}
        {"actions":[],"at_ns":"0","event":{"argv":["ops","model m-small expired"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"0","seq":6}
        {"actions":[],"at_ns":"0","event":{"caller":"job-0","kind":"model_request_started","request":0},"observed_ns":"0","seq":7}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"refused","message":"the model provider refused the route's key, which ends the model route: make no more model requests","status":400},"api":"responses","caller":"job-0","first_byte_ns":"0","kept":{"request":{"body_bytes":175,"body_sha256":"3c0cdf32d3c30701e3f2321e9bdb0d6bf020435c0f7bb528cc43be1139395b72","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":7}},"kind":"model_answered","model":"m-small","request":0,"stream":false,"upstream":"stand-in","upstream_status":401},"observed_ns":"0","seq":8}
        {"actions":[],"at_ns":"0","event":{"command":0,"end":{"exited":{"code":0}},"job":0,"kind":"command_ended"},"observed_ns":"0","seq":9}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_returned","returned":[]},"observed_ns":"0","seq":10}
        {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":11}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"0","seq":12}
        {"actions":[{"kind":"reply","reply":{"kind":"call","remaining_seconds":30,"returned":{"state":"accepted"}}}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":1,"request":{"evaluated":{"effects":["set_submission","end_work"],"returned":[{"name":"state","value":"accepted"}]},"grant":"submit","kind":"now","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":13}
        {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":14}
        {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":15}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":16}
        {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":17}
        {"actions":[{"job":1,"kind":"start_final_call","selections":[{"role":"agent","selection":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}],"timeout_ns":"120000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":18}
        {"actions":[],"at_ns":"0","event":{"job":1,"kind":"call_started"},"observed_ns":"0","seq":19}
        {"actions":[],"at_ns":"0","event":{"bytes":6,"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","job":1,"kind":"target_put","path":"/pumpkin/in/payload/raw"},"observed_ns":"0","seq":20}
        {"actions":[],"at_ns":"0","event":{"argv":["judge","/pumpkin/in/payload/raw"],"command":0,"job":1,"kind":"target_run"},"observed_ns":"0","seq":21}
        {"actions":[],"at_ns":"0","event":{"command":0,"end":{"exited":{"code":0}},"job":1,"kind":"command_ended"},"observed_ns":"0","seq":22}
        {"actions":[],"at_ns":"0","event":{"bytes":15,"digest":"sha256:68e3ea1c52db57d6e28c6d873ef07678bc738b836a1ec8ae5f831875083035e9","job":1,"kind":"target_get","path":"/pumpkin/out/result.json"},"observed_ns":"0","seq":23}
        {"actions":[],"at_ns":"0","event":{"job":1,"kind":"call_returned","returned":[{"name":"score","value":6}]},"observed_ns":"0","seq":24}
        {"actions":[],"at_ns":"0","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-1"},"seq":25}
        {"actions":[{"kind":"finish"}],"at_ns":"0","event":{"job":1,"kind":"call_torn_down"},"observed_ns":"0","seq":26}
        its last record came before the agent's teardown was to end
        model exchange 0 kept: request {"input":["sha256:7299370c4d7aceb2a25f8c7cd9960c3999397e6f89a015f536d8c1e0a73388a8","sha256:6752f5d96dfccaffbf0772742e4557281e807f48cc031258473eb59fb6390c57"],"model":"vendor/m-small","store":false}; response refused
        model items kept: 2
        the model endpoint at each create: [false, true, false]"#]]
    .assert_eq(&shown);
}

// A priced response costs $0.0000894: the agent's, through `responses`, leaves its `chat` entry's
// $0.00005 unspent; `train`'s fourth passes $0.0003, its other entry answers, none takes `m-huge`.
#[test]
#[expect(clippy::too_many_lines, reason = "most of it is its snapshot")]
fn budgets_spent() {
    let argv = [
        "ops",
        "model m-small usage",
        "chat m-small 5",
        "call train 3",
        "await 0 completed",
        "call train 3",
        "await 1 completed",
    ];
    let steps = [
        "model m-small 5",
        "model m-small cut",
        "model m-small usage",
        "model m-small 5",
        "model m-large 5",
        "model m-huge 5",
    ];
    let mut ir = trainer_routed(ops_workflow(&argv, 30), &steps);
    ir["roles"][1]["runtime"]["target"]["egress"]["model"] = json!([
        { "api": "responses", "models": ["m-small"], "budget": { "microusd": 300 } },
        { "api": "responses", "models": ["m-large"] },
    ]);
    ir["roles"][0]["runtime"]["agent"]["egress"]["model"] = json!([
        { "api": "responses", "models": ["m-small"] },
        { "api": "chat", "models": ["m-small"], "budget": { "microusd": 50 } },
    ]);
    let shown = executed(
        &SimHost::new(),
        "budgets-spent",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &[Also::Usage],
    );
    expect![[r#"
        finished, showing [score=0]
        payloads hold: ["xxx"]
        provider.json routes: {"agent":[{"api":"responses","models":{"m-small":{"upstream_name":"vendor/m-small","usd_per_mtok":{"cache_write":3.75,"cached":0.3,"input":3.0,"output":15.0}}},"upstream":"stand-in"},{"api":"chat","models":{"m-small":{"upstream_name":"vendor/m-small","usd_per_mtok":{"cache_write":3.75,"cached":0.3,"input":3.0,"output":15.0}}},"upstream":"stand-in"}],"trainer":[{"api":"responses","models":{"m-small":{"upstream_name":"vendor/m-small","usd_per_mtok":{"cache_write":3.75,"cached":0.3,"input":3.0,"output":15.0}}},"upstream":"stand-in"},{"api":"responses","models":{"m-large":{"upstream_name":"vendor/m-large"}},"upstream":"stand-in"}]}
        outcome.json: {"agents":[{"role":"agent","submission":null,"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":0}]}}}
        outcome.json's usage: {"calls":[{"grant":"train","job":0,"model_requests":6,"tokens":{"cached":16,"input":24,"output":10,"reasoning":6,"responses":2}},{"grant":"train","job":1,"model_requests":6,"tokens":{"cached":16,"input":24,"output":10,"reasoning":6,"responses":2}}],"model_requests":14,"ran_ns":"<ns>","sandboxes":[{"class":"sim","count":4,"lasted_ns":"<ns>","provider":"simulated"}],"tokens":{"cached":40,"input":60,"output":25,"reasoning":15,"responses":5},"unpriced_models":["m-large"],"unreported_responses":6,"usd":"<usd>"}
        log.jsonl:
        {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"model_request_started","request":0},"observed_ns":"0","seq":3}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"agent-0","first_byte_ns":"0","first_token_ns":"0","kept":{"request":{"body_bytes":173,"body_sha256":"adf416133003fe506e3c6c79d89b48d93a2fab8fa6183b4146e91bfcb0cdf293","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":330}},"kind":"model_answered","model":"m-small","request":0,"stream":false,"upstream":"stand-in","upstream_status":200,"usage":{"cached_tokens":8,"input_tokens":12,"output_tokens":5,"reasoning_tokens":3}},"observed_ns":"0","seq":4}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"model_request_started","request":1},"observed_ns":"0","seq":5}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"chat","caller":"agent-0","first_byte_ns":"0","kept":{"request":{"body_bytes":158,"body_sha256":"feaee5bb9ff1d4d0d01cb26f2632e7bec66dd081e81f53f412d80a73d811ecb4","bytes":187,"items":{"count":2,"field":"messages"}},"response":{"bytes":1}},"kind":"model_answered","model":"m-small","request":1,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"0","seq":6}
        {"actions":[{"kind":"reply","reply":{"job":0,"kind":"call","remaining":{"calls":99,"seconds":null}}},{"caller":"agent-0","grant":"train","job":0,"kind":"start_call","payload":{"digest":"sha256:cd2eb0837c9b4c962c22d2ff8b5441b7b45805887f051d39bf133b583baf6860","size":3},"timeout_ns":"20000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"call","payload":{"digest":"sha256:cd2eb0837c9b4c962c22d2ff8b5441b7b45805887f051d39bf133b583baf6860","size":3}}},"observed_ns":"0","seq":7}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":8}
        {"actions":[],"at_ns":"0","event":{"bytes":3,"digest":"sha256:cd2eb0837c9b4c962c22d2ff8b5441b7b45805887f051d39bf133b583baf6860","job":0,"kind":"target_put","path":"/pumpkin/in/payload/raw"},"observed_ns":"0","seq":9}
        {"actions":[],"at_ns":"0","event":{"argv":["ops","model m-small 5","model m-small cut","model m-small usage","model m-small 5","model m-large 5","model m-huge 5"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"0","seq":10}
        {"actions":[],"at_ns":"0","event":{"caller":"job-0","kind":"model_request_started","request":2},"observed_ns":"0","seq":11}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"job-0","first_byte_ns":"0","kept":{"request":{"body_bytes":169,"body_sha256":"0687d2985455096f15d6a4404ef14584b391c7b3d31d6355df3fbfda36a3a57a","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":1}},"kind":"model_answered","model":"m-small","request":2,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"0","seq":12}
        {"actions":[],"at_ns":"0","event":{"caller":"job-0","kind":"model_request_started","request":3},"observed_ns":"0","seq":13}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"cut"},"api":"responses","caller":"job-0","first_byte_ns":"0","first_token_ns":"0","kept":{"request":{"body_bytes":171,"body_sha256":"c038d8c8ced74099f33f300abdb4f2be4ac15b5575f81b89ed1ab5d833607b01","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":330}},"kind":"model_answered","model":"m-small","request":3,"stream":false,"upstream":"stand-in","upstream_status":200,"usage":{"cached_tokens":8,"input_tokens":12,"output_tokens":5,"reasoning_tokens":3}},"observed_ns":"0","seq":14}
        {"actions":[],"at_ns":"0","event":{"caller":"job-0","kind":"model_request_started","request":4},"observed_ns":"0","seq":15}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"job-0","first_byte_ns":"0","first_token_ns":"0","kept":{"request":{"body_bytes":173,"body_sha256":"adf416133003fe506e3c6c79d89b48d93a2fab8fa6183b4146e91bfcb0cdf293","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":330}},"kind":"model_answered","model":"m-small","request":4,"stream":false,"upstream":"stand-in","upstream_status":200,"usage":{"cached_tokens":8,"input_tokens":12,"output_tokens":5,"reasoning_tokens":3}},"observed_ns":"0","seq":16}
        {"actions":[],"at_ns":"0","event":{"caller":"job-0","kind":"model_request_started","request":5},"observed_ns":"0","seq":17}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"job-0","first_byte_ns":"0","kept":{"request":{"body_bytes":169,"body_sha256":"0687d2985455096f15d6a4404ef14584b391c7b3d31d6355df3fbfda36a3a57a","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":1}},"kind":"model_answered","model":"m-small","request":5,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"0","seq":18}
        {"actions":[],"at_ns":"0","event":{"caller":"job-0","kind":"model_request_started","request":6},"observed_ns":"0","seq":19}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"job-0","first_byte_ns":"0","kept":{"request":{"body_bytes":169,"body_sha256":"3fe3f6a7abc05ff3deda6543ea5e80da33cb0e0d5f32614fa82b53edfa88a70c","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":1}},"kind":"model_answered","model":"m-large","request":6,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"0","seq":20}
        {"actions":[],"at_ns":"0","event":{"answer":{"bar":"rules","kind":"barred","message":"the request names `m-huge`, which the route doesn't allow: name `m-small` or `m-large`","status":400},"caller":"job-0","kept":{"request":{"body_bytes":161,"body_sha256":"35f43b82f1d3a750893d2f76551f29bdae42d9d54c7e00c1d8af6b4b95768319","bytes":161},"response":{"bytes":166}},"kind":"model_answered","request":7,"upstream_status":null},"observed_ns":"0","seq":21}
        {"actions":[],"at_ns":"0","event":{"command":0,"end":{"exited":{"code":0}},"job":0,"kind":"command_ended"},"observed_ns":"0","seq":22}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_returned","returned":[]},"observed_ns":"0","seq":23}
        {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":24}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"0","seq":25}
        {"actions":[{"kind":"reply","reply":{"job":1,"kind":"call","remaining":{"calls":98,"seconds":null}}},{"caller":"agent-0","grant":"train","job":1,"kind":"start_call","payload":{"digest":"sha256:cd2eb0837c9b4c962c22d2ff8b5441b7b45805887f051d39bf133b583baf6860","size":3},"timeout_ns":"20000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":1,"request":{"grant":"train","kind":"call","payload":{"digest":"sha256:cd2eb0837c9b4c962c22d2ff8b5441b7b45805887f051d39bf133b583baf6860","size":3}}},"observed_ns":"0","seq":26}
        {"actions":[],"at_ns":"0","event":{"job":1,"kind":"call_started"},"observed_ns":"0","seq":27}
        {"actions":[],"at_ns":"0","event":{"bytes":3,"digest":"sha256:cd2eb0837c9b4c962c22d2ff8b5441b7b45805887f051d39bf133b583baf6860","job":1,"kind":"target_put","path":"/pumpkin/in/payload/raw"},"observed_ns":"0","seq":28}
        {"actions":[],"at_ns":"0","event":{"argv":["ops","model m-small 5","model m-small cut","model m-small usage","model m-small 5","model m-large 5","model m-huge 5"],"command":0,"job":1,"kind":"target_run"},"observed_ns":"0","seq":29}
        {"actions":[],"at_ns":"0","event":{"caller":"job-1","kind":"model_request_started","request":8},"observed_ns":"0","seq":30}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"job-1","first_byte_ns":"0","kept":{"request":{"body_bytes":169,"body_sha256":"0687d2985455096f15d6a4404ef14584b391c7b3d31d6355df3fbfda36a3a57a","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":1}},"kind":"model_answered","model":"m-small","request":8,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"0","seq":31}
        {"actions":[],"at_ns":"0","event":{"caller":"job-1","kind":"model_request_started","request":9},"observed_ns":"0","seq":32}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"cut"},"api":"responses","caller":"job-1","first_byte_ns":"0","first_token_ns":"0","kept":{"request":{"body_bytes":171,"body_sha256":"c038d8c8ced74099f33f300abdb4f2be4ac15b5575f81b89ed1ab5d833607b01","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":330}},"kind":"model_answered","model":"m-small","request":9,"stream":false,"upstream":"stand-in","upstream_status":200,"usage":{"cached_tokens":8,"input_tokens":12,"output_tokens":5,"reasoning_tokens":3}},"observed_ns":"0","seq":33}
        {"actions":[],"at_ns":"0","event":{"caller":"job-1","kind":"model_request_started","request":10},"observed_ns":"0","seq":34}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"job-1","first_byte_ns":"0","first_token_ns":"0","kept":{"request":{"body_bytes":173,"body_sha256":"adf416133003fe506e3c6c79d89b48d93a2fab8fa6183b4146e91bfcb0cdf293","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":330}},"kind":"model_answered","model":"m-small","request":10,"stream":false,"upstream":"stand-in","upstream_status":200,"usage":{"cached_tokens":8,"input_tokens":12,"output_tokens":5,"reasoning_tokens":3}},"observed_ns":"0","seq":35}
        {"actions":[],"at_ns":"0","event":{"answer":{"bar":"budget","kind":"barred","message":"the model budget of the role's entry for `m-small`, $0.0003 at the configured prices, is exhausted: make no more requests for its models","status":400},"api":"responses","caller":"job-1","kept":{"request":{"body_bytes":162,"body_sha256":"fbad949ecafca6d36e3c4e8eb7a799e56b2d6e36bc2cdab84f9326a5cdf05b4f","bytes":162},"response":{"bytes":216}},"kind":"model_answered","request":11,"upstream":"stand-in","upstream_status":null},"observed_ns":"0","seq":36}
        {"actions":[],"at_ns":"0","event":{"caller":"job-1","kind":"model_request_started","request":12},"observed_ns":"0","seq":37}
        {"actions":[],"at_ns":"0","event":{"answer":{"kind":"forwarded"},"api":"responses","caller":"job-1","first_byte_ns":"0","kept":{"request":{"body_bytes":169,"body_sha256":"3fe3f6a7abc05ff3deda6543ea5e80da33cb0e0d5f32614fa82b53edfa88a70c","bytes":198,"dropped_headers":["user-agent"],"items":{"count":2,"field":"input"}},"response":{"bytes":1}},"kind":"model_answered","model":"m-large","request":12,"stream":false,"upstream":"stand-in","upstream_status":200},"observed_ns":"0","seq":38}
        {"actions":[],"at_ns":"0","event":{"answer":{"bar":"rules","kind":"barred","message":"the request names `m-huge`, which the route doesn't allow: name `m-small` or `m-large`","status":400},"caller":"job-1","kept":{"request":{"body_bytes":161,"body_sha256":"35f43b82f1d3a750893d2f76551f29bdae42d9d54c7e00c1d8af6b4b95768319","bytes":161},"response":{"bytes":166}},"kind":"model_answered","request":13,"upstream_status":null},"observed_ns":"0","seq":39}
        {"actions":[],"at_ns":"0","event":{"command":0,"end":{"exited":{"code":0}},"job":1,"kind":"command_ended"},"observed_ns":"0","seq":40}
        {"actions":[],"at_ns":"0","event":{"job":1,"kind":"call_returned","model_requests_over_budget":1,"returned":[]},"observed_ns":"0","seq":41}
        {"actions":[],"at_ns":"0","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-1"},"seq":42}
        {"actions":[],"at_ns":"0","event":{"job":1,"kind":"call_torn_down"},"observed_ns":"0","seq":43}
        {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":44}
        {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 2 files of 463 bytes","seq":45}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":46}
        {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":47}
        {"actions":[{"job":2,"kind":"start_final_call","selections":[{"role":"agent","selection":null}],"timeout_ns":"120000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":48}
        {"actions":[],"at_ns":"0","event":{"job":2,"kind":"call_started"},"observed_ns":"0","seq":49}
        {"actions":[],"at_ns":"0","event":{"job":2,"kind":"call_returned","returned":[{"name":"score","value":0}]},"observed_ns":"0","seq":50}
        {"actions":[],"at_ns":"0","event":{"id":"sim-4","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-2"},"seq":51}
        {"actions":[{"kind":"finish"}],"at_ns":"0","event":{"job":2,"kind":"call_torn_down"},"observed_ns":"0","seq":52}
        its last record came before the agent's teardown was to end
        agent `agent`'s standard output:
        model request 0: 200 [("content-type", "text/event-stream")] data: {"type":"response.created","response":{"usage":null}}\n\ndata: {"type":"response.output_text.delta","delta":"6"}\n\nevent: response.completed\ndata: {"type":"response.completed","response":{"usage":{"input_tokens":12,"input_tokens_details":{"cached_tokens":8},"output_tokens":5,"output_tokens_details":{"reasoning_tokens":3}}}}\n\n
        model request 1: 200 [("content-type", "text/plain")] 5
        the model endpoint at each create: [true, true, true, false]"#]].assert_eq(&shown);
}

// A simulated host's time, as the Unix time that its runs read.
fn sim_unix_ns(time: &SimTime) -> u64 {
    let unix = time.unix().duration_since(UNIX_EPOCH).unwrap();
    u64::try_from(unix.as_nanos()).unwrap()
}

fn unix_ns() -> u64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    u64::try_from(since.as_nanos()).unwrap()
}

fn nanos(value: &Value) -> Option<u64> {
    value.as_str()?.parse().ok()
}

// A run holds what it reserved until its end, interrupted or not: a signal ends the second
// before it publishes, and a third is then prepared.
#[test]
fn holds_until_its_end() {
    let runs = TestDir::new();
    let sim = Sim::new().with_cpus(16);
    let ir: ir::Workflow = serde_json::from_value(workflow(&["exit", "0"])).unwrap();
    let unpriced = BTreeMap::new();
    let prepare = |name: &str| {
        let placed = placed(&ir, sim.ready()).unwrap();
        let dir = runs.path().join(name);
        let Ok(servings) = placed
            .workflow()
            .by_role(|_| Ok::<_, Infallible>(Vec::new()));
        let on = providers_of(&placed, |_| &sim);
        let recorded = Recorded {
            args: &[],
            prices: &unpriced,
            sources: &NO_SOURCES,
        };
        let id = new_id(SystemTime::now()).unwrap();
        let budgets = runtime::budgets(placed.workflow(), &unpriced).unwrap();
        let models = ModelRoutes {
            servings: &servings,
            budgets,
        };
        Run::prepare(&System, placed, on, &dir, id, models, recorded)
    };
    let prepared = |run: &Result<Run<'_, System>, String>| match run {
        Ok(_) => "prepared".to_owned(),
        Err(error) => format!("refused: {error}"),
    };
    let execute = |run: Run<'_, System>| {
        let record = run.dir().to_owned();
        let executed = run.execute(
            Path::new(SANDBOXD),
            None,
            &Dialer::new(test_support::edge::client()),
            &Bodies { record: &record },
            || Ok(()),
            driver_used,
        );
        match executed {
            Ok(outcome) => finished(&outcome),
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
        the first: finished, showing [score=0]
        then the second: prepared
        the second: was interrupted by SIGTERM
        then a third: prepared"#]].assert_eq(&lines.join("\n"));
}

fn lease_workflow(argv: &[&str], deadline: u64, image: &str) -> Value {
    let mut ir = ops_workflow(argv, deadline);
    let mut runner = ir["roles"][1].clone();
    runner["name"] = json!("gpu_runner");
    runner["image"] = json!(image);
    runner["runtime"] = json!({ "runner": { "cwd": "/work", "env": ["CUDA=1"] } });
    ir["roles"].as_array_mut().unwrap().push(runner);
    let grant = json!({ "name": "gpu", "session": {
        "runner": "gpu_runner", "instances": 2,
        "instance_seconds": 600, "commands": 4, "transfer_bytes": 64,
    } });
    ir["roles"][0]["runtime"]["agent"]["grants"]
        .as_array_mut()
        .unwrap()
        .insert(1, grant);
    ir
}

const RUNNER: &str = "sha256:3333333333333333333333333333333333333333333333333333333333333333";

/// [`ops_workflow`]'s, with two targets: `checker`, of the agent's job function `check`, and
/// `slower`, of `slow`, each function's body the tests' `def` of its name ([`Bodies`]), `check`'s
/// with its caller's `label`.
fn calling_workflow(argv: &[&str], deadline: u64) -> Value {
    let mut ir = ops_workflow(argv, deadline);
    for (name, image) in [("checker", RUNNER), ("slower", RUNNER_TOO)] {
        let mut runner = ir["roles"][1].clone();
        runner["name"] = json!(name);
        runner["image"] = json!(image);
        runner["runtime"] = json!({ "target": { "cwd": "/work" } });
        ir["roles"].as_array_mut().unwrap().push(runner);
    }
    let grants = ir["roles"][0]["runtime"]["agent"]["grants"]
        .as_array_mut()
        .unwrap();
    let label = json!({ "label": { "type": "str" } });
    for (name, target, arguments) in [("check", "checker", label), ("slow", "slower", json!({}))] {
        let body = json!({ "file": "//check.star", "def": name, "arguments": arguments });
        grants.push(json!({ "name": name, "job": {
            "target": target, "body": body,
            "keywords": { "dir": "/work" }, "payload_max_bytes": 8, "call_timeout_seconds": 1,
        } }));
    }
    ir
}

const RUNNER_TOO: &str = "sha256:4444444444444444444444444444444444444444444444444444444444444444";

// A job's body runs its commands and returns, or times out in one or fails after one: the record
// keeps the target's `out` and each command's log either way.
#[test]
#[expect(clippy::too_many_lines, reason = "most of it is its snapshot")]
fn job_calls() {
    let argv = [
        "ops",
        "ops",
        "call check 6 label=first",
        "await 0 completed",
        "call slow 6",
        "await 1 errored",
        "call train 6",
        "await 2 completed",
        "call train large",
        "await 3 errored",
        "call submit 6 finalize=true",
    ];
    let mut ir = calling_workflow(&argv, 30);
    // `check`'s `out` changes until its call's end freezes the target; `slow`'s target fails the
    // freeze, and its call's end collects all the same.
    ir["roles"][3]["image"] = json!(sim::image(SCRIBBLED).as_str());
    ir["roles"][4]["image"] = json!(sim::image(UNFREEZABLE).as_str());
    // The clock reaches the slow call's 1 s timeout once its command has started.
    let timed_out = SlowSync {
        record: Synced::Holding("log.jsonl", r#""job":1,"kind":"target_run""#),
        queued: 0,
        at: Duration::from_secs(1),
    };
    let shown = executed(
        &SimHost::new().slow_sync(timed_out),
        "job-calls",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &[Also::Polls, Also::Listing],
    );
    expect![[r#"
        finished, showing [score=6]
        a poll, {"job":"0","kind":"status"}, is answered {"kind":"status"}
        a poll, {"job":"1","kind":"status"}, is answered {"kind":"status"}
        a poll, {"job":"2","kind":"status"}, is answered {"kind":"status"}
        a poll, {"job":"3","kind":"status"}, is answered {"kind":"status"}
        record: agents/agent/log/collected, agents/agent/out/collected, agents/agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/got/1/command.stdout, jobs/0/log/0/collected, jobs/0/log/0/command.stdout, jobs/0/out/collected, jobs/0/sandbox.json, jobs/1/log/0/collected, jobs/1/out/collected, jobs/1/sandbox.json, jobs/2/got/1/result.json, jobs/2/log/0/collected, jobs/2/out/collected, jobs/2/out/result.json, jobs/2/sandbox.json, jobs/3/got/1/, jobs/3/log/0/collected, jobs/3/out/collected, jobs/3/out/result.json, jobs/3/sandbox.json, jobs/4/got/1/result.json, jobs/4/log/0/collected, jobs/4/out/collected, jobs/4/out/result.json, jobs/4/sandbox.json, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, payloads/d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9, provider.json, record.json
        payloads hold: ["large", "xxxxxx"]
        outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":6}]}}}
        log.jsonl:
        {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
        {"actions":[{"kind":"reply","reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"job","name":"train","payload_max_bytes":8},{"arguments":{"label":{"type":"str"}},"call_timeout_seconds":1,"calls":null,"cumulative_seconds":null,"kind":"job","name":"check","payload_max_bytes":8},{"call_timeout_seconds":1,"calls":null,"cumulative_seconds":null,"kind":"job","name":"slow","payload_max_bytes":8},{"arguments":{"finalize":{"default":false,"type":"bool"}},"kind":"now","name":"submit","payload_max_bytes":16}],"kind":"ops","remaining_seconds":30,"resources":{"cpu_millis":8000,"memory_bytes":34359738368},"role":"agent"}}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"kind":"ops"}},"observed_ns":"0","seq":3}
        {"actions":[{"kind":"reply","reply":{"job":0,"kind":"call","remaining":{"calls":null,"seconds":null}}},{"caller":"agent-0","grant":"check","job":0,"kind":"start_call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":1,"request":{"arguments":{"label":"first"},"grant":"check","kind":"call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":4}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":5}
        {"actions":[],"at_ns":"0","event":{"bytes":6,"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","job":0,"kind":"target_put","path":"/work/payload"},"observed_ns":"0","seq":6}
        {"actions":[],"at_ns":"0","event":{"argv":["list","/work"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"0","seq":7}
        {"actions":[],"at_ns":"0","event":{"command":0,"end":{"exited":{"code":0}},"job":0,"kind":"command_ended"},"observed_ns":"0","seq":8}
        {"actions":[],"at_ns":"0","event":{"bytes":20,"digest":"sha256:7bac131c0d0099b465090b2c412cc6d29e6ccae243db4f48714f03af841d0cb7","job":0,"kind":"target_get","path":"/pumpkin/log/0/command.stdout"},"observed_ns":"0","seq":9}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_returned","returned":[{"name":"printed","value":"0644 payload: xxxxxx"},{"name":"label","value":"first"}]},"observed_ns":"0","seq":10}
        {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":11}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"0","seq":12}
        {"actions":[{"kind":"reply","reply":{"job":1,"kind":"call","remaining":{"calls":null,"seconds":null}}},{"caller":"agent-0","grant":"slow","job":1,"kind":"start_call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":2,"request":{"grant":"slow","kind":"call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":13}
        {"actions":[],"at_ns":"0","event":{"job":1,"kind":"call_started"},"observed_ns":"0","seq":14}
        {"actions":[],"at_ns":"0","event":{"argv":["slow"],"command":0,"job":1,"kind":"target_run"},"observed_ns":"0","seq":15}
        {"actions":[],"at_ns":"1000000000","event":{"job":1,"kind":"call_timed_out"},"observed_ns":"1000000000","seq":16}
        {"at_ns":"1000000000","note":"freezing job 1's target failed: the sandbox's processes couldn't be killed: Operation not permitted (os error 1)","seq":17}
        {"actions":[],"at_ns":"1000000000","event":{"job":1,"kind":"call_failed","reason":"the call's timeout passed"},"observed_ns":"1000000000","seq":18}
        {"actions":[],"at_ns":"1000000000","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-1"},"seq":19}
        {"actions":[],"at_ns":"1000000000","event":{"job":1,"kind":"call_torn_down"},"observed_ns":"1000000000","seq":20}
        {"actions":[{"kind":"reply","reply":{"job":2,"kind":"call","remaining":{"calls":99,"seconds":null}}},{"caller":"agent-0","grant":"train","job":2,"kind":"start_call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"op_arrived","op":3,"request":{"grant":"train","kind":"call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"1000000000","seq":21}
        {"actions":[],"at_ns":"1000000000","event":{"job":2,"kind":"call_started"},"observed_ns":"1000000000","seq":22}
        {"actions":[],"at_ns":"1000000000","event":{"bytes":6,"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","job":2,"kind":"target_put","path":"/work/payload"},"observed_ns":"1000000000","seq":23}
        {"actions":[],"at_ns":"1000000000","event":{"argv":["judge","/work/payload","/work/result.json"],"command":0,"job":2,"kind":"target_run"},"observed_ns":"1000000000","seq":24}
        {"actions":[],"at_ns":"1000000000","event":{"command":0,"end":{"exited":{"code":0}},"job":2,"kind":"command_ended"},"observed_ns":"1000000000","seq":25}
        {"actions":[],"at_ns":"1000000000","event":{"bytes":15,"digest":"sha256:68e3ea1c52db57d6e28c6d873ef07678bc738b836a1ec8ae5f831875083035e9","job":2,"kind":"target_get","path":"/work/result.json"},"observed_ns":"1000000000","seq":26}
        {"actions":[],"at_ns":"1000000000","event":{"job":2,"kind":"call_returned","returned":[{"name":"raw_score","value":6}]},"observed_ns":"1000000000","seq":27}
        {"actions":[],"at_ns":"1000000000","event":{"id":"sim-4","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-2"},"seq":28}
        {"actions":[],"at_ns":"1000000000","event":{"job":2,"kind":"call_torn_down"},"observed_ns":"1000000000","seq":29}
        {"actions":[{"kind":"reply","reply":{"job":3,"kind":"call","remaining":{"calls":98,"seconds":null}}},{"caller":"agent-0","grant":"train","job":3,"kind":"start_call","payload":{"digest":"sha256:d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9","size":5},"timeout_ns":"1000000000"}],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"op_arrived","op":4,"request":{"grant":"train","kind":"call","payload":{"digest":"sha256:d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9","size":5}}},"observed_ns":"1000000000","seq":30}
        {"actions":[],"at_ns":"1000000000","event":{"job":3,"kind":"call_started"},"observed_ns":"1000000000","seq":31}
        {"actions":[],"at_ns":"1000000000","event":{"bytes":5,"digest":"sha256:d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9","job":3,"kind":"target_put","path":"/work/payload"},"observed_ns":"1000000000","seq":32}
        {"actions":[],"at_ns":"1000000000","event":{"argv":["judge","/work/payload","/work/result.json"],"command":0,"job":3,"kind":"target_run"},"observed_ns":"1000000000","seq":33}
        {"actions":[],"at_ns":"1000000000","event":{"command":0,"end":{"exited":{"code":0}},"job":3,"kind":"command_ended"},"observed_ns":"1000000000","seq":34}
        {"actions":[],"at_ns":"1000000000","event":{"job":3,"kind":"call_failed","reason":"getting /work/result.json failed: the tree's files hold more than 1048576 bytes"},"observed_ns":"1000000000","seq":35}
        {"actions":[],"at_ns":"1000000000","event":{"id":"sim-5","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-3"},"seq":36}
        {"actions":[],"at_ns":"1000000000","event":{"job":3,"kind":"call_torn_down"},"observed_ns":"1000000000","seq":37}
        {"actions":[{"kind":"reply","reply":{"kind":"call","remaining_seconds":29,"returned":{"state":"accepted"}}}],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"op_arrived","op":5,"request":{"evaluated":{"effects":["set_submission","end_work"],"returned":[{"name":"state","value":"accepted"}]},"grant":"submit","kind":"now","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"1000000000","seq":38}
        {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"1000000000","seq":39}
        {"at_ns":"1000000000","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":40}
        {"actions":[],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"agent_collected"},"seq":41}
        {"actions":[],"at_ns":"1000000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":42}
        {"actions":[{"job":4,"kind":"start_final_call","selections":[{"role":"agent","selection":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}],"timeout_ns":"120000000000"}],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"1000000000","seq":43}
        {"actions":[],"at_ns":"1000000000","event":{"job":4,"kind":"call_started"},"observed_ns":"1000000000","seq":44}
        {"actions":[],"at_ns":"1000000000","event":{"bytes":6,"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","job":4,"kind":"target_put","path":"/pumpkin/in/payload/raw"},"observed_ns":"1000000000","seq":45}
        {"actions":[],"at_ns":"1000000000","event":{"argv":["judge","/pumpkin/in/payload/raw"],"command":0,"job":4,"kind":"target_run"},"observed_ns":"1000000000","seq":46}
        {"actions":[],"at_ns":"1000000000","event":{"command":0,"end":{"exited":{"code":0}},"job":4,"kind":"command_ended"},"observed_ns":"1000000000","seq":47}
        {"actions":[],"at_ns":"1000000000","event":{"bytes":15,"digest":"sha256:68e3ea1c52db57d6e28c6d873ef07678bc738b836a1ec8ae5f831875083035e9","job":4,"kind":"target_get","path":"/pumpkin/out/result.json"},"observed_ns":"1000000000","seq":48}
        {"actions":[],"at_ns":"1000000000","event":{"job":4,"kind":"call_returned","returned":[{"name":"score","value":6}]},"observed_ns":"1000000000","seq":49}
        {"actions":[],"at_ns":"1000000000","event":{"id":"sim-6","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-4"},"seq":50}
        {"actions":[{"kind":"finish"}],"at_ns":"1000000000","event":{"job":4,"kind":"call_torn_down"},"observed_ns":"1000000000","seq":51}
        its last record came before the agent's teardown was to end
        job 0's command 0's standard output:
        0644 payload: xxxxxx"#]]
    .assert_eq(&shown);
}

/// [`ops_workflow`]'s, with `train`'s body the tests' `def` `body` ([`Bodies`]), and its call
/// timeout `timeout` seconds.
fn body_workflow(argv: &[&str], body: &str, timeout: u64) -> Value {
    let mut ir = ops_workflow(argv, 30);
    let train = &mut ir["roles"][0]["runtime"]["agent"]["grants"][0]["job"];
    train["body"]["def"] = json!(body);
    train["call_timeout_seconds"] = json!(timeout);
    ir
}

// A body's second command starts, and what it printed is got, once the target's startup has
// passed: the call's timeout bounds what a body asks of its target.
#[test]
fn commands_past_the_startup() {
    let argv = [
        "ops",
        "call train 6",
        "await 0 completed",
        "call submit 6 finalize=true",
    ];
    // The first command's sleep moves the clock 2 s on, past the 1 s startup.
    check(
        &SimHost::new(),
        "commands-past-the-startup",
        &[(PROVIDER, &Sim::with_startup(SHORT_STARTUP))],
        &body_workflow(&argv, "again", 4),
        SANDBOXD,
        &expect![[r#"
            finished, showing [score=6]
            payloads hold: ["xxxxxx"]
            outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":6}]}}}
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"213000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"kind":"reply","reply":{"job":0,"kind":"call","remaining":{"calls":99,"seconds":null}}},{"caller":"agent-0","grant":"train","job":0,"kind":"start_call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"4000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":3}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":4}
            {"actions":[],"at_ns":"0","event":{"argv":["ops","await_log target_run","sleep 2"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"0","seq":5}
            {"actions":[],"at_ns":"2000000000","event":{"command":0,"end":{"exited":{"code":0}},"job":0,"kind":"command_ended"},"observed_ns":"2000000000","seq":6}
            {"actions":[],"at_ns":"2000000000","event":{"argv":["print","two"],"command":1,"job":0,"kind":"target_run"},"observed_ns":"2000000000","seq":7}
            {"actions":[],"at_ns":"2000000000","event":{"command":1,"end":{"exited":{"code":0}},"job":0,"kind":"command_ended"},"observed_ns":"2000000000","seq":8}
            {"actions":[],"at_ns":"2000000000","event":{"bytes":3,"digest":"sha256:3fc4ccfe745870e2c0d99f71f30ff0656c8dedd41cc1d7d3d376b0dbe685e2f3","job":0,"kind":"target_get","path":"/pumpkin/log/1/command.stdout"},"observed_ns":"2000000000","seq":9}
            {"actions":[],"at_ns":"2000000000","event":{"job":0,"kind":"call_returned","returned":[{"name":"printed","value":"two"}]},"observed_ns":"2000000000","seq":10}
            {"actions":[],"at_ns":"2000000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":11}
            {"actions":[],"at_ns":"2000000000","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"2000000000","seq":12}
            {"actions":[{"kind":"reply","reply":{"kind":"call","remaining_seconds":28,"returned":{"state":"accepted"}}}],"at_ns":"2000000000","event":{"caller":"agent-0","kind":"op_arrived","op":1,"request":{"evaluated":{"effects":["set_submission","end_work"],"returned":[{"name":"state","value":"accepted"}]},"grant":"submit","kind":"now","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"2000000000","seq":13}
            {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"2000000000","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"2000000000","seq":14}
            {"at_ns":"2000000000","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":15}
            {"actions":[],"at_ns":"2000000000","event":{"caller":"agent-0","kind":"agent_collected"},"seq":16}
            {"actions":[],"at_ns":"2000000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":17}
            {"actions":[{"job":1,"kind":"start_final_call","selections":[{"role":"agent","selection":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}],"timeout_ns":"120000000000"}],"at_ns":"2000000000","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"2000000000","seq":18}
            {"actions":[],"at_ns":"2000000000","event":{"job":1,"kind":"call_started"},"observed_ns":"2000000000","seq":19}
            {"actions":[],"at_ns":"2000000000","event":{"bytes":6,"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","job":1,"kind":"target_put","path":"/pumpkin/in/payload/raw"},"observed_ns":"2000000000","seq":20}
            {"actions":[],"at_ns":"2000000000","event":{"argv":["judge","/pumpkin/in/payload/raw"],"command":0,"job":1,"kind":"target_run"},"observed_ns":"2000000000","seq":21}
            {"actions":[],"at_ns":"2000000000","event":{"command":0,"end":{"exited":{"code":0}},"job":1,"kind":"command_ended"},"observed_ns":"2000000000","seq":22}
            {"actions":[],"at_ns":"2000000000","event":{"bytes":15,"digest":"sha256:68e3ea1c52db57d6e28c6d873ef07678bc738b836a1ec8ae5f831875083035e9","job":1,"kind":"target_get","path":"/pumpkin/out/result.json"},"observed_ns":"2000000000","seq":23}
            {"actions":[],"at_ns":"2000000000","event":{"job":1,"kind":"call_returned","returned":[{"name":"score","value":6}]},"observed_ns":"2000000000","seq":24}
            {"actions":[],"at_ns":"2000000000","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-1"},"seq":25}
            {"actions":[{"kind":"finish"}],"at_ns":"2000000000","event":{"job":1,"kind":"call_torn_down"},"observed_ns":"2000000000","seq":26}
            its last record came before the agent's teardown was to end"#]],
    );
}

// The call's timeout passes while its body waits on its target between two commands.
#[test]
fn times_out_between_commands() {
    let argv = [
        "ops",
        "call train 6",
        "await 0 errored",
        "call submit 6 finalize=true",
    ];
    // The clock passes the 1 s timeout once the body has got the first command's output. Its
    // one ask left, the held get, then times out before it asks or while it waits on the sandbox.
    let timed_out = SlowSync {
        record: Synced::Holding("log.jsonl", r#""job":0,"kind":"target_get""#),
        queued: 0,
        at: Duration::from_secs(1),
    };
    check(
        &SimHost::new().slow_sync(timed_out),
        "times-out-between-commands",
        &[(PROVIDER, &Sim::new())],
        &body_workflow(&argv, "holds", 1),
        SANDBOXD,
        &expect![[r#"
            finished, showing [score=6]
            payloads hold: ["xxxxxx"]
            outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":6}]}}}
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"kind":"reply","reply":{"job":0,"kind":"call","remaining":{"calls":99,"seconds":null}}},{"caller":"agent-0","grant":"train","job":0,"kind":"start_call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":3}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":4}
            {"actions":[],"at_ns":"0","event":{"argv":["print","one"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"0","seq":5}
            {"actions":[],"at_ns":"0","event":{"command":0,"end":{"exited":{"code":0}},"job":0,"kind":"command_ended"},"observed_ns":"0","seq":6}
            {"actions":[],"at_ns":"0","event":{"bytes":3,"digest":"sha256:7692c3ad3540bb803c020b3aee66cd8887123234ea0c6e7143c0add73ff431ed","job":0,"kind":"target_get","path":"/pumpkin/log/0/command.stdout"},"observed_ns":"0","seq":7}
            {"actions":[],"at_ns":"1000000000","event":{"job":0,"kind":"call_timed_out"},"observed_ns":"1000000000","seq":8}
            {"actions":[],"at_ns":"1000000000","event":{"job":0,"kind":"call_failed","reason":"the call's timeout passed"},"observed_ns":"1000000000","seq":9}
            {"actions":[],"at_ns":"1000000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":10}
            {"actions":[],"at_ns":"1000000000","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"1000000000","seq":11}
            {"actions":[{"kind":"reply","reply":{"kind":"call","remaining_seconds":29,"returned":{"state":"accepted"}}}],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"op_arrived","op":1,"request":{"evaluated":{"effects":["set_submission","end_work"],"returned":[{"name":"state","value":"accepted"}]},"grant":"submit","kind":"now","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"1000000000","seq":12}
            {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"1000000000","seq":13}
            {"at_ns":"1000000000","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":14}
            {"actions":[],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"agent_collected"},"seq":15}
            {"actions":[],"at_ns":"1000000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":16}
            {"actions":[{"job":1,"kind":"start_final_call","selections":[{"role":"agent","selection":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}],"timeout_ns":"120000000000"}],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"1000000000","seq":17}
            {"actions":[],"at_ns":"1000000000","event":{"job":1,"kind":"call_started"},"observed_ns":"1000000000","seq":18}
            {"actions":[],"at_ns":"1000000000","event":{"bytes":6,"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","job":1,"kind":"target_put","path":"/pumpkin/in/payload/raw"},"observed_ns":"1000000000","seq":19}
            {"actions":[],"at_ns":"1000000000","event":{"argv":["judge","/pumpkin/in/payload/raw"],"command":0,"job":1,"kind":"target_run"},"observed_ns":"1000000000","seq":20}
            {"actions":[],"at_ns":"1000000000","event":{"command":0,"end":{"exited":{"code":0}},"job":1,"kind":"command_ended"},"observed_ns":"1000000000","seq":21}
            {"actions":[],"at_ns":"1000000000","event":{"bytes":15,"digest":"sha256:68e3ea1c52db57d6e28c6d873ef07678bc738b836a1ec8ae5f831875083035e9","job":1,"kind":"target_get","path":"/pumpkin/out/result.json"},"observed_ns":"1000000000","seq":22}
            {"actions":[],"at_ns":"1000000000","event":{"job":1,"kind":"call_returned","returned":[{"name":"score","value":6}]},"observed_ns":"1000000000","seq":23}
            {"actions":[],"at_ns":"1000000000","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-1"},"seq":24}
            {"actions":[{"kind":"finish"}],"at_ns":"1000000000","event":{"job":1,"kind":"call_torn_down"},"observed_ns":"1000000000","seq":25}
            its last record came before the agent's teardown was to end
            job 0's command 0's standard output:
            one"#]],
    );
}

// The final call's timeout passes as it collects a command's output, which ends its session. The
// end reaches the call before the body's next ask, which still fails as timed out.
#[test]
fn times_out_once_its_session_has_ended() {
    let mut ir = short_workflow(&["ops", "call submit 6 finalize=true"], 30);
    ir["final"]["body"]["def"] = json!("routed");
    // The command stalls its sandbox once it exits, which moves the clock past the 1 s timeout as
    // it takes the collect.
    ir["final"]["keywords"] = json!({ "argv": ["ops", "exit", "stall 2"] });
    check(
        &SimHost::new().awaiting(Duration::from_secs(1)),
        "times-out-once-its-session-has-ended",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished without a result
            payloads hold: ["xxxxxx"]
            outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}},"work_end":{"kind":"finalized"}}],"verdict":{"failure":{"kind":"timeout"},"kind":"no_result"}}
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"kind":"reply","reply":{"kind":"call","remaining_seconds":0,"returned":{"state":"accepted"}}},{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"evaluated":{"effects":["set_submission","end_work"],"returned":[{"name":"state","value":"accepted"}]},"grant":"submit","kind":"now","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":3}
            {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":4}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":5}
            {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":6}
            {"actions":[{"job":0,"kind":"start_final_call","selections":[{"role":"agent","selection":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}],"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":7}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":8}
            {"actions":[],"at_ns":"0","event":{"argv":["ops","exit","stall 2"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"0","seq":9}
            {"actions":[],"at_ns":"0","event":{"command":0,"end":{"exited":{"code":0}},"job":0,"kind":"command_ended"},"observed_ns":"0","seq":10}
            {"at_ns":"2000000000","note":"job 0's log/0 is incomplete: collecting /pumpkin/log/0 failed: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts","seq":11}
            {"actions":[],"at_ns":"2000000000","event":{"job":0,"kind":"call_timed_out"},"observed_ns":"2000000000","seq":12}
            {"actions":[],"at_ns":"2000000000","event":{"job":0,"kind":"call_failed","reason":"the call's timeout passed"},"observed_ns":"2000000000","seq":13}
            {"actions":[],"at_ns":"2000000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":14}
            {"actions":[{"kind":"finish"}],"at_ns":"2000000000","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"2000000000","seq":15}
            its last record came before the agent's teardown was to end"#]],
    );
}

// The sandbox ends the session before the final call's timeout, as the call waits for its
// command: the call fails with the session's end.
#[test]
fn fails_as_its_sandbox_ends_the_session() {
    let mut ir = short_workflow(&["ops", "call submit 6 finalize=true"], 30);
    ir["final"]["body"]["def"] = json!("routed");
    let argv = ["ops", "await_log target_run", "close"];
    ir["final"]["keywords"] = json!({ "argv": argv });
    check(
        &SimHost::new(),
        "fails-as-its-sandbox-ends-the-session",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished without a result
            payloads hold: ["xxxxxx"]
            outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}},"work_end":{"kind":"finalized"}}],"verdict":{"failure":{"kind":"error","reason":"the target's session ended: the sandbox ended the session"},"kind":"no_result"}}
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"kind":"reply","reply":{"kind":"call","remaining_seconds":0,"returned":{"state":"accepted"}}},{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"evaluated":{"effects":["set_submission","end_work"],"returned":[{"name":"state","value":"accepted"}]},"grant":"submit","kind":"now","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":3}
            {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":4}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":5}
            {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":6}
            {"actions":[{"job":0,"kind":"start_final_call","selections":[{"role":"agent","selection":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}],"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":7}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":8}
            {"actions":[],"at_ns":"0","event":{"argv":["ops","await_log target_run","close"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"0","seq":9}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_failed","reason":"the target's session ended: the sandbox ended the session"},"observed_ns":"0","seq":10}
            {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":11}
            {"actions":[{"kind":"finish"}],"at_ns":"0","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"0","seq":12}
            its last record came before the agent's teardown was to end"#]],
    );
}

// The sandbox ends the session before the final call's timeout, and the call takes the end only
// once the clock has passed its timeout. What came first, the session's end, fails the call.
#[test]
fn fails_as_its_session_ends_though_it_hears_past_its_timeout() {
    let mut ir = short_workflow(&["ops", "call submit 6 finalize=true"], 30);
    ir["final"]["body"]["def"] = json!("routed");
    let argv = ["ops", "await_log target_run", "close"];
    ir["final"]["keywords"] = json!({ "argv": argv });
    let started = Synced::Holding("log.jsonl", r#""kind":"start_final_call""#);
    check(
        &SimHost::new().late_input(started, Duration::from_secs(2)),
        "fails-as-its-session-ends-though-it-hears-past-its-timeout",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished without a result
            payloads hold: ["xxxxxx"]
            outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}},"work_end":{"kind":"finalized"}}],"verdict":{"failure":{"kind":"error","reason":"the target's session ended: the sandbox ended the session"},"kind":"no_result"}}
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"kind":"reply","reply":{"kind":"call","remaining_seconds":0,"returned":{"state":"accepted"}}},{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"evaluated":{"effects":["set_submission","end_work"],"returned":[{"name":"state","value":"accepted"}]},"grant":"submit","kind":"now","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":3}
            {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":4}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":5}
            {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":6}
            {"actions":[{"job":0,"kind":"start_final_call","selections":[{"role":"agent","selection":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}],"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":7}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":8}
            {"actions":[],"at_ns":"0","event":{"argv":["ops","await_log target_run","close"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"0","seq":9}
            {"actions":[],"at_ns":"2000000000","event":{"job":0,"kind":"call_failed","reason":"the target's session ended: the sandbox ended the session"},"observed_ns":"2000000000","seq":10}
            {"actions":[],"at_ns":"2000000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":11}
            {"actions":[{"kind":"finish"}],"at_ns":"2000000000","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"2000000000","seq":12}
            its last record came before the agent's teardown was to end"#]],
    );
}

// The sandbox ends the session as the clock reaches the final call's timeout, and the end reaches
// the call. An end at the timeout times the call out.
#[test]
fn times_out_as_its_session_ends_at_its_timeout() {
    let mut ir = short_workflow(&["ops", "call submit 6 finalize=true"], 30);
    ir["final"]["body"]["def"] = json!("routed");
    let argv = ["ops", "await_log target_run", "sleep 1", "close"];
    ir["final"]["keywords"] = json!({ "argv": argv });
    check(
        &SimHost::new().awaiting(Duration::from_secs(1)),
        "times-out-as-its-session-ends-at-its-timeout",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished without a result
            payloads hold: ["xxxxxx"]
            outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}},"work_end":{"kind":"finalized"}}],"verdict":{"failure":{"kind":"timeout"},"kind":"no_result"}}
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"121000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"kind":"reply","reply":{"kind":"call","remaining_seconds":0,"returned":{"state":"accepted"}}},{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"evaluated":{"effects":["set_submission","end_work"],"returned":[{"name":"state","value":"accepted"}]},"grant":"submit","kind":"now","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":3}
            {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":4}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":5}
            {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":6}
            {"actions":[{"job":0,"kind":"start_final_call","selections":[{"role":"agent","selection":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}],"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":7}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":8}
            {"actions":[],"at_ns":"0","event":{"argv":["ops","await_log target_run","sleep 1","close"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"0","seq":9}
            {"actions":[],"at_ns":"1000000000","event":{"job":0,"kind":"call_timed_out"},"observed_ns":"1000000000","seq":10}
            {"actions":[],"at_ns":"1000000000","event":{"job":0,"kind":"call_failed","reason":"the call's timeout passed"},"observed_ns":"1000000000","seq":11}
            {"actions":[],"at_ns":"1000000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":12}
            {"actions":[{"kind":"finish"}],"at_ns":"1000000000","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"1000000000","seq":13}
            its last record came before the agent's teardown was to end"#]],
    );
}

// The final call's target crashes as its body waits on a lease op, before the call's timeout.
// The call takes the end only past its timeout, and fails with it.
#[test]
fn lease_op_fails_as_its_session_ends_though_it_hears_past_its_timeout() {
    let mut ir = scoped_runner(&["exit", "0"]);
    ir["roles"][0]["runtime"]["agent"]["command"]["argv"] =
        json!(["ops", "call submit 6 finalize=true"]);
    ir["final"]["timeout_seconds"] = json!(1);
    // A runner that never dials, so that the lease op waits until the target's end.
    ir["roles"][3]["image"] = json!(sim::image(HUNG).as_str());
    let sim = Sim::with_startup(1);
    let crash = sim.crashing("sim-2");
    let sim = sim.once_logged(&[r#""kind":"lease_open""#], crash);
    let started = Synced::Holding("log.jsonl", r#""kind":"start_final_call""#);
    // The lease's start gives up at the call's timeout as the call's teardown runs, in either
    // order.
    let shown = executed(
        &SimHost::new().late_input(started, Duration::from_secs(2)),
        "lease-op-fails-as-its-session-ends-though-it-hears-past-its-timeout",
        &[(PROVIDER, &sim)],
        &ir,
        SANDBOXD,
        &[],
    );
    let failed = r#"{"job":0,"kind":"call_failed","reason":"the target's session ended: the sandbox ended the session"},"observed_ns":"2000000000""#;
    assert!(shown.contains(failed), "{shown}");
    assert!(!shown.contains("call_timed_out"), "{shown}");
}

// A shared target's session ends before the call's timeout, and the loop forwards the end once
// the clock has passed it. The call fails with the session's end.
#[test]
fn shared_call_fails_as_its_session_ends_though_it_hears_past_its_timeout() {
    let agent = [
        "ops",
        "call train 1",
        "await_log target_run",
        "ops",
        "await 0 errored",
        "call submit ok finalize=true",
    ];
    let mut ir = work_target_workflow(&agent, "run");
    // The command closes the session as the loop holds the agent's ops query.
    let argv = ["ops", "await_log ops", "close"];
    ir["roles"][0]["runtime"]["agent"]["grants"][0]["job"]["keywords"] = json!({ "argv": argv });
    let held = SlowSync {
        record: Synced::Holding("log.jsonl", r#""request":{"kind":"ops"}"#),
        queued: 1,
        at: Duration::from_secs(2),
    };
    let host = SimHost::new()
        .slow_sync(held)
        .awaiting(Duration::from_secs(1));
    let dir = TestDir::new();
    let (record, _, _) = judged(&host, ir, &dir);
    let log = read_log(&record);
    let event = |kind: &str| {
        let line = log.iter().find(|line| line["event"]["kind"] == kind);
        line.map(|line| line["event"].clone())
    };
    let shared_failed = log
        .iter()
        .find(|line| line["event"]["kind"] == "shared_failed");
    assert_eq!(shared_failed.unwrap()["observed_ns"], "0");
    assert_eq!(event("call_timed_out"), None);
    assert_eq!(
        event("call_failed").unwrap()["reason"],
        "the shared role's session ended: the sandbox ended the session"
    );
}

// The core stops a call between two of its body's commands: the second never starts.
#[test]
fn stopped_between_commands() {
    let argv = ["ops", "call train 6", "await_log command_ended", "exit"];
    check(
        &SimHost::new(),
        "stopped-between-commands",
        &[(PROVIDER, &Sim::new())],
        &body_workflow(&argv, "waits", 20),
        SANDBOXD,
        &expect![[r#"
            finished, showing [score=0]
            payloads hold: ["xxxxxx"]
            outcome.json: {"agents":[{"role":"agent","submission":null,"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":0}]}}}
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"kind":"reply","reply":{"job":0,"kind":"call","remaining":{"calls":99,"seconds":null}}},{"caller":"agent-0","grant":"train","job":0,"kind":"start_call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"20000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":3}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":4}
            {"actions":[],"at_ns":"0","event":{"argv":["print","one"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"0","seq":5}
            {"actions":[],"at_ns":"0","event":{"command":0,"end":{"exited":{"code":0}},"job":0,"kind":"command_ended"},"observed_ns":"0","seq":6}
            {"actions":[{"job":0,"kind":"stop_call"},{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":7}
            {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":8}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":9}
            {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":10}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":11}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_failed","reason":"the call was stopped"},"observed_ns":"0","seq":12}
            {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":13}
            {"actions":[{"job":1,"kind":"start_final_call","selections":[{"role":"agent","selection":null}],"timeout_ns":"120000000000"}],"at_ns":"0","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"0","seq":14}
            {"actions":[],"at_ns":"0","event":{"job":1,"kind":"call_started"},"observed_ns":"0","seq":15}
            {"actions":[],"at_ns":"0","event":{"job":1,"kind":"call_returned","returned":[{"name":"score","value":0}]},"observed_ns":"0","seq":16}
            {"actions":[],"at_ns":"0","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-1"},"seq":17}
            {"actions":[{"kind":"finish"}],"at_ns":"0","event":{"job":1,"kind":"call_torn_down"},"observed_ns":"0","seq":18}
            its last record came before the agent's teardown was to end
            job 0's command 0's standard output:
            one"#]],
    );
}

// A body that fails ends its call with the reason it gave.
#[test]
fn body_fails() {
    let argv = [
        "ops",
        "call train 6",
        "await 0 errored",
        "call submit 6 finalize=true",
    ];
    check(
        &SimHost::new(),
        "body-fails",
        &[(PROVIDER, &Sim::new())],
        &body_workflow(&argv, "missing", 1),
        SANDBOXD,
        &expect![[r#"
            finished, showing [score=6]
            payloads hold: ["xxxxxx"]
            outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":6}]}}}
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"kind":"reply","reply":{"job":0,"kind":"call","remaining":{"calls":99,"seconds":null}}},{"caller":"agent-0","grant":"train","job":0,"kind":"start_call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":3}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":4}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_failed","reason":"getting /work/missing failed: No such file or directory (os error 2)"},"observed_ns":"0","seq":5}
            {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":6}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"0","seq":7}
            {"actions":[{"kind":"reply","reply":{"kind":"call","remaining_seconds":30,"returned":{"state":"accepted"}}}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":1,"request":{"evaluated":{"effects":["set_submission","end_work"],"returned":[{"name":"state","value":"accepted"}]},"grant":"submit","kind":"now","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":8}
            {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":9}
            {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":10}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":11}
            {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":12}
            {"actions":[{"job":1,"kind":"start_final_call","selections":[{"role":"agent","selection":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}],"timeout_ns":"120000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":13}
            {"actions":[],"at_ns":"0","event":{"job":1,"kind":"call_started"},"observed_ns":"0","seq":14}
            {"actions":[],"at_ns":"0","event":{"bytes":6,"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","job":1,"kind":"target_put","path":"/pumpkin/in/payload/raw"},"observed_ns":"0","seq":15}
            {"actions":[],"at_ns":"0","event":{"argv":["judge","/pumpkin/in/payload/raw"],"command":0,"job":1,"kind":"target_run"},"observed_ns":"0","seq":16}
            {"actions":[],"at_ns":"0","event":{"command":0,"end":{"exited":{"code":0}},"job":1,"kind":"command_ended"},"observed_ns":"0","seq":17}
            {"actions":[],"at_ns":"0","event":{"bytes":15,"digest":"sha256:68e3ea1c52db57d6e28c6d873ef07678bc738b836a1ec8ae5f831875083035e9","job":1,"kind":"target_get","path":"/pumpkin/out/result.json"},"observed_ns":"0","seq":18}
            {"actions":[],"at_ns":"0","event":{"job":1,"kind":"call_returned","returned":[{"name":"score","value":6}]},"observed_ns":"0","seq":19}
            {"actions":[],"at_ns":"0","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-1"},"seq":20}
            {"actions":[{"kind":"finish"}],"at_ns":"0","event":{"job":1,"kind":"call_torn_down"},"observed_ns":"0","seq":21}
            its last record came before the agent's teardown was to end"#]],
    );
}

// A body's return nested as deep as a reader of the record reads back is logged; one nested
// deeper, or one larger than the record holds, a job's or the final call's, fails its call.
#[test]
#[expect(clippy::too_many_lines, reason = "most of it is its snapshot")]
fn deep_returns() {
    let argv = [
        "ops",
        "call deep 6",
        "await 0 completed",
        "call deeper 6",
        "await 1 errored",
        "call large 6",
        "await 2 errored",
        "call submit 6 finalize=true",
    ];
    let mut ir = calling_workflow(&argv, 30);
    let mut larger = ir["roles"][1].clone();
    larger["name"] = json!("larger");
    larger["runtime"] = json!({ "target": { "cwd": "/work" } });
    ir["roles"].as_array_mut().unwrap().push(larger);
    ir["final"]["body"]["def"] = json!("large");
    let grants = ir["roles"][0]["runtime"]["agent"]["grants"]
        .as_array_mut()
        .unwrap();
    let body = json!({ "file": "//check.star", "def": "large" });
    grants.push(json!({ "name": "large", "job": {
        "target": "larger", "body": body, "payload_max_bytes": 8, "call_timeout_seconds": 1,
    } }));
    let calls = [("check", "deep", 0), ("slow", "deeper", 1)];
    for grant in grants {
        let Some(&(_, name, deeper)) = calls.iter().find(|(was, ..)| grant["name"] == *was) else {
            continue;
        };
        grant["name"] = json!(name);
        let job = &mut grant["job"];
        job["body"] = json!({ "file": "//check.star", "def": "deep" });
        job["keywords"] = json!({ "depth": RESULT_MAX_DEPTH + deeper });
    }
    check(
        &SimHost::new(),
        "deep-returns",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished without a result
            payloads hold: ["xxxxxx"]
            outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"failure":{"kind":"error","reason":"the body's return takes 1048588 bytes as JSON, over the limit of 1048576: return at most that"},"kind":"no_result"}}
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"kind":"reply","reply":{"job":0,"kind":"call","remaining":{"calls":null,"seconds":null}}},{"caller":"agent-0","grant":"deep","job":0,"kind":"start_call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"deep","kind":"call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":3}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":4}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_returned","returned":[{"name":"deep","value":[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]}]},"observed_ns":"0","seq":5}
            {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":6}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"0","seq":7}
            {"actions":[{"kind":"reply","reply":{"job":1,"kind":"call","remaining":{"calls":null,"seconds":null}}},{"caller":"agent-0","grant":"deeper","job":1,"kind":"start_call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":1,"request":{"grant":"deeper","kind":"call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":8}
            {"actions":[],"at_ns":"0","event":{"job":1,"kind":"call_started"},"observed_ns":"0","seq":9}
            {"actions":[],"at_ns":"0","event":{"job":1,"kind":"call_failed","reason":"the body's return nests lists, tuples and dicts more than 120 deep, its dict included: nest them at most that deep"},"observed_ns":"0","seq":10}
            {"actions":[],"at_ns":"0","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-1"},"seq":11}
            {"actions":[],"at_ns":"0","event":{"job":1,"kind":"call_torn_down"},"observed_ns":"0","seq":12}
            {"actions":[{"kind":"reply","reply":{"job":2,"kind":"call","remaining":{"calls":null,"seconds":null}}},{"caller":"agent-0","grant":"large","job":2,"kind":"start_call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":2,"request":{"grant":"large","kind":"call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":13}
            {"actions":[],"at_ns":"0","event":{"job":2,"kind":"call_started"},"observed_ns":"0","seq":14}
            {"actions":[],"at_ns":"0","event":{"job":2,"kind":"call_failed","reason":"the body's return takes 1048588 bytes as JSON, over the limit of 1048576: return at most that"},"observed_ns":"0","seq":15}
            {"actions":[],"at_ns":"0","event":{"id":"sim-4","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-2"},"seq":16}
            {"actions":[],"at_ns":"0","event":{"job":2,"kind":"call_torn_down"},"observed_ns":"0","seq":17}
            {"actions":[{"kind":"reply","reply":{"kind":"call","remaining_seconds":30,"returned":{"state":"accepted"}}}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":3,"request":{"evaluated":{"effects":["set_submission","end_work"],"returned":[{"name":"state","value":"accepted"}]},"grant":"submit","kind":"now","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":18}
            {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":19}
            {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":20}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":21}
            {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":22}
            {"actions":[{"job":3,"kind":"start_final_call","selections":[{"role":"agent","selection":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}],"timeout_ns":"120000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":23}
            {"actions":[],"at_ns":"0","event":{"job":3,"kind":"call_started"},"observed_ns":"0","seq":24}
            {"actions":[],"at_ns":"0","event":{"job":3,"kind":"call_failed","reason":"the body's return takes 1048588 bytes as JSON, over the limit of 1048576: return at most that"},"observed_ns":"0","seq":25}
            {"actions":[],"at_ns":"0","event":{"id":"sim-5","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-3"},"seq":26}
            {"actions":[{"kind":"finish"}],"at_ns":"0","event":{"job":3,"kind":"call_torn_down"},"observed_ns":"0","seq":27}
            its last record came before the agent's teardown was to end"#]],
    );
}

// The agent and a call's target end by the agents' teardown's end, the final call's by the hard
// end. A runner ends a startup after its life from its latest ready, by its holder's end at most.
#[test]
fn hard_ends() {
    let argv = [
        "ops",
        // Each lease opens once the clock has moved.
        "sleep 7",
        "gpu.open 10",
        "await_lease gpu 0 ready",
        "gpu.open 200",
        "await_lease gpu 1 ready",
        "gpu.close 0",
        "gpu.close 1",
        "await_lease gpu 0 closed",
        "await_lease gpu 1 closed",
        "call train 6",
        "await 0 completed",
        "call submit 6 finalize=true",
    ];
    let holder_argv = ["ops", "box.open", "await_lease box 2 ready"];
    let mut ir = held_lease_workflow(&argv, &holder_argv);
    // The call's runner may take 30 s to start, past the call's 20 s timeout, which cuts it short.
    // The others take 1 s, apart from the other roles' 30 s, and each lease's life differs.
    for (name, image) in [("gpu_runner", RUNNER), ("final_box", RUNNER_TOO)] {
        let mut runner = ir["roles"][3].clone();
        runner["name"] = json!(name);
        runner["image"] = json!(image);
        runner["needs"]["cpu_model"] = json!(BRIEF);
        ir["roles"].as_array_mut().unwrap().push(runner);
    }
    let gpu = json!({ "name": "gpu", "session": {
        "runner": "gpu_runner", "instances": 2, "instance_seconds": 600, "transfer_bytes": 64,
    } });
    let grants = ir["roles"][0]["runtime"]["agent"]["grants"].as_array_mut();
    grants.unwrap().push(gpu);
    ir["roles"][2]["runtime"]["target"]["sessions"] = json!([{ "name": "box", "session": {
        "runner": "final_box", "instances": 1, "instance_seconds": 45, "commands": 3,
        "transfer_bytes": 64,
    } }]);
    ir["final"]["body"]["def"] = json!("scoped");
    ir["final"]["keywords"] = json!({ "argv": ["worker", "0"] });
    let also = [Also::HardEnds];
    let shown = executed(
        &SimHost::new(),
        "hard-ends",
        &[(PROVIDER, &Sim::new())],
        &ir,
        SANDBOXD,
        &also,
    );
    let hard_ends: Vec<&str> = (shown.lines())
        .filter(|line| line.contains("'s hard end: "))
        .collect();
    expect![[r#"
        agent-0's hard end: 150 s after the run's start
        lease-0's hard end: 19 s after the run's start
        lease-1's hard end: 150 s after the run's start
        job-0's hard end: 150 s after the run's start
        lease-2's hard end: 117 s after the run's start
        job-1's hard end: 300 s after the run's start
        lease-3's hard end: 54 s after the run's start"#]]
    .assert_eq(&hard_ends.join("\n"));
}

// The log keeps the container that a lease's command or copy names, though the core refuses each
// here, as the runner holds no companions.
#[test]
fn logged_containers() {
    let argv = [
        "ops",
        "gpu.open",
        "await_lease gpu 0 ready",
        "gpu.run 0:scheduler - print one",
        "gpu.put 0:scheduler /workspace /work/in",
        "gpu.get 0:scheduler /work/out /pumpkin/out/got",
        "gpu.close 0",
        "await_lease gpu 0 closed",
        "call submit 6 finalize=true",
    ];
    let shown = executed(
        &SimHost::new(),
        "logged-containers",
        &[(PROVIDER, &Sim::new())],
        &lease_workflow(&argv, 30, RUNNER),
        SANDBOXD,
        &[],
    );
    let named: Vec<&str> = (shown.lines())
        .filter(|line| line.contains("\"container\""))
        .collect();
    expect![[r#"
        {"actions":[{"kind":"reply","reply":{"kind":"refused","message":"lease 0 holds no container `scheduler`: name `main`, the one container that it holds"}}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":1,"request":{"argv":["print","one"],"container":"scheduler","grant":"gpu","kind":"lease_run","lease":0,"timeout_seconds":null}},"observed_ns":"0","seq":5}
        {"actions":[{"kind":"reply","reply":{"kind":"refused","message":"lease 0 holds no container `scheduler`: name `main`, the one container that it holds"}}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":2,"request":{"container":"scheduler","grant":"gpu","kind":"lease_put","lease":0,"replace":false,"to":"/work/in","tree":"/workspace"}},"observed_ns":"0","seq":6}
        {"actions":[{"kind":"reply","reply":{"kind":"refused","message":"lease 0 holds no container `scheduler`: name `main`, the one container that it holds"}}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":3,"request":{"container":"scheduler","from":"/work/out","grant":"gpu","kind":"lease_get","lease":0,"to":"/pumpkin/out/got"}},"observed_ns":"0","seq":7}"#]].assert_eq(&named.join("\n"));
}

// A runner whose session ends as its lease starts fails the lease, as one whose session ends
// later does. The lease's first look at its inbox, past the agents' start, waits for that end.
#[test]
fn fails_as_its_session_ended_before_it_was_ready() {
    let argv = [
        "ops",
        "sleep 1",
        "gpu.open",
        "await_lease gpu 0 failed",
        "call submit 6 finalize=true",
    ];
    let shown = executed(
        &SimHost::new().awaiting(Duration::from_secs(1)),
        "fails-as-its-session-ended-before-it-was-ready",
        &[(PROVIDER, &Sim::new().ending_once_set_up("lease-0", "main"))],
        &lease_workflow(&argv, 30, RUNNER),
        SANDBOXD,
        &[],
    );
    expect![[r#"
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":4,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"grant":"gpu","holder":"agent-0","instance":0,"kind":"start_lease","lease":0,"start_by_ns":"30000000000"}],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"gpu","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"1000000000","seq":3}
        {"actions":[],"at_ns":"1000000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"lease-0"},"seq":4}
        {"actions":[],"at_ns":"1000000000","event":{"kind":"lease_failed","lease":0,"reason":"the runner's session ended: the sandbox ended the session"},"observed_ns":"1000000000","seq":5}"#]].assert_eq(&leased(&shown));
}

/// [`lease_workflow`]'s, whose runner declares the companions `scheduler` and `swe-0`, and whose
/// lease allows 8 commands, and 256 bytes a transfer.
fn companions_workflow(argv: &[&str]) -> Value {
    let mut ir = lease_workflow(argv, 30, RUNNER);
    let lease = &mut ir["roles"][0]["runtime"]["agent"]["grants"][1]["session"];
    lease["commands"] = json!(8);
    lease["transfer_bytes"] = json!(256);
    let companion = |name: &str, image: &str, id: u32, cwd: &str, env: &str| {
        json!({
            "name": name, "image": image, "user": { "uid": id, "gid": id },
            "cwd": cwd, "env": [env],
        })
    };
    let runner = ir["roles"].as_array_mut().unwrap().last_mut().unwrap();
    runner["runtime"]["runner"]["companions"] = json!([
        companion("scheduler", SCHEDULER, 2001, "/srv", "ROLE=scheduler"),
        companion("swe-0", SWE, 2002, "/testbed", "INSTANCE=0"),
    ]);
    ir
}

const SCHEDULER: &str = "sha256:5555555555555555555555555555555555555555555555555555555555555555";
const SWE: &str = "sha256:6666666666666666666666666666666666666666666666666666666666666666";

/// A simulated provider whose sandboxes listen, and hold companions.
fn accompanied() -> Sim {
    Sim::new().listening().offering_companions()
}

/// `shown`, with the port of each endpoint on this host hidden, and TLS's own words for a
/// certificate that it refuses.
fn unported(shown: &str) -> String {
    let mut hidden = String::new();
    let mut rest = shown;
    while let Some(at) = rest.find("localhost:") {
        let (before, after) = rest.split_at(at + "localhost:".len());
        hidden.push_str(before);
        hidden.push_str("$PORT");
        rest = after.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    hidden.push_str(rest);
    let failed = "the TLS handshake failed: ";
    match (hidden.find(failed), hidden.find(": expected TLS 1.3")) {
        (Some(from), Some(to)) => {
            let from = from + failed.len();
            format!("{}<why>{}", &hidden[..from], &hidden[to..])
        }
        _ => hidden,
    }
}

/// What `shown` says of the run's leases: each line that names one, down to the log's last
/// record, and all that follows it.
fn leased(shown: &str) -> String {
    let after = shown.find("\nits last record came").unwrap_or(shown.len());
    let (logged, files) = shown.split_at(after);
    let leased = logged.lines().filter(|line| line.contains("lease"));
    let after = |line: &&str| !line.is_empty() && !line.starts_with("its last record came");
    let files = files.lines().filter(after);
    leased.chain(files).collect::<Vec<_>>().join("\n")
}

// Each container of a runner's sandbox meets the gateway while the others do. Here each says its
// hello once the one before it has met, in another order than the declared one.
#[test]
fn meets_every_container_at_once() {
    let argv = [
        "ops",
        "gpu.open",
        "await_lease gpu 0 ready",
        "gpu.close 0",
        "await_lease gpu 0 closed",
        "call submit 6 finalize=true",
    ];
    let turns = &[("swe-0", 1), ("main", 2), ("scheduler", 3)];
    let sim = accompanied().meeting_in_turn(turns);
    let shown = executed(
        &SimHost::new(),
        "meets-every-container-at-once",
        &[(PROVIDER, &sim)],
        &companions_workflow(&argv),
        SANDBOXD,
        &[Also::Met("lease-0")],
    );
    let met = shown
        .lines()
        .filter(|line| line.contains("_met\"") || line.contains("meetings"));
    let held = sim.companions().into_iter();
    expect![[r#"
        sim-2 holds `scheduler`, of image sha256:5555555555555555555555555555555555555555555555555555555555555555, as 2001:2001, in /srv, with ["ROLE=scheduler"]
        sim-2 holds `swe-0`, of image sha256:6666666666666666666666666666666666666666666666666666666666666666, as 2002:2002, in /testbed, with ["INSTANCE=0"]
        the meetings of lease-0's containers:
          {"container":"swe-0","kind":"container_met","sandbox":"lease-0"}, observed at 1s
          {"container":"main","kind":"container_met","sandbox":"lease-0"}, observed at 2s
          {"container":"scheduler","kind":"container_met","sandbox":"lease-0"}, observed at 3s
          {"kind":"sandbox_met","sandbox":"lease-0"}, observed at 3s"#]].assert_eq(&held.chain(met.map(str::to_owned)).collect::<Vec<_>>().join("\n"));
}

// A command or a copy reaches the container that it names, and no other. A command starts in its
// container's directory, with its container's environment.
#[test]
fn serves_each_container() {
    let argv = [
        "ops",
        "gpu.open",
        "await_lease gpu 0 ready",
        "gpu.put 0:scheduler /workspace/.codex /work/in",
        "await_transfer gpu 0 moved",
        "gpu.run 0:scheduler - list /work/in",
        "await_lease gpu 0 ready",
        "gpu.run 0 - list /work/in",
        "await_lease gpu 0 ready",
        "gpu.run 0:swe-0 - where",
        "await_lease gpu 0 ready",
        "gpu.run 0 - where",
        "await_lease gpu 0 ready",
        "gpu.get 0:scheduler /work/in /pumpkin/out/scheduler",
        "await_transfer gpu 0 moved",
        "gpu.get 0:swe-0 /work/in /pumpkin/out/swe-0",
        "await_transfer gpu 0 moved",
        "gpu.close 0",
        "await_lease gpu 0 closed",
        "call submit 6 finalize=true",
    ];
    let outputs = [
        "leases/0/log/1/command.stdout",
        "leases/0/log/2/command.stdout",
        "leases/0/log/3/command.stdout",
        "leases/0/log/4/command.stdout",
    ];
    let also = iter::once(Also::Listing).chain(outputs.map(Also::File));
    let shown = executed(
        &SimHost::new(),
        "serves-each-container",
        &[(PROVIDER, &accompanied())],
        &companions_workflow(&argv),
        SANDBOXD,
        &also.collect::<Vec<_>>(),
    );
    expect![[r#"
        record: agents/agent/log/collected, agents/agent/out/collected, agents/agent/out/scheduler/collected, agents/agent/out/scheduler/config.toml, agents/agent/out/swe-0/collected, agents/agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/got/1/result.json, jobs/0/log/0/collected, jobs/0/out/collected, jobs/0/out/result.json, jobs/0/sandbox.json, leases/0/log/1/collected, leases/0/log/1/command.stdout, leases/0/log/2/collected, leases/0/log/2/command.stdout, leases/0/log/3/collected, leases/0/log/3/command.stdout, leases/0/log/4/collected, leases/0/log/4/command.stdout, leases/0/out/collected, leases/0/sandbox.json, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":8,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"grant":"gpu","holder":"agent-0","instance":0,"kind":"start_lease","lease":0,"start_by_ns":"30000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"gpu","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"0","seq":3}
        {"actions":[],"at_ns":"0","event":{"kind":"lease_ready","lease":0},"observed_ns":"0","seq":4}
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":7,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"direction":"put","kind":"transfer","lease":0,"max_bytes":256,"transfer":0}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":1,"request":{"container":"scheduler","grant":"gpu","kind":"lease_put","lease":0,"replace":false,"to":"/work/in","tree":"/workspace/.codex"}},"observed_ns":"0","seq":5}
        {"actions":[],"at_ns":"0","event":{"end":{"moved":{"bytes":31,"entries":2}},"kind":"transfer_ended","lease":0,"transfer":0},"observed_ns":"0","seq":6}
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":6,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"command":1,"kind":"run_command","lease":0,"timeout_ns":null}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":2,"request":{"argv":["list","/work/in"],"container":"scheduler","grant":"gpu","kind":"lease_run","lease":0,"timeout_seconds":null}},"observed_ns":"0","seq":7}
        {"actions":[],"at_ns":"0","event":{"command":1,"end":{"exited":{"code":0}},"kind":"command_ended","lease":0},"observed_ns":"0","seq":8}
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":5,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"command":2,"kind":"run_command","lease":0,"timeout_ns":null}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":3,"request":{"argv":["list","/work/in"],"grant":"gpu","kind":"lease_run","lease":0,"timeout_seconds":null}},"observed_ns":"0","seq":9}
        {"actions":[],"at_ns":"0","event":{"command":2,"end":{"exited":{"code":0}},"kind":"command_ended","lease":0},"observed_ns":"0","seq":10}
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":4,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"command":3,"kind":"run_command","lease":0,"timeout_ns":null}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":4,"request":{"argv":["where"],"container":"swe-0","grant":"gpu","kind":"lease_run","lease":0,"timeout_seconds":null}},"observed_ns":"0","seq":11}
        {"actions":[],"at_ns":"0","event":{"command":3,"end":{"exited":{"code":0}},"kind":"command_ended","lease":0},"observed_ns":"0","seq":12}
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":3,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"command":4,"kind":"run_command","lease":0,"timeout_ns":null}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":5,"request":{"argv":["where"],"grant":"gpu","kind":"lease_run","lease":0,"timeout_seconds":null}},"observed_ns":"0","seq":13}
        {"actions":[],"at_ns":"0","event":{"command":4,"end":{"exited":{"code":0}},"kind":"command_ended","lease":0},"observed_ns":"0","seq":14}
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":2,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"direction":"get","kind":"transfer","lease":0,"max_bytes":256,"transfer":5}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":6,"request":{"container":"scheduler","from":"/work/in","grant":"gpu","kind":"lease_get","lease":0,"to":"/pumpkin/out/scheduler"}},"observed_ns":"0","seq":15}
        {"actions":[],"at_ns":"0","event":{"end":{"moved":{"bytes":31,"entries":2}},"kind":"transfer_ended","lease":0,"transfer":5},"observed_ns":"0","seq":16}
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":1,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"direction":"get","kind":"transfer","lease":0,"max_bytes":256,"transfer":6}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":7,"request":{"container":"swe-0","from":"/work/in","grant":"gpu","kind":"lease_get","lease":0,"to":"/pumpkin/out/swe-0"}},"observed_ns":"0","seq":17}
        {"actions":[],"at_ns":"0","event":{"end":{"moved":{"bytes":9,"entries":1}},"kind":"transfer_ended","lease":0,"transfer":6},"observed_ns":"0","seq":18}
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":1,"cumulative_seconds":null,"instance_seconds":0,"instances":1}}},{"kind":"close_lease","lease":0}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":8,"request":{"grant":"gpu","kind":"lease_close","lease":0}},"observed_ns":"0","seq":19}
        {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"lease-0"},"seq":20}
        {"actions":[],"at_ns":"0","event":{"kind":"lease_ended","lease":0},"observed_ns":"0","seq":21}
        leases/0/log/1/command.stdout:
        0644 collected: collected
        0600 config.toml: model = "gpt-6-astra"
        leases/0/log/2/command.stdout:
        leases/0/log/3/command.stdout:
        /testbed ["HOME=/pumpkin/home","INSTANCE=0"]
        leases/0/log/4/command.stdout:
        /work ["HOME=/pumpkin/home","CUDA=1"]"#]].assert_eq(&leased(&shown));
}

// The end of a companion's session fails the lease, by the companion's name. What `main` holds is
// still collected.
#[test]
fn fails_as_a_companions_session_ends() {
    let argv = [
        "ops",
        "gpu.open",
        "await_lease gpu 0 ready",
        "gpu.run 0:scheduler - crash",
        "await_lease gpu 0 failed",
        "call submit 6 finalize=true",
    ];
    let shown = executed(
        &SimHost::new(),
        "fails-as-a-companions-session-ends",
        &[(PROVIDER, &accompanied())],
        &companions_workflow(&argv),
        SANDBOXD,
        &[Also::Listing],
    );
    expect![[r#"
        record: agents/agent/log/collected, agents/agent/out/collected, agents/agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/got/1/result.json, jobs/0/log/0/collected, jobs/0/out/collected, jobs/0/out/result.json, jobs/0/sandbox.json, leases/0/log/, leases/0/out/collected, leases/0/sandbox.json, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":8,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"grant":"gpu","holder":"agent-0","instance":0,"kind":"start_lease","lease":0,"start_by_ns":"30000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"gpu","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"0","seq":3}
        {"actions":[],"at_ns":"0","event":{"kind":"lease_ready","lease":0},"observed_ns":"0","seq":4}
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":7,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"command":0,"kind":"run_command","lease":0,"timeout_ns":null}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":1,"request":{"argv":["crash"],"container":"scheduler","grant":"gpu","kind":"lease_run","lease":0,"timeout_seconds":null}},"observed_ns":"0","seq":5}
        {"at_ns":"0","note":"lease 0's log/0 is incomplete: freezing the runner's `scheduler` failed: the sandbox ended the session","seq":6}
        {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"lease-0"},"seq":7}
        {"actions":[],"at_ns":"0","event":{"kind":"lease_failed","lease":0,"reason":"the session of the runner's companion `scheduler` ended: the sandbox ended the session"},"observed_ns":"0","seq":8}"#]].assert_eq(&leased(&shown));
}

// A companion that the gateway can't trust fails its sandbox's start at once, by its name. `main`
// had met, and `scheduler` never says hello: the failure ends its wait, and the sandbox is deleted.
#[test]
fn untrusted_companion_fails_the_start() {
    let argv = [
        "ops",
        "gpu.open",
        "await_lease gpu 0 failed",
        "call submit 6 finalize=true",
    ];
    let sim = accompanied()
        .meeting_in_turn(&[("main", 1), ("swe-0", 2)])
        .impostor("swe-0");
    let shown = executed(
        &SimHost::new(),
        "untrusted-companion-fails-the-start",
        &[(PROVIDER, &sim)],
        &companions_workflow(&argv),
        SANDBOXD,
        &[Also::Met("lease-0")],
    );
    expect![[r#"
        the meetings of lease-0's containers:
          {"container":"main","kind":"container_met","sandbox":"lease-0"}, observed at 1s
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":8,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"grant":"gpu","holder":"agent-0","instance":0,"kind":"start_lease","lease":0,"start_by_ns":"30000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"gpu","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"0","seq":3}
        {"actions":[],"at_ns":"2000000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"lease-0"},"seq":4}
        {"actions":[],"at_ns":"2000000000","event":{"kind":"lease_failed","lease":0,"reason":"the runner's sandbox didn't start: sandbox sim-2: its container `swe-0`: the gateway can't trust companion `swe-0` behind the relay at localhost:$PORT: the TLS handshake failed: <why>: expected TLS 1.3 from `swe-0`, with a certificate trusted for it"},"observed_ns":"2000000000","seq":5}"#]].assert_eq(&leased(&unported(&shown)));
}

// A sandbox whose `main` ends before it meets fails its start by `main`'s name. That failure ends
// the waits of its companions, neither of which says hello.
#[test]
fn main_that_ended_stops_its_companions() {
    let argv = [
        "ops",
        "gpu.open",
        "await_lease gpu 0 failed",
        "call submit 6 finalize=true",
    ];
    let mut ir = companions_workflow(&argv);
    let runner = ir["roles"].as_array_mut().unwrap().last_mut().unwrap();
    runner["image"] = json!(sim::image(MUTE).as_str());
    // The clock passes the wait between two looks at the provider's list.
    let looked = Synced::Holding("leases/0/.sandbox.json.partial", "sim-2");
    let shown = executed(
        &SimHost::new().moving_after_read(looked, Duration::from_secs(1)),
        "main-that-ended-stops-its-companions",
        &[(PROVIDER, &accompanied().meeting_in_turn(&[]))],
        &ir,
        SANDBOXD,
        &[Also::Met("lease-0")],
    );
    expect![[r#"
        the meetings of lease-0's containers:
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":8,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"grant":"gpu","holder":"agent-0","instance":0,"kind":"start_lease","lease":0,"start_by_ns":"30000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"gpu","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"0","seq":3}
        {"actions":[],"at_ns":"1000000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"lease-0"},"seq":4}
        {"actions":[],"at_ns":"1000000000","event":{"kind":"lease_failed","lease":0,"reason":"the runner's sandbox didn't start: sandbox sim-2: its container `main`: it ended before it met the gateway: expected its `sandboxd` to run until it connects; its console ends: sandboxd: a simulated one, which never dials"},"observed_ns":"1000000000","seq":5}"#]].assert_eq(&leased(&shown));
}

// What ends while a companion still waits to meet fails the start at once, and ends that wait.
// The clock stays short of the start's deadline. `swe-0` never says hello.
fn ends_as_a_companion_waits(name: &str, sim: &Sim) -> String {
    let argv = [
        "ops",
        "gpu.open",
        "await_lease gpu 0 failed",
        "call submit 6 finalize=true",
    ];
    let ir = companions_workflow(&argv);
    let (host, met) = (SimHost::new(), &[Also::Met("lease-0")]);
    leased(&executed(
        &host,
        name,
        &[(PROVIDER, sim)],
        &ir,
        SANDBOXD,
        met,
    ))
}

// The provider lists the sandbox as ended once `scheduler` and `main` have met.
#[test]
fn fails_as_its_sandbox_ends_while_a_companion_waits() {
    let turns = &[("scheduler", 0), ("main", 0), (sim::ENDS, 1)];
    let sim = accompanied().meeting_in_turn(turns);
    let shown = ends_as_a_companion_waits("sandbox-ends-as-a-companion-waits", &sim);
    expect![[r#"
        the meetings of lease-0's containers:
          {"container":"main","kind":"container_met","sandbox":"lease-0"}, observed at 0ns
          {"container":"scheduler","kind":"container_met","sandbox":"lease-0"}, observed at 0ns
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":8,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"grant":"gpu","holder":"agent-0","instance":0,"kind":"start_lease","lease":0,"start_by_ns":"30000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"gpu","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"0","seq":3}
        {"actions":[],"at_ns":"1000000000","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"lease-0"},"seq":4}
        {"actions":[],"at_ns":"1000000000","event":{"kind":"lease_failed","lease":0,"reason":"the runner's sandbox didn't start: sandbox sim-2: it ended before its companions met the gateway: expected it to run until each of them connects"},"observed_ns":"1000000000","seq":5}"#]].assert_eq(&shown);
}

// The session of a container that has met ends: `main`'s, then a companion's.
#[test]
fn fails_as_a_met_session_ends_while_a_companion_waits() {
    let ended = |container| {
        let sim = accompanied()
            .meeting_in_turn(&[("scheduler", 0), ("main", 0)])
            .ending_once_set_up("lease-0", container);
        ends_as_a_companion_waits("met-session-ends-as-a-companion-waits", &sim)
    };
    expect![[r#"
        the meetings of lease-0's containers:
          {"container":"main","kind":"container_met","sandbox":"lease-0"}, observed at 0ns
          {"container":"scheduler","kind":"container_met","sandbox":"lease-0"}, observed at 0ns
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":8,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"grant":"gpu","holder":"agent-0","instance":0,"kind":"start_lease","lease":0,"start_by_ns":"30000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"gpu","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"0","seq":3}
        {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"lease-0"},"seq":4}
        {"actions":[],"at_ns":"0","event":{"kind":"lease_failed","lease":0,"reason":"the runner's sandbox didn't start: sandbox sim-2: its container `main`: its session ended: the sandbox ended the session"},"observed_ns":"0","seq":5}
        the meetings of lease-0's containers:
          {"container":"main","kind":"container_met","sandbox":"lease-0"}, observed at 0ns
          {"container":"scheduler","kind":"container_met","sandbox":"lease-0"}, observed at 0ns
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":8,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"grant":"gpu","holder":"agent-0","instance":0,"kind":"start_lease","lease":0,"start_by_ns":"30000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"gpu","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"0","seq":3}
        {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"lease-0"},"seq":4}
        {"actions":[],"at_ns":"0","event":{"kind":"lease_failed","lease":0,"reason":"the runner's sandbox didn't start: sandbox sim-2: its container `scheduler`: its session ended: the sandbox ended the session"},"observed_ns":"0","seq":5}"#]].assert_eq(&format!("{}\n{}", ended("main"), ended("scheduler")));
}

// A create that was given companions and names no relay leaves a sandbox that can't be met, which
// is deleted.
#[test]
fn created_without_a_relay() {
    let argv = [
        "ops",
        "gpu.open",
        "await_lease gpu 0 failed",
        "call submit 6 finalize=true",
    ];
    let shown = executed(
        &SimHost::new(),
        "created-without-a-relay",
        &[(PROVIDER, &accompanied().naming_no_relay())],
        &companions_workflow(&argv),
        SANDBOXD,
        &[],
    );
    expect![[r#"
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":8,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"grant":"gpu","holder":"agent-0","instance":0,"kind":"start_lease","lease":0,"start_by_ns":"30000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"gpu","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"0","seq":3}
        {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"lease-0"},"seq":4}
        {"actions":[],"at_ns":"0","event":{"kind":"lease_failed","lease":0,"reason":"the runner's sandbox didn't start: sandbox sim-2: its create gave no relay, though it was given 2 companions: expected the endpoint where its relay listens"},"observed_ns":"0","seq":5}"#]].assert_eq(&leased(&shown));
}

// Only a sandbox that listens holds companions, so a runner that declares them where sandboxes
// dial is refused before any sandbox starts.
#[test]
fn companions_where_sandboxes_dial() {
    let shown = executed(
        &SimHost::new(),
        "companions-where-sandboxes-dial",
        &[(PROVIDER, &Sim::new().offering_companions())],
        &companions_workflow(&["exit", "0"]),
        SANDBOXD,
        &[Also::Listing],
    );
    expect![[r#"
        failed: role `gpu_runner` declares companions, and the sandboxes of provider `simulated` hold none, as they don't listen: declare none, or run it on a provider whose sandboxes listen
        record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json
        log.jsonl:"#]].assert_eq(&shown);
}

#[test]
fn lease_ends() {
    let argv = [
        "ops",
        "gpu.open 1",
        "await_lease gpu 0 ready",
        "gpu.run 0 - sleep",
        // Past the lease's 1 s.
        "sleep 2",
        // A lease is `expired` only once its runner is gone, so its end comes before the next
        // lease's events.
        "await_lease gpu 0 expired",
        "gpu.open",
        "await_lease gpu 1 ready",
        "gpu.run 1 - sleep",
        "call submit 6",
        "exit",
    ];
    let ir = lease_workflow(&argv, 30, RUNNER);
    let dir = TestDir::new();
    let (record, _, _) = judged(&SimHost::new(), ir, &dir);
    let log = read_log(&record);
    let closes: Vec<_> = log
        .iter()
        .flat_map(|line| {
            line["actions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|action| action["kind"] == "close_lease")
                .map(|action| (&line["event"]["kind"], &action["lease"]))
        })
        .collect();
    assert_eq!(
        closes,
        [
            (&json!("tick"), &json!(0)),
            (&json!("agent_exited"), &json!(1))
        ]
    );
    let judge = log
        .iter()
        .position(|line| {
            line["actions"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|action| action["kind"] == "start_final_call")
        })
        .unwrap();
    for lease in [0, 1] {
        assert!(log[..=judge].iter().any(|line| {
            line["event"]["kind"] == "lease_ended" && line["event"]["lease"] == lease
        }));
    }
    assert!(
        log[..=judge]
            .iter()
            .any(|line| line["event"]["kind"] == "agent_torn_down")
    );
}

// Each entry keeps the mode it had in the agent's sandbox, whatever the host's umask; a transfer
// counts as a command.
#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the transcript")]
fn lease_transfers() {
    // A umask that narrows every mode the agent's sandbox sent, so that a put restores each.
    rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o077));
    let argv = [
        "ops",
        "gpu.open",
        "await_lease gpu 0 ready",
        "gpu.put 0 /workspace/.codex /work/in",
        "await_transfer gpu 0 moved",
        "gpu.run 0 - list /work/in",
        "await_lease gpu 0 ready",
        "gpu.put 0 /workspace/.codex /work/in",
        "await_transfer gpu 0 failed",
        "gpu.get 0 /pumpkin/log/1 /pumpkin/out/got",
        "await_transfer gpu 0 moved",
        "gpu.get 0 /work/in /pumpkin/out/back",
        "await_transfer gpu 0 moved",
        "gpu.put 0 /pumpkin/log /work/in replace",
        "await_transfer gpu 0 moved",
        "gpu.get 0 /work/in /pumpkin/out/replaced",
        "await_transfer gpu 0 moved",
        "gpu.put 0 /workspace/big /work/big",
        "await_transfer gpu 0 failed",
        "gpu.close 0",
        "await_lease gpu 0 closed",
        "call submit 6 finalize=true",
    ];
    let mut ir = lease_workflow(&argv, 30, RUNNER);
    let lease = &mut ir["roles"][0]["runtime"]["agent"]["grants"][1]["session"];
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
    let shown = listening_too(|sim| {
        executed(
            &SimHost::new(),
            "lease-transfers",
            &[(PROVIDER, &sim)],
            &ir,
            SANDBOXD,
            &[Also::Listing, Also::File("leases/0/log/1/command.stdout")],
        )
    });
    expect![[r#"
        finished, showing [score=6]
        record: agents/agent/log/collected, agents/agent/out/back/collected, agents/agent/out/back/config.toml, agents/agent/out/back/ro, agents/agent/out/back/shared, agents/agent/out/back/shut/key, agents/agent/out/back/tool*, agents/agent/out/collected, agents/agent/out/got/collected, agents/agent/out/got/command.stdout, agents/agent/out/replaced/collected, agents/agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/got/1/result.json, jobs/0/log/0/collected, jobs/0/out/collected, jobs/0/out/result.json, jobs/0/sandbox.json, leases/0/log/1/collected, leases/0/log/1/command.stdout, leases/0/out/collected, leases/0/sandbox.json, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
        payloads hold: ["xxxxxx"]
        outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":6}]}}}
        log.jsonl:
        {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":8,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"grant":"gpu","holder":"agent-0","instance":0,"kind":"start_lease","lease":0,"start_by_ns":"30000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"gpu","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"0","seq":3}
        {"actions":[],"at_ns":"0","event":{"kind":"lease_ready","lease":0},"observed_ns":"0","seq":4}
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":7,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"direction":"put","kind":"transfer","lease":0,"max_bytes":256,"transfer":0}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":1,"request":{"grant":"gpu","kind":"lease_put","lease":0,"replace":false,"to":"/work/in","tree":"/workspace/.codex"}},"observed_ns":"0","seq":5}
        {"actions":[],"at_ns":"0","event":{"end":{"moved":{"bytes":38,"entries":7}},"kind":"transfer_ended","lease":0,"transfer":0},"observed_ns":"0","seq":6}
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":6,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"command":1,"kind":"run_command","lease":0,"timeout_ns":null}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":2,"request":{"argv":["list","/work/in"],"grant":"gpu","kind":"lease_run","lease":0,"timeout_seconds":null}},"observed_ns":"0","seq":7}
        {"actions":[],"at_ns":"0","event":{"command":1,"end":{"exited":{"code":0}},"kind":"command_ended","lease":0},"observed_ns":"0","seq":8}
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":5,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"direction":"put","kind":"transfer","lease":0,"max_bytes":256,"transfer":2}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":3,"request":{"grant":"gpu","kind":"lease_put","lease":0,"replace":false,"to":"/work/in","tree":"/workspace/.codex"}},"observed_ns":"0","seq":9}
        {"actions":[],"at_ns":"0","event":{"end":{"failed":"putting the tree at /work/in in the runner failed: /work/in: File exists (os error 17): the tree's root becomes /work/in, which must not exist: give `--replace` to replace it"},"kind":"transfer_ended","lease":0,"transfer":2},"observed_ns":"0","seq":10}
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":4,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"direction":"get","kind":"transfer","lease":0,"max_bytes":256,"transfer":3}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":4,"request":{"from":"/pumpkin/log/1","grant":"gpu","kind":"lease_get","lease":0,"to":"/pumpkin/out/got"}},"observed_ns":"0","seq":11}
        {"actions":[],"at_ns":"0","event":{"end":{"moved":{"bytes":145,"entries":2}},"kind":"transfer_ended","lease":0,"transfer":3},"observed_ns":"0","seq":12}
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":3,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"direction":"get","kind":"transfer","lease":0,"max_bytes":256,"transfer":4}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":5,"request":{"from":"/work/in","grant":"gpu","kind":"lease_get","lease":0,"to":"/pumpkin/out/back"}},"observed_ns":"0","seq":13}
        {"actions":[],"at_ns":"0","event":{"end":{"moved":{"bytes":38,"entries":7}},"kind":"transfer_ended","lease":0,"transfer":4},"observed_ns":"0","seq":14}
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":2,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"direction":"replace","kind":"transfer","lease":0,"max_bytes":256,"transfer":5}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":6,"request":{"grant":"gpu","kind":"lease_put","lease":0,"replace":true,"to":"/work/in","tree":"/pumpkin/log"}},"observed_ns":"0","seq":15}
        {"actions":[],"at_ns":"0","event":{"end":{"moved":{"bytes":9,"entries":1}},"kind":"transfer_ended","lease":0,"transfer":5},"observed_ns":"0","seq":16}
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":1,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"direction":"get","kind":"transfer","lease":0,"max_bytes":256,"transfer":6}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":7,"request":{"from":"/work/in","grant":"gpu","kind":"lease_get","lease":0,"to":"/pumpkin/out/replaced"}},"observed_ns":"0","seq":17}
        {"actions":[],"at_ns":"0","event":{"end":{"moved":{"bytes":9,"entries":1}},"kind":"transfer_ended","lease":0,"transfer":6},"observed_ns":"0","seq":18}
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":0,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"direction":"put","kind":"transfer","lease":0,"max_bytes":256,"transfer":7}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":8,"request":{"grant":"gpu","kind":"lease_put","lease":0,"replace":false,"to":"/work/big","tree":"/workspace/big"}},"observed_ns":"0","seq":19}
        {"actions":[],"at_ns":"0","event":{"end":{"failed":"collecting /workspace/big failed: the tree's files hold more than 256 bytes"},"kind":"transfer_ended","lease":0,"transfer":7},"observed_ns":"0","seq":20}
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":0,"cumulative_seconds":null,"instance_seconds":0,"instances":1}}},{"kind":"close_lease","lease":0}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":9,"request":{"grant":"gpu","kind":"lease_close","lease":0}},"observed_ns":"0","seq":21}
        {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"lease-0"},"seq":22}
        {"actions":[],"at_ns":"0","event":{"kind":"lease_ended","lease":0},"observed_ns":"0","seq":23}
        {"actions":[{"kind":"reply","reply":{"kind":"call","remaining_seconds":30,"returned":{"state":"accepted"}}}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":10,"request":{"evaluated":{"effects":["set_submission","end_work"],"returned":[{"name":"state","value":"accepted"}]},"grant":"submit","kind":"now","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":24}
        {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":25}
        {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 10 files of 201 bytes; log, 1 file of 9 bytes","seq":26}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":27}
        {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":28}
        {"actions":[{"job":0,"kind":"start_final_call","selections":[{"role":"agent","selection":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}],"timeout_ns":"120000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":29}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":30}
        {"actions":[],"at_ns":"0","event":{"bytes":6,"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","job":0,"kind":"target_put","path":"/pumpkin/in/payload/raw"},"observed_ns":"0","seq":31}
        {"actions":[],"at_ns":"0","event":{"argv":["judge","/pumpkin/in/payload/raw"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"0","seq":32}
        {"actions":[],"at_ns":"0","event":{"command":0,"end":{"exited":{"code":0}},"job":0,"kind":"command_ended"},"observed_ns":"0","seq":33}
        {"actions":[],"at_ns":"0","event":{"bytes":15,"digest":"sha256:68e3ea1c52db57d6e28c6d873ef07678bc738b836a1ec8ae5f831875083035e9","job":0,"kind":"target_get","path":"/pumpkin/out/result.json"},"observed_ns":"0","seq":34}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_returned","returned":[{"name":"score","value":6}]},"observed_ns":"0","seq":35}
        {"actions":[],"at_ns":"0","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":36}
        {"actions":[{"kind":"finish"}],"at_ns":"0","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"0","seq":37}
        its last record came before the agent's teardown was to end
        leases/0/log/1/command.stdout:
        0644 collected: collected
        0600 config.toml: model = "gpt-6-astra"

        0444 ro: r
        0666 shared: s
        0551 shut/
        0600 shut/key: k
        0750 tool: true"#]]
    .assert_eq(&shown);
}

// The collect from the agent's sandbox waits, so the run goes on deciding meanwhile, answering
// the agent's ops.
#[test]
fn transfer_cut() {
    let argv = [
        "ops",
        "gpu.open",
        "await_lease gpu 0 ready",
        "gpu.put 0 /workspace/held /work/in",
        "ops",
        "gpu.close 0",
        // The close's reply comes before its stop reaches the lease, and an op's reply after the
        // actions of the op before, so that the tree arrives only once the stop has, not as the
        // threads happen to run.
        "gpu.status 0",
        "release",
        "await_transfer gpu 0 failed",
        // The lease's end and the agent's exit reach the run's loop from different threads;
        // `closed` is said only once the loop has taken the end, so the end comes before the exit.
        "await_lease gpu 0 closed",
        "call submit 6 finalize=true",
    ];
    let shown = executed(
        &SimHost::new(),
        "transfer-cut",
        &[(PROVIDER, &Sim::new())],
        &lease_workflow(&argv, 30, RUNNER),
        SANDBOXD,
        &[Also::Listing],
    );
    expect![[r#"
        finished, showing [score=6]
        record: agents/agent/log/collected, agents/agent/out/collected, agents/agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/got/1/result.json, jobs/0/log/0/collected, jobs/0/out/collected, jobs/0/out/result.json, jobs/0/sandbox.json, leases/0/log/, leases/0/out/collected, leases/0/sandbox.json, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
        payloads hold: ["xxxxxx"]
        outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":6}]}}}
        log.jsonl:
        {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
        {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":4,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"grant":"gpu","holder":"agent-0","instance":0,"kind":"start_lease","lease":0,"start_by_ns":"30000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"gpu","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"0","seq":3}
        {"actions":[],"at_ns":"0","event":{"kind":"lease_ready","lease":0},"observed_ns":"0","seq":4}
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":3,"cumulative_seconds":null,"instance_seconds":600,"instances":1}}},{"direction":"put","kind":"transfer","lease":0,"max_bytes":64,"transfer":0}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":1,"request":{"grant":"gpu","kind":"lease_put","lease":0,"replace":false,"to":"/work/in","tree":"/workspace/held"}},"observed_ns":"0","seq":5}
        {"actions":[{"kind":"reply","reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"job","name":"train","payload_max_bytes":8},{"commands":4,"cumulative_seconds":null,"instance_seconds":600,"instances":2,"kind":"session","name":"gpu","remaining":{"commands":3,"cumulative_seconds":null,"instance_seconds":600,"instances":1},"transfer_bytes":64},{"arguments":{"finalize":{"default":false,"type":"bool"}},"kind":"now","name":"submit","payload_max_bytes":16}],"kind":"ops","remaining_seconds":30,"resources":{"cpu_millis":8000,"memory_bytes":34359738368},"role":"agent"}}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":2,"request":{"kind":"ops"}},"observed_ns":"0","seq":6}
        {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":3,"cumulative_seconds":null,"instance_seconds":0,"instances":1}}},{"kind":"close_lease","lease":0}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":3,"request":{"grant":"gpu","kind":"lease_close","lease":0}},"observed_ns":"0","seq":7}
        {"actions":[],"at_ns":"0","event":{"end":{"failed":"the lease's end cut the transfer short: collecting /workspace/held failed: stopped before the tree was whole"},"kind":"transfer_ended","lease":0,"transfer":0},"observed_ns":"0","seq":8}
        {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"lease-0"},"seq":9}
        {"actions":[],"at_ns":"0","event":{"kind":"lease_ended","lease":0},"observed_ns":"0","seq":10}
        {"actions":[{"kind":"reply","reply":{"kind":"call","remaining_seconds":30,"returned":{"state":"accepted"}}}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":4,"request":{"evaluated":{"effects":["set_submission","end_work"],"returned":[{"name":"state","value":"accepted"}]},"grant":"submit","kind":"now","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":11}
        {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"0","seq":12}
        {"at_ns":"0","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":13}
        {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_collected"},"seq":14}
        {"actions":[],"at_ns":"0","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":15}
        {"actions":[{"job":0,"kind":"start_final_call","selections":[{"role":"agent","selection":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}],"timeout_ns":"120000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"0","seq":16}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":17}
        {"actions":[],"at_ns":"0","event":{"bytes":6,"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","job":0,"kind":"target_put","path":"/pumpkin/in/payload/raw"},"observed_ns":"0","seq":18}
        {"actions":[],"at_ns":"0","event":{"argv":["judge","/pumpkin/in/payload/raw"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"0","seq":19}
        {"actions":[],"at_ns":"0","event":{"command":0,"end":{"exited":{"code":0}},"job":0,"kind":"command_ended"},"observed_ns":"0","seq":20}
        {"actions":[],"at_ns":"0","event":{"bytes":15,"digest":"sha256:68e3ea1c52db57d6e28c6d873ef07678bc738b836a1ec8ae5f831875083035e9","job":0,"kind":"target_get","path":"/pumpkin/out/result.json"},"observed_ns":"0","seq":21}
        {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_returned","returned":[{"name":"score","value":6}]},"observed_ns":"0","seq":22}
        {"actions":[],"at_ns":"0","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":23}
        {"actions":[{"kind":"finish"}],"at_ns":"0","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"0","seq":24}
        its last record came before the agent's teardown was to end"#]]
    .assert_eq(&shown);
}

/// [`ops_workflow`]'s, with a runner `box`, of which the trainer holds a session, and whose `train`
/// runs `holder_argv` ([`train_runs`]).
fn held_lease_workflow(argv: &[&str], holder_argv: &[&str]) -> Value {
    let mut ir = ops_workflow(argv, 30);
    let mut runner = ir["roles"][1].clone();
    runner["name"] = json!("box");
    runner["runtime"] = json!({ "runner": { "cwd": "/work" } });
    ir["roles"].as_array_mut().unwrap().push(runner);
    ir["roles"][1]["runtime"]["target"]["sessions"] = json!([{ "name": "box", "session": {
        "runner": "box", "instances": 1, "instance_seconds": 60, "transfer_bytes": 64,
    } }]);
    train_runs(ir, holder_argv)
}

// The lease starts by the call's timeout, not by its class's startup, so its create gives up
// then.
#[test]
fn lease_starts_by_its_holders_end() {
    let holder_argv = ["ops", "box.open"];
    let argv = [
        "ops",
        "call train 6",
        "await 0 completed",
        // The call's lease fails after its result comes.
        "await_log lease_failed",
        "call submit 6 finalize=true",
    ];
    let mut ir = held_lease_workflow(&argv, &holder_argv);
    ir["roles"][0]["runtime"]["agent"]["grants"][0]["job"]["call_timeout_seconds"] = json!(1);
    ir["roles"][3]["image"] = json!(sim::image(SLOW).as_str());
    // The clock reaches the call's 1 s timeout, where the lease's create gives up, once the
    // call's teardown is logged.
    let host = SimHost::new();
    let torn_down = &[r#"{"job":0,"kind":"call_torn_down"}"#];
    let sim = Sim::new().once_logged(torn_down, host.moving_to(Duration::from_secs(1)));
    check(
        &host,
        "lease-starts-by-its-holders-end",
        &[(PROVIDER, &sim)],
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, showing [score=6]
            payloads hold: ["xxxxxx"]
            outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":6}]}}}
            log.jsonl:
            {"actions":[{"kind":"start_agents"}],"at_ns":"0","event":{"hard_end_ns":"300000000000","kind":"run_started","unix_ns":"<during execute>"},"observed_ns":"0","seq":0}
            {"actions":[],"at_ns":"0","event":{"kind":"work_started"},"observed_ns":"0","seq":1}
            {"actions":[],"at_ns":"0","event":{"caller":"agent-0","kind":"agent_started"},"observed_ns":"0","seq":2}
            {"actions":[{"kind":"reply","reply":{"job":0,"kind":"call","remaining":{"calls":99,"seconds":null}}},{"caller":"agent-0","grant":"train","job":0,"kind":"start_call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"0","event":{"caller":"agent-0","kind":"op_arrived","op":0,"request":{"grant":"train","kind":"call","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"0","seq":3}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_started"},"observed_ns":"0","seq":4}
            {"actions":[],"at_ns":"0","event":{"bytes":6,"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","job":0,"kind":"target_put","path":"/pumpkin/in/payload/raw"},"observed_ns":"0","seq":5}
            {"actions":[],"at_ns":"0","event":{"argv":["ops","box.open"],"command":0,"job":0,"kind":"target_run"},"observed_ns":"0","seq":6}
            {"actions":[{"kind":"reply","reply":{"kind":"lease","lease":0,"remaining":{"commands":null,"cumulative_seconds":null,"instance_seconds":60,"instances":0}}},{"grant":"box","holder":"job-0","instance":0,"kind":"start_lease","lease":0,"start_by_ns":"1000000000"}],"at_ns":"0","event":{"caller":"job-0","kind":"op_arrived","op":1,"request":{"grant":"box","instance_seconds":null,"kind":"lease_open"}},"observed_ns":"0","seq":7}
            {"actions":[],"at_ns":"0","event":{"command":0,"end":{"exited":{"code":0}},"job":0,"kind":"command_ended"},"observed_ns":"0","seq":8}
            {"actions":[{"kind":"close_lease","lease":0}],"at_ns":"0","event":{"job":0,"kind":"call_returned","returned":[]},"observed_ns":"0","seq":9}
            {"actions":[],"at_ns":"0","event":{"id":"sim-2","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-0"},"seq":10}
            {"actions":[],"at_ns":"0","event":{"job":0,"kind":"call_torn_down"},"observed_ns":"0","seq":11}
            {"actions":[],"at_ns":"1000000000","event":{"kind":"lease_failed","lease":0,"reason":"the runner's sandbox didn't start: the simulated create gave up at its deadline, and can't name what it made: expected the run's list to find it"},"observed_ns":"1000000000","seq":12}
            {"actions":[{"kind":"reply","reply":{"kind":"call","remaining_seconds":29,"returned":{"state":"accepted"}}}],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"op_arrived","op":2,"request":{"evaluated":{"effects":["set_submission","end_work"],"returned":[{"name":"state","value":"accepted"}]},"grant":"submit","kind":"now","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"observed_ns":"1000000000","seq":13}
            {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"agent_exited","status":{"code":0}},"observed_ns":"1000000000","seq":14}
            {"at_ns":"1000000000","note":"collected from agent `agent`'s sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":15}
            {"actions":[],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"agent_collected"},"seq":16}
            {"actions":[],"at_ns":"1000000000","event":{"id":"sim-1","kind":"sandbox_deleted","provider":"simulated","sandbox":"agent-0"},"seq":17}
            {"actions":[{"job":1,"kind":"start_final_call","selections":[{"role":"agent","selection":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}],"timeout_ns":"120000000000"}],"at_ns":"1000000000","event":{"caller":"agent-0","kind":"agent_torn_down"},"observed_ns":"1000000000","seq":18}
            {"actions":[],"at_ns":"1000000000","event":{"id":"sim-3","kind":"sandbox_deleted","provider":"simulated","sandbox":null},"seq":19}
            {"actions":[],"at_ns":"1000000000","event":{"job":1,"kind":"call_started"},"observed_ns":"1000000000","seq":20}
            {"actions":[],"at_ns":"1000000000","event":{"bytes":6,"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","job":1,"kind":"target_put","path":"/pumpkin/in/payload/raw"},"observed_ns":"1000000000","seq":21}
            {"actions":[],"at_ns":"1000000000","event":{"argv":["judge","/pumpkin/in/payload/raw"],"command":0,"job":1,"kind":"target_run"},"observed_ns":"1000000000","seq":22}
            {"actions":[],"at_ns":"1000000000","event":{"command":0,"end":{"exited":{"code":0}},"job":1,"kind":"command_ended"},"observed_ns":"1000000000","seq":23}
            {"actions":[],"at_ns":"1000000000","event":{"bytes":15,"digest":"sha256:68e3ea1c52db57d6e28c6d873ef07678bc738b836a1ec8ae5f831875083035e9","job":1,"kind":"target_get","path":"/pumpkin/out/result.json"},"observed_ns":"1000000000","seq":24}
            {"actions":[],"at_ns":"1000000000","event":{"job":1,"kind":"call_returned","returned":[{"name":"score","value":6}]},"observed_ns":"1000000000","seq":25}
            {"actions":[],"at_ns":"1000000000","event":{"id":"sim-4","kind":"sandbox_deleted","provider":"simulated","sandbox":"job-1"},"seq":26}
            {"actions":[{"kind":"finish"}],"at_ns":"1000000000","event":{"job":1,"kind":"call_torn_down"},"observed_ns":"1000000000","seq":27}
            its last record came before the agent's teardown was to end"#]],
    );
}

fn submissions_workflow(argv: &[&str]) -> Value {
    let mut ir = ops_workflow(argv, 30);
    ir["final"]["body"]["def"] = json!("submissions");
    let alpha = &mut ir["roles"][0];
    alpha["name"] = json!("alpha");
    ir
}

fn execute_agents(
    host: &impl Shown,
    startup: u64,
    ir: Value,
    dir: &TestDir,
    at: Option<At>,
) -> (PathBuf, Result<Outcome, runtime::Stopped>) {
    let ir = serde_json::from_value(ir).unwrap();
    let sim = Sim::with_startup(startup).priced(1.0);
    let placed = placed(&ir, sim.ready()).unwrap();
    let ids = placed
        .workflow()
        .agents()
        .map(|agent| agent.id)
        .collect::<Vec<_>>();
    let upstream = model_routed(&ir).then(|| StandIn::start(host.time()));
    let servings = StandIn::servings(upstream.as_ref(), placed.workflow(), dir.path());
    let prices = prices();
    let (models, recorded) = kept(&placed, &servings, &prices);
    let tag = new_id(SystemTime::now()).unwrap();
    let record = dir.path().join(tag.as_str());
    let provider = Interrupting {
        sim: &sim,
        at,
        record: &record,
    };
    let on = providers_of(&placed, |_| &provider);
    let run = Run::prepare(host, placed, on, dir.path(), tag.clone(), models, recorded).unwrap();
    sim.records(&record, host.time());
    let outcome = thread::scope(|scope| {
        if let Some(At::Logged(event)) = at {
            scope.spawn(|| {
                sim::logged(&record, event);
                interrupt();
            });
        }
        run.execute(
            Path::new(SANDBOXD),
            None,
            &Dialer::new(test_support::edge::client()),
            &Bodies { record: &record },
            || Ok(()),
            driver_used,
        )
    });
    if let Ok(Outcome { agents, .. }) = &outcome {
        assert_eq!(agents.iter().map(|m| m.agent).collect::<Vec<_>>(), ids);
    }
    assert!(sim.list(&tag).unwrap().is_empty());
    if let Some(upstream) = upstream {
        upstream.stop();
    }
    (record, outcome)
}

fn judged(
    host: &impl Shown,
    ir: Value,
    dir: &TestDir,
) -> (PathBuf, Vec<decision::AgentOutcome>, decision::CallResult) {
    let (record, outcome) = execute_agents(host, 2, ir, dir, None);
    let Ok(Outcome {
        agents,
        result: Ok(result),
    }) = outcome
    else {
        panic!("expected final judge to return: {outcome:?}")
    };
    (record, agents, result)
}

#[test]
fn op_arriving_before_launch_confirmation_can_set_submission() {
    let ir = submissions_workflow(&["ops_before_started", "call submit 6 finalize=true"]);
    let dir = TestDir::new();
    let (_, agents, result) = judged(&SimHost::new(), ir, &dir);
    assert!(agents[0].submission.as_ref().unwrap().finalized);
    assert_eq!(
        result.values()[0].1,
        json!({"alpha":[120,120,120,120,120,120]})
    );
}

fn scoped_runner(argv: &[&str]) -> Value {
    let mut ir = held_lease_workflow(&["ops", "call submit binary finalize=true"], &["exit", "0"]);
    let grants = ir["roles"][1]["runtime"]["target"]
        .as_object_mut()
        .unwrap()
        .remove("sessions")
        .unwrap();
    ir["roles"][2]["runtime"]["target"]["sessions"] = grants;
    ir["roles"][2]["runtime"]["target"]["sessions"][0]["session"]["commands"] = json!(3);
    ir["final"]["body"]["def"] = json!("scoped");
    ir["final"]["keywords"] = json!({ "argv": argv });
    ir
}

#[test]
fn work_target_bodies_run_separate_leases_for_each_callers_job() {
    let mut ir = held_lease_workflow(
        &[
            "ops",
            "call train first",
            "await 0 completed",
            "call submit a finalize=true",
        ],
        &["worker", "0"],
    );
    ir["roles"][1]["runtime"]["target"]["lifetime"] = json!("work_phase");
    ir["roles"][1]["runtime"]["target"]["sessions"][0]["session"]["commands"] = json!(3);
    let agent = &mut ir["roles"][0];
    agent["name"] = json!("alpha");
    agent["runtime"]["agent"]["grants"][0]["job"]["body"]["def"] = json!("scoped");
    let mut peer = agent.clone();
    peer["name"] = json!("beta");
    peer["runtime"]["agent"]["command"]["argv"] = json!([
        "ops",
        "await_log agent_collected",
        "call train second",
        "await 1 completed",
        "call submit b finalize=true"
    ]);
    ir["roles"].as_array_mut().unwrap().push(peer);
    ir["final"]["body"]["def"] = json!("submissions");

    let dir = TestDir::new();
    let (record, agents, _) = judged(&SimHost::new(), ir, &dir);
    assert!(agents.iter().all(|agent| agent.submission.is_some()));
    let log = read_log(&record);
    let leases: Vec<_> = log
        .iter()
        .flat_map(|line| line["actions"].as_array().into_iter().flatten())
        .filter(|action| action["kind"] == "start_lease")
        .map(|action| {
            (
                action["holder"].as_str().unwrap(),
                action["lease"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(leases, [("job-0", 0), ("job-1", 1)]);
    for job in 0..leases.len() {
        assert!(record.join(format!("leases/{job}/sandbox.json")).is_file());
        assert!(!record.join(format!("jobs/{job}/sandbox.json")).exists());
    }
    assert!(record.join("shared/trainer/sandbox.json").is_file());
}

#[test]
fn body_waits_for_its_numbered_work_and_deletion() {
    let dir = TestDir::new();
    let (record, _, result) = judged(&SimHost::new(), scoped_runner(&["worker", "7"]), &dir);
    let command = &result.values()[0].1;
    assert_eq!(command["end"], json!({"exited":{"code":7}}));
    assert_eq!(command["stdout_tail"], "absent");
    assert_eq!(command["stderr_tail"], "worker diagnostic");
    assert_eq!(
        fs::read(record.join("jobs/0/out/final/source")).unwrap(),
        [0, 255, 1]
    );
    assert_eq!(
        fs::read(record.join("leases/0/log/1/command.stderr")).unwrap(),
        b"worker diagnostic"
    );
    let log = read_log(&record);
    assert!(
        log.iter()
            .all(|line| line["event"]["request"]["kind"] != "lease_status")
    );
    let event_at = |kind| {
        log.iter()
            .position(|line| line["event"]["kind"] == kind)
            .unwrap()
    };
    assert!(event_at("lease_ended") < event_at("call_returned"));
    let deleted = log
        .iter()
        .position(|line| {
            line["event"]["kind"] == "sandbox_deleted" && line["event"]["sandbox"] == "lease-0"
        })
        .unwrap();
    assert!(deleted < event_at("lease_ended"));
}

#[test]
fn runner_command_timeout_returns_its_collected_output() {
    let dir = TestDir::new();
    let (_, _, result) = judged(&System, scoped_runner(&["worker", "wait"]), &dir);
    let command = &result.values()[0].1;
    assert_eq!(command["end"], "timed_out");
    assert_eq!(command["stdout_tail"], "absent");
    assert_eq!(command["stderr_tail"], "worker diagnostic");
}

#[test]
fn a_body_cannot_act_on_an_unsynced_runner_receipt() {
    let dir = TestDir::new();
    let host = SimHost::new().failed_sync(Synced::Holding("log.jsonl", r#""kind":"lease_ready""#));
    let (record, outcome) = execute_agents(&host, 2, scoped_runner(&["worker", "0"]), &dir, None);
    let Err(runtime::Stopped::Failed(reason)) = outcome else {
        panic!("expected a failed sync: {outcome:?}")
    };
    assert!(reason.contains("the simulated host's sync failed"));
    let log = read_log(&record);
    assert!(
        log.iter()
            .any(|line| line["event"]["kind"] == "lease_ready")
    );
    assert!(log.iter().all(|line| line["event"]["kind"] != "target_put"));
}

#[test]
fn a_close_receipt_at_the_parent_deadline_times_out_the_body() {
    let dir = TestDir::new();
    let mut ir = scoped_runner(&["worker", "0"]);
    ir["final"]["timeout_seconds"] = json!(10);
    let host = SimHost::new().slow_sync(SlowSync {
        record: Synced::Holding("leases/0/.sandbox.json.partial", DELETED),
        queued: 0,
        at: Duration::from_secs(10),
    });
    let (record, outcome) = execute_agents(&host, 2, ir, &dir, None);
    assert!(matches!(
        outcome.unwrap().result,
        Err(decision::Failure::Timeout)
    ));
    let log = read_log(&record);
    let observed = |kind| {
        log.iter()
            .find(|line| line["event"]["kind"] == kind)
            .unwrap()["observed_ns"]
            .clone()
    };
    assert_eq!(observed("lease_ended"), "10000000000");
    assert_eq!(observed("call_timed_out"), "10000000000");
}

#[test]
fn interrupting_a_body_wait_deletes_its_runner() {
    let dir = TestDir::new();
    let (record, outcome) = execute_agents(
        &System,
        2,
        scoped_runner(&["worker", "wait"]),
        &dir,
        Some(At::Logged(r#""kind":"run_command""#)),
    );
    assert!(matches!(outcome, Err(runtime::Stopped::Interrupted(_))));
    let deleted: Value =
        serde_json::from_slice(&fs::read(record.join("leases/0/sandbox.json")).unwrap()).unwrap();
    assert_eq!(deleted["delete"]["state"], "deleted");
    assert!(record.join("leases/0/log/1/command.stderr").is_file());
}

#[test]
fn failure_to_delete_a_scoped_runner_fails_the_run() {
    let mut ir = scoped_runner(&["worker", "0"]);
    ir["roles"][3]["image"] = json!(sim::image(REFUSED_ONCE).as_str());
    let dir = TestDir::new();
    let (_, outcome) = execute_agents(&SimHost::new(), 2, ir, &dir, None);
    let Err(runtime::Stopped::Failed(reason)) = outcome else {
        panic!("expected deletion failure: {outcome:?}")
    };
    assert!(reason.contains("can't be deleted"));
}

#[test]
fn missing_runner_evidence_fails_the_call() {
    let dir = TestDir::new();
    let (record, outcome) = execute_agents(
        &SimHost::new(),
        2,
        scoped_runner(&["print", "answer"]),
        &dir,
        None,
    );
    assert!(outcome.unwrap().result.is_err());
    let log = read_log(&record);
    let failure = log
        .iter()
        .find(|line| line["event"]["kind"] == "call_failed")
        .unwrap();
    assert!(
        failure["event"]["reason"]
            .as_str()
            .unwrap()
            .contains("command.stderr")
    );
}

#[test]
fn releases_agents_and_a_shared_target_ready_before_the_driver_takes_their_notices() {
    let mut ir = work_target_workflow(&["ops", "call submit 6 finalize=true"], "board");
    let mut peer = ir["roles"][0].clone();
    peer["name"] = json!("beta");
    ir["roles"].as_array_mut().unwrap().push(peer);
    ir["roles"][0]["runtime"]["agent"]["command"]["argv"] = json!([
        "ops",
        "call train x",
        "await 0 completed",
        "call submit 6 finalize=true"
    ]);
    let wait = LateTake {
        behind: 2,
        at: Duration::from_secs(3),
    };
    let host = SimHost::new().during_setup(wait, || {});
    let dir = TestDir::new();
    let (_, result) = execute_agents(&host, 2, ir, &dir, None);
    assert!(
        result
            .unwrap()
            .agents
            .iter()
            .all(|agent| agent.submission.is_some())
    );
}

// The clock passes the startup once both sandboxes have sent their set-up's put, so each put fails
// at its bound, waiting for its answer, and the setup takes both notices at once.
#[test]
fn set_up_past_the_startup() {
    let past = Duration::from_secs(31);
    let both = LateTake {
        behind: 1,
        at: past,
    };
    let host = SimHost::new().during_setup(both, || {});
    let sim = Sim::new().holding_set_ups(2, host.moving_to(past));
    let ir = work_target_workflow(&["ops", "call submit 6 finalize=true"], "board");
    let shown = executed(
        &host,
        "set-up-past-the-startup",
        &[(PROVIDER, &sim)],
        &ir,
        SANDBOXD,
        &[],
    );
    // The two notices race each other into the setup's inbox, which orders their failures, so
    // each is shown without its place.
    let failure = |line: &&str| {
        line.starts_with('{')
            && (line.contains(r#""kind":"agent_failed""#)
                || line.contains(r#""kind":"shared_failed""#))
    };
    let mut failed: Vec<String> = (shown.lines())
        .filter(failure)
        .map(|line| {
            let mut failure: Value = serde_json::from_str(line).unwrap();
            failure.as_object_mut().unwrap().remove("seq");
            failure.to_string()
        })
        .collect();
    failed.sort_unstable();
    let outcome = shown.lines().find(|line| line.starts_with("outcome.json:"));
    failed.extend(outcome.map(str::to_owned));
    expect![[r#"
        {"actions":[],"at_ns":"31000000000","event":{"kind":"shared_failed","reason":"the directories under `/pumpkin` can't be put in the sandbox: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts","shared":1},"observed_ns":"31000000000"}
        {"actions":[{"caller":"agent-0","kind":"tear_down_agent"}],"at_ns":"31000000000","event":{"caller":"agent-0","kind":"agent_failed","reason":"the agent's sandbox didn't start: the directories under `/pumpkin` can't be put in the sandbox: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts"},"observed_ns":"31000000000"}
        outcome.json: {"agents":[{"role":"alpha","submission":null,"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: the directories under `/pumpkin` can't be put in the sandbox: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts"}}],"verdict":{"kind":"returned","result":{"values":[{"name":"files","value":{"alpha":null}}]}}}"#]].assert_eq(&failed.join("\n"));
}

#[test]
fn interrupt_during_peer_setup_keeps_ready_commands_unlaunched() {
    let mut ir = submissions_workflow(&["mark"]);
    let mut peer = ir["roles"][0].clone();
    peer["name"] = json!("beta");
    peer["image"] = json!(sim::image(HUNG).as_str());
    ir["roles"].as_array_mut().unwrap().push(peer);
    let wait = LateTake {
        behind: 0,
        at: Duration::ZERO,
    };
    let host = SimHost::new().during_setup(wait, interrupt);
    let dir = TestDir::new();
    let (record, result) = execute_agents(&host, 2, ir, &dir, None);
    assert!(matches!(result, Err(runtime::Stopped::Interrupted(_))));
    assert!(!record.join("launched").exists());
}

#[test]
fn final_body_reads_submitted_bytes_after_one_agent_fails_to_start() {
    let mut ir = submissions_workflow(&["ops", "call train 1", "call submit binary finalize=true"]);
    let roles = ir["roles"].as_array_mut().unwrap();
    let mut broken = roles[0].clone();
    broken["name"] = json!("broken");
    broken["image"] = json!(sim::image(sim::HUNG_LAST).as_str());
    // A startup shorter than the other agents', at which its create's record ends.
    broken["needs"]["cpu_model"] = json!(BRIEF);
    let startup = SlowSync {
        record: Synced::Holding("agents/broken/.sandbox.json.partial", "sim-"),
        queued: 0,
        at: Duration::from_secs(1),
    };
    let mut beta = roles[0].clone();
    beta["name"] = json!("beta");
    beta["runtime"]["agent"]["command"]["argv"] = json!([
        "ops",
        "ops",
        "await_log start_call",
        "status 0",
        "call submit beta finalize=true"
    ]);
    roles.extend([broken, beta]);
    let dir = TestDir::new();
    let (record, agents, result) = judged(&SimHost::new().slow_sync(startup), ir, &dir);
    assert!(agents[0].submission.as_ref().unwrap().finalized);
    assert!(matches!(
        agents[1].work_end,
        decision::WorkEnd::AgentFailed(_)
    ));
    assert!(agents[1].submission.is_none());
    assert!(agents[2].submission.as_ref().unwrap().finalized);
    let [(files, bytes)] = result.values() else {
        panic!("unexpected final result {result:?}")
    };
    assert_eq!(files.as_str(), "files");
    assert_eq!(
        bytes,
        &json!({"alpha":[0,255,1], "broken":null, "beta":[98,101,116,97]})
    );
    let log = read_log(&record);
    let final_job = log
        .iter()
        .flat_map(|line| line["actions"].as_array().into_iter().flatten())
        .find(|action| action["kind"] == "start_final_call")
        .unwrap()["job"]
        .clone();
    let puts: Vec<_> = log
        .iter()
        .map(|line| &line["event"])
        .filter(|event| event["job"] == final_job && event["kind"] == "target_put")
        .map(|event| event["path"].as_str().unwrap())
        .collect();
    assert_eq!(puts, ["/work/submission", "/work/submission"]);
    let status = log
        .iter()
        .find(|line| line.pointer("/event/request/kind") == Some(&json!("status")))
        .unwrap();
    assert_eq!(status["event"]["caller"], "agent-4");
    assert_eq!(status["actions"][0]["reply"]["kind"], "refused");
    for role in ["alpha", "broken", "beta"] {
        let sandbox = fs::read(record.join("agents").join(role).join("sandbox.json")).unwrap();
        let sandbox: Value = serde_json::from_slice(&sandbox).unwrap();
        assert_eq!(sandbox["delete"]["state"], "deleted");
    }
}

#[test]
fn a_due_grace_does_not_advance_past_another_agents_queued_submission() {
    const STORING_ALPHA: &str =
        "payloads/.8ed3f6ad685b959ead7022518e1af76cd816f8e8ec7ccdda1ed4018e8f2223f8.partial";
    let mut ir = submissions_workflow(&[
        "ops",
        "await_log agent_started agent-0",
        "await_log ops",
        "call submit alpha finalize=true",
    ]);
    let roles = ir["roles"].as_array_mut().unwrap();
    roles[0]["runtime"]["agent"]["finalize_grace_seconds"] = json!(1);
    let mut beta = roles[0].clone();
    beta["name"] = json!("beta");
    beta["runtime"]["agent"]["command"]["argv"] = json!([
        "ops",
        "await_log agent_started agent-3",
        "ops",
        format!("await_file {STORING_ALPHA}"),
        "call submit beta"
    ]);
    roles.push(beta);

    // Alpha finalizes at 0. While the driver stores its submission, beta submits at 2. The
    // driver resumes at 31, past alpha's grace at 1 and the work deadline at 30.
    let host = SimHost::new()
        .slow_sync(SlowSync {
            record: Synced::Holding("log.jsonl", r#""caller":"agent-0","kind":"op_arrived""#),
            at: Duration::from_secs(2),
            queued: 0,
        })
        .slow_sync(SlowSync {
            record: Synced::Holding(STORING_ALPHA, "alpha"),
            at: Duration::from_secs(31),
            queued: 1,
        });
    let dir = TestDir::new();
    let (record, outcome) = execute_agents(&host, 2, ir, &dir, None);
    let Ok(Outcome { agents, .. }) = outcome else {
        panic!("run failed: {outcome:?}")
    };
    assert!(matches!(agents[0].work_end, decision::WorkEnd::Finalized));
    assert_eq!(agents[1].submission.as_ref().unwrap().payload.size, 4);
    let log = read_log(&record);
    let now_op = |caller: &str| {
        let made = |line: &Value| {
            line["event"]["caller"] == caller && line["event"]["request"]["kind"] == "now"
        };
        log.iter().position(made).unwrap()
    };
    let (finalized, queued) = (now_op("agent-0"), now_op("agent-3"));
    let tick = (log.iter())
        .position(|line| line["event"]["kind"] == "tick")
        .unwrap();
    assert_eq!(log[finalized]["observed_ns"], "0");
    assert_eq!(log[tick]["observed_ns"], "1000000000");
    assert_eq!(log[queued]["observed_ns"], "2000000000");
    assert!(finalized < tick && tick < queued);
}

#[test]
fn judge_waits_for_failed_agent_deletion() {
    let mut ir = submissions_workflow(&["ops", "call submit binary finalize=true"]);
    let roles = ir["roles"].as_array_mut().unwrap();
    let mut broken = roles[0].clone();
    broken["name"] = json!("broken");
    broken["image"] = json!(sim::image(sim::HUNG_REFUSED).as_str());
    // A startup shorter than the other agent's, at which its create's record ends.
    broken["needs"]["cpu_model"] = json!(BRIEF);
    roles.push(broken);
    let startup = SlowSync {
        record: Synced::Holding("agents/broken/.sandbox.json.partial", "sim-"),
        queued: 0,
        at: Duration::from_secs(1),
    };
    let dir = TestDir::new();
    let (record, outcome) = execute_agents(&SimHost::new().slow_sync(startup), 2, ir, &dir, None);
    assert!(
        outcome
            .unwrap_err()
            .to_string()
            .contains("deleting it failed")
    );
    assert!(read_log(&record).iter().all(|line| {
        !line["actions"]
            .as_array()
            .is_some_and(|actions| actions.iter().any(|act| act["kind"] == "start_final_call"))
    }));
}

#[test]
fn judge_sees_missing_inputs_and_each_agent_keeps_its_model_failure_and_usage() {
    let mut ir = routed(submissions_workflow(&["ops", "model m-large expired"]));
    let roles = ir["roles"].as_array_mut().unwrap();
    let mut beta = roles[0].clone();
    beta["name"] = json!("beta");
    beta["runtime"]["agent"]["command"]["argv"] = json!(["ops", "model m-large 7"]);
    roles.push(beta);
    let dir = TestDir::new();
    let (record, agents, result) = judged(&SimHost::new(), ir, &dir);
    let [alpha, beta] = agents.as_slice() else {
        panic!("expected both agents")
    };
    assert!(alpha.route_failure.as_ref().unwrap().contains("its key"));
    assert!(beta.route_failure.is_none());
    assert!(agents.iter().all(|agent| agent.submission.is_none()));
    let [(.., files)] = result.values() else {
        panic!("expected the submissions")
    };
    assert_eq!(files, &json!({"alpha": null, "beta": null}));
    let progress = Record::open(&record)
        .unwrap()
        .progress()
        .unwrap()
        .progress
        .unwrap();
    assert_eq!(progress.agents.len(), 2);
    let usage = |id| {
        progress.agents[&runtime::CallerName::Agent(id)]
            .model
            .as_ref()
            .unwrap()
    };
    assert_eq!(usage(0).requests, 1);
    assert_eq!(usage(3).requests, 1);
    assert_eq!(usage(0).answers.get("refused"), Some(&1));
    assert_eq!(usage(3).answers.get("200"), Some(&1));
}

fn finished(outcome: &Outcome) -> String {
    match &outcome.result {
        Ok(result) => {
            let values = result.values().iter();
            let values: Vec<String> = values
                .map(|(name, value)| format!("{name}={value}"))
                .collect();
            format!("finished, showing [{}]", values.join(" "))
        }
        Err(_) => "finished without a result".to_owned(),
    }
}

fn work_target_workflow(agent: &[&str], body: &str) -> Value {
    let mut ir = submissions_workflow(agent);
    ir["roles"][0]["runtime"]["agent"]["grants"][0]["job"]["body"]["def"] = json!(body);
    ir["roles"][1]["runtime"] = json!({"target": {"lifetime":"work_phase", "cwd":"/work"}});
    ir
}

fn work_and_call_targets() -> Value {
    let mut ir = work_target_workflow(
        &[
            "ops",
            "call train forged",
            "await 0 completed",
            "call submit a finalize=true",
        ],
        "board",
    );
    let mut beta = ir["roles"][0].clone();
    beta["name"] = json!("beta");
    beta["runtime"]["agent"]["command"]["argv"] = json!([
        "ops",
        "await_log agent_collected",
        "call train beta",
        "await 1 completed",
        "call fresh beta",
        "await 2 errored",
        "call submit b finalize=true"
    ]);
    let mut fresh = ir["roles"][1].clone();
    fresh["name"] = json!("fresh");
    fresh["runtime"]["target"]["lifetime"] = json!("per_call");
    let mut grant = beta["runtime"]["agent"]["grants"][0].clone();
    grant["name"] = json!("fresh");
    grant["job"]["target"] = json!("fresh");
    beta["runtime"]["agent"]["grants"]
        .as_array_mut()
        .unwrap()
        .push(grant);
    ir["roles"].as_array_mut().unwrap().extend([beta, fresh]);
    ir
}

#[test]
fn work_target_calls_share_files_and_collect_each_call_once() {
    let dir = TestDir::new();
    let (record, agents, _) = judged(&SimHost::new(), work_and_call_targets(), &dir);
    assert!(agents.iter().all(|agent| agent.submission.is_some()));
    for (job, caller) in ["alpha", "beta"].into_iter().enumerate() {
        let job_dir = record.join(format!("jobs/{job}"));
        assert_eq!(fs::read(job_dir.join("got/1/message")).unwrap(), b"forged");
        assert_eq!(
            fs::read(job_dir.join("log/0/command.stdout")).unwrap(),
            caller.as_bytes()
        );
        assert!(!job_dir.join("sandbox.json").exists());
    }
    let log = read_log(&record);
    assert!(record.join("jobs/2/sandbox.json").is_file());
    assert!(!record.join("jobs/2/got/1/message").exists());
    assert!(
        log.iter()
            .any(|line| line["event"]["kind"] == "call_failed" && line["event"]["job"] == 2)
    );
    let deleted: Vec<_> = log
        .iter()
        .enumerate()
        .filter(|(_, line)| {
            line["event"]["kind"] == "sandbox_deleted" && line["event"]["sandbox"] == "shared-1"
        })
        .map(|(index, _)| index)
        .collect();
    let final_start = log
        .iter()
        .position(|line| {
            line["actions"].as_array().is_some_and(|actions| {
                actions
                    .iter()
                    .any(|action| action["kind"] == "start_final_call")
            })
        })
        .unwrap();
    assert_eq!(deleted.len(), 1);
    assert!(deleted[0] < final_start);
    let sandbox: Value =
        serde_json::from_slice(&fs::read(record.join("shared/trainer/sandbox.json")).unwrap())
            .unwrap();
    assert_eq!(sandbox["delete"]["state"], "deleted");

    // A record that declares the role `shared`, or names the lifetimes `work` and `call`,
    // identifies the same physical sandbox.
    let path = record.join("ir.json");
    let written: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let mut shared = written.clone();
    shared["roles"][1]["runtime"] =
        json!({"shared":{"command":{"argv":["server"],"cwd":"/"},"cwd":"/work"}});
    let mut named = written;
    named["roles"][1]["runtime"]["target"]["lifetime"] = json!("work");
    named["roles"][4]["runtime"]["target"]["lifetime"] = json!("call");
    for old in [shared, named] {
        fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        let progress = Record::open(&record)
            .unwrap()
            .progress()
            .unwrap()
            .progress
            .unwrap();
        assert_eq!(progress.shared[&1].role, "trainer");
        assert_eq!(progress.calls[1].shared, Some(1));
        assert_eq!(progress.calls[2].shared, None);
    }
}

fn capture_workflow() -> Value {
    let mut ir = work_target_workflow(
        &[
            "ops",
            "call train [65,10]",
            "await 0 completed",
            "call train 0",
            "await 1 completed",
            "call absent 0",
            "await 2 completed",
            "call fail [66]",
            "await 3 errored",
            "call train [255]",
            "await 4 errored",
            "call large 0",
            "await 5 errored",
            "call cleared 0",
            "await 6 completed",
            "call submit a finalize=true",
        ],
        "capture",
    );
    let grants = ir["roles"][0]["runtime"]["agent"]["grants"]
        .as_array_mut()
        .unwrap();
    grants[0]["job"]["keywords"] = json!({"argv": ["worker", "0"], "stdin": true});
    for (name, argv, stdin) in [
        ("absent", json!(["worker", "0"]), false),
        ("fail", json!(["worker", "17"]), true),
        ("large", json!(["fill", "1048577"]), false),
        ("cleared", json!(["list", "/pumpkin/.stdin"]), false),
    ] {
        let mut grant = grants[0].clone();
        grant["name"] = json!(name);
        grant["job"]["keywords"] = json!({"argv": argv, "stdin": stdin});
        grants.push(grant);
    }
    ir
}

#[test]
fn captures_complete_worker_replies_on_a_shared_target() {
    let ir = capture_workflow();
    let dir = TestDir::new();
    let (record, _, _) = judged(&SimHost::new(), ir, &dir);
    let log = read_log(&record);
    let replies: Vec<_> = log
        .iter()
        .filter_map(|line| {
            let event = &line["event"];
            match event["kind"].as_str()? {
                "call_returned" => event.pointer("/returned/0/value")?.as_str(),
                "call_failed" => event["reason"].as_str(),
                _ => None,
            }
        })
        .collect();
    expect![[r#"
        [
            "A\n",
            "",
            "absent",
            "command 0 exited with code 17",
            "command 0's stdout is not UTF-8 text",
            "command 0's stdout exceeds the capture limit of 1048576 bytes",
            "",
        ]
    "#]]
    .assert_debug_eq(&replies);
    let job = record.join("jobs/3/log/0");
    assert_eq!(fs::read(job.join("command.stdout")).unwrap(), b"B");
    assert_eq!(
        fs::read(job.join("command.stderr")).unwrap(),
        b"worker diagnostic"
    );
    assert_eq!(
        fs::read(record.join("jobs/4/log/0/command.stdout")).unwrap(),
        [255]
    );
    assert_eq!(
        fs::metadata(record.join("jobs/5/log/0/command.stdout"))
            .unwrap()
            .len(),
        1_048_577
    );
}

#[test]
fn shared_timeout_stops_its_client_before_the_next_call() {
    let mut ir = work_target_workflow(
        &[
            "ops",
            "call train slow",
            "try_call fast busy",
            "await 0 errored",
            "call fast next",
            "await 1 completed",
            "call submit ok finalize=true",
        ],
        "slow",
    );
    let grants = ir["roles"][0]["runtime"]["agent"]["grants"]
        .as_array_mut()
        .unwrap();
    let mut fast = grants[0].clone();
    fast["name"] = json!("fast");
    fast["job"]["body"]["def"] = json!("board");
    grants.push(fast);
    // The clock reaches the slow call's timeout once its command has started.
    let timed_out = SlowSync {
        record: Synced::Holding("log.jsonl", r#""job":0,"kind":"target_run""#),
        queued: 0,
        at: Duration::from_secs(1),
    };
    let dir = TestDir::new();
    let (record, _, _) = judged(&SimHost::new().slow_sync(timed_out), ir, &dir);
    let log = read_log(&record);
    assert!(
        log.iter()
            .any(|line| line["event"]["kind"] == "call_timed_out" && line["event"]["job"] == 0)
    );
    assert!(
        log.iter()
            .all(|line| line["event"]["kind"] != "shared_failed")
    );
    assert!(
        log.iter().any(
            |line| line["actions"]
                .as_array()
                .is_some_and(
                    |actions| actions.iter().any(|action| action["reply"]["message"]
                        .as_str()
                        .is_some_and(|reason| reason.contains("busy")))
                )
        )
    );
    assert_eq!(
        fs::read(record.join("jobs/1/got/1/message")).unwrap(),
        b"next"
    );
}

#[test]
fn shared_call_loses_its_session_after_its_client_exits() {
    let mut ir = work_target_workflow(
        &[
            "ops",
            "call train 1",
            "await 0 errored",
            "call train 2",
            "call submit ok finalize=true",
        ],
        "run",
    );
    ir["roles"][0]["runtime"]["agent"]["grants"][0]["job"]["keywords"] =
        json!({"argv":["exit-close","0"]});
    let dir = TestDir::new();
    let (record, agents, _) = judged(&SimHost::new(), ir, &dir);
    assert!(agents[0].submission.as_ref().unwrap().finalized);
    let log = read_log(&record);
    assert!(
        log.iter()
            .any(|line| line["event"]["kind"] == "call_failed" && line["event"]["job"] == 0)
    );
    assert!(
        log.iter()
            .any(|line| line["event"]["kind"] == "shared_failed")
    );
    assert_eq!(
        log.iter()
            .filter(|line| line["event"]["kind"] == "target_run" && line["event"]["job"] == 0)
            .count(),
        1
    );
}

#[test]
fn shared_start_failure_is_disposed_before_final() {
    let mut ir = work_target_workflow(
        &["ops", "call train 1", "call submit ok finalize=true"],
        "board",
    );
    ir["roles"][1]["image"] = json!(sim::image(MUTE).as_str());
    // Once the start has read the clock past the create's record, the clock moves to its first
    // look, which finds the sandbox ended.
    let created = Synced::Holding("shared/trainer/.sandbox.json.partial", "sim-");
    let host = SimHost::new().moving_after_read(created, Duration::from_secs(1));
    let dir = TestDir::new();
    let (record, agents, _) = judged(&host, ir, &dir);
    assert!(agents[0].submission.as_ref().unwrap().finalized);
    let log = read_log(&record);
    let failure = log
        .iter()
        .find(|line| line["event"]["kind"] == "shared_failed")
        .unwrap();
    assert!(
        failure["event"]["reason"]
            .as_str()
            .unwrap()
            .contains("before it met the gateway")
    );
    let deleted = log
        .iter()
        .position(|line| {
            line["event"]["kind"] == "sandbox_deleted" && line["event"]["sandbox"] == "shared-1"
        })
        .unwrap();
    let final_start = log
        .iter()
        .position(|line| {
            line["actions"].as_array().is_some_and(|actions| {
                actions
                    .iter()
                    .any(|action| action["kind"] == "start_final_call")
            })
        })
        .unwrap();
    assert!(deleted < final_start);
}

#[test]
fn shared_call_interruption_collects_and_deletes_without_starting_the_judge() {
    let mut ir = work_target_workflow(&["ops", "call train 1", "await 0 completed"], "slow");
    ir["roles"][0]["runtime"]["agent"]["grants"][0]["job"]["call_timeout_seconds"] = json!(30);
    let dir = TestDir::new();
    let (record, outcome) = execute_agents(
        &System,
        2,
        ir,
        &dir,
        Some(At::Logged(r#""kind":"target_run""#)),
    );
    assert!(matches!(outcome, Err(runtime::Stopped::Interrupted(_))));
    let deleted: Value =
        serde_json::from_slice(&fs::read(record.join("shared/trainer/sandbox.json")).unwrap())
            .unwrap();
    assert_eq!(deleted["delete"]["state"], "deleted");
    assert!(record.join("shared/trainer/out/collected").is_file());
    assert!(record.join("jobs/0/log/0/collected").is_file());
    assert!(!record.join("jobs/1").exists());
}

#[test]
fn an_unconfirmed_command_stop_closes_the_work_target_to_later_calls() {
    let mut ir = work_target_workflow(
        &[
            "ops",
            "call train 1",
            // Past the call's 1 s timeout, once its command has started.
            "await_log target_run",
            "sleep 1",
            "await_log shared_failed",
            "call train 2",
            "call submit ok finalize=true",
        ],
        "run",
    );
    // The command stalls its sandbox, which holds the stop that follows the call's 1 s timeout
    // until that stop's own deadline, 1 s later.
    let argv = json!({ "argv": ["stall", "2"] });
    ir["roles"][0]["runtime"]["agent"]["grants"][0]["job"]["keywords"] = argv;
    let dir = TestDir::new();
    let (record, agents, _) = judged(&SimHost::new(), ir, &dir);
    assert!(agents[0].submission.as_ref().unwrap().finalized);
    let log = read_log(&record);
    assert!(
        log.iter()
            .any(|line| line["event"]["kind"] == "call_failed" && line["event"]["job"] == 0)
    );
    assert!(
        log.iter().any(
            |line| line["actions"]
                .as_array()
                .is_some_and(
                    |actions| actions.iter().any(|action| action["reply"]["message"]
                        .as_str()
                        .is_some_and(|reason| reason.contains("unavailable")))
                )
        )
    );
    assert_eq!(
        log.iter()
            .filter(|line| line["event"]["kind"] == "target_run" && line["event"]["job"] == 0)
            .count(),
        1
    );
}

fn failed_shared_create(fault: &str) -> Value {
    let mut ir = work_target_workflow(&["ops", "call submit ok finalize=true"], "board");
    ir["roles"][1]["image"] = json!(sim::image(fault).as_str());
    ir
}

#[test]
fn declined_shared_create_has_no_physical_usage() {
    let dir = TestDir::new();
    let (record, _, _) = judged(&SimHost::new(), failed_shared_create(sim::UNCREATED), &dir);
    let progress = Record::open(&record)
        .unwrap()
        .progress()
        .unwrap()
        .progress
        .unwrap();
    assert!(!progress.cost().unwrap().partial());
    let shared = &progress.shared[&1];
    assert!(
        shared
            .failure
            .as_ref()
            .unwrap()
            .contains("declined to create")
    );
    assert!(!record.join("shared/trainer/sandbox.json").exists());
    let usage = progress.sandboxes();
    assert_eq!(usage.values().map(|used| used.count).sum::<u64>(), 2);
    let agent = &progress.agents[&runtime::CallerName::Agent(0)].sandbox;
    let judge = &progress.calls[0].sandbox;
    let lasted = [agent, judge]
        .iter()
        .map(|sandbox| sandbox.gone.unwrap().checked_sub(sandbox.asked).unwrap())
        .sum::<Duration>();
    assert_eq!(
        usage.values().map(|used| used.lasted).sum::<Duration>(),
        lasted
    );
}

#[test]
fn unnamed_shared_create_is_deleted_before_the_judge_starts() {
    let dir = TestDir::new();
    let (record, _, _) = judged(&SimHost::new(), failed_shared_create(LOST), &dir);
    let log = read_log(&record);
    // The failed create logs no create, though its sandbox exists.
    let created = |line: &Value| line["event"]["kind"] == "sandbox_created";
    assert!(
        !log.iter()
            .any(|line| created(line) && line["event"]["sandbox"] == "shared-1")
    );
    let orphan = log
        .iter()
        .position(|line| {
            line["event"]["kind"] == "sandbox_deleted" && line["event"]["sandbox"].is_null()
        })
        .unwrap();
    let judge = log
        .iter()
        .position(|line| line["event"]["kind"] == "call_started")
        .unwrap();
    assert!(orphan < judge);
    let record = Record::open(&record).unwrap();
    let progress = record.progress().unwrap().progress.unwrap();
    assert!(progress.cost().unwrap().partial());
    assert!(record.used().unwrap().unwrap().partial);
    assert_eq!(
        progress
            .sandboxes()
            .values()
            .map(|used| used.count)
            .sum::<u64>(),
        2
    );
}

#[test]
fn interrupt_during_the_pre_final_sweep_skips_the_judge() {
    let dir = TestDir::new();
    let (record, outcome) = execute_agents(
        &System,
        2,
        failed_shared_create(LOST),
        &dir,
        Some(At::FinalSweep),
    );
    assert!(matches!(
        outcome,
        Err(runtime::Stopped::Interrupted(runtime::Signal::Term))
    ));
    assert!(!record.join("jobs/0").exists());
    assert!(read_log(&record).iter().any(|line| {
        line["event"]["kind"] == "sandbox_deleted" && line["event"]["sandbox"].is_null()
    }));
}

#[test]
fn failed_deletion_of_an_unnamed_shared_instance_prevents_the_judge() {
    let dir = TestDir::new();
    let (record, outcome) = execute_agents(
        &SimHost::new(),
        2,
        failed_shared_create(sim::LOST_REFUSED),
        &dir,
        None,
    );
    let Err(runtime::Stopped::Failed(reason)) = outcome else {
        panic!("the judge ran: {outcome:?}")
    };
    assert!(reason.contains("cleaning up work sandboxes before the final call"));
    assert!(!record.join("jobs/0").exists());
}
