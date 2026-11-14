//! A recipe's fetch stages ([`FetchStage`]), so that a step may take the files it would fetch
//! from a stage that pins each by its SHA-256, rather than from the network.
//!
//! A fetch stage begins with the line `FROM scratch AS <name>`, below its comment, the lines of
//! `#` right above it, and ends at the first blank line after it, or at the recipe's end. Its
//! body is an `ADD` for each file, sorted by the file's path, which the build fetches by its
//! digest, readable by every user:
//!
//! ```text
//! ADD --link --chmod=0644 --checksum=sha256:<64 hexadecimal digits> https://<url> /<path>
//! ```
//!
//! One later step reads it, a `RUN` that bind-mounts it (`--mount=type=bind,from=<name>,…`), so
//! no layer of the image holds the files. Its comment holds its lock line, `# pumpkin lock:
//! <resolver> <argument>…`, from which `pumpkin lock <recipe> <name>` writes the body afresh,
//! with what the resolver prints, run online in the image that the recipe builds up to that
//! step ([`FetchStage::prefix`]). Its resolvers are a [`Resolver`]'s.
//!
//! A lock goes stale safely: a step that asks for a file the stage lacks fails, offline.

use alloc::borrow::ToOwned;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::{Line, Stage, at_line, bound_from, check_source, digest, lines, name};

/// What starts a fetch stage's lock line, after its `#`.
const LOCK: &str = "pumpkin lock:";

/// A recipe's fetch stage, read, as the [module](self) says.
#[derive(Debug)]
pub struct FetchStage {
    /// The recipe's lines, as `\n` splits them.
    lines: Vec<String>,
    /// The index of the stage's `FROM` line, and of the line after its body.
    from_index: usize,
    end_index: usize,
    resolver: Resolver,
    /// The recipe up to the step that reads the stage, without the stage's body.
    prefix: String,
}

/// What resolves a fetch stage's files, as its lock line names it.
#[derive(Debug)]
pub enum Resolver {
    /// `apt <package>…`: what `apt-get install --no-install-recommends <package>…` fetches, and
    /// each suite's `InRelease` and each index that apt reads, each at its path below the root of
    /// the one archive that the image's sources name, an `https://` URL, so that the stage is a
    /// mirror of the part of the archive that the install reads, whose `InRelease` apt checks
    /// against the keyring the sources name.
    Apt(Vec<String>),
}

impl FetchStage {
    /// The fetch stage `stage` of `text`, the recipe named `name`.
    ///
    /// # Errors
    ///
    /// Fails, naming `name` and the line, unless `text` is UTF-8 and has one fetch stage `stage`,
    /// as the [module](self) says: a line `FROM scratch AS <stage>`, a comment above it with one
    /// lock line, of a resolver and its arguments, a body of `ADD`s alone, and one later `RUN`
    /// that bind-mounts the stage.
    pub fn read(name: &str, text: &[u8], stage: &Stage) -> Result<Self, String> {
        let text = str::from_utf8(text)
            .map_err(|error| format!("{name}: {error}: write the recipe in UTF-8"))?;
        let lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
        let at = |(index, error): (Option<usize>, String)| match index {
            Some(index) => at_line(name, index, &error),
            None => format!("{name}: {error}"),
        };
        let from_index = from(&lines, stage).map_err(at)?;
        let resolver = lock_line(&lines, from_index, stage).map_err(at)?;
        let end_index = body_end(&lines, from_index, stage).map_err(at)?;
        let reader_index = reader(text, from_index, end_index, stage).map_err(at)?;
        // Without the stage's body, which the step's image doesn't read, so that its steps are
        // the same whatever the body pins, and a builder's cache holds them; its tag still
        // changes with the recipe, where the context holds it.
        let prefix = [&lines[..=from_index], &lines[end_index..reader_index]]
            .concat()
            .join("\n");
        Ok(Self {
            lines,
            from_index,
            end_index,
            resolver,
            prefix,
        })
    }

    /// The recipe up to the step that reads the stage, without the stage's body: the image a
    /// lock runs the resolver in.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    pub fn resolver(&self) -> &Resolver {
        &self.resolver
    }

    /// The recipe, with the stage's body written afresh from `pins`, the lines the resolver
    /// printed, `<sha256> <url> <path>` each.
    ///
    /// # Errors
    ///
    /// Fails if a line is no pin: a SHA-256, in hexadecimal digits; a plain `https://` URL
    /// ([`check_source`]); and a relative path; or if two pin one path to different files.
    pub fn written(&self, pins: &str) -> Result<String, String> {
        let mut written = self.lines[..=self.from_index].to_vec();
        written.extend(adds(pins)?);
        written.extend_from_slice(&self.lines[self.end_index..]);
        Ok(written.join("\n"))
    }
}

/// Why a fetch stage is refused, at the index of the line it names, if it names one.
type Refusal = (Option<usize>, String);

/// The index of the line `FROM scratch AS <stage>` of `lines`, a recipe's.
fn from(lines: &[String], stage: &Stage) -> Result<usize, Refusal> {
    let header = format!("FROM scratch AS {stage}");
    let mut froms = (0..lines.len()).filter(|&index| lines[index].trim_end() == header);
    let Some(from_index) = froms.next() else {
        return Err((
            None,
            format!(
                "no line is `{header}`: begin the fetch stage with it, below a comment that \
                 holds its lock line, `# {LOCK} <resolver> <argument>...`"
            ),
        ));
    };
    match froms.next() {
        Some(again) => Err((
            Some(again),
            format!("`{header}` begins a second stage `{stage}`: name each stage once"),
        )),
        None => Ok(from_index),
    }
}

/// The resolver that the lock line names in the comment right above the line at `from_index`
/// of `lines`, which begins the stage `stage`.
fn lock_line(lines: &[String], from_index: usize, stage: &Stage) -> Result<Resolver, Refusal> {
    let comment = (0..from_index)
        .rev()
        .take_while(|&index| lines[index].starts_with('#'));
    let mut locks = comment.filter_map(|index| {
        let line = lines[index].strip_prefix('#')?.trim_start();
        Some((index, line.strip_prefix(LOCK)?))
    });
    let Some((lock_index, lock)) = locks.next() else {
        return Err((
            Some(from_index),
            format!(
                "the stage `{stage}` has no lock line in the comment right above it: add \
                 `# {LOCK} <resolver> <argument>...`, as `# {LOCK} apt <package>...`"
            ),
        ));
    };
    if let Some((index, _)) = locks.next() {
        let error = format!("the stage `{stage}` has two lock lines: keep one");
        return Err((Some(index), error));
    }
    Resolver::read(lock).map_err(|error| (Some(lock_index), error))
}

/// The index of the line after the body of the stage `stage`, whose line at `from_index` of
/// `lines` begins it: its first blank line after, or the recipe's end.
fn body_end(lines: &[String], from_index: usize, stage: &Stage) -> Result<usize, Refusal> {
    let start = from_index + 1;
    let end = (start..lines.len())
        .find(|&index| lines[index].trim().is_empty())
        .unwrap_or(lines.len());
    match (start..end).find(|&index| !lines[index].starts_with("ADD ")) {
        Some(index) => Err((
            Some(index),
            format!(
                "the stage `{stage}` holds a line that is no `ADD` before the blank line that \
                 ends it: end the stage with a blank line, as `pumpkin lock` writes it"
            ),
        )),
        None => Ok(end),
    }
}

/// The index of the line that starts the one `RUN` of `text`, a recipe, after the line at
/// `end_index`, that bind-mounts the stage `stage`, which the line at `from_index` begins.
fn reader(
    text: &str,
    from_index: usize,
    end_index: usize,
    stage: &Stage,
) -> Result<usize, Refusal> {
    let readers: Vec<usize> = (lines(text).into_iter())
        .filter_map(|line| match line {
            Line::Instruction(first, joined) => Some((first, joined)),
            Line::Leading(..) => None,
        })
        .filter(|(index, joined)| {
            *index >= end_index
                && (bound_from(joined).iter()).any(|from| from.eq_ignore_ascii_case(stage.as_str()))
        })
        .map(|(index, _)| index)
        .collect();
    match readers[..] {
        [reader] => Ok(reader),
        [] => Err((
            Some(from_index),
            format!(
                "no later `RUN` bind-mounts the stage `{stage}`: mount it, as \
                 `RUN --mount=type=bind,from={stage},target=<dir>`, in the step that reads it"
            ),
        )),
        [_, second, ..] => Err((
            Some(second),
            format!(
                "this `RUN` bind-mounts the stage `{stage}` too, but its files are resolved for \
                 one step: give each step a fetch stage of its own"
            ),
        )),
    }
}

impl Resolver {
    /// The resolver that `lock`, a lock line after its [`LOCK`], names.
    fn read(lock: &str) -> Result<Self, String> {
        let mut words = lock.split_ascii_whitespace();
        match words.next() {
            Some("apt") => {
                let packages: Vec<String> = words.map(str::to_owned).collect();
                if packages.is_empty() {
                    return Err(
                        "the lock line names no package for apt: name each that the step installs"
                            .to_owned(),
                    );
                }
                if let Some(odd) = packages.iter().find(|package| {
                    package.starts_with('-')
                        || !package
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || "+-.:=~_/".contains(c))
                }) {
                    return Err(format!(
                        "`{}` is no package for apt: name each as `<name>`, `<name>=<version>` or \
                         `<name>/<release>`",
                        odd.escape_debug()
                    ));
                }
                Ok(Self::Apt(packages))
            }
            Some(other) => Err(format!(
                "`{}` is no resolver: name `apt`",
                other.escape_debug()
            )),
            None => Err("the lock line names no resolver: name `apt`".to_owned()),
        }
    }
}

/// The body of a fetch stage of `pins`, a resolver's lines, `<sha256> <url> <path>` each.
fn adds(pins: &str) -> Result<Vec<String>, String> {
    let mut by_path = BTreeMap::new();
    for line in pins.lines() {
        let (path, sha256, url) =
            pin(line).map_err(|error| format!("the pin `{line}`: {error}"))?;
        if let Some(earlier) = by_path.insert(path, (sha256, url))
            && earlier != (sha256, url)
        {
            return Err(format!(
                "`{path}` is pinned twice, to different files: pin each path once"
            ));
        }
    }
    if by_path.is_empty() {
        return Err("the resolver printed no pin: it should print a line for each file".to_owned());
    }
    let add = |(path, (sha256, url)): (&str, (&str, &str))| {
        format!("ADD --link --chmod=0644 --checksum=sha256:{sha256} {url} /{path}")
    };
    Ok(by_path.into_iter().map(add).collect())
}

/// The path, the SHA-256 and the URL of `line`, a pin, `<sha256> <url> <path>`: a SHA-256, in
/// lowercase hexadecimal digits; a plain `https://` URL ([`check_source`]); and a relative path
/// with no empty, `.` or `..` component; each as the recipe check takes a name.
fn pin(line: &str) -> Result<(&str, &str, &str), String> {
    let [sha256, url, path] = line.split(' ').collect::<Vec<_>>()[..] else {
        return Err("it is no pin: print `<sha256> <url> <path>`".to_owned());
    };
    if !digest(&format!("sha256:{sha256}")) {
        return Err(format!(
            "`{sha256}` isn't a SHA-256: print 64 lowercase hexadecimal digits"
        ));
    }
    name(url)?;
    check_source(url)?;
    if !url.starts_with("https://") {
        return Err(format!(
            "`{url}` isn't an `https://` URL: fetch each file by one"
        ));
    }
    name(path)?;
    if (path.split('/')).any(|component| matches!(component, "" | "." | "..")) {
        return Err(format!(
            "`{path}` isn't a relative path with no empty, `.` or `..` component: print one"
        ));
    }
    Ok((path, sha256, url))
}
