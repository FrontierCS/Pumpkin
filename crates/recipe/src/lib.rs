//! Image recipes, Dockerfiles, and the check of what their builds fetch.
//!
//! An image's tag names what it is built from only where its recipe pins all that its build
//! fetches, so [`check`] refuses a recipe that may have Docker fetch:
//!
//! - an image no digest pins, `<image>@sha256:<64 hexadecimal digits>`, where a `FROM`, a
//!   `COPY --from` or a `RUN --mount`'s `from` names no `scratch`, earlier stage or named context
//!   the build is [`Given`]; a `FROM` may name a build argument, `$<name>` or `${<name>}`, that an
//!   `ARG` before the first `FROM` declares and the build is given, read as its value;
//! - what the host keeps, which a `RUN --mount` but a `tmpfs` or a bind mount may mount;
//! - a frontend, which a parser directive (`# <name>=…` before the first instruction) or a
//!   `BUILDKIT_` build argument can name;
//! - a URL, any `ADD` source with a `:`, but the one `https://` source that `--checksum` pins;
//! - a Git repository: any `ADD` source that [`check_source`] refuses, but the one source
//!   `https://<host>/<path>.git#<commit>`, whose full commit pins it;
//! - the host's network, a device or a privilege, which a `RUN` flag but `--mount` may give.
//!
//! It refuses a target that names no stage, a build argument or named context misnamed, and a
//! stage named as a named context. It reads a recipe as Docker frames it, with `\` as the escape
//! character, and refuses what it can't read as Docker would: a heredoc; `ONBUILD`; a keyword
//! that is no instruction; a `FROM` other than `FROM <image>` or `FROM <image> AS <name>`; and, in
//! a flag, a name or an `ADD`'s destination, a character other than printable ASCII, a quote or
//! `\`, or, in a name, `$`, but for a `FROM`'s build argument. What a `RUN` step's own command
//! fetches, which a stage may pin by checksum instead (an `ADD --checksum` that a `RUN --mount`
//! or a `COPY --from` reads), and a base's `ONBUILD` triggers are outside it.

// Without `std`, the check can name no file, process or clock: it reads only the text it is given.
#![no_std]

extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

// The characters that separate an instruction's keyword and words: ASCII whitespace, as Docker
// splits them, but the line feed, which no instruction holds once its lines are joined.
const BLANKS: [char; 5] = [' ', '\t', '\u{b}', '\u{c}', '\r'];

// The instructions that fetch nothing, of which the check reads only the keyword.
const INERT: [&str; 12] = [
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

/// Checks `text`, the recipe named `name`, of a build given `given`, as the [crate] doc says.
///
/// Fails, naming `name`, on a `text` that isn't UTF-8, or at the first thing the check refuses,
/// with the line its instruction starts on if it is in `text`.
pub fn check<B>(name: &str, text: &[u8], given: &Given<B>) -> Result<(), String> {
    let at = |error: String| format!("{name}: {error}");
    let text =
        str::from_utf8(text).map_err(|error| at(format!("{error}: write the recipe in UTF-8")))?;
    let Given {
        target,
        args,
        contexts,
    } = given;
    (args.keys().try_for_each(|name| arg(name))).map_err(at)?;
    (contexts.keys().try_for_each(|name| context(name))).map_err(at)?;
    let mut named = Named {
        stages: Vec::new(),
        declared: Vec::new(),
        args,
        contexts: contexts.keys().map(String::as_str).collect(),
    };
    read(text, &mut named).map_err(|(index, error)| at_line(name, index, &error))?;
    let Some(target) = target else {
        return Ok(());
    };
    if named.stage(target.as_str()) {
        return Ok(());
    }
    Err(at(format!(
        "the build's target, `{target}`, names no stage of the recipe: name one that a \
         `FROM <image> AS <name>` begins"
    )))
}

/// Fails unless `source`, an `ADD` source, is plain: it holds no `%`, `?` or `#`, and doesn't end
/// in `.git`, in any case, so no Docker from 25.0 on takes it for a Git repository's URL.
pub fn check_source(source: &str) -> Result<(), String> {
    if git_like(source) {
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

fn git_like(source: &str) -> bool {
    (source.rsplit_once('.')).is_some_and(|(_, end)| end.eq_ignore_ascii_case("git"))
}

/// What a build gives its recipe beside its context, all of it in the build's inputs.
///
/// `B` is a named context's image, such as what a provider built; [`check`] reads only the names.
#[derive(Debug)]
pub struct Given<B> {
    /// The stage to build, as `docker build --target` names it; the recipe's last without one.
    pub target: Option<Stage>,
    /// Each build argument's value, as `docker build --build-arg` gives it, by its name: ASCII
    /// letters, digits and `_`, not starting with a digit, and not `SOURCE_DATE_EPOCH`, the time
    /// that every build gives its image.
    pub args: BTreeMap<String, String>,
    /// Each named context's image, as `docker build --build-context` gives it, by the name the
    /// recipe gives it in a `FROM`, a `COPY --from` or a `RUN --mount`'s `from`: words of lowercase
    /// ASCII letters and digits, joined each by one `-`, `_` or `.`, starting with a letter, but
    /// `scratch`.
    pub contexts: BTreeMap<String, B>,
}

impl<B> Default for Given<B> {
    fn default() -> Self {
        Self {
            target: None,
            args: BTreeMap::new(),
            contexts: BTreeMap::new(),
        }
    }
}

/// The name of a stage of a recipe, as a build's target names it.
///
/// It is a-z, 0-9, `-`, `_` and `.`, starting with a letter, as Docker's Dockerfile frontend takes
/// a stage's name once it has lowercased it, and it matches a target in any case
/// ([`Stage::in_any_case`]).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Stage(String);

impl Stage {
    /// The stage that `name` names in any case, lowercased, refusing a `name` that isn't a stage's
    /// name once lowercased.
    pub fn in_any_case(name: &str) -> Result<Self, String> {
        Self::try_from(name.to_ascii_lowercase())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Stage {
    type Error = String;

    fn try_from(name: String) -> Result<Self, String> {
        let mut chars = name.chars();
        let valid = chars.next().is_some_and(|first| first.is_ascii_lowercase())
            && chars.all(|char| {
                char.is_ascii_lowercase()
                    || char.is_ascii_digit()
                    || matches!(char, '-' | '_' | '.')
            });
        if valid {
            Ok(Self(name))
        } else {
            Err(format!(
                "`{}` is no stage's name: use a-z, 0-9, `-`, `_` and `.`, starting with a letter",
                name.escape_debug()
            ))
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn arg(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let first = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if !first || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!(
            "`{}` is no build argument's name: name it with ASCII letters, digits and `_`, not \
             starting with a digit",
            name.escape_debug()
        ));
    }
    // BuildKit reads these for itself, before any recipe, as its section "BuildKit built-in build
    // args" says:
    // https://github.com/moby/buildkit/blob/1d510a3097d9fbd85a42d162d42c3a79eda1f795/frontend/dockerfile/docs/reference.md#L2870
    if name.starts_with("BUILDKIT_") {
        return Err(format!(
            "`{name}` starts with `BUILDKIT_`, a prefix BuildKit reads for itself, as \
             `BUILDKIT_SYNTAX` names a frontend to fetch: give no build argument with it"
        ));
    }
    if name == "SOURCE_DATE_EPOCH" {
        return Err(
            "`SOURCE_DATE_EPOCH` is the time BuildKit gives an image, which every build fixes: \
             give no build argument by this name"
                .to_owned(),
        );
    }
    Ok(())
}

fn context(name: &str) -> Result<(), String> {
    let word = |word: &str| {
        !word.is_empty()
            && (word.bytes()).all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    };
    let named =
        name.starts_with(|c: char| c.is_ascii_lowercase()) && name.split(['-', '_', '.']).all(word);
    if !named {
        return Err(format!(
            "`{}` is no named context's name: name it with words of lowercase ASCII letters and \
             digits, joined each by one `-`, `_` or `.`, starting with a letter",
            name.escape_debug()
        ));
    }
    if name == "scratch" {
        return Err(
            "`scratch` is the empty image's name, which no named context may take: name it \
             otherwise"
                .to_owned(),
        );
    }
    Ok(())
}

struct Named<'a> {
    stages: Vec<Option<String>>,
    declared: Vec<String>,
    args: &'a BTreeMap<String, String>,
    contexts: Vec<&'a str>,
}

impl Named<'_> {
    fn stage(&self, name: &str) -> bool {
        self.stages.iter().flatten().any(|stage| stage == name)
    }
}

fn read(text: &str, named: &mut Named<'_>) -> Result<(), (usize, String)> {
    for line in lines(text) {
        match line {
            Line::Leading(index, comment) if directive(comment) => {
                return Err((
                    index,
                    format!(
                        "`{}` reads as a parser directive, which can name a frontend for the \
                         build to fetch, or change how Docker reads the lines after it: drop it, \
                         or reword the comment",
                        shown(comment)
                    ),
                ));
            }
            Line::Leading(..) => {}
            Line::Instruction(first, joined) => {
                instruction(&joined, named).map_err(|error| (first, error))?;
            }
        }
    }
    Ok(())
}

pub(crate) fn at_line(name: &str, index: usize, error: &str) -> String {
    format!("{name}:{}: {error}", index + 1)
}

pub(crate) enum Line<'t> {
    Leading(usize, &'t str),
    Instruction(usize, String),
}

pub(crate) fn lines(text: &str) -> Vec<Line<'_>> {
    let mut read = Vec::new();
    let mut started = false;
    let mut open: Option<(usize, String)> = None;
    for (index, line) in text.split('\n').enumerate() {
        let line = line.trim_end_matches('\r');
        let trimmed = line.trim_start();
        // Docker drops a comment or a blank line even in a continued instruction, which goes on.
        if trimmed.is_empty() || trimmed.starts_with('#') {
            if !started {
                read.push(Line::Leading(index, trimmed));
            }
            continue;
        }
        started = true;
        // An instruction's first line starts at its keyword; its continued lines join as written,
        // with no separator.
        let (first, part, mut joined) = match open.take() {
            Some((first, joined)) => (first, line, joined),
            None => (index, trimmed, String::new()),
        };
        let body = continued(part);
        joined.push_str(body.unwrap_or(part));
        if body.is_some() {
            open = Some((first, joined));
        } else {
            read.push(Line::Instruction(first, joined));
        }
    }
    read.extend(open.map(|(first, joined)| Line::Instruction(first, joined)));
    read
}

fn directive(comment: &str) -> bool {
    let Some(rest) = comment.strip_prefix('#').map(str::trim_start) else {
        return false;
    };
    let after = rest.trim_start_matches(|c: char| c.is_ascii_alphanumeric());
    rest.starts_with(|c: char| c.is_ascii_alphabetic()) && after.trim_start().starts_with('=')
}

fn continued(line: &str) -> Option<&str> {
    let body = line.trim_end_matches([' ', '\t']).strip_suffix('\\')?;
    (!body.ends_with('\\')).then_some(body)
}

fn instruction(text: &str, named: &mut Named<'_>) -> Result<(), String> {
    let text = text.trim();
    let (keyword, rest) = text.split_once(BLANKS).unwrap_or((text, ""));
    match keyword.to_ascii_uppercase().as_str() {
        "FROM" => {
            let (_, words) = flags(rest)?;
            let stage = from(words, named)?;
            named.stages.push(stage);
        }
        // Docker expands a `FROM`'s build arguments with those of the `ARG`s before the first
        // `FROM` alone. A word that isn't `<name>` or `<name>=…` declares no name a `FROM` can
        // expand here.
        "ARG" => {
            if named.stages.is_empty() {
                let words = split(rest).into_iter();
                let declared = words.map(|word| word.split_once('=').map_or(word, |(arg, _)| arg));
                named.declared.extend(declared.map(str::to_owned));
            }
        }
        "RUN" | "COPY" | "ADD" if text.contains("<<") => {
            return Err(format!(
                "this `{keyword}` holds `<<`, which may begin a heredoc, and the check reads \
                 none: put the file in the build's context and copy it from there"
            ));
        }
        "RUN" => {
            let (flags, _) = flags(rest)?;
            for flag in flags {
                // Docker names a flag by its text up to its first `=`.
                let (name, value) = flag.split_once('=').unwrap_or((flag, ""));
                run_flag(name)?;
                mount(value, named)?;
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

fn run_flag(name: &str) -> Result<(), String> {
    match name {
        "--mount" => Ok(()),
        "--network" => Err(
            "`RUN --network` sets its step's network, which every build leaves as the engine \
             gives it, and which `--network=host` makes the host's: drop the flag"
                .to_owned(),
        ),
        "--device" => Err(
            "`RUN --device` gives its step a device of the host's, which no step of a build has: \
             drop the flag"
                .to_owned(),
        ),
        "--security" => Err(
            "`RUN --security` sets its step's privileges, which no step of a build has beyond a \
             container's: drop the flag"
                .to_owned(),
        ),
        _ => Err(format!(
            "`RUN {}` is no flag the check passes: give a `RUN` no flag but `--mount`",
            shown(name)
        )),
    }
}

fn mount(value: &str, named: &Named<'_>) -> Result<(), String> {
    name(value)?;
    let (kind, from) = fields(value);
    match kind.as_str() {
        "bind" => {}
        "tmpfs" => return Ok(()),
        "cache" | "secret" | "ssh" => {
            return Err(format!(
                "`RUN --mount=type={kind}` mounts what the host keeps, which the build's inputs \
                 don't name: mount the build's context, an earlier stage, a named context or an \
                 image pinned by digest with `type=bind`, or copy what the step needs with `COPY`"
            ));
        }
        "" => {
            return Err(
                "a `RUN --mount`'s `type` is empty: give it `bind` or `tmpfs`, the types of \
                 mount that the check passes"
                    .to_owned(),
            );
        }
        _ => {
            return Err(format!(
                "`{}` is no type of mount that the check passes: mount with `type=bind` or \
                 `type=tmpfs`",
                shown(&kind)
            ));
        }
    }
    if from.is_empty() {
        return Ok(());
    }
    if from.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "a `RUN --mount`'s `from={from}` names the image `docker.io/library/{from}`, as \
             Docker reads a number there, not a stage's index: name the stage by its `AS` name"
        ));
    }
    // Docker takes a name there for a stage's in any case.
    pinned(from, named.stage(&from.to_ascii_lowercase()), named)
}

fn fields(value: &str) -> (String, &str) {
    // Docker reads the value as fields split by commas, each `<key>=<value>` or a bare key, its key
    // in any case, and a later field of a key sets it again, each value expanded, which `name`
    // keeps from happening, as BuildKit's `parseMount` does it:
    // https://github.com/moby/buildkit/blob/1d510a3097d9fbd85a42d162d42c3a79eda1f795/frontend/dockerfile/instructions/commands_runmount.go#L133-L310
    let (mut kind, mut from) = ("bind".to_owned(), "");
    for field in value.split(',') {
        let (key, value) = field.split_once('=').unwrap_or((field, ""));
        match key.to_ascii_lowercase().as_str() {
            "type" => kind = value.to_ascii_lowercase(),
            "from" => from = value,
            _ => {}
        }
    }
    (kind, from)
}

fn flags(rest: &str) -> Result<(Vec<&str>, &str), String> {
    let mut flags = Vec::new();
    let mut rest = rest;
    loop {
        // Docker looks for flags byte by byte, so a no-break space, which is no ASCII whitespace,
        // starts the text after them, even if a `--` follows it.
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
    // Docker would build the named context in the stage's place, with what follows the `FROM` on
    // it.
    if let Some(stage) = &stage
        && named.contexts.contains(&stage.as_str())
    {
        return Err(format!(
            "the stage `{stage}` is named as a named context the build is given, which Docker \
             would build in its place: name the stage otherwise"
        ));
    }
    let Some(value) = expanded(image, named)? else {
        // Docker takes a stage's name exactly as the stage gives it, lowercased.
        pinned(image, named.stage(image), named)?;
        return Ok(stage);
    };
    (pinned(value, named.stage(value), named))
        .map_err(|error| format!("`{image}` expands to the build argument's value: {error}"))?;
    Ok(stage)
}

// The value of the build argument that `image`, a `FROM`'s, names, if it names one, as `$<name>` or
// `${<name>}`, the whole word, which Docker expands to its value. It refuses a name that no `ARG`
// before the first `FROM` declares, so that Docker would expand it to another value, and one that
// the build isn't given.
fn expanded<'n>(image: &str, named: &'n Named<'_>) -> Result<Option<&'n str>, String> {
    let braced = image
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'));
    let Some(variable) = braced.or_else(|| image.strip_prefix('$')) else {
        return Ok(None);
    };
    // Another shape is refused as a name that Docker may expand.
    if arg(variable).is_err() {
        return Ok(None);
    }
    if !named.declared.iter().any(|declared| declared == variable) {
        return Err(format!(
            "`{image}` names `{variable}`, which no `ARG` before the first `FROM` declares, so \
             Docker would expand it to nothing or to a value of its own: declare it there with \
             `ARG {variable}`, and give the build its value"
        ));
    }
    match named.args.get(variable) {
        Some(value) => Ok(Some(value)),
        None => Err(format!(
            "`{image}` names the build argument `{variable}`, which the build isn't given: give \
             it an image pinned by digest"
        )),
    }
}

fn add(flags: &[&str], words: &str) -> Result<(), String> {
    let words = split(words);
    let sources = match words.split_last() {
        Some((destination, sources)) => {
            // An `ADD` in JSON form with no blank in it is one word here, and Docker's to split.
            plain(destination)?;
            sources
        }
        None => &[][..],
    };
    if let [source] = sources
        && let Some((url, fragment)) = source.split_once('#')
        && url.contains("://")
        && git_like(url)
    {
        return git(flags, url, fragment);
    }
    if let Some(source) = (sources.iter()).find(|source| source.contains("://") && git_like(source))
    {
        return Err(format!(
            "`{source}` names a Git repository but no commit, so Docker would clone whatever \
             its default branch holds: pin a commit, as `https://<host>/<path>.git#<commit>`, \
             the one source of its `ADD`"
        ));
    }
    let checksums: Vec<&str> = (flags.iter())
        .filter_map(|flag| flag.strip_prefix("--checksum="))
        .collect();
    for source in sources {
        name(source)?;
        check_source(source)?;
    }
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

#[expect(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "Docker clones a URL whose path ends in `.git` in lowercase alone"
)]
// Refuses `<url>#<fragment>`, the one source of an `ADD` with `flags`, a URL whose path ends in
// `.git`, in any case, that Docker may clone, unless it is `https://<host>/<path>.git#<commit>`: a
// commit in full, 40 lowercase hexadecimal digits, which pins the tree by SHA-1, where a prefix, a
// branch or a tag may move; in a URL with no `%` or `?`, which can hide what it names; with no
// `--checksum`, which adds nothing to the commit, and no `--keep-git-dir`, whose `.git` each clone
// writes afresh.
fn git(flags: &[&str], url: &str, fragment: &str) -> Result<(), String> {
    name(url)?;
    let Some(rest) = url.strip_prefix("https://") else {
        return Err(format!(
            "`{url}` isn't an `https://` URL: name the Git repository by its `https://` URL"
        ));
    };
    if let Some(c) = url.chars().find(|c| matches!(c, '%' | '?')) {
        return Err(format!(
            "`{url}` holds `{c}`, which can hide what the URL names: name the Git repository by \
             a plain `https://` URL, with no escape or query"
        ));
    }
    // Docker takes a URL for a Git repository's where its path ends in `.git`, in lowercase.
    let path = rest.split_once('/').map_or("", |(_, path)| path);
    if !path.ends_with(".git") {
        return Err(format!(
            "`{url}` has no path that ends in `.git`, in lowercase, which Docker clones: name the \
             Git repository as `https://<host>/<path>.git`"
        ));
    }
    let hex = |byte: u8| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte);
    if fragment.len() != 40 || !fragment.bytes().all(|byte| hex(byte.to_ascii_lowercase())) {
        return Err(format!(
            "`#{fragment}` may name a branch, a tag or a prefix of a commit, any of which can \
             move: name the commit in full, 40 hexadecimal digits"
        ));
    }
    // BuildKit takes only lowercase digits for a commit, as its `IsCommitSHA` does, and others for
    // a ref's name:
    // https://github.com/moby/buildkit/blob/1d510a3097d9fbd85a42d162d42c3a79eda1f795/util/gitutil/git_commit.go#L3-L19
    if !fragment.bytes().all(hex) {
        return Err(format!(
            "`#{fragment}` is in uppercase, which Docker takes for a branch or a tag's name: \
             write the commit in lowercase"
        ));
    }
    for flag in flags {
        let (name, _) = flag.split_once('=').unwrap_or((flag, ""));
        let why = match name {
            "--checksum" => "adds nothing to the commit, which pins the tree",
            "--keep-git-dir" => "keeps the clone's `.git`, which each build writes afresh",
            _ => continue,
        };
        return Err(format!("`{name}` {why}: drop it"));
    }
    Ok(())
}

fn split(text: &str) -> Vec<&str> {
    text.split(BLANKS).filter(|word| !word.is_empty()).collect()
}

fn pinned(image: &str, stage: bool, named: &Named<'_>) -> Result<(), String> {
    name(image)?;
    let by_digest =
        (image.rsplit_once('@')).is_some_and(|(name, hex)| !name.is_empty() && digest(hex));
    if stage || image == "scratch" || named.contexts.contains(&image) || by_digest {
        return Ok(());
    }
    Err(format!(
        "`{image}` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, \
         `scratch`, an earlier stage or a named context the build is given"
    ))
}

fn digest(text: &str) -> bool {
    (text.strip_prefix("sha256:")).is_some_and(|hex| {
        hex.len() == 64 && (hex.bytes()).all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    })
}

pub(crate) fn name(word: &str) -> Result<(), String> {
    plain(word)?;
    if word.contains('$') {
        return Err(format!(
            "`{word}` names a variable, which Docker expands and the check doesn't: write its \
             value instead"
        ));
    }
    Ok(())
}

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

fn shown(text: &str) -> String {
    let show = |c: char| match c {
        ' ' | '!'..='~' => c.to_string(),
        _ => c.escape_unicode().to_string(),
    };
    text.chars().map(show).collect()
}
