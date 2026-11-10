//! Image recipes, Dockerfiles, which a harness writes and a provider builds.
//!
//! An image's tag names what it is built from only where its recipe pins all that its build
//! fetches, so [`check`] refuses a recipe that may have Docker fetch:
//!
//! - an image not pinned by digest, as `<image>@sha256:<64 hexadecimal digits>`, where a `FROM`
//!   or a `COPY --from` names neither `scratch`, an earlier stage nor the tag of one of the
//!   build's bases, or where a `RUN --mount` could name one;
//! - a frontend, which a parser directive such as `# syntax=` can name: any comment before the
//!   first instruction that reads as a directive, `# <name>=…`;
//! - a URL, which `ADD` fetches for a source such as `<scheme>://…` or `<user>@<host>:…`: any
//!   `ADD` source with a `:`, but the one `https://` source of an `ADD` whose
//!   `--checksum=sha256:<64 hexadecimal digits>` pins it;
//! - a Git repository, which some versions of Docker clone with no checksum checked: any `ADD`
//!   source that [`check_source`] refuses.
//!
//! It reads a recipe as Docker frames it, with `\` as the escape character: comments, continued
//! lines, each instruction's keyword, flags and words. What it can't read as Docker would, it
//! refuses rather than guess: a heredoc, which `<<` in a `RUN`, `COPY` or `ADD` may begin;
//! `ONBUILD`; a keyword that is no instruction; a `FROM` other than `FROM <image>` or
//! `FROM <image> AS <name>`; and, in a flag or a name, a character other than printable ASCII,
//! a quote or `\`, or, in a name, `$`. What a `RUN` step's own command fetches, and the
//! `ONBUILD` triggers a base may hold, are outside it.

// Without `std`, the check can name no file, process or clock: it reads only the text it is given.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// The characters that separate an instruction's keyword and words: ASCII whitespace, as
/// Docker splits them, but the line feed, which no instruction holds once its lines are joined.
const BLANKS: [char; 5] = [' ', '\t', '\u{b}', '\u{c}', '\r'];

/// The instructions that fetch nothing, of which the check reads only the keyword.
const INERT: [&str; 13] = [
    "ARG",
    "CMD",
    "ENTRYPOINT",
    "ENV",
    "EXPOSE",
    "HEALTHCHECK",
    "LABEL",
    "MAINTAINER",
    "SHELL",
    "STOPSIGNAL",
    "USER",
    "VOLUME",
    "WORKDIR",
];

/// Checks `text`, the recipe named `name`, of a build on images tagged `bases`, as the
/// [crate] doc says.
///
/// # Errors
///
/// Fails, naming `name`, if `text` isn't UTF-8, or on the first thing the check refuses, with
/// the line its instruction starts on.
pub fn check(name: &str, text: &[u8], bases: &[&str]) -> Result<(), String> {
    let text = str::from_utf8(text)
        .map_err(|error| format!("{name}: {error}: write the recipe in UTF-8"))?;
    let mut named = Named {
        stages: Vec::new(),
        bases,
    };
    read(text, &mut named).map_err(|(line, error)| format!("{name}:{line}: {error}"))
}

/// Fails unless `source`, an `ADD` source, is plain: it holds no `%`, `?` or `#`, and doesn't end
/// in `.git`, in any case, so no Docker from 25.0 on takes it for a Git repository's URL.
///
/// # Errors
///
/// Fails, saying why, if `source` isn't plain.
pub fn check_source(source: &str) -> Result<(), String> {
    let git = (source.rsplit_once('.')).is_some_and(|(_, end)| end.eq_ignore_ascii_case("git"));
    if git {
        return Err(format!(
            "Docker may take `{}` for a Git repository's URL, which some of its versions clone \
             without checking a checksum: name no `ADD` source that ends in `.git`, in any case",
            source.escape_debug()
        ));
    }
    // A `%`, `?` or `#` can hide such an ending from this check, which decodes no URL.
    if let Some(c) = source.chars().find(|c| matches!(c, '%' | '?' | '#')) {
        return Err(format!(
            "`{}` holds `{c}`, which can hide a `.git` ending: name a plain `ADD` source, with \
             no escape, query or fragment, so no `%`, `?` or `#`",
            source.escape_debug()
        ));
    }
    Ok(())
}

/// What an instruction may name for an image that no digest pins: the names of the stages
/// before it, lowercased, or `None` for one with no name, to which a `FROM` adds its stage; and
/// the build's bases, by their tags.
struct Named<'b> {
    stages: Vec<Option<String>>,
    bases: &'b [&'b str],
}

impl Named<'_> {
    /// Whether `name` is the name of an earlier stage, as its stage gives it, lowercased.
    fn stage(&self, name: &str) -> bool {
        self.stages.iter().flatten().any(|stage| stage == name)
    }
}

/// Checks `text`, a recipe, an instruction at a time, with its continued lines joined as
/// Docker joins them; fails with the number of the line an instruction starts on, and why the
/// check refuses it.
fn read(text: &str, named: &mut Named<'_>) -> Result<(), (usize, String)> {
    let mut started = false;
    // The instruction being continued: its first line's number, and its text so far.
    let mut open: Option<(usize, String)> = None;
    for (number, line) in (1..).zip(text.split('\n')) {
        let line = line.trim_end_matches('\r');
        let trimmed = line.trim_start();
        // Docker drops a comment or a blank line even in a continued instruction, which goes on.
        if trimmed.is_empty() || trimmed.starts_with('#') {
            if !started && directive(trimmed) {
                return Err((
                    number,
                    format!(
                        "`{}` reads as a parser directive, which can name a frontend for the \
                         build to fetch, or change how Docker reads the lines after it: drop it, \
                         or reword the comment",
                        shown(trimmed)
                    ),
                ));
            }
            continue;
        }
        started = true;
        // An instruction's first line starts at its keyword; its continued lines join as
        // written, with no separator.
        let (first, part, mut joined) = match open.take() {
            Some((first, joined)) => (first, line, joined),
            None => (number, trimmed, String::new()),
        };
        let body = continued(part);
        joined.push_str(body.unwrap_or(part));
        if body.is_some() {
            open = Some((first, joined));
        } else {
            instruction(&joined, named).map_err(|error| (first, error))?;
        }
    }
    if let Some((first, joined)) = open {
        instruction(&joined, named).map_err(|error| (first, error))?;
    }
    Ok(())
}

/// Whether `comment`, a line from its `#` on, reads as a parser directive, `# <name>=…`.
fn directive(comment: &str) -> bool {
    let Some(rest) = comment.strip_prefix('#').map(str::trim_start) else {
        return false;
    };
    let after = rest.trim_start_matches(|c: char| c.is_ascii_alphanumeric());
    rest.starts_with(|c: char| c.is_ascii_alphabetic()) && after.trim_start().starts_with('=')
}

/// `line` without the `\` that continues it, and the spaces and tabs after that, or `None` if
/// it isn't continued: a `\` after another escapes it, as `\\`.
fn continued(line: &str) -> Option<&str> {
    let body = line.trim_end_matches([' ', '\t']).strip_suffix('\\')?;
    (!body.ends_with('\\')).then_some(body)
}

/// Checks `text`, an instruction with its lines joined, given what it may name.
fn instruction(text: &str, named: &mut Named<'_>) -> Result<(), String> {
    let text = text.trim();
    let (keyword, rest) = text.split_once(BLANKS).unwrap_or((text, ""));
    match keyword.to_ascii_uppercase().as_str() {
        "FROM" => {
            let (_, words) = flags(rest)?;
            let stage = from(words, named)?;
            named.stages.push(stage);
        }
        "RUN" | "COPY" | "ADD" if text.contains("<<") => {
            return Err(format!(
                "this `{keyword}` holds `<<`, which may begin a heredoc, and the check reads \
                 none: put the file in the build's context and copy it from there"
            ));
        }
        "RUN" => {
            let (flags, _) = flags(rest)?;
            // Docker names a flag by its text up to its first `=`.
            let mount =
                |flag: &str| flag.split_once('=').map_or(flag, |(name, _)| name) == "--mount";
            if flags.into_iter().any(mount) {
                return Err(
                    "`RUN --mount` can mount an image no digest pins, or what the \
                     host keeps: copy what the step needs with `COPY` instead"
                        .to_owned(),
                );
            }
        }
        "COPY" => {
            let (flags, _) = flags(rest)?;
            for image in flags.iter().filter_map(|flag| flag.strip_prefix("--from=")) {
                // Docker takes a number for a stage's index, unless it is too large for Docker's
                // integers, and then for an image's name; and a name for a stage's in any case.
                let stage = match image.parse::<usize>() {
                    Ok(index) => index < named.stages.len(),
                    Err(_) => named.stage(&image.to_ascii_lowercase()),
                };
                pinned(image, stage, named)?;
            }
        }
        "ADD" => {
            let (flags, words) = flags(rest)?;
            add(&flags, words)?;
        }
        "ONBUILD" => {
            return Err(
                "`ONBUILD` runs its instruction in a later build, which the check \
                 doesn't see: drop it"
                    .to_owned(),
            );
        }
        known if INERT.contains(&known) => {}
        _ => {
            return Err(format!(
                "`{}` is no Dockerfile instruction: start each instruction with one, such as \
                 `FROM`, `RUN` or `COPY`",
                shown(keyword)
            ));
        }
    }
    Ok(())
}

/// The flags at the start of `rest`, an instruction's text after its keyword, and its text
/// after them, trimmed, as Docker splits them: words that start with `--`, up to a word `--`.
fn flags(rest: &str) -> Result<(Vec<&str>, &str), String> {
    let mut flags = Vec::new();
    let mut rest = rest;
    loop {
        // Docker looks for flags byte by byte, so a no-break space, which is no ASCII
        // whitespace, starts the text after them, even if a `--` follows it.
        rest = rest.trim_start_matches(BLANKS);
        if !rest.starts_with("--") {
            break;
        }
        let (flag, after) = rest.split_at(rest.find(BLANKS).unwrap_or(rest.len()));
        plain(flag)?;
        rest = after;
        if flag == "--" {
            break;
        }
        flags.push(flag);
    }
    Ok((flags, rest.trim()))
}

/// Checks `words`, a `FROM`'s text after its flags, and returns the name of the stage it
/// begins, lowercased, if it names one.
fn from(words: &str, named: &Named<'_>) -> Result<Option<String>, String> {
    let (image, stage) = match split(words)[..] {
        [image] => (image, None),
        [image, word, stage] if word.eq_ignore_ascii_case("as") => {
            (image, Some(stage.to_ascii_lowercase()))
        }
        _ => {
            return Err(format!(
                "`{}` isn't an image, then optionally `AS` and a stage's name: write \
                 `FROM <image>` or `FROM <image> AS <name>`",
                shown(words)
            ));
        }
    };
    // Docker takes a stage's name exactly as the stage gives it, lowercased.
    pinned(image, named.stage(image), named)?;
    Ok(stage)
}

/// Refuses each source in `words`, an `ADD`'s text after `flags`, that [`check_source`]
/// refuses, or that Docker may fetch as a URL, one with a `:`, unless `flags` pin it, as the
/// [crate] doc says, and each checksum in `flags` that isn't a SHA-256 digest.
fn add(flags: &[&str], words: &str) -> Result<(), String> {
    let words = split(words);
    let sources = words.split_last().map_or(&[][..], |(_, sources)| sources);
    for source in sources {
        name(source)?;
        check_source(source)?;
    }
    let checksums: Vec<&str> = (flags.iter())
        .filter_map(|flag| flag.strip_prefix("--checksum="))
        .collect();
    for checksum in &checksums {
        if !digest(checksum) {
            return Err(format!(
                "`{checksum}` isn't a SHA-256 digest: write \
                 `--checksum=sha256:<64 hexadecimal digits>`"
            ));
        }
    }
    if let ([_], [source]) = (&checksums[..], sources)
        && source.starts_with("https://")
    {
        return Ok(());
    }
    if let Some(source) = sources.iter().find(|source| source.contains(':')) {
        return Err(format!(
            "`{source}` may be a URL, for `ADD` to fetch, that no checksum pins: copy files \
             from the build's context with `COPY`, or fetch one `https://` URL with \
             `ADD --checksum=sha256:<64 hexadecimal digits>`"
        ));
    }
    Ok(())
}

/// The words of `text`, as Docker splits them.
fn split(text: &str) -> Vec<&str> {
    text.split(BLANKS).filter(|word| !word.is_empty()).collect()
}

/// Refuses `image`, which a `FROM` or a `COPY --from` names, unless it is a [`name`], and a
/// `stage` before it, `scratch`, the tag of one of `named`'s bases, or pinned by digest.
fn pinned(image: &str, stage: bool, named: &Named<'_>) -> Result<(), String> {
    name(image)?;
    let by_digest =
        (image.rsplit_once('@')).is_some_and(|(name, hex)| !name.is_empty() && digest(hex));
    if stage || image == "scratch" || named.bases.contains(&image) || by_digest {
        return Ok(());
    }
    Err(format!(
        "`{image}` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, \
         `scratch`, an earlier stage or a base the build is given"
    ))
}

/// Whether `text` is a SHA-256 digest as Docker writes one: `sha256:` and 64 lowercase
/// hexadecimal digits.
fn digest(text: &str) -> bool {
    (text.strip_prefix("sha256:")).is_some_and(|hex| {
        hex.len() == 64 && (hex.bytes()).all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// Refuses `word`, an image or a source, unless Docker takes it as written: [`plain`], with no
/// `$`, which Docker may expand.
fn name(word: &str) -> Result<(), String> {
    plain(word)?;
    if word.contains('$') {
        return Err(format!(
            "`{word}` names a variable, which Docker expands and the check doesn't: write its \
             value instead"
        ));
    }
    Ok(())
}

/// Refuses `word`, a flag or a name, unless Docker splits it and takes it as written: printable
/// ASCII, with no quote or `\`.
fn plain(word: &str) -> Result<(), String> {
    let odd = |c: &char| !c.is_ascii_graphic() || matches!(c, '\'' | '"' | '\\');
    match word.chars().find(odd) {
        Some(c) => Err(format!(
            "`{}` holds `{}`, which Docker may split, unquote or unescape: write it in \
             printable ASCII, with no quote or `\\`",
            shown(word),
            shown(c.encode_utf8(&mut [0; 4]))
        )),
        None => Ok(()),
    }
}

/// `text` for a message, with each character but printable ASCII and the space escaped, as
/// `\u{a0}`.
fn shown(text: &str) -> String {
    let show = |c: char| match c {
        ' ' | '!'..='~' => c.to_string(),
        _ => c.escape_unicode().to_string(),
    };
    text.chars().map(show).collect()
}
