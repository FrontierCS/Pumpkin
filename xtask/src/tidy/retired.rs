//! The check of retired words. Each word that the `retired` table of [`CEILINGS`] lists is
//! refused outside the paths where it is right. `docs/glossary.md` defines the words to write
//! instead.
//!
//! A word is a run of letters, digits and `_`, split as an identifier is. So a word is found alike
//! in prose, in a string and in a name. The words of a URL name what it links, and are left out.

use std::collections::BTreeSet;

use super::ceilings::CEILINGS;
use super::{split, stem};
use crate::Result;

/// A word that Pumpkin no longer uses.
struct Retired {
    /// One lowercase word.
    word: String,
    /// What to write instead.
    replacement: String,
    /// The paths where the word is right: each a file, or a directory as a path that ends in `/`.
    exceptions: Vec<String>,
}

impl Retired {
    fn stem(&self) -> &str {
        stem(&self.word)
    }
}

/// Whether `exception`, a file or a directory as a path that ends in `/`, holds the file at `path`.
fn covers(exception: &str, path: &str) -> bool {
    if exception.ends_with('/') {
        path.starts_with(exception)
    } else {
        path == exception
    }
}

/// The problems of `files`, each a path and its text: each retired word that `settings`, the
/// parsed [`CEILINGS`], lists, outside its exceptions. So is an exception that holds no such word.
pub(super) fn check(settings: &toml::Table, files: &[(String, String)]) -> Result<Vec<String>> {
    let retired = retired(settings)?;
    let mut problems = Vec::new();
    // Each exception that holds its word, by the index of its word and its own.
    let mut used = BTreeSet::new();
    for (path, text) in files {
        for (index, line) in text.lines().enumerate() {
            for (column, name, word) in words(line) {
                let word_stem = stem(&word);
                for (at, retired) in retired.iter().enumerate() {
                    if word_stem != retired.stem() {
                        continue;
                    }
                    let exception =
                        (retired.exceptions.iter()).position(|exception| covers(exception, path));
                    if let Some(exception) = exception {
                        used.insert((at, exception));
                        continue;
                    }
                    let line = index + 1;
                    problems.push(format!(
                        "{path}:{line}:{column}: `{name}` holds the retired word `{}`: write `{}`, \
                         as docs/glossary.md does, or list the file as an exception in {CEILINGS} \
                         where the word is another project's",
                        retired.word, retired.replacement
                    ));
                }
            }
        }
    }
    for (at, retired) in retired.iter().enumerate() {
        for (index, exception) in retired.exceptions.iter().enumerate() {
            if !used.contains(&(at, index)) {
                problems.push(format!(
                    "{CEILINGS}: `{exception}`, an exception for `{}`, holds no such word: take \
                     it off the list",
                    retired.word
                ));
            }
        }
    }
    Ok(problems)
}

/// The retired words that `settings` lists, in its `retired` table.
fn retired(settings: &toml::Table) -> Result<Vec<Retired>> {
    let listed = settings.get("retired").and_then(toml::Value::as_table);
    let mut read = Vec::new();
    for (word, entry) in listed.into_iter().flatten() {
        let bad = |what: &str| format!("{CEILINGS}: the retired word `{word}` {what}");
        if word.is_empty() || !word.chars().all(|c| c.is_ascii_lowercase()) {
            return Err(bad("isn't one lowercase word: list each word by itself").into());
        }
        let replacement = entry.get("use").and_then(toml::Value::as_str);
        let Some(replacement) = replacement else {
            return Err(bad("has no `use`: give the word to write instead").into());
        };
        let exceptions = match entry.get("except") {
            None => Vec::new(),
            Some(exceptions) => {
                let paths = exceptions.as_array().and_then(|paths| {
                    let paths = paths.iter().map(|path| path.as_str().map(str::to_owned));
                    paths.collect::<Option<Vec<String>>>()
                });
                paths.ok_or_else(|| bad("has an `except` that isn't a list of paths"))?
            }
        };
        read.push(Retired {
            word: word.clone(),
            replacement: replacement.to_owned(),
            exceptions,
        });
    }
    Ok(read)
}

/// The words of `line`, each lowercase, with the name it is part of and that name's column, from 1.
fn words(line: &str) -> Vec<(usize, &str, String)> {
    let mut words = Vec::new();
    let mut column = 0;
    let mut start: Option<(usize, usize)> = None;
    let mut url = false;
    for (at, c) in line.char_indices().chain([(line.len(), ' ')]) {
        column += 1;
        if c.is_whitespace() {
            url = false;
        } else if line[at..].starts_with("://") {
            url = true;
            start = None;
        }
        let part = c.is_alphanumeric() || c == '_';
        match start {
            None if part && !url => start = Some((at, column)),
            Some((from, first)) if !part => {
                let name = &line[from..at];
                words.extend(split(name).into_iter().map(|word| (first, name, word)));
                start = None;
            }
            _ => {}
        }
    }
    words
}
