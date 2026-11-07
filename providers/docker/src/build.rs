//! Building an image from its recipe, tagged by the digest of the build's inputs, as
//! [`Engine::build`] says.
//!
//! The digest is the SHA-256 of a text that names the inputs: a line that names the text's form,
//! [`FORM`], and the options every build gives `docker build`, [`OPTIONS`]; the recipe's path in
//! the context; then a line for each directory and regular file of the context, the root first,
//! as `./`, and then the rest, sorted by their paths' bytes, a directory's without its `/`, so
//! `bin/` comes before `bin-x`:
//!
//! ```text
//! pumpkin-build/1 <option>...
//! <recipe>
//! ./ <mode>
//! <path>/ <mode>
//! <path> <mode> <sha256>
//! ```
//!
//! A path is relative to the context, a directory's ends in `/`, a mode is the permission bits
//! in four octal digits, and the SHA-256 is a file's, in lowercase hexadecimal. No name holds
//! whitespace or a control character, which would make its line ambiguous.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, DirBuilder, File};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;
use std::{env, fmt, io, iter};

use schema::ir::{Image, Sha256Digest};
use sha2::{Digest, Sha256};
use stdx::Context;

use crate::{ContextName, Engine, Stderr, docker, docker_to, fix_reported, image_id};

/// The permission bits of a mode: the rest of an `st_mode` is the file's type.
const PERMISSION_BITS: u32 = 0o7777;

/// The name and number of the inputs' text's form, the first word of its first line.
// The number changes with what the text holds or how it is written, so that the texts of two
// forms never coincide, and no tag of one names an image of another.
const FORM: &str = "pumpkin-build/1";

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

/// An image that [`Engine::build`] built or found.
#[derive(Debug)]
pub struct Built {
    /// The image's ID.
    pub image: Image,
    /// The tag `build` found the image by or gave it, `<repository>:<hex>`, by which a recipe's
    /// `FROM` can name it, until another build of the same inputs moves it to its own image.
    pub tag: String,
}

impl Engine {
    /// The image that `recipe`, a Dockerfile's path in `dir`, builds with `dir` as its context,
    /// tagged `<repository>:<hex>`, the hexadecimal digits of the digest of the build's inputs:
    /// the options it is built with, `recipe`, and the path, permission bits and content of
    /// each directory and regular file in `dir`, but not their owners or times. Once `check`
    /// has passed the recipe's bytes, those the digest covers, it is the image the engine has
    /// under that tag, or else one built now, with its progress on this process's standard
    /// error, and then tagged. The tag covers what an image is built from only where its recipe
    /// names by digest whatever it fetches, such as its base images.
    ///
    /// # Errors
    ///
    /// Fails if `dir` can't be read, or holds anything but directories and regular files, or a
    /// name that isn't UTF-8 or holds whitespace or a control character; if `recipe` is no
    /// regular file in it; as `check` does; if `docker` can't look the tag up, build the image,
    /// as its progress then says, or tag it; or if `dir` changed during the build.
    pub fn build(
        &self,
        dir: &Path,
        recipe: &str,
        repository: &str,
        check: impl FnOnce(&[u8]) -> Result<(), String>,
    ) -> Result<Built, String> {
        let inputs = Inputs::read(dir, recipe)?;
        // The check reads the very bytes the digest covers, so the image comes from bytes it
        // passed: a found one, as its tag names them, and one built now, as it is tagged only
        // if the inputs, read again after the build, have the same digest.
        check(&inputs.text)?;
        let hex = inputs.digest();
        let tag = format!("{repository}:{hex}");
        let context = &self.report.context;
        let found = image_id(context, &tag).map_err(|error| {
            fix_reported(format!(
                "the image tagged `{tag}` can't be looked up: {error}"
            ))
        })?;
        if let Some(id) = found {
            let image = Image::try_from(id)?;
            return Ok(Built { image, tag });
        }
        let image = build_untagged(context, dir, recipe, &hex)?;
        // The build reads a file when its recipe copies it, which can be minutes after the
        // inputs were read: if any changed in between, the image may hold what the tag doesn't
        // name, so it is tagged only if the inputs, read again, are the same.
        if Inputs::read(dir, recipe)?.digest() != hex {
            return Err(format!(
                "{} changed during the build, so the image isn't tagged: build again once \
                 nothing changes it",
                dir.display()
            ));
        }
        // The tag moves only if another build of the same inputs has tagged its image since
        // the lookup. And it is `docker tag`'s, not the build's: in the containerd image store,
        // a tag that a build moves to its image drops the image the tag named, which a run may
        // still be using, whereas one that `docker tag` moves leaves that image, untagged.
        let tagged = docker(context, ["tag", "--", image.as_str(), &tag]);
        tagged
            .map_err(|error| fix_reported(format!("{image} can't be tagged `{tag}`: {error}")))?;
        Ok(Built { image, tag })
    }
}

/// Builds `recipe` in `dir` on the engine of `context`, names the image it builds with no tag,
/// and returns its ID, which the build writes to a file in a directory of its own: named by
/// this process and `hex`, the digest of the build's inputs, and created only if nothing is at
/// its path, so that no other build writes there.
fn build_untagged(
    context: &ContextName,
    dir: &Path,
    recipe: &str,
    hex: &str,
) -> Result<Image, String> {
    let scratch = env::temp_dir().join(format!("pumpkin-build-{}-{hex}", std::process::id()));
    (DirBuilder::new().mode(0o700).create(&scratch)).map_err(|error| {
        format!(
            "{} can't be created for the image ID `docker build` writes: {error}: let this user \
             create it in the system's temporary directory",
            scratch.display()
        )
    })?;
    let id_file = scratch.join("id");
    let recipe = dir.join(recipe);
    // `docker build` has no `--pull=never`: it pulls a base the engine lacks, by the reference
    // its recipe names, which pins it by digest.
    let build = (iter::once("build").chain(OPTIONS).map(OsStr::new)).chain([
        OsStr::new("--file"),
        recipe.as_os_str(),
        OsStr::new("--iidfile"),
        id_file.as_os_str(),
        OsStr::new("--"),
        dir.as_os_str(),
    ]);
    let id = docker_to(context, build, Stderr::Shown)
        .map_err(fix_reported)
        .and_then(|_| {
            fs::read_to_string(&id_file).map_err(|error| {
                format!(
                    "the ID of the image `docker build` built can't be read from {}: {error}: \
                     the build should have written it there",
                    id_file.display()
                )
            })
        });
    let _ = fs::remove_dir_all(&scratch);
    Image::try_from(id?.trim().to_owned())
}

/// A build's inputs, as the [module](self)'s doc lists them.
struct Inputs {
    /// The recipe's path in the context, which names a regular file there.
    recipe: String,
    /// The recipe's bytes, whose SHA-256 its entry in `entries` holds.
    text: Vec<u8>,
    /// The context's permission bits.
    root: u32,
    /// Each directory and regular file below the context's root, by its path.
    entries: BTreeMap<String, Entry>,
}

/// A directory or a regular file below the context's root, with its permission bits, and a
/// file's SHA-256, in hexadecimal.
enum Entry {
    Dir { mode: u32 },
    File { mode: u32, sha256: String },
}

impl Inputs {
    /// The inputs of a build of `recipe` with `dir` as its context.
    fn read(dir: &Path, recipe: &str) -> Result<Self, String> {
        let metadata = fs::metadata(dir).context(dir.display())?;
        let mut entries = BTreeMap::new();
        read_dir(dir, "", &mut entries)?;
        let Some(Entry::File { sha256, .. }) = entries.get_mut(recipe) else {
            return Err(format!(
                "`{}` is no regular file in {}: name the build's recipe by its path there",
                recipe.escape_debug(),
                dir.display()
            ));
        };
        // Read again, whole, and hashed from these bytes, so that the digest covers the very
        // bytes a check of `text` reads, even if the file changed since it was first hashed.
        let path = dir.join(recipe);
        let text = fs::read(&path).context(path.display())?;
        *sha256 = hex(Sha256::digest(&text).into());
        Ok(Self {
            recipe: recipe.to_owned(),
            text,
            root: metadata.permissions().mode() & PERMISSION_BITS,
            entries,
        })
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
        writeln!(f, "./ {:04o}", self.root)?;
        for (path, entry) in &self.entries {
            match entry {
                Entry::Dir { mode } => writeln!(f, "{path}/ {mode:04o}")?,
                Entry::File { mode, sha256 } => writeln!(f, "{path} {mode:04o} {sha256}")?,
            }
        }
        Ok(())
    }
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
        if name.contains(|char: char| char.is_whitespace() || char.is_control()) {
            return Err(format!(
                "`{}` holds whitespace or a control character: name a build context's files \
                 without them",
                found.display().to_string().escape_debug()
            ));
        }
        let path = format!("{prefix}{name}");
        // Not followed: a link is neither a directory nor a regular file.
        let metadata = entry.metadata().context(found.display())?;
        let mode = metadata.permissions().mode() & PERMISSION_BITS;
        if metadata.is_dir() {
            read_dir(&found, &format!("{path}/"), entries)?;
            entries.insert(path, Entry::Dir { mode });
        } else if metadata.is_file() {
            let sha256 = file_sha256(&found).context(found.display())?;
            entries.insert(path, Entry::File { mode, sha256 });
        } else {
            return Err(format!(
                "{} is neither a directory nor a regular file: give a build's context only those",
                found.display()
            ));
        }
    }
    Ok(())
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
