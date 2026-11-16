//! The line diff of what two requests of a trace sent as one part: their system prompt, or their
//! list of tools, a tool a block.
//!
//! The diff is the longest common subsequence of the two sides' lines, over the lines between
//! their common head and tail; its hunks keep [`DIFF_CONTEXT_LINES`] of unchanged lines around
//! each change.

use std::collections::BTreeMap;

use route::read::{self, Item};
use runtime::Record;
use schema::ir::Sha256Digest;
use serde_json::{Value, json};

use super::{Step, Trace};

/// The most lines of a side that a diff compares: its table is the product of the sides'.
const DIFF_SIDE_LINES: usize = 5000;

/// The unchanged lines that a hunk shows before and after each change.
const DIFF_CONTEXT_LINES: usize = 3;

/// The part of two requests that a diff compares.
#[derive(Clone, Copy)]
pub(in crate::view) enum Compared {
    /// The field that its API holds its system prompt in ([`read::system_field`]); a system
    /// message in its conversation is an item of the conversation like any other.
    System,
    /// Its body's list of tools.
    Tools,
}

/// A line of a diff: kept from both sides, removed from the first, or added in the second.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Change {
    Kept,
    Removed,
    Added,
}

impl Trace {
    /// The diff of `part` of requests `a` and `b`: `same` if they sent one part, and else its
    /// hunks, unless a side has more than [`DIFF_SIDE_LINES`] lines, or a side can't be read.
    pub(super) fn diff_json(&self, record: &Record, a: u64, b: u64, part: Compared) -> Value {
        let mut reply = json!({
            "a": a, "b": b,
            "part": match part { Compared::System => "system", Compared::Tools => "tools" },
        });
        let step = |request: u64| {
            let step = self.at.get(&request).map(|&index| &self.steps[index]);
            step.filter(|step| step.placed && step.thread.is_some())
                .ok_or_else(|| {
                    format!(
                        "request {request}'s parts are not read: it is in flight, or unreadable"
                    )
                })
        };
        let sides = step(a).and_then(|a| Ok((a, step(b)?))).and_then(|(a, b)| {
            if a.key(part) == b.key(part) {
                return Ok(None);
            }
            Ok(Some((
                self.lines(record, a, part)?,
                self.lines(record, b, part)?,
            )))
        });
        let (lines_a, lines_b) = match sides {
            Err(unread) => {
                reply["unread"] = json!(unread);
                return reply;
            }
            Ok(None) => {
                reply["same"] = json!(true);
                return reply;
            }
            Ok(Some(sides)) => sides,
        };
        let (a, b): (Vec<&str>, Vec<&str>) = (
            lines_a.iter().map(String::as_str).collect(),
            lines_b.iter().map(String::as_str).collect(),
        );
        reply["same"] = json!(false);
        reply["lines"] = json!([a.len(), b.len()]);
        if a.len().max(b.len()) > DIFF_SIDE_LINES {
            reply["too_long"] = json!(format!(
                "too long to diff: {} and {} lines, past {DIFF_SIDE_LINES} a side",
                a.len(),
                b.len()
            ));
            return reply;
        }
        reply["hunks"] = hunks(&line_diff(&a, &b), DIFF_CONTEXT_LINES);
        super::cut(reply)
    }

    /// The lines of `step`'s `part`, or why they can't be read: its system prompt's texts, or its
    /// tools as the record keeps them, each printed as indented JSON.
    fn lines(&self, record: &Record, step: &Step, part: Compared) -> Result<Vec<String>, String> {
        let mut lines = Vec::new();
        match part {
            Compared::System => {
                let Some(part) = step.system_part() else {
                    return Ok(lines);
                };
                let entries = self.parts[part as usize].entries.as_ref();
                for entry in entries.map_err(String::clone)? {
                    if let Item::System { text } = entry {
                        lines.extend(text.lines().map(str::to_owned));
                    }
                }
            }
            Compared::Tools => {
                let Some(part) = step.tools_part else {
                    return Ok(lines);
                };
                let tools = self.tools_value(record, part)?;
                let tools = match tools {
                    Value::Array(tools) => tools,
                    other => vec![other],
                };
                for tool in tools {
                    let text = serde_json::to_string_pretty(&tool).unwrap_or_default();
                    lines.extend(text.lines().map(str::to_owned));
                }
            }
        }
        Ok(lines)
    }

    /// The JSON of `part`, a body's `tools`: its kept item read again, or the JSON it was held by.
    fn tools_value(&self, record: &Record, part: u32) -> Result<Value, String> {
        if let Some(hex) = &self.parts[part as usize].digest {
            let digest = Sha256Digest::try_from(format!("sha256:{hex}"))
                .map_err(|error| format!("item {hex}: {error}"))?;
            let bytes = record.kept_item(&digest)?;
            return serde_json::from_slice(&bytes).map_err(|error| format!("item {hex}: {error}"));
        }
        // A part sent in its body is held by its JSON alone.
        let held = self.keys.iter().find(|(_, index)| **index == part);
        let text = held.map_or("null", |((_, text), _)| text.as_str());
        serde_json::from_str(text).map_err(|error| format!("its tools: {error}"))
    }
}

/// The lines of `a` and `b`, each kept, removed or added, in order, so that the most are kept.
fn line_diff<'t>(a: &[&'t str], b: &[&'t str]) -> Vec<(Change, &'t str)> {
    let head = a
        .iter()
        .zip(b)
        .take_while(|(mid_a, mid_b)| mid_a == mid_b)
        .count();
    let tail = (a[head..].iter().rev())
        .zip(b[head..].iter().rev())
        .take_while(|(mid_a, mid_b)| mid_a == mid_b)
        .count();
    let (mid_a, mid_b) = (&a[head..a.len() - tail], &b[head..b.len() - tail]);
    // Each distinct line as a number, so that the table compares numbers.
    let mut numbers: BTreeMap<&'t str, u32> = BTreeMap::new();
    let mut number = |line: &'t str| {
        let next = u32::try_from(numbers.len()).unwrap_or(u32::MAX);
        *numbers.entry(line).or_insert(next)
    };
    let (num_a, num_b): (Vec<u32>, Vec<u32>) = (
        mid_a.iter().map(|line| number(line)).collect(),
        mid_b.iter().map(|line| number(line)).collect(),
    );
    let (len_a, len_b) = (num_a.len(), num_b.len());
    // Row `at_a` of the table: `below[at_b]` and `here[at_b]` are the longest common subsequence
    // of `num_a[at_a + 1..]`, or `num_a[at_a..]`, and `num_b[at_b..]`; a pair's bit in `drop_a`
    // says that dropping `num_a[at_a]` keeps it longest.
    let (mut below, mut here) = (vec![0_u32; len_b + 1], vec![0_u32; len_b + 1]);
    let mut drop_a = vec![0_u64; (len_a * len_b).div_ceil(64)];
    for at_a in (0..len_a).rev() {
        for at_b in (0..len_b).rev() {
            here[at_b] = if num_a[at_a] == num_b[at_b] {
                below[at_b + 1] + 1
            } else if below[at_b] >= here[at_b + 1] {
                drop_a[(at_a * len_b + at_b) / 64] |= 1 << ((at_a * len_b + at_b) % 64);
                below[at_b]
            } else {
                here[at_b + 1]
            };
        }
        std::mem::swap(&mut below, &mut here);
    }
    let mut lines: Vec<(Change, &str)> = a[..head].iter().map(|l| (Change::Kept, *l)).collect();
    let (mut at_a, mut at_b) = (0, 0);
    while at_a < len_a && at_b < len_b {
        if num_a[at_a] == num_b[at_b] {
            lines.push((Change::Kept, mid_a[at_a]));
            (at_a, at_b) = (at_a + 1, at_b + 1);
        } else if drop_a[(at_a * len_b + at_b) / 64] & (1 << ((at_a * len_b + at_b) % 64)) != 0 {
            lines.push((Change::Removed, mid_a[at_a]));
            at_a += 1;
        } else {
            lines.push((Change::Added, mid_b[at_b]));
            at_b += 1;
        }
    }
    lines.extend(mid_a[at_a..].iter().map(|line| (Change::Removed, *line)));
    lines.extend(mid_b[at_b..].iter().map(|line| (Change::Added, *line)));
    lines.extend(a[a.len() - tail..].iter().map(|line| (Change::Kept, *line)));
    lines
}

/// The hunks of `lines`, each its changes with up to `context` kept lines around them, joined
/// where they would overlap, and at the index of its first line in each side.
fn hunks(lines: &[(Change, &str)], context: usize) -> Value {
    // Each line's index in either side.
    let mut at = Vec::with_capacity(lines.len());
    let (mut in_a, mut in_b) = (0, 0);
    for (change, _) in lines {
        at.push((in_a, in_b));
        in_a += usize::from(*change != Change::Added);
        in_b += usize::from(*change != Change::Removed);
    }
    let changed =
        |from: usize| (lines[from..].iter()).position(|(change, _)| *change != Change::Kept);
    let mut hunks = Vec::new();
    let mut from = 0;
    while let Some(first) = changed(from).map(|ahead| from + ahead) {
        let mut last = first;
        while let Some(ahead) = changed(last + 1).filter(|&ahead| ahead <= 2 * context) {
            last += 1 + ahead;
        }
        let (start, end) = (
            first.saturating_sub(context),
            (last + 1 + context).min(lines.len()),
        );
        let shown: Vec<Value> = (lines[start..end].iter())
            .map(|(change, line)| {
                let sign = match change {
                    Change::Kept => " ",
                    Change::Removed => "-",
                    Change::Added => "+",
                };
                json!([sign, line])
            })
            .collect();
        hunks.push(json!({ "at": [at[start].0, at[start].1], "lines": shown }));
        from = end;
    }
    json!(hunks)
}

impl Step {
    /// What names its `part`: the part of the field that holds it, or `None` if it sent none.
    pub(super) fn key(&self, part: Compared) -> Option<u32> {
        match part {
            Compared::System => self.system_part(),
            Compared::Tools => self.tools_part,
        }
    }

    fn system_part(&self) -> Option<u32> {
        let field = self.api.and_then(read::system_field)?;
        self.fields.get(field).copied()
    }
}
