//! `pumpkin view --export <dir>`: the replies that the view's page reads, of every run, as files
//! that a static copy of the page reads in their place, each made by the code that answers its
//! request, as the controller's view answers it. Each file below `runs/`, and `runs.json`, holds
//! its reply in zlib's format, which HTTP's `Content-Encoding: deflate` names.
//!
//! Each row gives a key below `<dir>`, the request whose reply the file holds (below
//! `/api/runs/<id>/` unless it starts `/api/`), and when it is written: when its bytes change,
//! or, of a reply that can't change, once. `<c>` is each caller in the run's `lanes`; `<b>`, `<e>`
//! and `<k>` are 0 and each `next` after it. A reply that the view refuses has no file:
//!
//! | key | reply | written |
//! |---|---|---|
//! | `runs.json` | `/api/runs`, of every run that has its `run.json`, with `next` `null` | when changed, last |
//! | `runs/<id>/run.json` | `/api/runs/<id>` | when changed, the run's last |
//! | `runs/<id>/log/<b>.json` | `log?at=<b>` | when changed |
//! | `runs/<id>/trace/<c>/steps.json`, `…/ops.json`, `…/stats-<by>.json` | `trace/<c>/steps`, `trace/<c>/ops`, `trace/<c>/stats?by=<by>` of `step`, `turn` and `thread` | when changed |
//! | `runs/<id>/trace/<c>/entries/<n>-<e>.json` | `trace/<c>/entries?from=<n>&to=<n>&entry=<e>`, of each step's request `<n>` | when changed |
//! | `runs/<id>/trace/<c>/entry/<at>.json` | `trace/<c>/entry?at=<at>`, of each entry whose text those cut | when changed |
//! | `runs/<id>/trace/<c>/search/<k>.json` | chunk `<k>` of the text that the trace's search reads, in the form it matches in | when changed |
//! | `runs/<id>/exchange/<n>.json` | `exchange/<n>` | once |
//! | `runs/<id>/diff/<a>-<b>-<part>.json` | `diff?a=<a>&b=<b>&part=<part>`, of each step `<b>` whose `differs` names `<part>` of its parent `<a>` | once |
//! | `runs/<id>/kept/<n>.response`, `…/<n>.request` | `exchange/<n>/response`; request `<n>` as the record keeps it, its items apart, in place of `exchange/<n>/request` | once |
//! | `runs/<id>/kept/items/<hex>` | `item/<hex>`, of each item that a `kept/<n>.request` names | once |
//! | `index.html`, `<digest>/<file>` | the page, as it is served, its `index.html` marked to read these files | `<digest>/` once |
//! | `robots.txt` | what asks every crawler to stay out | when changed |

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

use miniz_oxide::deflate::{CompressionLevel, compress_to_vec_zlib};
use miniz_oxide::inflate::decompress_to_vec_zlib;
use provider::RunTag;
use runtime::Record;
use serde_json::{Value, json};
use stdx::Context;

use super::http::{Request, Response};
use super::json::was_cut;
use super::scrub::Scrub;
use super::{Asked, View, answered, api, page, trace};
use crate::say;

/// What `robots.txt` says: that no crawler may read any file, as inspect's static bundle of its
/// logs writes it:
/// <https://github.com/UKGovernmentBEIS/inspect_ai/blob/7cf09c929b3ea9138762ca9e4d8d23f119554ee6/src/inspect_ai/log/_bundle.py#L246-L257>.
const ROBOTS: &str = "User-agent: *\nDisallow: /\n";

/// Writes the runs in the directory `runs` names, or the default one, into `to`, as the module's
/// doc says, each reply with what `scrub` and each run's record name replaced
/// ([`Scrub::of_run`]), and reports each run it left out, with why, and the counts of what it
/// exported, left out and wrote.
pub(super) fn export(runs: Option<String>, to: &Path, scrub: Scrub) -> Result<ExitCode, String> {
    let (runs, _) = crate::runs_dir(runs)?;
    let view = View::new(PathBuf::from(&runs), None);
    // As the controller's view answers: the command that runs a run again names this machine.
    let mut view = View {
        again: false,
        ..view
    };
    let mut out = Out {
        dir: to.to_owned(),
        written: 0,
        files: BTreeSet::new(),
        refused: Vec::new(),
        scrub: scrub.clone(),
        every: scrub.clone(),
        given: scrub,
    };
    for (key, body, once) in page::Page::new().exported() {
        if !once || !out.has(&key)? {
            out.put(&key, &body)?;
        }
    }
    out.put("robots.txt", ROBOTS.as_bytes())?;
    // Only a run that has its `run.json` is a row of `runs.json`, and counted.
    let (mut listed, mut left_out) = (Vec::new(), Vec::new());
    for row in rows(&mut view)? {
        let Some(Ok(id)) = row["id"].as_str().map(|id| RunTag::try_from(id.to_owned())) else {
            continue;
        };
        match out.run(&mut view, &id)? {
            Ok(()) => listed.push(row),
            Err(why) => left_out.push(format!("run {}: {why}", id.as_str())),
        }
    }
    out.scrub = out.every.clone();
    let list = json!({ "runs": listed, "next": null });
    out.put_deflated("runs.json", list.to_string().as_bytes())?;
    let Out {
        written, refused, ..
    } = out;
    for left_out in &left_out {
        say(format_args!("left out {left_out}"));
    }
    if let Some(first) = refused.first() {
        say(format_args!(
            "replies refused, which have no file: {}; the first: {first}",
            refused.len()
        ));
    }
    say(format_args!(
        "exported {} into {}, and left out {}: wrote {written} files",
        crate::counted(listed.len(), "run"),
        to.display(),
        left_out.len(),
    ));
    Ok(ExitCode::SUCCESS)
}

/// Every run's row of the list, newest first, as the pages of `/api/runs` give them.
fn rows(view: &mut View) -> Result<Vec<Value>, String> {
    let mut rows = Vec::new();
    let mut before: Option<String> = None;
    loop {
        let page = reply_json(answered(Asked::Runs { before }, view))?;
        rows.extend(page["runs"].as_array().into_iter().flatten().cloned());
        match page["next"].as_str() {
            Some(next) => before = Some(next.to_owned()),
            None => return Ok(rows),
        }
    }
}

/// The body of `response`, or, if it is no `200 OK`, its status and body.
fn reply(response: Response) -> Result<Vec<u8>, String> {
    let sent = response.sent(false);
    if sent.status != "200 OK" {
        let body = String::from_utf8_lossy(&sent.body);
        return Err(format!("{}: {body}", sent.status));
    }
    Ok(sent.body)
}

fn reply_json(response: Response) -> Result<Value, String> {
    let body = reply(response)?;
    serde_json::from_slice(&body).map_err(|error| format!("the reply is no JSON: {error}"))
}

struct Out {
    dir: PathBuf,
    written: usize,
    /// The key of each file it found or wrote, which it then knows to be there.
    files: BTreeSet<String>,
    refused: Vec<String>,
    /// What the export was given to replace.
    given: Scrub,
    /// What the next reply written has replaced: its run's, and, for `runs.json`, every run's.
    scrub: Scrub,
    /// What every run's record names, and what it was given.
    every: Scrub,
}

impl Out {
    fn has(&mut self, key: &str) -> Result<bool, String> {
        if self.files.contains(key) {
            return Ok(true);
        }
        let path = self.path(key)?;
        let has = path.try_exists().context(path.display())?;
        if has {
            self.files.insert(key.to_owned());
        }
        Ok(has)
    }

    /// Writes `body` at `key`, unless the file there holds it already.
    fn put(&mut self, key: &str, body: &[u8]) -> Result<(), String> {
        let path = self.path(key)?;
        self.files.insert(key.to_owned());
        match fs::read(&path) {
            Ok(held) if held == body => return Ok(()),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context(path.display()),
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context(parent.display())?;
        }
        // Through a file of its own, so that an export stopped part-way leaves no file that holds
        // part of a reply, which a later one would keep as it is.
        let mut part = path.clone().into_os_string();
        part.push(".part");
        let part = PathBuf::from(part);
        fs::write(&part, body).context(part.display())?;
        fs::rename(&part, &path).context(path.display())?;
        self.written += 1;
        Ok(())
    }

    /// Writes `body`, scrubbed ([`Self::scrub`]) and deflated, at `key`, as [`Self::put`] does.
    fn put_deflated(&mut self, key: &str, body: &[u8]) -> Result<(), String> {
        let body = self.scrub.scrubbed(body);
        let deflated = compress_to_vec_zlib(&body, CompressionLevel::DefaultLevel as u8);
        self.put(key, &deflated)
    }

    /// The reply to `target`, a `GET` of the view's data, of `view`, or `None`, its refusal
    /// noted.
    fn get(&mut self, view: &mut View, target: &str) -> Result<Option<Vec<u8>>, String> {
        let request = Request::get(target, false).map_err(|refused| {
            let body = refused.sent(false).body;
            format!("{target}: {}", String::from_utf8_lossy(&body))
        })?;
        match reply(api(&request, view)) {
            Ok(body) => Ok(Some(body)),
            Err(refused) => {
                self.refused.push(format!("{target}: {refused}"));
                Ok(None)
            }
        }
    }

    /// Writes at `key` the reply to `target`, a `GET` of the view's data, of `view`, deflated, or
    /// notes its refusal; returns the reply, if any.
    fn put_reply(
        &mut self,
        view: &mut View,
        key: &str,
        target: &str,
    ) -> Result<Option<Vec<u8>>, String> {
        let body = self.get(view, target)?;
        if let Some(body) = &body {
            self.put_deflated(key, body)?;
        }
        Ok(body)
    }

    /// Writes at `key` the body of `response`, the answer to `asked`, deflated, or notes its
    /// refusal; returns the body, if any.
    fn put_answered(
        &mut self,
        key: &str,
        asked: &str,
        response: Response,
    ) -> Result<Option<Vec<u8>>, String> {
        match reply(response) {
            Ok(body) => {
                self.put_deflated(key, &body)?;
                Ok(Some(body))
            }
            Err(refused) => {
                self.refused.push(format!("{asked}: {refused}"));
                Ok(None)
            }
        }
    }

    /// Writes at `key`, unless a file is there, the reply to `target`, as [`Self::put_reply`].
    fn put_once(&mut self, view: &mut View, key: &str, target: &str) -> Result<(), String> {
        if !self.has(key)? {
            self.put_reply(view, key, target)?;
        }
        Ok(())
    }

    /// Whether the run whose `run.json` is at `key` had ended, and was not lost, as of the last
    /// export.
    fn ended(&mut self, key: &str) -> Result<bool, String> {
        let path = self.path(key)?;
        let deflated = match fs::read(&path) {
            Ok(deflated) => deflated,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context(path.display()),
        };
        let run = decompress_to_vec_zlib(&deflated).ok();
        let run: Option<Value> = run.and_then(|run| serde_json::from_slice(&run).ok());
        let ended = run.is_some_and(|run| run["live"] == false && run["state"] != "lost");
        if ended {
            self.files.insert(key.to_owned());
        }
        Ok(ended)
    }

    /// The path of the file at `key`, a relative path of plain names, or the refusal of a key
    /// that names another place, as a part that a record names may.
    fn path(&self, key: &str) -> Result<PathBuf, String> {
        let plain = Path::new(key)
            .components()
            .all(|part| matches!(part, Component::Normal(_)));
        if !plain {
            return Err(format!(
                "`{key}` is no path below the export's directory: expected plain names alone"
            ));
        }
        Ok(self.dir.join(key))
    }

    /// Writes run `id`'s files, of `view`, as the module's doc says, but none of a run whose
    /// `run.json` said it had ended, and not that it was lost, which a reap changes; returns
    /// why the run has no `run.json`, if it has none.
    fn run(&mut self, view: &mut View, id: &RunTag) -> Result<Result<(), String>, String> {
        let base = format!("runs/{}", id.as_str());
        let api = format!("/api/runs/{}", id.as_str());
        if self.ended(&format!("{base}/run.json"))? {
            // `runs.json`, which lists the run, replaces what the run's record names too.
            let record = Record::open(&view.runs.join(id.as_str()))?;
            match self.given.of_run(&record, []) {
                Ok(scrub) => self.every = std::mem::take(&mut self.every).with(&scrub),
                Err(error) => self.refused.push(format!("run {}: {error}", id.as_str())),
            }
            return Ok(Ok(()));
        }
        let run_body = match reply(answered(Asked::Run { id: id.clone() }, view)) {
            Ok(body) => body,
            Err(refused) => return Ok(Err(format!("{api}: {refused}"))),
        };
        let run: Value = serde_json::from_slice(&run_body).unwrap_or_default();
        let mut windows = Vec::new();
        let mut at = 0;
        while let Some(window) = self.get(view, &format!("{api}/log?at={at}"))? {
            let read: Value = serde_json::from_slice(&window).unwrap_or_default();
            let (next, end) = (read["next"].as_u64(), read["end"].as_u64());
            windows.push((format!("{base}/log/{at}.json"), window, read));
            match (next, end) {
                (Some(next), Some(end)) if next < end && next > at => at = next,
                _ => break,
            }
        }
        let record = Record::open(&view.runs.join(id.as_str()))?;
        let logged = (windows.iter())
            .flat_map(|(.., read)| read["records"].as_array().into_iter().flatten());
        self.scrub = match self.given.of_run(&record, logged) {
            Ok(scrub) => scrub,
            Err(error) => return Ok(Err(error)),
        };
        self.every = std::mem::take(&mut self.every).with(&self.scrub);
        for (key, window, _) in &windows {
            self.put_deflated(key, window)?;
        }
        let callers: BTreeSet<&str> = (run["lanes"].as_array().into_iter().flatten())
            .filter_map(|lane| lane["caller"].as_str())
            .collect();
        for caller in callers {
            self.trace(view, id, caller)?;
        }
        self.put_deflated(&format!("{base}/run.json"), &run_body)?;
        Ok(Ok(()))
    }

    /// Writes caller `caller`'s files of run `id`, of `view`, as the module's doc says: those of
    /// `kept/` once their request was answered, and of `exchange/` and `diff/` once every request
    /// of the caller up to their own was, as an earlier one answered later may place a step
    /// elsewhere in the trace.
    fn trace(&mut self, view: &mut View, id: &RunTag, caller: &str) -> Result<(), String> {
        let base = format!("runs/{}", id.as_str());
        let api = format!("/api/runs/{}", id.as_str());
        let (trace_base, trace_api) = (
            format!("{base}/trace/{caller}"),
            format!("{api}/trace/{caller}"),
        );
        let Some(steps) = self.put_reply(
            view,
            &format!("{trace_base}/steps.json"),
            &format!("{trace_api}/steps"),
        )?
        else {
            return Ok(());
        };
        for by in ["step", "turn", "thread"] {
            let (key, target) = (
                format!("{trace_base}/stats-{by}.json"),
                format!("{trace_api}/stats?by={by}"),
            );
            self.put_reply(view, &key, &target)?;
        }
        self.put_reply(
            view,
            &format!("{trace_base}/ops.json"),
            &format!("{trace_api}/ops"),
        )?;
        let steps: Value = serde_json::from_slice(&steps).unwrap_or_default();
        let column = |name: &str| steps["steps"][name].as_array().cloned().unwrap_or_default();
        let (requests, ends) = (column("request"), column("end_ns"));
        let (kept_requests, kept_responses) = (column("kept_bytes"), column("back_bytes"));
        let (parents, differs) = (column("parent"), column("differs"));
        // Whether every request up to this row's was answered, as the module's doc says.
        let mut settled = true;
        for (row, request) in requests.iter().enumerate() {
            let Some(n) = request.as_u64() else {
                continue;
            };
            let answered = !ends[row].is_null();
            settled &= answered;
            self.entries(view, &trace_base, &trace_api, n)?;
            if answered {
                let kept = (
                    !kept_requests[row].is_null(),
                    !kept_responses[row].is_null(),
                );
                self.kept(view, id, n, kept)?;
            }
            if !settled {
                continue;
            }
            self.put_once(
                view,
                &format!("{base}/exchange/{n}.json"),
                &format!("{api}/exchange/{n}"),
            )?;
            if let Some(parent) = parents[row].as_u64() {
                for part in differs[row]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    let key = format!("{base}/diff/{parent}-{n}-{part}.json");
                    let target = format!("{api}/diff?a={parent}&b={n}&part={part}");
                    self.put_once(view, &key, &target)?;
                }
            }
        }
        self.search(view, id, caller)
    }

    /// Writes the chunks of the text that caller `caller`'s search of run `id` reads, of `view`.
    fn search(&mut self, view: &mut View, id: &RunTag, caller: &str) -> Result<(), String> {
        let trace_base = format!("runs/{}/trace/{caller}", id.as_str());
        let trace_api = format!("/api/runs/{}/trace/{caller}", id.as_str());
        let mut chunk = 0;
        loop {
            let asked = trace::Asked::SearchChunk {
                caller: caller.to_owned(),
                chunk,
            };
            let asked = Asked::Trace {
                id: id.clone(),
                asked,
            };
            let key = format!("{trace_base}/search/{chunk}.json");
            let what = format!("{trace_api}, chunk {chunk} of its searchable text");
            let Some(body) = self.put_answered(&key, &what, answered(asked, view))? else {
                return Ok(());
            };
            let body: Value = serde_json::from_slice(&body).unwrap_or_default();
            match body["next"]
                .as_u64()
                .and_then(|next| usize::try_from(next).ok())
            {
                Some(next) => chunk = next,
                None => return Ok(()),
            }
        }
    }

    /// Writes the entries of step `n` of the trace whose files are at `base` and whose
    /// replies at `api`, and each entry of them that the window cut, of `view`.
    fn entries(&mut self, view: &mut View, base: &str, api: &str, n: u64) -> Result<(), String> {
        let mut entry = 0;
        loop {
            let key = format!("{base}/entries/{n}-{entry}.json");
            let target = format!("{api}/entries?from={n}&to={n}&entry={entry}");
            let Some(window) = self.put_reply(view, &key, &target)? else {
                return Ok(());
            };
            let window: Value = serde_json::from_slice(&window).unwrap_or_default();
            for listed in window["entries"].as_array().into_iter().flatten() {
                let (Some(text), Some(at)) = (listed["text"].as_str(), listed["at"].as_str())
                else {
                    continue;
                };
                if was_cut(text) {
                    let key = format!("{base}/entry/{at}.json");
                    self.put_reply(view, &key, &format!("{api}/entry?at={at}"))?;
                }
            }
            match window["next"]["entry"].as_u64() {
                Some(next) if window["next"]["request"] == n && next > entry => entry = next,
                _ => return Ok(()),
            }
        }
    }

    /// Writes, each once, request `n`'s response and request as run `id`'s record keeps them,
    /// each if `kept` says the record keeps it, and each item that the request names, of `view`.
    fn kept(
        &mut self,
        view: &mut View,
        id: &RunTag,
        n: u64,
        (request, response): (bool, bool),
    ) -> Result<(), String> {
        let (base, api) = (
            format!("runs/{}/kept", id.as_str()),
            format!("/api/runs/{}", id.as_str()),
        );
        if response {
            let target = format!("{api}/exchange/{n}/response");
            self.put_once(view, &format!("{base}/{n}.response"), &target)?;
        }
        let key = format!("{base}/{n}.request");
        if !request || self.has(&key)? {
            return Ok(());
        }
        let asked = trace::Asked::Exchange {
            request: n,
            part: Some(trace::Side::Kept),
        };
        let asked = Asked::Trace {
            id: id.clone(),
            asked,
        };
        let what = format!("{api}, request {n} as kept");
        // Its items first, so that a request's file names only items written.
        let body = match reply(answered(asked, view)) {
            Ok(body) => body,
            Err(refused) => {
                self.refused.push(format!("{what}: {refused}"));
                return Ok(());
            }
        };
        let kept: Value = serde_json::from_slice(&body).unwrap_or_default();
        let items = (kept["conversation"]["items"]
            .as_array()
            .into_iter()
            .flatten())
        .chain(kept["fields"].as_array().into_iter().flatten());
        for hex in items.filter_map(|part| part["item"].as_str()) {
            self.put_once(
                view,
                &format!("{base}/items/{hex}"),
                &format!("{api}/item/{hex}"),
            )?;
        }
        self.put_deflated(&key, &body)
    }
}
