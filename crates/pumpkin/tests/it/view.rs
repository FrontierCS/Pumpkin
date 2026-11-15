//! `view`, which serves the runs' records as JSON on the loopback interface: the list of runs,
//! a run's page, the poll of a live run, which answers from what its log gained, and windows of
//! its log; and the page that reads them, its files served without the token.

use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead as _, BufReader, Read as _, Write as IoWrite};
use std::net::TcpStream;
use std::process::{Child, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use expect_test::expect;
use serde_json::{Value, json};

use super::{
    Scratch, Step, asked, check, cut, judged, judged_log, lease_places, logged, poll, write_record,
    write_run,
};

/// A `pumpkin view` of `scratch`'s `runs`, killed as it is dropped.
struct Served {
    child: Child,
    port: u16,
    token: String,
    /// What the view printed as it started, its port and token as `<port>` and `<token>`.
    said: String,
}

impl Served {
    fn start(scratch: &Scratch) -> Self {
        let mut command = scratch.pumpkin(&["view", "--runs", "runs"]);
        command.stdout(Stdio::null()).stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let mut said = String::new();
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        for _ in 0..2 {
            stderr.read_line(&mut said).unwrap();
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

    /// The status line and the body of a `GET` of `path` with `headers`, as [`Self::ask`].
    fn get_with(&self, path: &str, headers: &[(&str, &str)]) -> (String, String) {
        self.ask("GET", path, headers)
    }

    /// The status line and the body of a request of `path` by `method` with `headers`, as
    /// [`Self::exchange`] sends them.
    fn ask(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> (String, String) {
        let (head, body) = self.exchange(method, path, headers);
        (head.lines().next().unwrap().to_owned(), body)
    }

    /// The head and the body of the answer to a request of `path` by `method` with `headers`, as
    /// `Host: 127.0.0.1` and the token are given unless `headers` names them.
    fn exchange(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> (String, String) {
        let (head, body) = self.exchange_bytes(method, path, headers);
        (head, String::from_utf8(body).unwrap())
    }

    /// The head and the body's bytes of the answer, as [`Self::exchange`].
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

    fn get(&self, path: &str) -> Value {
        let (status, body) = self.get_with(path, &[]);
        assert_eq!(status, "HTTP/1.1 200 OK", "{path}: {body}");
        serde_json::from_str(&body).unwrap()
    }

    /// How many bytes the view has read, of every file and socket, as Linux counts them.
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

/// `value`'s fields, one a line, as `name: json`, and each element of an array field `lines`
/// names on a line of its own.
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

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the requests and their snapshot"
)]
fn serves_runs() {
    let scratch = Scratch::new("serves-runs");
    let priced = lease_places(true);
    let ended = "20261110-090000-0000000a";
    let mut outcome = judged(49.36, json!({ "raw_score": 49.36 }));
    outcome["usage"] = json!({
        "ran_ns": "1100000000000", "model_requests": 4,
        "tokens": { "responses": 2, "input": 3000, "output": 300, "cached": 2600 },
        "sandboxes": [{ "provider": "aws", "class": "judge", "count": 2, "lasted_ns": "1",
            "usd_per_hour": 0.3672, "usd": 0.11 }, { "provider": "aws", "class": "box",
            "count": 1, "lasted_ns": "1" }],
        "usd": 0.11,
    });
    let dir = write_run(
        &scratch,
        ended,
        asked(ended, 30),
        &judged_log(),
        Some(outcome),
        &priced,
    );
    let mut routed = priced.clone();
    routed["routes"] = json!({ "solver": [{ "api": "responses", "upstream": "zai",
        "models": { "glm-5.3": { "upstream_name": "glm-5.3-flash" } } }] });
    fs::write(dir.join("provider.json"), routed.to_string()).unwrap();
    let lost = "20261110-100000-0000000b";
    write_run(
        &scratch,
        lost,
        asked(lost, 30),
        &cut(&judged_log(), 130),
        None,
        &priced,
    );
    let live = "20261110-110000-0000000c";
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let dir = write_run(
        &scratch,
        live,
        since.checked_sub(Duration::from_secs(200)).unwrap(),
        &cut(&judged_log(), 130),
        None,
        &priced,
    );
    let driver = File::open(dir.join("driver.pid")).unwrap();
    driver.lock().unwrap();
    write_record(&scratch, "20261110-120000-0000000d", true, None);
    // A run whose agent made no model request used no tokens; one whose requests none of the
    // responses reported their tokens for used some that no one counted.
    for (id, requests) in [
        ("20261110-070000-00000008", 0),
        ("20261110-080000-00000009", 3),
    ] {
        let mut outcome = json!({
            "state": "finished",
            "verdict": { "kind": "no_submission" },
            "work_end": { "kind": "deadline" },
        });
        outcome["usage"] = json!({
            "ran_ns": "60000000000", "model_requests": requests,
            "tokens": { "responses": 0, "input": 0, "output": 0 }, "sandboxes": [],
        });
        write_record(&scratch, id, true, Some(outcome));
    }
    fs::create_dir_all(scratch.0.join("runs/20261110-130000-0000000e")).unwrap();
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
    // The summary is what `wait` says of the run.
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
    let refusals: [(&str, &[(&str, &str)]); 6] = [
        ("/api/runs", &[("X-Pumpkin-Token", "")]),
        ("/api/runs", &[("X-Pumpkin-Token", "0123")]),
        ("/api/runs", &[("Host", "pumpkin.example:80")]),
        ("/api/runs/run-1", &[]),
        ("/api/runs/20261110-140000-0000000f", &[]),
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
              {"bound":"-","id":"20261110-120000-0000000d","live":false,"requests":null,"rest":"-","scored":false,"state":"lost","time":"-","tokens":null,"usd":"-"}
              {"bound":"of 30m","id":"<live>","live":true,"requests":3,"rest":"example/lease: # model requests, the last answered #m##s ago","scored":false,"state":"agent","time":"<t>","tokens":{"cached":2600,"input":3000,"output":300},"usd":"$0.06+"}
              {"bound":"-","id":"20261110-100000-0000000b","live":false,"requests":3,"rest":"example/lease","scored":false,"state":"lost","time":"2m10s","tokens":{"cached":2600,"input":3000,"output":300},"usd":"$0.06+"}
              {"bound":"49.36","id":"<ended>","live":false,"requests":4,"rest":"example/lease","scored":true,"state":"finished","time":"18m20s","tokens":{"cached":2600,"input":3000,"output":300},"usd":"$0.11+"}
              {"bound":"-","id":"20261110-080000-00000009","live":false,"requests":3,"rest":"-","scored":false,"state":"finished","time":"1m00s","tokens":{"cached":null,"input":null,"output":null},"usd":"-"}
              {"bound":"-","id":"20261110-070000-00000008","live":false,"requests":0,"rest":"-","scored":false,"state":"finished","time":"1m00s","tokens":{"cached":null,"input":0,"output":0},"usd":"-"}

            GET /api/runs?before=<live>
            {"next":null,"runs":[{"bound":"-","id":"20261110-100000-0000000b","live":false,"requests":3,"rest":"example/lease","scored":false,"state":"lost","time":"2m10s","tokens":{"cached":2600,"input":3000,"output":300},"usd":"$0.06+"},{"bound":"49.36","id":"<ended>","live":false,"requests":4,"rest":"example/lease","scored":true,"state":"finished","time":"18m20s","tokens":{"cached":2600,"input":3000,"output":300},"usd":"$0.11+"},{"bound":"-","id":"20261110-080000-00000009","live":false,"requests":3,"rest":"-","scored":false,"state":"finished","time":"1m00s","tokens":{"cached":null,"input":null,"output":null},"usd":"-"},{"bound":"-","id":"20261110-070000-00000008","live":false,"requests":0,"rest":"-","scored":false,"state":"finished","time":"1m00s","tokens":{"cached":null,"input":0,"output":0},"usd":"-"}]}
            GET /api/runs/<ended>
            again: {"command":"pumpkin run --runs <scratch>/runs <scratch>/task.star"}
            end: 8220
            id: "<ended>"
            lanes:
              {"answered":[10.0,20.0,31.0],"answers":{"count":3,"failed":0},"failed":[],"kind":"agent","lane":"agent","said":"agent on aws/judge, started at 0:05, exited 0 at 16:40; the work phase ended at 16:40","sandbox":{"asked":0.0,"deleted":true,"gone":1002.0,"place":"aws/judge","started":5.0}}
              {"answered":[],"answers":{"count":0,"failed":0},"failed":[],"grant":"train","kind":"judge","lane":"job 0","model":{"dropped_headers":[],"failed":[],"first_token":{"count":0,"median":null},"in_flight":[],"more_dropped_headers":false,"not_2xx":0,"requests":0,"tokens":{"cached":null,"input":0,"output":0},"took":{"count":0,"median":null}},"role":"trainer","said":"train judge job 0 on modal/a100_80 with unknown GPUs, started after 6s, ran 54s: 49.53","sandbox":{"asked":40.0,"deleted":true,"gone":100.0,"place":"modal/a100_80 with unknown GPUs","started":46.0}}
              {"grant":"box","holder":"job 0","kind":"runner","lane":"lease 0","role":"box","said":"box lease 0 of job 0 on aws/box, 52s: charged 45s, 5s of it busy, 2 commands, the last `sh -c set -e\\nprint 7` exited 0; closed as its call ended","sandbox":{"asked":50.0,"deleted":true,"gone":102.0,"place":"aws/box","started":55.0},"work":[{"end":null,"from":56.0,"kind":"put","label":"/pumpkin/in/payload → /work/in","to":null},{"end":{"exited":{"code":0}},"from":58.0,"kind":"command","label":"sh -c set -e\nprint 7","to":62.0}]}
              {"grant":"gpu","holder":"agent","kind":"runner","lane":"lease 1","role":"gpu","said":"gpu lease 1 on modal/a100_80 with NVIDIA A100-SXM4-80GB, 25s: charged 14s, 2s of it busy, 1 command, the last `nvidia-smi` exited 0; closed by its holder","sandbox":{"asked":120.0,"deleted":true,"gone":145.0,"place":"modal/a100_80 with NVIDIA A100-SXM4-80GB","started":126.0},"work":[{"end":{"exited":{"code":0}},"from":130.0,"kind":"command","label":"nvidia-smi","to":132.0}]}
              {"grant":"cpu","holder":"agent","kind":"runner","lane":"lease 2","role":"cpu","said":"cpu lease 2 on aws/judge, 14m45s: charged 14m10s, 30s of it busy, 1 command, the last `sleep 60` timed out; closed as the work phase ended","sandbox":{"asked":120.0,"deleted":true,"gone":1005.0,"place":"aws/judge","started":150.0},"work":[{"end":"timed_out","from":151.0,"kind":"command","label":"sleep 60","to":181.0}]}
              {"answered":[],"answers":{"count":0,"failed":0},"failed":[],"grant":"train","kind":"judge","lane":"job 1","model":{"dropped_headers":[],"failed":[],"first_token":{"count":0,"median":null},"in_flight":[],"more_dropped_headers":false,"not_2xx":0,"requests":0,"tokens":{"cached":null,"input":0,"output":0},"took":{"count":0,"median":null}},"role":"trainer","said":"train judge job 1 on modal/a100_80, never started, ended after 1m: failed: the judge's sandbox didn't start: Modal refused the helper's `create`: ResourceExhaustedError: Insufficient capacity to create sandbox: fix what Modal reports","sandbox":{"asked":200.0,"deleted":null,"gone":260.0,"place":"modal/a100_80","started":null}}
              {"answered":[],"answers":{"count":0,"failed":0},"failed":[],"grant":"train","kind":"judge","lane":"job 2","model":{"dropped_headers":[],"failed":[],"first_token":{"count":0,"median":null},"in_flight":[],"more_dropped_headers":false,"not_2xx":0,"requests":0,"tokens":{"cached":null,"input":0,"output":0},"took":{"count":0,"median":null}},"role":"trainer","said":"train judge job 2 on modal/a100_80 with NVIDIA A100-SXM4-80GB, started after 10s, ran 20s: failed: the call was stopped (stopped)","sandbox":{"asked":300.0,"deleted":true,"gone":330.0,"place":"modal/a100_80 with NVIDIA A100-SXM4-80GB","started":310.0}}
              {"answered":[],"answers":{"count":0,"failed":0},"failed":[],"grant":"train","kind":"judge","lane":"job 3","model":{"dropped_headers":[],"failed":[],"first_token":{"count":0,"median":null},"in_flight":[],"more_dropped_headers":false,"not_2xx":0,"requests":0,"tokens":{"cached":null,"input":0,"output":0},"took":{"count":0,"median":null}},"role":"trainer","said":"train judge job 3 on modal/a100_80 with unknown GPUs, started after 5s, ran 10s: no score, returned {\"exit_status\":1,\"stderr_tail\":\"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx… (timed out)","sandbox":{"asked":400.0,"deleted":true,"gone":415.0,"place":"modal/a100_80 with unknown GPUs","started":405.0}}
              {"grant":"gpu","holder":"agent","kind":"runner","lane":"lease 3","role":"gpu","said":"gpu lease 3 on modal/a100_80 with NVIDIA A100 80GB PCIe (its role needs NVIDIA A100-SXM4-80GB), 1m10s: charged 1m, 1s of it busy, 1 command, the last `true` exited 0; expired","sandbox":{"asked":500.0,"deleted":true,"gone":570.0,"place":"modal/a100_80 with NVIDIA A100 80GB PCIe (its role needs NVIDIA A100-SXM4-80GB)","started":505.0},"work":[{"end":{"exited":{"code":0}},"from":506.0,"kind":"command","label":"true","to":507.0}]}
              {"answered":[1020.0],"answers":{"count":1,"failed":1},"failed":[1020.0],"grant":null,"kind":"judge","lane":"job 4","model":{"dropped_headers":["x-stainless-lang","x-title"],"failed":[{"code":"rate_limit_exceeded","count":1}],"first_token":{"count":0,"median":null},"in_flight":[],"more_dropped_headers":false,"not_2xx":0,"requests":1,"tokens":{"cached":null,"input":null,"output":null},"took":{"count":0,"median":null}},"role":"scorer","said":"final judge job 4 on modal/a100_80 with NVIDIA A100-SXM4-80GB, started after 4s, ran 1m30s: 49.36","sandbox":{"asked":1006.0,"deleted":true,"gone":1100.0,"place":"modal/a100_80 with NVIDIA A100-SXM4-80GB","started":1010.0}}
            live: false
            log_epoch: 0
            model: {"dropped_headers":["x-stainless-lang","x-title"],"failed":[{"code":"rate_limit_exceeded","count":1}],"first_token":{"count":2,"median":1.375},"in_flight":[],"more_dropped_headers":false,"not_2xx":0,"requests":4,"tokens":{"cached":2600,"input":3000,"output":300},"took":{"count":3,"median":5.0}}
            other_build: null
            scored: true
            state: "finished"
            summary: "<as `wait` says it>"
            talk: {"edges":[{"back_bytes":null,"bytes":null,"count":1,"from":"scorer","kind":"model request","to":"model"},{"back_bytes":null,"bytes":null,"count":1,"from":"solver","kind":"command","to":"cpu"},{"back_bytes":null,"bytes":null,"count":1,"from":"solver","kind":"lease","to":"cpu"},{"back_bytes":null,"bytes":null,"count":2,"from":"solver","kind":"command","to":"gpu"},{"back_bytes":null,"bytes":null,"count":2,"from":"solver","kind":"lease","to":"gpu"},{"back_bytes":null,"bytes":null,"count":3,"from":"solver","kind":"model request","to":"model"},{"back_bytes":null,"bytes":null,"count":1,"from":"solver","kind":"submission","to":"scorer"},{"back_bytes":null,"bytes":null,"count":4,"from":"solver","kind":"judge call","to":"trainer"},{"back_bytes":null,"bytes":null,"count":1,"from":"trainer","kind":"command","to":"box"},{"back_bytes":null,"bytes":null,"count":1,"from":"trainer","kind":"lease","to":"box"},{"back_bytes":null,"bytes":1,"count":1,"from":"trainer","kind":"tree","to":"box"}],"roles":[{"kind":"agent","name":"solver","place":"aws/judge","routed":true,"routes":[{"api":"responses","models":{"glm-5.3":"glm-5.3-flash"},"upstream":"zai"}],"score":false},{"kind":null,"name":"trainer","place":"modal/a100_80","routed":false,"routes":[],"score":false},{"kind":null,"name":"scorer","place":"modal/a100_80","routed":false,"routes":[],"score":true},{"kind":null,"name":"gpu","place":"modal/a100_80","routed":false,"routes":[],"score":false},{"kind":null,"name":"cpu","place":"aws/judge","routed":false,"routes":[],"score":false},{"kind":null,"name":"box","place":"aws/box","routed":false,"routes":[],"score":false}]}
            task: "example/lease"
            times: {"end":1100.0,"hard_end":7200.0,"last":1100.0,"now":null,"started_unix":1794301230.0}
            unread: null
            verdict: "Run <ended> finished with a score of 49.36, after the work phase ended when the agent exited with status 0"

            GET /api/runs/<ended>/since?log=<end>
            {"end":8220,"live":false}
            GET /api/runs/<ended>/since?log=0
            end 8220, live false, next 8220, run "<ended>", 56 records, the first {"actions":[{"kind":"start_agent"}],"at_ns":"0","byte":0,"event":{"hard_end_ns":"7200000000000","kind":"run_started","unix_ns":"1794301230000000000"},"seq":0,"when":0.0}
            GET /api/runs/<ended>/log?at=7085
            next 8220, end 8220, bytes [Number(7085), Number(7228), Number(7359), Number(7448), Number(7566), Number(7872)], the last's event "judge_collected"
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
            {"error":"runs/run-1 is not a directory: expected the directory of a run's record"}
            GET /api/runs/20261110-140000-0000000f []
            HTTP/1.1 404 Not Found
            {"error":"runs/20261110-140000-0000000f is not a directory: expected the directory of a run's record"}
            GET /api/runs/<ended>/since?log=7 []
            HTTP/1.1 400 Bad Request
            {"error":"byte 7 starts no line of run <ended>'s log, which is read to byte 8220: expected 0, or a byte that an answer gave"}"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the log that grows, and the polls that follow it"
)]
fn follows_a_live_run() {
    let scratch = Scratch::new("follows-a-live-run");
    let id = "20261110-150000-00000010";
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let dir = write_run(
        &scratch,
        id,
        since.checked_sub(Duration::from_secs(60)).unwrap(),
        &[],
        None,
        &lease_places(true),
    );
    let driver = File::open(dir.join("driver.pid")).unwrap();
    driver.lock().unwrap();
    // 3,000 lines more, of model requests started and answered, each pair a second on.
    let mut grown = Vec::new();
    for request in 0..1500 {
        let caller = json!({ "caller": "agent", "request": request });
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
    // 250 lines at once, more than an answer holds, of which the first 150 are past what the
    // view holds, and are read again.
    log.write_all(lines[1..251].concat().as_bytes()).unwrap();
    for _ in 0..3 {
        let answer = poll(&mut next, &mut taken);
        said.push(format!(
            "after 250 lines: {} records, next {}, end {}",
            answer["records"].as_array().unwrap().len(),
            answer["next"],
            answer["end"]
        ));
    }
    // Then in 29 parts of at most 97 lines, each polled with the half of its last line written.
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
    assert_eq!(taken, lines.concat());
    // Each byte of the log once, and per poll no more than its own request and a few small files.
    assert!(
        read >= size && read < size + 2048 * polls,
        "{read} bytes read over {polls} polls of a log of {size}"
    );
    scratch.write(
        &format!("runs/{id}/outcome.json"),
        &json!({ "state": "failed" }),
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
    said.push(format!("then: {}\n{}", nothing.0, nothing.1));
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

            after 250 lines: 100 records, next 14054, end 35231
            after 250 lines: 100 records, next 28106, end 35231
            after 250 lines: 50 records, next 35231, end 35231
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
            then: HTTP/1.1 200 OK
            {"end":431109,"live":true}
            once ended: {"end":431109,"live":false}"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the requests and their snapshot"
)]
fn cancels_a_run() {
    let scratch = Scratch::new("cancels-a-run");
    let id = "20261110-160000-00000011";
    write_record(&scratch, id, true, None);
    // A driver: the process that holds the record's lock, which ignores SIGTERM, so that a
    // cancel goes on until the test kills it.
    let pid_path = scratch.0.join(format!("runs/{id}/driver.pid"));
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
    ask("GET", &cancelling, &[]);
    let unknown = "/api/runs/20261110-170000-00000012/cancel";
    ask("POST", unknown, &[("Origin", &page)]);
    let from_page = |steps: &mut Vec<String>| {
        let (status, _) = served.ask("POST", &cancel, &[("Origin", &page)]);
        // What it has said so far depends on how far the child has got.
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
    // Once it has signalled the driver, which it waits for then.
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
            {"error":"run <id> is cancelled by `POST`, which the page's cancel sends: cancel it there, or run: pumpkin cancel --runs <scratch>/runs <id>"}
            POST /api/runs [("Origin", "<page>")]
            HTTP/1.1 405 Method Not Allowed
            {"error":"`/api/runs` is read by `GET`: only a run's cancel, `/api/runs/<id>/cancel`, takes `POST`"}
            GET /api/runs/<id>/cancelling []
            HTTP/1.1 404 Not Found
            {"error":"the page started no cancel of run <id>: cancel it from the page, by POST /api/runs/<id>/cancel"}
            POST /api/runs/20261110-170000-00000012/cancel [("Origin", "<page>")]
            HTTP/1.1 404 Not Found
            {"error":"runs/20261110-170000-00000012 is not a directory: expected the directory of a run's record"}
            POST /api/runs/<id>/cancel from the page
            HTTP/1.1 202 Accepted
            POST /api/runs/<id>/cancel from the page while that goes on
            HTTP/1.1 409 Conflict
            {"error":"the page's cancel of run <id> goes on: read what it says: GET /api/runs/<id>/cancelling"}
            GET /api/runs/<id>/cancelling once ended: exit 0
            pumpkin: stopping run <id>, recorded in <scratch>/runs/<id>: it ends once its sandboxes are deleted
            pumpkin: run <id> was lost, as its driver ended without an outcome.json: it is now recorded as failed
            {
              "reason": "its driver ended without an outcome",
              "state": "failed"
            }
            pumpkin: run <id> failed: its driver ended without an outcome; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star

            then: "Run <id> failed: its driver ended without an outcome"
            POST /api/runs/<id>/cancel from the page
            HTTP/1.1 202 Accepted
            GET /api/runs/<id>/cancelling once ended: exit 0
            {
              "reason": "its driver ended without an outcome",
              "state": "failed"
            }
            pumpkin: run <id> failed: its driver ended without an outcome; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star

            the workspace's tmp/ then holds: []"#]],
    );
}

#[test]
fn starts_over_a_shortened_log() {
    let scratch = Scratch::new("starts-over");
    let id = "20261110-180000-00000013";
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let dir = write_run(
        &scratch,
        id,
        since.checked_sub(Duration::from_secs(200)).unwrap(),
        &judged_log(),
        None,
        &lease_places(true),
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
    let records = served.get(&format!("/api/runs/{id}/since?log=0"))["records"].clone();
    let byte = &records[20]["byte"];
    // The log cut back to its first 30 lines, so that the byte still starts one.
    let log_path = dir.join("log.jsonl");
    let log = fs::read_to_string(&log_path).unwrap();
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
    steps.push(format!(
        "then GET /api/runs/<id>: end {}, log_epoch {}",
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
            GET /api/runs/<id>: end 8220, log_epoch 0, a build that kept no routes: ["\"solver\" \"unknown\"", "\"trainer\" \"unknown\"", "\"scorer\" \"unknown\"", "\"gpu\" \"unknown\"", "\"cpu\" \"unknown\"", "\"box\" \"unknown\""]
            GET /api/runs/<id>/since?log=3003&log_epoch=0
            HTTP/1.1 409 Conflict
            {"error":"run <id>'s log started over since the reply that gave byte 3003: read the run again, and its log from byte 0","log_epoch":1}
            GET /api/runs/<id>/log?at=3003&log_epoch=0
            HTTP/1.1 409 Conflict
            {"error":"run <id>'s log started over since the reply that gave byte 3003: read the run again, and its log from byte 0","log_epoch":1}
            then GET /api/runs/<id>: end 4269, log_epoch 1
            GET /api/runs/<id>/since?log=3003&log_epoch=1
            end 4269, log_epoch 1, 10 records"#]],
    );
}

/// The paths of the modules that a file of the page loads, and of the files its style names.
fn loaded(body: &str) -> Vec<String> {
    let found = body
        .match_indices(" from \"./")
        .chain(body.match_indices("url(\"/"));
    found
        .map(|(at, _)| {
            let rest = &body[at..];
            let file = rest[rest.find('/').unwrap() + 1..]
                .split('"')
                .next()
                .unwrap();
            format!("/{file}")
        })
        .collect()
}

#[test]
fn serves_the_page() {
    let scratch = Scratch::new("serves-the-page");
    fs::create_dir_all(scratch.0.join("runs")).unwrap();
    let served = Served::start(&scratch);
    // The page reads the token from its address, so its own files are served without it.
    let no_token = [("X-Pumpkin-Token", "")];
    let mut steps = Vec::new();
    let mut paths = ["/", "/style.css", "/app.js"].map(str::to_owned).to_vec();
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
        steps.push(format!(
            "{path}: {} {}; {}",
            head.lines().next().unwrap(),
            header("Content-Type"),
            header("Cache-Control"),
        ));
        assert_eq!(header("Content-Length"), body.len().to_string(), "{path}");
        // Each file carries the policy, shown once.
        let policy = format!("policy: {}", header("Content-Security-Policy"));
        if path == "/" {
            steps.push(policy.clone());
        }
        assert_eq!(steps[1], policy, "{path}");
        let font = header("Content-Type") == "font/woff2";
        assert!(!font || body.starts_with(b"wOF2"), "{path}");
        for file in loaded(&String::from_utf8_lossy(&body)) {
            if !paths.contains(&file) {
                paths.push(file);
            }
        }
    }
    for (path, headers) in [
        ("/nothing.js", &[][..]),
        ("/", &[("Host", "pumpkin.example")][..]),
        ("/app.js", &[("Host", "pumpkin.example:80")][..]),
    ] {
        let (status, body) = served.get_with(path, headers);
        steps.push(format!("{path} {headers:?}: {status} {body}"));
    }
    check(
        &scratch,
        steps.into_iter().map(Step::Ran),
        false,
        &[],
        &expect![[r#"
            /: HTTP/1.1 200 OK text/html; charset=utf-8; no-store
            policy: default-src 'none'; script-src 'self'; style-src 'self'; font-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'
            /style.css: HTTP/1.1 200 OK text/css; charset=utf-8; no-store
            /app.js: HTTP/1.1 200 OK text/javascript; charset=utf-8; no-store
            /hanken-grotesk.woff2: HTTP/1.1 200 OK font/woff2; no-store
            /h.js: HTTP/1.1 200 OK text/javascript; charset=utf-8; no-store
            /list.js: HTTP/1.1 200 OK text/javascript; charset=utf-8; no-store
            /run.js: HTTP/1.1 200 OK text/javascript; charset=utf-8; no-store
            /nothing.js []: HTTP/1.1 404 Not Found {"error":"nothing is at `/nothing.js`: expected the page at `/`, or the runs at `/api/runs` and below it"}
            / [("Host", "pumpkin.example")]: HTTP/1.1 403 Forbidden {"error":"the request names the host `pumpkin.example`: expected one of 127.0.0.1, localhost, [::1], this machine's loopback, at which the view is served"}
            /app.js [("Host", "pumpkin.example:80")]: HTTP/1.1 403 Forbidden {"error":"the request names the host `pumpkin.example:80`: expected one of 127.0.0.1, localhost, [::1], this machine's loopback, at which the view is served"}"#]],
    );
}

#[test]
fn answers_past_a_connection_that_sends_nothing() {
    let scratch = Scratch::new("answers-past-silence");
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
fn pages_the_list() {
    let scratch = Scratch::new("pages-the-list");
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
        // Each run once, newest first.
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
