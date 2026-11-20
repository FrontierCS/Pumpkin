//! The space occupied by incoming operation requests in a run log, measured by whole lines.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead as _, BufReader, Read as _};
use std::path::Path;

use serde_json::Value;
use stdx::Context as _;

use super::Result;

#[derive(Default)]
struct Size {
    count: u64,
    bytes: u64,
}

impl Size {
    fn add(&mut self, bytes: u64) {
        self.count += 1;
        self.bytes += bytes;
    }
}

pub(super) fn report(run_dir: &Path, caller: Option<&str>) -> Result<()> {
    let path = run_dir.join("log.jsonl");
    let file = File::open(&path).context(path.display())?;
    // A live run can keep appending. Take the prefix that existed when this scan began.
    let prefix_bytes = file.metadata().context(path.display())?.len();
    let mut log = BufReader::new(file.take(prefix_bytes));
    let mut line = Vec::new();
    let mut log_bytes = 0;
    let mut total = Size::default();
    let mut unread = Size::default();
    let mut by_kind = BTreeMap::<_, Size>::new();
    let partial_bytes = loop {
        line.clear();
        let line_bytes = log.read_until(b'\n', &mut line).context(path.display())? as u64;
        log_bytes += line_bytes;
        let Some(whole) = line.strip_suffix(b"\n") else {
            break line_bytes;
        };
        let parsed = serde_json::from_slice::<Value>(whole);
        match parsed.as_ref().map_err(|_| ()).and_then(operation) {
            Ok(Some((from, kind))) if caller.is_none_or(|name| name == from) => {
                by_kind.entry(kind.to_owned()).or_default().add(line_bytes);
                total.add(line_bytes);
            }
            Ok(_) => {}
            Err(()) => unread.add(line_bytes),
        }
    };

    println!("Log size: {log_bytes} bytes");
    println!(
        "Unread complete lines: {} ({} bytes)",
        unread.count, unread.bytes
    );
    println!("Partial final line: {partial_bytes} bytes");
    match caller {
        Some(name) => println!("Incoming op requests from {name:?}"),
        None => println!("Incoming op requests from all callers"),
    }
    println!("{:<24} {:>10} {:>12}", "Request kind", "Count", "Log bytes");
    for (kind, Size { count, bytes }) in by_kind {
        println!("{:<24} {count:>10} {bytes:>12}", format!("{kind:?}"));
    }
    println!("{:<24} {:>10} {:>12}", "Total", total.count, total.bytes);
    Ok(())
}

fn operation(line: &Value) -> std::result::Result<Option<(&str, &str)>, ()> {
    line["at_ns"]
        .as_str()
        .and_then(|text| text.parse::<u64>().ok())
        .ok_or(())?;
    let event = &line["event"];
    if event.is_null() || event["kind"].as_str().ok_or(())? != "op_arrived" {
        return Ok(None);
    }
    let caller = event["caller"].as_str().filter(|name| !name.is_empty());
    let kind = event["request"]["kind"]
        .as_str()
        .filter(|kind| !kind.is_empty());
    Ok(Some((caller.ok_or(())?, kind.ok_or(())?)))
}
