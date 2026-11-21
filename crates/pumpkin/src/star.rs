//! A task's source, read below its root, and the standard library's, as the language's loader and
//! the builds read them.
//!
//! The root is the nearest directory at or above the entry that holds [`MARKER`], a file of
//! comments alone, as this `pumpkin` reads nothing else there. Each path that a task names is
//! looked up below the root with no symbolic link on the way, and each one found is kept, as what
//! the task reaches. A `@pumpkin//` path is a file or a directory of the standard library, which
//! this `pumpkin` holds: a build is given one of its directories as files written out
//! ([`Library`]). Evaluating is `lang`'s and building is `engine`'s: this module gives them the
//! files, and says each task's problems.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::{fmt, fs, io, iter, thread};

use engine::{BuildContext, Building, Built, Engine, Started};
use lang::{Args, Declared, Label, Loader, Plan, Problems, Recipe};
use recipe::Given;
use schema::ir::Image;
use schema::workflow::Workflow;
use tempfile::TempDir;

use crate::bodies::{self, Keeping, Sources};
use crate::say;
use crate::style::Formatter;
use crate::workspace::Workspace;

/// The file at a task's root, where its `//` labels start.
pub(crate) const MARKER: &str = "PUMPKIN.star";

#[derive(Clone)]
/// A task's entry file, as the operator named it: a path whose name ends in `.star`.
pub(crate) struct Entry(String);

impl Entry {
    pub(crate) fn parse(path: String) -> Result<Self, String> {
        let star = Path::new(&path)
            .file_name()
            .is_some_and(|name| name.as_bytes().ends_with(b".star"));
        if !star {
            return Err(format!(
                "`{path}` names no `.star` file: name a task's entry file, whose name ends in \
                 `.star`"
            ));
        }
        Ok(Self(path))
    }

    pub(crate) fn path(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

struct Root {
    dir: PathBuf,
    reached: BTreeSet<String>,
}

#[derive(Clone, Copy)]
enum Kind {
    File,
    Directory,
}

impl Kind {
    fn noun(self) -> &'static str {
        match self {
            Self::File => "regular file",
            Self::Directory => "directory",
        }
    }
}

impl Root {
    fn find(entry: &Entry) -> Result<(Self, Label), String> {
        let path = Path::new(entry.path());
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            unreachable!("`Entry::parse` makes an entry only of a path that names a file")
        };
        let given = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        let dir = fs::canonicalize(given).map_err(|error| {
            format!(
                "the entry's directory {} can't be found ({error}): name the entry by a path \
                 that exists",
                given.display()
            )
        })?;
        let mut below = vec![name];
        let mut at = dir.as_path();
        loop {
            let marker = at.join(MARKER);
            // A directory of the marker's name marks nothing; anything else marks the root, and is
            // refused there unless it is the file.
            match fs::symlink_metadata(&marker) {
                Ok(metadata) => {
                    if !metadata.is_dir() {
                        break;
                    }
                }
                Err(error) => {
                    if error.kind() != io::ErrorKind::NotFound {
                        return Err(format!("{} can't be looked up: {error}", marker.display()));
                    }
                }
            }
            let (Some(parent), Some(component)) = (at.parent(), at.file_name()) else {
                return Err(format!(
                    "no directory at or above {} holds `{MARKER}`: put one at the root of the \
                     task's repository, where its `//` labels start",
                    dir.display()
                ));
            };
            below.push(component);
            at = parent;
        }
        let mut root = Self {
            dir: at.to_owned(),
            reached: BTreeSet::new(),
        };
        // A component that isn't UTF-8 is no label's, as the label's parse then says.
        let below: Vec<_> = below
            .iter()
            .rev()
            .map(|part| part.to_string_lossy())
            .collect();
        let label = Label::parse(&format!("//{}", below.join("/"))).map_err(|error| {
            format!("the entry, below the root {}: {error}", root.dir.display())
        })?;
        let place = root.dir.join(MARKER);
        let marker = root.text(MARKER);
        let marker = marker.map_err(|error| format!("{}: {error}", place.display()))?;
        comments_only(&marker).map_err(|problem| format!("{}:{problem}", place.display()))?;
        Ok((root, label))
    }

    fn reach(&mut self, below: &str, kind: Kind) -> Result<PathBuf, String> {
        let mut path = self.dir.clone();
        let mut reached = String::new();
        let mut components = below.split('/').peekable();
        while let Some(component) = components.next() {
            path.push(component);
            if !reached.is_empty() {
                reached.push('/');
            }
            reached.push_str(component);
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let noun = kind.noun();
                    return Err(format!(
                        "`{below}` doesn't exist: name a {noun} below the root that does"
                    ));
                }
                Err(error) => return Err(format!("`{reached}` can't be looked up: {error}")),
            };
            if metadata.is_symlink() {
                return Err(format!(
                    "`{reached}` is a symbolic link, which could lead out of the root: put what \
                     it links to there instead"
                ));
            }
            let named = match kind {
                Kind::File => metadata.is_file(),
                Kind::Directory => metadata.is_dir(),
            };
            if components.peek().is_none() && !named {
                let noun = kind.noun();
                return Err(format!(
                    "`{below}` is no {noun}: name a {noun} below the root"
                ));
            }
        }
        self.reached.insert(below.to_owned());
        Ok(path)
    }

    fn bytes(&mut self, below: &str) -> Result<(PathBuf, Vec<u8>), String> {
        let path = self.reach(below, Kind::File)?;
        let bytes = fs::read(&path).map_err(|error| format!("`{below}` can't be read: {error}"))?;
        Ok((path, bytes))
    }

    fn text(&mut self, below: &str) -> Result<String, String> {
        let (_, bytes) = self.bytes(below)?;
        String::from_utf8(bytes)
            .map_err(|error| format!("`{below}` isn't UTF-8 ({error}): write it in UTF-8"))
    }

    fn recipe(&mut self, declared: &Recipe, contexts: &[&str]) -> Vec<String> {
        let Recipe {
            context,
            recipe: _,
            mounts,
            target,
            args,
            repository: _,
        } = declared;
        let mut problems = Vec::new();
        let mut reached = Vec::new();
        for dir in iter::once(context).chain(mounts.values()) {
            let found = match dir.root() {
                lang::Root::Task => self.reach(dir.path(), Kind::Directory).map(|_| ()),
                lang::Root::Library if library_dir(dir.path()) => Ok(()),
                lang::Root::Library => Err(not_in_library(dir, Kind::Directory)),
            };
            match found {
                Ok(()) => reached.push(dir),
                Err(problem) => problems.push(problem),
            }
        }
        let (dir, file) = Self::located(declared);
        // A recipe in a directory that wasn't found has that problem already.
        if reached.contains(&dir) {
            let given = Given {
                target: target.clone(),
                args: args.clone(),
                contexts: (contexts.iter())
                    .map(|name| ((*name).to_owned(), ()))
                    .collect(),
            };
            let text = match file.root {
                lang::Root::Task => self.bytes(&file.path).map(|(_, text)| text),
                lang::Root::Library => (library_file(&file.path).map(<[u8]>::to_vec))
                    .ok_or_else(|| not_in_library(&file, Kind::File)),
            };
            let checked = text.and_then(|text| recipe::check(&file.to_string(), &text, &given));
            problems.extend(checked.err());
        }
        problems
    }

    fn start(
        &self,
        engine: &Engine,
        declared: &Declared<'_, Built>,
        admit: &mut dyn engine::Admit,
    ) -> Result<Started, String> {
        let Declared {
            recipe,
            ref contexts,
        } = *declared;
        // Held while the engine reads the context, which it copies before it starts.
        let (context, given, _library) = self.inputs(recipe, contexts)?;
        let file = engine::Recipe::File(recipe.recipe.clone());
        engine.start(&context, &file, &given, &recipe.repository, admit)
    }

    /// The inputs of a build of `recipe`. The `Library` holds the standard library's files that
    /// the context reads: keep it until the context has been read.
    fn inputs<'b>(
        &self,
        recipe: &Recipe,
        contexts: &BTreeMap<&str, &'b Built>,
    ) -> Result<(BuildContext, Given<&'b Built>, Option<Library>), String> {
        let Recipe {
            context,
            recipe: _,
            mounts,
            target,
            args,
            repository: _,
        } = recipe;
        let library: Vec<&str> = (iter::once(context).chain(mounts.values()))
            .filter(|dir| dir.root() == lang::Root::Library)
            .map(Label::path)
            .collect();
        let library = if library.is_empty() {
            None
        } else {
            Some(Library::write(&library)?)
        };
        let dir = |label: &Label| match (label.root(), &library) {
            (lang::Root::Task, _) => self.dir.join(label.path()),
            (lang::Root::Library, Some(library)) => library.0.path().join(label.path()),
            (lang::Root::Library, None) => unreachable!(
                "`{label}` is a build's directory of the standard library, which is written out \
                 for any build that has one"
            ),
        };
        let context = BuildContext {
            dir: Some(dir(context)),
            mounts: (mounts.iter())
                .map(|(at, label)| (at.clone(), dir(label)))
                .collect(),
        };
        let given = Given {
            target: target.clone(),
            args: args.clone(),
            contexts: named(contexts),
        };
        Ok((context, given, library))
    }

    fn located(recipe: &Recipe) -> (&Label, Placed) {
        let mounted = engine::mounted(&recipe.mounts, recipe.recipe.as_str());
        let (dir, within) = mounted.unwrap_or((&recipe.context, recipe.recipe.as_str()));
        let path = match dir.path() {
            "" => within.to_owned(),
            path => format!("{path}/{within}"),
        };
        let file = Placed {
            root: dir.root(),
            path,
        };
        (dir, file)
    }
}

fn named<'b>(contexts: &BTreeMap<&str, &'b Built>) -> BTreeMap<String, &'b Built> {
    (contexts.iter())
        .map(|(name, built)| ((*name).to_owned(), *built))
        .collect()
}

impl Loader for Root {
    fn load(&mut self, label: &Label) -> Result<String, String> {
        match label.root() {
            lang::Root::Task => self.text(label.path()),
            lang::Root::Library => {
                let bytes = library_file(label.path());
                let bytes = bytes.ok_or_else(|| not_in_library(label, Kind::File))?;
                String::from_utf8(bytes.to_vec())
                    .map_err(|error| format!("`{label}` isn't UTF-8 ({error})"))
            }
        }
    }
}

#[derive(PartialEq, Eq)]
struct Placed {
    root: lang::Root,
    path: String,
}

impl fmt::Display for Placed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.root {
            lang::Root::Task => self.path.fmt(f),
            lang::Root::Library => write!(f, "@pumpkin//{}", self.path),
        }
    }
}

fn library_file(path: &str) -> Option<&'static [u8]> {
    let found = library::FILES.iter().find(|(file, _)| *file == path);
    found.map(|(_, bytes)| *bytes)
}

fn library_dir(dir: &str) -> bool {
    (library::FILES.iter()).any(|(path, _)| below_dir(path, dir))
}

fn below_dir(path: &str, dir: &str) -> bool {
    (path.strip_prefix(dir)).is_some_and(|rest| rest.starts_with('/'))
}

fn not_in_library(place: &impl fmt::Display, kind: Kind) -> String {
    let noun = kind.noun();
    format!("`{place}` names no {noun} of the standard library: name one that it holds")
}

/// Files of the standard library written out for a build, and removed when this is dropped.
struct Library(TempDir);

impl Library {
    fn write(dirs: &[&str]) -> Result<Self, String> {
        let dir = Workspace::find()?.scratch("pumpkin-library-")?;
        let below = |path: &str| (dirs.iter()).any(|dir| below_dir(path, dir));
        for (path, bytes) in library::FILES.iter().filter(|(path, _)| below(path)) {
            let file = dir.path().join(path);
            let written = (file.parent())
                .map_or(Ok(()), fs::create_dir_all)
                .and_then(|()| fs::write(&file, bytes));
            written.map_err(|error| format!("{} can't be written: {error}", file.display()))?;
        }
        Ok(Self(dir))
    }
}

fn comments_only(text: &str) -> Result<(), String> {
    for (number, line) in (1..).zip(text.split('\n')) {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let content = line.trim_start_matches([' ', '\t']);
        if content.is_empty() || content.starts_with('#') {
            continue;
        }
        let column = line.chars().count() - content.chars().count() + 1;
        return Err(format!(
            "{number}:{column}: this pumpkin reads only comments in `{MARKER}`: the repository \
             may need a newer pumpkin"
        ));
    }
    Ok(())
}

/// A checked task's root, each path below it that checking it reached, which is all that
/// building it reads, and its workflow.
pub(crate) struct Reach {
    pub(crate) root: PathBuf,
    pub(crate) paths: BTreeSet<String>,
    pub(crate) workflow: Workflow,
}

/// What checking `entry`'s task with `args` reaches. The outcome is as `checked`'s.
pub(crate) fn reached(entry: &Entry, args: &Args) -> Result<Result<Reach, Problems>, String> {
    checked(entry, args, |Task { root, workflow, .. }| {
        Ok(Ok(Reach {
            root: root.dir.clone(),
            paths: root.reached.clone(),
            workflow,
        }))
    })
}

/// Checks the task as `checked` does, then lints each of its files but the library's, by
/// `formatter`.
pub(crate) fn check(
    entry: &Entry,
    args: &Args,
    formatter: &Formatter,
) -> Result<Result<(), Problems>, String> {
    checked(entry, args, |Task { plan, .. }| {
        plan.lint(|label, text| {
            // The standard library is checked with this `pumpkin`, and no task can change it.
            if label.root() == lang::Root::Library {
                return Ok(Vec::new());
            }
            let linted = formatter.lint(text);
            linted.map_err(|error| format!("linting `{}` failed: {error}", label.path()))
        })
    })
}

/// An [`engine::Admit`] that is told when a build ends while others are still running.
pub(crate) trait Admission: engine::Admit {
    fn ended(&mut self, engine: &Engine);
}

impl Admission for () {
    fn ended(&mut self, _: &Engine) {}
}

/// The tag of each image that a task's build made.
pub(crate) type Tags = BTreeMap<Image, String>;

/// A task that evaluated and checked, whose files are those of the root that `checked` read.
pub(crate) struct Task<'r> {
    root: &'r mut Root,
    plan: Plan,
    workflow: Workflow,
    /// What the evaluation read, from which each of the workflow's bodies is evaluated.
    sources: Sources,
}

impl Task<'_> {
    pub(crate) fn workflow(&self) -> &Workflow {
        &self.workflow
    }

    /// Builds the task's images on `engine`, each build as `admission` admits it, and gives the
    /// workflow with its images built, the tag of each, and the sources of its bodies.
    pub(crate) fn build(
        self,
        engine: &Engine,
        admission: &mut (dyn Admission + Send),
    ) -> Result<(Workflow, Tags, Sources), Problems> {
        let Self {
            root,
            plan,
            workflow: _,
            sources,
        } = self;
        let mut builds = Builds::new(admission);
        let built = plan.build(jobs(), |ready| builds.step(&*root, engine, ready))?;
        Ok((built, builds.tags, sources))
    }
}

struct Builds<'a> {
    building: Vec<((usize, String), Building)>,
    said: BTreeSet<String>,
    admission: &'a mut (dyn Admission + Send),
    tags: Tags,
}

impl<'a> Builds<'a> {
    fn new(admission: &'a mut (dyn Admission + Send)) -> Self {
        Self {
            building: Vec::new(),
            said: BTreeSet::new(),
            admission,
            tags: Tags::new(),
        }
    }

    fn step(
        &mut self,
        root: &Root,
        engine: &Engine,
        ready: Vec<(usize, Declared<'_, Built>)>,
    ) -> Vec<(usize, Result<Built, String>)> {
        let mut ended = Vec::new();
        for (index, declared) in ready {
            match root.start(engine, &declared, &mut *self.admission) {
                Ok(Started::Found(built)) => ended.push((index, Ok(built))),
                Ok(Started::Building(started)) => {
                    let (repository, stage) = image(&declared);
                    if self.said.insert(repository.to_owned()) {
                        say(format_args!("building an image in {repository}"));
                    }
                    self.building
                        .push(((index, format!("{repository}{stage}")), started));
                }
                Err(why) => ended.push((index, Err(why))),
            }
        }
        // Wait for a build only when no start has ended, as nothing else can go on.
        if ended.is_empty() && !self.building.is_empty() {
            // `finish` writes a failed build's output, so the line that says it failed comes after.
            let ((index, image), built) = engine::finish(&mut self.building);
            match &built {
                Ok(_) => say(format_args!("built an image in {image}")),
                Err(_) => say(format_args!("the build of an image in {image} failed")),
            }
            if !self.building.is_empty() {
                self.admission.ended(engine);
            }
            ended.push((index, built));
        }
        for built in ended.iter().filter_map(|(_, built)| built.as_ref().ok()) {
            self.tags
                .insert(built.image().clone(), built.tag().to_owned());
        }
        ended
    }
}

fn image<'d>(declared: &Declared<'d, Built>) -> (&'d str, String) {
    let recipe = declared.recipe;
    let stage = recipe.target.iter().map(|stage| format!(", stage {stage}"));
    (&recipe.repository, stage.collect())
}

// This machine's, though the builds run on the engine, which may be another's: they are its
// processes that wait on the engine, and hash and stage what it builds.
fn jobs() -> NonZeroUsize {
    thread::available_parallelism().unwrap_or(NonZeroUsize::MIN)
}

/// Evaluates and checks the task of `entry` with `args`, then gives it to `then`.
///
/// The outer error is a failure outside the task, such as a file that can't be read; the inner is
/// the task's own problems, which `then` never sees.
pub(crate) fn checked<T: Send>(
    entry: &Entry,
    args: &Args,
    then: impl FnOnce(Task<'_>) -> Result<Result<T, Problems>, String> + Send,
) -> Result<Result<T, Problems>, String> {
    let (mut root, label) = Root::find(entry)?;
    // The plan stays on the thread it was evaluated on, as what it holds can't be sent to
    // another, so `then` runs there too.
    evaluating(|| {
        let mut keeping = Keeping::new(&mut root);
        let plan = match lang::evaluate(&label, &mut keeping, args, lang::Bounds::WORKFLOW) {
            Ok(plan) => plan,
            Err(problems) => return Ok(Err(problems)),
        };
        let sources = keeping.kept();
        let workflow = match plan.check(|recipe, contexts| root.recipe(recipe, contexts)) {
            Ok(workflow) => workflow,
            Err(problems) => return Ok(Err(problems)),
        };
        match bodies::check(&workflow, sources, args) {
            Ok(sources) => then(Task {
                root: &mut root,
                plan,
                workflow,
                sources,
            }),
            Err(problems) => Ok(Err(problems)),
        }
    })?
}

fn evaluating<T: Send>(evaluate: impl FnOnce() -> T + Send) -> Result<T, String> {
    // A thread of its own for its stack alone: the main thread's is as large as the process's
    // limit makes it, which may be less.
    stdx::on_stack("evaluation", lang::STACK_BYTES, evaluate)
        .map_err(|error| format!("a thread to evaluate the task on can't be started: {error}"))
}
