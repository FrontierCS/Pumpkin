//! A task's source, as the workflow language reads it on this machine: its entry file
//! ([`Entry`]), the root of its repository ([`Root`]), below which `lang` reads each file, and
//! the one path from an entry and its args to the plan that they evaluate to, checked
//! ([`checked`]), which [`check`] takes with no engine, and [`Task::build`] on to the workflow
//! that the plan's images give, built on one.
//!
//! What a label or an image names is a path below the root, as `lang::Label` says, and it stays
//! below the root on disk, as no component on its way from the root may be a symbolic link
//! ([`Root::reach`]).

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::{fmt, fs, io, iter, panic, thread};

use engine::{BuildContext, Building, Built, Engine, Started};
use lang::{Args, Declared, Label, Loader, Plan, Problems, Recipe};
use recipe::fetch::FetchStage;
use recipe::{Given, Stage};
use schema::ir::Image;
use schema::workflow::Workflow;

use crate::say;
use crate::style::Formatter;

/// The file that marks the root of a task's repository, where its `//` labels start.
pub(crate) const MARKER: &str = "PUMPKIN.star";

/// A task's entry file, by a path, in UTF-8, whose last component names a `.star` file, as each
/// file that the workflow language reads does.
#[derive(Clone)]
pub(crate) struct Entry(String);

impl Entry {
    /// The entry file at `path`.
    ///
    /// # Errors
    ///
    /// `path`'s last component names no `.star` file.
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

    /// The entry's path, as given.
    pub(crate) fn path(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Entry {
    /// The entry's path, as given.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The root of a task's repository, by its canonical path: the nearest directory, at or above
/// its entry file's, that holds [`MARKER`] as anything but a directory. The marker is a regular
/// file of comments alone, as this pumpkin reads nothing else there.
struct Root(PathBuf);

/// What a path below the root names.
#[derive(Clone, Copy)]
enum Kind {
    File,
    Directory,
}

impl Kind {
    /// What a path that names this is said to name.
    fn noun(self) -> &'static str {
        match self {
            Self::File => "regular file",
            Self::Directory => "directory",
        }
    }
}

impl Root {
    /// The root of the repository of `entry`, whose marker is as [`Root`] says, and `entry`'s
    /// label.
    ///
    /// # Errors
    ///
    /// Fails if the entry's directory can't be found, if no directory at or above it holds the
    /// marker, if the marker isn't as [`Root`] says, or if the entry's path below the root is no
    /// label.
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
        // The components from the root down to the entry, the entry's own first.
        let mut below = vec![name];
        let mut at = dir.as_path();
        loop {
            let marker = at.join(MARKER);
            // A directory of the marker's name marks nothing, as it can't be the file; anything
            // else marks the root, and is refused there unless it is the file.
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
        let root = Self(at.to_owned());
        // A component that isn't UTF-8 is no label's, as the label's parse then says.
        let below: Vec<_> = below
            .iter()
            .rev()
            .map(|part| part.to_string_lossy())
            .collect();
        let label = Label::parse(&format!("//{}", below.join("/")))
            .map_err(|error| format!("the entry, below the root {}: {error}", root.0.display()))?;
        // Named by its whole path, as the root the entry's labels are below is found, not given.
        let place = root.0.join(MARKER);
        let marker = root.text(MARKER);
        let marker = marker.map_err(|error| format!("{}: {error}", place.display()))?;
        comments_only(&marker).map_err(|problem| format!("{}:{problem}", place.display()))?;
        Ok((root, label))
    }

    /// The path of `below`, `/`-separated components below the root, which names a `kind`, and
    /// none of whose components, from the root down, is a symbolic link, which could lead out of
    /// the root.
    ///
    /// # Errors
    ///
    /// Fails if `below` names nothing or no `kind`, or if one of its components is a link or
    /// can't be looked up.
    fn reach(&self, below: &str, kind: Kind) -> Result<PathBuf, String> {
        let mut path = self.0.clone();
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
        Ok(path)
    }

    /// The bytes of the regular file at `below`, reached as [`Root::reach`] reaches it.
    fn bytes(&self, below: &str) -> Result<Vec<u8>, String> {
        let path = self.reach(below, Kind::File)?;
        fs::read(path).map_err(|error| format!("`{below}` can't be read: {error}"))
    }

    /// The text of the regular file at `below`, reached as [`Root::reach`] reaches it.
    fn text(&self, below: &str) -> Result<String, String> {
        String::from_utf8(self.bytes(below)?)
            .map_err(|error| format!("`{below}` isn't UTF-8 ({error}): write it in UTF-8"))
    }

    /// The problems of `recipe`, an image's, given the named contexts `contexts`, that
    /// `engine::Engine::build` would refuse before building: a context or a mount that is no
    /// directory below the root, as [`Root::reach`] reaches it; and a recipe, in the directory
    /// that `engine::mounted` says holds it, that is no regular file there, or that
    /// `recipe::check` refuses, with its target and args.
    fn recipe(&self, declared: &Recipe, contexts: &[&str]) -> Vec<String> {
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
            match self.reach(dir.path(), Kind::Directory) {
                Ok(_) => reached.push(dir),
                Err(problem) => problems.push(problem),
            }
        }
        let (dir, below) = Self::located(declared);
        // A directory that can't be reached has said why.
        if reached.contains(&dir) {
            let given = Given {
                target: target.clone(),
                args: args.clone(),
                contexts: (contexts.iter())
                    .map(|name| ((*name).to_owned(), ()))
                    .collect(),
            };
            let checked =
                (self.bytes(&below)).and_then(|text| recipe::check(&below, &text, &given));
            problems.extend(checked.err());
        }
        problems
    }

    /// The image that `declared` is, found on `engine`, or its build started there, by
    /// `engine::Engine::start`: a recipe's, with its context and mounts below the root, its target, args and named
    /// contexts; or a layer's, from its recipe's text, given its named context.
    fn start(&self, engine: &Engine, declared: &Declared<'_, Built>) -> Result<Started, String> {
        match *declared {
            Declared::Recipe {
                recipe,
                ref contexts,
            } => {
                let (context, given) = self.inputs(recipe, contexts);
                let file = engine::Recipe::File(recipe.recipe.clone());
                engine.start(&context, &file, &given, &recipe.repository)
            }
            Declared::Layer {
                ref recipe,
                ref contexts,
                repository,
            } => {
                let recipe = engine::Recipe::Text(recipe.clone());
                let given = Given {
                    contexts: named(contexts),
                    ..Given::default()
                };
                engine.start(&BuildContext::default(), &recipe, &given, repository)
            }
        }
    }

    /// The context and what else a build of `recipe` is given, with the named contexts
    /// `contexts`: its context and mounts below the root, its target and its args.
    fn inputs<'b>(
        &self,
        recipe: &Recipe,
        contexts: &BTreeMap<&str, &'b Built>,
    ) -> (BuildContext, Given<&'b Built>) {
        let Recipe {
            context,
            recipe: _,
            mounts,
            target,
            args,
            repository: _,
        } = recipe;
        let dir = |label: &Label| self.0.join(label.path());
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
        (context, given)
    }

    /// The directory that holds `recipe`'s file, as `engine::mounted` says, its context or one
    /// of its mounts, and the file's path below the root.
    fn located(recipe: &Recipe) -> (&Label, String) {
        let mounted = engine::mounted(&recipe.mounts, recipe.recipe.as_str());
        let (dir, within) = mounted.unwrap_or((&recipe.context, recipe.recipe.as_str()));
        let below = match dir.path() {
            "" => within.to_owned(),
            path => format!("{path}/{within}"),
        };
        (dir, below)
    }

    /// The path below the root of the file at `path`, relative to the working directory, as an
    /// entry's is: its directory's canonical path below the root, then its name.
    ///
    /// # Errors
    ///
    /// Fails if its directory can't be found, or isn't below the root, or the path isn't UTF-8.
    fn below_root(&self, path: &str) -> Result<String, String> {
        let given = Path::new(path);
        let (dir, Some(name)) = (given.parent(), given.file_name()) else {
            return Err(format!("`{path}` names no file: name the recipe's file"));
        };
        let dir = dir.filter(|dir| !dir.as_os_str().is_empty());
        let dir = dir.unwrap_or(Path::new("."));
        let dir = fs::canonicalize(dir).map_err(|error| {
            format!("the recipe's directory {} can't be found ({error}): name the recipe by a path that exists", dir.display())
        })?;
        let below = dir.join(name);
        let below = below.strip_prefix(&self.0).map_err(|_| {
            format!(
                "`{path}` isn't below the task's root, {}: name a recipe of the task",
                self.0.display()
            )
        })?;
        match below.to_str() {
            Some(below) => Ok(below.to_owned()),
            None => Err(format!(
                "`{path}` isn't UTF-8 below the root: name one that is"
            )),
        }
    }
}

/// `contexts`, a declared image's named contexts, as a build is given them.
fn named<'b>(contexts: &BTreeMap<&str, &'b Built>) -> BTreeMap<String, &'b Built> {
    (contexts.iter())
        .map(|(name, built)| ((*name).to_owned(), *built))
        .collect()
}

impl Loader for Root {
    fn load(&mut self, label: &Label) -> Result<String, String> {
        self.text(label.path())
    }
}

/// Refuses `text`, the marker's, unless each of its lines holds a comment or nothing after its
/// spaces and tabs, with the line and the column of the first that holds more.
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

/// Checks the task whose entry is `entry`, given `args`, as `pumpkin check --help` says: once it
/// is [`checked`], lints each file that its evaluation read with `formatter`.
///
/// # Errors
///
/// As [`checked`] fails, and as the lint does; and gives what the lint finds, if anything.
pub(crate) fn check(
    entry: &Entry,
    args: &Args,
    formatter: &Formatter,
) -> Result<Result<(), Problems>, String> {
    checked(entry, args, |Task { plan, .. }| {
        plan.lint(|label, text| {
            let linted = formatter.lint(text);
            linted.map_err(|error| format!("linting `{}` failed: {error}", label.path()))
        })
    })
}

/// The tag of each image of a built task, by the image's ID: what its build's inputs name
/// ([`Built::tag`]).
pub(crate) type Tags = BTreeMap<Image, String>;

/// A task, [`checked`], on the thread that evaluated it, which its plan can't leave: its
/// workflow, and how to build its images.
pub(crate) struct Task<'r> {
    root: &'r Root,
    plan: Plan,
    workflow: Workflow,
}

impl Task<'_> {
    /// The task's workflow, as checked, before its images are built.
    pub(crate) fn workflow(&self) -> &Workflow {
        &self.workflow
    }

    /// Builds the task's images on `engine`, as `pumpkin build --help` says ([`Root::start`]),
    /// as many at once as [`jobs`] says, each once those it is built on are, saying each
    /// repository as its first build starts, and each build, after its output, as it ends. Gives
    /// the workflow that names them, and their tags.
    ///
    /// # Errors
    ///
    /// Gives each build that failed, at its place.
    pub(crate) fn build(self, engine: &Engine) -> Result<(Workflow, Tags), Problems> {
        let Self {
            root,
            plan,
            workflow: _,
        } = self;
        let mut builds = Builds::new();
        let built = plan.build(jobs(), |ready| builds.step(root, engine, ready))?;
        Ok((built, builds.tags))
    }
}

/// Writes the fetch stage `stage` of the recipe `recipe` of the task `entry`, given `args`, as
/// `pumpkin lock --help` says: once the task is [`checked`] and the stage read, calls `reach`
/// for the engine, builds the named contexts that its images give the recipe, as [`Task::build`] does,
/// locks the stage (`Engine::lock`) with the images' inputs, then writes the recipe.
///
/// # Errors
///
/// As [`checked`], `reach`, the read and the lock fail; if no image, or two that differ in their
/// inputs, build `recipe`; if it can't be written. Gives the task's problems, or failed builds.
pub(crate) fn lock(
    entry: &Entry,
    args: &Args,
    recipe: &str,
    stage: &Stage,
    reach: impl FnOnce() -> Result<Engine, String> + Send,
) -> Result<Result<(), Problems>, String> {
    checked(entry, args, |Task { root, plan, .. }| {
        let below = root.below_root(recipe)?;
        let picked = |declared: &Recipe| Root::located(declared).1 == below;
        same_inputs(
            entry,
            &below,
            plan.recipes().filter(|(declared, _)| picked(declared)),
        )?;
        let path = root.reach(&below, Kind::File)?;
        let text = root.bytes(&below)?;
        let fetch = FetchStage::read(&below, &text, stage)?;
        let engine = reach()?;
        let mut builds = Builds::new();
        let step = |ready: Vec<(usize, Declared<'_, Built>)>| builds.step(root, &engine, ready);
        let written = plan.build_for(
            jobs(),
            picked,
            step,
            |declared: Vec<Declared<'_, Built>>| {
                let Some(Declared::Recipe { recipe, contexts }) = declared.first() else {
                    unreachable!(
                        "an image of the recipe was found, and only recipes' images are picked"
                    )
                };
                let (context, given) = root.inputs(recipe, contexts);
                engine.lock(&fetch, &context, given)
            },
        );
        let written = match written {
            Ok(written) => written?,
            Err(problems) => return Ok(Err(problems)),
        };
        fs::write(&path, written)
            .map_err(|error| format!("`{below}` can't be written: {error}"))?;
        Ok(Ok(()))
    })
}

/// Nothing if each of `picked`, the task's images of the recipe at `below` below the root, with
/// their named contexts, gives it the same inputs, but for their targets: its context, mounts,
/// args and named contexts.
///
/// # Errors
///
/// Fails if `picked` is empty, or two of them give the recipe different inputs.
fn same_inputs<'p>(
    entry: &Entry,
    below: &str,
    mut picked: impl Iterator<Item = (&'p Recipe, BTreeMap<&'p str, usize>)>,
) -> Result<(), String> {
    let inputs = |(recipe, contexts): (&'p Recipe, BTreeMap<&'p str, usize>)| {
        let Recipe {
            context,
            recipe: _,
            mounts,
            target: _,
            args,
            repository: _,
        } = recipe;
        (context, mounts, args, contexts)
    };
    let Some(first) = picked.next().map(inputs) else {
        return Err(format!(
            "{entry}: the task declares no image of the recipe `//{below}`: name a recipe that \
             an image() call of the task builds"
        ));
    };
    // The step's image may read any of them, so they all must agree, rather than those it reads.
    if picked.map(inputs).any(|other| other != first) {
        return Err(format!(
            "{entry}: the task's images give the recipe `//{below}` different inputs, as its \
             context, mounts, args or named contexts, for which its stage may resolve \
             differently: give each image a recipe of its own, or the same inputs"
        ));
    }
    Ok(())
}

/// The builds that [`Task::build`] and [`lock`] run: each running build, by its image's index and how
/// it is said; and each repository said to be built in, once, though several stages of one
/// recipe are built there.
struct Builds {
    building: Vec<((usize, String), Building)>,
    said: BTreeSet<String>,
    /// The tag of each image that ended, by the image's ID, which the run's record keeps.
    tags: Tags,
}

impl Builds {
    /// No build yet.
    fn new() -> Self {
        Self {
            building: Vec::new(),
            said: BTreeSet::new(),
            tags: Tags::new(),
        }
    }

    /// A step of `Plan::build`: starts the build of each of `ready` on `engine`, from `root`
    /// ([`Root::start`]), saying each repository as its first build starts, and gives each that
    /// has ended, waiting for one if none has, said, after its output, as it ends.
    fn step(
        &mut self,
        root: &Root,
        engine: &Engine,
        ready: Vec<(usize, Declared<'_, Built>)>,
    ) -> Vec<(usize, Result<Built, String>)> {
        let mut ended = Vec::new();
        for (index, declared) in ready {
            match root.start(engine, &declared) {
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
        if ended.is_empty() && !self.building.is_empty() {
            // After all that its `docker build` wrote, which this closes.
            let ((index, image), built) = engine::finish(&mut self.building);
            match &built {
                Ok(_) => say(format_args!("built an image in {image}")),
                Err(_) => say(format_args!("the build of an image in {image} failed")),
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

/// The repository that `declared` is built in, and its stage, as `, stage <name>`, if its recipe
/// names one.
fn image<'d>(declared: &Declared<'d, Built>) -> (&'d str, String) {
    match declared {
        Declared::Recipe { recipe, .. } => {
            let stage = recipe.target.iter().map(|stage| format!(", stage {stage}"));
            (&recipe.repository, stage.collect())
        }
        Declared::Layer { repository, .. } => (repository, String::new()),
    }
}

/// How many images [`Task::build`] builds at once: as many as the CPUs this process may run on, or one
/// if that can't be told.
// This machine's, though the builds run on the engine, which may be another's: they are its
// processes that wait on the engine, and hash and stage what it builds.
fn jobs() -> NonZeroUsize {
    thread::available_parallelism().unwrap_or(NonZeroUsize::MIN)
}

/// What `then` gives for the task whose entry is `entry`, given `args`, once it has no problem:
/// finds its [`Root`], evaluates it, then checks each image's recipe ([`Root::recipe`]) and the
/// workflow ([`Plan::check`]), all reaching no engine, and calls `then` on the thread that
/// evaluated it ([`Task`]).
///
/// # Errors
///
/// Fails if the root can't be found, or isn't as [`Root`] says, or if no thread can be started
/// for the evaluation, or as `then` does; and gives the task's problems, if it has any.
pub(crate) fn checked<T: Send>(
    entry: &Entry,
    args: &Args,
    then: impl FnOnce(Task<'_>) -> Result<Result<T, Problems>, String> + Send,
) -> Result<Result<T, Problems>, String> {
    let (mut root, label) = Root::find(entry)?;
    // The plan stays on the thread it was evaluated on, as what it holds can't be sent to
    // another, so `then` runs there too.
    evaluating(|| {
        let plan = match lang::evaluate(&label, &mut root, args, lang::Bounds::WORKFLOW) {
            Ok(plan) => plan,
            Err(problems) => return Ok(Err(problems)),
        };
        match plan.check(|recipe, contexts| root.recipe(recipe, contexts)) {
            Ok(workflow) => then(Task {
                root: &root,
                plan,
                workflow,
            }),
            Err(problems) => Ok(Err(problems)),
        }
    })?
}

/// What `evaluate` gives, run on a thread named `evaluation` with the stack that
/// `lang::evaluate` needs, [`lang::STACK_BYTES`].
///
/// # Errors
///
/// Fails if the thread can't be started.
fn evaluating<T: Send>(evaluate: impl FnOnce() -> T + Send) -> Result<T, String> {
    thread::scope(|scope| {
        // A thread of its own for its stack alone: the main thread's is as large as the process's
        // limit makes it, which may be less.
        let thread = (thread::Builder::new())
            .name("evaluation".to_owned())
            .stack_size(lang::STACK_BYTES);
        let evaluating = thread.spawn_scoped(scope, evaluate).map_err(|error| {
            format!("a thread to evaluate the task on can't be started: {error}")
        })?;
        Ok(evaluating
            .join()
            .unwrap_or_else(|panic| panic::resume_unwind(panic)))
    })
}
