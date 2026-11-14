//! Building an image from its context and recipe, tagged by the digest of the build's inputs,
//! as [`Engine::build`] says.
//!
//! A build that finds no image by its tag reads its context from a stage, a copy of it that is
//! the build's alone, and tags its image by the stage's digest: the SHA-256 of a text of a line
//! for the text's form, [`FORM`], and [`OPTIONS`]; one for the recipe, `file` and its path or
//! `text` and its SHA-256; one for each of what the build is [`Given`], a named context by its
//! image's tag ([`Built::tag`]); and one for each directory and regular file of the context or
//! its stage, the root first, as `./`, then the rest by their paths' bytes, a directory's
//! without its `/`, so `bin/` comes before `bin-x`:
//!
//! ```text
//! pumpkin-build/4 <option>...
//! file <recipe>
//! target <stage>
//! arg <name> <sha256>
//! context <name> <tag>
//! ./ <mode>
//! <path>/ <mode>
//! <path> <mode> <sha256>
//! ```
//!
//! A mode is the bits the stage gives the entry ([`BuildContext`]), in four octal digits, and a
//! SHA-256 is in lowercase hexadecimal. No name holds whitespace or a control character.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File, Permissions};
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, UNIX_EPOCH};
use std::{fmt, io};

use recipe::Given;
use schema::ir::{Image, Sha256Digest};
use schema::sandbox::RelativePath;
use sha2::{Digest, Sha256};
use stdx::Context;
use tempfile::TempDir;

use crate::{ContextName, Engine, docker, docker_command, fix_reported, image_id};

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

/// The time of every build, since the Unix epoch, 1980-01-01T00:00:00Z: when each file and
/// directory of its stage was last modified, and, as `SOURCE_DATE_EPOCH`, when its image and each
/// step its recipe adds to the image's history were created.
// One time for every build, so that a layer that holds only staged files and directories, as a
// `COPY` of them into `/` does, is the same bytes from any engine that compresses it alike, and
// after the build cache is lost, and so is an image whose every layer is: a registry, and a
// provider that imports images by their digests, then has it already. BuildKit gives the build's
// own time to what a `RUN` writes, and to each directory above what a copy copies, made or changed,
// such as a directory of the base it copies into, so such a layer differs still. Not the epoch
// itself: the ZIP format, which Python's wheels and Java's archives use, holds no time before 1980,
// and Python's `zipfile` refuses to write one, so a recipe that zips staged files would fail. The
// layers' own times are left as BuildKit gives them, not clamped to this one (its
// `rewrite-timestamp`): that would change what a `RUN` installs, such as a Python module whose
// compiled form records the module's time, so that it would be compiled again at each import.
// The days from 1970 to 1980, two of whose years are leap years.
const BUILD_TIME: Duration = Duration::from_hours(3_652 * 24);

/// The name and number of the inputs' text's form, the first word of its first line.
// The number changes with what the text holds, how it is written, or how a build reads what it
// names, such as its stage's times, so that the texts of two forms never coincide, and no tag of
// one names an image of another. The lines of what a build is given share it with a build given
// nothing: each starts with a word, `target`, `arg` or `context`, where a text without them has
// `./ `, so no text with them is one without.
const FORM: &str = "pumpkin-build/4";

/// The options every build gives `docker build`, beside those that name its recipe, its context
/// and the file it writes the image's ID to: these, and [`BUILD_TIME`] as `SOURCE_DATE_EPOCH`
/// ([`options`]).
// They are in the inputs' text, so that a change to them is a change of every tag, rather than
// one that a found image hides until a task's files change.
//
// A provenance attestation records each build's own ID and times, so a build with one would
// make an image of its own even from the cache. `--provenance` is Buildx's, from its version
// 0.10. Docker 23.0 ships Buildx 0.10.2 as a plugin, which `docker build` runs when it is
// installed; without it, `docker build` falls back to the classic builder, which has no such
// option. And the recipe that adds a harness to an agent's image (`Pin::recipe`) fetches with
// `ADD --checksum`, which BuildKit's Dockerfile frontend takes outside its labs channel only
// from BuildKit 0.12, which Docker 25.0 is the first to ship. So builds need Docker 25.0 or later
// with its Buildx plugin, which the engine's own floor, Docker 28.0, as the crate's doc says,
// meets.
//
// The image's layers are zstd's, in an OCI manifest, which a zstd layer needs: BuildKit compresses
// with zstd several times faster than with gzip, its default, and engines unpack it faster too.
// `force-compression` gives the base's layers zstd too, so that every layer of an image unpacks as
// fast, wherever it runs: Docker pulls one from its 23.0, which a provider that pulls images onto a
// Docker of its own requires. So a base's layers are no longer its registry's: the engine
// compresses them again, once per base until its build cache is pruned, and their digests follow
// the version of its zstd encoder. The image exporter, given no name, keeps the image in the engine
// untagged, as `docker build` does without `--output`.
const OPTIONS: [&str; 2] = [
    "--provenance=false",
    "--output=type=image,compression=zstd,oci-mediatypes=true,force-compression=true",
];

/// The options every build gives `docker build`, as [`OPTIONS`] says.
fn options() -> impl Iterator<Item = String> {
    let time = format!("--build-arg=SOURCE_DATE_EPOCH={}", BUILD_TIME.as_secs());
    (OPTIONS.into_iter().map(str::to_owned)).chain([time])
}

/// An image that [`Engine::build`] built or found, which only it makes, so that a build may be
/// [`Given`] it as a named context.
#[derive(Debug)]
pub struct Built {
    image: Image,
    tag: String,
}

impl Built {
    /// The image's ID, the digest of its manifest, by which a workflow names the image.
    pub fn image(&self) -> &Image {
        &self.image
    }

    /// The tag `build` found the image by or gave it, `<repository>:<hex>`, by which, with its
    /// ID, a build on it names it to the engine, until another build of the same inputs moves it
    /// to its own image.
    /// Its digest names the build's inputs, not the image: a key that finds what they built
    /// before, as two builds of the same inputs may make different images.
    pub fn tag(&self) -> &str {
        &self.tag
    }
}

impl AsRef<Image> for Built {
    /// [`Built::image`].
    fn as_ref(&self) -> &Image {
        &self.image
    }
}

/// A build's context, which [`Engine::build`] copies to a stage of the build's own if it finds
/// no image by its tag.
///
/// It holds what `dir` holds, or nothing without one, and each directory of `mounts` at its
/// path, which must name nothing that `dir` or an earlier mount holds, in a directory that one
/// of them holds. It holds only directories and regular files, named in UTF-8 without
/// whitespace or a control character. Its stage gives each directory mode 0755, and each file
/// 0755 if its owner may execute it, else 0644, and each of them one time, the build's, as the
/// time it was last modified.
#[derive(Debug, Default)]
pub struct BuildContext {
    pub dir: Option<PathBuf>,
    pub mounts: BTreeMap<RelativePath, PathBuf>,
}

/// The mount of `mounts`, a [`BuildContext`]'s, that holds what is at `path` in the context.
///
/// It is the deepest of the mounts whose paths `path` is below, given with the rest of `path`
/// below it; or `None` if there is none, as the context's own directory then holds it. This is
/// how [`Engine::build`] lays the context out (`read_context`): its own directory, each mount
/// laid over it, and a mount below another laid over what that one holds.
pub fn mounted<'m, 'p, D>(
    mounts: &'m BTreeMap<RelativePath, D>,
    path: &'p str,
) -> Option<(&'m D, &'p str)> {
    let below = mounts.iter().filter_map(|(at, dir)| {
        let within = path.strip_prefix(at.as_str())?.strip_prefix('/')?;
        Some((dir, within))
    });
    below.min_by_key(|(_, within)| within.len())
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
    /// The image that `recipe` builds with `context`, given `given`, tagged `<repository>:<hex>`,
    /// as [`Engine::start`] starts its build, once that build has ended ([`finish`]).
    ///
    /// # Errors
    ///
    /// As [`Engine::start`] and [`finish`] fail.
    pub fn build(
        &self,
        context: &BuildContext,
        recipe: &Recipe,
        given: &Given<&Built>,
        repository: &str,
    ) -> Result<Built, String> {
        match self.start(context, recipe, given, repository)? {
            Started::Found(built) => Ok(built),
            Started::Building(building) => finish(&mut Vec::from([((), building)])).1,
        }
    }

    /// Starts the build of the image that `recipe`, once [`recipe::check`] passes it, builds with
    /// `context` and `given`, tagged `<repository>:<hex>`, the digest of the build's inputs: the
    /// engine's image under that tag, or a `docker build` of a stage of the context, to be tagged
    /// once it has ended ([`finish`]).
    ///
    /// # Errors
    ///
    /// Fails as [`recipe::check`] does, if `context` isn't as [`BuildContext`] says, if a
    /// [`Recipe::File`] is no regular file of it, or if `docker` or the stage fails.
    pub fn start(
        &self,
        context: &BuildContext,
        recipe: &Recipe,
        given: &Given<&Built>,
        repository: &str,
    ) -> Result<Started, String> {
        // An image found by the tag was built from a stage whose inputs are those the context's
        // own files give now, so only a build needs a stage.
        let inputs = Inputs::checked(read_context(context)?, None, recipe, given)?;
        let tag = format!("{repository}:{}", inputs.digest());
        if let Some(built) = self.tagged(&tag)? {
            return Ok(Started::Found(built));
        }
        let scratch = make_scratch(&self.dir, "build")?;
        match self.start_in(scratch.path(), &inputs, recipe, given, repository) {
            Ok((child, recipe, tag)) => Ok(Started::Building(Building {
                child,
                scratch,
                recipe,
                tag,
                context: self.report.context.clone(),
            })),
            Err(error) => removed(scratch, "build", Err(error)),
        }
    }

    /// The image that the engine has under `tag`, if any.
    fn tagged(&self, tag: &str) -> Result<Option<Built>, String> {
        let found = image_id(&self.report.context, tag).map_err(|error| {
            let error = fix_reported(&error);
            format!("the image tagged `{tag}` can't be looked up: {error}")
        })?;
        let Some(id) = found else {
            return Ok(None);
        };
        let image = Image::try_from(id)?;
        Ok(Some(Built {
            image,
            tag: tag.to_owned(),
        }))
    }

    /// Starts the `docker build` of [`Engine::start`] that `recipe` builds from a stage of
    /// `inputs`, in `scratch`, an empty directory of the build's own, which also holds a text
    /// recipe, the file the build writes the image's ID to, and the build's `output`; and gives
    /// it with the recipe, as a failure names it, and the tag of the stage's inputs.
    fn start_in(
        &self,
        scratch: &Path,
        inputs: &Inputs,
        recipe: &Recipe,
        given: &Given<&Built>,
        repository: &str,
    ) -> Result<(Child, String, String), String> {
        let staging = scratch.join("context");
        // The stage is this build's alone, so the bytes the check reads there are those the
        // digest covers and the build reads, which may differ from those `inputs` read.
        let staged = Inputs::checked(inputs.stage(&staging)?, Some(&staging), recipe, given)?;
        let digest = staged.digest();
        // Named by its digest: Buildx keys what it sends of a build's directories, its context's
        // and its recipe's, by the name of the context's directory alone, and the machine, and
        // BuildKit skips each file it sent under that key before whose size, mode and time are as
        // they were.
        // Every staged file has the same time, so no two stages of other files may share that
        // name, as `reproducible`, a test of a file changed to another of the same size, checks.
        let stage = scratch.join(&digest);
        fs::rename(&staging, &stage).context(format!(
            "{} to {}",
            staging.display(),
            stage.display()
        ))?;
        // Once renamed, which may change its time.
        set_build_time(&stage)?;
        let (file, shown) = match recipe {
            Recipe::File(path) => (stage.join(path.as_str()), format!("`{path}`")),
            Recipe::Text(text) => {
                let file = scratch.join("Dockerfile");
                fs::write(&file, text).context(file.display())?;
                (file, "its recipe's text".to_owned())
            }
        };
        let child = start_untagged(self, &stage, &file, given, scratch)?;
        Ok((child, shown, format!("{repository}:{digest}")))
    }
}

/// What [`Engine::start`] gives: the image found by its tag, or its build, started.
#[derive(Debug)]
pub enum Started {
    /// The image the engine has under the tag of the build's inputs.
    Found(Built),
    /// Its `docker build`, running.
    Building(Building),
}

/// A `docker build` that [`Engine::start`] has started, and [`finish`] waits for.
#[derive(Debug)]
pub struct Building {
    child: Child,
    /// The build's directory of its own, removed once it has ended, which holds its `output`,
    /// its standard output and error.
    scratch: TempDir,
    /// The recipe, as a failure names it.
    recipe: String,
    tag: String,
    context: ContextName,
}

/// Waits until one of `building` has ended, takes it out, and gives its key with its image.
///
/// The image is tagged as [`Engine::start`] says, once its directory is removed. A failed
/// `docker build` has all it wrote put on standard error at once, as `make --output-sync` does.
///
/// # Errors
///
/// Gives, with that key, why: `docker build`, or its waiting, output, image's ID, tag or
/// directory's removal failed. Panics if `building` is empty, as none of it would ever end.
pub fn finish<K>(building: &mut Vec<(K, Building)>) -> (K, Result<Built, String>) {
    let mut children: Vec<&mut Child> = (building.iter_mut()).map(|(_, b)| &mut b.child).collect();
    let (index, status) = stdx::process::wait_any(&mut children);
    let (key, mut ended) = building.remove(index);
    let built = match status {
        // A build that succeeds says nothing that its image doesn't: only a failure's output
        // tells the operator something to act on.
        Ok(status) if status.success() => ended.built(),
        Ok(status) => ended.shown().and_then(|()| {
            Err(format!(
                "`docker build` of {} failed with {status}: its output above says why: fix what \
                 Docker reports",
                ended.recipe
            ))
        }),
        Err(error) => {
            // Killed, and reaped, so that it stops using its directory before it is removed.
            let _ = ended.child.kill().and_then(|()| ended.child.wait());
            Err(format!(
                "`docker build` of {} can't be waited for, so it is killed: {error}",
                ended.recipe
            ))
        }
    };
    (key, removed(ended.scratch, "build", built))
}

impl Building {
    /// Writes all of the build's output to this process's standard error, whole.
    fn shown(&self) -> Result<(), String> {
        let output = self.scratch.path().join("output");
        let mut output = File::open(&output).context(output.display())?;
        let mut stderr = io::stderr().lock();
        // A standard error that can't be written leaves the output unshown, as `pumpkin` leaves
        // whatever else it says, and the build's outcome still stands.
        let _ = io::copy(&mut output, &mut stderr).and_then(|_| stderr.flush());
        Ok(())
    }

    /// The image of this build, which has succeeded, tagged.
    fn built(&self) -> Result<Built, String> {
        let id_file = self.scratch.path().join("id");
        let id = fs::read_to_string(&id_file).map_err(|error| {
            format!(
                "the ID of the image `docker build` built can't be read from {}: {error}: the \
                 build should have written it there",
                id_file.display()
            )
        })?;
        // In the containerd image store, which `Engine::connect` requires, the ID of an image
        // built for the engine's platform alone, with no attestation (`OPTIONS`), is its
        // manifest's digest.
        let image = Image::try_from(id.trim().to_owned())?;
        let tag = &self.tag;
        // The tag moves only if an image of the same inputs has it: one that another build
        // tagged since the lookup, or one of what the context's files changed to since they were
        // read. And it is `docker tag`'s, not the build's: in the containerd image store,
        // a tag that a build moves to its image drops the image the tag named, which a run may
        // still be using, whereas one that `docker tag` moves leaves that image, untagged.
        let tagged = docker(&self.context, ["tag", "--", image.as_str(), tag]);
        tagged.map_err(|error| {
            format!("{image} can't be tagged `{tag}`: {}", fix_reported(&error))
        })?;
        Ok(Built {
            image,
            tag: tag.clone(),
        })
    }
}

/// A directory of its own in `dir` for one `task`, made with bits 0700, making `dir` if it is
/// missing: named by `task`, this process and a random word, so that each that a process that
/// has ended made is removed then ([`sweep`]).
// A process killed during its task leaves this directory, with what the task put there, such as
// a build's stage, a copy of the whole context, behind, until another process makes one.
fn make_scratch(dir: &Path, task: &str) -> Result<TempDir, String> {
    fs::create_dir_all(dir).context(dir.display())?;
    let scratch = tempfile::Builder::new()
        .prefix(&format!("pumpkin-{task}-{}-", std::process::id()))
        .permissions(Permissions::from_mode(0o700))
        .tempdir_in(dir)
        .map_err(|error| {
            format!(
                "a directory for the {task} can't be made in {}: {error}: let this user make one \
                 there",
                dir.display()
            )
        })?;
    sweep(scratch.path());
    Ok(scratch)
}

/// Removes each directory beside `ours`, one that [`make_scratch`] has just made, that it made
/// for a process that has ended, as this process's user: one that a process killed during its
/// task left behind.
fn sweep(ours: &Path) {
    let (Some(dir), Ok(made)) = (ours.parent(), fs::symlink_metadata(ours)) else {
        return;
    };
    // What can't be read or removed is left as it was before the sweep: it is no task's of this
    // process, which has no use for it.
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(scratch_pid) else {
            continue;
        };
        // A process that has ended has no directory in `/proc`, of the PID namespace this
        // process is in, which every `pumpkin` that shares this directory is in too.
        let ended = !Path::new("/proc").join(pid.to_string()).exists();
        let path = entry.path();
        let theirs = fs::symlink_metadata(&path)
            .is_ok_and(|found| found.is_dir() && found.uid() == made.uid());
        if ended && theirs {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

/// The ID of the process that [`make_scratch`] made the directory named `name` for, if it made
/// it: `pumpkin-<task>-<pid>-<word>`.
fn scratch_pid(name: &str) -> Option<u32> {
    let mut words = name.rsplitn(3, '-');
    let (word, pid, task) = (words.next()?, words.next()?, words.next()?);
    let task = task.strip_prefix("pumpkin-")?;
    let named = !task.is_empty() && task.bytes().all(|b| b.is_ascii_lowercase());
    let digits = !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit());
    (named && digits && !word.is_empty()).then(|| pid.parse().ok())?
}

/// `done`, what `task` gave in `scratch`, once `scratch` is removed.
fn removed<T>(scratch: TempDir, task: &str, done: Result<T, String>) -> Result<T, String> {
    let path = scratch.path().to_owned();
    let left = |removing: io::Error| {
        format!(
            "{} can't be removed after the {task}: {removing}: remove it",
            path.display()
        )
    };
    match (done, scratch.close()) {
        (done, Ok(())) => done,
        (Ok(_), Err(removing)) => Err(left(removing)),
        (Err(error), Err(removing)) => Err(format!("{error}; and {}", left(removing))),
    }
}

/// Starts `docker build` of `recipe`, a file, with `stage` as its context, given `given`, on
/// `engine`, to name the image it builds with no tag, and to write its ID to `id` in `scratch`,
/// and what it outputs to `output` there.
fn start_untagged(
    engine: &Engine,
    stage: &Path,
    recipe: &Path,
    given: &Given<&Built>,
    scratch: &Path,
) -> Result<Child, String> {
    let Given {
        target,
        args,
        contexts,
    } = given;
    let mut flags: Vec<String> = Vec::new();
    if let Some(target) = target {
        flags.extend(["--target".to_owned(), target.to_string()]);
    }
    for (name, value) in args {
        flags.extend(["--build-arg".to_owned(), format!("{name}={value}")]);
    }
    // Each an image's named context, which BuildKit resolves on the engine as it does a `FROM`.
    // By its digest as well as its tag, so that the engine finds that image even if the tag has
    // moved, and anything else it fetches, as from Docker Hub, is the same image.
    for (name, built) in contexts {
        let named = format!(
            "{name}=docker-image://{}@{}",
            built.tag(),
            built.image().as_str()
        );
        flags.extend(["--build-context".to_owned(), named]);
    }
    // `docker build` has no `--pull=never`: it pulls a base the engine lacks, by the reference
    // its recipe names, which pins it by digest.
    let id_file = scratch.join("id");
    // BuildKit's plain progress, as it writes it to a file, and not to a terminal, either way;
    // it shapes the output alone, not the image, so it is not among the inputs' `OPTIONS`.
    let options: Vec<String> = options().collect();
    let build = (["build", "--progress=plain"].into_iter().map(OsStr::new))
        .chain(options.iter().map(OsStr::new))
        .chain(flags.iter().map(OsStr::new))
        .chain([
            OsStr::new("--file"),
            recipe.as_os_str(),
            OsStr::new("--iidfile"),
            id_file.as_os_str(),
            OsStr::new("--"),
            stage.as_os_str(),
        ]);
    let output = scratch.join("output");
    let file = File::create(&output).context(output.display())?;
    let errors = file.try_clone().context(output.display())?;
    let mut command = docker_command(&engine.report.context, build);
    // Buildx keeps a state of its own, the builds' references and this machine's ID among it, in
    // the engine's directory, rather than in Docker's configuration.
    command.env("BUILDX_CONFIG", engine.dir.join("buildx"));
    // In this process's group, so that the terminal's interrupt reaches it too: a session that
    // BuildKit cancels once its client is gone.
    let started = (command.stdin(Stdio::null()).stdout(file).stderr(errors)).spawn();
    started.map_err(|error| {
        format!("`docker build` didn't start: {error}: install Docker's command line on `PATH`")
    })
}

/// A build's inputs, as the [module](self)'s doc lists them.
struct Inputs {
    /// The recipe's line.
    recipe: String,
    /// The lines of what the build is given.
    given: Vec<String>,
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
    /// it was read from, with its mode, and each entry below `stage` last modified at
    /// [`BUILD_TIME`]; and returns the entries that the stage holds.
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
        // Once all is made, as making an entry changes the time of the directory it is in.
        for path in entries.keys() {
            set_build_time(&stage.join(path))?;
        }
        Ok(entries)
    }

    /// The inputs with `entries`, given `given`, once [`recipe::check`] has passed them and
    /// `recipe`'s bytes: a [`Recipe::File`]'s as read from `stage`, if given, or else from the
    /// path it was found at, and then named by its entry.
    fn checked(
        mut entries: BTreeMap<String, Entry>,
        stage: Option<&Path>,
        recipe: &Recipe,
        given: &Given<&Built>,
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
                recipe::check(&from.display().to_string(), &text, given)?;
                // The digest covers the bytes the check passed, even if the file has changed
                // since it was hashed.
                *sha256 = hex(Sha256::digest(&text).into());
                format!("file {path}")
            }
            Recipe::Text(text) => {
                // Named as Docker names a recipe by default.
                recipe::check("Dockerfile", text.as_bytes(), given)?;
                format!("text {}", hex(Sha256::digest(text).into()))
            }
        };
        let Given {
            target,
            args,
            contexts,
        } = given;
        // No line holds a line feed: the check passes only a target that names a stage, and
        // build arguments and named contexts with names of the characters it says; and a tag
        // holds none.
        let target = target.iter().map(|target| format!("target {target}"));
        let args = (args.iter())
            .map(|(name, value)| format!("arg {name} {}", hex(Sha256::digest(value).into())));
        let contexts =
            (contexts.iter()).map(|(name, built)| format!("context {name} {}", built.tag()));
        let lines = target.chain(args).chain(contexts).collect();
        Ok(Self {
            recipe,
            given: lines,
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
        writeln!(f, "{FORM} {}", options().collect::<Vec<_>>().join(" "))?;
        writeln!(f, "{}", self.recipe)?;
        for line in &self.given {
            writeln!(f, "{line}")?;
        }
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

/// Sets the time the file or directory at `path` was last modified to [`BUILD_TIME`].
fn set_build_time(path: &Path) -> Result<(), String> {
    (File::open(path))
        .and_then(|file| file.set_modified(UNIX_EPOCH + BUILD_TIME))
        .context(path.display())
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
    Sha256Digest::from(sha256).hex().to_owned()
}
