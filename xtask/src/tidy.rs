//! The checks of the rules that a machine can check, which `cargo xtask ci` runs: of comments and
//! docs, of a source file's lines and a document's words, of links, of the figures and of the
//! layout of `tests/`. `docs/dev/` doesn't restate a rule that a check holds, so each problem says
//! what is wrong and what to do.
//!
//! Each file is read as proc-macro2's tokens, so a string literal is never taken for a comment: a
//! doc comment is a token, and any other comment lies between two. Code in a comment is told from
//! prose by how its line ends, and a restating doc by its words, so only some of each is found.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::mem;
use std::ops::Range;
use std::path::Path;

use proc_macro2::{Delimiter, LineColumn, Spacing, TokenStream, TokenTree};

use crate::Result;
use crate::repo::Repo;

// By their paths, as xtask's test binary includes this file from elsewhere.
#[path = "tidy/figures.rs"]
mod figures;
#[path = "tidy/links.rs"]
mod links;

// The 95th percentiles of module docs' and item docs' lengths in comparable Rust applications.
const MODULE_CAP: usize = 25;

const ITEM_CAP: usize = 9;

const TEST_COMMENT_CAP: usize = 2;

// Rustfmt's `max_width` for code.
const WIDTH: usize = 100;

// A file past it holds several things, each better read and changed in a module of its own.
const FILE_CAP: usize = 1_000;

// A guide grows a sentence at a time, each harmless alone, until it restates the help or the code.
// The cap sits above all but a few documents, which `CEILINGS` lists, so that most edits of a
// document leave `CEILINGS` as it is.
const WORD_CAP: usize = 1_500;

const SOURCES: [&str; 8] = ["rs", "star", "js", "css", "html", "py", "sh", "tf"];

/// Where the suites that a third party wrote are vendored, which the checks leave as they are.
const VENDORED: &str = "crates/lang/tests/conformance/";

// The files over a cap, each with its ceiling, the most it may hold, which only goes down: a
// ratchet, as TigerBeetle's tidy holds the functions that are already long and stops the others
// growing past its cap, and fails on an exception that names no file:
// https://github.com/tigerbeetle/tigerbeetle/blob/24d92bff033453dcc757adfcd9aa14655d1b1734/src/tidy.zig#L731-L753
// https://github.com/tigerbeetle/tigerbeetle/blob/24d92bff033453dcc757adfcd9aa14655d1b1734/src/tidy.zig#L1496-L1508
const CEILINGS: &str = ".config/tidy.toml";

/// A cap on what each file of a kind holds, whose exceptions are its table in [`CEILINGS`].
struct Cap {
    table: &'static str,
    /// The files it applies to, as a problem names a file that isn't one.
    kind: &'static str,
    unit: &'static str,
    limit: usize,
    /// What to do with a file over the cap.
    fix: &'static str,
}

const LINES: Cap = Cap {
    table: "long_files",
    kind: "source file",
    unit: "lines",
    limit: FILE_CAP,
    fix: "put what is new in a module of its own, or split the file into modules that each hold \
          one thing, in a pull request that only moves code",
};

const WORDS: Cap = Cap {
    table: "words",
    kind: "Markdown document",
    unit: "words",
    limit: WORD_CAP,
    fix: "shorten it, moving each fact to the help or the code it describes",
};

/// Fails unless each of the repository's Rust files, tracked or not yet, but not ignored, its
/// other source files, its `ARCHITECTURE.md`, its Markdown documents, the tracked ones' words, its
/// figures and the files under its `tests/` directories pass the checks. The error holds a line
/// for each problem.
pub(crate) fn check(repo: &Repo<'_>) -> Result<()> {
    let mut problems = Vec::new();
    let mut names = BTreeSet::new();
    for path in files(repo, "rs")? {
        let text = repo.read(&path)?;
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
        figures::defined(tokens.clone(), &mut names);
        let comments = comments(&text, tokens.clone());
        let mut found = Found::default();
        for comment in &comments {
            found.banned.extend(comment.problems(&text));
        }
        scan(&text, tokens, &mut found);
        let Found {
            docs,
            mut banned,
            tests,
        } = found;
        banned.extend(test_comments(&text, &comments, &tests));
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
    problems.extend(architecture(repo)?);
    let listed = listed(repo)?;
    problems.extend(test_data(&listed));
    problems.extend(test_binaries(&listed));
    let ceilings = ceilings(repo)?;
    problems.extend(capped(&ceilings, &LINES, line_counts(repo, &listed)?)?);
    problems.extend(capped(&ceilings, &WORDS, word_counts(repo)?)?);
    problems.extend(links::check(repo, &listed)?);
    names.extend(figures::packages(repo, &listed)?);
    problems.extend(figures::check(repo, &listed, &names)?);
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("\n").into())
    }
}

/// The files that Git lists, see [`check`], whose extension is `extension`.
pub(crate) fn files(repo: &Repo<'_>, extension: &str) -> Result<Vec<String>> {
    let mut files = listed(repo)?;
    files.retain(|path| Path::new(path).extension() == Some(extension.as_ref()));
    Ok(files)
}

/// The paths of the files that Git lists, tracked or untracked and not ignored, that exist.
fn listed(repo: &Repo<'_>) -> Result<Vec<String>> {
    // Every file, which each check filters itself; `-z` keeps unusual names as they are rather
    // than quoted.
    let files = repo.git([
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
    ])?;
    let kept = |path: &&str| !path.is_empty() && repo.has(path);
    Ok(files.split('\0').filter(kept).map(str::to_owned).collect())
}

/// The problems of `paths`: a file under a `tests/` directory that isn't Rust, but for
/// `crates/lang`'s vendored suites and the `.star` fixtures, which are input and not test data.
/// The `.svg` snapshots of `pumpkin`'s help are allowed too, as `cargo xtask refill` writes them.
fn test_data(paths: &[String]) -> Vec<String> {
    let mut problems = Vec::new();
    for path in paths {
        let file = Path::new(path);
        let extension = file.extension().and_then(|extension| extension.to_str());
        let input = path.starts_with(VENDORED)
            || (path.starts_with("crates/lang/tests/fixtures/") && extension == Some("star"));
        let snapshot =
            path.starts_with("crates/pumpkin/tests/it/help/") && extension == Some("svg");
        if under_tests(file) && extension != Some("rs") && !input && !snapshot {
            problems.push(format!(
                "{path}: a file that isn't Rust, under a `tests/` directory: make what the test \
                 reads in the test, with Pumpkin's own writer where Pumpkin reads it"
            ));
        }
    }
    problems
}

/// The problems of `paths`: a package with more than one integration test binary, of which cargo
/// builds one of each `tests/*.rs` and each `tests/*/main.rs` beside a `Cargo.toml`.
// Each binary links the package and its dependencies again, as matklad's post says:
// https://matklad.github.io/2021/02/27/delete-cargo-integration-tests.html
fn test_binaries(paths: &[String]) -> Vec<String> {
    let mut problems = Vec::new();
    let manifests = paths
        .iter()
        .filter(|path| path.rsplit('/').next() == Some("Cargo.toml"));
    for manifest in manifests {
        let dir = manifest.strip_suffix("Cargo.toml").unwrap_or_default();
        let tests: Vec<&str> = (paths.iter())
            .filter_map(|path| path.strip_prefix(dir)?.strip_prefix("tests/"))
            .filter(|test| match test.split_once('/') {
                None => Path::new(test).extension() == Some("rs".as_ref()),
                Some((_, file)) => file == "main.rs",
            })
            .collect();
        if tests.len() > 1 {
            problems.push(format!(
                "{manifest}: the package has {} integration test binaries, `tests/{}`: make each a \
                 module of one, as each links the package and its dependencies again",
                tests.len(),
                tests.join("`, `tests/")
            ));
        }
    }
    problems
}

fn under_tests(file: &Path) -> bool {
    (file.parent()).is_some_and(|parent| {
        (parent.components()).any(|component| component.as_os_str() == "tests")
    })
}

fn ceilings(repo: &Repo<'_>) -> Result<toml::Table> {
    if !repo.has(CEILINGS) {
        return Ok(toml::Table::new());
    }
    Ok(toml::from_str(&repo.read(CEILINGS)?)
        .map_err(|error| format!("{CEILINGS} isn't TOML: {error}"))?)
}

fn ceilings_of(ceilings: &toml::Table, table: &str, unit: &str) -> Result<BTreeMap<String, usize>> {
    let listed = ceilings.get(table).and_then(toml::Value::as_table);
    let mut read = BTreeMap::new();
    for (path, ceiling) in listed.into_iter().flatten() {
        let ceiling = ceiling
            .as_integer()
            .and_then(|ceiling| usize::try_from(ceiling).ok());
        let Some(ceiling) = ceiling else {
            return Err(format!(
                "{CEILINGS}: `{path}`'s ceiling isn't a count of {unit}: give it the file's"
            )
            .into());
        };
        read.insert(path.clone(), ceiling);
    }
    Ok(read)
}

/// The problems of `counts`, each file of `cap`'s kind with how many of its unit it holds: one
/// over the cap and not listed in `cap`'s table of `ceilings`, or past its ceiling there, and each
/// entry of the table that is stale, its file gone, within the cap or under its ceiling.
fn capped(ceilings: &toml::Table, cap: &Cap, counts: Vec<(String, usize)>) -> Result<Vec<String>> {
    let Cap {
        table,
        kind,
        unit,
        limit: cap,
        fix,
    } = *cap;
    let mut ceilings = ceilings_of(ceilings, table, unit)?;
    let mut problems = Vec::new();
    for (path, count) in counts {
        let file = format!("{path}: the file is {count} {unit}");
        let problem = match ceilings.remove(&path) {
            None if count > cap => format!("{file}, over the cap of {cap}: {fix}"),
            Some(ceiling) if count > ceiling => format!(
                "{file}, over the cap of {cap} and its ceiling of {ceiling} in {CEILINGS}, which \
                 only goes down: {fix}"
            ),
            Some(_) if count <= cap => {
                format!("{file}, within the cap of {cap}: take it off {CEILINGS}")
            }
            Some(ceiling) if count < ceiling => format!(
                "{file}, under its ceiling of {ceiling} in {CEILINGS}: lower the ceiling to {count}"
            ),
            None | Some(_) => continue,
        };
        problems.push(problem);
    }
    for path in ceilings.keys() {
        problems.push(format!(
            "{CEILINGS}: `{path}` isn't a {kind} of the repository: take it off the list"
        ));
    }
    Ok(problems)
}

/// The lines of each of `paths` that is a source file ([`SOURCES`]) outside a `tests/` directory.
fn line_counts(repo: &Repo<'_>, paths: &[String]) -> Result<Vec<(String, usize)>> {
    let mut counts = Vec::new();
    for path in paths {
        let file = Path::new(path);
        let extension = file.extension().and_then(|extension| extension.to_str());
        if !under_tests(file) && extension.is_some_and(|extension| SOURCES.contains(&extension)) {
            counts.push((path.clone(), repo.read(path)?.lines().count()));
        }
    }
    Ok(counts)
}

/// The words of each tracked Markdown document, as `wc -w` counts them.
fn word_counts(repo: &Repo<'_>) -> Result<Vec<(String, usize)>> {
    // Only tracked documents count: a checkout's untracked notes aren't the repository's. A path
    // that a merge left in conflict is listed once.
    let tracked = repo.git(["ls-files", "-z", "--cached", "--deduplicate", "--", "*.md"])?;
    let mut counts = Vec::new();
    for path in tracked.split('\0').filter(|path| !path.is_empty()) {
        let file = Path::new(path);
        // A symbolic link counts as its target, and a document under `tests/` is a test's input.
        if !under_tests(file) && repo.has(file) && !repo.root.join(file).is_symlink() {
            let words = repo.read(file)?.split_whitespace().count();
            counts.push((path.to_owned(), words));
        }
    }
    Ok(counts)
}

/// The problems of `ARCHITECTURE.md`'s links: one to code, or to a document that isn't there.
fn architecture(repo: &Repo<'_>) -> Result<Vec<String>> {
    const PATH: &str = "ARCHITECTURE.md";
    if !repo.has(PATH) {
        return Ok(Vec::new());
    }
    let text = repo.read(PATH)?;
    let mut problems = Vec::new();
    for (index, line) in text.lines().enumerate() {
        for (at, opening) in line.match_indices("](") {
            let target = &line[at + opening.len()..];
            let target = &target[..target.find(')').unwrap_or(target.len())];
            if target.contains("://") || target.starts_with('#') {
                continue;
            }
            // An image is the document's own content, which `figures` checks.
            if (line[..at].rfind('[')).is_some_and(|open| line[..open].ends_with('!')) {
                continue;
            }
            let path = &target[..target.find('#').unwrap_or(target.len())];
            let problem = if Path::new(path).extension() != Some("md".as_ref()) {
                "code in the repository: name it instead, in backticks"
            } else if repo.has(path) {
                continue;
            } else {
                "which isn't in the repository: link a document that is, or remove the link"
            };
            let (line, column) = (index + 1, line[..at].chars().count() + 3);
            problems.push(format!(
                "{PATH}:{line}:{column}: a link to `{target}`, {problem}"
            ));
        }
    }
    Ok(problems)
}

/// What `scan` finds in a file.
#[derive(Default)]
struct Found {
    docs: Vec<Doc>,
    banned: Vec<(LineColumn, String)>,
    /// For each `#[test]` item, the lines from its first attribute's to its own, left out.
    tests: Vec<Range<usize>>,
}

/// A doc, or a module doc, that is over its cap.
struct Doc {
    line: usize,
    what: &'static str,
    lines: usize,
    cap: usize,
}

/// Adds to `found` what is wrong in `tokens`, those of `file`, and in each group of them, and the
/// line ranges of its tests.
///
/// A module's doc is its inner `doc = "…"` attributes, doc comments included, and an item's the
/// outer ones in its run of attributes; each counts the lines it spans.
fn scan(file: &str, tokens: TokenStream, found: &mut Found) {
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    let (mut module, mut item) = (Block::default(), Block::default());
    let mut at = 0;
    while at < tokens.len() {
        if let Some(Attribute {
            inner,
            tokens,
            start,
            doc,
            prose,
            test,
        }) = attribute(file, &tokens[at..])
        {
            let block = if inner { &mut module } else { &mut item };
            block.start.get_or_insert(start.line);
            if let Some((first, lines)) = doc {
                block.add(first, lines);
            }
            block.prose.extend(prose);
            block.test |= test;
            at += tokens;
            continue;
        }
        if let Some(start) = item.start.filter(|_| item.test) {
            found.tests.push(start..tokens[at].span().start().line);
        }
        found.banned.extend(item.restated(file, &tokens[at..]));
        found.docs.extend(item.over("an item's doc", ITEM_CAP));
        found.banned.extend(item.on_test());
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
                found.banned.push((
                    name.span().start(),
                    "`unreachable!` has no message: say in one why only a bug can reach it"
                        .to_owned(),
                ));
            } else if let Some(kept) = name_text.strip_prefix("debug_")
                && matches!(kept, "assert" | "assert_eq" | "assert_ne")
            {
                found.banned.push((
                    name.span().start(),
                    format!(
                        "`{name_text}!` is skipped by release builds: use `{kept}!`, which \
                         guards the real run too"
                    ),
                ));
            }
        }
        if let TokenTree::Group(group) = &tokens[at] {
            scan(file, group.stream(), found);
        }
        at += 1;
    }
    found.docs.extend(item.over("an item's doc", ITEM_CAP));
    found.banned.extend(item.on_test());
    found.docs.extend(module.over("the module doc", MODULE_CAP));
}

/// The attributes above an item, or the inner attributes of a module: where they start, how many
/// lines of doc they hold, its prose, and whether the item is a test.
#[derive(Default)]
struct Block<'a> {
    start: Option<usize>,
    doc: Option<(LineColumn, usize)>,
    prose: Vec<&'a str>,
    test: bool,
}

impl Block<'_> {
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

    fn on_test(&self) -> Option<(LineColumn, String)> {
        let (first, _) = self.doc.filter(|_| self.test)?;
        let problem = "a test has a doc: remove it, and put a why the test needs in a `//` comment \
                       at the line it explains";
        Some((first, problem.to_owned()))
    }

    /// A problem if each word of the doc's first sentence, but the `FILLER`, is one that the item's
    /// first line already holds, in its name, its type or its parameters, stems alike.
    fn restated(&self, file: &str, tokens: &[TokenTree]) -> Option<(LineColumn, String)> {
        let (first, _) = self.doc?;
        if self.prose.first()?.starts_with("```") {
            return None;
        }
        let name = name(tokens)?;
        let line = file.lines().nth(tokens.first()?.span().start().line - 1)?;
        let named: Vec<String> = identifiers(line).into_iter().flat_map(split).collect();
        let named: BTreeSet<&str> = named.iter().map(|word| stem(word)).collect();
        let prose = self.prose.join(" ");
        let words = words(first_sentence(&prose));
        let mut content = words
            .iter()
            .filter(|word| !FILLER.contains(&word.as_str()))
            .peekable();
        content.peek()?;
        content.all(|word| named.contains(stem(word))).then(|| {
            let problem = format!(
                "the doc's first sentence only restates `{name}`, its type or its parameters: say \
                 what they leave a caller room to get wrong, or remove the doc"
            );
            (first, problem)
        })
    }
}

/// The words that a doc's first sentence may hold without saying more than its item's line does.
const FILLER: &[&str] = &[
    "a", "an", "the", "its", "it", "it's", "this", "that", "these", "those", "their", "his", "her",
    "them", "they", "of", "for", "to", "in", "on", "at", "by", "from", "with", "as", "into",
    "onto", "over", "under", "per", "is", "are", "was", "were", "be", "been", "being", "has",
    "have", "had", "do", "does", "did", "and", "or", "nor", "but", "so", "if", "else", "than",
    "then", "which", "what", "who", "whose", "whom", "where", "when", "how", "why", "each",
    "every", "one", "some", "any", "all", "no", "not", "here", "there", "now",
];

/// The name that the item at the start of `tokens` declares, if it declares one: of a function,
/// a type, a constant, a macro, a field or a variant.
fn name(tokens: &[TokenTree]) -> Option<String> {
    match unqualified(tokens) {
        [
            TokenTree::Ident(kind),
            TokenTree::Punct(bang),
            TokenTree::Ident(name),
            ..,
        ] if kind == "macro_rules" && bang.as_char() == '!' => Some(name.to_string()),
        [
            TokenTree::Ident(kind),
            TokenTree::Ident(word),
            TokenTree::Ident(name),
            ..,
        ] if kind == "static" && word == "mut" => Some(name.to_string()),
        [kind, TokenTree::Ident(name), ..]
            if matches!(
                word(kind).as_str(),
                "fn" | "struct" | "enum" | "trait" | "type" | "mod" | "static" | "const" | "union"
            ) =>
        {
            Some(name.to_string())
        }
        [TokenTree::Ident(name), TokenTree::Punct(colon), ..]
            if colon.as_char() == ':' && colon.spacing() == Spacing::Alone =>
        {
            Some(name.to_string())
        }
        [TokenTree::Ident(name), next @ ..]
            if name
                .to_string()
                .starts_with(|c: char| c.is_ascii_uppercase())
                && match next.first() {
                    None | Some(TokenTree::Group(_)) => true,
                    Some(TokenTree::Punct(punct)) => matches!(punct.as_char(), ',' | '='),
                    Some(_) => false,
                } =>
        {
            Some(name.to_string())
        }
        _ => None,
    }
}

/// `tokens` without the qualifiers that an item begins with, such as `pub(crate)` or `async`.
fn unqualified(tokens: &[TokenTree]) -> &[TokenTree] {
    let mut at = 0;
    while let Some(token) = tokens.get(at) {
        let before = at.checked_sub(1).map(|before| word(&tokens[before]));
        let qualifier = match token {
            TokenTree::Ident(_) => match word(token).as_str() {
                "pub" | "async" | "unsafe" | "extern" => true,
                "const" => tokens.get(at + 1).is_some_and(|next| {
                    matches!(word(next).as_str(), "fn" | "async" | "unsafe" | "extern")
                }),
                _ => false,
            },
            // `pub(crate)`'s scope, and `extern "C"`'s ABI.
            TokenTree::Group(group) => {
                group.delimiter() == Delimiter::Parenthesis && before.as_deref() == Some("pub")
            }
            TokenTree::Literal(_) => before.as_deref() == Some("extern"),
            TokenTree::Punct(_) => false,
        };
        if !qualifier {
            break;
        }
        at += 1;
    }
    &tokens[at..]
}

fn word(token: &TokenTree) -> String {
    match token {
        TokenTree::Ident(word) => word.to_string(),
        _ => String::new(),
    }
}

/// The identifiers in `line`, keywords included, and those in its string literals and comments.
fn identifiers(line: &str) -> Vec<&str> {
    let mut identifiers = Vec::new();
    let mut start = None;
    for (at, c) in line.char_indices().chain([(line.len(), ' ')]) {
        match start {
            None if c.is_ascii_alphabetic() || c == '_' => start = Some(at),
            Some(from) if !(c.is_alphanumeric() || c == '_') => {
                identifiers.push(&line[from..at]);
                start = None;
            }
            _ => {}
        }
    }
    identifiers
}

/// The lowercase words of `identifier`, as written in snake or camel case: `HTTPServer2` gives
/// `http`, `server` and `2`.
fn split(identifier: &str) -> Vec<String> {
    let chars: Vec<char> = identifier.chars().collect();
    let mut parts = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let run = |from: usize, keep: fn(&char) -> bool| {
            from + chars[from..].iter().take_while(|c| keep(c)).count()
        };
        let c = chars[at];
        let end = if c.is_ascii_uppercase() {
            let upper = run(at, char::is_ascii_uppercase);
            match chars.get(upper) {
                // The last capital starts a word with the lowercase after it.
                Some(next) if next.is_ascii_lowercase() && upper - at > 1 => upper - 1,
                Some(next) if next.is_ascii_lowercase() => run(upper, char::is_ascii_lowercase),
                _ => upper,
            }
        } else if c.is_ascii_lowercase() {
            run(at, char::is_ascii_lowercase)
        } else if c.is_ascii_digit() {
            run(at, char::is_ascii_digit)
        } else {
            at += 1;
            continue;
        };
        parts.push(
            chars[at..end]
                .iter()
                .map(char::to_ascii_lowercase)
                .collect(),
        );
        at = end;
    }
    parts
}

/// `word` without a usual ending, so that `listed` and `lists` are of the one stem as `list` is.
fn stem(word: &str) -> &str {
    for suffix in ["ies", "es", "s", "ed", "ing", "er"] {
        if word.len() > suffix.len() + 2
            && let Some(stem) = word.strip_suffix(suffix)
        {
            return stem;
        }
    }
    word
}

/// The text of `prose` to the first stop that is followed by a space or the end.
fn first_sentence(prose: &str) -> &str {
    let end = prose.char_indices().skip(1).find(|&(at, c)| {
        matches!(c, '.' | '!' | '?')
            && prose[at + 1..]
                .chars()
                .next()
                .is_none_or(char::is_whitespace)
    });
    end.map_or(prose, |(at, _)| &prose[..=at]).trim()
}

/// The lowercase words of `sentence`, each a letter and the letters, digits and `'` after it,
/// leaving out the target of each Markdown link.
fn words(sentence: &str) -> Vec<String> {
    let (mut words, mut word) = (Vec::new(), String::new());
    let mut chars = sentence.chars();
    let mut last = ' ';
    while let Some(c) = chars.next() {
        if last == ']' && c == '(' {
            chars.by_ref().find(|&c| c == ')');
            last = ')';
            continue;
        }
        if c.is_ascii_alphabetic() || !word.is_empty() && (c.is_ascii_digit() || c == '\'') {
            word.push(c.to_ascii_lowercase());
        } else if !word.is_empty() {
            words.push(mem::take(&mut word));
        }
        last = c;
    }
    words.extend((!word.is_empty()).then_some(word));
    words
}

/// An attribute, with the lines that it takes and the prose of it if it is a doc.
struct Attribute<'a> {
    inner: bool,
    /// How many of the tokens it takes.
    tokens: usize,
    start: LineColumn,
    /// For a `doc = "…"` attribute, where it starts and how many lines it spans.
    doc: Option<(LineColumn, usize)>,
    /// For a `///` comment, its text after the slashes, trimmed.
    prose: Option<&'a str>,
    test: bool,
}

/// The attribute that `tokens`, those of `file`, begin with, if one.
fn attribute<'a>(file: &'a str, tokens: &[TokenTree]) -> Option<Attribute<'a>> {
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
    let start = hash.span().start();
    let doc = match attribute.as_slice() {
        [
            TokenTree::Ident(name),
            TokenTree::Punct(equals),
            TokenTree::Literal(_),
        ] if name == "doc" && equals.as_char() == '=' => {
            Some((start, group.span().end().line - start.line + 1))
        }
        _ => None,
    };
    // A doc comment's tokens each span all of it.
    let prose = doc
        .and_then(|_| file[hash.span().byte_range()].strip_prefix("///"))
        .filter(|text| !text.starts_with('/'))
        .map(str::trim);
    let test = matches!(attribute.as_slice(), [TokenTree::Ident(name)] if name == "test");
    let tokens = if inner { 3 } else { 2 };
    Some(Attribute {
        inner,
        tokens,
        start,
        doc,
        prose,
        test,
    })
}

/// The problems of the `comments` in `tests`, ranges of lines of tests, or right above them: a
/// block of `//` lines over `TEST_COMMENT_CAP`.
fn test_comments(
    file: &str,
    comments: &[Comment<'_>],
    tests: &[Range<usize>],
) -> Vec<(LineColumn, String)> {
    let own_line: BTreeMap<usize, LineColumn> = comments
        .iter()
        .filter(|comment| !comment.doc && comment.text.starts_with("//"))
        .filter(|comment| {
            let before = file[..comment.at].rsplit('\n').next();
            before.is_some_and(|before| before.trim().is_empty())
        })
        .map(|comment| {
            let start = position(file, comment.at);
            (start.line, start)
        })
        .collect();
    let mut problems = Vec::new();
    for lines in tests {
        let mut top = lines.start;
        while own_line.contains_key(&(top - 1)) {
            top -= 1;
        }
        // The item's own line holds no `//` comment that starts it, so it ends the last run.
        let mut run: Option<(LineColumn, usize)> = None;
        for line in top..=lines.end {
            if let Some(&start) = own_line.get(&line) {
                run.get_or_insert((start, 0)).1 += 1;
            } else if let Some((start, count)) = run.take()
                && count > TEST_COMMENT_CAP
            {
                problems.push((
                    start,
                    format!(
                        "a test has a `//` block of {count} lines, over the cap of \
                         {TEST_COMMENT_CAP}: keep to a why of a line or two, at the line it \
                         explains"
                    ),
                ));
            }
        }
    }
    problems
}

const MARKERS: [&str; 3] = ["TODO", "FIXME", "XXX"];

/// A comment of a file, with where it starts in the text and whether it documents an item.
struct Comment<'a> {
    at: usize,
    text: &'a str,
    doc: bool,
}

impl Comment<'_> {
    /// What is wrong in the comment: a marker of unfinished work, code, and a line over `WIDTH`.
    /// `file` is the text it lies in.
    fn problems(&self, file: &str) -> Vec<(LineColumn, String)> {
        let mut problems = Vec::new();
        let part = |c: char| c.is_alphanumeric() || c == '_';
        for marker in MARKERS {
            for (at, _) in self.text.match_indices(marker) {
                let (before, after) = (&self.text[..at], &self.text[at + marker.len()..]);
                // The marker as a word of its own, not inside another.
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
        let mut at = self.at;
        for line in self.text.split('\n') {
            let start = file[..at].rfind('\n').map_or(0, |newline| newline + 1);
            let end = file[at..]
                .find('\n')
                .map_or(file.len(), |newline| at + newline);
            let columns = file[start..end].chars().count();
            let prose = line.trim_start().trim_start_matches(['/', '*', '!']).trim();
            // Neither a Markdown table's row nor a word, such as a URL, can be wrapped.
            if columns > WIDTH && !prose.starts_with('|') && prose.contains(char::is_whitespace) {
                let line = position(file, start).line;
                problems.push((
                    LineColumn {
                        line,
                        column: WIDTH,
                    },
                    format!(
                        "a line of a comment is {columns} columns, over the cap of {WIDTH}: \
                         wrap it"
                    ),
                ));
            }
            at += line.len() + 1;
        }
        problems
    }
}

/// The comments of `text`, whose tokens are `tokens`: a doc is a token, and each other comment
/// lies in the text between the tokens' spans.
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

/// The length of the block comment that `text` begins with, in which block comments nest.
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

/// The line, from 1, and the column, from 0 in characters, of the byte `at` of `text`.
fn position(text: &str, at: usize) -> LineColumn {
    let before = &text[..at];
    let line_start = before.rfind('\n').map_or(0, |newline| newline + 1);
    LineColumn {
        line: before.matches('\n').count() + 1,
        column: before[line_start..].chars().count(),
    }
}
