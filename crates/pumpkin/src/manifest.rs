//! A task's tree as the client sends it to the controller: by content, to a store there of files
//! named by their SHA-256, which keeps each for every later send, whatever task, repository or
//! commit names it.
//!
//! The client hashes each file into a [`Manifest`], whose own SHA-256 names the tree, and asks
//! what the store lacks of it ([`query`]); the controller answers with each blob it lacks
//! ([`missing`]), and the pieces of it that it holds whole. The client deals the rest of the
//! pieces ([`pieces`]) to several streams at once ([`deal`]), each a `tar` stream ([`stream`])
//! that the controller extracts into the store's [`INCOMING`] ([`receive`]); the job's start then
//! takes each blob whose pieces have all arrived into the store, once its SHA-256 is its name
//! ([`admit`]). This follows the content-addressable storage of Bazel's remote execution, and
//! the find-missing call it makes before every upload, `FindMissingBlobs`:
//! <https://github.com/bazelbuild/remote-apis/blob/c2c07e3d718d3d490a4958344d9c3e47de0bfaf3/build/bazel/remote/execution/v2/remote_execution.proto#L350-L362>,
//! and Modal's check of a mount's hashes, then the put of each that it lacks:
//! <https://github.com/modal-labs/modal-client/blob/dbb24aba92e2c6c4fa8a123234813cfa48e09b0b/py/modal/mount.py#L502-L536>.
//! It turns down `BuildKit`'s kept context, a stream of each file's metadata against a copy kept
//! by where it was sent, which resends a task's files once another task's were sent in between:
//! <https://github.com/moby/buildkit/blob/1d510a3097d9fbd85a42d162d42c3a79eda1f795/vendor/github.com/tonistiigi/fsutil/receive.go#L1-L22>.
//!
//! The store holds each blob as `<sha256>`, 0444; the same bytes as `<sha256>.x`, 0555, for a
//! file that its owner may execute, as a link shares its file's mode; each manifest as
//! [`MANIFESTS`]`/<sha256>`; and the pieces that have arrived, unchecked, as
//! [`INCOMING`]`/<sha256>.<index>`. The job makes its tree of links into the store, and collects
//! what no tree or recent manifest names.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::iter;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use schema::ir::Sha256Digest;
use schema::sandbox::RelativePath;
use sha2::{Digest as _, Sha256};
use stdx::Context as _;

use crate::job::STORE;

// The first line of a manifest, which the scripts check too; any change to what a line holds
// changes it. The job runs the client's build to make its tree, but the prune reads every
// manifest in the store, and keeps every blob while one is of a form it can't read.
const FORM: &str = "pumpkin-tree/1";

/// The most bytes of a blob that one piece carries, and so of what a dropped connection loses.
// About ten seconds of one session's work: small enough that a dropped connection loses little,
// large enough that a gigabyte of datasets is tens of pieces, not thousands.
pub(crate) const PIECE_BYTES: u64 = 16 << 20;

// Enough digits for the index of each piece of a blob of `BLOB_BYTES`.
const INDEX_DIGITS: usize = 5;

/// The most bytes of a file that a send carries: 100,000 pieces, which `INDEX_DIGITS` number.
const BLOB_BYTES: u64 = 100_000 * PIECE_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Staged as 0644.
    File,
    /// A file that its owner may execute, staged as 0755, as the builder reads that bit.
    Executable,
    /// A symbolic link, whose blob is its target's text, as Git stores a link.
    Link,
}

impl Kind {
    const fn letter(self) -> char {
        match self {
            Self::File => 'f',
            Self::Executable => 'x',
            Self::Link => 'l',
        }
    }

    fn from_letter(letter: &str) -> Option<Self> {
        match letter {
            "f" => Some(Self::File),
            "x" => Some(Self::Executable),
            "l" => Some(Self::Link),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) kind: Kind,
    pub(crate) digest: Sha256Digest,
    pub(crate) size_bytes: u64,
    pub(crate) path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// The files of a tree, sorted by the bytes of their paths, so that equal trees have one text.
pub(crate) struct Manifest {
    entries: Vec<Entry>,
}

impl Manifest {
    fn new(mut entries: Vec<Entry>) -> Self {
        entries.sort_by(|one, other| one.path.as_bytes().cmp(other.path.as_bytes()));
        Self { entries }
    }

    pub(crate) fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The text that is sent, and that [`Manifest::name`] hashes.
    pub(crate) fn text(&self) -> Vec<u8> {
        let lines = self.entries.iter().map(|entry| {
            let Entry {
                kind,
                digest,
                size_bytes,
                path,
            } = entry;
            format!("{} {} {size_bytes} {path}\n", kind.letter(), digest.hex())
        });
        let text: String = iter::once(format!("{FORM}\n")).chain(lines).collect();
        text.into_bytes()
    }

    /// The tree's name, by which the store keeps its manifest.
    pub(crate) fn name(&self) -> Sha256Digest {
        sha256(&self.text())
    }

    pub(crate) fn parse(text: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(text)
            .map_err(|error| format!("the manifest isn't UTF-8 ({error}): expected text"))?;
        let mut lines = text.split_terminator('\n');
        if lines.next() != Some(FORM) {
            return Err(format!(
                "the manifest doesn't start with `{FORM}`: expected a manifest of this Pumpkin's \
                 form"
            ));
        }
        let entries = lines.map(entry).collect::<Result<_, _>>()?;
        Ok(Self { entries })
    }
}

fn entry(line: &str) -> Result<Entry, String> {
    let wrong = || {
        format!(
            "the manifest's line `{}` isn't `<kind> <sha256> <size> <path>`: expected `f`, `x` \
             or `l`, 64 lowercase hexadecimal digits, a number of bytes and a path",
            line.escape_debug()
        )
    };
    let mut fields = line.splitn(4, ' ');
    let (Some(kind), Some(hex), Some(size), Some(path)) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return Err(wrong());
    };
    let kind = Kind::from_letter(kind).ok_or_else(wrong)?;
    let digest = digest(hex).ok_or_else(wrong)?;
    let size_bytes = size.parse().map_err(|_| wrong())?;
    sendable(path).map_err(|error| {
        format!(
            "the manifest's line `{}` is refused: {error}",
            line.escape_debug()
        )
    })?;
    Ok(Entry {
        kind,
        digest,
        size_bytes,
        path: path.to_owned(),
    })
}

/// The digest that `hex`, 64 lowercase hexadecimal digits, spells.
pub(crate) fn digest(hex: &str) -> Option<Sha256Digest> {
    Sha256Digest::try_from(format!("sha256:{hex}")).ok()
}

pub(crate) fn sha256(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest::from(<[u8; 32]>::from(Sha256::digest(bytes)))
}

fn sendable(path: &str) -> Result<(), String> {
    if path.contains(['\n', '\t']) || RelativePath::try_from(path.to_owned()).is_err() {
        return Err(format!(
            "`{}` can't be sent to the controller, whose tree names each file by a line: rename \
             it with no newline or tab, and no empty, `.` or `..` component",
            path.escape_debug()
        ));
    }
    // A job's tree has `.pumpkin` as a link to the controller's one workspace, which a file of the
    // tree's would be written through.
    if path.split('/').next() == Some(crate::workspace::DIR) {
        return Err(format!(
            "`{}` can't be sent to the controller, where a job's `{}` is the controller's own \
             workspace: leave it out of the task",
            path.escape_debug(),
            crate::workspace::DIR
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Where a blob's bytes are.
pub(crate) enum Source {
    /// A file of the tree.
    File(PathBuf),
    /// A symbolic link's target, its text.
    Text(Vec<u8>),
}

/// A manifest, and where the bytes of each blob that it names are.
pub(crate) struct Hashed {
    pub(crate) manifest: Manifest,
    pub(crate) sources: BTreeMap<Sha256Digest, Source>,
}

impl Hashed {
    /// How a message names the blob with `digest`.
    pub(crate) fn shown(&self, digest: &Sha256Digest) -> String {
        (self.sources.get(digest)).map_or_else(|| format!("the blob {}", digest.hex()), shown)
    }
}

/// The manifest of a tree, and where its blobs are: each file below `top` whose path is in
/// `files`, which lists NUL-ended paths as `git ls-files -z` does.
///
/// A path listed twice is taken once; one that is gone, or is a directory, as a submodule's is, is
/// left out. It fails on a path that a manifest
/// can't hold, a file that is neither a regular file nor a symbolic link, or one over
/// `BLOB_BYTES`.
pub(crate) fn hash(top: &Path, files: &[u8]) -> Result<Hashed, String> {
    let mut entries = Vec::new();
    let mut sources = BTreeMap::new();
    // A path listed twice, as git lists one in a merge's conflict once for each of its stages.
    let mut seen = BTreeSet::new();
    for listed in files
        .split(|byte| *byte == 0)
        .filter(|file| !file.is_empty() && seen.insert(*file))
    {
        let Ok(path) = std::str::from_utf8(listed) else {
            return Err(format!(
                "`{}` isn't UTF-8, so it can't be sent to the controller, whose tree names each \
                 file by a line of text: rename it",
                String::from_utf8_lossy(listed).escape_debug()
            ));
        };
        sendable(path)?;
        let file = top.join(std::ffi::OsStr::from_bytes(listed));
        let metadata = match fs::symlink_metadata(&file) {
            Ok(metadata) => metadata,
            // Listed by git, but removed since.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context(file.display()),
        };
        let kind = metadata.file_type();
        let (kind, source) = if kind.is_symlink() {
            let target = fs::read_link(&file).context(file.display())?;
            (
                Kind::Link,
                Source::Text(target.as_os_str().as_bytes().to_vec()),
            )
        } else if kind.is_file() {
            let executable = metadata.permissions().mode() & 0o100 != 0;
            let kind = if executable {
                Kind::Executable
            } else {
                Kind::File
            };
            (kind, Source::File(file))
        } else if kind.is_dir() {
            continue;
        } else {
            return Err(format!(
                "{} is neither a file nor a symbolic link, so it can't be sent to the controller: \
                 remove it, or ignore it in git",
                file.display()
            ));
        };
        let (digest, size_bytes) = hashed(&source)?;
        entries.push(Entry {
            kind,
            digest: digest.clone(),
            size_bytes,
            path: path.to_owned(),
        });
        sources.insert(digest, source);
    }
    Ok(Hashed {
        manifest: Manifest::new(entries),
        sources,
    })
}

fn hashed(source: &Source) -> Result<(Sha256Digest, u64), String> {
    let (digest, size_bytes) = match source {
        Source::Text(text) => (sha256(text), text.len() as u64),
        Source::File(path) => file_sha256(path)?,
    };
    if size_bytes > BLOB_BYTES {
        return Err(format!(
            "{} holds {size_bytes} bytes, more than the {BLOB_BYTES} a send carries of one file: \
             leave it out of the task",
            shown(source)
        ));
    }
    Ok((digest, size_bytes))
}

/// The SHA-256 of the file at `path`, and its length in bytes.
pub(crate) fn file_sha256(path: &Path) -> Result<(Sha256Digest, u64), String> {
    let mut hasher = Hasher(Sha256::new());
    let file = File::open(path).context(path.display());
    let copied = io::copy(&mut file?, &mut hasher).context(path.display())?;
    let Hasher(sha256) = hasher;
    let digest = Sha256Digest::from(<[u8; 32]>::from(sha256.finalize()));
    Ok((digest, copied))
}

fn shown(source: &Source) -> String {
    match source {
        Source::File(path) => path.display().to_string(),
        Source::Text(text) => format!("the link to `{}`", String::from_utf8_lossy(text)),
    }
}

struct Hasher(Sha256);

impl Write for Hasher {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) const MANIFESTS: &str = "manifests";

pub(crate) const INCOMING: &str = "incoming";

/// The exit status of [`admit`]'s script when it refuses a blob, after printing why for
/// [`unadmitted`].
pub(crate) const UNADMITTED: i32 = 102;

/// The store's lock, which a command holds while it lists the store, or changes what it holds.
pub(crate) const LOCK: &str = "lock";

/// A shell function that reads the manifest at `$1`, refuses one that isn't of this form, and
/// prints `missing <sha256> <size> <indices>` for each blob that the store lacks, with the
/// indices of its pieces in [`INCOMING`] that arrived whole. An executable's blob is held if its
/// copy to run is.
// A piece is whole if its size is what its blob's size gives it, so a stream cut off part way
// through its last piece leaves that piece to be sent again. The listing and the manifest are
// two inputs, so no name a listing holds is read as a manifest's line, and each digest is checked
// before a file is named by it.
fn lacking() -> String {
    format!(
        r#"lacking() {{
  {{ find . -maxdepth 1 -type f -printf 'have %f\n'
    find {INCOMING} -maxdepth 1 -type f -name '*.{dots}' -printf 'piece %f %s\n'; }} |
  awk -v piece={PIECE_BYTES} -v form={FORM} '
    !manifest && $1 == "have" {{ have[$2] = 1; next }}
    !manifest && $1 == "piece" {{ whole[$2] = $3; next }}
    !manifest {{ next }}
    FNR == 1 {{
      if ($0 != form) {{ print FILENAME " does not start with " form > "/dev/stderr"; exit 1 }}
      next }}
    !(($1 == "f" || $1 == "x" || $1 == "l" || $1 == "b") && length($2) == 64 &&
      $2 !~ /[^0-9a-f]/ && $3 ~ /^[0-9]+$/ && NF >= 4) {{
      print FILENAME ": line " FNR " is not <kind> <sha256> <size> <path>" > "/dev/stderr"
      exit 1 }}
    ($2 in have) || ($1 == "x" && (($2 ".x") in have)) || ($2 in told) {{ next }}
    {{
      told[$2] = 1; parts = ""
      for (i = 0; i * piece < $3 || i == 0; i++) {{
        want = ($3 - i * piece < piece) ? $3 - i * piece : piece
        name = sprintf("%s.%0{INDEX_DIGITS}d", $2, i)
        if ((name in whole) && whole[name] == want) parts = parts (parts == "" ? "" : ",") i
      }}
      print "missing", $2, $3, parts }}' - manifest=1 "$1"
}}
"#,
        dots = "?".repeat(INDEX_DIGITS),
    )
}

/// A script that takes the manifest of `tree` on its standard input, stores it, and prints what
/// the store lacks of the tree, one line for each blob and `end`, as [`missing`] reads it.
///
/// It fails if what it reads does not have the SHA-256 `tree`.
// One call for the whole manifest, as REAPI's FindMissingBlobs takes many digests a call:
// https://github.com/bazelbuild/bazel/blob/a765dc177f48efb4c9e1b980609b838c9ba27f42/src/main/java/com/google/devtools/build/lib/remote/GrpcCacheClient.java#L182-L198
// Under the store's lock, which the prune holds while it collects, so that the prune, which spares
// what a manifest of the last day names, either ran before the listing or finds the manifest.
pub(crate) fn query(tree: &Sha256Digest) -> String {
    let tree = tree.hex();
    format!(
        r#"set -o errexit -o nounset
store="$HOME/{STORE}"
mkdir --parents "$store/{INCOMING}" "$store/{MANIFESTS}"
cd "$store"
held=$(mktemp --tmpdir={INCOMING} manifest.XXXXXX)
cat > "$held"
if ! printf '%s  %s\n' {tree} "$held" | sha256sum --check --status; then
  rm "$held"
  echo "the manifest that arrived isn't tree {tree}'s: expected its SHA-256 to be its name" >&2
  exit 1
fi
chmod 0444 "$held"
{lacking}exec 9> {LOCK}
flock 9
lacking "$held" || {{ rm "$held"; exit 1; }}
mv --force "$held" {MANIFESTS}/{tree}
echo end
"#,
        lacking = lacking(),
    )
}

/// A script that extracts the tar archive on its standard input into [`INCOMING`].
// `--touch`, as the stream's headers carry no time, and the prune collects a piece by its age.
pub(crate) fn receive() -> String {
    format!(
        "set -o errexit -o nounset\ncd \"$HOME/{STORE}/{INCOMING}\"\n\
         exec tar --extract --touch --no-same-owner --no-overwrite-dir --file=-\n"
    )
}

/// A script that, for each blob of the stored manifest of `tree` that the store lacks, joins its
/// pieces, checks its digest, and moves it into the store.
///
/// It ends with [`UNADMITTED`], after printing `changed <digest>` for a blob that doesn't match
/// its digest and `gone <digest>` for one whose pieces are not all there, which
/// [`unadmitted`] words.
// A blob is checked as `incoming/<sha256>`, a name that no receive writes: its one piece renamed
// to it, or its pieces joined into it. GNU tar replaces a piece by removing it and writing a new
// file under its name, so a piece checked under its own name could, by its move into the store,
// be another receive's, part written. Under the store's lock, so that two starts that take in one
// blob don't write it at once, nor the prune collect meanwhile; a start with nothing to take in
// takes no lock, so it never waits for another's.
pub(crate) fn admit(tree: &Sha256Digest) -> String {
    let tree = tree.hex();
    format!(
        r#"cd "$HOME/{STORE}"
[ -r "{MANIFESTS}/{tree}" ] || {{ echo "the tree {tree} is missing: send it again" >&2; exit 1; }}
{lacking}whole() {{
  lacking {MANIFESTS}/{tree} | awk -v piece={PIECE_BYTES} '
    $1 == "missing" {{
      count = int(($3 + piece - 1) / piece); if (count == 0) count = 1
      if (split($4, held, ",") < count) {{ print "gone", $2; next }}
      names = ""
      for (i = 0; i < count; i++) names = names " " sprintf("%s.%0{INDEX_DIGITS}d", $2, i)
      print "whole", $2 names }}'
}}
[ -n "$(whole)" ] || exit 0
exec 9> {LOCK}
flock 9
refused=$(mktemp --tmpdir={INCOMING} refused.XXXXXX)
checks=$(mktemp --tmpdir={INCOMING} checks.XXXXXX)
whole | while read -r state sha names; do
  if [ "$state" = gone ]; then echo "gone $sha" >> "$refused"; continue; fi
  set -- $names
  if [ $# -gt 1 ]; then
    (cd {INCOMING} && cat -- "$@" > "$sha")
  else
    mv -- "{INCOMING}/$1" "{INCOMING}/$sha"
  fi
  echo "$sha  {INCOMING}/$sha"
done > "$checks"
sha256sum --check "$checks" 2>/dev/null | while IFS= read -r line; do
  sha=${{line%: *}}; sha=${{sha#{INCOMING}/}}
  case $line in
    *": OK") chmod 0444 "{INCOMING}/$sha"; mv -- "{INCOMING}/$sha" "$sha" ;;
    *) echo "changed $sha" >> "$refused" ;;
  esac
  rm --force -- "{INCOMING}/$sha" "{INCOMING}/$sha".{dots}
done
rm "$checks"
if [ -s "$refused" ]; then cat "$refused"; rm "$refused"; exit {UNADMITTED}; fi
rm "$refused"
"#,
        lacking = lacking(),
        dots = "?".repeat(INDEX_DIGITS),
    )
}

/// What to tell the operator when the start refused a blob.
pub(crate) fn unadmitted(
    hashed: &Hashed,
    changed: &[Sha256Digest],
    gone: &[Sha256Digest],
) -> String {
    if let Some(first) = changed.first() {
        let others = match changed.len() - 1 {
            0 => " changed while it was sent".to_owned(),
            others => format!(
                ", and {}, changed while they were sent",
                crate::counted(others, "other file")
            ),
        };
        return format!("{}{others}: run it again", hashed.shown(first));
    }
    let gone: BTreeSet<_> = gone.iter().collect();
    let files = hashed
        .manifest
        .entries
        .iter()
        .filter(|entry| gone.contains(&entry.digest))
        .count();
    let is = if files == 1 { "is" } else { "are" };
    format!("{files} of the task's files {is} no longer whole on the controller: run it again")
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// A blob that the store lacks, with the indices of its pieces that it holds.
pub(crate) struct Missing {
    pub(crate) digest: Sha256Digest,
    pub(crate) size_bytes: u64,
    pub(crate) held: BTreeSet<u64>,
}

/// Reads what [`query`]'s script printed. It fails if the answer is cut off, before its `end`, or
/// is anything else.
pub(crate) fn missing(answer: &[u8]) -> Result<Vec<Missing>, String> {
    let answer = String::from_utf8_lossy(answer);
    let mut lines = answer.lines();
    let mut missing = Vec::new();
    loop {
        let Some(line) = lines.next() else {
            return Err(
                "the controller's answer of what it lacks has no `end`, so it was cut off: send \
                 the task again"
                    .to_owned(),
            );
        };
        if line == "end" {
            break;
        }
        missing.push(lacked(line).ok_or_else(|| {
            format!(
                "the controller answered `{}` of what it lacks: expected `missing <sha256> \
                 <size> <indices>` or `end`",
                line.escape_debug()
            )
        })?);
    }
    if let Some(line) = lines.next() {
        return Err(format!(
            "the controller answered `{}` after the `end` of what it lacks: expected nothing \
             more",
            line.escape_debug()
        ));
    }
    Ok(missing)
}

fn lacked(line: &str) -> Option<Missing> {
    let mut fields = line.split(' ');
    let (Some("missing"), Some(hex), Some(size), held, None) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next().unwrap_or(""),
        fields.next(),
    ) else {
        return None;
    };
    let size_bytes = size.parse().ok()?;
    let held = held.split_terminator(',').map(str::parse);
    Some(Missing {
        digest: digest(hex)?,
        size_bytes,
        held: held.collect::<Result<_, _>>().ok()?,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Bytes `index * PIECE_BYTES` and on, `size_bytes` of them, of the blob with `digest`.
pub(crate) struct Piece {
    pub(crate) digest: Sha256Digest,
    pub(crate) index: u64,
    pub(crate) size_bytes: u64,
}

impl Piece {
    /// The name that the piece has in [`INCOMING`].
    pub(crate) fn name(&self) -> String {
        format!("{}.{:0INDEX_DIGITS$}", self.digest.hex(), self.index)
    }

    fn offset_bytes(&self) -> u64 {
        self.index * PIECE_BYTES
    }
}

/// The pieces of each blob in `missing` that the store doesn't hold. A blob of no bytes has one.
pub(crate) fn pieces(missing: &[Missing]) -> Vec<Piece> {
    let mut pieces = Vec::new();
    for Missing {
        digest,
        size_bytes,
        held,
    } in missing
    {
        let count = size_bytes.div_ceil(PIECE_BYTES).max(1);
        for index in (0..count).filter(|index| !held.contains(index)) {
            let size_bytes = (size_bytes - index * PIECE_BYTES).min(PIECE_BYTES);
            pieces.push(Piece {
                digest: digest.clone(),
                index,
                size_bytes,
            });
        }
    }
    pieces
}

/// The most streams that carry a send's pieces at once, each a [`receive`] over a connection of
/// its own.
// A session's stream is bounded on its own by `ssh`'s channel window, 2 MiB a round trip, so
// streams at once add up over a link of long round trips. The workspace's connections are as many
// masters as `controller` asserts this is within, and a stream over a master that another stream
// holds shares its connection, so more streams would gain nothing.
// https://github.com/openssh/openssh-portable/blob/5dba72adacbd39736960543373737bc01e35f778/channels.h#L223-L225
pub(crate) const STREAMS: usize = 16;

/// Deals `pieces` out to streams of about the same bytes: one for each `PIECE_BYTES` of them,
/// but at most `STREAMS`, and none for no pieces.
pub(crate) fn deal(mut pieces: Vec<Piece>) -> Vec<Vec<Piece>> {
    let size_bytes: u64 = pieces.iter().map(|piece| piece.size_bytes).sum();
    // The master of each stream past the first opens in about a second, while the streams before
    // it send, and a piece takes about ten: so a piece earns its own.
    let wanted = usize::try_from(size_bytes.div_ceil(PIECE_BYTES)).unwrap_or(STREAMS);
    let count = wanted.clamp(1, STREAMS).min(pieces.len());
    pieces.sort_by(|one, other| {
        let by_size = other.size_bytes.cmp(&one.size_bytes);
        by_size.then_with(|| (&one.digest, one.index).cmp(&(&other.digest, other.index)))
    });
    let mut streams: Vec<(u64, Vec<Piece>)> = vec![(0, Vec::new()); count];
    for piece in pieces {
        let Some((dealt_bytes, dealt)) = streams.iter_mut().min_by_key(|(bytes, _)| *bytes) else {
            unreachable!("there is a stream for each piece's deal, as there are pieces")
        };
        *dealt_bytes += piece.size_bytes;
        dealt.push(piece);
    }
    streams.into_iter().map(|(_, dealt)| dealt).collect()
}

/// Writes `pieces` to `out` as a tar archive, each a file named by [`Piece::name`], and calls
/// `sent` with the bytes of each chunk as it is written.
///
/// The inner error says that a piece's source changed, or can't be read, while it was sent, and
/// the archive ends after the last whole piece if the source was found so before its piece began;
/// the outer error is of writing `out`.
pub(crate) fn stream(
    pieces: &[Piece],
    sources: &BTreeMap<Sha256Digest, Source>,
    out: &mut impl Write,
    sent: &mut impl FnMut(u64),
) -> io::Result<Result<(), String>> {
    let end = |out: &mut dyn Write| out.write_all(&[0; 2 * BLOCK]);
    for piece in pieces {
        let Some(source) = sources.get(&piece.digest) else {
            unreachable!("a piece's blob is one the manifest names, which `hash` read")
        };
        let changed = || format!("{} changed while it was sent: run it again", shown(source));
        let blob: Box<dyn ReadSeek> = match source {
            Source::Text(text) => Box::new(io::Cursor::new(text.clone())),
            Source::File(path) => {
                let opened = File::open(path).and_then(|file| Ok((file.metadata()?.len(), file)));
                match opened {
                    Ok((size_bytes, _)) if size_bytes < piece.offset_bytes() + piece.size_bytes => {
                        end(out)?;
                        return Ok(Err(changed()));
                    }
                    Ok((_, file)) => Box::new(file),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        end(out)?;
                        return Ok(Err(changed()));
                    }
                    Err(error) => {
                        end(out)?;
                        return Ok(Err(format!("{}: {error}", shown(source))));
                    }
                }
            }
        };
        out.write_all(&header(&piece.name(), 0o444, piece.size_bytes))?;
        match part(blob, piece, out, sent)? {
            Ok(copied) if copied == piece.size_bytes => {}
            // Shortened since it was opened: the member is cut off, and so the stream.
            Ok(_) => return Ok(Err(changed())),
            Err(error) => return Ok(Err(format!("{}: {error}", shown(source)))),
        }
        out.write_all(&[0; BLOCK][..padding(piece.size_bytes)])?;
    }
    end(out)?;
    Ok(Ok(()))
}

trait ReadSeek: Read + Seek {}

impl<T: Read + Seek> ReadSeek for T {}

fn part(
    mut blob: impl Read + Seek,
    piece: &Piece,
    out: &mut impl Write,
    sent: &mut impl FnMut(u64),
) -> io::Result<io::Result<u64>> {
    if let Err(error) = blob.seek(SeekFrom::Start(piece.offset_bytes())) {
        return Ok(Err(error));
    }
    let mut blob = blob.take(piece.size_bytes);
    // All that a stream holds of a blob, whatever its size.
    let mut buffer = vec![0; 1 << 16];
    let mut copied = 0;
    loop {
        let read = match blob.read(&mut buffer) {
            Ok(0) => return Ok(Ok(copied)),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Ok(Err(error)),
        };
        out.write_all(&buffer[..read])?;
        sent(read as u64);
        copied += read as u64;
    }
}

const BLOCK: usize = 512;

fn padding(size_bytes: u64) -> usize {
    let size = usize::try_from(size_bytes % BLOCK as u64).unwrap_or(0);
    (BLOCK - size) % BLOCK
}

/// A ustar archive of `files`, each a name and what the file holds, each mode 0600: made in memory,
/// so that nothing it holds is written on this machine but in the files that hold it.
///
/// # Panics
///
/// If a name is over 100 bytes, which a ustar header holds at most.
pub(crate) fn archive(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut archive = Vec::new();
    for (name, holds) in files {
        archive.extend_from_slice(&header(name, 0o600, holds.len() as u64));
        archive.extend_from_slice(holds);
        archive.resize(archive.len().next_multiple_of(BLOCK), 0);
    }
    // Two blocks of zeros end it.
    archive.resize(archive.len() + 2 * BLOCK, 0);
    archive
}

/// The ustar header of a file `name` of `mode` and `size_bytes`, owned by user and group 0, last
/// changed at time 0.
///
/// # Panics
///
/// If `name` is over 100 bytes, which a ustar header holds at most.
fn header(name: &str, mode: u32, size_bytes: u64) -> [u8; BLOCK] {
    assert!(name.len() <= 100, "a ustar name holds 100 bytes at most");
    let mut header = [0; BLOCK];
    header[..name.len()].copy_from_slice(name.as_bytes());
    // Its mode, owner, group, size and time, each a field of octal digits that a NUL ends.
    let numbers = [
        (100, format!("{mode:07o}")),
        (108, format!("{:07o}", 0)),
        (116, format!("{:07o}", 0)),
        (124, format!("{size_bytes:011o}")),
        (136, format!("{:011o}", 0)),
    ];
    for (at, number) in numbers {
        header[at..at + number.len()].copy_from_slice(number.as_bytes());
    }
    // A regular file, of the ustar format, whose checksum is the sum of the header's bytes, its
    // own counted as spaces.
    header[156] = b'0';
    header[257..265].copy_from_slice(b"ustar\x0000");
    header[148..156].fill(b' ');
    let sum: u32 = header.iter().copied().map(u32::from).sum();
    header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    header
}
