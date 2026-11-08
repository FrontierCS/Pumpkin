//! The checks of the repository's Rust files that `cargo xtask ci` runs: each doc within its
//! length cap, each `unreachable!` with a message, and no `debug_assert!`, as `docs/dev/style.md`
//! asks.

use std::path::Path;

use proc_macro2::{Delimiter, LineColumn, TokenStream, TokenTree};
use xshell::{Shell, cmd};

use crate::Result;

// Both caps round up the 99th percentiles of module docs and of item docs in comparable
// codebases, 36 and 14 lines: a block longer than these is rare among them.

/// The most lines a module's doc, its inner doc comments and `#![doc = "…"]` attributes, may
/// span.
const MODULE_CAP: usize = 40;

/// The most lines an item's doc, its outer doc comments and `#[doc = "…"]` attributes, may span.
const ITEM_CAP: usize = 15;

/// Fails unless, in each of the repository's Rust files, tracked or not yet, but not ignored,
/// each doc is within its cap, each `unreachable!` has a message and no `debug_assert!` is
/// used.
pub(crate) fn check(sh: &Shell) -> Result<()> {
    // Every file, filtered here: a pathspec's meaning changes with Git's variables, such as
    // `GIT_LITERAL_PATHSPECS`, and `-z` keeps unusual names as they are rather than quoted.
    let files = cmd!(sh, "git ls-files -z --cached --others --exclude-standard").read()?;
    let mut problems = Vec::new();
    // Rust files, but not a tracked one deleted from the working tree: it has nothing left to
    // check.
    let rust =
        |path: &&str| Path::new(path).extension() == Some("rs".as_ref()) && sh.path_exists(path);
    for path in files.split('\0').filter(rust) {
        let tokens: TokenStream = match sh.read_file(path)?.parse() {
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

/// A doc over its cap: its first line's number, what it documents, the lines it spans and its
/// cap.
struct Doc {
    line: usize,
    what: &'static str,
    lines: usize,
    cap: usize,
}

/// Adds to `docs` each doc over its cap in `tokens`, a module's or an item's body, and in the
/// bodies within it, and to `banned` where each banned macro call there starts, with what is wrong: an
/// `unreachable!` without a message, or a `debug_assert!`, `debug_assert_eq!` or
/// `debug_assert_ne!`.
///
/// The tokens hold every doc comment as a `doc` attribute, which spans the comment's lines;
/// strings and other comments are understood exactly. The inner `doc` attributes of `tokens`
/// are its module's doc. A run of outer attributes is an item's, whose doc is the outer `doc`
/// attributes among them. A `doc` attribute counts in the form `doc = "…"`, not in another,
/// such as `#[doc(hidden)]` or `#[doc = include_str!(…)]`.
fn scan(tokens: TokenStream, docs: &mut Vec<Doc>, banned: &mut Vec<(LineColumn, String)>) {
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    let (mut module, mut item) = (Block::default(), Block::default());
    let mut at = 0;
    while at < tokens.len() {
        if let Some(Attribute { inner, tokens, doc }) = attribute(&tokens[at..]) {
            if let Some((first, lines)) = doc {
                let block = if inner { &mut module } else { &mut item };
                block.add(first, lines);
            }
            at += tokens;
            continue;
        }
        docs.extend(item.over("an item's doc", ITEM_CAP));
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
    docs.extend(module.over("the module doc", MODULE_CAP));
}

/// The lines that the doc attributes of one module or one item span: the first one's number,
/// and how many.
#[derive(Default)]
struct Block(Option<(usize, usize)>);

impl Block {
    fn add(&mut self, first: usize, lines: usize) {
        self.0.get_or_insert((first, 0)).1 += lines;
    }

    /// The block, as a doc of `what`, if it is longer than `cap`.
    fn over(&self, what: &'static str, cap: usize) -> Option<Doc> {
        let (line, lines) = self.0?;
        (lines > cap).then_some(Doc {
            line,
            what,
            lines,
            cap,
        })
    }
}

/// An attribute that a run of tokens starts with.
struct Attribute {
    inner: bool,
    /// How many of the tokens it takes.
    tokens: usize,
    /// For a `doc = "…"` attribute, its first line's number and how many lines it spans.
    doc: Option<(usize, usize)>,
}

/// The attribute that `tokens` start with, if any.
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
            let (start, end) = (hash.span().start().line, group.span().end().line);
            Some((start, end - start + 1))
        }
        _ => None,
    };
    let tokens = if inner { 3 } else { 2 };
    Some(Attribute { inner, tokens, doc })
}
