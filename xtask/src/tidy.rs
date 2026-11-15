//! The checks of the repository's Rust files that `cargo xtask ci` runs, each for a rule of
//! `docs/dev/writing.md` or `docs/dev/style.md`.
//!
//! Each file is read as proc-macro2's tokens, so a string literal is never taken for a comment: a
//! doc comment is a token, and any other comment lies between two. Code in a comment is told from
//! prose by how its line ends, so only some of it is found.

use std::cmp::Reverse;
use std::ops::Range;
use std::path::Path;

use proc_macro2::{Delimiter, LineColumn, TokenStream, TokenTree};
use xshell::{Shell, cmd};

use crate::Result;

// The 95th percentiles of module docs' and item docs' lengths in comparable Rust applications.

/// The most lines a module's doc, its inner doc comments and `#![doc = "…"]` attributes, may
/// span.
const MODULE_CAP: usize = 25;

/// The most lines an item's doc, its outer doc comments and `#[doc = "…"]` attributes, may span.
const ITEM_CAP: usize = 9;

/// Fails unless each of the repository's Rust files, tracked or not yet, but not ignored, passes
/// the checks.
pub(crate) fn check(sh: &Shell) -> Result<()> {
    let mut problems = Vec::new();
    for path in files(sh, "rs")? {
        let text = sh.read_file(&path)?;
        let tokens: TokenStream = match text.parse() {
            Ok(tokens) => tokens,
            Err(error) => {
                let LineColumn { line, column } = error.span().start();
                // proc-macro2 counts columns from 0, and editors from 1.
                let column = column + 1;
                problems.push(format!(
                    "{path}:{line}:{column}: the file is not valid Rust here, so it can't be \
                     checked: make it valid Rust"
                ));
                continue;
            }
        };
        let (mut docs, mut banned) = (Vec::new(), Vec::new());
        for comment in comments(&text, tokens.clone()) {
            banned.extend(comment.problems(&text));
        }
        scan(tokens, &mut docs, &mut banned);
        let docs = docs.into_iter().map(|doc| {
            let Doc {
                line,
                what,
                lines,
                cap,
            } = doc;
            let problem = format!(
                "{path}:{line}: {what} is {lines} lines, over the cap of {cap}: shorten it to \
                 {cap} or fewer, moving each fact to its home"
            );
            (line, problem)
        });
        let banned = banned
            .into_iter()
            .map(|(LineColumn { line, column }, what)| {
                let column = column + 1;
                (line, format!("{path}:{line}:{column}: {what}"))
            });
        let mut found: Vec<(usize, String)> = docs.chain(banned).collect();
        found.sort_by_key(|(line, _)| *line);
        problems.extend(found.into_iter().map(|(_, problem)| problem));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("\n").into())
    }
}

/// The repository's files that end in `.<extension>`, tracked or not yet, but not ignored, and
/// not a tracked one deleted from the working tree, which has nothing left to check.
pub(crate) fn files(sh: &Shell, extension: &str) -> Result<Vec<String>> {
    // Every file, filtered here: a pathspec's meaning changes with Git's variables, such as
    // `GIT_LITERAL_PATHSPECS`, and `-z` keeps unusual names as they are rather than quoted.
    let files = cmd!(sh, "git ls-files -z --cached --others --exclude-standard").read()?;
    let kept = |path: &&str| {
        Path::new(path).extension() == Some(extension.as_ref()) && sh.path_exists(path)
    };
    Ok(files.split('\0').filter(kept).map(str::to_owned).collect())
}

struct Doc {
    line: usize,
    what: &'static str,
    lines: usize,
    cap: usize,
}

/// Adds to `docs` each doc over its cap in `tokens`, a module's or an item's body, and in the
/// bodies within it, and to `banned` where each banned form there starts, with what is wrong: a
/// doc on a `#[test]` item, an `unreachable!` without a message, or a `debug_assert!`,
/// `debug_assert_eq!` or `debug_assert_ne!`.
///
/// A module's doc is its inner `doc = "…"` attributes, doc comments included, and an item's the
/// outer ones in its run of attributes; each counts the lines it spans.
fn scan(tokens: TokenStream, docs: &mut Vec<Doc>, banned: &mut Vec<(LineColumn, String)>) {
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    let (mut module, mut item) = (Block::default(), Block::default());
    let mut at = 0;
    while at < tokens.len() {
        if let Some(Attribute {
            inner,
            tokens,
            doc,
            test,
        }) = attribute(&tokens[at..])
        {
            let block = if inner { &mut module } else { &mut item };
            if let Some((first, lines)) = doc {
                block.add(first, lines);
            }
            block.test |= test;
            at += tokens;
            continue;
        }
        docs.extend(item.over("an item's doc", ITEM_CAP));
        banned.extend(item.on_test());
        item = Block::default();
        if let [
            TokenTree::Ident(name),
            TokenTree::Punct(bang),
            TokenTree::Group(args),
            ..,
        ] = &tokens[at..]
            && bang.as_char() == '!'
        {
            let name_text = name.to_string();
            if name_text == "unreachable" && args.stream().is_empty() {
                banned.push((
                    name.span().start(),
                    "`unreachable!` has no message: say in one why only a bug can reach it"
                        .to_owned(),
                ));
            } else if let Some(kept) = name_text.strip_prefix("debug_")
                && matches!(kept, "assert" | "assert_eq" | "assert_ne")
            {
                banned.push((
                    name.span().start(),
                    format!(
                        "`{name_text}!` is skipped by release builds: use `{kept}!`, which \
                         guards the real run too"
                    ),
                ));
            }
        }
        if let TokenTree::Group(group) = &tokens[at] {
            scan(group.stream(), docs, banned);
        }
        at += 1;
    }
    docs.extend(item.over("an item's doc", ITEM_CAP));
    banned.extend(item.on_test());
    docs.extend(module.over("the module doc", MODULE_CAP));
}

/// The attributes of one module or one item: where the first doc attribute starts and how many
/// lines the doc attributes span, and whether one is `#[test]`.
#[derive(Default)]
struct Block {
    doc: Option<(LineColumn, usize)>,
    test: bool,
}

impl Block {
    fn add(&mut self, first: LineColumn, lines: usize) {
        self.doc.get_or_insert((first, 0)).1 += lines;
    }

    fn over(&self, what: &'static str, cap: usize) -> Option<Doc> {
        let (LineColumn { line, .. }, lines) = self.doc?;
        (lines > cap).then_some(Doc {
            line,
            what,
            lines,
            cap,
        })
    }

    /// Where the doc of a `#[test]` item starts, with what is wrong.
    fn on_test(&self) -> Option<(LineColumn, String)> {
        let (first, _) = self.doc.filter(|_| self.test)?;
        let problem = "a test has a doc: remove it, and put a why the test needs in a `//` comment \
                       at the line it explains";
        Some((first, problem.to_owned()))
    }
}

struct Attribute {
    inner: bool,
    /// How many of the tokens it takes.
    tokens: usize,
    /// For a `doc = "…"` attribute, where it starts and how many lines it spans.
    doc: Option<(LineColumn, usize)>,
    test: bool,
}

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
    let attribute: Vec<TokenTree> = group.stream().into_iter().collect();
    let doc = match attribute.as_slice() {
        [
            TokenTree::Ident(name),
            TokenTree::Punct(equals),
            TokenTree::Literal(_),
        ] if name == "doc" && equals.as_char() == '=' => {
            let (start, end) = (hash.span().start(), group.span().end().line);
            Some((start, end - start.line + 1))
        }
        _ => None,
    };
    let test = matches!(attribute.as_slice(), [TokenTree::Ident(name)] if name == "test");
    let tokens = if inner { 3 } else { 2 };
    Some(Attribute {
        inner,
        tokens,
        doc,
        test,
    })
}

const MARKERS: [&str; 3] = ["TODO", "FIXME", "XXX"];

/// A doc comment, or a comment the lexer skips.
struct Comment<'a> {
    /// The byte it starts at in its file.
    at: usize,
    text: &'a str,
    doc: bool,
}

impl Comment<'_> {
    /// Where the comment, in `file`, holds a word of [`MARKERS`] or, as a `//` comment, a line of
    /// code, with what is wrong.
    fn problems(&self, file: &str) -> Vec<(LineColumn, String)> {
        let mut problems = Vec::new();
        let part = |c: char| c.is_alphanumeric() || c == '_';
        for marker in MARKERS {
            for (at, _) in self.text.match_indices(marker) {
                let (before, after) = (&self.text[..at], &self.text[at + marker.len()..]);
                if !before.ends_with(part) && !after.starts_with(part) {
                    problems.push((
                        position(file, self.at + at),
                        format!(
                            "a comment holds `{marker}`: open an issue for the work left, and \
                             say a known limit as a why at its code"
                        ),
                    ));
                }
            }
        }
        if let Some(line) = self.text.strip_prefix("//")
            && !self.doc
        {
            // Prose breaks its lines after a `;` too, but not after a call or an index, and
            // doesn't start a line with `let`.
            let line = line.trim();
            let statement = line
                .strip_suffix(';')
                .is_some_and(|code| code.starts_with("let ") || code.ends_with([')', ']', '?']));
            if statement || line.ends_with(['{', '}']) {
                problems.push((
                    position(file, self.at),
                    "a `//` comment holds code: remove it, as Git keeps what was there".to_owned(),
                ));
            }
        }
        problems
    }
}

/// Each comment in `text`, whose tokens are `tokens`, top to bottom.
fn comments(text: &str, tokens: TokenStream) -> Vec<Comment<'_>> {
    let mut spans = Vec::new();
    spans_of(tokens, &mut spans);
    // An empty span at the end, so that the comments after the last token are found too.
    spans.push(text.len()..text.len());
    // A doc comment is several tokens, each spanning all or a part of it, so the whole comes
    // first and its parts after it are passed over.
    spans.sort_by_key(|span| (span.start, Reverse(span.end)));
    let mut comments = Vec::new();
    let mut end = 0;
    for span in spans {
        if span.start < end {
            continue;
        }
        let mut between = &text[end..span.start];
        while let Some(start) = between.find('/') {
            let rest = &between[start..];
            let len = if rest.starts_with("/*") {
                block_len(rest)
            } else {
                rest.find('\n').unwrap_or(rest.len())
            };
            let at = span.start - between.len() + start;
            comments.push(Comment {
                at,
                text: &rest[..len],
                doc: false,
            });
            between = &rest[len..];
        }
        let token = &text[span.clone()];
        if token.starts_with("//") || token.starts_with("/*") {
            comments.push(Comment {
                at: span.start,
                text: token,
                doc: true,
            });
        }
        end = span.end;
    }
    comments
}

/// Adds to `spans` where in its file each of `tokens` lies, a group's delimiters each by itself.
fn spans_of(tokens: TokenStream, spans: &mut Vec<Range<usize>>) {
    for token in tokens {
        if let TokenTree::Group(group) = &token {
            spans.push(group.span_open().byte_range());
            spans.push(group.span_close().byte_range());
            spans_of(group.stream(), spans);
        } else {
            spans.push(token.span().byte_range());
        }
    }
}

/// How many bytes the block comment that `text` starts with takes, with those nested in it.
fn block_len(text: &str) -> usize {
    let mut depth = 0_usize;
    let mut at = 0;
    while let Some(next) = text[at..].find(['/', '*']) {
        at += next;
        if text[at..].starts_with("/*") {
            depth += 1;
            at += 2;
        } else if text[at..].starts_with("*/") {
            depth -= 1;
            at += 2;
            if depth == 0 {
                return at;
            }
        } else {
            at += 1;
        }
    }
    unreachable!("the lexer took the text as tokens, so each block comment in it ends")
}

/// Where byte `at` of `text` lies, its column counted from 0 in characters, as proc-macro2
/// counts.
fn position(text: &str, at: usize) -> LineColumn {
    let before = &text[..at];
    let line_start = before.rfind('\n').map_or(0, |newline| newline + 1);
    LineColumn {
        line: before.matches('\n').count() + 1,
        column: before[line_start..].chars().count(),
    }
}
