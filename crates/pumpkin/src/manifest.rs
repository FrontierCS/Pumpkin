//! A task's tree as the client sends it to the controller: by content, to a store there of files
//! named by their SHA-256, which keeps each for every later send, whatever task, repository or
//! commit names it.
//!
//! The client hashes each tree into a manifest, asks which blobs the controller lacks, and sends
//! the rest by offset. The controller's [`store`] admits each complete blob by its SHA-256, as
//! Bazel's remote execution finds missing blobs before a resumable write:
//! <https://github.com/bazelbuild/remote-apis/blob/c2c07e3d718d3d490a4958344d9c3e47de0bfaf3/build/bazel/remote/execution/v2/remote_execution.proto#L350-L362>.
//!
//! The store holds a blob as `<sha256>`, 0444, and its executable copy as `<sha256>.x`, 0555,
//! as links share their file's mode. A tree of hard links and its collection are the job's.

pub(crate) mod store;

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

// The first line of a manifest; any change to what a line holds
// changes it. The job runs the client's build to make its tree, but the prune reads every
// manifest in the store, and keeps every blob while one is of a form it can't read.
const FORM: &str = "pumpkin-tree/1";

/// The largest file that a task can send, the limit of the store's first manifest form.
pub(crate) const BLOB_BYTES: u64 = 100_000 * (16 << 20);

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
    pub(crate) fn new(mut entries: Vec<Entry>) -> Self {
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
        let lines = lines(text)?;
        let entries = lines.map(entry).collect::<Result<_, _>>()?;
        Ok(Self { entries })
    }
}

/// The prune also reads manifests of builds that carried a git bundle. Its row names a blob,
/// just as a file does, but its path is the commit that the bundle holds.
pub(crate) fn referenced(text: &[u8]) -> Result<BTreeSet<Sha256Digest>, String> {
    lines(text)?
        .map(|line| {
            if let Some(fields) = line.strip_prefix("b ") {
                let file = entry(&format!("f {fields}"))?;
                if file.path.len() != 40
                    || !file
                        .path
                        .bytes()
                        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
                {
                    return Err(format!(
                        "the bundle's path is not a git commit: {}",
                        file.path.escape_debug()
                    ));
                }
                Ok(file.digest)
            } else {
                Ok(entry(line)?.digest)
            }
        })
        .collect()
}

fn lines(text: &[u8]) -> Result<impl Iterator<Item = &str>, String> {
    let text = std::str::from_utf8(text)
        .map_err(|error| format!("the manifest isn't UTF-8 ({error}): expected text"))?;
    let mut lines = text.split_terminator('\n');
    if lines.next() != Some(FORM) {
        return Err(format!(
            "the manifest doesn't start with `{FORM}`: expected a manifest of this Pumpkin's \
                 form"
        ));
    }
    Ok(lines)
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

impl Source {
    pub(crate) fn at(&self, from: u64, expected_bytes: u64) -> Result<Box<dyn Read + '_>, String> {
        let changed = || {
            format!(
                "{} changed since the task was hashed: send the task again",
                shown(self)
            )
        };
        match self {
            Self::File(path) => {
                let mut file = File::open(path).context(path.display())?;
                if file.metadata().context(path.display())?.len() != expected_bytes {
                    return Err(changed());
                }
                file.seek(SeekFrom::Start(from)).context(path.display())?;
                Ok(Box::new(file))
            }
            Self::Text(text) => {
                let rest = usize::try_from(from).ok().and_then(|from| text.get(from..));
                Ok(Box::new(rest.ok_or_else(changed)?))
            }
        }
    }
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
    let file = File::open(path).context(path.display())?;
    hash_reader(file).context(path.display())
}

fn hash_reader(mut file: impl Read) -> io::Result<(Sha256Digest, u64)> {
    let mut hasher = Hasher(Sha256::new());
    let copied = io::copy(&mut file, &mut hasher)?;
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

/// Held while admitting a blob or collecting the store.
pub(crate) const LOCK: &str = "lock";

const BLOCK: usize = 512;

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
