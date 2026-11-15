//! What an export replaces in the bytes of each reply, so that no file of it names the machines
//! and the accounts that ran its runs: exact values that the exporter knows, each with what
//! stands in for it, never a pattern, so that a text that merely looks like such a value, such
//! as an id an agent wrote, is left as it is. The record itself is never changed.
//!
//! The exporter reads the values here, from the process and from each record, as `pumpkin view
//! --help` says of `--export`; what only the machine that publishes knows, such as its own
//! addresses, the publisher gives ([`Scrub::replace`]).

use std::path::Component;

use runtime::Record;
use serde_json::Value;

use super::trace::searchable;

/// The fewest bytes of a value that is found, not given, which an export replaces: a shorter
/// one, such as a user `root` or a report's `default`, is a word that text holds for its own
/// sake.
const FOUND_MIN_BYTES: usize = 8;

/// Values that an export replaces, each with its stand-in, the longest first, so that a value
/// that holds another is replaced whole.
#[derive(Clone, Default)]
pub(super) struct Scrub {
    replaced: Vec<(String, String)>,
}

impl Scrub {
    /// What names this machine: its home, which becomes `~`, and its user, which becomes
    /// `<user>`, as the environment's `HOME` and `USER` name them.
    pub(super) fn of_machine() -> Self {
        let mut scrub = Self::default();
        if let Some(home) = std::env::var_os("HOME").and_then(|home| home.into_string().ok()) {
            scrub.found(&home, "~");
        }
        if let Some(user) = std::env::var_os("USER").and_then(|user| user.into_string().ok()) {
            scrub.found(&user, "<user>");
        }
        scrub
    }

    /// Replaces `value` with `stand_in` too, and its form that a search matches in
    /// ([`searchable`]), unless an earlier value is the same.
    pub(super) fn replace(&mut self, value: &str, stand_in: &str) {
        let searched = searchable(value);
        if searched != value {
            self.replace_exactly(&searched, stand_in);
        }
        self.replace_exactly(value, stand_in);
    }

    /// Replaces `value`, exactly, with `stand_in` too, unless an earlier value is the same.
    fn replace_exactly(&mut self, value: &str, stand_in: &str) {
        if value.is_empty() || self.replaced.iter().any(|(held, _)| held == value) {
            return;
        }
        let at = (self.replaced.iter()).position(|(held, _)| held.len() < value.len());
        let at = at.unwrap_or(self.replaced.len());
        let replaced = (value.to_owned(), stand_in.to_owned());
        self.replaced.insert(at, replaced);
    }

    /// Replaces `value`, found in a record or in the environment, with `stand_in`, if it is
    /// [`FOUND_MIN_BYTES`] long.
    fn found(&mut self, value: &str, stand_in: &str) {
        if value.len() >= FOUND_MIN_BYTES {
            self.replace(value, stand_in);
        }
    }

    /// These values, and those that `record`'s run names, whose log's records are `logged`.
    ///
    /// # Errors
    ///
    /// Fails if a `sandbox.json` or `provider.json` of the record can't be read.
    pub(super) fn of_run<'l>(
        &self,
        record: &Record,
        logged: impl IntoIterator<Item = &'l Value>,
    ) -> Result<Self, String> {
        let mut scrub = self.clone();
        for record in logged {
            let event = &record["event"];
            if let ("sandbox_deleted", Some(id), Some(of)) = (
                event["kind"].as_str().unwrap_or_default(),
                event["id"].as_str(),
                event["sandbox"].as_str(),
            ) {
                scrub.replace(id, &format!("<sandbox of {of}>"));
            }
        }
        for (dir, sandbox) in record.sandboxes()? {
            let parts: Vec<String> = (dir.components())
                .filter_map(|part| match part {
                    Component::Normal(part) => part.to_str().map(str::to_owned),
                    _ => None,
                })
                .collect();
            let of = match &parts[..] {
                [jobs, n] if jobs == "jobs" => format!("job-{n}"),
                [leases, n] if leases == "leases" => format!("lease-{n}"),
                _ => parts.join("/"),
            };
            if let Some(id) = sandbox["id"].as_str() {
                scrub.replace(id, &format!("<sandbox of {of}>"));
            }
        }
        for arg in record.args() {
            if let Some(user) = arg
                .strip_prefix("/home/")
                .and_then(|path| path.split('/').next())
            {
                scrub.found(&format!("/home/{user}"), "~");
                scrub.found(user, "<user>");
            }
        }
        for (provider, report) in record.provider_reports()? {
            scrub.report(&provider.to_string(), &report);
        }
        Ok(scrub)
    }

    /// Replaces each string of `report`, at `path` in a provider's report, with `<path>`, but
    /// those of its Docker engine and its classes' processors, which a sandbox's own text names
    /// too.
    fn report(&mut self, path: &str, report: &Value) {
        match report {
            Value::String(value) => self.found(value, &format!("<{path}>")),
            Value::Array(values) => {
                for value in values {
                    self.report(path, value);
                }
            }
            Value::Object(fields) => {
                for (name, value) in fields {
                    if !matches!(name.as_str(), "docker" | "cpu_features" | "cpu_model") {
                        self.report(&format!("{path}.{name}"), value);
                    }
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    /// `self` with each value of `other` too.
    pub(super) fn with(mut self, other: &Self) -> Self {
        for (value, stand_in) in &other.replaced {
            self.replace_exactly(value, stand_in);
        }
        self
    }

    /// `body` with each value it holds replaced by its stand-in, the longest that starts at a
    /// byte first.
    pub(super) fn scrubbed(&self, body: &[u8]) -> Vec<u8> {
        let mut by_first: Vec<Vec<usize>> = vec![Vec::new(); 256];
        for (index, (value, _)) in self.replaced.iter().enumerate() {
            by_first[usize::from(value.as_bytes()[0])].push(index);
        }
        let mut scrubbed = Vec::with_capacity(body.len());
        let mut at = 0;
        while let Some(&byte) = body.get(at) {
            let rest = &body[at..];
            let found = (by_first[usize::from(byte)].iter())
                .map(|&index| &self.replaced[index])
                .find(|(value, _)| rest.starts_with(value.as_bytes()));
            if let Some((value, stand_in)) = found {
                scrubbed.extend_from_slice(stand_in.as_bytes());
                at += value.len();
            } else {
                scrubbed.push(byte);
                at += 1;
            }
        }
        scrubbed
    }
}
