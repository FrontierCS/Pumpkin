//! The check that keeps doc comments within their length caps, which `cargo xtask ci` runs.

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

/// Fails unless each doc in the repository's Rust files, tracked or not yet, but not ignored,
/// is within its cap.
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
                    "{path}:{line}:{column}: the file is not valid Rust here, so its docs can't \
                     be measured: make it valid Rust"
                ));
                continue;
            }
        };
        let mut docs = Vec::new();
        scan(tokens, &mut docs);
        docs.sort_by_key(|doc| doc.line);
        problems.extend(docs.into_iter().map(|doc| {
            let Doc {
                line,
                what,
                lines,
                cap,
            } = doc;
            format!(
                "{path}:{line}: {what} is {lines} lines, over the cap of {cap}: shorten it to \
                 {cap} or fewer, moving each fact to its home"
            )
        }));
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
/// bodies within it.
///
/// The tokens hold every doc comment as a `doc` attribute, which spans the comment's lines;
/// strings and other comments are understood exactly. The inner `doc` attributes of `tokens`
/// are its module's doc. A run of outer attributes is an item's, whose doc is the outer `doc`
/// attributes among them. A `doc` attribute counts in the form `doc = "…"`, not in another,
/// such as `#[doc(hidden)]` or `#[doc = include_str!(…)]`.
fn scan(tokens: TokenStream, docs: &mut Vec<Doc>) {
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
        if let TokenTree::Group(group) = &tokens[at] {
            scan(group.stream(), docs);
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
