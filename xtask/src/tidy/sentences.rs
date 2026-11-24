//! The check of a Markdown document's sentences, each one idea: at most [`SENTENCE_CAP`] words,
//! and no semicolon that chains clauses. Paragraphs and list items hold sentences. Headings,
//! tables, code, HTML, images and links' targets hold none.

use std::mem;

use super::{Cap, Counts};

// GitLab's documentation caps a sentence at 25 words, and Coder's guide counts a code span as one
// word, as a reader takes it in as one:
// https://gitlab.com/gitlab-org/gitlab-runner/-/blob/56eb4592c87be8cbc81d8d8f55a45a2c5ea53e6a/docs/.vale/gitlab_base/SentenceLength.yml#L1-L11
// https://github.com/coder/coder/blob/8d5ad46e28bb73db8ec9d809ad400bc7f2e244c6/docs/.style/style-guide/voice-and-tone.md#L178-L179
const SENTENCE_CAP: usize = 25;

/// The cap of a document's sentences to split: none, unless its table lists the document.
pub(super) const CAP: Cap = Cap {
    table: "sentences",
    kind: "Markdown document",
    unit: ["sentence to split", "sentences to split"],
    limit: 0,
    fix: "split each that a line below names",
};

/// A paragraph or a list item: its lines, each with its number from 1, trimmed.
#[derive(Default)]
struct Block<'a> {
    lines: Vec<(usize, &'a str)>,
    item: bool,
}

/// The sentences to split of each of `documents`, each a path and its text.
pub(super) fn counts(documents: &[(String, String)]) -> Counts {
    let counts = documents.iter().map(|(path, text)| {
        let found = check(path, text);
        (path.clone(), found.len(), found)
    });
    counts.collect()
}

/// The problems of `text`, the Markdown document at `path`: each sentence over [`SENTENCE_CAP`]
/// words, or with a semicolon, at the line it starts on.
fn check(path: &str, text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for Block { lines, item } in blocks(text) {
        // Where each line starts in the text of the block, which joins them.
        let (mut starts, mut joined) = (Vec::new(), String::new());
        for (number, line) in lines {
            starts.push((joined.len(), number));
            joined.push_str(line);
            joined.push(' ');
        }
        let prose = prose(&joined);
        let mut words: Vec<(usize, String)> = (prose.split(|(_, c)| c.is_whitespace()))
            .filter(|word| !word.is_empty())
            .map(|word| (word[0].0, word.iter().map(|(_, c)| c).collect()))
            .collect();
        // A semicolon that ends a list item separates it from the next.
        if let Some((_, last)) = words.last_mut().filter(|_| item) {
            last.truncate(last.trim_end_matches(';').len());
        }
        let mut first = 0;
        for end in 1..=words.len() {
            if (words.get(end)).is_some_and(|(_, next)| !ends(&words[end - 1].1, next)) {
                continue;
            }
            let sentence = &words[mem::replace(&mut first, end)..end];
            // The first line starts at 0, at or before each word.
            let line = starts[starts.partition_point(|&(at, _)| at <= sentence[0].0) - 1].1;
            let count = (sentence.iter())
                .filter(|(_, word)| word.contains(|c: char| c == '`' || c.is_alphanumeric()))
                .count();
            let problem = if count > SENTENCE_CAP {
                format!("a sentence of {count} words, over the cap of {SENTENCE_CAP}: split it")
            } else if sentence.iter().any(|(_, word)| word.contains(';')) {
                "a sentence chains clauses with a semicolon: make each a sentence, or each part \
                 an item of a list"
                    .to_owned()
            } else {
                continue;
            };
            problems.push(format!("{path}:{line}: {problem}"));
        }
    }
    problems
}

/// The paragraphs and list items of `text`, a Markdown document.
fn blocks(text: &str) -> Vec<Block<'_>> {
    let mut blocks = Vec::new();
    let mut block = Block::default();
    // Whether the line is in a fenced code block, or in an HTML block, which runs to a blank line.
    let (mut fenced, mut html) = (false, false);
    for (index, line) in text.lines().enumerate() {
        // A block quote's text, without its markers, is prose as any other.
        let trimmed = line.trim().trim_start_matches(['>', ' ']);
        let starts = block.lines.is_empty();
        let heading = trimmed.starts_with('#') && trimmed.trim_start_matches('#').starts_with(' ');
        if fenced {
            fenced = !trimmed.starts_with("```");
        } else if html {
            html = !trimmed.is_empty();
        } else if trimmed.starts_with("```") {
            blocks.extend(taken(&mut block));
            fenced = true;
        } else if starts && (line.starts_with("    ") || trimmed.starts_with('<')) {
            // Indented code, or HTML.
            html = trimmed.starts_with('<');
        } else if let Some(text) = item(trimmed) {
            blocks.extend(taken(&mut block));
            block = Block {
                lines: vec![(index + 1, text)],
                item: true,
            };
        } else if trimmed.is_empty() || heading || trimmed.starts_with('|') {
            blocks.extend(taken(&mut block));
        } else {
            block.lines.push((index + 1, trimmed));
        }
    }
    blocks.extend(taken(&mut block));
    blocks
}

fn taken<'a>(block: &mut Block<'a>) -> Option<Block<'a>> {
    Some(mem::take(block)).filter(|block| !block.lines.is_empty())
}

/// The text of the list item that `line` starts, after its marker, if it starts one: a `-`, a `*`,
/// or a number and a `.` or a `)`.
// The markers that rustc-dev-guide's `sembr` reads:
// https://github.com/rust-lang/rust/blob/58cf8f9c35b355199727627d07ae34a4e2846d67/src/doc/rustc-dev-guide/ci/sembr/src/main.rs#L28-L30
fn item(line: &str) -> Option<&str> {
    let after_number = line.trim_start_matches(|c: char| c.is_ascii_digit());
    let rest = if after_number.len() < line.len() {
        after_number.strip_prefix(['.', ')'])
    } else {
        line.strip_prefix(['-', '*'])
    };
    rest?.strip_prefix(' ')
}

/// The characters of `text`, each with where it starts, less its images and its links' targets,
/// and with each code span as one backtick.
fn prose(text: &str) -> Vec<(usize, char)> {
    let mut prose = Vec::new();
    let mut at = 0;
    while let Some(c) = text[at..].chars().next() {
        let rest = &text[at..];
        let ticks = rest.len() - rest.trim_start_matches('`').len();
        let code = (ticks > 0)
            .then(|| rest[ticks..].find(&rest[..ticks]))
            .flatten();
        let left_out = if rest.starts_with("![") {
            (rest.find("](")).and_then(|alt| Some(alt + rest[alt..].find(')')? + 1))
        } else if rest.starts_with("](") {
            rest.find(')').map(|end| end + 1)
        } else {
            None
        };
        if let Some(len) = left_out {
            at += len;
        } else if let Some(end) = code {
            prose.push((at, '`'));
            at += ticks + end + ticks;
        } else {
            prose.push((at, c));
            at += c.len_utf8();
        }
    }
    prose
}

/// Whether a sentence ends between `word` and `next`: after a stop and the marks that close what
/// it ends. A stop before a lowercase word, as in "e.g. a", ends no sentence.
fn ends(word: &str, next: &str) -> bool {
    let closing = [')', '"', '\'', '’', '”', '*', '_'];
    let stop = word.trim_end_matches(closing).ends_with(['.', '!', '?']);
    let next = next.trim_start_matches(['(', '"', '\'', '‘', '“', '*', '_', '[']);
    stop && !next.starts_with(char::is_lowercase)
}
