//! Building an image from its context and recipe, tagged by the digest of the build's inputs,
//! as [`Engine::build`] says.
//!
//! A build that finds no image by its tag reads its context from a stage, a copy of it that is
//! the build's alone, and tags its image by the stage's digest. The digest is the SHA-256 of a
//! text that names the inputs: a line that names the text's form, [`FORM`], and the options
//! every build gives `docker build`, [`OPTIONS`]; a line for the recipe, `file` and its path in
//! the context, or `text` and the SHA-256 of a [`Recipe::Text`]; then a line for each directory
//! and regular file of the context, or of its stage, the root first, as `./`, and then the rest,
//! sorted by their paths' bytes, a directory's without its `/`, so `bin/` comes before `bin-x`:
//!
//! ```text
//! pumpkin-build/2 <option>...
//! file <recipe>
//! ./ <mode>
//! <path>/ <mode>
//! <path> <mode> <sha256>
//! ```
//!
//! A path is relative to the context, a directory's ends in `/`, a mode is the permission bits
//! that the stage gives the entry ([`BuildContext`]), in four octal digits, and a SHA-256 is in
//! lowercase hexadecimal. No name holds whitespace or a control character, which would make its
//! line ambiguous.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::{fmt, io, iter};

use schema::ir::{Image, Sha256Digest};
use schema::sandbox::RelativePath;
use sha2::{Digest, Sha256};
use stdx::Context;

use crate::{ContextName, Engine, Stderr, docker, docker_to, fix_reported, image_id};

/// The permission bits of each directory of a stage.
// Git's two modes for a regular file, as a checkout under a 022 umask gives them, so that one
// commit has one tag whatever the umask of its checkout. The build reads the staged bits that
// the digest covers, so the tag still names what is built.
const DIR_MODE: u32 = 0o755;

/// The permission bits of each file of a stage that its owner may execute.
const EXECUTABLE_MODE: u32 = 0o755;

/// The permission bits of each other file of a stage.
const FILE_MODE: u32 = 0o644;

const OWNER_EXECUTE: u32 = 0o100;

/// The name and number of the inputs' text's form, the first word of its first line.
// The number changes with what the text holds or how it is written, so that the texts of two
// forms never coincide, and no tag of one names an image of another.
const FORM: &str = "pumpkin-build/2";

/// The options every build gives `docker build`, beside those that name its recipe, its context
/// and the file it writes the image's ID to.
// They are in the inputs' text, so that a change to them is a change of every tag, rather than
// one that a found image hides until a task's files change.
//
// A provenance attestation records each build's own ID and times, so a build with one would
// make an image of its own even from the cache. `--provenance` is Buildx's, from its version
// 0.10. Docker 23.0 ships Buildx 0.10.2 as a plugin, which `docker build` runs when it is
// installed; without it, `docker build` falls back to the classic builder, which has no such
// option. And the recipe that adds a harness to an agent's image (`Pin::recipe`) fetches with
// `ADD --checksum`, which BuildKit's Dockerfile frontend takes outside its labs channel only
// from BuildKit 0.12, which Docker 25.0 is the first to ship. So the provider needs Docker 25.0
// or later with its Buildx plugin.
const OPTIONS: [&str; 1] = ["--provenance=false"];

/// An image that [`Engine::build`] built or found, and only it makes: a build given one as a
/// base may name it by its tag.
#[derive(Debug)]
pub struct Built {
    image: Image,
    tag: String,
}

impl Built {
    /// The image's ID.
    pub fn image(&self) -> &Image {
        &self.image
    }

    /// The tag `build` found the image by or gave it, `<repository>:<hex>`, by which a recipe's
    /// `FROM` can name it, until another build of the same inputs moves it to its own image.
    pub fn tag(&self) -> &str {
        &self.tag
    }
}

/// A build's context, which [`Engine::build`] copies to a stage of the build's own if it finds
/// no image by its tag.
///
/// It holds what `dir` holds, or nothing without one, and each directory of `mounts` at its
/// path, which must name nothing that `dir` or an earlier mount holds, in a directory that one
/// of them holds. It holds only directories and regular files, named in UTF-8 without
/// whitespace or a control character. Its stage gives each directory mode 0755, and each file
/// 0755 if its owner may execute it, else 0644.
#[derive(Debug, Default)]
pub struct BuildContext {
    pub dir: Option<PathBuf>,
    pub mounts: BTreeMap<RelativePath, PathBuf>,
}

/// A build's recipe, a Dockerfile.
#[derive(Debug)]
pub enum Recipe {
    /// The regular file at this path in the context.
    File(RelativePath),
    /// This text, which the context needn't hold.
    Text(String),
}

impl Engine {
    /// The image that `recipe`, once [`recipe::check`] passes it, builds with `context` on
    /// `bases`, which it may name by their tags: the engine's under the tag `<repository>:<hex>`,
    /// the digest of the build's inputs, or else one built now from a stage of the context.
    ///
    /// # Errors
    ///
    /// Fails as [`recipe::check`] does, if `context` isn't as [`BuildContext`] says, if a
    /// [`Recipe::File`] is no regular file of it, if `docker` fails, or if the stage can't be
    /// made or removed.
    pub fn build(
        &self,
        context: &BuildContext,
        recipe: &Recipe,
        bases: &[&Built],
        repository: &str,
    ) -> Result<Built, String> {
        let bases: Vec<&str> = bases.iter().map(|base| base.tag()).collect();
        // An image found by the tag was built from a stage whose inputs are those the context's
        // own files give now, so only a build needs a stage.
        let inputs = Inputs::checked(read_context(context)?, None, recipe, &bases)?;
        let tag = format!("{repository}:{}", inputs.digest());
        let found = image_id(&self.report.context, &tag).map_err(|error| {
            fix_reported(format!(
                "the image tagged `{tag}` can't be looked up: {error}"
            ))
        })?;
        if let Some(id) = found {
            let image = Image::try_from(id)?;
            return Ok(Built { image, tag });
        }
        let scratch = tempfile::Builder::new()
            .prefix("pumpkin-build-")
            .permissions(Permissions::from_mode(0o700))
            .tempdir()
            .map_err(|error| {
                format!(
                    "a directory for a build can't be made: {error}: let this user make one in \
                     the system's temporary directory"
                )
            })?;
        let path = scratch.path().to_owned();
        let built = self.build_in(&path, &inputs, recipe, &bases, repository);
        let left = |removing: io::Error| {
            format!(
                "{} can't be removed after the build: {removing}: remove it",
                path.display()
            )
        };
        match (built, scratch.close()) {
            (built, Ok(())) => built,
            (Ok(_), Err(removing)) => Err(left(removing)),
            (Err(error), Err(removing)) => Err(format!("{error}; and {}", left(removing))),
        }
    }

    /// The image of [`Engine::build`] that `recipe` builds from a stage of `inputs`, in
    /// `scratch`, an empty directory of the build's own, which also holds a text recipe and the
    /// file the build writes the image's ID to.
    fn build_in(
        &self,
        scratch: &Path,
        inputs: &Inputs,
        recipe: &Recipe,
        bases: &[&str],
        repository: &str,
    ) -> Result<Built, String> {
        let stage = scratch.join("context");
        // The stage is this build's alone, so the bytes the check reads there are those the
        // digest covers and the build reads, which may differ from those `inputs` read.
        let staged = Inputs::checked(inputs.stage(&stage)?, Some(&stage), recipe, bases)?;
        let file = match recipe {
            Recipe::File(path) => stage.join(path.as_str()),
            Recipe::Text(text) => {
                let file = scratch.join("Dockerfile");
                fs::write(&file, text).context(file.display())?;
                file
            }
        };
        let tag = format!("{repository}:{}", staged.digest());
        let context = &self.report.context;
        let image = build_untagged(context, &stage, &file, &scratch.join("id"))?;
        // The tag moves only if an image of the same inputs has it: one that another build
        // tagged since the lookup, or one of what the context's files changed to since they were
        // read. And it is `docker tag`'s, not the build's: in the containerd image store,
        // a tag that a build moves to its image drops the image the tag named, which a run may
        // still be using, whereas one that `docker tag` moves leaves that image, untagged.
        let tagged = docker(context, ["tag", "--", image.as_str(), &tag]);
        tagged
            .map_err(|error| fix_reported(format!("{image} can't be tagged `{tag}`: {error}")))?;
        Ok(Built { image, tag })
    }
}

/// Builds `recipe`, a file, with `stage` as its context, on the engine of `context`, names the
/// image it builds with no tag, and returns its ID, which the build writes to `id_file`.
fn build_untagged(
    context: &ContextName,
    stage: &Path,
    recipe: &Path,
    id_file: &Path,
) -> Result<Image, String> {
    // `docker build` has no `--pull=never`: it pulls a base the engine lacks, by the reference
    // its recipe names, which pins it by digest.
    let build = (iter::once("build").chain(OPTIONS).map(OsStr::new)).chain([
        OsStr::new("--file"),
        recipe.as_os_str(),
        OsStr::new("--iidfile"),
        id_file.as_os_str(),
        OsStr::new("--"),
        stage.as_os_str(),
    ]);
    docker_to(context, build, Stderr::Shown).map_err(fix_reported)?;
    let id = fs::read_to_string(id_file).map_err(|error| {
        format!(
            "the ID of the image `docker build` built can't be read from {}: {error}: the build \
             should have written it there",
            id_file.display()
        )
    })?;
    Image::try_from(id.trim().to_owned())
}

/// A build's inputs, as the [module](self)'s doc lists them.
struct Inputs {
    /// The recipe's line.
    recipe: String,
    /// Each directory and regular file below the context's root, by its path.
    entries: BTreeMap<String, Entry>,
}

/// A directory or a regular file below the context's root, with, for a file, the mode a stage
/// gives it, its SHA-256, in hexadecimal, and the path it was read from, for a stage to copy
/// and the recipe's check to name.
enum Entry {
    Dir,
    File {
        mode: u32,
        sha256: String,
        from: PathBuf,
    },
}

impl Inputs {
    /// Copies these inputs' entries to `stage`, which names nothing yet, each file from the path
    /// it was read from, with its mode; and returns the entries that the stage holds.
    fn stage(&self, stage: &Path) -> Result<BTreeMap<String, Entry>, String> {
        make_dir(stage)?;
        let mut entries = BTreeMap::new();
        // By their paths, so each directory is made before what is below it.
        for (path, entry) in &self.entries {
            let to = stage.join(path);
            let staged = match entry {
                Entry::Dir => {
                    make_dir(&to)?;
                    Entry::Dir
                }
                Entry::File {
                    mode,
                    sha256: _,
                    from,
                } => {
                    fs::copy(from, &to).context(format!(
                        "{} to {}",
                        from.display(),
                        to.display()
                    ))?;
                    fs::set_permissions(&to, Permissions::from_mode(*mode))
                        .context(to.display())?;
                    Entry::File {
                        mode: *mode,
                        // Hashed from the copy, which is what the build reads.
                        sha256: file_sha256(&to).context(to.display())?,
                        from: from.clone(),
                    }
                }
            };
            entries.insert(path.clone(), staged);
        }
        Ok(entries)
    }

    /// The inputs with `entries`, once [`recipe::check`] has passed `recipe`'s bytes, as a
    /// recipe that may name `bases` by their tags: a [`Recipe::File`]'s as read from `stage`,
    /// if given, or else from the path it was found at, and then named by its entry.
    fn checked(
        mut entries: BTreeMap<String, Entry>,
        stage: Option<&Path>,
        recipe: &Recipe,
        bases: &[&str],
    ) -> Result<Self, String> {
        let recipe = match recipe {
            Recipe::File(path) => {
                let Some(Entry::File { sha256, from, .. }) = entries.get_mut(path.as_str()) else {
                    return Err(format!(
                        "`{}` is no regular file in the build's context: name the build's \
                         recipe by its path there",
                        path.as_str().escape_debug()
                    ));
                };
                let file = stage.map_or_else(|| from.clone(), |stage| stage.join(path.as_str()));
                let text = fs::read(&file).context(file.display())?;
                recipe::check(&from.display().to_string(), &text, bases)?;
                // The digest covers the bytes the check passed, even if the file has changed
                // since it was hashed.
                *sha256 = hex(Sha256::digest(&text).into());
                format!("file {path}")
            }
            Recipe::Text(text) => {
                // Named as Docker names a recipe by default.
                recipe::check("Dockerfile", text.as_bytes(), bases)?;
                format!("text {}", hex(Sha256::digest(text).into()))
            }
        };
        Ok(Self { recipe, entries })
    }

    /// The hexadecimal digits of the inputs' digest.
    fn digest(&self) -> String {
        hex(Sha256::digest(self.to_string()).into())
    }
}

impl fmt::Display for Inputs {
    /// The text whose lines the module doc gives, each ending in a line feed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{FORM} {}", OPTIONS.join(" "))?;
        writeln!(f, "{}", self.recipe)?;
        writeln!(f, "./ {DIR_MODE:04o}")?;
        for (path, entry) in &self.entries {
            match entry {
                Entry::Dir => writeln!(f, "{path}/ {DIR_MODE:04o}")?,
                Entry::File {
                    mode,
                    sha256,
                    from: _,
                } => writeln!(f, "{path} {mode:04o} {sha256}")?,
            }
        }
        Ok(())
    }
}

/// Each directory and regular file below `context`'s root, by its path: what its directory
/// holds, then each of its mounts, each file hashed as it is now.
fn read_context(context: &BuildContext) -> Result<BTreeMap<String, Entry>, String> {
    let mut entries = BTreeMap::new();
    if let Some(dir) = &context.dir {
        read_dir(dir, "", &mut entries)?;
    }
    for (at, dir) in &context.mounts {
        let at = at.as_str();
        if ambiguous(at) {
            return Err(format!(
                "`{}` holds whitespace or a control character: mount {} at a path without them",
                at.escape_debug(),
                dir.display()
            ));
        }
        if entries.contains_key(at) {
            return Err(format!(
                "the build's context holds `{at}` already, where {} is to be mounted: mount it \
                 where the context holds nothing",
                dir.display()
            ));
        }
        if let Some((parent, _)) = at.rsplit_once('/')
            && !matches!(entries.get(parent), Some(Entry::Dir))
        {
            return Err(format!(
                "the build's context holds no directory `{parent}` for {} to be mounted in: \
                 mount it in a directory the context holds",
                dir.display()
            ));
        }
        read_dir(dir, &format!("{at}/"), &mut entries)?;
        entries.insert(at.to_owned(), Entry::Dir);
    }
    Ok(entries)
}

/// Makes the directory `dir`, which names nothing yet, with bits [`DIR_MODE`].
fn make_dir(dir: &Path) -> Result<(), String> {
    (fs::create_dir(dir))
        .and_then(|()| fs::set_permissions(dir, Permissions::from_mode(DIR_MODE)))
        .context(dir.display())
}

/// Adds each directory and regular file below `dir` to `entries`, by its path in the context:
/// `prefix`, which is empty or ends in `/`, then its path below `dir`.
fn read_dir(dir: &Path, prefix: &str, entries: &mut BTreeMap<String, Entry>) -> Result<(), String> {
    for entry in fs::read_dir(dir).context(dir.display())? {
        let entry = entry.context(dir.display())?;
        let found = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(format!(
                "{} isn't named in UTF-8: name a build context's files in UTF-8",
                found.display()
            ));
        };
        if ambiguous(name) {
            return Err(format!(
                "`{}` holds whitespace or a control character: name a build context's files \
                 without them",
                found.display().to_string().escape_debug()
            ));
        }
        let path = format!("{prefix}{name}");
        // Not followed: a link is neither a directory nor a regular file.
        let metadata = entry.metadata().context(found.display())?;
        if metadata.is_dir() {
            read_dir(&found, &format!("{path}/"), entries)?;
            entries.insert(path, Entry::Dir);
        } else if metadata.is_file() {
            let executable = metadata.permissions().mode() & OWNER_EXECUTE != 0;
            let file = Entry::File {
                mode: if executable {
                    EXECUTABLE_MODE
                } else {
                    FILE_MODE
                },
                sha256: file_sha256(&found).context(found.display())?,
                from: found,
            };
            entries.insert(path, file);
        } else {
            return Err(format!(
                "{} is neither a directory nor a regular file: give a build's context only those",
                found.display()
            ));
        }
    }
    Ok(())
}

/// Whether `name` holds whitespace or a control character, which would make its line of the
/// inputs' text ambiguous.
fn ambiguous(name: &str) -> bool {
    name.contains(|char: char| char.is_whitespace() || char.is_control())
}

/// The SHA-256 of the file at `path`, in hexadecimal.
fn file_sha256(path: &Path) -> io::Result<String> {
    let mut hasher = Hasher(Sha256::new());
    io::copy(&mut File::open(path)?, &mut hasher)?;
    let Hasher(sha256) = hasher;
    Ok(hex(sha256.finalize().into()))
}

/// A writer that hashes what is written to it, so that [`io::copy`] can read a file into it.
struct Hasher(Sha256);

impl io::Write for Hasher {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The hexadecimal digits of the [`Sha256Digest`] of SHA-256's 32 bytes, `sha256`.
fn hex(sha256: [u8; 32]) -> String {
    Sha256Digest::from(sha256)
        .as_str()
        .trim_start_matches("sha256:")
        .to_owned()
}
