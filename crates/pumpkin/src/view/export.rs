//! `pumpkin view --export <dir>`: the replies that the view's page reads, of every run, as files
//! that a static copy of the page reads in their place, each made by the code that answers its
//! request, as the controller's view answers it. Each file below `runs/`, and `runs.json`, holds
//! its reply in zlib's format, which HTTP's `Content-Encoding: deflate` names, scrubbed.
//!
//! A file is written when its bytes change; one whose reply can't change is written once: a run's
//! `exchange/`, `diff/` and `kept/` files, and the page's files below a directory named by their
//! digest. A reply that the view refuses has no file. A run's `run.json` is the last of its files,
//! and `runs.json`, which lists only the runs that have their `run.json`, the last of all.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

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

/// Exports the runs of `runs` into `to` and says what was written; the flag's help is the
/// interface.
pub(super) fn export(runs: Option<String>, to: &Path, scrub: Scrub) -> Result<ExitCode, String> {
    let (runs, _) = crate::runs_dir(runs)?;
    let exported = exported(Path::new(&runs), to, scrub, None)?;
    exported.say_refused();
    let Exported {
        ids,
        left_out,
        written,
        refused: _,
    } = exported;
    say(format_args!(
        "exported {} into {}, and left out {}: wrote {written} files",
        crate::counted(ids.len(), "run"),
        to.display(),
        left_out.len(),
    ));
    Ok(ExitCode::SUCCESS)
}

/// The runs an export wrote, and those it left out or had refused.
pub(crate) struct Exported {
    /// The runs whose files are in the export.
    pub(crate) ids: Vec<RunTag>,
    /// Why each run that has no files was left out.
    pub(crate) left_out: Vec<String>,
    /// How many files were written, not counting those whose bytes did not change.
    pub(crate) written: usize,
    /// Each request whose reply was refused, and so has no file.
    pub(crate) refused: Vec<String>,
}

impl Exported {
    /// Says on the standard error what was left out or refused.
    pub(crate) fn say_refused(&self) {
        for left_out in &self.left_out {
            say(format_args!("left out {left_out}"));
        }
        if let Some(first) = self.refused.first() {
            say(format_args!(
                "replies refused, which have no file: {}; the first: {first}",
                self.refused.len()
            ));
        }
    }
}

/// Writes into `to` the page, a `robots.txt`, `runs.json` and each run's files, with the values of
/// `scrub`, and those that each run names, replaced by their stand-ins.
///
/// Only runs minted at `since` or later are exported, if it is given. A run whose files say it
/// had ended is not read again, and a file is rewritten only if its bytes changed.
///
/// # Errors
///
/// Fails if a file of `to` can't be read or written, or a run's record can't be read.
pub(crate) fn exported(
    runs: &Path,
    to: &Path,
    scrub: Scrub,
    since: Option<Duration>,
) -> Result<Exported, String> {
    let view = View::new(runs.to_owned(), None);
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
    let mut rows = rows(&mut view)?;
    rows.retain(|row| {
        // A run's id starts with the time its run started.
        let minted = row["id"].as_str().and_then(runtime::minted);
        since.is_none_or(|since| minted.is_some_and(|minted| minted >= since))
    });
    let (mut listed, mut ids, mut left_out) = (Vec::new(), Vec::new(), Vec::new());
    for row in rows {
        let Some(Ok(id)) = row["id"].as_str().map(|id| RunTag::try_from(id.to_owned())) else {
            continue;
        };
        match out.run(&mut view, &id)? {
            // Only a run that has its `run.json` is a row of `runs.json`, and counted.
            Ok(()) => {
                listed.push(row);
                ids.push(id);
            }
            Err(why) => left_out.push(format!("run {}: {why}", id.as_str())),
        }
    }
    out.scrub = out.every.clone();
    let list = json!({ "runs": listed, "next": null });
    out.put_deflated("runs.json", list.to_string().as_bytes())?;
    let Out {
        written, refused, ..
    } = out;
    Ok(Exported {
        ids,
        left_out,
        written,
        refused,
    })
}

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

/// The export in progress: its directory and the values to scrub.
struct Out {
    dir: PathBuf,
    written: usize,
    files: BTreeSet<String>,
    refused: Vec<String>,
    /// What the export was given to replace.
    given: Scrub,
    /// What the next reply written replaces: its run's and, for `runs.json`, every run's.
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

    fn put_deflated(&mut self, key: &str, body: &[u8]) -> Result<(), String> {
        let body = self.scrub.scrubbed(body);
        let deflated = compress_to_vec_zlib(&body, CompressionLevel::DefaultLevel as u8);
        self.put(key, &deflated)
    }

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

    fn put_once(&mut self, view: &mut View, key: &str, target: &str) -> Result<(), String> {
        if !self.has(key)? {
            self.put_reply(view, key, target)?;
        }
        Ok(())
    }

    /// Whether the run whose `run.json` is at `key` had ended, and was not lost, as of the last
    /// export: a reap changes a lost run, and no later export changes the rest.
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

    /// The path of the file at `key`, below the export's directory; refused if `key` names another
    /// place, as a part that a record names may.
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

    fn run(&mut self, view: &mut View, id: &RunTag) -> Result<Result<(), String>, String> {
        let base = format!("runs/{}", id.as_str());
        let api = format!("/api/runs/{}", id.as_str());
        if self.ended(&format!("{base}/run.json"))? {
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
        // `runs.json`, which lists the run, replaces what the run's record names too.
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
        // `exchange/` and `diff/` files are written once every request of the caller up to their
        // own is answered, as an earlier one answered later may place a step elsewhere in the
        // trace; a `kept/` file, once its own is.
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
        // Its items first, so that a request's file names only items written.
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
