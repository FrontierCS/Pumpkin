//! What an evaluation gives: a [`Plan`] of the images it declares and of the workflow that names
//! them, which the IR's types and rules check once the images have IDs.
//!
//! The images are declared in the order the calls of `image()` run, so that each comes after the
//! images it is built on, and an image declared again, equal to one before, is that one. An
//! image's problems, from its check or its build, are at the call that declares it, with the calls
//! it is in.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroUsize;

use schema::ir;
use schema::sandbox::RelativePath;
use schema::workflow::Workflow;

use crate::problem::{Fault, Finding, Problem, Problems, Sources, list};
use crate::value::{ImageId, Span, Value};
use crate::{Args, Label};

// What `pumpkin`'s functions have recorded of an evaluation so far.
pub(crate) struct Planned<'a> {
    pub(crate) args: &'a Args,
    pub(crate) workflow: Option<WorkflowCall>,
    /// Each arg an `arg()` call has declared, with its first call.
    pub(crate) declared: BTreeMap<String, Span>,
    pub(crate) images: Vec<Declaration>,
}

impl<'a> Planned<'a> {
    pub(crate) fn new(args: &'a Args) -> Self {
        Self {
            args,
            workflow: None,
            declared: BTreeMap::new(),
            images: Vec::new(),
        }
    }

    // The name of each `--arg` given that no `arg()` call has declared so far, sorted.
    pub(crate) fn given_undeclared(&self) -> impl Iterator<Item = &str> {
        (self.args.0.keys())
            .map(String::as_str)
            .filter(|name| !self.declared.contains_key(*name))
    }

    fn undeclared(&self, entry: &Label) -> Vec<Problem> {
        let declared: Vec<&str> = self.declared.keys().map(String::as_str).collect();
        let expected = if declared.is_empty() {
            String::from(", as none is made: drop it")
        } else {
            format!(": give one of {}", list(&declared))
        };
        self.given_undeclared()
            .map(|name| {
                let name = name.escape_debug();
                let message = format!("`--arg {name}` is declared by no `arg()` call{expected}");
                Problem::file(entry, message)
            })
            .collect()
    }
}

// What a `workflow()` call gives: a dict of its keyword arguments, the IR's fields.
pub(crate) struct WorkflowCall {
    pub(crate) at: Span,
    /// Made at the call, with each key at its keyword.
    pub(crate) fields: Value,
}

// An image, and where it was first declared, for the problems that building it finds.
pub(crate) struct Declaration {
    pub(crate) image: Image,
    place: Place,
}

// Two images that are equal are one: declaring the same one again gives the first.
#[derive(PartialEq)]
pub(crate) struct Image {
    pub(crate) recipe: Recipe,
    pub(crate) contexts: BTreeMap<String, ImageId>,
}

// A call that declares an image, and the calls of functions that it is in, innermost first.
pub(crate) struct Place {
    pub(crate) at: Span,
    pub(crate) calls: Vec<(String, Span)>,
}

impl Place {
    fn fault(&self, message: String) -> Fault {
        let fault = Fault::new(self.at, message);
        (self.calls.iter()).fold(fault, |fault, (note, at)| {
            fault.note(note.clone(), Some(*at))
        })
    }
}

pub(crate) fn declare(images: &mut Vec<Declaration>, image: Image, place: Place) -> ImageId {
    if let Some(found) = images.iter().position(|declared| declared.image == image) {
        return ImageId(found);
    }
    images.push(Declaration { image, place });
    ImageId(images.len() - 1)
}

/// What builds an image, which `image()` declares.
#[derive(Debug, PartialEq, Eq)]
pub struct Recipe {
    /// The directory the image is built from, with `mounts`.
    pub context: Label,
    /// The recipe's path in the context.
    pub recipe: RelativePath,
    /// Each directory mounted in the context, by its path there.
    pub mounts: BTreeMap<RelativePath, Label>,
    /// The stage it builds; the recipe's last without one.
    pub target: Option<recipe::Stage>,
    /// Each build argument's value, by its name.
    pub args: BTreeMap<String, String>,
    /// The repository that the image is tagged in, a name Docker takes for one: `pumpkin/`, the
    /// context's path below the task's root and the recipe's directory in the context, or
    /// `pumpkin-library/` and the same, for a context of the standard library.
    pub repository: String,
}

/// An image to build, as [`Plan::build`] asks for it, `B` being what a build gives: its recipe,
/// and each image of its named contexts, built before, by its name.
#[derive(Debug)]
pub struct Declared<'p, B> {
    pub recipe: &'p Recipe,
    pub contexts: BTreeMap<&'p str, &'p B>,
}

/// What an evaluation declares: its images, and its workflow.
pub struct Plan {
    sources: Sources,
    workflow: WorkflowCall,
    images: Vec<Declaration>,
}

impl Plan {
    /// The plan of the evaluation of the entry at `entry`, which has run, reading `sources`;
    /// refused if the entry calls no `workflow()`, or an `--arg` given names no arg that an
    /// `arg()` call declares, each one.
    pub(crate) fn new(
        entry: &Label,
        sources: Sources,
        planned: Planned<'_>,
    ) -> Result<Self, Problems> {
        let undeclared = planned.undeclared(entry);
        let problems = match planned.workflow {
            Some(workflow) if undeclared.is_empty() => {
                return Ok(Self {
                    sources,
                    workflow,
                    images: planned.images,
                });
            }
            Some(_) => undeclared,
            None => {
                let message = "this file calls no `workflow()`: call it once, with the workflow";
                let mut problems = Vec::from([Problem::file(entry, message)]);
                problems.extend(undeclared);
                problems
            }
        };
        Err(Problems::new(sources, problems))
    }

    /// Calls `lint` with the label and text of each file the evaluation read, and gives its
    /// findings as problems.
    ///
    /// # Panics
    ///
    /// A finding's span isn't within its file's text, on characters' boundaries.
    pub fn lint<E>(
        &self,
        mut lint: impl FnMut(&Label, &str) -> Result<Vec<Finding>, E>,
    ) -> Result<Result<(), Problems>, E> {
        let mut problems = Vec::new();
        for (file, label, text) in self.sources.files() {
            for Finding { span, message } in lint(label, text)? {
                assert!(
                    text.get(span.clone()).is_some(),
                    "a finding in `{label}` is about {span:?}, which isn't within its text"
                );
                let at = Span {
                    file,
                    start: span.start,
                    end: span.end,
                };
                problems.push(Problem::At(Fault::new(at, message)));
            }
        }
        if problems.is_empty() {
            Ok(Ok(()))
        } else {
            Ok(Err(Problems::new(self.sources.clone(), problems)))
        }
    }

    /// Each image's recipe, in the order the images were declared, with the images that each
    /// recipe names its contexts by: each name's image, as its index in this order.
    pub fn recipes(&self) -> impl Iterator<Item = (&Recipe, BTreeMap<&str, usize>)> {
        (self.images.iter()).map(|Declaration { image, .. }| {
            let Image { recipe, contexts } = image;
            let contexts = contexts.iter().map(|(name, base)| (name.as_str(), base.0));
            (recipe, contexts.collect())
        })
    }

    /// Checks each image's recipe with `recipes`, which gives the problems it finds (such as a
    /// recipe that is no file), sorted, given the recipe and the names of its contexts; and gives
    /// the workflow, its images standing as IDs of their own, which no build gave.
    ///
    /// # Errors
    ///
    /// Each problem `recipes` gives, at the call declaring the recipe's image; and the first
    /// value the IR's types refuse, or every IR rule the workflow breaks, [`Workflow::parse`]'s.
    pub fn check(
        &self,
        mut recipes: impl FnMut(&Recipe, &[&str]) -> Vec<String>,
    ) -> Result<Workflow, Problems> {
        let mut problems = Vec::new();
        for Declaration { image, place } in &self.images {
            let Image { recipe, contexts } = image;
            let names: Vec<&str> = contexts.keys().map(String::as_str).collect();
            let found = recipes(recipe, &names).into_iter();
            problems.extend(found.map(|message| Problem::At(place.fault(message))));
        }
        let stand_ins: Vec<ir::Image> = (0..self.images.len()).map(stand_in).collect();
        match self.workflow(&stand_ins) {
            Ok(workflow) if problems.is_empty() => Ok(workflow),
            Ok(_) => Err(Problems::new(self.sources.clone(), problems)),
            Err(found) => {
                problems.extend(found);
                Err(Problems::new(self.sources.clone(), problems))
            }
        }
    }

    /// The workflow, with every image built through `step`, which `build_for` describes.
    ///
    /// # Errors
    ///
    /// Each build that fails, at the call that declares its image, or the workflow's problems.
    ///
    /// # Panics
    ///
    /// `step` gives no build while some run, or one that it wasn't given or gave already.
    pub fn build<B: AsRef<ir::Image>>(
        self,
        jobs: NonZeroUsize,
        step: impl FnMut(Vec<(usize, Declared<'_, B>)>) -> Vec<(usize, Result<B, String>)>,
    ) -> Result<Workflow, Problems> {
        let every = Vec::from_iter((0..self.images.len()).map(|_| true));
        let done = self.built(jobs, &every, step)?;
        let images: Vec<ir::Image> = (done.iter())
            .map(|done| match done {
                Some(done) => done.as_ref().clone(),
                None => unreachable!(
                    "every image is built once none failed, as each is declared after those it \
                     is built on, so each in turn may start"
                ),
            })
            .collect();
        let workflow = self.workflow(&images);
        workflow.map_err(|problems| Problems::new(self.sources, problems))
    }

    /// What `then` gives, called once with each image `wanted` picks, in order, with its built
    /// contexts. The images they are built on are built, and no other, not even the picked ones.
    ///
    /// `step` is given images to build, and gives the builds that ended by index, at least one
    /// while any run, waiting if given none. At most `jobs` run, and after a failure none starts.
    ///
    /// # Panics
    ///
    /// `step` gives no build while some run, or one that it wasn't given or gave already.
    pub fn build_for<B, T>(
        &self,
        jobs: NonZeroUsize,
        mut wanted: impl FnMut(&Recipe) -> bool,
        step: impl FnMut(Vec<(usize, Declared<'_, B>)>) -> Vec<(usize, Result<B, String>)>,
        then: impl FnOnce(Vec<Declared<'_, B>>) -> T,
    ) -> Result<T, Problems> {
        let picked: Vec<bool> = (self.images.iter())
            .map(|Declaration { image, .. }| wanted(&image.recipe))
            .collect();
        // Each image is declared after those it is built on, so that walking back from the last
        // reaches an image after every image that needs it.
        let mut needed = Vec::from_iter((0..self.images.len()).map(|_| false));
        for (index, Declaration { image, .. }) in self.images.iter().enumerate().rev() {
            if picked[index] || needed[index] {
                image
                    .contexts
                    .values()
                    .for_each(|base| needed[base.0] = true);
            }
        }
        let done = self.built(jobs, &needed, step)?;
        let declared = (self.images.iter().zip(&picked))
            .filter(|(_, picked)| **picked)
            .map(|(Declaration { image, .. }, _)| declared(image, &done))
            .collect();
        Ok(then(declared))
    }

    // Each image that `needed` marks, by its index, built as `build_for` says by `step`, and `None`
    // for each other.
    fn built<B>(
        &self,
        jobs: NonZeroUsize,
        needed: &[bool],
        mut step: impl FnMut(Vec<(usize, Declared<'_, B>)>) -> Vec<(usize, Result<B, String>)>,
    ) -> Result<Vec<Option<B>>, Problems> {
        let count = self.images.len();
        let mut done: Vec<Option<B>> = (0..count).map(|_| None).collect();
        // What isn't needed counts as started, so it never starts.
        let mut started = Vec::from_iter((0..count).map(|index| !needed[index]));
        let mut running = 0_usize;
        let mut failed: Vec<(usize, String)> = Vec::new();
        loop {
            let mut ready = Vec::new();
            for (index, Declaration { image, .. }) in self.images.iter().enumerate() {
                if !failed.is_empty() || running + ready.len() == jobs.get() {
                    break;
                }
                if !started[index] && (image.contexts.values()).all(|base| done[base.0].is_some()) {
                    started[index] = true;
                    ready.push((index, declared(image, &done)));
                }
            }
            if ready.is_empty() && running == 0 {
                break;
            }
            running += ready.len();
            let ended = step(ready);
            assert!(
                !ended.is_empty(),
                "a build step gave no build while some ran"
            );
            for (index, result) in ended {
                assert!(
                    started[index] && done[index].is_none() && running > 0,
                    "a build step gave image {index}, which it wasn't given, or gave it again"
                );
                running -= 1;
                match result {
                    Ok(image) => done[index] = Some(image),
                    Err(why) => failed.push((index, why)),
                }
            }
        }
        if failed.is_empty() {
            return Ok(done);
        }
        // By the order of the declarations, whatever the order the builds ended in.
        failed.sort_by_key(|(index, _)| *index);
        let problems = (failed.into_iter())
            .map(|(index, why)| {
                let fault =
                    (self.images[index].place).fault(format!("building the image failed: {why}"));
                Problem::At(fault)
            })
            .collect();
        Err(Problems::new(self.sources.clone(), problems))
    }

    fn workflow(&self, images: &[ir::Image]) -> Result<Workflow, Vec<Problem>> {
        let fields = &self.workflow.fields;
        let ir = crate::ir::workflow(fields, images, &self.sources)?;
        Workflow::parse(&ir).map_err(|errors| {
            let faults = crate::ir::locate(fields, errors.as_slice());
            faults.into_iter().map(Problem::At).collect()
        })
    }
}

fn declared<'p, B>(image: &'p Image, done: &'p [Option<B>]) -> Declared<'p, B> {
    let built = |base: &ImageId| match &done[base.0] {
        Some(built) => built,
        None => unreachable!("an image's build starts only once those it is built on are built"),
    };
    let Image { recipe, contexts } = image;
    Declared {
        recipe,
        contexts: (contexts.iter())
            .map(|(name, base)| (name.as_str(), built(base)))
            .collect(),
    }
}

// The ID that stands for the image at `place` among a plan's before it is built: a manifest
// digest, as a built one's is, and no other image's.
fn stand_in(place: usize) -> ir::Image {
    match ir::Image::try_from(format!("sha256:{:064x}", place + 1)) {
        Ok(image) => image,
        Err(error) => unreachable!(
            "`sha256:` and 64 lowercase hexadecimal digits are an image's manifest digest, yet \
             {error}"
        ),
    }
}
