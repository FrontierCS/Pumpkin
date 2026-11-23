use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{BufRead as _, BufReader, Read as _, Write as IoWrite};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use expect_test::expect;
use schema::sandbox::RESULT_MAX_BYTES;
use serde_json::{Value, json};

use super::{
    Prices, Scratch, Step, asked, check, cut, lease_places, logged, payload, poll, returned,
    whole_log, write_record, write_run,
};

/// A `pumpkin view` process that listens on loopback, and is killed when dropped.
///
/// `said` is what the process printed as it started, with its port and token replaced by
/// `<port>` and `<token>`.
pub(super) struct Served {
    child: Child,
    pub(super) port: u16,
    token: String,
    pub(super) said: String,
}

impl Served {
    fn start(scratch: &Scratch) -> Self {
        Self::of(scratch.pumpkin(&["view", "--runs", "runs"]))
    }

    /// Starts `command`, which must run `pumpkin view`, and waits for the address it prints.
    pub(super) fn of(mut command: Command) -> Self {
        command.stdout(Stdio::null()).stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let mut said = String::new();
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        while !said.contains("http://127.0.0.1:") {
            assert!(stderr.read_line(&mut said).unwrap() > 0, "{said}");
        }
        let address = said.split_once("http://127.0.0.1:").unwrap().1;
        let (port, token) = address.trim_end().split_once("/?t=").unwrap();
        let said = said.replace(token, "<token>").replace(port, "<port>");
        Self {
            port: port.parse().unwrap(),
            token: token.to_owned(),
            child,
            said,
        }
    }

    fn get_with(&self, path: &str, headers: &[(&str, &str)]) -> (String, String) {
        self.ask("GET", path, headers)
    }

    /// Sends a request with the token and a loopback `Host`, except for a header that `headers`
    /// names; a header whose value is empty is left out. Returns the status line and the body.
    pub(super) fn ask(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> (String, String) {
        let (head, body) = self.exchange(method, path, headers);
        (head.lines().next().unwrap().to_owned(), body)
    }

    fn exchange(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> (String, String) {
        let (head, body) = self.exchange_bytes(method, path, headers);
        (head, String::from_utf8(body).unwrap())
    }

    fn exchange_bytes(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> (String, Vec<u8>) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let mut head = format!("{method} {path} HTTP/1.1\r\n");
        let token = ("X-Pumpkin-Token", self.token.as_str());
        let given = [("Host", "127.0.0.1"), token];
        let named = |name: &str| headers.iter().any(|(given, _)| *given == name);
        for (name, value) in given.iter().filter(|(name, _)| !named(name)).chain(headers) {
            if !value.is_empty() {
                let _ = write!(head, "{name}: {value}\r\n");
            }
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).unwrap();
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).unwrap();
        let at = answer
            .windows(4)
            .position(|end| end == b"\r\n\r\n")
            .unwrap();
        let head = String::from_utf8(answer[..at].to_vec()).unwrap();
        (head, answer[at + 4..].to_vec())
    }

    fn as_frame(&self, target: &str, deflate: bool) -> (Value, Vec<u8>) {
        let accepts: &[(&str, &str)] = if deflate {
            &[("Accept-Encoding", "deflate")]
        } else {
            &[]
        };
        let (head, body) = self.exchange_bytes("GET", target, accepts);
        let header = |name: &str| {
            let value = head.lines().find_map(|line| line.strip_prefix(name));
            value.map(str::to_owned)
        };
        let status = head.lines().next().unwrap();
        let answered = json!({
            "type": "viewed",
            "status": status.strip_prefix("HTTP/1.1 ").unwrap(),
            "content_type": header("Content-Type: ").unwrap(),
            "deflated": header("Content-Encoding: ").as_deref() == Some("deflate"),
            "body_bytes": body.len(),
        });
        (answered, body)
    }

    fn get(&self, path: &str) -> Value {
        let (status, body) = self.get_with(path, &[]);
        assert_eq!(status, "HTTP/1.1 200 OK", "{path}: {body}");
        serde_json::from_str(&body).unwrap()
    }

    fn read_bytes(&self) -> u64 {
        let io = fs::read_to_string(format!("/proc/{}/io", self.child.id())).unwrap();
        let rchar = io.lines().find_map(|line| line.strip_prefix("rchar: "));
        rchar.unwrap().parse().unwrap()
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

/// The fields of `value`, one on each line as `name: json`, but for each array field that
/// `lines` names, which has one line for each element.
fn fields(value: &Value, lines: &[&str]) -> String {
    let mut text = String::new();
    for (name, value) in value.as_object().unwrap() {
        match value.as_array() {
            Some(elements) if lines.contains(&name.as_str()) => {
                let _ = writeln!(text, "{name}:");
                for element in elements {
                    let _ = writeln!(text, "  {element}");
                }
            }
            _ => {
                let _ = writeln!(text, "{name}: {value}");
            }
        }
    }
    text
}

const SERVED_ENDED: &str = "20261110-090000-0000000a";

const SERVED_LIVE: &str = "20261110-110000-0000000c";

/// Writes the runs that the tests of the served page share, and returns the lock on the live
/// run's `driver.pid`, which keeps `SERVED_LIVE` live until it is dropped.
fn write_served(scratch: &Scratch) -> File {
    let priced = lease_places(Prices::Both);
    let ended = SERVED_ENDED;
    let mut outcome = returned(49.36);
    // Its sandboxes all priced, so that its unpriced model alone makes its cost more.
    outcome["usage"] = json!({
        "ran_ns": "1100000000000", "model_requests": 4,
        "tokens": { "responses": 2, "input": 3000, "output": 300, "cached": 2600 },
        "sandboxes": [{ "provider": "aws", "class": "judge", "count": 2, "lasted_ns": "1",
            "usd_per_hour": 0.3672, "usd": 0.11 }],
        "usd": 0.77, "unpriced_models": ["model-b"],
    });
    write_run(
        scratch,
        ended,
        asked(ended, 30),
        &whole_log(),
        Some(outcome),
        &priced,
    );
    let lost = "20261110-100000-0000000b";
    write_run(
        scratch,
        lost,
        asked(lost, 30),
        &cut(&whole_log(), 130),
        None,
        &priced,
    );
    let live = SERVED_LIVE;
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let dir = write_run(
        scratch,
        live,
        since.checked_sub(Duration::from_secs(200)).unwrap(),
        &cut(&whole_log(), 130),
        None,
        &priced,
    );
    let driver = File::open(dir.join("driver.pid")).unwrap();
    driver.lock().unwrap();
    // A record with no log.
    write_record(scratch, "20261110-120000-0000000d", true, None);
    // A run whose agent made no model request used no tokens; one whose requests none of the
    // responses reported their tokens for used some that no one counted.
    for (id, requests) in [
        ("20261110-070000-00000008", 0),
        ("20261110-080000-00000009", 3),
    ] {
        let mut outcome = super::unsubmitted();
        outcome["usage"] = json!({
            "ran_ns": "60000000000", "model_requests": requests,
            "tokens": { "responses": 0, "input": 0, "output": 0 }, "sandboxes": [],
        });
        write_record(scratch, id, true, Some(outcome));
    }
    // A run of which a response left tokens unreported, so that its cost is a floor.
    let mut outcome = super::unsubmitted();
    outcome["usage"] = json!({
        "ran_ns": "60000000000", "model_requests": 2,
        "tokens": { "responses": 2, "input": 1000, "output": 100 }, "sandboxes": [],
        "usd": 0.45, "unreported_responses": 1,
    });
    write_record(scratch, "20261110-083000-0000000c", true, Some(outcome));
    // A directory with no record.
    fs::create_dir_all(scratch.0.join("runs/20261110-130000-0000000e")).unwrap();
    // A file among the runs.
    fs::write(scratch.0.join("runs/20261110-131000-00000013"), "").unwrap();
    driver
}

fn write_team(scratch: &Scratch, id: &str) {
    let mut ir = super::lease_task();
    let mut peer = ir["roles"][0].clone();
    peer["name"] = json!("peer");
    peer["runtime"]["agent"]["grants"][0]["job"]["target"] = json!("tester");
    let mut target = ir["roles"][1].clone();
    target["name"] = json!("tester");
    target["runtime"]["target"]
        .as_object_mut()
        .unwrap()
        .remove("sessions");
    ir["roles"].as_array_mut().unwrap().extend([peer, target]);
    let selected = payload();
    let final_call = json!({"kind":"start_final_call","job":2,"timeout_ns":"3000000000",
        "selections":[{"role":"solver","selection":null},{"role":"peer","selection":selected}]});
    let call = |caller, job| {
        json!({"kind":"start_call","grant":"train","caller":caller,"job":job,
        "payload":selected,"timeout_ns":"3000000000"})
    };
    let log = [
        logged(1, json!({"kind":"work_started"}), json!([])),
        logged(
            2,
            json!({"kind":"tick"}),
            json!([call("agent-0", 0), call("agent-6", 1)]),
        ),
        logged(3, json!({"kind":"tick"}), json!([final_call])),
        logged(4, json!({"kind":"tick"}), json!([{"kind":"finish"}])),
    ];
    let outcome = json!({"state":"finished","agents":[
        {"role":"solver","work_end":{"kind":"deadline"},"submission":null},
        {"role":"peer","work_end":{"kind":"finalized"},"submission":super::submission()}
    ],"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":1}]}}});
    write_run(
        scratch,
        id,
        asked(id, 1),
        &log,
        Some(outcome),
        &lease_places(Prices::Nothing),
    );
    scratch.write(&format!("runs/{id}/ir.json"), &ir);
}

#[test]
fn team_roles() {
    let scratch = Scratch::new();
    let id = SERVED_ENDED;
    write_team(&scratch, id);
    let served = Served::start(&scratch);
    let path = format!("/api/runs/{id}");
    let run = served.get(&path);
    let lanes = run["lanes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|lane| json!([lane["caller"], lane["role"], lane["holder"]]))
        .collect::<Vec<_>>();
    let edges = run["talk"]["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| json!([edge["from"], edge["to"], edge["kind"], edge["bytes"]]))
        .collect::<Vec<_>>();
    let traces = ["agent", "agent-0", "agent-6", "agent-1", "agent-06"].map(|caller| {
        let (status, _) = served.get_with(&format!("{path}/trace/{caller}/ops"), &[]);
        format!("{caller}: {status}")
    });
    let text = format!(
        "lanes {}\nedges {}\nresult {}\n{}",
        json!(lanes),
        json!(edges),
        run["result"],
        traces.join("\n")
    );
    check(
        &scratch,
        [Step::Ran(text)],
        false,
        &[],
        &expect![[r#"
            lanes [["agent-0","solver",null],["agent-6","peer",null],["job-0","trainer","agent 0"],["job-1","tester","agent 6"],["job-2","scorer",null]]
            edges [["peer","scorer","final call",3],["peer","tester","job call",3],["solver","trainer","job call",3]]
            result {"values":[{"name":"score","value":1}]}
            agent: HTTP/1.1 400 Bad Request
            agent-0: HTTP/1.1 200 OK
            agent-6: HTTP/1.1 200 OK
            agent-1: HTTP/1.1 404 Not Found
            agent-06: HTTP/1.1 400 Bad Request"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the requests and their snapshot"
)]
fn serves_runs() {
    let scratch = Scratch::new();
    let (ended, live) = (SERVED_ENDED, SERVED_LIVE);
    let driver = write_served(&scratch);
    let others = [
        ("20261110-060000-00000005", Prices::Tokens),
        ("20261110-050000-00000006", Prices::Sandboxes),
        ("20261110-040000-00000007", Prices::Nothing),
    ];
    for (id, prices) in others {
        let outcome = returned(49.36);
        let places = lease_places(prices);
        write_run(
            &scratch,
            id,
            asked(id, 30),
            &whole_log(),
            Some(outcome),
            &places,
        );
    }
    // A run of a later build, whose record this one can't read.
    scratch.write(
        "runs/20261110-125000-00000010/record.json",
        &json!({
            "record_version": "pumpkin.record/v9",
            "pumpkin": { "version": "9.9.9", "commit": null },
            "args": [],
        }),
    );
    // A run whose record the kernel won't let the view read.
    let denied = "20261110-124000-00000011";
    write_record(&scratch, denied, false, None);
    let record = scratch.0.join(format!("runs/{denied}/record.json"));
    fs::set_permissions(record, Permissions::from_mode(0o000)).unwrap();
    let served = Served::start(&scratch);
    let mut steps = vec![format!(
        "pumpkin view --runs runs\n{}",
        served.said.trim_end()
    )];
    let mut listed = served.get("/api/runs");
    for row in listed["runs"].as_array_mut().unwrap() {
        if row["id"] == live {
            row["time"] = json!("<t>");
            row["rest"] = json!(row["rest"].as_str().unwrap().replace(char::is_numeric, "#"));
        }
    }
    steps.push(format!("GET /api/runs\n{}", fields(&listed, &["runs"])));
    let page = served.get(&format!("/api/runs?before={live}"));
    steps.push(format!("GET /api/runs?before=<live>\n{page}"));
    let mut run = served.get(&format!("/api/runs/{ended}"));
    let waited = scratch
        .pumpkin(&["wait", "--runs", "runs", ended])
        .output()
        .unwrap();
    let summary = run["summary"].as_str().unwrap();
    assert!(String::from_utf8_lossy(&waited.stderr).contains(&format!("pumpkin: {summary}")));
    run["summary"] = json!("<as `wait` says it>");
    steps.push(format!(
        "GET /api/runs/<ended>\n{}",
        fields(&run, &["lanes"])
    ));
    for (id, _) in others {
        let cost = &served.get(&format!("/api/runs/{id}"))["cost"];
        steps.push(format!("GET /api/runs/{id}: its cost: {cost}"));
    }
    let end = run["end"].as_u64().unwrap();
    let since = served.get(&format!("/api/runs/{ended}/since?log={end}"));
    steps.push(format!("GET /api/runs/<ended>/since?log=<end>\n{since}"));
    let grown = served.get(&format!("/api/runs/{ended}/since?log=0"));
    let records = grown["records"].as_array().unwrap();
    steps.push(format!(
        "GET /api/runs/<ended>/since?log=0\nend {}, live {}, next {}, run {}, {} records, the \
         first {}",
        grown["end"],
        grown["live"],
        grown["next"],
        grown["run"]["id"],
        records.len(),
        records[0]
    ));
    let window = |query: &str| {
        let window = served.get(&format!("/api/runs/{ended}/log?{query}"));
        let records = window["records"].as_array().unwrap();
        let bytes: Vec<&Value> = records.iter().map(|record| &record["byte"]).collect();
        let kind = records.last().map(|record| &record["event"]["kind"]);
        format!(
            "GET /api/runs/<ended>/log?{query}\nnext {}, end {}, bytes {bytes:?}, the last's \
             event {}",
            window["next"],
            window["end"],
            kind.unwrap_or(&Value::Null)
        )
    };
    steps.push(window(&format!("at={}", records[50]["byte"])));
    let refusals: [(&str, &[(&str, &str)]); 10] = [
        ("/api/runs", &[("X-Pumpkin-Token", "")]),
        ("/api/runs", &[("X-Pumpkin-Token", "0123")]),
        ("/api/runs", &[("Host", "pumpkin.example:80")]),
        ("/api/runs/run-1", &[]),
        ("/api/runs/20261110-140000-0000000f", &[]),
        ("/api/runs/20261110-130000-0000000e", &[]),
        ("/api/runs/20261110-131000-00000013", &[]),
        ("/api/runs/20261110-125000-00000010", &[]),
        ("/api/runs/20261110-124000-00000011", &[]),
        ("/api/runs/20261110-090000-0000000a/since?log=7", &[]),
    ];
    for (path, headers) in refusals {
        let (status, body) = served.get_with(path, headers);
        steps.push(format!("GET {path} {headers:?}\n{status}\n{body}"));
    }
    drop(driver);
    let shown = [(ended, "<ended>"), (live, "<live>")];
    check(
        &scratch,
        steps.into_iter().map(Step::Ran),
        false,
        &shown,
        &expect![[r#"
            pumpkin view --runs runs
            pumpkin: serving the runs in runs to this machine alone, until stopped, at
                http://127.0.0.1:<port>/?t=<token>
            GET /api/runs
            next: null
            runs:
              {"bound":"","has_result":false,"id":"20261110-125000-00000010","live":false,"requests":null,"rest":"made by pumpkin 9.9.9","state":"unread","time":"-","tokens":null,"usd":"-"}
              {"error":"runs/20261110-124000-00000011/record.json: Permission denied (os error 13)","id":"20261110-124000-00000011"}
              {"bound":"-","has_result":false,"id":"20261110-120000-0000000d","live":false,"requests":null,"rest":"-","state":"lost","time":"-","tokens":null,"usd":"-"}
              {"bound":"of 30m","has_result":false,"id":"<live>","live":true,"requests":3,"rest":"example/lease: # agent working","state":"work","time":"<t>","tokens":{"cached":2600,"input":3000,"output":300},"usd":"$0.73+"}
              {"bound":"-","has_result":false,"id":"20261110-100000-0000000b","live":false,"requests":3,"rest":"example/lease","state":"lost","time":"2m10s","tokens":{"cached":2600,"input":3000,"output":300},"usd":"$0.73+"}
              {"bound":"-","has_result":true,"id":"<ended>","live":false,"requests":4,"rest":"example/lease: score=49.36 metrics={\"solved\":3}","state":"finished","time":"18m20s","tokens":{"cached":2600,"input":3000,"output":300},"usd":"$0.77+"}
              {"bound":"-","has_result":false,"id":"20261110-083000-0000000c","live":false,"requests":2,"rest":"-","state":"finished","time":"1m00s","tokens":{"cached":null,"input":1000,"output":100},"usd":"$0.45+"}
              {"bound":"-","has_result":false,"id":"20261110-080000-00000009","live":false,"requests":3,"rest":"-","state":"finished","time":"1m00s","tokens":{"cached":null,"input":null,"output":null},"usd":"-"}
              {"bound":"-","has_result":false,"id":"20261110-070000-00000008","live":false,"requests":0,"rest":"-","state":"finished","time":"1m00s","tokens":{"cached":null,"input":0,"output":0},"usd":"-"}
              {"bound":"-","has_result":true,"id":"20261110-060000-00000005","live":false,"requests":4,"rest":"example/lease: score=49.36 metrics={\"solved\":3}","state":"finished","time":"18m20s","tokens":{"cached":2600,"input":3500,"output":350},"usd":"$0.66+"}
              {"bound":"-","has_result":true,"id":"20261110-050000-00000006","live":false,"requests":4,"rest":"example/lease: score=49.36 metrics={\"solved\":3}","state":"finished","time":"18m20s","tokens":{"cached":2600,"input":3500,"output":350},"usd":"$0.45+"}
              {"bound":"-","has_result":true,"id":"20261110-040000-00000007","live":false,"requests":4,"rest":"example/lease: score=49.36 metrics={\"solved\":3}","state":"finished","time":"18m20s","tokens":{"cached":2600,"input":3500,"output":350},"usd":"-"}

            GET /api/runs?before=<live>
            {"next":null,"runs":[{"bound":"-","has_result":false,"id":"20261110-100000-0000000b","live":false,"requests":3,"rest":"example/lease","state":"lost","time":"2m10s","tokens":{"cached":2600,"input":3000,"output":300},"usd":"$0.73+"},{"bound":"-","has_result":true,"id":"<ended>","live":false,"requests":4,"rest":"example/lease: score=49.36 metrics={\"solved\":3}","state":"finished","time":"18m20s","tokens":{"cached":2600,"input":3000,"output":300},"usd":"$0.77+"},{"bound":"-","has_result":false,"id":"20261110-083000-0000000c","live":false,"requests":2,"rest":"-","state":"finished","time":"1m00s","tokens":{"cached":null,"input":1000,"output":100},"usd":"$0.45+"},{"bound":"-","has_result":false,"id":"20261110-080000-00000009","live":false,"requests":3,"rest":"-","state":"finished","time":"1m00s","tokens":{"cached":null,"input":null,"output":null},"usd":"-"},{"bound":"-","has_result":false,"id":"20261110-070000-00000008","live":false,"requests":0,"rest":"-","state":"finished","time":"1m00s","tokens":{"cached":null,"input":0,"output":0},"usd":"-"},{"bound":"-","has_result":true,"id":"20261110-060000-00000005","live":false,"requests":4,"rest":"example/lease: score=49.36 metrics={\"solved\":3}","state":"finished","time":"18m20s","tokens":{"cached":2600,"input":3500,"output":350},"usd":"$0.66+"},{"bound":"-","has_result":true,"id":"20261110-050000-00000006","live":false,"requests":4,"rest":"example/lease: score=49.36 metrics={\"solved\":3}","state":"finished","time":"18m20s","tokens":{"cached":2600,"input":3500,"output":350},"usd":"$0.45+"},{"bound":"-","has_result":true,"id":"20261110-040000-00000007","live":false,"requests":4,"rest":"example/lease: score=49.36 metrics={\"solved\":3}","state":"finished","time":"18m20s","tokens":{"cached":2600,"input":3500,"output":350},"usd":"-"}]}
            GET /api/runs/<ended>
            again: {"command":"pumpkin run --runs <scratch>/runs <scratch>/task.star"}
            cost: {"said":"$1.11, $0.66 of it for tokens and $0.45 for sandboxes, and more without a price, and for 1 response's unreported tokens","shown":"$1.11+"}
            end: 10332
            failure: null
            failures: [{"at":260.0,"evidence":[],"mark":{"at":260.0,"between":["solver","trainer"],"lane":"job 1","record":5592,"role":"trainer"},"reason":"the job's sandbox didn't start: Modal refused the helper's `create`: ResourceExhaustedError: Insufficient capacity to create sandbox: fix what Modal reports","state":"failed","subject":{"job":1,"kind":"call"}},{"at":1020.0,"evidence":[],"mark":{"at":1020.0,"between":["scorer","model"],"lane":"job 4","record":9561,"role":"scorer"},"reason":null,"state":"failed","subject":{"caller":"job-4","kind":"request","request":3,"upstream":null}}]
            has_result: true
            id: "<ended>"
            lanes:
              {"answered":[10.0,20.0,31.0],"answers":{"count":3,"failed":0},"caller":"agent-0","failed":[],"kind":"agent","lane":"agent 0","model":{"dropped_headers":[],"failed":[],"first_token":{"count":2,"median":1.375},"in_flight":[],"more_dropped_headers":false,"not_2xx":0,"requests":3,"tokens":{"cached":2600,"input":3000,"output":300},"took":{"count":3,"median":5.0}},"role":"solver","said":"solver on aws/judge, started at 0:05, exited 0 at 16:40; its work ended at 16:40","sandbox":{"asked":0.0,"deleted":true,"gone":1002.0,"place":"aws/judge","started":5.0}}
              {"answered":[],"answers":{"count":0,"failed":0},"caller":"job-0","failed":[],"holder":"agent 0","kind":"target","lane":"job 0","model":{"dropped_headers":[],"failed":[],"first_token":{"count":0,"median":null},"in_flight":[],"more_dropped_headers":false,"not_2xx":0,"requests":0,"tokens":{"cached":null,"input":0,"output":0},"took":{"count":0,"median":null}},"of":{"function":"train"},"role":"trainer","said":"train job 0 of agent 0 on modal/a100_80 with unknown GPUs, started after 6s, ran 54s: score=49.53","sandbox":{"asked":40.0,"deleted":true,"gone":110.0,"place":"modal/a100_80 with unknown GPUs","started":46.0}}
              {"grant":"box","holder":"job 0","kind":"runner","lane":"lease 0","role":"box","said":"box lease 0 of job 0 on aws/box, 52s: charged 45s, 5s of it busy, 2 commands, the last `sh -c set -e\\nprint 7` exited 0; closed as its call ended","sandbox":{"asked":50.0,"deleted":true,"gone":102.0,"place":"aws/box","started":55.0},"work":[{"end":null,"from":56.0,"kind":"put","label":"/pumpkin/in/payload → /work/in","to":null},{"end":{"exited":{"code":0}},"from":58.0,"kind":"command","label":"sh -c set -e\nprint 7","to":62.0}]}
              {"grant":"gpu","holder":"agent 0","kind":"runner","lane":"lease 1","role":"gpu","said":"gpu lease 1 of agent 0 on modal/a100_80 with NVIDIA A100-SXM4-80GB, 25s: charged 14s, 2s of it busy, 1 command, the last `nvidia-smi` exited 0; closed by its holder","sandbox":{"asked":120.0,"deleted":true,"gone":145.0,"place":"modal/a100_80 with NVIDIA A100-SXM4-80GB","started":126.0},"work":[{"end":{"exited":{"code":0}},"from":130.0,"kind":"command","label":"nvidia-smi","to":132.0}]}
              {"grant":"cpu","holder":"agent 0","kind":"runner","lane":"lease 2","role":"cpu","said":"cpu lease 2 of agent 0 on aws/judge, 14m45s: charged 14m10s, 30s of it busy, 1 command, the last `sleep 60` timed out; closed as the work phase ended","sandbox":{"asked":120.0,"deleted":true,"gone":1005.0,"place":"aws/judge","started":150.0},"work":[{"end":"timed_out","from":151.0,"kind":"command","label":"sleep 60","to":181.0}]}
              {"answered":[],"answers":{"count":0,"failed":0},"caller":"job-1","failed":[],"failure":{"at":260.0,"evidence":[],"mark":{"at":260.0,"between":["solver","trainer"],"lane":"job 1","record":5592,"role":"trainer"},"reason":"the job's sandbox didn't start: Modal refused the helper's `create`: ResourceExhaustedError: Insufficient capacity to create sandbox: fix what Modal reports","state":"failed","subject":{"job":1,"kind":"call"}},"holder":"agent 0","kind":"target","lane":"job 1","model":{"dropped_headers":[],"failed":[],"first_token":{"count":0,"median":null},"in_flight":[],"more_dropped_headers":false,"not_2xx":0,"requests":0,"tokens":{"cached":null,"input":0,"output":0},"took":{"count":0,"median":null}},"of":{"function":"train"},"role":"trainer","said":"train job 1 of agent 0 on modal/a100_80, never started, ended after 1m: failed: the job's sandbox didn't start: Modal refused the helper's `create`: ResourceExhaustedError: Insufficient capacity to create sandbox: fix what Modal reports","sandbox":{"asked":200.0,"deleted":null,"gone":260.0,"place":"modal/a100_80","started":null}}
              {"answered":[],"answers":{"count":0,"failed":0},"caller":"job-2","failed":[],"holder":"agent 0","kind":"target","lane":"job 2","model":{"dropped_headers":[],"failed":[],"first_token":{"count":0,"median":null},"in_flight":[],"more_dropped_headers":false,"not_2xx":0,"requests":0,"tokens":{"cached":null,"input":0,"output":0},"took":{"count":0,"median":null}},"of":{"function":"train"},"role":"trainer","said":"train job 2 of agent 0 on modal/a100_80 with NVIDIA A100-SXM4-80GB, started after 10s, ran 20s: failed: the call was stopped (stopped)","sandbox":{"asked":300.0,"deleted":true,"gone":335.0,"place":"modal/a100_80 with NVIDIA A100-SXM4-80GB","started":310.0}}
              {"answered":[],"answers":{"count":0,"failed":0},"caller":"job-3","failed":[],"holder":"agent 0","kind":"target","lane":"job 3","model":{"dropped_headers":[],"failed":[],"first_token":{"count":0,"median":null},"in_flight":[],"more_dropped_headers":false,"not_2xx":0,"requests":0,"tokens":{"cached":null,"input":0,"output":0},"took":{"count":0,"median":null}},"of":{"function":"train"},"role":"trainer","said":"train job 3 of agent 0 on modal/a100_80 with unknown GPUs, started after 5s, ran 10s: a result (timed out)","sandbox":{"asked":400.0,"deleted":true,"gone":415.0,"place":"modal/a100_80 with unknown GPUs","started":405.0}}
              {"grant":"gpu","holder":"agent 0","kind":"runner","lane":"lease 3","role":"gpu","said":"gpu lease 3 of agent 0 on modal/a100_80 with NVIDIA A100 80GB PCIe (its role needs NVIDIA A100-SXM4-80GB), 1m10s: charged 1m, 1s of it busy, 1 command, the last `true` exited 0; expired","sandbox":{"asked":500.0,"deleted":true,"gone":570.0,"place":"modal/a100_80 with NVIDIA A100 80GB PCIe (its role needs NVIDIA A100-SXM4-80GB)","started":505.0},"work":[{"end":{"exited":{"code":0}},"from":506.0,"kind":"command","label":"true","to":507.0}]}
              {"answered":[1020.0],"answers":{"count":1,"failed":1},"caller":"job-4","failed":[1020.0],"kind":"target","lane":"job 4","model":{"dropped_headers":["x-stainless-lang","x-title"],"failed":[{"code":"rate_limit_exceeded","count":1}],"first_token":{"count":0,"median":null},"in_flight":[],"more_dropped_headers":false,"not_2xx":0,"requests":1,"tokens":{"cached":null,"input":500,"output":50},"took":{"count":0,"median":null}},"of":"final","role":"scorer","said":"final call job 4 on modal/a100_80 with NVIDIA A100-SXM4-80GB, started after 4s, ran 1m30s: score=49.36 metrics={\"solved\":3}","sandbox":{"asked":1006.0,"deleted":true,"gone":1100.0,"place":"modal/a100_80 with NVIDIA A100-SXM4-80GB","started":1010.0}}
            live: false
            log_epoch: 0
            model: {"dropped_headers":["x-stainless-lang","x-title"],"failed":[{"code":"rate_limit_exceeded","count":1}],"first_token":{"count":2,"median":1.375},"in_flight":[],"more_dropped_headers":false,"not_2xx":0,"requests":4,"tokens":{"cached":2600,"input":3500,"output":350},"took":{"count":3,"median":5.0}}
            other_build: null
            result: {"values":[{"name":"score","value":49.36},{"name":"metrics","value":{"solved":3}}]}
            stage: null
            state: "finished"
            summary: "<as `wait` says it>"
            talk: {"edges":[{"back_bytes":null,"bytes":null,"count":1,"from":"scorer","kind":"model request","to":"model"},{"back_bytes":null,"bytes":null,"count":1,"from":"solver","kind":"command","to":"cpu"},{"back_bytes":null,"bytes":null,"count":1,"from":"solver","kind":"lease","to":"cpu"},{"back_bytes":null,"bytes":null,"count":2,"from":"solver","kind":"command","to":"gpu"},{"back_bytes":null,"bytes":null,"count":2,"from":"solver","kind":"lease","to":"gpu"},{"back_bytes":null,"bytes":null,"count":3,"from":"solver","kind":"model request","to":"model"},{"back_bytes":null,"bytes":3,"count":1,"from":"solver","kind":"final call","to":"scorer"},{"back_bytes":null,"bytes":12,"count":4,"from":"solver","kind":"job call","to":"trainer"},{"back_bytes":null,"bytes":null,"count":1,"from":"trainer","kind":"command","to":"box"},{"back_bytes":null,"bytes":null,"count":1,"from":"trainer","kind":"lease","to":"box"},{"back_bytes":null,"bytes":1,"count":1,"from":"trainer","kind":"tree","to":"box"}],"grants":[{"from":"solver","kind":"job","to":"trainer"},{"from":"solver","kind":"lease","to":"gpu"},{"from":"solver","kind":"lease","to":"cpu"},{"from":"trainer","kind":"lease","to":"box"},{"from":"solver","kind":"final","to":"scorer"}],"roles":[{"final_target":false,"kind":"agent","name":"solver","place":"aws/judge","routed":true,"routes":[{"api":"responses","models":{"model-a":"vendor/model-a"},"upstream":"gateway"}]},{"final_target":false,"kind":"target","lifetime":"call","name":"trainer","place":"modal/a100_80","routed":false,"routes":[]},{"final_target":true,"kind":"target","lifetime":"call","name":"scorer","place":"modal/a100_80","routed":true,"routes":[{"api":"responses","models":{"model-b":"vendor/model-b"},"upstream":"gateway"}]},{"final_target":false,"kind":"runner","name":"gpu","place":"modal/a100_80","routed":false,"routes":[]},{"final_target":false,"kind":"runner","name":"cpu","place":"aws/judge","routed":false,"routes":[]},{"final_target":false,"kind":"runner","name":"box","place":"aws/box","routed":false,"routes":[]}]}
            task: "example/lease"
            times: {"end":1100.0,"hard_end":7200.0,"last":1100.0,"now":null,"started_unix":1794301230.0}
            unread: null
            verdict: "Run <ended> finished with a result (score=49.36 metrics={\"solved\":3})"

            GET /api/runs/20261110-060000-00000005: its cost: {"said":"$0.66 for tokens, and more for sandboxes, which lack a price, and for 1 response's unreported tokens","shown":"$0.66+"}
            GET /api/runs/20261110-050000-00000006: its cost: {"said":"$0.45 for sandboxes, and more for tokens, which lack a price","shown":"$0.45+"}
            GET /api/runs/20261110-040000-00000007: its cost: null
            GET /api/runs/<ended>/since?log=<end>
            {"end":10332,"live":false}
            GET /api/runs/<ended>/since?log=0
            end 10332, live false, next 10332, run "<ended>", 69 records, the first {"actions":[{"kind":"start_agents"}],"at_ns":"0","byte":0,"event":{"hard_end_ns":"7200000000000","kind":"run_started","unix_ns":"1794301230000000000"},"seq":0,"when":0.0}
            GET /api/runs/<ended>/log?at=7566
            next 10332, end 10332, bytes [Number(7566), Number(7655), Number(7843), Number(7975), Number(8063), Number(8241), Number(8371), Number(8474), Number(8562), Number(8759), Number(8861), Number(8950), Number(9224), Number(9355), Number(9443), Number(9561), Number(9951), Number(10124), Number(10225)], the last's event "call_torn_down"
            GET /api/runs [("X-Pumpkin-Token", "")]
            HTTP/1.1 401 Unauthorized
            {"error":"the request carries no token in `X-Pumpkin-Token`: give the one in the address that `pumpkin view` printed as it started"}
            GET /api/runs [("X-Pumpkin-Token", "0123")]
            HTTP/1.1 401 Unauthorized
            {"error":"the request carries a wrong token in `X-Pumpkin-Token`: give the one in the address that `pumpkin view` printed as it started"}
            GET /api/runs [("Host", "pumpkin.example:80")]
            HTTP/1.1 403 Forbidden
            {"error":"the request names the host `pumpkin.example:80`: expected one of 127.0.0.1, localhost, [::1], this machine's loopback, at which the view is served"}
            GET /api/runs/run-1 []
            HTTP/1.1 404 Not Found
            {"error":"no run that this view serves matches `run-1`: name the id of one that its list shows"}
            GET /api/runs/20261110-140000-0000000f []
            HTTP/1.1 404 Not Found
            {"error":"no run that this view serves matches `20261110-140000-0000000f`: name the id of one that its list shows"}
            GET /api/runs/20261110-130000-0000000e []
            HTTP/1.1 404 Not Found
            {"error":"no run that this view serves matches `20261110-130000-0000000e`: name the id of one that its list shows"}
            GET /api/runs/20261110-131000-00000013 []
            HTTP/1.1 404 Not Found
            {"error":"no run that this view serves matches `20261110-131000-00000013`: name the id of one that its list shows"}
            GET /api/runs/20261110-125000-00000010 []
            HTTP/1.1 404 Not Found
            {"error":"runs/20261110-125000-00000010/record.json holds record_version \"pumpkin.record/v9\": expected \"pumpkin.record/v1\", which this `pumpkin` reads"}
            GET /api/runs/20261110-124000-00000011 []
            HTTP/1.1 500 Internal Server Error
            {"error":"runs/20261110-124000-00000011/record.json: Permission denied (os error 13)"}
            GET /api/runs/<ended>/since?log=7 []
            HTTP/1.1 400 Bad Request
            {"error":"byte 7 starts no line of run <ended>'s log, which is read to byte 10332: expected 0, or a byte that an answer gave"}"#]],
    );
}

/// A `pumpkin controller serve` process, which answers a request for the view in each frame of
/// its standard input.
pub(super) struct Piped {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Piped {
    /// Starts it as the user whose home is the scratch directory, and reads its hello.
    pub(super) fn start(scratch: &Scratch) -> Self {
        let mut command = scratch.pumpkin(&["controller", "serve", "--member", "alice"]);
        command.env("HOME", &scratch.0);
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let (hello, _) = schema::frame::read::<Value>(&mut stdout).unwrap().unwrap();
        assert_eq!(hello, json!({ "type": "serving" }));
        Self {
            stdin: child.stdin.take().unwrap(),
            stdout,
            child,
        }
    }

    /// Returns the answer to a request for `target`, and its body.
    pub(super) fn ask(&mut self, target: &str, deflate: bool) -> (Value, Vec<u8>) {
        let asking =
            json!({ "type": "view", "target": target, "deflate": deflate, "controller": "alice" });
        schema::frame::write(&mut self.stdin, &asking, &[]).unwrap();
        let (answer, _) = schema::frame::read::<Value>(&mut self.stdout)
            .unwrap()
            .unwrap();
        let mut body = Vec::new();
        while body.len() < usize::try_from(answer["body_bytes"].as_u64().unwrap()).unwrap() {
            let (_, part) = schema::frame::read::<Value>(&mut self.stdout)
                .unwrap()
                .unwrap();
            body.extend(part);
        }
        (answer, body)
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the requests and their snapshot"
)]
fn answers_over_a_pipe() {
    let scratch = Scratch::new();
    let (ended, live) = (SERVED_ENDED, SERVED_LIVE);
    let driver = write_served(&scratch);
    fs::rename(scratch.0.join("runs"), scratch.0.join(".pumpkin/runs")).unwrap();
    let view = scratch.pumpkin(&["view"]);
    let served = Served::of(view);
    let mut piped = Piped::start(&scratch);
    let mut said = Vec::new();
    // The live run's row and page say how long ago things were, which can change between two
    // asks, so they are read through the pipe alone, below.
    let targets = [
        format!("/api/runs?before={live}"),
        format!("/api/runs/{ended}/log?at=0"),
        format!("/api/runs/{ended}/trace/agent-0/steps"),
        "/api/runs/run-1".to_owned(),
        format!("/api/runs/{ended}/since?log=7"),
        "/api/nothing".to_owned(),
    ];
    for target in &targets {
        for deflate in [false, true] {
            let reply = piped.ask(target, deflate);
            assert_eq!(reply, served.as_frame(target, deflate), "{target}");
            if !deflate {
                said.push(format!("{target}: {}", reply.0["status"].as_str().unwrap()));
            }
        }
    }
    // A run's reply holds no command to run it again, as it would name the controller's paths.
    for (target, run) in [
        (format!("/api/runs/{ended}"), ""),
        (format!("/api/runs/{ended}/since?log=0"), "/run"),
    ] {
        let mut answered: Value = serde_json::from_str(&served.get_with(&target, &[]).1).unwrap();
        let again = answered
            .pointer_mut(run)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("again");
        let (head, body) = piped.ask(&target, false);
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            answered,
            "{target}"
        );
        said.push(format!(
            "{target}: {}, as `pumpkin view` answers but for its `again`, {}",
            head["status"].as_str().unwrap(),
            again.unwrap()
        ));
    }
    let (_, cancel) = piped.ask(&format!("/api/runs/{live}/cancel"), false);
    said.push(format!("cancel: {}", String::from_utf8(cancel).unwrap()));
    let (_, first) = piped.ask(&format!("/api/runs/{live}/since?log=0"), false);
    let first: Value = serde_json::from_slice(&first).unwrap();
    let end = first["end"].as_u64().unwrap();
    let mut next = whole_log()[cut(&whole_log(), 130).len()].clone();
    next["seq"] = json!(1 + cut(&whole_log(), 130).len());
    let log_path = scratch.0.join(format!(".pumpkin/runs/{live}/log.jsonl"));
    let mut log = OpenOptions::new().append(true).open(&log_path).unwrap();
    writeln!(log, "{next}").unwrap();
    let (_, grown) = piped.ask(&format!("/api/runs/{live}/since?log={end}"), false);
    let grown: Value = serde_json::from_slice(&grown).unwrap();
    let records = grown["records"].as_array().unwrap();
    assert_eq!(records[0]["byte"], end);
    said.push(format!(
        "the live run, after a line more: live {}, {} record, from where the last poll ended",
        grown["live"],
        records.len(),
    ));
    drop(piped.stdin);
    said.push(format!(
        "once its standard input ends: {}",
        piped.child.wait().unwrap()
    ));
    drop(driver);
    check(
        &scratch,
        said.into_iter().map(Step::Ran),
        false,
        &[(ended, "<ended>"), (live, "<live>")],
        &expect![[r#"
            /api/runs?before=<live>: 200 OK
            /api/runs/<ended>/log?at=0: 200 OK
            /api/runs/<ended>/trace/agent-0/steps: 200 OK
            /api/runs/run-1: 404 Not Found
            /api/runs/<ended>/since?log=7: 400 Bad Request
            /api/nothing: 404 Not Found
            /api/runs/<ended>: 200 OK, as `pumpkin view` answers but for its `again`, {"command":"pumpkin run --runs <scratch>/runs <scratch>/task.star"}
            /api/runs/<ended>/since?log=0: 200 OK, as `pumpkin view` answers but for its `again`, {"command":"pumpkin run --runs <scratch>/runs <scratch>/task.star"}
            cancel: {"error":"the view on a controller answers no cancel: cancel run <live> from the page that `pumpkin view` serves, or run: pumpkin cancel --controller alice <live>"}
            the live run, after a line more: live true, 1 record, from where the last poll ended
            once its standard input ends: exit status: 0"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the log that grows, and the polls that follow it"
)]
fn follows_a_live_run() {
    let scratch = Scratch::new();
    let id = "20261110-150000-00000010";
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let dir = write_run(
        &scratch,
        id,
        since.checked_sub(Duration::from_secs(60)).unwrap(),
        &[],
        None,
        &lease_places(Prices::Both),
    );
    let driver = File::open(dir.join("driver.pid")).unwrap();
    driver.lock().unwrap();
    let mut grown = Vec::new();
    for request in 0..1500 {
        let caller = json!({ "caller": "agent-0", "request": request });
        let mut started = caller.clone();
        started["kind"] = json!("model_request_started");
        let mut answered = caller;
        answered["kind"] = json!("model_answered");
        answered["answer"] = json!({ "kind": "forwarded" });
        answered["upstream_status"] = json!(200);
        grown.push(logged(request + 1, started, json!([])));
        grown.push(logged(request + 1, answered, json!([])));
    }
    let log_path = dir.join("log.jsonl");
    let mut log = OpenOptions::new().append(true).open(&log_path).unwrap();
    let mut seq = 1;
    let mut lines: Vec<String> = (grown.iter())
        .map(|record| {
            let mut record = record.clone();
            record["seq"] = json!(seq);
            seq += 1;
            format!("{record}\n")
        })
        .collect();
    lines.insert(0, fs::read_to_string(&log_path).unwrap());
    let served = Served::start(&scratch);
    let before = served.read_bytes();
    let (mut next, mut polls, mut taken, mut said) = (0, 0, String::new(), Vec::new());
    let mut poll = |next: &mut u64, taken: &mut String| {
        polls += 1;
        let answer = served.get(&format!("/api/runs/{id}/since?log={next}"));
        for record in answer["records"].as_array().into_iter().flatten() {
            let mut record = record.clone();
            let record = record.as_object_mut().unwrap();
            assert_eq!(record.remove("byte"), Some(json!(taken.len())));
            record.remove("when");
            let _ = writeln!(taken, "{}", Value::Object(record.clone()));
        }
        *next = answer["next"].as_u64().unwrap_or(*next);
        answer
    };
    let first = poll(&mut next, &mut taken);
    said.push(format!(
        "the first poll: {}",
        fields(&first["run"]["model"], &[])
    ));
    // More lines at once than the view holds of a log's last lines.
    log.write_all(lines[1..251].concat().as_bytes()).unwrap();
    let answer = poll(&mut next, &mut taken);
    said.push(format!(
        "after 250 lines: {} records, next {}, end {}",
        answer["records"].as_array().unwrap().len(),
        answer["next"],
        answer["end"]
    ));
    // Each chunk ends in half a line, which no poll may report before its other half is written.
    for part in lines[251..].chunks(97) {
        let (whole, cut) = part.split_at(part.len() - 1);
        log.write_all(whole.concat().as_bytes()).unwrap();
        let cut = cut[0].as_bytes();
        log.write_all(&cut[..cut.len() / 2]).unwrap();
        poll(&mut next, &mut taken);
        log.write_all(&cut[cut.len() / 2..]).unwrap();
    }
    let last = poll(&mut next, &mut taken);
    let nothing = served.get_with(&format!("/api/runs/{id}/since?log={next}"), &[]);
    polls += 1;
    let read = served.read_bytes() - before;
    let size = fs::metadata(&log_path).unwrap().len();
    let ir = fs::metadata(dir.join("ir.json")).unwrap().len();
    // Every byte is read once, as the polls ask only for what is new, and not much more: each
    // poll also reads the run's `ir.json`, for the task's name.
    assert_eq!(taken, lines.concat());
    assert!(
        read >= size && read < size + (2048 + ir) * polls,
        "{read} bytes read over {polls} polls of a log of {size}"
    );
    scratch.write(
        &format!("runs/{id}/outcome.json"),
        &json!({ "state": "failed", "reason": "stopped" }),
    );
    drop(driver);
    let ended = served.get(&format!("/api/runs/{id}/since?log={next}"));
    said.push(format!(
        "after the last poll: {}",
        fields(&last["run"]["model"], &[])
    ));
    let agent = &last["run"]["lanes"][0];
    let answered = agent["answered"].as_array().unwrap();
    said.push(format!(
        "the agent's lane: answers {}, {} times of them, the first {} and the last {}",
        agent["answers"],
        answered.len(),
        answered[0],
        answered[answered.len() - 1]
    ));
    let quiet: Value = serde_json::from_str(&nothing.1).unwrap();
    let clock = quiet["run"]["times"]["now"].is_f64();
    said.push(format!(
        "then: {}, end {}, live {}, the run's end {} and its clock on: {clock}",
        nothing.0, quiet["end"], quiet["live"], quiet["run"]["end"],
    ));
    said.push(format!("once ended: {ended}"));
    check(
        &scratch,
        said.into_iter().map(Step::Ran),
        false,
        &[(id, "<id>")],
        &expect![[r#"
            the first poll: dropped_headers: []
            failed: []
            first_token: {"count":0,"median":null}
            in_flight: []
            more_dropped_headers: false
            not_2xx: 0
            requests: 0
            tokens: {"cached":null,"input":0,"output":0}
            took: {"count":0,"median":null}

            after 250 lines: 250 records, next 35732, end 35732
            after the last poll: dropped_headers: []
            failed: []
            first_token: {"count":0,"median":null}
            in_flight: []
            more_dropped_headers: false
            not_2xx: 0
            requests: 1500
            tokens: {"cached":null,"input":null,"output":null}
            took: {"count":1500,"median":0.0}

            the agent's lane: answers {"count":1500,"failed":0}, 500 times of them, the first 1.0 and the last 1497.0
            then: HTTP/1.1 200 OK, end 437110, live true, the run's end 437110 and its clock on: true
            once ended: {"end":437110,"live":false}"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the requests and their snapshot"
)]
fn cancels_a_run() {
    let scratch = Scratch::new();
    let id = "20261110-160000-00000011";
    write_record(&scratch, id, true, None);
    let pid_path = scratch.0.join(format!("runs/{id}/driver.pid"));
    // A driver: the process that holds the record's lock, which ignores `SIGTERM`, so that a
    // cancel goes on until the test kills it.
    let mut driver = scratch.program("flock", &["--no-fork", pid_path.to_str().unwrap()]);
    driver.args(["sh", "-c", "trap '' TERM; echo locked; exec sleep 600"]);
    let mut driver = driver.stdout(Stdio::piped()).spawn().unwrap();
    let mut locked = String::new();
    BufReader::new(driver.stdout.take().unwrap())
        .read_line(&mut locked)
        .unwrap();
    fs::write(&pid_path, format!("{}\n", driver.id())).unwrap();
    let served = Served::start(&scratch);
    let page = format!("http://127.0.0.1:{}", served.port);
    let cancel = format!("/api/runs/{id}/cancel");
    let cancelling = format!("/api/runs/{id}/cancelling");
    let mut steps = Vec::new();
    let mut ask = |method: &str, path: &str, headers: &[(&str, &str)]| {
        let (status, body) = served.ask(method, path, headers);
        steps.push(format!("{method} {path} {headers:?}\n{status}\n{body}"));
    };
    ask("POST", &cancel, &[]);
    ask("POST", &cancel, &[("Origin", "http://pumpkin.example")]);
    ask(
        "POST",
        &cancel,
        &[("Origin", &page), ("X-Pumpkin-Token", "")],
    );
    ask("GET", &cancel, &[]);
    ask("POST", "/api/runs", &[("Origin", &page)]);
    ask("POST", "/api/controller/start", &[("Origin", &page)]);
    ask("GET", &cancelling, &[]);
    let unknown = "/api/runs/20261110-170000-00000012/cancel";
    ask("POST", unknown, &[("Origin", &page)]);
    let from_page = |steps: &mut Vec<String>| {
        // Only the status: what it has said so far depends on how far the child has got.
        let (status, _) = served.ask("POST", &cancel, &[("Origin", &page)]);
        steps.push(format!("POST {cancel} from the page\n{status}"));
    };
    let ended = |steps: &mut Vec<String>| {
        let said = poll(|| Some(served.get(&cancelling)).filter(|said| said["ended"] == true));
        steps.push(format!(
            "GET {cancelling} once ended: exit {}\n{}",
            said["exit"],
            said["said"].as_str().unwrap()
        ));
    };
    from_page(&mut steps);
    poll(|| {
        let said = served.get(&cancelling)["said"].as_str().unwrap().to_owned();
        said.contains("stopping run").then_some(())
    });
    let (status, body) = served.ask("POST", &cancel, &[("Origin", &page)]);
    steps.push(format!(
        "POST {cancel} from the page while that goes on\n{status}\n{body}"
    ));
    driver.kill().unwrap();
    assert!(!driver.wait().unwrap().success());
    ended(&mut steps);
    steps.push(format!(
        "then: {}",
        served.get(&format!("/api/runs/{id}"))["verdict"]
    ));
    from_page(&mut steps);
    ended(&mut steps);
    let tmp = fs::read_dir(scratch.0.join(".pumpkin/tmp")).unwrap();
    let left: Vec<_> = tmp.map(|entry| entry.unwrap().file_name()).collect();
    steps.push(format!("the workspace's tmp/ then holds: {left:?}"));
    check(
        &scratch,
        steps.into_iter().map(Step::Ran),
        false,
        &[(id, "<id>"), (&page, "<page>")],
        &expect![[r#"
            POST /api/runs/<id>/cancel []
            HTTP/1.1 403 Forbidden
            {"error":"the cancel of run <id> carries no `Origin`, which a browser sends: cancel it from the page that `pumpkin view` serves, or run: pumpkin cancel --runs <scratch>/runs <id>"}
            POST /api/runs/<id>/cancel [("Origin", "http://pumpkin.example")]
            HTTP/1.1 403 Forbidden
            {"error":"the cancel of run <id> comes from `http://pumpkin.example`, a page of another site: cancel it from the page that `pumpkin view` serves, or run: pumpkin cancel --runs <scratch>/runs <id>"}
            POST /api/runs/<id>/cancel [("Origin", "<page>"), ("X-Pumpkin-Token", "")]
            HTTP/1.1 401 Unauthorized
            {"error":"the request carries no token in `X-Pumpkin-Token`: give the one in the address that `pumpkin view` printed as it started"}
            GET /api/runs/<id>/cancel []
            HTTP/1.1 405 Method Not Allowed
            {"error":"the cancel of run <id> is sent by `POST`, as the page sends it: cancel it from the page that `pumpkin view` serves, or run: pumpkin cancel --runs <scratch>/runs <id>"}
            POST /api/runs [("Origin", "<page>")]
            HTTP/1.1 405 Method Not Allowed
            {"error":"`/api/runs` is read by `GET`: only a run's cancel, `/api/runs/<id>/cancel`, and the controller's start, `/api/controller/start`, take `POST`"}
            POST /api/controller/start [("Origin", "<page>")]
            HTTP/1.1 404 Not Found
            {"error":"this view serves the runs of a directory, `runs`, not a controller's: start a controller by a command that reaches it, which starts it: pumpkin ls"}
            GET /api/runs/<id>/cancelling []
            HTTP/1.1 404 Not Found
            {"error":"the page started no cancel of run <id>: cancel it from the page, by POST /api/runs/<id>/cancel"}
            POST /api/runs/20261110-170000-00000012/cancel [("Origin", "<page>")]
            HTTP/1.1 404 Not Found
            {"error":"no run that this view serves matches `20261110-170000-00000012`: name the id of one that its list shows"}
            POST /api/runs/<id>/cancel from the page
            HTTP/1.1 202 Accepted
            POST /api/runs/<id>/cancel from the page while that goes on
            HTTP/1.1 409 Conflict
            {"error":"the page's cancel of run <id> goes on: read what it says: GET /api/runs/<id>/cancelling"}
            GET /api/runs/<id>/cancelling once ended: exit 0
            pumpkin: stopping run <id>, recorded in <scratch>/runs/<id>: it ends once its sandboxes are deleted
            pumpkin: run <id> was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run <id> failed: its driver ended without an outcome; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star

            then: "Run <id> failed: its driver ended without an outcome"
            POST /api/runs/<id>/cancel from the page
            HTTP/1.1 202 Accepted
            GET /api/runs/<id>/cancelling once ended: exit 0
            pumpkin: run <id> failed: its driver ended without an outcome; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star

            the workspace's tmp/ then holds: []"#]],
    );
}

#[test]
fn counts_a_jobs_answers_under_its_caller_name() {
    let scratch = Scratch::new();
    let id = "20261110-170000-00000012";
    let call = json!({ "kind": "start_call", "caller": "agent-0", "job": 4, "grant": "train",
        "payload": { "size": 0 }, "timeout_ns": "10000000000" });
    let mut answer = answered(3, "job-04", 0, 429, json!({}), None);
    answer["event"]["failed"] = json!({ "code": "rate_limit_exceeded" });
    let log = [
        logged(1, json!({ "kind": "tick" }), json!([call])),
        started(2, "job-4", 0),
        answer,
    ];
    write_run(
        &scratch,
        id,
        asked(id, 0),
        &log,
        None,
        &lease_places(Prices::Both),
    );
    let served = Served::start(&scratch);
    let since = served.get(&format!("/api/runs/{id}/since?log=0"));
    let lane = &since["run"]["lanes"][1];
    let event = &since["records"].as_array().unwrap().last().unwrap()["event"];
    let said = format!(
        "logged {}, lane {}, caller {}, answered {}, failed {}",
        event["caller"], lane["lane"], lane["caller"], lane["answered"], lane["failed"]
    );
    check(
        &scratch,
        [Step::Ran(said)],
        false,
        &[],
        &expect![[
            r#"logged "job-04", lane "job 4", caller "job-4", answered [3.0], failed [3.0]"#
        ]],
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the three runs' logs")]
fn states_failures() {
    let scratch = Scratch::new();
    let none = || json!([]);
    let call = |job: u64| {
        json!({ "kind": "start_call", "caller": "agent-0", "job": job, "grant": "train",
            "payload": payload(), "timeout_ns": "10000000000" })
    };
    let failed = |seconds, kind: &str, id: &str, n: u64, reason: &str| {
        logged(
            seconds,
            json!({ "kind": kind, id: n, "reason": reason }),
            none(),
        )
    };
    let routed = "20261110-190000-00000014";
    let mut refused = answered(
        3,
        "agent-0",
        0,
        403,
        json!({ "response": { "bytes": 256 } }),
        Some("responses"),
    );
    refused["event"]["answer"] = json!({ "kind": "refused", "status": 400,
        "message": "the model provider refused the request, with a 403" });
    let reason = "the upstream `zai` failed the agent's last model request, 0";
    write_run(
        &scratch,
        routed,
        asked(routed, 0),
        &[started(2, "agent-0", 0), refused],
        Some(json!({ "state": "failed", "reason": reason })),
        &lease_places(Prices::Both),
    );
    let unjudged = "20261110-191000-00000015";
    let final_call = json!({ "kind": "start_final_call", "job": 1, "timeout_ns": "3000000000",
        "selections": [{ "role": "solver", "selection": payload() }] });
    let stop = json!([{ "kind": "stop_call", "job": 0 }, final_call]);
    let log = [
        logged(1, json!({ "kind": "tick" }), json!([call(0)])),
        logged(2, json!({ "kind": "tick" }), stop),
        failed(3, "call_failed", "job", 0, "the call was stopped"),
        failed(4, "call_failed", "job", 1, "the target's session ended"),
    ];
    write_run(
        &scratch,
        unjudged,
        asked(unjudged, 0),
        &log,
        Some(super::unsubmitted()),
        &lease_places(Prices::Both),
    );
    let went_on = "20261110-192000-00000016";
    let lease = json!({ "kind": "start_lease", "grant": "gpu", "holder": "agent-0", "lease": 0 });
    let log = [
        logged(1, json!({ "kind": "tick" }), json!([call(0), lease])),
        failed(2, "call_failed", "job", 0, "the job's sandbox didn't start"),
        failed(
            3,
            "lease_failed",
            "lease",
            0,
            "the runner's sandbox didn't start",
        ),
    ];
    write_run(
        &scratch,
        went_on,
        asked(went_on, 0),
        &log,
        Some(returned(1.0)),
        &lease_places(Prices::Both),
    );
    let served = Served::start(&scratch);
    let steps = [routed, unjudged, went_on].map(|id| {
        let run = served.get(&format!("/api/runs/{id}"));
        let failures = fields(&json!({ "failures": run["failures"] }), &["failures"]);
        Step::Ran(format!(
            "{}: {}\nfailure: {}\n{failures}",
            run["state"], run["verdict"], run["failure"]
        ))
    });
    check(
        &scratch,
        steps,
        false,
        &[],
        &expect![[r#"
        "failed": "Run 20261110-190000-00000014 failed: the upstream `zai` failed the agent's last model request, 0"
        failure: {"at":3.0,"evidence":[],"mark":{"at":3.0,"between":null,"lane":null,"record":null,"role":null},"reason":"the upstream `zai` failed the agent's last model request, 0","state":"failed","subject":{"kind":"run"}}
        failures:
          {"at":3.0,"evidence":[{"bytes":256,"name":"The response, as kept","path":"exchange/0/response"}],"mark":{"at":3.0,"between":["solver","model"],"lane":"agent 0","record":294,"role":"solver"},"reason":"the model provider refused the request, with a 403","state":"failed","subject":{"caller":"agent-0","kind":"request","request":0,"upstream":"zai"}}

        "finished": "Run 20261110-191000-00000015 finished without a result: the final call gave none (error: solver submitted nothing); it ran no command"
        failure: {"at":4.0,"evidence":[],"mark":{"at":4.0,"between":null,"lane":"job 1","record":826,"role":"scorer"},"reason":"the target's session ended","state":"finished without a result","subject":{"job":1,"kind":"call"}}
        failures:
          {"at":4.0,"evidence":[],"mark":{"at":4.0,"between":null,"lane":"job 1","record":826,"role":"scorer"},"reason":"the target's session ended","state":"failed","subject":{"job":1,"kind":"call"}}

        "finished": "Run 20261110-192000-00000016 finished with a result (score=1 metrics={\"solved\":3})"
        failure: null
        failures:
          {"at":2.0,"evidence":[],"mark":{"at":2.0,"between":["solver","trainer"],"lane":"job 0","record":480,"role":"trainer"},"reason":"the job's sandbox didn't start","state":"failed","subject":{"job":0,"kind":"call"}}
          {"at":3.0,"evidence":[],"mark":{"at":3.0,"between":["solver","gpu"],"lane":"lease 0","record":605,"role":"gpu"},"reason":"the runner's sandbox didn't start","state":"failed","subject":{"kind":"lease","lease":0}}
    "#]],
    );
}

#[test]
fn starts_over_a_shortened_log() {
    let scratch = Scratch::new();
    let id = "20261110-180000-00000013";
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let mut places = lease_places(Prices::Both);
    places.as_object_mut().unwrap().remove("routes");
    let dir = write_run(
        &scratch,
        id,
        since.checked_sub(Duration::from_secs(200)).unwrap(),
        &whole_log(),
        None,
        &places,
    );
    let driver = File::open(dir.join("driver.pid")).unwrap();
    driver.lock().unwrap();
    let served = Served::start(&scratch);
    let mut steps = Vec::new();
    let run = served.get(&format!("/api/runs/{id}"));
    let roles = run["talk"]["roles"].as_array().unwrap();
    let routes: Vec<String> = (roles.iter())
        .map(|role| format!("{} {}", role["name"], role["routes"]))
        .collect();
    steps.push(format!(
        "GET /api/runs/<id>: end {}, log_epoch {}, a build that kept no routes: {routes:?}",
        run["end"], run["log_epoch"]
    ));
    let byte = &served.get(&format!("/api/runs/{id}/since?log=0"))["records"][20]["byte"];
    let log_path = dir.join("log.jsonl");
    let log = fs::read_to_string(&log_path).unwrap();
    // The log cut back to its first 30 lines, so that the byte still starts one.
    let kept: String = log.split_inclusive('\n').take(30).collect();
    fs::write(&log_path, kept).unwrap();
    for path in [
        format!("/api/runs/{id}/since?log={byte}&log_epoch=0"),
        format!("/api/runs/{id}/log?at={byte}&log_epoch=0"),
    ] {
        let (status, body) = served.get_with(&path, &[]);
        steps.push(format!("GET {path}\n{status}\n{body}"));
    }
    let run = served.get(&format!("/api/runs/{id}"));
    let row = served.get("/api/runs")["runs"][0]["started_over"].clone();
    steps.push(format!(
        "then GET /api/runs/<id>: end {}, log_epoch {}; its row's started_over: {row}",
        run["end"], run["log_epoch"]
    ));
    let path = format!("/api/runs/{id}/since?log={byte}&log_epoch=1");
    let since = served.get(&path);
    steps.push(format!(
        "GET {path}\nend {}, log_epoch {}, {} records",
        since["end"],
        since["run"]["log_epoch"],
        since["records"].as_array().unwrap().len()
    ));
    drop(driver);
    check(
        &scratch,
        steps.into_iter().map(Step::Ran),
        false,
        &[(id, "<id>")],
        &expect![[r#"
            GET /api/runs/<id>: end 10332, log_epoch 0, a build that kept no routes: ["\"solver\" \"unknown\"", "\"trainer\" \"unknown\"", "\"scorer\" \"unknown\"", "\"gpu\" \"unknown\"", "\"cpu\" \"unknown\"", "\"box\" \"unknown\""]
            GET /api/runs/<id>/since?log=3294&log_epoch=0
            HTTP/1.1 409 Conflict
            {"error":"run <id>'s log started over since the reply that gave byte 3294: read the run again, and its log from byte 0","log_epoch":1}
            GET /api/runs/<id>/log?at=3294&log_epoch=0
            HTTP/1.1 409 Conflict
            {"error":"run <id>'s log started over since the reply that gave byte 3294: read the run again, and its log from byte 0","log_epoch":1}
            then GET /api/runs/<id>: end 4645, log_epoch 1; its row's started_over: "run <id>: its log is 4645 bytes, shorter than the 10332 bytes read of it before: reading it again from its start"
            GET /api/runs/<id>/since?log=3294&log_epoch=1
            end 4645, log_epoch 1, 10 records"#]],
    );
}

#[test]
fn windows_a_log_by_its_bytes() {
    let scratch = Scratch::new();
    let id = "20261110-190000-00000014";
    let tick = logged(1, json!({ "kind": "tick" }), json!([]));
    // One record of a megabyte, which a window holds alone.
    let mut padded = tick.clone();
    padded["pad"] = json!("x".repeat(1024 * 1024));
    let mut log = vec![tick; 20_000];
    log.insert(10_000, padded);
    log.extend(whole_log());
    let outcome = Some(returned(49.36));
    write_run(
        &scratch,
        id,
        asked(id, 30),
        &log,
        outcome,
        &lease_places(Prices::Both),
    );
    let served = Served::start(&scratch);
    let (mut steps, mut at, mut records) = (Vec::new(), 0, 0);
    loop {
        let window = served.get(&format!("/api/runs/{id}/log?at={at}"));
        let count = window["records"].as_array().unwrap().len();
        records += count;
        steps.push(format!(
            "from byte {at}: {count} records, next {}",
            window["next"]
        ));
        at = window["next"].as_u64().unwrap();
        if window["next"] == window["end"] {
            break;
        }
    }
    assert_eq!(records, log.len() + 1);
    check(
        &scratch,
        steps.into_iter().map(Step::Ran),
        false,
        &[],
        &expect![[r#"
            from byte 0: 10001 records, next 709045
            from byte 709045: 1 records, next 1757702
            from byte 1757702: 10068 records, next 2488096"#]],
    );
}

/// The paths of the files that the page's file at `path`, whose text is `body`, names.
fn loaded(path: &str, body: &str) -> Vec<String> {
    let dir = &path[..=path.rfind('/').unwrap()];
    let named = |prefix: &'static str, base: &str| {
        let found = body.match_indices(prefix).map(|(at, _)| at + prefix.len());
        let base = base.to_owned();
        found.map(move |at| format!("{base}{}", body[at..].split('"').next().unwrap()))
    };
    (named("href=\"/", "/").chain(named("src=\"/", "/")))
        .chain(named(" from \"./", dir).chain(named("url(\"", dir)))
        .collect()
}

#[test]
fn serves_the_page() {
    let scratch = Scratch::new();
    fs::create_dir_all(scratch.0.join("runs")).unwrap();
    let served = Served::start(&scratch);
    // The page reads the token from its address, so its own files are served without it.
    let no_token = [("X-Pumpkin-Token", "")];
    let mut steps = Vec::new();
    let mut paths = vec!["/".to_owned()];
    // How the page's files are served, by kind, so that a file added to the page changes nothing.
    let mut kinds = BTreeSet::new();
    let mut preloaded = BTreeSet::new();
    let mut classic = String::new();
    let mut at = 0;
    while let Some(path) = paths.get(at).cloned() {
        at += 1;
        let (head, body) = served.exchange_bytes("GET", &path, &no_token);
        let header = |name: &str| {
            let line = head
                .lines()
                .find(|line| line.starts_with(&format!("{name}: ")));
            line.map_or("none", |line| &line[name.len() + 2..])
                .to_owned()
        };
        let answered = format!(
            "{} {}; {}",
            head.lines().next().unwrap(),
            header("Content-Type"),
            header("Cache-Control"),
        );
        assert_eq!(header("Content-Length"), body.len().to_string(), "{path}");
        let policy = format!("policy: {}", header("Content-Security-Policy"));
        if path == "/" {
            steps.push(format!("{path}: {answered}"));
            steps.push(policy.clone());
            let page = String::from_utf8_lossy(&body);
            for line in page.lines() {
                let preload = r#"<link rel="modulepreload" href=""#;
                if let Some(href) = line.strip_prefix(preload) {
                    preloaded.insert(href.trim_end_matches("\">").to_owned());
                } else if line.starts_with("<script") || line.starts_with("<link") {
                    steps.push(format!("head: {line}"));
                }
                if let Some(src) = line.strip_prefix(r#"<script src=""#) {
                    classic = src.split('"').next().unwrap().to_owned();
                }
            }
        } else {
            let (dir, name) = path.rsplit_once('/').unwrap();
            let (_, extension) = name.rsplit_once('.').unwrap();
            kinds.insert(format!("{dir}/*.{extension}: {answered}"));
        }
        assert_eq!(steps[1], policy, "{path}");
        let font = header("Content-Type") == "font/woff2";
        assert!(!font || body.starts_with(b"wOF2"), "{path}");
        for file in loaded(&path, &String::from_utf8_lossy(&body)) {
            if !paths.contains(&file) {
                paths.push(file);
            }
        }
    }
    // Each module that the page loads is preloaded, so that none waits for its importer.
    let modules: BTreeSet<&String> = (paths.iter())
        .filter(|path| Path::new(path).extension() == Some("js".as_ref()) && **path != classic)
        .collect();
    assert_eq!(modules, preloaded.iter().collect());
    steps.extend(kinds);
    let dir = paths[1].rsplit_once('/').unwrap().0.to_owned();
    // A page of another build, still open, asks for a file at its own path, which the files of
    // this one are never served at.
    let other = format!("/{}/app.js", "0".repeat(64));
    let app = format!("{dir}/app.js");
    for (path, headers) in [
        ("/nothing.js", &[][..]),
        ("/app.js", &[][..]),
        (&other, &[][..]),
        (&format!("{dir}/"), &[][..]),
        ("/", &[("Host", "pumpkin.example")][..]),
        (&app, &[("Host", "pumpkin.example:80")][..]),
    ] {
        let (status, body) = served.get_with(path, headers);
        steps.push(format!("{path} {headers:?}: {status} {body}"));
    }
    check(
        &scratch,
        steps.into_iter().map(Step::Ran),
        false,
        &[(&dir, "/<digest>")],
        &expect![[r#"
            /: HTTP/1.1 200 OK text/html; charset=utf-8; no-store
            policy: default-src 'none'; script-src 'self'; style-src 'self'; font-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'
            head: <script src="/<digest>/scheme.js"></script>
            head: <link rel="stylesheet" href="/<digest>/style.css">
            head: <script type="module" src="/<digest>/app.js"></script>
            /<digest>/*.css: HTTP/1.1 200 OK text/css; charset=utf-8; public, max-age=31536000, immutable
            /<digest>/*.js: HTTP/1.1 200 OK text/javascript; charset=utf-8; public, max-age=31536000, immutable
            /<digest>/*.woff2: HTTP/1.1 200 OK font/woff2; public, max-age=31536000, immutable
            /nothing.js []: HTTP/1.1 404 Not Found {"error":"nothing is at `/nothing.js`: expected the page at `/`, or the runs at `/api/runs` and below it"}
            /app.js []: HTTP/1.1 404 Not Found {"error":"nothing is at `/app.js`: expected the page at `/`, or the runs at `/api/runs` and below it"}
            /0000000000000000000000000000000000000000000000000000000000000000/app.js []: HTTP/1.1 404 Not Found {"error":"nothing is at `/0000000000000000000000000000000000000000000000000000000000000000/app.js`: expected the page at `/`, or the runs at `/api/runs` and below it"}
            /<digest>/ []: HTTP/1.1 404 Not Found {"error":"nothing is at `/<digest>/`: expected the page at `/`, or the runs at `/api/runs` and below it"}
            / [("Host", "pumpkin.example")]: HTTP/1.1 403 Forbidden {"error":"the request names the host `pumpkin.example`: expected one of 127.0.0.1, localhost, [::1], this machine's loopback, at which the view is served"}
            /<digest>/app.js [("Host", "pumpkin.example:80")]: HTTP/1.1 403 Forbidden {"error":"the request names the host `pumpkin.example:80`: expected one of 127.0.0.1, localhost, [::1], this machine's loopback, at which the view is served"}"#]],
    );
}

#[test]
fn answers_past_a_connection_that_sends_nothing() {
    let scratch = Scratch::new();
    fs::create_dir_all(scratch.0.join("runs")).unwrap();
    let served = Served::start(&scratch);
    // A browser opens a spare connection, to use later or to close unused.
    let mut spare = TcpStream::connect(("127.0.0.1", served.port)).unwrap();
    drop(TcpStream::connect(("127.0.0.1", served.port)).unwrap());
    let (status, _) = served.get_with("/", &[]);
    spare
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .unwrap();
    let mut answer = String::new();
    spare.read_to_string(&mut answer).unwrap();
    let spares = answer.lines().next().unwrap();
    let answered = [
        format!("past a spare and a closed connection: {status}"),
        format!("the spare's, sent after: {spares}"),
    ];
    check(
        &scratch,
        answered.map(Step::Ran),
        false,
        &[],
        &expect![[r#"
            past a spare and a closed connection: HTTP/1.1 200 OK
            the spare's, sent after: HTTP/1.1 200 OK"#]],
    );
}

#[test]
fn deflates_what_the_request_accepts() {
    let scratch = Scratch::new();
    let id = "20261110-200000-00000015";
    let outcome = Some(returned(49.36));
    let places = lease_places(Prices::Both);
    write_run(&scratch, id, asked(id, 30), &whole_log(), outcome, &places);
    let served = Served::start(&scratch);
    let mut steps = Vec::new();
    let log = format!("/api/runs/{id}/log?at=0");
    let end = served.get(&format!("/api/runs/{id}"))["end"].clone();
    // So short that deflating lengthens it.
    let nothing = format!("/api/runs/{id}/since?log={end}");
    for path in ["/", &log, &nothing] {
        let (_, plain) = served.exchange_bytes("GET", path, &[]);
        for accepted in [
            "gzip, deflate, br, zstd",
            "gzip",
            "gzip, deflate;q=0",
            "deflate;q=0.5",
        ] {
            let asked = [("Accept-Encoding", accepted)];
            let (head, body) = served.exchange_bytes("GET", path, &asked);
            let header = |name: &str| {
                let prefix = format!("{name}: ");
                head.lines().find_map(|line| line.strip_prefix(&prefix))
            };
            assert_eq!(header("Content-Length"), Some(&*body.len().to_string()));
            let encoding = header("Content-Encoding");
            let sent = match encoding {
                Some("deflate") => miniz_oxide::inflate::decompress_to_vec_zlib(&body).unwrap(),
                _ => body,
            };
            assert_eq!(sent, plain, "{path} {accepted}");
            steps.push(format!(
                "{path} {accepted:?}: Content-Encoding {}, Vary {}",
                encoding.unwrap_or("none"),
                header("Vary").unwrap_or("none")
            ));
        }
    }
    check(
        &scratch,
        steps.into_iter().map(Step::Ran),
        false,
        &[(id, "<id>")],
        &expect![[r#"
            / "gzip, deflate, br, zstd": Content-Encoding deflate, Vary Accept-Encoding
            / "gzip": Content-Encoding none, Vary Accept-Encoding
            / "gzip, deflate;q=0": Content-Encoding none, Vary Accept-Encoding
            / "deflate;q=0.5": Content-Encoding deflate, Vary Accept-Encoding
            /api/runs/<id>/log?at=0 "gzip, deflate, br, zstd": Content-Encoding deflate, Vary Accept-Encoding
            /api/runs/<id>/log?at=0 "gzip": Content-Encoding none, Vary Accept-Encoding
            /api/runs/<id>/log?at=0 "gzip, deflate;q=0": Content-Encoding none, Vary Accept-Encoding
            /api/runs/<id>/log?at=0 "deflate;q=0.5": Content-Encoding deflate, Vary Accept-Encoding
            /api/runs/<id>/since?log=10332 "gzip, deflate, br, zstd": Content-Encoding none, Vary Accept-Encoding
            /api/runs/<id>/since?log=10332 "gzip": Content-Encoding none, Vary Accept-Encoding
            /api/runs/<id>/since?log=10332 "gzip, deflate;q=0": Content-Encoding none, Vary Accept-Encoding
            /api/runs/<id>/since?log=10332 "deflate;q=0.5": Content-Encoding none, Vary Accept-Encoding"#]],
    );
}

#[test]
fn pages_the_list() {
    let scratch = Scratch::new();
    let write = |runs: std::ops::Range<u64>| {
        for run in runs {
            let id = format!(
                "20261111-{:02}{:02}{:02}-{run:08x}",
                run / 3600,
                run / 60 % 60,
                run % 60
            );
            let outcome = json!({ "state": "failed", "reason": "stopped" });
            write_record(&scratch, &id, true, Some(outcome));
        }
    };
    write(0..101);
    let served = Served::start(&scratch);
    let mut steps = Vec::new();
    let mut pages = |count: usize| {
        let (mut listed, mut nexts) = (Vec::new(), Vec::new());
        let mut page = served.get("/api/runs");
        loop {
            let runs = page["runs"].as_array().unwrap();
            listed.extend(
                runs.iter()
                    .map(|run| run["id"].as_str().unwrap().to_owned()),
            );
            nexts.push(format!("{} runs, next {}", runs.len(), page["next"]));
            let Some(next) = page["next"].as_str() else {
                break;
            };
            page = served.get(&format!("/api/runs?before={next}"));
        }
        let mut sorted = listed.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        sorted.dedup();
        assert_eq!(listed, sorted);
        assert_eq!(listed.len(), count);
        steps.push(format!("of {count} runs: {}", nexts.join("; ")));
    };
    pages(101);
    write(101..201);
    pages(201);
    check(
        &scratch,
        steps.into_iter().map(Step::Ran),
        false,
        &[],
        &expect![[r#"
            of 101 runs: 100 runs, next "20261111-000001-00000001"; 1 runs, next null
            of 201 runs: 100 runs, next "20261111-000141-00000065"; 100 runs, next "20261111-000001-00000001"; 1 runs, next null"#]],
    );
}

/// The model exchanges that a run's record keeps under `model/`, with each item stored once and
/// named by its number, as a test needs no SHA-256.
pub(super) struct Model {
    dir: std::path::PathBuf,
    items: Vec<String>,
}

impl Model {
    /// Starts the store in the run's directory `dir`.
    pub(super) fn new(dir: &std::path::Path) -> Self {
        fs::create_dir_all(dir.join("model/items")).unwrap();
        Self {
            dir: dir.join("model"),
            items: Vec::new(),
        }
    }

    fn item(&mut self, value: &Value) -> Value {
        let text = value.to_string();
        let index = self.items.iter().position(|item| *item == text);
        let index = index.unwrap_or_else(|| {
            self.items.push(text.clone());
            self.items.len() - 1
        });
        let hex = format!("{index:064x}");
        fs::write(self.dir.join("items").join(&hex), text).unwrap();
        json!(format!("sha256:{hex}"))
    }

    /// Writes request `n` and its response, and returns the `kept` field that `model_answered`
    /// carries.
    ///
    /// The request holds `items` in its `field`, each of `whole` as one item of its own, and
    /// `rest`. A response of `None` is named by `kept` but not written.
    pub(super) fn keep(
        &mut self,
        n: u64,
        field: &str,
        items: &[Value],
        whole: &[(&str, Value)],
        rest: Value,
        response: Option<&str>,
    ) -> Value {
        let mut body = rest;
        body[field] = Value::Array(items.iter().map(|item| self.item(item)).collect());
        for (name, value) in whole {
            body[*name] = self.item(value);
        }
        let request = format!("model/{n}.request");
        fs::write(self.dir.join(format!("{n}.request")), body.to_string()).unwrap();
        let mut kept = json!({ "request": { "path": request, "bytes": body.to_string().len(),
            "body_bytes": 100, "items": { "field": field, "count": items.len() } } });
        if !whole.is_empty() {
            kept["request"]["whole"] =
                json!(whole.iter().map(|(name, _)| *name).collect::<Vec<_>>());
        }
        if let Some(response) = response {
            fs::write(self.dir.join(format!("{n}.response")), response).unwrap();
        }
        kept["response"] = json!({ "path": format!("model/{n}.response"),
            "bytes": response.map_or(0, str::len) });
        kept
    }
}

/// The record of `caller` starting model request `n` at `seconds`.
pub(super) fn started(seconds: u64, caller: &str, n: u64) -> Value {
    let event = json!({ "kind": "model_request_started", "caller": caller, "request": n });
    let mut record = logged(seconds, event, json!([]));
    record["observed_ns"] = json!((seconds * 1_000_000_000 - 1).to_string());
    record
}

/// The record of model request `n` answered with `status` and `kept`.
///
/// With an `api`, the record also names the upstream and model and times the first byte and
/// token, as a forwarded answer does.
pub(super) fn answered(
    seconds: u64,
    caller: &str,
    n: u64,
    status: u64,
    kept: Value,
    api: Option<&str>,
) -> Value {
    let mut event = json!({ "kind": "model_answered", "caller": caller, "request": n,
        "answer": { "kind": "forwarded" }, "upstream_status": status,
        "usage": { "input_tokens": 100 * n, "output_tokens": 10, "cached_tokens": 50 } });
    event["kept"] = kept;
    if let Some(api) = api {
        event["api"] = json!(api);
        event["upstream"] = json!("zai");
        event["model"] = json!("model-a");
        event["first_byte_ns"] = json!((seconds * 1_000_000_000 - 500).to_string());
        event["first_token_ns"] = json!((seconds * 1_000_000_000 - 400_000_000).to_string());
    }
    logged(seconds, event, json!([]))
}

fn columns_text(reply: &Value, field: &str) -> String {
    let columns = reply[field].as_object().unwrap();
    let mut text = String::new();
    let rows = columns
        .values()
        .next()
        .map_or(0, |column| column.as_array().unwrap().len());
    for row in 0..rows {
        let cells: Vec<String> = (columns.iter())
            .filter(|(_, column)| !column[row].is_null())
            .map(|(name, column)| format!("{name}={}", column[row]))
            .collect();
        let _ = writeln!(text, "{}", cells.join(" "));
    }
    for (name, value) in reply.as_object().unwrap() {
        if name != field {
            let _ = writeln!(text, "{name}: {value}");
        }
    }
    text
}

fn entries_text(entries: &Value) -> String {
    let mut text = String::new();
    for entry in entries["entries"].as_array().unwrap() {
        let _ = writeln!(text, "{entry}");
    }
    let _ = write!(text, "next: {}", entries["next"]);
    text
}

/// Asserts that each row of the steps reply `steps` of `trace` counts as `added_entries` and
/// `answers` the entries that the entries reply gives of its step.
fn entries_counted(served: &Served, trace: &str, steps: &Value) {
    let columns = &steps["steps"];
    let column = |name: &str| columns[name].as_array().unwrap();
    for (row, request) in column("request").iter().enumerate() {
        let entries = served.get(&format!("{trace}/entries?from={request}&to={request}"));
        let given = |side: &str| {
            (entries["entries"].as_array().unwrap().iter())
                .filter(|entry| entry["side"] == side)
                .count()
        };
        let added = column("added_entries")[row].as_u64().unwrap_or(0);
        let given_added = u64::try_from(given("added")).unwrap();
        assert_eq!(added, given_added, "request {request}");
        assert_eq!(
            column("answers")[row],
            json!(given("answer")),
            "request {request}"
        );
    }
}

/// A user message of the Responses API.
pub(super) fn user(text: &str) -> Value {
    json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": text }] })
}

/// An assistant message of the Responses API.
pub(super) fn said(text: &str) -> Value {
    json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": text }] })
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the record's exchanges and the snapshot"
)]
fn traces_a_caller() {
    let scratch = Scratch::new();
    let id = "20261110-170000-00000012";
    let dir = scratch.0.join("runs").join(id);
    let mut model = Model::new(&dir);
    let tools = json!([{ "type": "function", "name": "exec" }]);
    let whole = [
        ("instructions", json!("be brief")),
        ("tools", tools.clone()),
    ];
    let rest = || json!({ "model": "glm-5.3", "stream": false });
    let call = json!({ "type": "function_call", "call_id": "c1", "name": "exec", "arguments": "{\"cmd\":\"ls\"}" });
    let output = json!({ "type": "function_call_output", "call_id": "c1", "output": "a.txt" });
    let answer = |items: Value| json!({ "status": "completed", "output": items }).to_string();
    let first = [user("list the files")];
    let second = [user("list the files"), call.clone(), output];
    let mut third = second.to_vec();
    third.extend([said("a.txt is there"), user("and now?")]);
    let other = [user("another thread")];
    let mut log = vec![started(1, "agent-0", 0)];
    let kept = model.keep(
        0,
        "input",
        &first,
        &whole,
        rest(),
        Some(&answer(json!([call]))),
    );
    log.push(answered(2, "agent-0", 0, 200, kept, Some("responses")));
    log.push(started(3, "agent-0", 1));
    let kept = model.keep(
        1,
        "input",
        &second,
        &whole,
        rest(),
        Some(&answer(json!([said("a.txt is there")]))),
    );
    log.push(answered(4, "agent-0", 1, 200, kept, Some("responses")));
    // A thread of its own, failed, retried, then sent again.
    log.push(started(5, "agent-0", 2));
    let kept = model.keep(
        2,
        "input",
        &other,
        &whole,
        rest(),
        Some("{\"error\":{\"message\":\"slow down\"}}"),
    );
    log.push(answered(6, "agent-0", 2, 429, kept, Some("responses")));
    for n in [3, 4] {
        log.push(started(n * 2 + 1, "agent-0", n));
        let kept = model.keep(
            n,
            "input",
            &other,
            &whole,
            rest(),
            Some(&answer(json!([said("hello")]))),
        );
        let mut answered = answered(n * 2 + 2, "agent-0", n, 200, kept, Some("responses"));
        // Cut short, which makes its cost unknown.
        if n == 4 {
            answered["event"]["answer"]["kind"] = json!("cut");
        }
        log.push(answered);
    }
    // A new turn, whose response is a link out of the record, as damage would leave it.
    log.push(started(11, "agent-0", 5));
    let kept = model.keep(5, "input", &third, &whole, rest(), None);
    let outside = scratch.0.join("outside");
    fs::write(&outside, "not the record's").unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("model/5.response")).unwrap();
    log.push(answered(12, "agent-0", 5, 200, kept, Some("responses")));
    // A call's Chat Completions request, which names no API and keeps its tools in each request.
    let system = json!({ "role": "system", "content": "judge it" });
    let asked = json!({ "role": "user", "content": "is it right?" });
    let calls = |id: &str| {
        json!({ "role": "assistant", "content": "let me look", "reasoning_content": "check",
            "tool_calls": [{ "id": id, "type": "function", "function": { "name": "grep", "arguments": "{}" } }] })
    };
    let tool = json!({ "role": "tool", "tool_call_id": "t1", "content": "found" });
    let chat_rest = || json!({ "model": "glm-5.3-flash", "tools": [{ "type": "function", "function": { "name": "grep" } }] });
    let chat = |message: Value, finish: &str| {
        json!({ "choices": [{ "index": 0, "message": message, "finish_reason": finish }] })
            .to_string()
    };
    // Two samples of one list, whose answers say the same and call apart: the child continues
    // the first, as each of its first item's entries, the text and the call, sends it back.
    for (n, id) in [(6, "t1"), (7, "t2")] {
        let kept = model.keep(
            n,
            "messages",
            &[system.clone(), asked.clone()],
            &[],
            chat_rest(),
            Some(&chat(calls(id), "tool_calls")),
        );
        log.push(answered(7 + n, "job-0", n, 200, kept, None));
    }
    let back = json!({ "role": "assistant", "content": "let me look",
        "tool_calls": [{ "id": "t1", "type": "function", "function": { "name": "grep", "arguments": "{}" } }] });
    let sent = [system, asked, back, tool];
    let kept = model.keep(
        8,
        "messages",
        &sent,
        &[],
        chat_rest(),
        Some(&chat(
            json!({ "role": "assistant", "content": "yes" }),
            "stop",
        )),
    );
    log.push(answered(15, "job-0", 8, 200, kept, None));
    // And the same again, abandoned as the call's route ended.
    log.push(started(16, "job-0", 9));
    let ended = "{\"error\":{\"message\":\"the model route has ended\"}}";
    let kept = model.keep(9, "messages", &sent, &[], chat_rest(), Some(ended));
    let mut abandoned = answered(17, "job-0", 9, 0, kept, None);
    let event = abandoned["event"].as_object_mut().unwrap();
    event.remove("usage");
    event.insert("answer".to_owned(), json!({ "kind": "abandoned" }));
    event.insert("upstream_status".to_owned(), Value::Null);
    event.insert("model".to_owned(), json!("glm-5.3-flash"));
    log.push(abandoned);
    // Barred before it went upstream, which keeps its body whole and starts no request.
    let body =
        json!({ "model": "glm-5.3-flash", "messages": [{ "role": "user", "content": "and?" }] });
    let body = body.to_string();
    let refusal = "{\"error\":{\"message\":\"the model budget is exhausted\"}}";
    fs::write(dir.join("model/10.request"), &body).unwrap();
    fs::write(dir.join("model/10.response"), refusal).unwrap();
    let kept = json!({
        "request": { "path": "model/10.request", "bytes": body.len(), "body_bytes": body.len() },
        "response": { "path": "model/10.response", "bytes": refusal.len() },
    });
    let mut barred = answered(18, "job-0", 10, 0, kept, Some("chat"));
    let event = barred["event"].as_object_mut().unwrap();
    for field in ["usage", "model", "first_byte_ns", "first_token_ns"] {
        event.remove(field);
    }
    let answer = json!({ "kind": "barred", "bar": "budget", "status": 400,
        "message": "the model budget is exhausted" });
    event.insert("answer".to_owned(), answer);
    event.insert("upstream_status".to_owned(), Value::Null);
    log.push(barred);
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    write_run(
        &scratch,
        id,
        since.checked_sub(Duration::from_secs(60)).unwrap(),
        &log,
        Some(json!({ "state": "failed", "reason": "stopped" })),
        &lease_places(Prices::Both),
    );
    let served = Served::start(&scratch);
    let run = format!("/api/runs/{id}");
    let mut said = Vec::new();
    let steps = served.get(&format!("{run}/trace/agent-0/steps"));
    entries_counted(&served, &format!("{run}/trace/agent-0"), &steps);
    said.push(format!(
        "the agent's steps:\n{}",
        columns_text(&steps, "steps")
    ));
    let entries = served.get(&format!("{run}/trace/agent-0/entries?from=0&to=5"));
    said.push(format!("its entries:\n{}", entries_text(&entries)));
    let window = served.get(&format!(
        "{run}/trace/agent-0/entries?from=0&to=5&kind=user&thread=2"
    ));
    said.push(format!("thread 2's users':\n{}", entries_text(&window)));
    let found = served.get(&format!("{run}/trace/agent-0/search?q=A.TXT"));
    said.push(format!("found: {found}"));
    let found = served.get(&format!("{run}/trace/agent-0/search?q=a.txt&kind=output"));
    said.push(format!("found in outputs: {found}"));
    for by in ["step", "turn", "thread"] {
        let folded = served.get(&format!("{run}/trace/agent-0/stats?by={by}"));
        said.push(format!("by {by}:\n{}", columns_text(&folded, "rows")));
    }
    let steps = served.get(&format!("{run}/trace/job-0/steps"));
    entries_counted(&served, &format!("{run}/trace/job-0"), &steps);
    said.push(format!(
        "the call's steps:\n{}",
        columns_text(&steps, "steps")
    ));
    let entries = served.get(&format!("{run}/trace/job-0/entries?from=0&to=9"));
    said.push(format!("its entries:\n{}", entries_text(&entries)));
    for by in ["step", "thread"] {
        let folded = served.get(&format!("{run}/trace/job-0/stats?by={by}"));
        said.push(format!("by {by}:\n{}", columns_text(&folded, "rows")));
    }
    let exchange = served.get(&format!("{run}/exchange/1"));
    said.push(format!("exchange 1:\n{}", fields(&exchange, &[])));
    let (status, request) = served.get_with(&format!("{run}/exchange/1/request"), &[]);
    said.push(format!("its request: {status}\n{request}"));
    let (status, response) = served.get_with(&format!("{run}/exchange/5/response"), &[]);
    said.push(format!("exchange 5's response: {status}\n{response}"));
    for at in ["0/head/instructions/0", "1/in/2/0", "8/out/0"] {
        let caller = if at.starts_with('8') {
            "job-0"
        } else {
            "agent-0"
        };
        let entry = served.get(&format!("{run}/trace/{caller}/entry?at={at}"));
        said.push(format!("entry {at}: {entry}"));
    }
    let item = format!("{:064x}", 0);
    let (status, body) = served.get_with(&format!("{run}/item/{item}"), &[]);
    said.push(format!("item 0: {status}\n{body}"));
    let search = format!("trace/agent-0/search?q={}", "a".repeat(1025));
    for path in [
        "trace/someone/steps",
        "trace/job-01/steps",
        "trace/job-99/steps",
        "trace/job-99/ops",
        "trace/agent-0/entries?from=0&to=5&kind=tool",
        "item/abc",
        "exchange/99",
        "trace/agent-0/entries?from=0",
        "trace/agent-0/entries?from=5&to=3",
        "trace/agent-0/stats?by=day",
        "trace/agent-0/entry",
        "trace/agent-0/entry?at=1/in/x/0",
        "trace/agent-0/entry?at=1/out",
        "trace/agent-0/entry?at=1/in/9/0",
        "trace/agent-0/entry?at=6/out/0",
        &search,
    ] {
        let (status, body) = served.get_with(&format!("{run}/{path}"), &[]);
        let path = if path.len() > 80 {
            "a search of 1025 bytes"
        } else {
            path
        };
        said.push(format!("{path}: {status}\n{body}"));
    }
    served.get(&format!("{run}/trace/agent-0/steps"));
    // The view drops the run's held trace as other ended runs are read, and the log is cut back
    // while none holds it: the trace is read again from the log as it stands.
    for other in 0..9 {
        let other = format!("20261110-170100-{other:08x}");
        let outcome = Some(json!({ "state": "failed", "reason": "stopped" }));
        write_run(
            &scratch,
            &other,
            since,
            &[],
            outcome,
            &lease_places(Prices::Both),
        );
        served.get(&format!("/api/runs/{other}"));
    }
    let log_path = dir.join("log.jsonl");
    let log = fs::read_to_string(&log_path).unwrap();
    fs::write(
        &log_path,
        log.split_inclusive('\n').take(3).collect::<String>(),
    )
    .unwrap();
    let steps = served.get(&format!("{run}/trace/agent-0/steps"));
    said.push(format!("once cut: {}", steps["steps"]["request"]));
    check(
        &scratch,
        said.into_iter().map(Step::Ran),
        false,
        &[(id, "<id>")],
        &expect![[r#"
            the agent's steps:
            added=1 added_entries=2 answer="forwarded" answers=1 api="responses" back_bytes=118 cached=50 calls=["exec"] end_ns="2000000000" first_byte_ns="1999999500" first_token_ns="1600000000" input=0 items=1 kept_bytes=289 model_ns=1000000001 output=10 request=0 sent_bytes=100 start_ns="999999999" status=200 stop="completed" thread=0 tools=0 turn=0 upstream="zai" whole=true
            added=2 added_entries=1 answer="forwarded" answers=1 api="responses" back_bytes=130 cache_hit=0.5 cached=50 calls=[] differs=[] end_ns="4000000000" first_byte_ns="3999999500" first_token_ns="3600000000" input=100 items=3 kept_bytes=437 model_ns=1000000001 output=10 parent=0 request=1 sent_bytes=100 start_ns="2999999999" status=200 stop="completed" thread=0 tools=0 turn=0 upstream="zai" whole=true
            added=1 added_entries=2 answer="forwarded" answers=0 api="responses" back_bytes=33 cache_hit=0.25 cached=50 calls=[] end_ns="6000000000" first_byte_ns="5999999500" first_token_ns="5600000000" input=200 items=1 kept_bytes=289 model_ns=1000000001 output=10 request=2 sent_bytes=100 start_ns="4999999999" status=429 thread=2 tools=0 turn=2 upstream="zai" whole=true
            added=1 added_entries=2 answer="forwarded" answers=1 api="responses" back_bytes=121 cache_hit=0.16666666666666666 cached=50 calls=[] end_ns="8000000000" first_byte_ns="7999999500" first_token_ns="7600000000" input=300 items=1 kept_bytes=289 model_ns=1000000001 output=10 request=3 retry_count=1 retry_of=2 sent_bytes=100 start_ns="6999999999" status=200 stop="completed" thread=2 tools=0 turn=2 upstream="zai" whole=true
            added=1 added_entries=2 answer="cut" answers=1 api="responses" back_bytes=121 cache_hit=0.125 cached=50 calls=[] end_ns="10000000000" first_byte_ns="9999999500" first_token_ns="9600000000" input=400 items=1 kept_bytes=289 model_ns=1000000001 output=10 repeat_of=3 request=4 sent_bytes=100 start_ns="8999999999" status=200 stop="completed" thread=2 tools=0 turn=2 upstream="zai" whole=true
            added=2 added_entries=1 answer="forwarded" answers=0 api="responses" back_bytes=0 cache_hit=0.1 cached=50 calls=[] differs=[] end_ns="12000000000" first_byte_ns="11999999500" first_token_ns="11600000000" input=500 items=5 kept_bytes=585 model_ns=1000000001 notes=["its response can't be read: `<scratch>/runs/<id>/model/5.response` is a symbolic link: expected a file of the record's own"] output=10 parent=1 request=5 sent_bytes=100 start_ns="10999999999" status=200 thread=0 tools=0 turn=5 upstream="zai" whole=false
            caller: "agent-0"
            end: 6273
            live: false
            log_epoch: 0
            thread_spans: {"span_ns":[11000000001,5000000001],"thread":[0,2]}
            threads: 2
            tool_lists: [{"digest":"0000000000000000000000000000000000000000000000000000000000000002","names":["exec"]}]

            its entries:
            {"at":"0/head/instructions/0","digest":"0000000000000000000000000000000000000000000000000000000000000001","index":0,"kind":"system","request":0,"side":"added","text":"be brief"}
            {"at":"0/in/0/0","digest":"0000000000000000000000000000000000000000000000000000000000000000","index":1,"kind":"user","request":0,"side":"added","text":"list the files"}
            {"at":"0/out/0","call_id":"c1","digest":null,"index":2,"kind":"call","name":"exec","output":"1/in/2/0","request":0,"side":"answer","text":"{\"cmd\":\"ls\"}"}
            {"at":"1/in/2/0","call":"0/out/0","call_id":"c1","digest":"0000000000000000000000000000000000000000000000000000000000000004","index":0,"kind":"output","request":1,"side":"added","text":"a.txt"}
            {"at":"1/out/0","digest":null,"index":1,"kind":"assistant","request":1,"side":"answer","text":"a.txt is there"}
            {"at":"2/head/instructions/0","digest":"0000000000000000000000000000000000000000000000000000000000000001","index":0,"kind":"system","request":2,"side":"added","text":"be brief"}
            {"at":"2/in/0/0","digest":"0000000000000000000000000000000000000000000000000000000000000005","index":1,"kind":"user","request":2,"side":"added","text":"another thread"}
            {"at":"3/head/instructions/0","digest":"0000000000000000000000000000000000000000000000000000000000000001","index":0,"kind":"system","request":3,"side":"added","text":"be brief"}
            {"at":"3/in/0/0","digest":"0000000000000000000000000000000000000000000000000000000000000005","index":1,"kind":"user","request":3,"side":"added","text":"another thread"}
            {"at":"3/out/0","digest":null,"index":2,"kind":"assistant","request":3,"side":"answer","text":"hello"}
            {"at":"4/head/instructions/0","digest":"0000000000000000000000000000000000000000000000000000000000000001","index":0,"kind":"system","request":4,"side":"added","text":"be brief"}
            {"at":"4/in/0/0","digest":"0000000000000000000000000000000000000000000000000000000000000005","index":1,"kind":"user","request":4,"side":"added","text":"another thread"}
            {"at":"4/out/0","digest":null,"index":2,"kind":"assistant","request":4,"side":"answer","text":"hello"}
            {"at":"5/in/4/0","digest":"0000000000000000000000000000000000000000000000000000000000000007","index":0,"kind":"user","request":5,"side":"added","text":"and now?"}
            next: null
            thread 2's users':
            {"at":"2/in/0/0","digest":"0000000000000000000000000000000000000000000000000000000000000005","index":1,"kind":"user","request":2,"side":"added","text":"another thread"}
            {"at":"3/in/0/0","digest":"0000000000000000000000000000000000000000000000000000000000000005","index":1,"kind":"user","request":3,"side":"added","text":"another thread"}
            {"at":"4/in/0/0","digest":"0000000000000000000000000000000000000000000000000000000000000005","index":1,"kind":"user","request":4,"side":"added","text":"another thread"}
            next: null
            found: {"count":2,"hits":[{"at":"1/in/2/0","byte":0,"context":["","a.txt",""],"index":0,"kind":"output","request":1},{"at":"1/out/0","byte":0,"context":["","a.txt"," is there"],"index":1,"kind":"assistant","request":1}]}
            found in outputs: {"count":1,"hits":[{"at":"1/in/2/0","byte":0,"context":["","a.txt",""],"index":0,"kind":"output","request":1}]}
            by step:
            cached=50 calls={"exec":1} decode_ns=400000000 input=0 model_ns=1000000001 outcome="ok" output=10 request=0 status=200 thread=0 to_first_byte_ns=999999501 to_first_token_ns=600000001 tok_s=25.0 turn=0
            cache_hit=0.5 cached=50 calls={} decode_ns=400000000 input=100 model_ns=1000000001 outcome="ok" output=10 queue_ns=999999999 request=1 status=200 thread=0 to_first_byte_ns=999999501 to_first_token_ns=600000001 tok_s=25.0 turn=0 usd=0.0315
            cache_hit=0.25 cached=50 calls={} decode_ns=400000000 input=200 model_ns=1000000001 outcome="failed" output=10 request=2 status=429 thread=2 to_first_byte_ns=999999501 to_first_token_ns=600000001 tok_s=25.0 turn=2 usd=0.0615
            cache_hit=0.16666666666666666 cached=50 calls={} decode_ns=400000000 input=300 model_ns=1000000001 outcome="ok" output=10 request=3 retry_of=2 status=200 thread=2 to_first_byte_ns=999999501 to_first_token_ns=600000001 tok_s=25.0 turn=2 usd=0.0915
            cache_hit=0.125 cached=50 calls={} decode_ns=400000000 input=400 model_ns=1000000001 outcome="failed" output=10 repeat_of=3 request=4 status=200 thread=2 to_first_byte_ns=999999501 to_first_token_ns=600000001 tok_s=25.0 turn=2
            cache_hit=0.1 cached=50 calls={} decode_ns=400000000 input=500 model_ns=1000000001 outcome="ok" output=10 queue_ns=6999999999 request=5 status=200 thread=0 to_first_byte_ns=999999501 to_first_token_ns=600000001 tok_s=25.0 turn=5 usd=0.1515
            by: "step"
            caller: "agent-0"
            totals: {"cache_hit":0.2,"cache_write":null,"cache_write_unknown":6,"cached":300,"cached_unknown":0,"calls":{"exec":1},"decode_ns":2400000000,"decode_ns_unknown":0,"failures":2,"first_request":0,"in_flight":0,"input":1500,"input_unknown":0,"median_to_first_byte_ns":999999501,"median_to_first_byte_ns_unknown":0,"median_to_first_token_ns":600000001,"median_to_first_token_ns_unknown":0,"model_ns":6000000006,"model_ns_unknown":0,"output":60,"output_unknown":0,"queue_ns":7999999998,"queue_ns_unknown":4,"reasoning":null,"reasoning_unknown":6,"refusals":0,"repeats":1,"retries":1,"span_ns":11000000001,"steps":6,"threads":2,"tok_s":25.0,"turns":3,"usd":0.33599999999999997,"usd_unknown":2}

            by turn:
            cache_hit=1.0 cache_write_unknown=2 cached=100 cached_unknown=0 calls={"exec":1} decode_ns=800000000 decode_ns_unknown=0 failures=0 first_request=0 in_flight=0 input=100 input_unknown=0 median_to_first_byte_ns=999999501 median_to_first_byte_ns_unknown=0 median_to_first_token_ns=600000001 median_to_first_token_ns_unknown=0 model_ns=2000000002 model_ns_unknown=0 output=20 output_unknown=0 queue_ns=999999999 queue_ns_unknown=1 reasoning_unknown=2 refusals=0 repeats=0 retries=0 span_ns=3000000001 steps=2 thread=0 tok_s=25.0 turn=0 usd=0.0315 usd_unknown=1
            cache_hit=0.16666666666666666 cache_write_unknown=3 cached=150 cached_unknown=0 calls={} decode_ns=1200000000 decode_ns_unknown=0 failures=2 first_request=2 in_flight=0 input=900 input_unknown=0 median_to_first_byte_ns=999999501 median_to_first_byte_ns_unknown=0 median_to_first_token_ns=600000001 median_to_first_token_ns_unknown=0 model_ns=3000000003 model_ns_unknown=0 output=30 output_unknown=0 queue_ns_unknown=3 reasoning_unknown=3 refusals=0 repeats=1 retries=1 span_ns=5000000001 steps=3 thread=2 tok_s=25.0 turn=2 usd=0.153 usd_unknown=1
            cache_hit=0.1 cache_write_unknown=1 cached=50 cached_unknown=0 calls={} decode_ns=400000000 decode_ns_unknown=0 failures=0 first_request=5 in_flight=0 input=500 input_unknown=0 median_to_first_byte_ns=999999501 median_to_first_byte_ns_unknown=0 median_to_first_token_ns=600000001 median_to_first_token_ns_unknown=0 model_ns=1000000001 model_ns_unknown=0 output=10 output_unknown=0 queue_ns=6999999999 queue_ns_unknown=0 reasoning_unknown=1 refusals=0 repeats=0 retries=0 span_ns=1000000001 steps=1 thread=0 tok_s=25.0 turn=5 usd=0.1515 usd_unknown=0
            by: "turn"
            caller: "agent-0"
            totals: {"cache_hit":0.2,"cache_write":null,"cache_write_unknown":6,"cached":300,"cached_unknown":0,"calls":{"exec":1},"decode_ns":2400000000,"decode_ns_unknown":0,"failures":2,"first_request":0,"in_flight":0,"input":1500,"input_unknown":0,"median_to_first_byte_ns":999999501,"median_to_first_byte_ns_unknown":0,"median_to_first_token_ns":600000001,"median_to_first_token_ns_unknown":0,"model_ns":6000000006,"model_ns_unknown":0,"output":60,"output_unknown":0,"queue_ns":7999999998,"queue_ns_unknown":4,"reasoning":null,"reasoning_unknown":6,"refusals":0,"repeats":1,"retries":1,"span_ns":11000000001,"steps":6,"threads":2,"tok_s":25.0,"turns":3,"usd":0.33599999999999997,"usd_unknown":2}

            by thread:
            cache_hit=0.25 cache_write_unknown=3 cached=150 cached_unknown=0 calls={"exec":1} decode_ns=1200000000 decode_ns_unknown=0 failures=0 first_request=0 in_flight=0 input=600 input_unknown=0 median_to_first_byte_ns=999999501 median_to_first_byte_ns_unknown=0 median_to_first_token_ns=600000001 median_to_first_token_ns_unknown=0 model_ns=3000000003 model_ns_unknown=0 output=30 output_unknown=0 queue_ns=7999999998 queue_ns_unknown=1 reasoning_unknown=3 refusals=0 repeats=0 retries=0 span_ns=11000000001 steps=3 thread=0 tok_s=25.0 usd=0.183 usd_unknown=1
            cache_hit=0.16666666666666666 cache_write_unknown=3 cached=150 cached_unknown=0 calls={} decode_ns=1200000000 decode_ns_unknown=0 failures=2 first_request=2 in_flight=0 input=900 input_unknown=0 median_to_first_byte_ns=999999501 median_to_first_byte_ns_unknown=0 median_to_first_token_ns=600000001 median_to_first_token_ns_unknown=0 model_ns=3000000003 model_ns_unknown=0 output=30 output_unknown=0 queue_ns_unknown=3 reasoning_unknown=3 refusals=0 repeats=1 retries=1 span_ns=5000000001 steps=3 thread=2 tok_s=25.0 usd=0.153 usd_unknown=1
            by: "thread"
            caller: "agent-0"
            totals: {"cache_hit":0.2,"cache_write":null,"cache_write_unknown":6,"cached":300,"cached_unknown":0,"calls":{"exec":1},"decode_ns":2400000000,"decode_ns_unknown":0,"failures":2,"first_request":0,"in_flight":0,"input":1500,"input_unknown":0,"median_to_first_byte_ns":999999501,"median_to_first_byte_ns_unknown":0,"median_to_first_token_ns":600000001,"median_to_first_token_ns_unknown":0,"model_ns":6000000006,"model_ns_unknown":0,"output":60,"output_unknown":0,"queue_ns":7999999998,"queue_ns_unknown":4,"reasoning":null,"reasoning_unknown":6,"refusals":0,"repeats":1,"retries":1,"span_ns":11000000001,"steps":6,"threads":2,"tok_s":25.0,"turns":3,"usd":0.33599999999999997,"usd_unknown":2}

            the call's steps:
            added=2 added_entries=2 answer="forwarded" answers=3 back_bytes=226 cache_hit=0.08333333333333333 cached=50 calls=["grep"] end_ns="13000000000" input=600 items=2 kept_bytes=243 output=10 request=6 sent_bytes=100 status=200 stop="tool_calls" thread=6 tools=0 turn=6 whole=true
            added=2 added_entries=2 answer="forwarded" answers=3 back_bytes=226 cache_hit=0.07142857142857142 cached=50 calls=["grep"] end_ns="14000000000" input=700 items=2 kept_bytes=243 output=10 repeat_of=6 request=7 sent_bytes=100 status=200 stop="tool_calls" thread=6 tools=0 turn=6 whole=true
            added=2 added_entries=1 answer="forwarded" answers=1 back_bytes=95 cache_hit=0.0625 cached=50 calls=[] differs=[] end_ns="15000000000" input=800 items=4 kept_bytes=391 output=10 parent=6 request=8 sent_bytes=100 status=200 stop="stop" thread=6 tools=0 turn=6 whole=true
            added=2 added_entries=1 answer="abandoned" answers=0 back_bytes=49 calls=[] differs=[] end_ns="17000000000" items=4 kept_bytes=391 model_ns=1000000001 parent=6 repeat_of=8 request=9 sent_bytes=100 start_ns="15999999999" thread=6 tools=0 turn=6 whole=true
            added_entries=1 answer="barred" answers=0 api="chat" back_bytes=53 calls=[] end_ns="18000000000" kept_bytes=71 notes=["the record kept its body whole, so its conversation is compared with no other"] request=10 sent_bytes=71 thread=10 turn=10 upstream="zai" whole=true
            caller: "job-0"
            end: 6273
            live: false
            log_epoch: 0
            thread_spans: {"span_ns":[1000000001,null],"thread":[6,10]}
            threads: 2
            tool_lists: [{"digest":null,"names":["grep"]}]

            its entries:
            {"at":"6/in/0/0","digest":"0000000000000000000000000000000000000000000000000000000000000008","index":0,"kind":"system","request":6,"side":"added","text":"judge it"}
            {"at":"6/in/1/0","digest":"0000000000000000000000000000000000000000000000000000000000000009","index":1,"kind":"user","request":6,"side":"added","text":"is it right?"}
            {"at":"6/out/0","digest":null,"index":2,"kind":"reasoning","request":6,"side":"answer","text":"check"}
            {"at":"6/out/1","digest":null,"index":3,"kind":"assistant","request":6,"side":"answer","text":"let me look"}
            {"at":"6/out/2","call_id":"t1","digest":null,"index":4,"kind":"call","name":"grep","output":"8/in/3/0","request":6,"side":"answer","text":"{}"}
            {"at":"7/in/0/0","digest":"0000000000000000000000000000000000000000000000000000000000000008","index":0,"kind":"system","request":7,"side":"added","text":"judge it"}
            {"at":"7/in/1/0","digest":"0000000000000000000000000000000000000000000000000000000000000009","index":1,"kind":"user","request":7,"side":"added","text":"is it right?"}
            {"at":"7/out/0","digest":null,"index":2,"kind":"reasoning","request":7,"side":"answer","text":"check"}
            {"at":"7/out/1","digest":null,"index":3,"kind":"assistant","request":7,"side":"answer","text":"let me look"}
            {"at":"7/out/2","call_id":"t2","digest":null,"index":4,"kind":"call","name":"grep","request":7,"side":"answer","text":"{}"}
            {"at":"8/in/3/0","call":"6/out/2","call_id":"t1","digest":"000000000000000000000000000000000000000000000000000000000000000b","index":0,"kind":"output","request":8,"side":"added","text":"found"}
            {"at":"8/out/0","digest":null,"index":1,"kind":"assistant","request":8,"side":"answer","text":"yes"}
            {"at":"9/in/3/0","call":"6/out/2","call_id":"t1","digest":"000000000000000000000000000000000000000000000000000000000000000b","index":0,"kind":"output","request":9,"side":"added","text":"found"}
            next: null
            by step:
            cache_hit=0.08333333333333333 cached=50 calls={"grep":1} input=600 outcome="ok" output=10 request=6 status=200 thread=6 turn=6
            cache_hit=0.07142857142857142 cached=50 calls={"grep":1} input=700 outcome="ok" output=10 repeat_of=6 request=7 status=200 thread=6 turn=6
            cache_hit=0.0625 cached=50 calls={} input=800 outcome="ok" output=10 request=8 status=200 thread=6 turn=6
            calls={} model_ns=1000000001 outcome="refused" queue_ns=2999999999 repeat_of=8 request=9 thread=6 turn=6
            calls={} outcome="refused" request=10 thread=10 turn=10
            by: "step"
            caller: "job-0"
            totals: {"cache_hit":0.07142857142857142,"cache_write":null,"cache_write_unknown":5,"cached":150,"cached_unknown":2,"calls":{"grep":2},"decode_ns":null,"decode_ns_unknown":5,"failures":0,"first_request":6,"in_flight":0,"input":2100,"input_unknown":2,"median_to_first_byte_ns":null,"median_to_first_byte_ns_unknown":5,"median_to_first_token_ns":null,"median_to_first_token_ns_unknown":5,"model_ns":1000000001,"model_ns_unknown":4,"output":30,"output_unknown":2,"queue_ns":2999999999,"queue_ns_unknown":4,"reasoning":null,"reasoning_unknown":5,"refusals":2,"repeats":2,"retries":0,"span_ns":2000000001,"steps":5,"threads":2,"tok_s":null,"turns":2,"usd":null,"usd_unknown":5}

            by thread:
            cache_hit=0.07142857142857142 cache_write_unknown=4 cached=150 cached_unknown=1 calls={"grep":2} decode_ns_unknown=4 failures=0 first_request=6 in_flight=0 input=2100 input_unknown=1 median_to_first_byte_ns_unknown=4 median_to_first_token_ns_unknown=4 model_ns=1000000001 model_ns_unknown=3 output=30 output_unknown=1 queue_ns=2999999999 queue_ns_unknown=3 reasoning_unknown=4 refusals=1 repeats=2 retries=0 span_ns=1000000001 steps=4 thread=6 usd_unknown=4
            cache_write_unknown=1 cached_unknown=1 calls={} decode_ns_unknown=1 failures=0 first_request=10 in_flight=0 input_unknown=1 median_to_first_byte_ns_unknown=1 median_to_first_token_ns_unknown=1 model_ns_unknown=1 output_unknown=1 queue_ns_unknown=1 reasoning_unknown=1 refusals=1 repeats=0 retries=0 steps=1 thread=10 usd_unknown=1
            by: "thread"
            caller: "job-0"
            totals: {"cache_hit":0.07142857142857142,"cache_write":null,"cache_write_unknown":5,"cached":150,"cached_unknown":2,"calls":{"grep":2},"decode_ns":null,"decode_ns_unknown":5,"failures":0,"first_request":6,"in_flight":0,"input":2100,"input_unknown":2,"median_to_first_byte_ns":null,"median_to_first_byte_ns_unknown":5,"median_to_first_token_ns":null,"median_to_first_token_ns_unknown":5,"model_ns":1000000001,"model_ns_unknown":4,"output":30,"output_unknown":2,"queue_ns":2999999999,"queue_ns_unknown":4,"reasoning":null,"reasoning_unknown":5,"refusals":2,"repeats":2,"retries":0,"span_ns":2000000001,"steps":5,"threads":2,"tok_s":null,"turns":2,"usd":null,"usd_unknown":5}

            exchange 1:
            answer: {"kind":"forwarded"}
            api: "responses"
            caller: "agent-0"
            config: {"max_tokens":null,"model":"glm-5.3","reasoning_effort":null,"stop":[],"stream":false,"temperature":null,"tools":["exec"]}
            failed: null
            kept: {"request":{"body_bytes":100,"bytes":437,"items":{"count":3,"field":"input"},"path":"model/1.request","whole":["instructions","tools"]},"response":{"bytes":130,"path":"model/1.response"}}
            request: 1
            request_id: null
            stats: {"cache_hit":0.5,"cache_write":null,"cached":50,"calls":{},"decode_ns":400000000,"input":100,"model_ns":1000000001,"outcome":"ok","output":10,"queue_ns":999999999,"reasoning":null,"repeat_of":null,"request":1,"retry_of":null,"status":200,"thread":0,"to_first_byte_ns":999999501,"to_first_token_ns":600000001,"tok_s":25.0,"turn":0,"usd":0.0315}
            status: 200
            times: {"end_ns":"4000000000","first_byte_ns":"3999999500","first_token_ns":"3600000000","start_ns":"2999999999"}
            upstream: "zai"
            usage: {"cached_tokens":50,"input_tokens":100,"output_tokens":10}

            its request: HTTP/1.1 200 OK
            {"input":[{"content":[{"text":"list the files","type":"input_text"}],"role":"user","type":"message"},{"arguments":"{\"cmd\":\"ls\"}","call_id":"c1","name":"exec","type":"function_call"},{"call_id":"c1","output":"a.txt","type":"function_call_output"}],"instructions":"be brief","model":"glm-5.3","stream":false,"tools":[{"name":"exec","type":"function"}]}
            exchange 5's response: HTTP/1.1 404 Not Found
            {"error":"`<scratch>/runs/<id>/model/5.response` is a symbolic link: expected a file of the record's own"}
            entry 0/head/instructions/0: {"at":"0/head/instructions/0","digest":"0000000000000000000000000000000000000000000000000000000000000001","index":0,"kind":"system","request":0,"side":"added","text":"be brief","text_bytes":8}
            entry 1/in/2/0: {"at":"1/in/2/0","call":"0/out/0","call_id":"c1","digest":"0000000000000000000000000000000000000000000000000000000000000004","index":0,"kind":"output","request":1,"side":"added","text":"a.txt","text_bytes":5}
            entry 8/out/0: {"at":"8/out/0","digest":null,"index":1,"kind":"assistant","request":8,"side":"answer","text":"yes","text_bytes":3}
            item 0: HTTP/1.1 200 OK
            {"content":[{"text":"list the files","type":"input_text"}],"role":"user","type":"message"}
            trace/someone/steps: HTTP/1.1 400 Bad Request
            {"error":"`someone` is no caller: expected `agent-<n>` or `job-<n>`"}
            trace/job-01/steps: HTTP/1.1 400 Bad Request
            {"error":"`job-01` is no caller: expected `agent-<n>` or `job-<n>`"}
            trace/job-99/steps: HTTP/1.1 404 Not Found
            {"error":"run <id>'s log names no job 99: name an agent or a job of the run"}
            trace/job-99/ops: HTTP/1.1 404 Not Found
            {"error":"run <id>'s log names no job 99: name an agent or a job of the run"}
            trace/agent-0/entries?from=0&to=5&kind=tool: HTTP/1.1 400 Bad Request
            {"error":"`kind` is no entry's kind: unknown variant `tool`, expected one of `system`, `user`, `assistant`, `reasoning`, `call`, `output`, `other`, `unread`"}
            item/abc: HTTP/1.1 400 Bad Request
            {"error":"`abc` is no item: expected 64 lowercase hexadecimal digits"}
            exchange/99: HTTP/1.1 404 Not Found
            {"error":"run <id>'s log names no model request 99"}
            trace/agent-0/entries?from=0: HTTP/1.1 400 Bad Request
            {"error":"give a request's number: `?to=<request>`"}
            trace/agent-0/entries?from=5&to=3: HTTP/1.1 400 Bad Request
            {"error":"`to` is 3, before `from`, 5: expected `from` <= `to`"}
            trace/agent-0/stats?by=day: HTTP/1.1 400 Bad Request
            {"error":"`by` is `day`: expected `step`, `turn` or `thread`"}
            trace/agent-0/entry: HTTP/1.1 400 Bad Request
            {"error":"give an entry's address: `?at=<address>`"}
            trace/agent-0/entry?at=1/in/x/0: HTTP/1.1 400 Bad Request
            {"error":"`1/in/x/0` is no entry's address: expected `<request>/in/<item>/<part>`, `<request>/head/<field>/<part>` or `<request>/out/<part>`"}
            trace/agent-0/entry?at=1/out: HTTP/1.1 400 Bad Request
            {"error":"`1/out` is no entry's address: expected `<request>/in/<item>/<part>`, `<request>/head/<field>/<part>` or `<request>/out/<part>`"}
            trace/agent-0/entry?at=1/in/9/0: HTTP/1.1 404 Not Found
            {"error":"agent-0's trace has no entry at `1/in/9/0`"}
            trace/agent-0/entry?at=6/out/0: HTTP/1.1 404 Not Found
            {"error":"agent-0's trace has no entry at `6/out/0`"}
            a search of 1025 bytes: HTTP/1.1 400 Bad Request
            {"error":"`q` is 1025 bytes: expected at most 1024"}
            once cut: [0]"#]],
    );
}

// A Messages API stream of thinking, then a tool call, cut down.
const MESSAGES_STREAM: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Tars built.\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"signature_delta\",\"signature\":\"93bf01a7\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":1,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"Bash\",\"input\":{}}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":1,",
    "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\\\"ls\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":1,",
    "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\" -la\\\"}\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},",
    "\"usage\":{\"input_tokens\":174,\"output_tokens\":83,\"cache_read_input_tokens\":36544}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the record's exchanges and the snapshot"
)]
fn traces_a_messages_exchange() {
    let scratch = Scratch::new();
    let id = "20261110-190000-00000014";
    let dir = scratch.0.join("runs").join(id);
    let mut model = Model::new(&dir);
    let system = json!([
        { "type": "text", "text": "You are a Claude agent." },
        { "type": "text", "text": "Assist with the task.", "cache_control": { "type": "ephemeral" } },
    ]);
    let tools = json!([{ "name": "Bash", "description": "Executes a bash command.",
        "input_schema": { "type": "object", "properties": { "command": { "type": "string" } } } }]);
    let whole = [("system", system), ("tools", tools.clone())];
    let rest = || {
        json!({ "model": "glm-5.3", "max_tokens": 32000, "stream": true,
        "thinking": { "type": "adaptive" } })
    };
    let first = json!({ "role": "user", "content": "list the files" });
    // Claude Code marks the last message's last block for the cache, as a text block where it is a
    // text, and sends that message in its next request as the text, unmarked.
    let marked = json!({ "role": "user", "content": [{ "type": "text", "text": "list the files",
        "cache_control": { "type": "ephemeral" } }] });
    let call = json!({ "role": "assistant", "content": [{ "type": "tool_use", "id": "call_1",
        "name": "Bash", "input": { "command": "ls -la" } }] });
    let result = json!({ "role": "user", "content": [{ "type": "tool_result",
        "tool_use_id": "call_1", "content": "ls: cannot access 'x'", "is_error": true }] });
    // Claude Code puts its own environment updates into the conversation as `system` messages.
    let update = json!({ "role": "system", "content": [{ "type": "text",
        "text": "# Environment update\n - Primary working directory: /workspace",
        "cache_control": { "type": "ephemeral" } }] });
    let answer = |text: &str| {
        let start = json!({ "type": "content_block_start", "index": 0,
            "content_block": { "type": "text", "text": "" } });
        let delta = json!({ "type": "content_block_delta", "index": 0,
            "delta": { "type": "text_delta", "text": text } });
        format!(
            "event: content_block_start\ndata: {start}\n\nevent: content_block_delta\ndata: {delta}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
        )
    };
    let mut log = vec![started(1, "agent-0", 0)];
    let kept = model.keep(
        0,
        "messages",
        &[marked],
        &whole,
        rest(),
        Some(MESSAGES_STREAM),
    );
    let mut record = answered(2, "agent-0", 0, 200, kept, Some("messages"));
    // As the route logs the stream's counts, with 120 tokens written to the cache: its input
    // counts the cache's reads and writes.
    record["event"]["usage"] = json!({ "input_tokens": 36838, "output_tokens": 83,
        "cached_tokens": 36544, "cache_write_tokens": 120 });
    log.push(record);
    log.push(started(3, "agent-0", 1));
    let kept = model.keep(
        1,
        "messages",
        &[first.clone(), call.clone(), result.clone(), update],
        &whole,
        rest(),
        Some(&answer("a.txt is there")),
    );
    log.push(answered(4, "agent-0", 1, 200, kept, Some("messages")));
    // This request sends the update again, its mark left out, as the item its parent sent.
    let update = json!({ "role": "system",
        "content": "# Environment update\n - Primary working directory: /workspace" });
    let reply = json!({ "role": "assistant", "content": "a.txt is there" });
    let next = json!({ "role": "user", "content": [{ "type": "text", "text": "and b?",
        "cache_control": { "type": "ephemeral" } }] });
    log.push(started(5, "agent-0", 2));
    let sent = [first, call, result, update, reply, next];
    let kept = model.keep(
        2,
        "messages",
        &sent,
        &whole,
        rest(),
        Some(&answer("b.txt is not")),
    );
    log.push(answered(6, "agent-0", 2, 200, kept, Some("messages")));
    let changed = json!([{ "type": "text", "text": "You are a Claude agent.\nAssist with the task, briefly." }]);
    let sent = [
        &sent[..],
        &[
            json!({ "role": "assistant", "content": "b.txt is not" }),
            json!({ "role": "user", "content": "and c?" }),
        ],
    ]
    .concat();
    log.push(started(7, "agent-0", 3));
    let kept = model.keep(
        3,
        "messages",
        &sent,
        &[("system", changed), ("tools", tools)],
        rest(),
        Some(&answer("c.txt is not")),
    );
    log.push(answered(8, "agent-0", 3, 200, kept, Some("messages")));
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    write_run(
        &scratch,
        id,
        since.checked_sub(Duration::from_secs(60)).unwrap(),
        &log,
        Some(json!({ "state": "failed", "reason": "stopped" })),
        &lease_places(Prices::Both),
    );
    let served = Served::start(&scratch);
    let run = format!("/api/runs/{id}");
    let mut said = Vec::new();
    let steps = served.get(&format!("{run}/trace/agent-0/steps"));
    said.push(format!(
        "the agent's steps:\n{}",
        columns_text(&steps, "steps")
    ));
    entries_counted(&served, &format!("{run}/trace/agent-0"), &steps);
    let entries = served.get(&format!("{run}/trace/agent-0/entries?from=0&to=1"));
    said.push(format!("its entries:\n{}", entries_text(&entries)));
    let found = served.get(&format!("{run}/trace/agent-0/search?q=CANNOT"));
    said.push(format!("found: {found}"));
    let folded = served.get(&format!("{run}/trace/agent-0/stats?by=step"));
    said.push(format!("by step:\n{}", columns_text(&folded, "rows")));
    let exchange = served.get(&format!("{run}/exchange/0"));
    said.push(format!("exchange 0:\n{}", fields(&exchange, &[])));
    let diff = served.get(&format!("{run}/diff?a=2&b=3&part=system"));
    said.push(format!("diff of steps 2 and 3's system: {diff}"));
    check(
        &scratch,
        said.into_iter().map(Step::Ran),
        false,
        &[(id, "<id>")],
        &expect![[r##"
            the agent's steps:
            added=1 added_entries=2 answer="forwarded" answers=2 api="messages" back_bytes=1376 cache_hit=0.9920191107009066 cache_write=120 cached=36544 calls=["Bash"] end_ns="2000000000" first_byte_ns="1999999500" first_token_ns="1600000000" input=36838 items=1 kept_bytes=335 model_ns=1000000001 output=83 request=0 sent_bytes=100 start_ns="999999999" status=200 stop="tool_use" thread=0 tools=0 turn=0 upstream="zai" whole=true
            added=3 added_entries=2 answer="forwarded" answers=1 api="messages" back_bytes=297 cache_hit=0.5 cached=50 calls=[] differs=[] end_ns="4000000000" first_byte_ns="3999999500" first_token_ns="3600000000" input=100 items=4 kept_bytes=557 model_ns=1000000001 output=10 parent=0 request=1 sent_bytes=100 start_ns="2999999999" status=200 thread=0 tools=0 turn=0 upstream="zai" whole=true
            added=2 added_entries=1 answer="forwarded" answers=1 api="messages" back_bytes=295 cache_hit=0.25 cached=50 calls=[] differs=[] end_ns="6000000000" first_byte_ns="5999999500" first_token_ns="5600000000" input=200 items=6 kept_bytes=705 model_ns=1000000001 output=10 parent=1 request=2 sent_bytes=100 start_ns="4999999999" status=200 thread=0 tools=0 turn=2 upstream="zai" whole=true
            added=2 added_entries=2 answer="forwarded" answers=1 api="messages" back_bytes=295 cache_hit=0.16666666666666666 cached=50 calls=[] differs=["system"] end_ns="8000000000" first_byte_ns="7999999500" first_token_ns="7600000000" input=300 items=8 kept_bytes=853 model_ns=1000000001 output=10 parent=2 request=3 sent_bytes=100 start_ns="6999999999" status=200 thread=0 tools=0 turn=3 upstream="zai" whole=true
            caller: "agent-0"
            end: 2877
            live: false
            log_epoch: 0
            thread_spans: {"span_ns":[7000000001],"thread":[0]}
            threads: 1
            tool_lists: [{"digest":"0000000000000000000000000000000000000000000000000000000000000002","names":["Bash"]}]

            its entries:
            {"at":"0/head/system/0","digest":"0000000000000000000000000000000000000000000000000000000000000001","index":0,"kind":"system","request":0,"side":"added","text":"You are a Claude agent.\nAssist with the task."}
            {"at":"0/in/0/0","digest":"0000000000000000000000000000000000000000000000000000000000000000","index":1,"kind":"user","request":0,"side":"added","text":"list the files"}
            {"at":"0/out/0","digest":null,"index":2,"kind":"reasoning","request":0,"side":"answer","text":"Tars built."}
            {"at":"0/out/1","call_id":"call_1","digest":null,"index":3,"kind":"call","name":"Bash","output":"1/in/2/0","request":0,"side":"answer","text":"{\"command\":\"ls -la\"}"}
            {"at":"1/in/2/0","call":"0/out/1","call_id":"call_1","digest":"0000000000000000000000000000000000000000000000000000000000000005","error":true,"index":0,"kind":"output","request":1,"side":"added","text":"ls: cannot access 'x'"}
            {"at":"1/in/3/0","digest":"0000000000000000000000000000000000000000000000000000000000000006","index":1,"kind":"system","request":1,"side":"added","text":"# Environment update\n - Primary working directory: /workspace"}
            {"at":"1/out/0","digest":null,"index":2,"kind":"assistant","request":1,"side":"answer","text":"a.txt is there"}
            next: null
            found: {"count":1,"hits":[{"at":"1/in/2/0","byte":4,"context":["ls: ","cannot"," access 'x'"],"index":0,"kind":"output","request":1}]}
            by step:
            cache_hit=0.9920191107009066 cache_write=120 cached=36544 calls={"Bash":1} decode_ns=400000000 input=36838 model_ns=1000000001 outcome="ok" output=83 request=0 status=200 thread=0 to_first_byte_ns=999999501 to_first_token_ns=600000001 tok_s=207.5 turn=0 usd=1.31802
            cache_hit=0.5 cached=50 calls={} decode_ns=400000000 input=100 model_ns=1000000001 outcome="ok" output=10 queue_ns=999999999 request=1 status=200 thread=0 to_first_byte_ns=999999501 to_first_token_ns=600000001 tok_s=25.0 turn=0 usd=0.0315
            cache_hit=0.25 cached=50 calls={} decode_ns=400000000 input=200 model_ns=1000000001 outcome="ok" output=10 queue_ns=999999999 request=2 status=200 thread=0 to_first_byte_ns=999999501 to_first_token_ns=600000001 tok_s=25.0 turn=2 usd=0.0615
            cache_hit=0.16666666666666666 cached=50 calls={} decode_ns=400000000 input=300 model_ns=1000000001 outcome="ok" output=10 queue_ns=999999999 request=3 status=200 thread=0 to_first_byte_ns=999999501 to_first_token_ns=600000001 tok_s=25.0 turn=3 usd=0.0915
            by: "step"
            caller: "agent-0"
            totals: {"cache_hit":0.9801271435439928,"cache_write":120,"cache_write_unknown":3,"cached":36694,"cached_unknown":0,"calls":{"Bash":1},"decode_ns":1600000000,"decode_ns_unknown":0,"failures":0,"first_request":0,"in_flight":0,"input":37438,"input_unknown":0,"median_to_first_byte_ns":999999501,"median_to_first_byte_ns_unknown":0,"median_to_first_token_ns":600000001,"median_to_first_token_ns_unknown":0,"model_ns":4000000004,"model_ns_unknown":0,"output":113,"output_unknown":0,"queue_ns":2999999997,"queue_ns_unknown":1,"reasoning":null,"reasoning_unknown":4,"refusals":0,"repeats":0,"retries":0,"span_ns":7000000001,"steps":4,"threads":1,"tok_s":70.625,"turns":3,"usd":1.50252,"usd_unknown":0}

            exchange 0:
            answer: {"kind":"forwarded"}
            api: "messages"
            caller: "agent-0"
            config: {"max_tokens":32000,"model":"glm-5.3","reasoning_effort":null,"stop":[],"stream":true,"temperature":null,"tools":["Bash"]}
            failed: null
            kept: {"request":{"body_bytes":100,"bytes":335,"items":{"count":1,"field":"messages"},"path":"model/0.request","whole":["system","tools"]},"response":{"bytes":1376,"path":"model/0.response"}}
            request: 0
            request_id: null
            stats: {"cache_hit":0.9920191107009066,"cache_write":120,"cached":36544,"calls":{"Bash":1},"decode_ns":400000000,"input":36838,"model_ns":1000000001,"outcome":"ok","output":83,"queue_ns":null,"reasoning":null,"repeat_of":null,"request":0,"retry_of":null,"status":200,"thread":0,"to_first_byte_ns":999999501,"to_first_token_ns":600000001,"tok_s":207.5,"turn":0,"usd":1.31802}
            status: 200
            times: {"end_ns":"2000000000","first_byte_ns":"1999999500","first_token_ns":"1600000000","start_ns":"999999999"}
            upstream: "zai"
            usage: {"cache_write_tokens":120,"cached_tokens":36544,"input_tokens":36838,"output_tokens":83}

            diff of steps 2 and 3's system: {"a":2,"b":3,"hunks":[{"at":[0,0],"lines":[[" ","You are a Claude agent."],["-","Assist with the task."],["+","Assist with the task, briefly."]]}],"lines":[2,2],"part":"system","same":false}"##]],
    );
}
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the record's exchanges and the snapshot"
)]
fn follows_a_trace() {
    let scratch = Scratch::new();
    let id = "20261110-180000-00000013";
    let dir = scratch.0.join("runs").join(id);
    let mut model = Model::new(&dir);
    let rest = || json!({ "model": "glm-5.3", "stream": true });
    let event = |data: Value| {
        format!(
            "event: {}\ndata: {data}\n\n",
            data["type"].as_str().unwrap()
        )
    };
    let done = |item: Value| {
        event(json!({ "type": "response.output_item.done", "output_index": 0, "item": item }))
    };
    let kept = model.keep(
        0,
        "input",
        &[user("hi")],
        &[],
        rest(),
        Some(&done(said("hello"))),
    );
    // Request 2 sends request 1's list again while 1 is in flight, and is answered first.
    let again = model.keep(
        2,
        "input",
        &[user("hi"), said("hello"), user("more")],
        &[],
        rest(),
        Some(&done(said("again"))),
    );
    let log = [
        started(1, "agent-0", 0),
        answered(2, "agent-0", 0, 200, kept, Some("responses")),
        started(3, "agent-0", 1),
        started(4, "agent-0", 2),
        answered(5, "agent-0", 2, 200, again, Some("responses")),
    ];
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    write_run(
        &scratch,
        id,
        since.checked_sub(Duration::from_secs(60)).unwrap(),
        &log,
        None,
        &lease_places(Prices::Both),
    );
    let driver = File::open(dir.join("driver.pid")).unwrap();
    driver.lock().unwrap();
    // Request 1 is in flight, its answer begun, its last line torn.
    let response = dir.join("model/1.response");
    let begun = done(said("part one"));
    fs::write(&response, format!("{begun}data: {{\"type\":\"resp")).unwrap();
    let served = Served::start(&scratch);
    let run = format!("/api/runs/{id}/trace/agent-0");
    let mut said_ = Vec::new();
    let first = served.get(&format!("{run}/steps"));
    entries_counted(&served, &run, &first);
    said_.push(format!("in flight:\n{}", columns_text(&first, "steps")));
    let entries = served.get(&format!("{run}/entries?from=1&to=1"));
    said_.push(format!("its answer so far:\n{}", entries_text(&entries)));
    let end = first["end"].as_u64().unwrap();
    let nothing = served.get(&format!("{run}/steps?since={end}&log_epoch=0"));
    said_.push(format!("nothing new:\n{}", columns_text(&nothing, "steps")));
    let whole = json!({ "type": "response.completed",
        "response": { "status": "completed", "output": [said("part one"), said("part two")] } });
    fs::write(&response, format!("{begun}{}", event(whole))).unwrap();
    let kept = model.keep(
        1,
        "input",
        &[user("hi"), said("hello"), user("more")],
        &[],
        rest(),
        None,
    );
    // Request 1 fails, so request 2, answered before it, becomes its retry.
    let line = answered(6, "agent-0", 1, 429, kept, Some("responses"));
    let mut file = OpenOptions::new()
        .append(true)
        .open(dir.join("log.jsonl"))
        .unwrap();
    // Request 3 calls a tool, whose output request 4 sends.
    let call =
        json!({ "type": "function_call", "call_id": "c1", "name": "exec", "arguments": "{}" });
    let output = json!({ "type": "function_call_output", "call_id": "c1", "output": "a.txt" });
    let mut asked = vec![
        user("hi"),
        said("hello"),
        user("more"),
        said("again"),
        user("run it"),
    ];
    let kept = model.keep(3, "input", &asked, &[], rest(), Some(&done(call.clone())));
    let mut lines = vec![line, started(7, "agent-0", 3)];
    lines.push(answered(8, "agent-0", 3, 200, kept, Some("responses")));
    let mut append = |lines: &mut [Value], seq: usize| {
        for (at, line) in lines.iter_mut().enumerate() {
            line["seq"] = json!(seq + at);
            writeln!(file, "{line}").unwrap();
        }
    };
    append(&mut lines, log.len() + 1);
    let ended = served.get(&format!("{run}/steps?since={end}&log_epoch=0"));
    entries_counted(&served, &run, &ended);
    said_.push(format!("once answered:\n{}", columns_text(&ended, "steps")));
    let entries = served.get(&format!("{run}/entries?from=1&to=1"));
    said_.push(format!("its entries:\n{}", entries_text(&entries)));
    let entries = served.get(&format!("{run}/entries?from=3&to=3"));
    said_.push(format!(
        "a call with no output yet:\n{}",
        entries_text(&entries)
    ));
    asked.extend([call, output]);
    let kept = model.keep(4, "input", &asked, &[], rest(), Some(&done(said("done"))));
    let mut lines = [
        started(9, "agent-0", 4),
        answered(10, "agent-0", 4, 200, kept, Some("responses")),
    ];
    append(&mut lines, log.len() + 4);
    drop(driver);
    let since = ended["end"].as_u64().unwrap();
    let output = served.get(&format!("{run}/steps?since={since}&log_epoch=0"));
    entries_counted(&served, &run, &output);
    said_.push(format!(
        "its output sent:\n{}",
        columns_text(&output, "steps")
    ));
    let entries = served.get(&format!("{run}/entries?from=3&to=3"));
    said_.push(format!("the call's output:\n{}", entries_text(&entries)));
    let stale = served.get_with(&format!("{run}/steps?since={end}&log_epoch=1"), &[]);
    said_.push(format!("of another epoch: {}\n{}", stale.0, stale.1));
    check(
        &scratch,
        said_.into_iter().map(Step::Ran),
        false,
        &[(id, "<id>")],
        &expect![[r#"
            in flight:
            added=1 added_entries=1 answer="forwarded" answers=1 api="responses" back_bytes=189 cached=50 calls=[] end_ns="2000000000" first_byte_ns="1999999500" first_token_ns="1600000000" input=0 items=1 kept_bytes=117 model_ns=1000000001 output=10 request=0 sent_bytes=100 start_ns="999999999" status=200 thread=0 turn=0 upstream="zai" whole=false
            answers=1 calls=[] request=1 start_ns="2999999999" whole=false
            added=2 added_entries=1 answer="forwarded" answers=1 api="responses" back_bytes=189 cache_hit=0.25 cached=50 calls=[] differs=[] end_ns="5000000000" first_byte_ns="4999999500" first_token_ns="4600000000" input=200 items=3 kept_bytes=265 model_ns=1000000001 output=10 parent=0 request=2 sent_bytes=100 start_ns="3999999999" status=200 thread=0 turn=2 upstream="zai" whole=false
            caller: "agent-0"
            end: 1581
            live: true
            log_epoch: 0
            thread_spans: {"span_ns":[4000000001],"thread":[0]}
            threads: 1
            tool_lists: []

            its answer so far:
            {"at":"1/out/0","digest":null,"index":0,"kind":"assistant","request":1,"side":"answer","text":"part one"}
            next: null
            nothing new:
            caller: "agent-0"
            end: 1581
            live: true
            log_epoch: 0
            thread_spans: {"span_ns":[],"thread":[]}
            threads: 1
            tool_lists: []

            once answered:
            added=2 added_entries=1 answer="forwarded" answers=2 api="responses" back_bytes=0 cache_hit=0.5 cached=50 calls=[] differs=[] end_ns="6000000000" first_byte_ns="5999999500" first_token_ns="5600000000" input=100 items=3 kept_bytes=265 model_ns=3000000001 output=10 parent=0 request=1 sent_bytes=100 start_ns="2999999999" status=429 stop="completed" thread=0 turn=1 upstream="zai" whole=true
            added=2 added_entries=1 answer="forwarded" answers=1 api="responses" back_bytes=189 cache_hit=0.25 cached=50 calls=[] differs=[] end_ns="5000000000" first_byte_ns="4999999500" first_token_ns="4600000000" input=200 items=3 kept_bytes=265 model_ns=1000000001 output=10 parent=0 request=2 retry_count=1 retry_of=1 sent_bytes=100 start_ns="3999999999" status=200 thread=0 turn=1 upstream="zai" whole=false
            added=2 added_entries=1 answer="forwarded" answers=1 api="responses" back_bytes=172 cache_hit=0.16666666666666666 cached=50 calls=["exec"] differs=[] end_ns="8000000000" first_byte_ns="7999999500" first_token_ns="7600000000" input=300 items=5 kept_bytes=413 model_ns=1000000001 output=10 parent=2 request=3 sent_bytes=100 start_ns="6999999999" status=200 thread=0 turn=3 upstream="zai" whole=false
            caller: "agent-0"
            end: 2726
            live: true
            log_epoch: 0
            thread_spans: {"span_ns":[7000000001],"thread":[0]}
            threads: 1
            tool_lists: []

            its entries:
            {"at":"1/in/2/0","digest":"0000000000000000000000000000000000000000000000000000000000000002","index":0,"kind":"user","request":1,"side":"added","text":"more"}
            {"at":"1/out/0","digest":null,"index":1,"kind":"assistant","request":1,"side":"answer","text":"part one"}
            {"at":"1/out/1","digest":null,"index":2,"kind":"assistant","request":1,"side":"answer","text":"part two"}
            next: null
            a call with no output yet:
            {"at":"3/in/4/0","digest":"0000000000000000000000000000000000000000000000000000000000000004","index":0,"kind":"user","request":3,"side":"added","text":"run it"}
            {"at":"3/out/0","call_id":"c1","digest":null,"index":1,"kind":"call","name":"exec","request":3,"side":"answer","text":"{}"}
            next: null
            its output sent:
            added=2 added_entries=1 answer="forwarded" answers=1 api="responses" back_bytes=172 cache_hit=0.16666666666666666 cached=50 calls=["exec"] differs=[] end_ns="8000000000" first_byte_ns="7999999500" first_token_ns="7600000000" input=300 items=5 kept_bytes=413 model_ns=1000000001 output=10 parent=2 request=3 sent_bytes=100 start_ns="6999999999" status=200 thread=0 turn=3 upstream="zai" whole=false
            added=2 added_entries=1 answer="forwarded" answers=1 api="responses" back_bytes=188 cache_hit=0.125 cached=50 calls=[] differs=[] end_ns="10000000000" first_byte_ns="9999999500" first_token_ns="9600000000" input=400 items=7 kept_bytes=561 model_ns=1000000001 output=10 parent=3 request=4 sent_bytes=100 start_ns="8999999999" status=200 thread=0 turn=3 upstream="zai" whole=false
            caller: "agent-0"
            end: 3373
            live: false
            log_epoch: 0
            thread_spans: {"span_ns":[9000000001],"thread":[0]}
            threads: 1
            tool_lists: []

            the call's output:
            {"at":"3/in/4/0","digest":"0000000000000000000000000000000000000000000000000000000000000004","index":0,"kind":"user","request":3,"side":"added","text":"run it"}
            {"at":"3/out/0","call_id":"c1","digest":null,"index":1,"kind":"call","name":"exec","output":"4/in/6/0","request":3,"side":"answer","text":"{}"}
            next: null
            of another epoch: HTTP/1.1 409 Conflict
            {"error":"run <id>'s log started over since the reply that gave byte 1581: read the run again, and its log from byte 0","log_epoch":0}"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the requests' parts and the snapshot"
)]
fn diffs_two_requests() {
    let scratch = Scratch::new();
    let id = "20261110-190000-00000014";
    let dir = scratch.0.join("runs").join(id);
    let mut model = Model::new(&dir);
    let exec = json!({ "type": "function", "name": "exec" });
    let grep = json!({ "type": "function", "name": "grep", "parameters": { "type": "object" } });
    let numbered = |changed: &[usize]| {
        let lines = (1..=20).map(|n| {
            if changed.contains(&n) {
                format!("line {n}, changed")
            } else {
                format!("line {n}")
            }
        });
        json!(lines.collect::<Vec<_>>().join("\n"))
    };
    let too_long = json!(vec!["a"; 5001].join("\n"));
    let sent = [
        (Some(json!("a\nb\nc")), json!([exec])),
        (Some(json!("a\nb\nc")), json!([exec])),
        (Some(json!("x\ny\nz")), json!([exec])),
        (Some(json!("new\na\nb\nc\nend")), json!([exec, grep])),
        (None, json!([exec])),
        (Some(too_long), json!([exec])),
        (Some(numbered(&[])), json!([exec])),
        (Some(numbered(&[2, 18])), json!([exec])),
        (Some(numbered(&[10, 14])), json!([exec])),
    ];
    let mut log = Vec::new();
    for (n, (instructions, tools)) in (0..).zip(sent) {
        let items: Vec<Value> = (0..=n).map(|at| user(&format!("u{at}"))).collect();
        let mut whole = vec![("tools", tools)];
        whole.extend(instructions.map(|text| ("instructions", text)));
        let response = json!({ "status": "completed", "output": [said("ok")] }).to_string();
        let kept = model.keep(n, "input", &items, &whole, json!({}), Some(&response));
        log.push(started(n * 2 + 1, "agent-0", n));
        log.push(answered(
            n * 2 + 2,
            "agent-0",
            n,
            200,
            kept,
            Some("responses"),
        ));
    }
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    write_run(
        &scratch,
        id,
        since.checked_sub(Duration::from_secs(60)).unwrap(),
        &log,
        Some(json!({ "state": "failed", "reason": "stopped" })),
        &lease_places(Prices::Both),
    );
    let served = Served::start(&scratch);
    let run = format!("/api/runs/{id}");
    let steps = served.get(&format!("{run}/trace/agent-0/steps"));
    let mut said = vec![format!("differs: {}", steps["steps"]["differs"])];
    for asked in [
        "a=0&b=1&part=system",
        "a=0&b=2&part=system",
        "a=0&b=3&part=system",
        "a=0&b=4&part=system",
        "a=4&b=0&part=system",
        "a=0&b=5&part=system",
        "a=6&b=7&part=system",
        "a=6&b=8&part=system",
        "a=0&b=1&part=tools",
        "a=0&b=3&part=tools",
        "a=0&b=1&part=prompt",
        "a=0&part=system",
        "a=0&b=99&part=system",
    ] {
        let (status, body) = served.get_with(&format!("{run}/diff?{asked}"), &[]);
        said.push(format!("{asked}: {status}\n{body}"));
    }
    check(
        &scratch,
        said.into_iter().map(Step::Ran),
        false,
        &[(id, "<id>")],
        &expect![[r#"
            differs: [null,[],["system"],["system","tools"],["system","tools"],["system"],["system"],["system"],["system"]]
            a=0&b=1&part=system: HTTP/1.1 200 OK
            {"a":0,"b":1,"part":"system","same":true}
            a=0&b=2&part=system: HTTP/1.1 200 OK
            {"a":0,"b":2,"hunks":[{"at":[0,0],"lines":[["-","a"],["-","b"],["-","c"],["+","x"],["+","y"],["+","z"]]}],"lines":[3,3],"part":"system","same":false}
            a=0&b=3&part=system: HTTP/1.1 200 OK
            {"a":0,"b":3,"hunks":[{"at":[0,0],"lines":[["+","new"],[" ","a"],[" ","b"],[" ","c"],["+","end"]]}],"lines":[3,5],"part":"system","same":false}
            a=0&b=4&part=system: HTTP/1.1 200 OK
            {"a":0,"b":4,"hunks":[{"at":[0,0],"lines":[["-","a"],["-","b"],["-","c"]]}],"lines":[3,0],"part":"system","same":false}
            a=4&b=0&part=system: HTTP/1.1 200 OK
            {"a":4,"b":0,"hunks":[{"at":[0,0],"lines":[["+","a"],["+","b"],["+","c"]]}],"lines":[0,3],"part":"system","same":false}
            a=0&b=5&part=system: HTTP/1.1 200 OK
            {"a":0,"b":5,"lines":[3,5001],"part":"system","same":false,"too_long":"too long to diff: 3 and 5001 lines, past 5000 a side"}
            a=6&b=7&part=system: HTTP/1.1 200 OK
            {"a":6,"b":7,"hunks":[{"at":[0,0],"lines":[[" ","line 1"],["-","line 2"],["+","line 2, changed"],[" ","line 3"],[" ","line 4"],[" ","line 5"]]},{"at":[14,14],"lines":[[" ","line 15"],[" ","line 16"],[" ","line 17"],["-","line 18"],["+","line 18, changed"],[" ","line 19"],[" ","line 20"]]}],"lines":[20,20],"part":"system","same":false}
            a=6&b=8&part=system: HTTP/1.1 200 OK
            {"a":6,"b":8,"hunks":[{"at":[6,6],"lines":[[" ","line 7"],[" ","line 8"],[" ","line 9"],["-","line 10"],["+","line 10, changed"],[" ","line 11"],[" ","line 12"],[" ","line 13"],["-","line 14"],["+","line 14, changed"],[" ","line 15"],[" ","line 16"],[" ","line 17"]]}],"lines":[20,20],"part":"system","same":false}
            a=0&b=1&part=tools: HTTP/1.1 200 OK
            {"a":0,"b":1,"part":"tools","same":true}
            a=0&b=3&part=tools: HTTP/1.1 200 OK
            {"a":0,"b":3,"hunks":[{"at":[1,1],"lines":[[" ","  \"name\": \"exec\","],[" ","  \"type\": \"function\""],[" ","}"],["+","{"],["+","  \"name\": \"grep\","],["+","  \"parameters\": {"],["+","    \"type\": \"object\""],["+","  },"],["+","  \"type\": \"function\""],["+","}"]]}],"lines":[4,11],"part":"tools","same":false}
            a=0&b=1&part=prompt: HTTP/1.1 400 Bad Request
            {"error":"`part` is `prompt`: expected `system` or `tools`"}
            a=0&part=system: HTTP/1.1 400 Bad Request
            {"error":"give a request's number: `?b=<request>`"}
            a=0&b=99&part=system: HTTP/1.1 404 Not Found
            {"error":"run <id>'s log names no model request 99"}"#]],
    );
}

fn call(id: &str) -> Value {
    json!({ "type": "function_call", "call_id": id, "name": "exec", "arguments": "{}" })
}

fn output(id: &str) -> Value {
    json!({ "type": "function_call_output", "call_id": id, "output": format!("ran {id}") })
}

fn rows(reply: &Value) -> std::collections::BTreeMap<u64, Value> {
    let columns = reply["steps"].as_object().unwrap();
    let requests = columns["request"].as_array().unwrap();
    let row = |at: usize| {
        let cells = columns
            .iter()
            .map(|(name, column)| (name.clone(), column[at].clone()));
        cells.collect::<serde_json::Map<_, _>>()
    };
    (requests.iter().enumerate())
        .map(|(at, request)| (request.as_u64().unwrap(), Value::Object(row(at))))
        .collect()
}

fn thread_spans(reply: &Value) -> std::collections::BTreeMap<u64, Value> {
    let spans = &reply["thread_spans"];
    let threads = spans["thread"].as_array().unwrap().iter();
    let threads = threads.map(|thread| thread.as_u64().unwrap());
    threads
        .zip(spans["span_ns"].as_array().unwrap().clone())
        .collect()
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the record's exchanges and the snapshot"
)]
fn places_steps_whatever_their_answers_order() {
    let scratch = Scratch::new();
    let (live, done) = ("20261110-200000-00000015", "20261110-200000-00000016");
    let judge = user("judge it");
    let one = [judge.clone(), call("c1"), output("c1")];
    let zero = [judge.clone(), call("c0"), output("c0")];
    let mut waits = zero.to_vec();
    waits.extend([call("c3"), user("wait")]);
    let mut ran = waits.clone();
    ran.extend([said("waiting"), output("c3")]);
    let longer = [user("ask"), user("more")];
    let longest = [user("ask"), user("more"), user("most")];
    // Two identical roots, each answered with its own call and continued by its own child; a
    // call whose output comes two steps on, in a request that fails and is retried; and a step
    // that leaves its thread for a longer root answered after it.
    let requests: [(&[Value], u64, Value); 10] = [
        (std::slice::from_ref(&judge), 200, json!([call("c0")])),
        (std::slice::from_ref(&judge), 200, json!([call("c1")])),
        (&one, 200, json!([said("one")])),
        (&zero, 200, json!([call("c3")])),
        (&waits, 200, json!([said("waiting")])),
        (&ran, 429, json!([])),
        (&ran, 200, json!([said("done")])),
        (&longer, 200, json!([said("longer")])),
        (&longer[..1], 200, json!([said("shorter")])),
        (&longest, 200, json!([said("longest")])),
    ];
    let mut kept = Vec::new();
    for dir in [live, done] {
        let mut model = Model::new(&scratch.0.join("runs").join(dir));
        kept = (0..)
            .zip(&requests)
            .map(|(n, (items, _, answer))| {
                let response = json!({ "status": "completed", "output": answer }).to_string();
                model.keep(n, "input", items, &[], json!({}), Some(&response))
            })
            .collect();
    }
    // Each root answered after the other's child, as two samples of a call run at once.
    let order = [1, 0, 3, 2, 4, 5, 6, 8, 9, 7];
    let mut lines: Vec<Value> = (0..10).map(|n| started(n + 1, "job-0", n)).collect();
    for (at, n) in (0..).zip(order) {
        let at_n = usize::try_from(n).unwrap();
        let (kept, status) = (kept[at_n].clone(), requests[at_n].1);
        lines.push(answered(
            10 + at,
            "job-0",
            n,
            status,
            kept,
            Some("responses"),
        ));
    }
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let started_at = since.checked_sub(Duration::from_secs(60)).unwrap();
    let places = lease_places(Prices::Both);
    write_run(
        &scratch,
        done,
        started_at,
        &lines,
        Some(json!({ "state": "failed", "reason": "stopped" })),
        &places,
    );
    let dir = write_run(&scratch, live, started_at, &lines, None, &places);
    let log_path = dir.join("log.jsonl");
    // The live run's log grows a line at a time, from the first answer on.
    let whole = fs::read_to_string(&log_path).unwrap();
    let whole: Vec<&str> = whole.split_inclusive('\n').collect();
    let first = whole.len() - order.len();
    fs::write(&log_path, whole[..first].concat()).unwrap();
    let driver = File::open(dir.join("driver.pid")).unwrap();
    driver.lock().unwrap();
    let served = Served::start(&scratch);
    let trace = |run: &str| format!("/api/runs/{run}/trace/job-0");
    let first_reply = served.get(&format!("{}/steps", trace(live)));
    let (mut page, mut end) = (rows(&first_reply), first_reply["end"].clone());
    let mut spans = thread_spans(&first_reply);
    let mut said = Vec::new();
    for line in &whole[first..] {
        let mut file = OpenOptions::new().append(true).open(&log_path).unwrap();
        file.write_all(line.as_bytes()).unwrap();
        let reply = served.get(&format!("{}/steps?since={end}&log_epoch=0", trace(live)));
        let sent = rows(&reply);
        said.push(format!("sent again: {:?}", sent.keys().collect::<Vec<_>>()));
        page.extend(sent);
        spans.extend(thread_spans(&reply));
        end = reply["end"].clone();
        // The held trace, extended by each line, says what a view started now reads in one pass.
        let fresh = Served::start(&scratch).get(&format!("{}/steps", trace(live)));
        assert_eq!(served.get(&format!("{}/steps", trace(live))), fresh);
    }
    drop(driver);
    // The page that replaced each row sent again holds what a fresh read of the log says.
    let followed = served.get(&format!("{}/steps", trace(live)));
    let read = served.get(&format!("{}/steps", trace(done)));
    assert_eq!(page, rows(&read));
    let fresh = thread_spans(&read);
    spans.retain(|thread, _| fresh.contains_key(thread));
    assert_eq!(spans, fresh);
    assert_eq!(followed, read);
    let entries = |run: &str| served.get(&format!("{}/entries?from=0&to=6", trace(run)));
    assert_eq!(entries(live), entries(done));
    let shown = [
        "request",
        "thread",
        "parent",
        "turn",
        "retry_of",
        "repeat_of",
        "added",
    ];
    for (_, row) in rows(&read) {
        let cells = shown.map(|name| format!("{name}={}", row[name]));
        said.push(cells.join(" "));
    }
    for entry in entries(done)["entries"].as_array().unwrap() {
        let paired = entry.get("call").or(entry.get("output"));
        said.push(format!(
            "{} {}{}",
            entry["at"].as_str().unwrap(),
            entry["kind"].as_str().unwrap(),
            paired.map_or_else(String::new, |paired| format!(" paired with {paired}"))
        ));
    }
    check(
        &scratch,
        said.into_iter().map(Step::Ran),
        false,
        &[],
        &expect![[r#"
            sent again: [1]
            sent again: [0, 1]
            sent again: [0, 3]
            sent again: [1, 2]
            sent again: [4]
            sent again: [3, 5]
            sent again: [6]
            sent again: [8]
            sent again: [9]
            sent again: [7, 8, 9]
            request=0 thread=0 parent=null turn=0 retry_of=null repeat_of=null added=1
            request=1 thread=0 parent=null turn=0 retry_of=null repeat_of=0 added=1
            request=2 thread=0 parent=1 turn=0 retry_of=null repeat_of=null added=2
            request=3 thread=0 parent=0 turn=0 retry_of=null repeat_of=null added=2
            request=4 thread=0 parent=3 turn=4 retry_of=null repeat_of=null added=2
            request=5 thread=0 parent=4 turn=4 retry_of=null repeat_of=null added=2
            request=6 thread=0 parent=4 turn=4 retry_of=5 repeat_of=null added=2
            request=7 thread=7 parent=null turn=7 retry_of=null repeat_of=null added=2
            request=8 thread=8 parent=null turn=8 retry_of=null repeat_of=null added=1
            request=9 thread=7 parent=7 turn=9 retry_of=null repeat_of=null added=1
            0/in/0/0 user
            0/out/0 call paired with "3/in/2/0"
            1/in/0/0 user
            1/out/0 call paired with "2/in/2/0"
            2/in/2/0 output paired with "1/out/0"
            2/out/0 assistant
            3/in/2/0 output paired with "0/out/0"
            3/out/0 call paired with "5/in/6/0"
            4/in/4/0 user
            4/out/0 assistant
            5/in/6/0 output paired with "3/out/0"
            6/in/6/0 output paired with "3/out/0"
            6/out/0 assistant"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the reads and their snapshot"
)]
fn holds_each_live_trace_and_the_last_three_ended() {
    let scratch = Scratch::new();
    let (ended, live) = ("20261110-210000-00000017", "20261110-210000-00000018");
    let callers = ["agent-0", "job-0", "job-1", "job-2"];
    let response = json!({ "status": "completed", "output": [said("done")] }).to_string();
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let failed = json!({ "state": "failed", "reason": "stopped" });
    for (id, outcome) in [(ended, Some(failed)), (live, None)] {
        let mut model = Model::new(&scratch.0.join("runs").join(id));
        let log: Vec<Value> = (0..)
            .zip(callers)
            .map(|(n, caller)| {
                let items = [user(&format!("ask {caller}"))];
                let kept = model.keep(n, "input", &items, &[], json!({}), Some(&response));
                answered(n + 1, caller, n, 200, kept, Some("responses"))
            })
            .collect();
        let places = lease_places(Prices::Both);
        write_run(&scratch, id, since, &log, outcome, &places);
    }
    let driver = File::open(scratch.0.join("runs").join(live).join("driver.pid")).unwrap();
    driver.lock().unwrap();
    let served = Served::start(&scratch);
    let path = |id: &str, caller: &str| format!("/api/runs/{id}/trace/{caller}/steps");
    // Each caller's held read costs the bytes of its request alone.
    let mut held = std::collections::BTreeMap::new();
    for id in [ended, live] {
        for caller in callers {
            served.get(&path(id, caller));
            let before = served.read_bytes();
            let reply = served.get(&path(id, caller));
            held.insert((id, caller), (served.read_bytes() - before, reply));
        }
    }
    let mut said = Vec::new();
    let asked = [
        (ended, "job-1"),
        (live, "agent-0"),
        (ended, "job-0"),
        (ended, "job-2"),
        (live, "job-0"),
        (ended, "agent-0"),
        (live, "job-1"),
        (ended, "job-1"),
        (live, "job-2"),
    ];
    for (id, caller) in asked {
        let before = served.read_bytes();
        let reply = served.get(&path(id, caller));
        let (bytes, first) = &held[&(id, caller)];
        assert_eq!(&reply, first, "{id} {caller}");
        let read = served.read_bytes() - before != *bytes;
        let run = if id == live { "live" } else { "ended" };
        said.push(format!(
            "{run} run's {caller} asked: its record read again: {read}"
        ));
    }
    check(
        &scratch,
        said.into_iter().map(Step::Ran),
        false,
        &[],
        &expect![[r#"
            ended run's job-1 asked: its record read again: false
            live run's agent-0 asked: its record read again: false
            ended run's job-0 asked: its record read again: false
            ended run's job-2 asked: its record read again: false
            live run's job-0 asked: its record read again: false
            ended run's agent-0 asked: its record read again: true
            live run's job-1 asked: its record read again: false
            ended run's job-1 asked: its record read again: true
            live run's job-2 asked: its record read again: false"#]],
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the log of the ops")]
fn times_a_callers_ops() {
    let scratch = Scratch::new();
    let id = "20261110-220000-00000018";
    let arrived = |seconds: u64, caller: &str, op: u64, request: Value, actions: Value| {
        let event = json!({ "kind": "op_arrived", "caller": caller, "op": op, "request": request });
        logged(seconds, event, actions)
    };
    let reply = |op: u64, reply: Value| json!({ "kind": "reply", "op": op, "reply": reply });
    let ended = |seconds: u64, event: Value| logged(seconds, event, json!([]));
    let call = json!({ "kind": "call", "grant": "train" });
    let log = [
        arrived(
            1,
            "agent-0",
            0,
            json!({ "kind": "status", "job": "0" }),
            json!([]),
        ),
        // The first call waits for a slot: its job starts on a later line.
        arrived(
            2,
            "agent-0",
            1,
            call.clone(),
            json!([reply(1, json!({ "kind": "call", "job": 0 }))]),
        ),
        ended(3, json!({ "kind": "call_started", "job": 0 })),
        arrived(
            4,
            "agent-0",
            2,
            call.clone(),
            json!([reply(
                2,
                json!({ "kind": "refused", "message": "one call at a time" })
            )]),
        ),
        arrived(
            5,
            "agent-0",
            3,
            json!({ "kind": "now", "grant": "final", "evaluated": {
                "effects": ["set_submission"], "returned": [{ "name": "state", "value": "stored" }],
            } }),
            json!([reply(
                3,
                json!({ "kind": "call", "returned": { "state": "stored" } })
            )]),
        ),
        arrived(
            6,
            "job-1",
            4,
            json!({ "kind": "lease_run", "lease": 0 }),
            json!([reply(4, json!({ "kind": "lease", "lease": 0 })),
            { "kind": "run_command", "lease": 0, "command": 1 }]),
        ),
        arrived(
            7,
            "agent-0",
            5,
            json!({ "kind": "lease_put", "lease": 2 }),
            json!([reply(5, json!({ "kind": "lease", "lease": 2 })),
            { "kind": "transfer", "lease": 2, "transfer": 0 }]),
        ),
        ended(
            8,
            json!({ "kind": "transfer_ended", "lease": 2, "transfer": 0, "end": { "moved": { "bytes": 840, "entries": 3 } } }),
        ),
        arrived(
            9,
            "agent-0",
            6,
            json!({ "kind": "lease_run", "lease": 2 }),
            json!([reply(6, json!({ "kind": "lease", "lease": 2 })),
            { "kind": "run_command", "lease": 2, "command": 1 }]),
        ),
        // Job 1's command 1 on its lease 0 is not the agent's on lease 2.
        ended(
            10,
            json!({ "kind": "command_ended", "lease": 0, "command": 1, "end": { "exited": { "code": 0 } } }),
        ),
        ended(
            11,
            json!({
                "kind": "call_returned", "job": 0,
                "returned": [{ "name": "score", "value": 14 }],
            }),
        ),
        ended(
            12,
            json!({ "kind": "command_ended", "lease": 2, "command": 1, "end": "timed_out" }),
        ),
        arrived(
            13,
            "agent-0",
            7,
            call.clone(),
            json!([reply(7, json!({ "kind": "call", "job": 1 }))]),
        ),
        arrived(
            14,
            "agent-0",
            8,
            call,
            json!([reply(8, json!({ "kind": "call", "job": 2 }))]),
        ),
        ended(
            15,
            json!({ "kind": "call_failed", "job": 2, "reason": "the sandbox was lost" }),
        ),
        arrived(
            16,
            "agent-0",
            9,
            json!({ "kind": "lease_get", "lease": 2 }),
            json!([reply(9, json!({ "kind": "lease", "lease": 2 })),
            { "kind": "transfer", "lease": 2, "transfer": 1 }]),
        ),
        ended(
            17,
            json!({ "kind": "transfer_ended", "lease": 2, "transfer": 1, "end": { "moved": { "bytes": 96, "entries": 2 } } }),
        ),
        // A now function's body that returned nothing.
        arrived(
            18,
            "agent-0",
            10,
            json!({ "kind": "now", "grant": "final", "evaluated": { "effects": [], "returned": [] } }),
            json!([reply(10, json!({ "kind": "call", "returned": {} }))]),
        ),
    ];
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    write_run(&scratch, id, since, &log, None, &lease_places(Prices::Both));
    let served = Served::start(&scratch);
    let said = ["agent-0", "job-1"].map(|caller| {
        let reply = served.get(&format!("/api/runs/{id}/trace/{caller}/ops"));
        format!("{caller}:\n{}", columns_text(&reply, "ops"))
    });
    check(
        &scratch,
        said.into_iter().map(Step::Ran),
        false,
        &[],
        &expect![[r#"
            agent-0:
            end_ns="11000000000" failed=false kind="call" name="train" op=1 result="returned score=14" start_ns="2000000000"
            end_ns="4000000000" failed=true kind="call" name="train" op=2 result="refused: one call at a time" start_ns="4000000000"
            end_ns="5000000000" failed=false kind="now" name="final" op=3 result="returned state=\"stored\"" start_ns="5000000000"
            end_ns="8000000000" failed=false kind="lease_put" name="lease 2" op=5 result="moved 840 bytes, 3 entries" start_ns="7000000000"
            end_ns="12000000000" failed=true kind="lease_run" name="lease 2" op=6 result="timed_out" start_ns="9000000000"
            failed=false kind="call" name="train" op=7 start_ns="13000000000"
            end_ns="15000000000" failed=true kind="call" name="train" op=8 result="failed: the sandbox was lost" start_ns="14000000000"
            end_ns="17000000000" failed=false kind="lease_get" name="lease 2" op=9 result="moved 96 bytes, 2 entries" start_ns="16000000000"
            end_ns="18000000000" failed=false kind="now" name="final" op=10 result="returned" start_ns="18000000000"
            caller: "agent-0"

            job-1:
            end_ns="10000000000" failed=false kind="lease_run" name="lease 0" op=4 result="exit 0" start_ns="6000000000"
            caller: "job-1"
        "#]],
    );
}

// A call that returns the most its body may, and one that fails with as much: each reply that
// says how they ended cuts it short.
#[test]
fn cuts_a_long_call_end_short() {
    let scratch = Scratch::new();
    let id = "20261110-230000-00000019";
    let most = usize::try_from(RESULT_MAX_BYTES).unwrap();
    let output = "a".repeat(most - 64);
    let values = json!([{ "name": "output", "value": output }, { "name": "score", "value": 1 }]);
    let reason = "a line of stdout\n".repeat(most / 17);
    let ends = [
        json!({ "kind": "call_returned", "job": 0, "returned": values }),
        json!({ "kind": "call_failed", "job": 1, "reason": reason }),
    ];
    let mut log = Vec::new();
    for (job, end) in (0_u64..).zip(ends) {
        let arrived = json!({
            "kind": "op_arrived", "caller": "agent-0", "op": job,
            "request": { "kind": "call", "grant": "train" },
        });
        let start = json!([{ "kind": "reply", "op": job, "reply": { "kind": "call", "job": job } }, {
            "kind": "start_call", "caller": "agent-0", "job": job, "grant": "train",
            "payload": payload(), "timeout_ns": "60000000000",
        }]);
        log.extend([
            logged(1, arrived, start),
            logged(1, json!({ "kind": "call_started", "job": job }), json!([])),
            logged(2, end, json!([])),
        ]);
    }
    let outcome = Some(returned(6.0));
    let places = lease_places(Prices::Nothing);
    write_run(&scratch, id, asked(id, 0), &log, outcome, &places);
    let served = Served::start(&scratch);
    let (_, body) = served.get_with(&format!("/api/runs/{id}"), &[]);
    // Four cut reasons of 8 KiB, and some kilobytes of the rest.
    assert!(body.len() < 64 * 1024, "{} bytes", body.len());
    let run: Value = serde_json::from_str(&body).unwrap();
    let ops = served.get(&format!("/api/runs/{id}/trace/agent-0/ops"));
    let summary = run["summary"].as_str().unwrap();
    let mut said = String::new();
    for job in 0..2 {
        let called = summary
            .lines()
            .find(|line| line.contains(&format!("train job {job}")));
        let lane = &run["lanes"][1 + job];
        said.push_str(&ending("summary", called.unwrap().trim_start()));
        said.push_str(&ending(
            lane["lane"].as_str().unwrap(),
            lane["said"].as_str().unwrap(),
        ));
        said.push_str(&ending("op", ops["ops"]["result"][job].as_str().unwrap()));
    }
    said.push_str(&ending(
        "failure",
        run["failures"][0]["reason"].as_str().unwrap(),
    ));
    check(
        &scratch,
        [Step::Ran(said)],
        false,
        &[],
        &expect![[r#"
            summary: `0:01 train job 0 of agent 0 on modal/a100_80 wit`, 233 bytes, ending `aaaaaaaaaaaaaaaaaaaaaaa…`
            job 0: `train job 0 of agent 0 on modal/a100_80 with unk`, 228 bytes, ending `aaaaaaaaaaaaaaaaaaaaaaa…`
            op: `returned output="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa`, 152 bytes, ending `aaaaaaaaaaaaaaaaaaaaaaa…`
            summary: `0:01 train job 1 of agent 0 on modal/a100_80 wit`, 8293 bytes, ending `t\na line of stdout\na …`
            job 1: `train job 1 of agent 0 on modal/a100_80 with unk`, 8288 bytes, ending `t\na line of stdout\na …`
            op: `failed: a line of stdout\na line of stdout\na li`, 8203 bytes, ending `t\na line of stdout\na …`
            failure: `a line of stdout
            a line of stdout
            a line of stdo`, 8218 bytes, ending `… [1048560 bytes in all]`
        "#]],
    );
}

/// `text` by its start, its length in bytes and its end, after `name`, on a line.
fn ending(name: &str, text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let start: String = chars[..chars.len().min(48)].iter().collect();
    let end: String = chars[chars.len().saturating_sub(24)..].iter().collect();
    format!("{name}: `{start}`, {} bytes, ending `{end}`\n", text.len())
}
