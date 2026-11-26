//! The premises that a change adds to the code that runs: the calls that panic if a belief of the
//! author's is wrong. Each reads as written, from the tokens of its file.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::ops::RangeInclusive;

use proc_macro2::{Delimiter, Group, Spacing, TokenStream, TokenTree};

use crate::repo::Repo;
use crate::tidy::under_tests;
use crate::{Result, diff};

/// The macros that are premises.
const MACROS: [&str; 4] = ["unreachable", "panic", "assert", "debug_assert"];

/// The methods that are premises.
const METHODS: [&str; 2] = ["expect", "unwrap"];

/// What `crates/test-support` holds, which `docs/dev/testing.md` and `ARCHITECTURE.md` give to
/// the tests alone.
const TEST_SUPPORT: &str = "crates/test-support/";

struct Premise {
    /// The lines from the call's name to its end.
    lines: RangeInclusive<usize>,
    /// The call as written, on one line. A call without arguments shows the line it is on.
    shown: String,
}

/// The premises in the Rust files outside tests that the working tree adds or changes since
/// `base`. The report has a line for each, sorted by file and line, and ends with their count.
pub(crate) fn list(repo: &Repo<'_>, base: &str) -> Result<String> {
    let again = format!("cargo xtask premises {base}");
    let added = added_lines(&diff::since(repo, base, &[], &again)?)?;
    let (mut report, mut count, mut files) = (String::new(), 0, 0);
    let mut unread = String::new();
    for (path, lines) in added {
        let file = std::path::Path::new(&path);
        let tested = under_tests(file) || path.starts_with(TEST_SUPPORT);
        if file.extension() != Some("rs".as_ref()) || tested {
            continue;
        }
        let text = repo.read(&path)?;
        let premises = match premises(&text) {
            Ok(premises) => premises,
            Err(error) => {
                let _ = writeln!(unread, "{path}: not read: {error}");
                continue;
            }
        };
        let changed: Vec<&Premise> = (premises.iter())
            .filter(|premise| premise.lines.clone().any(|line| lines.contains(&line)))
            .collect();
        files += usize::from(!changed.is_empty());
        count += changed.len();
        for Premise { lines, shown } in changed {
            let _ = writeln!(report, "{path}:{}: {shown}", lines.start());
        }
    }
    report.push_str(&unread);
    let _ = writeln!(report, "premises: {count}, files: {files}");
    Ok(report)
}

/// The lines that `diff`, a unified diff, adds to each file, by their numbers in the new file. A
/// file that the diff deletes has none.
fn added_lines(diff: &str) -> Result<BTreeMap<String, BTreeSet<usize>>> {
    let mut files = BTreeMap::<String, BTreeSet<usize>>::new();
    let mut path: Option<&str> = None;
    // How many lines of the hunk's old and new sides are still to come.
    let (mut old_left, mut new_left, mut line) = (0, 0, 0);
    for text in diff.lines() {
        if old_left > 0 || new_left > 0 {
            match text.chars().next() {
                Some('+') => {
                    new_left -= 1;
                    if let Some(path) = path {
                        files.entry(path.to_owned()).or_default().insert(line);
                    }
                    line += 1;
                }
                Some('-') => old_left -= 1,
                // A line of context. An empty one is a context line whose blank was stripped.
                Some(' ') | None => {
                    old_left -= 1;
                    new_left -= 1;
                    line += 1;
                }
                // `\ No newline at end of file`.
                Some(_) => {}
            }
        } else if let Some(name) = text.strip_prefix("+++ ") {
            path = match name.trim_end_matches('\t') {
                "/dev/null" => None,
                quoted if quoted.starts_with('"') => {
                    return Err(format!(
                        "git quotes the path {quoted} in the diff: rename the file, as this \
                         command reads no quoted path"
                    )
                    .into());
                }
                name => Some(name.strip_prefix("b/").unwrap_or(name)),
            };
        } else if let Some(header) = text.strip_prefix("@@ -") {
            let malformed =
                || format!("the diff has the hunk header `{text}`: expected `@@ -a,b +c,d @@`");
            let (old, rest) = header.split_once(" +").ok_or_else(malformed)?;
            let new = rest.split_once(" @@").ok_or_else(malformed)?.0;
            let counts = |range: &str| {
                let (start, count) = range.split_once(',').unwrap_or((range, "1"));
                Some((start.parse::<usize>().ok()?, count.parse::<usize>().ok()?))
            };
            let ((_, old_count), (new_start, new_count)) =
                counts(old).zip(counts(new)).ok_or_else(malformed)?;
            (old_left, new_left, line) = (old_count, new_count, new_start);
        }
    }
    Ok(files)
}

/// The premises in the Rust source `text`, outside its tests, or why `text` isn't Rust tokens.
fn premises(text: &str) -> std::result::Result<Vec<Premise>, String> {
    let tokens: TokenStream = text.parse().map_err(|error| format!("{error}"))?;
    let mut found = Vec::new();
    scan(text, tokens, &mut found);
    found.sort_by_key(|premise: &Premise| *premise.lines.start());
    Ok(found)
}

/// Adds to `found` the premises in `tokens`, the tokens of `text`, and in each group of them.
/// It leaves out an item that `#[test]` or `#[cfg(test)]` marks. An inner `#![cfg(test)]` leaves
/// out all of `tokens`.
fn scan(text: &str, tokens: TokenStream, found: &mut Vec<Premise>) {
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    let mut at = 0;
    while at < tokens.len() {
        if let Some(attribute) = attribute(&tokens[at..]) {
            match attribute {
                Attribute::Other { len } => at += len,
                Attribute::Test { inner: true, .. } => return,
                Attribute::Test { len, .. } => at = after_item(&tokens, at + len),
            }
            continue;
        }
        match &tokens[at] {
            TokenTree::Group(group) => scan(text, group.stream(), found),
            TokenTree::Ident(name) => {
                if let [_, TokenTree::Punct(bang), TokenTree::Group(args), ..] = &tokens[at..]
                    && bang.as_char() == '!'
                    && bang.spacing() == Spacing::Alone
                    && MACROS.contains(&name.to_string().as_str())
                {
                    let start = name.span().start().line;
                    found.push(premise(text, &format!("{name}!"), start, args));
                }
            }
            TokenTree::Punct(dot) if dot.as_char() == '.' => {
                if let [_, TokenTree::Ident(name), TokenTree::Group(args), ..] = &tokens[at..]
                    && args.delimiter() == Delimiter::Parenthesis
                    && METHODS.contains(&name.to_string().as_str())
                    && (name != "unwrap" || args.stream().is_empty())
                {
                    let start = name.span().start().line;
                    found.push(premise(text, &format!(".{name}"), start, args));
                }
            }
            TokenTree::Punct(_) | TokenTree::Literal(_) => {}
        }
        at += 1;
    }
}

fn premise(text: &str, call: &str, start: usize, args: &Group) -> Premise {
    let range = args.span().byte_range();
    // The arguments without their delimiters, as written, on one line.
    let inner = &text[range.start + 1..range.end - 1];
    let arguments = inner.split_whitespace().collect::<Vec<_>>().join(" ");
    let shown = if arguments.is_empty() {
        let line = text.lines().nth(start - 1).unwrap_or_default();
        format!("{call}() in `{}`", line.trim())
    } else {
        format!("{call}({arguments})")
    };
    Premise {
        lines: start..=args.span().end().line,
        shown,
    }
}

enum Attribute {
    /// An attribute that marks a test, for an item, or for the module or file that holds an inner
    /// one.
    Test {
        inner: bool,
        len: usize,
    },
    Other {
        len: usize,
    },
}

/// The attribute that `tokens` begin with, if one.
fn attribute(tokens: &[TokenTree]) -> Option<Attribute> {
    let [TokenTree::Punct(hash), rest @ ..] = tokens else {
        return None;
    };
    let (inner, group) = match rest {
        [TokenTree::Punct(bang), TokenTree::Group(group), ..] if bang.as_char() == '!' => {
            (true, group)
        }
        [TokenTree::Group(group), ..] => (false, group),
        _ => return None,
    };
    if hash.as_char() != '#' || group.delimiter() != Delimiter::Bracket {
        return None;
    }
    let len = if inner { 3 } else { 2 };
    let parts: Vec<TokenTree> = group.stream().into_iter().collect();
    let test = match parts.as_slice() {
        [TokenTree::Ident(name)] => name == "test",
        [TokenTree::Ident(name), TokenTree::Group(args)] if name == "cfg" => {
            matches!(args.stream().into_iter().collect::<Vec<_>>().as_slice(), [TokenTree::Ident(only)] if only == "test")
        }
        _ => false,
    };
    Some(if test {
        Attribute::Test { inner, len }
    } else {
        Attribute::Other { len }
    })
}

/// Where the item that begins at `at`, after its attributes, ends: after its braces or its `;`.
fn after_item(tokens: &[TokenTree], mut at: usize) -> usize {
    while let Some(attribute) = attribute(&tokens[at..]) {
        at += match attribute {
            Attribute::Test { len, .. } | Attribute::Other { len } => len,
        };
    }
    while let Some(token) = tokens.get(at) {
        at += 1;
        match token {
            TokenTree::Group(group) if group.delimiter() == Delimiter::Brace => break,
            TokenTree::Punct(semicolon) if semicolon.as_char() == ';' => break,
            _ => {}
        }
    }
    at
}
