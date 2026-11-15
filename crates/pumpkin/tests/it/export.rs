//! `view --export`, which writes the replies that the view's page reads as files that a static
//! copy of it reads: each holds what the view answers its request with, each is written again
//! only once its reply changed, and a live run's export rewrites only what its log changed.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use expect_test::expect;
use serde_json::{Value, json};

use super::view::{Model, Piped, answered, said, started, user};
use super::{Scratch, Step, check, judged, judged_log, lease_places, logged, write_run};

/// The run that [`write_exported`] writes.
const EXPORTED: &str = "20261110-170000-00000012";

/// A run whose `provider.json` routes a role through an API this build doesn't know, so that
/// the view refuses its `run.json` and the export leaves it out.
const UNREADABLE: &str = "20261110-180000-00000013";

/// Writes into `scratch`'s default runs directory run [`EXPORTED`], of [`judged_log`] and then
/// the agent's requests 10 to 12, which call a tool whose output is longer than an entry's text
/// in a window, and change the instructions at the last, and a judge's request 13; it ended if
/// `ended`, else it is live, and the lock of its driver is returned held; the agent's first
/// message says `asked`, and each provider reports `reports`.
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the record's exchanges"
)]
fn write_exported(scratch: &Scratch, ended: bool, asked: &str, reports: &Value) -> Option<File> {
    let mut model = Model::new(&scratch.0.join("runs").join(EXPORTED));
    let rest = || json!({ "model": "glm-5.3", "stream": false });
    let tools = ("tools", json!([{ "type": "function", "name": "exec" }]));
    let brief = [("instructions", json!("be brief")), tools.clone()];
    let call = json!({ "type": "function_call", "call_id": "c1", "name": "exec", "arguments": "{\"cmd\":\"ls\"}" });
    let listing = format!("a.txt\n{}", "b.txt\n".repeat(2000));
    let output = json!({ "type": "function_call_output", "call_id": "c1", "output": listing });
    let answer = |items: Value| json!({ "status": "completed", "output": items }).to_string();
    let first = [user(asked)];
    let second = [user(asked), call.clone(), output];
    let mut third = second.to_vec();
    third.extend([said("A.TXT is there"), user("and now?")]);
    let mut log = judged_log();
    log.push(started(1001, "agent", 10));
    let kept = model.keep(
        10,
        "input",
        &first,
        &brief,
        rest(),
        Some(&answer(json!([call]))),
    );
    log.push(answered(1002, "agent", 10, 200, kept, Some("responses")));
    log.push(started(1003, "agent", 11));
    let kept = model.keep(
        11,
        "input",
        &second,
        &brief,
        rest(),
        Some(&answer(json!([said("A.TXT is there")]))),
    );
    log.push(answered(1004, "agent", 11, 200, kept, Some("responses")));
    log.push(started(1005, "agent", 12));
    let briefer = [("instructions", json!("be briefer")), tools];
    let kept = model.keep(
        12,
        "input",
        &third,
        &briefer,
        rest(),
        Some(&answer(json!([]))),
    );
    log.push(answered(1006, "agent", 12, 200, kept, Some("responses")));
    let judging = [
        json!({ "role": "system", "content": "judge it" }),
        json!({ "role": "user", "content": "is a.txt right?" }),
    ];
    let yes = json!({ "choices": [{ "index": 0, "finish_reason": "stop",
        "message": { "role": "assistant", "content": "yes" } }] });
    let kept = model.keep(
        13,
        "messages",
        &judging,
        &[],
        rest(),
        Some(&yes.to_string()),
    );
    log.push(started(1007, "job-0", 13));
    log.push(answered(1008, "job-0", 13, 200, kept, Some("chat")));
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let outcome = ended.then(|| judged(49.36, json!({ "raw_score": 49.36 })));
    let started = since.checked_sub(Duration::from_secs(1100)).unwrap();
    let mut places = lease_places(true);
    for (provider, report) in reports.as_object().unwrap() {
        places["providers"][provider]["report"] = report.clone();
    }
    write_run(scratch, EXPORTED, started, &log, outcome, &places);
    let runs = scratch.0.join(".pumpkin/runs");
    fs::rename(scratch.0.join("runs"), &runs).unwrap();
    let driver = File::open(runs.join(EXPORTED).join("driver.pid")).unwrap();
    (!ended).then(|| {
        driver.lock().unwrap();
        driver
    })
}

/// Two providers' reports that name no account.
fn reports() -> Value {
    json!({ "aws": {}, "modal": {} })
}

/// What `pumpkin view --export public` of `scratch` said, as user `exporter-user`, at home in
/// `/home/exporter`.
fn export(scratch: &Scratch) -> String {
    let mut export = scratch.pumpkin(&["view", "--export", "public"]);
    export
        .env("HOME", "/home/exporter")
        .env("USER", "exporter-user");
    let output = export.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stderr).unwrap()
}

/// Each file below `dir`, by its path there, as it holds it.
fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut dirs = vec![dir.to_owned()];
    while let Some(at) = dirs.pop() {
        for entry in fs::read_dir(at).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                let key = path.strip_prefix(dir).unwrap().to_str().unwrap().to_owned();
                files.insert(key, fs::read(&path).unwrap());
            }
        }
    }
    files
}

/// Each file of the export in `dir`, by its key, as it holds it: the page's and `robots.txt`
/// as they are, every other inflated.
fn exported(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = files(dir);
    for (key, bytes) in &mut files {
        if key.starts_with("runs") {
            *bytes = miniz_oxide::inflate::decompress_to_vec_zlib(bytes).unwrap();
        }
    }
    files
}

/// The request that the file at `key` answers, as the export module's doc lists them; `None` for
/// a file that answers none of the view's.
fn target(key: &str) -> Option<String> {
    if key == "runs.json" {
        return Some("/api/runs".to_owned());
    }
    let (id, file) = key.strip_prefix("runs/")?.split_once('/')?;
    let api = format!("/api/runs/{id}");
    let parts: Vec<&str> = file.trim_end_matches(".json").split('/').collect();
    let asked = match parts[..] {
        ["run"] => api,
        ["log", at] => format!("{api}/log?at={at}"),
        [
            "trace",
            caller,
            "stats-step" | "stats-turn" | "stats-thread",
        ] => {
            let by = parts[2].strip_prefix("stats-")?;
            format!("{api}/trace/{caller}/stats?by={by}")
        }
        ["trace", caller, what @ ("steps" | "ops")] => format!("{api}/trace/{caller}/{what}"),
        ["trace", caller, "entries", from] => {
            let (n, entry) = from.split_once('-')?;
            format!("{api}/trace/{caller}/entries?from={n}&to={n}&entry={entry}")
        }
        ["trace", caller, "entry", ..] => {
            format!("{api}/trace/{caller}/entry?at={}", parts[3..].join("/"))
        }
        ["exchange", n] => format!("{api}/exchange/{n}"),
        ["diff", pair] => {
            let [a, b, part] = pair.splitn(3, '-').collect::<Vec<_>>()[..] else {
                return None;
            };
            format!("{api}/diff?a={a}&b={b}&part={part}")
        }
        ["kept", "items", hex] => format!("{api}/item/{hex}"),
        ["kept", response] => {
            let n = response.strip_suffix(".response")?;
            format!("{api}/exchange/{n}/response")
        }
        _ => return None,
    };
    Some(asked)
}

/// The places of `query` in caller `caller`'s trace of `files`, an export's, as a substring of its
/// chunks' text: each entry's address, index and kind, and the byte.
fn chunks_found(files: &BTreeMap<String, Vec<u8>>, caller: &str, query: &str) -> Vec<Value> {
    let query = query.to_ascii_lowercase();
    let mut found = Vec::new();
    let mut chunk = Some(0);
    while let Some(at) = chunk {
        let key = format!("runs/{EXPORTED}/trace/{caller}/search/{at}.json");
        let read: Value = serde_json::from_slice(&files[&key]).unwrap();
        for step in read["steps"].as_array().unwrap() {
            for entry in step["entries"].as_array().unwrap() {
                let text = entry[3].as_str().unwrap();
                for (byte, _) in text.match_indices(&query) {
                    found.push(json!([entry[1], entry[0], entry[2], byte]));
                }
            }
        }
        chunk = read["next"].as_u64();
    }
    found
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the snapshot")]
fn exports_what_the_view_answers() {
    let scratch = Scratch::new("exports-what-the-view-answers");
    write_exported(&scratch, true, "list the files", &reports());
    let mut said = vec![export(&scratch)];
    let files = exported(&scratch.0.join("public"));
    let mut piped = Piped::start(&scratch);
    let mut keys = String::new();
    let page = files
        .keys()
        .filter(|key| key.contains('/') && !key.starts_with("runs/"));
    let _ = writeln!(
        keys,
        "the page: index.html, and {} files below its digest",
        page.count()
    );
    for (key, body) in files
        .iter()
        .filter(|(key, _)| key.starts_with("runs") || !key.contains('/'))
    {
        if key == "index.html" {
            continue;
        }
        let answers = target(key).map(|target| {
            let (head, reply) = piped.ask(&target, false);
            assert_eq!(head["status"], "200 OK", "{target}");
            assert!(
                *body == reply,
                "{key} holds other than the reply to {target}"
            );
            target
        });
        let _ = writeln!(keys, "{key}: {}", answers.as_deref().unwrap_or("-"));
    }
    said.push(keys);
    let index = String::from_utf8(files["index.html"].clone()).unwrap();
    assert!(index.contains("<html lang=\"en\" data-source=\"static\">"));
    // The view's policy, but `frame-ancestors`, which a `<meta>` can't say.
    let policy = "default-src 'none'; script-src 'self'; style-src 'self'; font-src 'self'; \
         connect-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'";
    let meta = format!("<meta http-equiv=\"Content-Security-Policy\" content=\"{policy}\">");
    assert!(index.contains(&meta), "{index}");
    // Each request kept names items that the export holds.
    for (key, body) in files.iter().filter(|(key, _)| key.ends_with(".request")) {
        let kept: Value = serde_json::from_slice(body).unwrap();
        let items = (kept["conversation"]["items"].as_array().unwrap().iter())
            .chain(kept["fields"].as_array().unwrap());
        for hex in items.filter_map(|part| part["item"].as_str()) {
            let item = format!("runs/{EXPORTED}/kept/items/{hex}");
            assert!(files.contains_key(&item), "{key} names {item}");
        }
    }
    let request = String::from_utf8(files[&format!("runs/{EXPORTED}/kept/12.request")].clone());
    said.push(format!("request 12, as kept: {}", request.unwrap()));
    for (caller, query) in [("agent", "A.TXT"), ("agent", "a.txt\nb"), ("job-0", "is")] {
        let target = format!("/api/runs/{EXPORTED}/trace/{caller}/search?q={query}");
        let target = target.replace('\n', "%0A");
        let (_, reply) = piped.ask(&target, false);
        let reply: Value = serde_json::from_slice(&reply).unwrap();
        let hits = reply["hits"].as_array().unwrap().iter();
        let searched: Vec<Value> = hits
            .map(|hit| json!([hit["at"], hit["index"], hit["kind"], hit["byte"]]))
            .collect();
        assert_eq!(chunks_found(&files, caller, query), searched, "{target}");
        said.push(format!(
            "{caller}'s {query:?}: {} found, as the view's search finds them",
            searched.len()
        ));
    }
    said.push(export(&scratch));
    let again = exported(&scratch.0.join("public"));
    assert!(
        again == files,
        "a second export changed what the first wrote"
    );
    check(
        &scratch,
        said.into_iter().map(Step::Ran),
        false,
        &[(EXPORTED, "<id>")],
        &expect![[r#"
            pumpkin: exported 1 run into public, and left out 0: wrote 96 files

            the page: index.html, and 19 files below its digest
            robots.txt: -
            runs.json: /api/runs
            runs/<id>/diff/11-12-system.json: /api/runs/<id>/diff?a=11&b=12&part=system
            runs/<id>/exchange/0.json: /api/runs/<id>/exchange/0
            runs/<id>/exchange/1.json: /api/runs/<id>/exchange/1
            runs/<id>/exchange/10.json: /api/runs/<id>/exchange/10
            runs/<id>/exchange/11.json: /api/runs/<id>/exchange/11
            runs/<id>/exchange/12.json: /api/runs/<id>/exchange/12
            runs/<id>/exchange/13.json: /api/runs/<id>/exchange/13
            runs/<id>/exchange/2.json: /api/runs/<id>/exchange/2
            runs/<id>/exchange/3.json: /api/runs/<id>/exchange/3
            runs/<id>/kept/10.request: -
            runs/<id>/kept/10.response: /api/runs/<id>/exchange/10/response
            runs/<id>/kept/11.request: -
            runs/<id>/kept/11.response: /api/runs/<id>/exchange/11/response
            runs/<id>/kept/12.request: -
            runs/<id>/kept/12.response: /api/runs/<id>/exchange/12/response
            runs/<id>/kept/13.request: -
            runs/<id>/kept/13.response: /api/runs/<id>/exchange/13/response
            runs/<id>/kept/items/0000000000000000000000000000000000000000000000000000000000000000: /api/runs/<id>/item/0000000000000000000000000000000000000000000000000000000000000000
            runs/<id>/kept/items/0000000000000000000000000000000000000000000000000000000000000001: /api/runs/<id>/item/0000000000000000000000000000000000000000000000000000000000000001
            runs/<id>/kept/items/0000000000000000000000000000000000000000000000000000000000000002: /api/runs/<id>/item/0000000000000000000000000000000000000000000000000000000000000002
            runs/<id>/kept/items/0000000000000000000000000000000000000000000000000000000000000003: /api/runs/<id>/item/0000000000000000000000000000000000000000000000000000000000000003
            runs/<id>/kept/items/0000000000000000000000000000000000000000000000000000000000000004: /api/runs/<id>/item/0000000000000000000000000000000000000000000000000000000000000004
            runs/<id>/kept/items/0000000000000000000000000000000000000000000000000000000000000005: /api/runs/<id>/item/0000000000000000000000000000000000000000000000000000000000000005
            runs/<id>/kept/items/0000000000000000000000000000000000000000000000000000000000000006: /api/runs/<id>/item/0000000000000000000000000000000000000000000000000000000000000006
            runs/<id>/kept/items/0000000000000000000000000000000000000000000000000000000000000007: /api/runs/<id>/item/0000000000000000000000000000000000000000000000000000000000000007
            runs/<id>/kept/items/0000000000000000000000000000000000000000000000000000000000000008: /api/runs/<id>/item/0000000000000000000000000000000000000000000000000000000000000008
            runs/<id>/kept/items/0000000000000000000000000000000000000000000000000000000000000009: /api/runs/<id>/item/0000000000000000000000000000000000000000000000000000000000000009
            runs/<id>/log/0.json: /api/runs/<id>/log?at=0
            runs/<id>/run.json: /api/runs/<id>
            runs/<id>/trace/agent/entries/0-0.json: /api/runs/<id>/trace/agent/entries?from=0&to=0&entry=0
            runs/<id>/trace/agent/entries/1-0.json: /api/runs/<id>/trace/agent/entries?from=1&to=1&entry=0
            runs/<id>/trace/agent/entries/10-0.json: /api/runs/<id>/trace/agent/entries?from=10&to=10&entry=0
            runs/<id>/trace/agent/entries/11-0.json: /api/runs/<id>/trace/agent/entries?from=11&to=11&entry=0
            runs/<id>/trace/agent/entries/12-0.json: /api/runs/<id>/trace/agent/entries?from=12&to=12&entry=0
            runs/<id>/trace/agent/entries/2-0.json: /api/runs/<id>/trace/agent/entries?from=2&to=2&entry=0
            runs/<id>/trace/agent/entry/11/in/2/0.json: /api/runs/<id>/trace/agent/entry?at=11/in/2/0
            runs/<id>/trace/agent/ops.json: /api/runs/<id>/trace/agent/ops
            runs/<id>/trace/agent/search/0.json: -
            runs/<id>/trace/agent/stats-step.json: /api/runs/<id>/trace/agent/stats?by=step
            runs/<id>/trace/agent/stats-thread.json: /api/runs/<id>/trace/agent/stats?by=thread
            runs/<id>/trace/agent/stats-turn.json: /api/runs/<id>/trace/agent/stats?by=turn
            runs/<id>/trace/agent/steps.json: /api/runs/<id>/trace/agent/steps
            runs/<id>/trace/job-0/entries/13-0.json: /api/runs/<id>/trace/job-0/entries?from=13&to=13&entry=0
            runs/<id>/trace/job-0/ops.json: /api/runs/<id>/trace/job-0/ops
            runs/<id>/trace/job-0/search/0.json: -
            runs/<id>/trace/job-0/stats-step.json: /api/runs/<id>/trace/job-0/stats?by=step
            runs/<id>/trace/job-0/stats-thread.json: /api/runs/<id>/trace/job-0/stats?by=thread
            runs/<id>/trace/job-0/stats-turn.json: /api/runs/<id>/trace/job-0/stats?by=turn
            runs/<id>/trace/job-0/steps.json: /api/runs/<id>/trace/job-0/steps
            runs/<id>/trace/job-1/ops.json: /api/runs/<id>/trace/job-1/ops
            runs/<id>/trace/job-1/search/0.json: -
            runs/<id>/trace/job-1/stats-step.json: /api/runs/<id>/trace/job-1/stats?by=step
            runs/<id>/trace/job-1/stats-thread.json: /api/runs/<id>/trace/job-1/stats?by=thread
            runs/<id>/trace/job-1/stats-turn.json: /api/runs/<id>/trace/job-1/stats?by=turn
            runs/<id>/trace/job-1/steps.json: /api/runs/<id>/trace/job-1/steps
            runs/<id>/trace/job-2/ops.json: /api/runs/<id>/trace/job-2/ops
            runs/<id>/trace/job-2/search/0.json: -
            runs/<id>/trace/job-2/stats-step.json: /api/runs/<id>/trace/job-2/stats?by=step
            runs/<id>/trace/job-2/stats-thread.json: /api/runs/<id>/trace/job-2/stats?by=thread
            runs/<id>/trace/job-2/stats-turn.json: /api/runs/<id>/trace/job-2/stats?by=turn
            runs/<id>/trace/job-2/steps.json: /api/runs/<id>/trace/job-2/steps
            runs/<id>/trace/job-3/ops.json: /api/runs/<id>/trace/job-3/ops
            runs/<id>/trace/job-3/search/0.json: -
            runs/<id>/trace/job-3/stats-step.json: /api/runs/<id>/trace/job-3/stats?by=step
            runs/<id>/trace/job-3/stats-thread.json: /api/runs/<id>/trace/job-3/stats?by=thread
            runs/<id>/trace/job-3/stats-turn.json: /api/runs/<id>/trace/job-3/stats?by=turn
            runs/<id>/trace/job-3/steps.json: /api/runs/<id>/trace/job-3/steps
            runs/<id>/trace/job-4/entries/3-0.json: /api/runs/<id>/trace/job-4/entries?from=3&to=3&entry=0
            runs/<id>/trace/job-4/ops.json: /api/runs/<id>/trace/job-4/ops
            runs/<id>/trace/job-4/search/0.json: -
            runs/<id>/trace/job-4/stats-step.json: /api/runs/<id>/trace/job-4/stats?by=step
            runs/<id>/trace/job-4/stats-thread.json: /api/runs/<id>/trace/job-4/stats?by=thread
            runs/<id>/trace/job-4/stats-turn.json: /api/runs/<id>/trace/job-4/stats?by=turn
            runs/<id>/trace/job-4/steps.json: /api/runs/<id>/trace/job-4/steps

            request 12, as kept: {"conversation":{"field":"input","items":[{"item":"0000000000000000000000000000000000000000000000000000000000000000"},{"item":"0000000000000000000000000000000000000000000000000000000000000003"},{"item":"0000000000000000000000000000000000000000000000000000000000000004"},{"item":"0000000000000000000000000000000000000000000000000000000000000005"},{"item":"0000000000000000000000000000000000000000000000000000000000000006"}]},"fields":[{"item":"0000000000000000000000000000000000000000000000000000000000000007","name":"instructions"},{"name":"model","sent":"glm-5.3"},{"name":"stream","sent":false},{"item":"0000000000000000000000000000000000000000000000000000000000000002","name":"tools"}]}
            agent's "A.TXT": 2 found, as the view's search finds them
            agent's "a.txt\nb": 1 found, as the view's search finds them
            job-0's "is": 1 found, as the view's search finds them
            pumpkin: exported 1 run into public, and left out 0: wrote 0 files
        "#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the fixture and the snapshot"
)]
fn rewrites_what_a_live_run_changed() {
    let scratch = Scratch::new("rewrites-what-a-live-run-changed");
    let mut unreadable = lease_places(true);
    unreadable["routes"] =
        json!({ "solver": [{ "api": "messages", "upstream": "zai", "models": {} }] });
    let started_at = super::asked(UNREADABLE, 0);
    write_run(
        &scratch,
        UNREADABLE,
        started_at,
        &judged_log(),
        None,
        &unreadable,
    );
    let driver = write_exported(&scratch, false, "list the files", &reports());
    let mut said = vec![export(&scratch)];
    let before = exported(&scratch.0.join("public"));
    let list: Value = serde_json::from_slice(&before["runs.json"]).unwrap();
    let listed = list["runs"].as_array().unwrap().iter();
    let listed: Vec<Option<&str>> = listed.map(|run| run["id"].as_str()).collect();
    assert_eq!(
        listed,
        [Some(EXPORTED)],
        "runs.json lists a run that has no run.json"
    );
    let unread = format!("runs/{UNREADABLE}/");
    let written = before.keys().filter(|key| key.starts_with(&unread));
    assert_eq!(written.count(), 0, "the run left out has files");
    let log = scratch
        .0
        .join(format!(".pumpkin/runs/{EXPORTED}/log.jsonl"));
    let lines = fs::read_to_string(&log).unwrap().lines().count();
    let mut grown = started(1009, "agent", 14);
    grown["seq"] = json!(lines);
    writeln!(
        OpenOptions::new().append(true).open(&log).unwrap(),
        "{grown}"
    )
    .unwrap();
    said.push(export(&scratch));
    let after = exported(&scratch.0.join("public"));
    let mut changed = String::from("changed:\n");
    for (key, body) in &after {
        match before.get(key) {
            Some(was) if was == body => {}
            Some(_) => {
                let _ = writeln!(changed, "  {key}");
            }
            None => {
                let _ = writeln!(changed, "  {key}, new");
            }
        }
    }
    said.push(changed);
    drop(driver);
    check(
        &scratch,
        said.into_iter().map(Step::Ran),
        false,
        &[(EXPORTED, "<id>"), (UNREADABLE, "<unreadable>")],
        &expect![[r#"
            pumpkin: left out run <unreadable>: /api/runs/<unreadable>: 500 Internal Server Error: {"error":"<scratch>/.pumpkin/runs/<unreadable>/provider.json: unknown variant `messages`, expected `responses` or `chat` at line 1 column 507"}
            pumpkin: exported 1 run into public, and left out 1: wrote 96 files

            pumpkin: left out run <unreadable>: /api/runs/<unreadable>: 500 Internal Server Error: {"error":"<scratch>/.pumpkin/runs/<unreadable>/provider.json: unknown variant `messages`, expected `responses` or `chat` at line 1 column 507"}
            pumpkin: exported 1 run into public, and left out 1: wrote 14 files

            changed:
              runs.json
              runs/<id>/log/0.json
              runs/<id>/run.json
              runs/<id>/trace/agent/entries/14-0.json, new
              runs/<id>/trace/agent/search/0.json
              runs/<id>/trace/agent/stats-step.json
              runs/<id>/trace/agent/stats-thread.json
              runs/<id>/trace/agent/stats-turn.json
              runs/<id>/trace/agent/steps.json
              runs/<id>/trace/job-0/steps.json
              runs/<id>/trace/job-1/steps.json
              runs/<id>/trace/job-2/steps.json
              runs/<id>/trace/job-3/steps.json
              runs/<id>/trace/job-4/steps.json
        "#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the record's values and the snapshot"
)]
fn replaces_what_names_the_machines() {
    let scratch = Scratch::new("replaces-what-names-the-machines");
    let (ec2, modal) = ("i-0aaaabbbbccccdddd", "sb-AbCdEfGhIjKlMnOpQrStUv");
    let asked = format!(
        "list the files of {ec2} and {modal}, dialled from 3.101.18.71 in account 311100000000, \
         in /home/exporter/work and /home/pumpkin/trees, as exporter-user and pumpkin, on \
         Ubuntu 24.04.5 LTS; i-0123456789abcdef0 is no sandbox of the run's"
    );
    let reports = json!({
        "aws": { "account": "311100000000", "docker": { "os": "Ubuntu 24.04.5 LTS" } },
        "modal": { "dials_from": "3.101.18.71", "workspace": "own" },
    });
    write_exported(&scratch, true, &asked, &reports);
    let dir = scratch.0.join(format!(".pumpkin/runs/{EXPORTED}"));
    let sandbox = json!({ "id": ec2, "provider": "aws", "delete": { "state": "deleted" } });
    fs::write(dir.join("agent/sandbox.json"), sandbox.to_string()).unwrap();
    let record = dir.join("record.json");
    let mut made: Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
    made["args"] = json!(["/home/pumpkin/trees/task.star"]);
    fs::write(&record, made.to_string()).unwrap();
    let log = dir.join("log.jsonl");
    let lines = fs::read_to_string(&log).unwrap().lines().count();
    let deleted =
        json!({ "kind": "sandbox_deleted", "id": modal, "provider": "modal", "sandbox": "job-0" });
    let mut deleted = logged(1009, deleted, json!([]));
    deleted["seq"] = json!(lines);
    writeln!(
        OpenOptions::new().append(true).open(&log).unwrap(),
        "{deleted}"
    )
    .unwrap();
    let kept = files(&dir);
    let mut said = vec![export(&scratch)];
    assert!(files(&dir) == kept, "the export changed the record");
    let exported = exported(&scratch.0.join("public"));
    let holding = |text: &str| -> Vec<&str> {
        let held = exported.iter().filter(|(key, body)| {
            if !key.starts_with("runs") {
                return false;
            }
            let body = String::from_utf8_lossy(body);
            body.contains(text) || body.contains(&text.to_ascii_lowercase())
        });
        held.map(|(key, _)| key.as_str()).collect()
    };
    for (text, why) in [
        (ec2, "the agent's sandbox"),
        (modal, "a judge's sandbox, as the log names it"),
        ("3.101.18.71", "an address that a provider reported"),
        ("311100000000", "an account that a provider reported"),
        ("/home/exporter", "this machine's home"),
        ("/home/pumpkin", "the home that the run's arguments name"),
        ("exporter-user", "this machine's user"),
    ] {
        let held = holding(text);
        assert!(held.is_empty(), "{why}, {text}, is in {held:?}");
    }
    for text in [
        "<sandbox of agent>",
        "<sandbox of job-0>",
        "<modal.dials_from>",
        "<aws.account>",
        "~/work",
        "~/trees",
        "<user>",
        "pumpkin,",
        "Ubuntu 24.04.5 LTS",
        "i-0123456789abcdef0",
    ] {
        let held = holding(text);
        said.push(format!(
            "{text}: in {} files, {}",
            held.len(),
            held.join(" ")
        ));
    }
    check(
        &scratch,
        said.into_iter().map(Step::Ran),
        false,
        &[(EXPORTED, "<id>")],
        &expect![[r#"
            pumpkin: exported 1 run into public, and left out 0: wrote 96 files

            <sandbox of agent>: in 3 files, runs/<id>/kept/items/0000000000000000000000000000000000000000000000000000000000000000 runs/<id>/trace/agent/entries/10-0.json runs/<id>/trace/agent/search/0.json
            <sandbox of job-0>: in 4 files, runs/<id>/kept/items/0000000000000000000000000000000000000000000000000000000000000000 runs/<id>/log/0.json runs/<id>/trace/agent/entries/10-0.json runs/<id>/trace/agent/search/0.json
            <modal.dials_from>: in 3 files, runs/<id>/kept/items/0000000000000000000000000000000000000000000000000000000000000000 runs/<id>/trace/agent/entries/10-0.json runs/<id>/trace/agent/search/0.json
            <aws.account>: in 3 files, runs/<id>/kept/items/0000000000000000000000000000000000000000000000000000000000000000 runs/<id>/trace/agent/entries/10-0.json runs/<id>/trace/agent/search/0.json
            ~/work: in 3 files, runs/<id>/kept/items/0000000000000000000000000000000000000000000000000000000000000000 runs/<id>/trace/agent/entries/10-0.json runs/<id>/trace/agent/search/0.json
            ~/trees: in 3 files, runs/<id>/kept/items/0000000000000000000000000000000000000000000000000000000000000000 runs/<id>/trace/agent/entries/10-0.json runs/<id>/trace/agent/search/0.json
            <user>: in 3 files, runs/<id>/kept/items/0000000000000000000000000000000000000000000000000000000000000000 runs/<id>/trace/agent/entries/10-0.json runs/<id>/trace/agent/search/0.json
            pumpkin,: in 3 files, runs/<id>/kept/items/0000000000000000000000000000000000000000000000000000000000000000 runs/<id>/trace/agent/entries/10-0.json runs/<id>/trace/agent/search/0.json
            Ubuntu 24.04.5 LTS: in 3 files, runs/<id>/kept/items/0000000000000000000000000000000000000000000000000000000000000000 runs/<id>/trace/agent/entries/10-0.json runs/<id>/trace/agent/search/0.json
            i-0123456789abcdef0: in 3 files, runs/<id>/kept/items/0000000000000000000000000000000000000000000000000000000000000000 runs/<id>/trace/agent/entries/10-0.json runs/<id>/trace/agent/search/0.json"#]],
    );
}
