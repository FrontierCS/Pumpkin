//! A task's tree as the client sends it to the controller: by content, to a store there of files
//! named by their SHA-256, which keeps each for every later send, whatever task, repository or
//! commit names it.
//!
//! The client hashes each file it would send into a [`Manifest`], whose own SHA-256 names the
//! tree, and sends it in one call ([`query`]); the controller keeps it, and answers with each
//! blob its store lacks ([`missing`]), with the pieces of it that it already holds whole; the
//! client sends the rest of those pieces ([`pieces`]) as members of a `tar` stream ([`stream`]),
//! which the controller extracts into the store's [`INCOMING`] ([`receive`]); the job's start
//! then takes each blob whose pieces have all arrived into the store, once its SHA-256 is its name
//! ([`admit`]). This is REAPI's content-addressable storage and its `FindMissingBlobs`, the
//! find-missing call before every upload that Bazel's remote execution makes:
//! <https://github.com/bazelbuild/remote-apis/blob/c2c07e3d718d3d490a4958344d9c3e47de0bfaf3/build/bazel/remote/execution/v2/remote_execution.proto#L350-L362>,
//! and Modal's check of a mount's every hash at once, then the put of each that it lacks:
//! <https://github.com/modal-labs/modal-client/blob/dbb24aba92e2c6c4fa8a123234813cfa48e09b0b/py/modal/mount.py#L502-L536>.
//! It turns down `BuildKit`'s kept context, a stream of each file's metadata against a copy kept
//! by where it was sent, which resends a task's files once another task's were sent in between:
//! <https://github.com/moby/buildkit/blob/1d510a3097d9fbd85a42d162d42c3a79eda1f795/vendor/github.com/tonistiigi/fsutil/receive.go#L1-L22>.
//!
//! The store, below the controller user's home ([`STORE`]), holds each blob as `<sha256>`, 0444;
//! the same bytes as `<sha256>.x`, 0555, for a file that its owner may execute, as a link shares
//! its file's mode; each manifest as [`MANIFESTS`]`/<sha256>`; and the pieces that have arrived,
//! unchecked, as [`INCOMING`]`/<sha256>.<index>`. What this leaves to the job: making its tree of
//! links into the store, and collecting what no tree links.

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

/// The first line of a manifest, which names its form: its number goes up with any change to
/// what a line holds.
// The client writes a manifest and the job of the Pumpkin it pins makes its tree, so those two
// are of one commit; the prune of any job reads every manifest in the store, and keeps every
// blob while one is of a form it can't read.
const FORM: &str = "pumpkin-tree/1";

/// The most bytes of a blob that one piece carries, and so of what a dropped connection loses.
// About ten seconds of one session's work: small enough that a dropped connection loses little,
// large enough that a GB of datasets is tens of pieces, not thousands.
pub(crate) const PIECE_BYTES: u64 = 16 << 20;

/// The digits of a piece's index in its name, `<sha256>.<index>`.
const INDEX_DIGITS: usize = 5;

/// The most bytes a blob may hold: as many pieces as [`INDEX_DIGITS`] can number.
const BLOB_BYTES: u64 = 100_000 * PIECE_BYTES;

/// What an [`Entry`] is, by its letter in a manifest's line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `f`, a file, staged as 0644.
    File,
    /// `x`, a file its owner may execute, staged as 0755, as the builder reads that bit.
    Executable,
    /// `l`, a symbolic link, whose blob is its target's text, as Git stores a link.
    Link,
    /// `b`, the git bundle of the Pumpkin that the tree pins, whose path is that commit.
    Bundle,
}

impl Kind {
    const fn letter(self) -> char {
        match self {
            Self::File => 'f',
            Self::Executable => 'x',
            Self::Link => 'l',
            Self::Bundle => 'b',
        }
    }

    fn from_letter(letter: &str) -> Option<Self> {
        match letter {
            "f" => Some(Self::File),
            "x" => Some(Self::Executable),
            "l" => Some(Self::Link),
            "b" => Some(Self::Bundle),
            _ => None,
        }
    }
}

/// A line of a [`Manifest`]: a file of the tree, or the bundle, by its content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) kind: Kind,
    pub(crate) digest: Sha256Digest,
    pub(crate) size_bytes: u64,
    /// Below the tree's top, `/`-separated, with no empty, `.` or `..` component and no newline
    /// or tab; the commit, of the bundle.
    pub(crate) path: String,
}

/// A tree, as a manifest names it: text, its form's line, then a line for each entry,
/// `<kind> <sha256> <size> <path>`, sorted by path; directories are implied by the paths.
#[derive(Clone, Debug, PartialEq, Eq)]
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

    /// The manifest's bytes, which [`parse`](Self::parse) reads back as this manifest.
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

    /// The tree's name: the SHA-256 of [`text`](Self::text), as REAPI's input root digest and a
    /// Git tree's id each name a whole tree, under which the controller keeps the manifest.
    pub(crate) fn name(&self) -> Sha256Digest {
        sha256(&self.text())
    }

    /// The manifest whose bytes are `text`.
    ///
    /// # Errors
    ///
    /// Fails, saying the first line it can't read, if `text` isn't a manifest of this form.
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

/// The [`Entry`] that `line` of a manifest is.
fn entry(line: &str) -> Result<Entry, String> {
    let wrong = || {
        format!(
            "the manifest's line `{}` isn't `<kind> <sha256> <size> <path>`: expected `f`, `x`, \
             `l` or `b`, 64 lowercase hexadecimal digits, a number of bytes and a path",
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
    match kind {
        Kind::Bundle if commit(path) => {}
        Kind::Bundle => return Err(wrong()),
        Kind::File | Kind::Executable | Kind::Link => sendable(path).map_err(|error| {
            format!(
                "the manifest's line `{}` is refused: {error}",
                line.escape_debug()
            )
        })?,
    }
    Ok(Entry {
        kind,
        digest,
        size_bytes,
        path: path.to_owned(),
    })
}

/// The digest whose 64 lowercase hexadecimal digits are `hex`, if they are.
pub(crate) fn digest(hex: &str) -> Option<Sha256Digest> {
    Sha256Digest::try_from(format!("sha256:{hex}")).ok()
}

fn sha256(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest::from(<[u8; 32]>::from(Sha256::digest(bytes)))
}

/// Whether `path`, a bundle's, is a commit of Pumpkin: 40 lowercase hexadecimal digits.
fn commit(path: &str) -> bool {
    path.len() == 40
        && path
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Checks that `path` can be a manifest's path: a [`RelativePath`] with no newline or tab, as a
/// line of the manifest, and of the shell that reads it, carries it, and not below `.pumpkin`.
// A job's tree has `.pumpkin` as a link to the controller's one workspace, which a file of the
// tree's would be written through.
fn sendable(path: &str) -> Result<(), String> {
    if path.contains(['\n', '\t']) || RelativePath::try_from(path.to_owned()).is_err() {
        return Err(format!(
            "`{}` can't be sent to the controller, whose tree names each file by a line: rename \
             it with no newline or tab, and no empty, `.` or `..` component",
            path.escape_debug()
        ));
    }
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

/// Where the bytes of a blob are on this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// A file, the tree's or the bundle.
    File(PathBuf),
    /// A symbolic link's target, its text.
    Text(Vec<u8>),
}

/// A tree as [`hash`] read it: its manifest, and where the bytes of each of its blobs are.
pub(crate) struct Hashed {
    pub(crate) manifest: Manifest,
    pub(crate) sources: BTreeMap<Sha256Digest, Source>,
}

impl Hashed {
    /// Where this machine holds the blob `digest`, which the manifest names, for a person to read.
    pub(crate) fn shown(&self, digest: &Sha256Digest) -> String {
        (self.sources.get(digest)).map_or_else(|| format!("the blob {}", digest.hex()), shown)
    }
}

/// Hashes `files`, paths below `top`, each ended by a NUL, as `git ls-files -z` lists them, and
/// the bundle at `bundle` of Pumpkin's commit `commit`, into a manifest: a link as a link, a
/// file by whether its owner may execute it, a path listed twice once; a path that is gone, or a
/// directory, as a submodule's is, is left out.
///
/// # Errors
///
/// Fails, naming the path, if one can't be a manifest's path ([`sendable`]), isn't UTF-8, is
/// neither a file nor a link, can't be read, or holds more than a send carries.
pub(crate) fn hash(
    top: &Path,
    files: &[u8],
    commit: &str,
    bundle: &Path,
) -> Result<Hashed, String> {
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
    let source = Source::File(bundle.to_owned());
    let (digest, size_bytes) = hashed(&source)?;
    entries.push(Entry {
        kind: Kind::Bundle,
        digest: digest.clone(),
        size_bytes,
        path: commit.to_owned(),
    });
    sources.insert(digest, source);
    Ok(Hashed {
        manifest: Manifest::new(entries),
        sources,
    })
}

/// The SHA-256 of what `source` holds, and its size in bytes, read once.
fn hashed(source: &Source) -> Result<(Sha256Digest, u64), String> {
    let (digest, size_bytes) = match source {
        Source::Text(text) => (sha256(text), text.len() as u64),
        Source::File(path) => {
            let mut hasher = Hasher(Sha256::new());
            let file = File::open(path).context(path.display());
            let copied = io::copy(&mut file?, &mut hasher).context(path.display())?;
            let Hasher(sha256) = hasher;
            (
                Sha256Digest::from(<[u8; 32]>::from(sha256.finalize())),
                copied,
            )
        }
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

fn shown(source: &Source) -> String {
    match source {
        Source::File(path) => path.display().to_string(),
        Source::Text(text) => format!("the link to `{}`", String::from_utf8_lossy(text)),
    }
}

/// A writer that hashes what is written to it, so that [`io::copy`] can read a file into it.
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

/// The store's directory of manifests, each named by its SHA-256, the name of its tree.
pub(crate) const MANIFESTS: &str = "manifests";

/// The store's directory of the pieces that have arrived, unchecked, each `<sha256>.<index>`.
pub(crate) const INCOMING: &str = "incoming";

/// The exit status of the start's [`admit`] once a blob it takes in isn't whole or isn't what its
/// name says, as [`unadmitted`] reads what it printed.
pub(crate) const UNADMITTED: i32 = 102;

/// The store's lock, which a command holds while it lists the store, or changes what it holds.
pub(crate) const LOCK: &str = "lock";

/// A shell function, `lacking <manifest>`, run in the store, that prints a line for each distinct
/// blob that the manifest names and the store lacks, `missing <sha256> <size> <indices>`, the
/// indices, `,`-separated, of the pieces of it whole in [`INCOMING`]; an executable's blob is
/// held if its copy to run is. It fails, saying the line, if the manifest isn't of this form.
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

/// What a command on the controller runs to be sent the tree named `tree`, whose manifest is on
/// its standard input: once the manifest's SHA-256 is that name, and it is of this form, it prints
/// what the store lacks of it, as `lacking` does ([`lacking`]), keeps it in the store's
/// [`MANIFESTS`] under that name, then prints `end`, which [`missing`] reads.
// One call for the whole manifest, as REAPI's FindMissingBlobs takes thousands of digests a call:
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

/// What a command on the controller runs to take a stream of pieces on its standard input, as
/// [`stream`] writes it, into the store's [`INCOMING`], unchecked: the start checks each blob.
// `--touch`, as the stream's headers carry no time, and the prune collects a piece by its age.
pub(crate) fn receive() -> String {
    format!(
        "set -o errexit -o nounset\ncd \"$HOME/{STORE}/{INCOMING}\"\n\
         exec tar --extract --touch --no-same-owner --no-overwrite-dir --file=-\n"
    )
}

/// What the start of a job of the tree named `tree` runs, as a subshell, to take into the store
/// each blob that the tree's manifest names, the store lacks, and [`INCOMING`] holds every piece
/// of: their pieces, in order, are its bytes, which it keeps once their SHA-256 is its name, and
/// removes otherwise. If one isn't whole, or isn't what its name says, it prints a line for each,
/// `gone <sha256>` or `changed <sha256>`, then exits [`UNADMITTED`].
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

/// Why the start refused the blobs that `printed`, what [`admit`] printed as it exited
/// [`UNADMITTED`], names, of `hashed`: the first that changed, by where this machine holds it,
/// else how many of the task's files are gone, and whether Pumpkin's bundle is.
pub(crate) fn unadmitted(hashed: &Hashed, printed: &[u8]) -> String {
    let printed = String::from_utf8_lossy(printed);
    let mut changed = Vec::new();
    let mut gone = BTreeSet::new();
    for line in printed.lines() {
        let (word, hex) = line.split_once(' ').unwrap_or((line, ""));
        let Some(digest) = digest(hex) else { continue };
        match word {
            "changed" => changed.push(digest),
            _ => {
                gone.insert(digest);
            }
        }
    }
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
    let entries = hashed.manifest.entries.iter();
    let (bundles, files): (Vec<&Entry>, Vec<&Entry>) = (entries)
        .filter(|entry| gone.contains(&entry.digest))
        .partition(|entry| entry.kind == Kind::Bundle);
    let mut what = Vec::new();
    if !files.is_empty() {
        what.push(format!("{} of the task's files", files.len()));
    }
    if !bundles.is_empty() {
        what.push("Pumpkin's bundle".to_owned());
    }
    let is = if files.len() + bundles.len() == 1 {
        "is"
    } else {
        "are"
    };
    format!(
        "{} {is} no longer on the controller, as its prune removes what no tree holds: run it \
         again",
        what.join(" and ")
    )
}

/// A blob that the controller's store lacks, as its answer to [`query`] names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Missing {
    pub(crate) digest: Sha256Digest,
    pub(crate) size_bytes: u64,
    /// The indices of its pieces that the controller already holds whole.
    pub(crate) held: BTreeSet<u64>,
}

/// The blobs that `answer`, the controller's to [`query`], says its store lacks.
///
/// # Errors
///
/// Fails, saying the line, if a line is none that [`query`] prints, or `answer` doesn't end with
/// `end`, as one cut off doesn't.
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

/// The [`Missing`] that `line`, of the controller's answer, names.
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

/// A piece of a blob, its bytes from `index` times [`PIECE_BYTES`] on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Piece {
    pub(crate) digest: Sha256Digest,
    pub(crate) index: u64,
    pub(crate) size_bytes: u64,
}

impl Piece {
    /// Its name in `incoming/`, `<sha256>.<index>`, the index in [`INDEX_DIGITS`] digits.
    pub(crate) fn name(&self) -> String {
        format!("{}.{:0INDEX_DIGITS$}", self.digest.hex(), self.index)
    }

    fn offset_bytes(&self) -> u64 {
        self.index * PIECE_BYTES
    }
}

/// The pieces of `missing` to send, in order: each blob's, but those the controller holds.
pub(crate) fn pieces(missing: &[Missing]) -> Vec<Piece> {
    let mut pieces = Vec::new();
    for Missing {
        digest,
        size_bytes,
        held,
    } in missing
    {
        // An empty blob is one piece of no bytes.
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

/// Writes `pieces`, each read from where `sources`, which hold each one's blob, say it is, to
/// `out` as a POSIX `ustar` stream that [`receive`] extracts, a member each, named as
/// [`Piece::name`] names it, 0444.
///
/// # Errors
///
/// Fails if writing to `out` fails; else gives, naming it, a source that can't be read, or is
/// gone or holds fewer bytes than its piece, as one that changed while it was sent. The stream
/// then ends after the last whole piece, if the source was found so before its piece began.
pub(crate) fn stream(
    pieces: &[Piece],
    sources: &BTreeMap<Sha256Digest, Source>,
    out: &mut impl Write,
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
        match part(blob, piece, out)? {
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

/// What a blob is read from.
trait ReadSeek: Read + Seek {}

impl<T: Read + Seek> ReadSeek for T {}

/// Copies `piece`'s bytes from `blob` to `out`, until `blob` ends; gives how many there were, or
/// why `blob` can't be read. Fails if writing to `out` fails.
fn part(
    mut blob: impl Read + Seek,
    piece: &Piece,
    out: &mut impl Write,
) -> io::Result<io::Result<u64>> {
    if let Err(error) = blob.seek(SeekFrom::Start(piece.offset_bytes())) {
        return Ok(Err(error));
    }
    let mut blob = blob.take(piece.size_bytes);
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
        copied += read as u64;
    }
}

/// A `tar` block, of which a member's header is one, and its bytes a whole number.
pub(crate) const BLOCK: usize = 512;

/// The zeros that fill a member of `size_bytes` to a whole block.
fn padding(size_bytes: u64) -> usize {
    let size = usize::try_from(size_bytes % BLOCK as u64).unwrap_or(0);
    (BLOCK - size) % BLOCK
}

/// The POSIX `ustar` header of a regular file named `name`, of at most 100 bytes, with mode
/// `mode`, owned by user and group 0, holding `size_bytes`, at the epoch.
pub(crate) fn header(name: &str, mode: u32, size_bytes: u64) -> [u8; BLOCK] {
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
