//! The workflow's bodies, as a check reads them and a run calls them: each evaluation of a body's
//! file reads the text that the workflow's evaluation read, with the same args, so that it gives
//! the same function, whatever changed on disk since.

use std::collections::BTreeMap;

use lang::{Args, Body, BodyKind, Bounds, Host, Label, Loader, Problems};
use schema::ir::{self, AbsolutePath, Argv, Name};
use schema::workflow::Workflow;
use serde_json::{Map, Value};

/// The text of each file that the workflow's evaluation read, by its label.
#[derive(Default)]
pub(crate) struct Sources(BTreeMap<Label, String>);

impl Loader for &Sources {
    fn load(&mut self, label: &Label) -> Result<String, String> {
        // A body is a `def` of a file that the workflow's evaluation ran, and that file's loads
        // ran with it, so an evaluation of the body's file reads only what this holds.
        let text = self
            .0
            .get(label)
            .ok_or_else(|| format!("`{label}` is no file that the workflow's evaluation read"))?;
        Ok(text.clone())
    }
}

/// A [`Loader`] that keeps the text of each file that `loader` gives.
pub(crate) struct Keeping<'l, L> {
    loader: &'l mut L,
    kept: Sources,
}

impl<'l, L: Loader> Keeping<'l, L> {
    pub(crate) fn new(loader: &'l mut L) -> Self {
        Self {
            loader,
            kept: Sources::default(),
        }
    }

    pub(crate) fn kept(self) -> Sources {
        self.kept
    }
}

impl Sources {
    /// The text of each file, by its label.
    pub(crate) fn texts(&self) -> &BTreeMap<Label, String> {
        &self.0
    }
}

impl<L: Loader> Loader for Keeping<'_, L> {
    fn load(&mut self, label: &Label) -> Result<String, String> {
        let text = self.loader.load(label)?;
        self.kept.0.insert(label.clone(), text.clone());
        Ok(text)
    }
}

/// Checks, without calling it, that each job function's body of `workflow` takes a job's call,
/// on an evaluation of its file from `sources` with `args`, and gives `sources` back.
///
/// # Errors
///
/// The problems of the first body that [`lang::check`] refuses.
pub(crate) fn check(
    workflow: &Workflow,
    sources: Sources,
    args: &Args,
) -> Result<Sources, Problems> {
    for (_, function) in workflow.job_functions() {
        let body = body(&function.body, &function.keywords);
        lang::check(&body, BodyKind::Job, &mut &sources, args, Bounds::WORKFLOW)?;
    }
    Ok(sources)
}

fn body<'b>(body: &'b ir::Body, keywords: &'b Map<String, Value>) -> Body<'b> {
    Body {
        file: &body.file,
        def: body.def.as_str(),
        keywords,
    }
}

/// The bodies of the workflow that a run runs, each called on an evaluation of its file afresh,
/// as [`check`] has checked it.
pub(crate) struct Bodies<'a> {
    sources: &'a Sources,
    args: &'a Args,
}

impl<'a> Bodies<'a> {
    /// The bodies of the workflow evaluated with `args`, whose files `sources` holds, as [`check`]
    /// gives them.
    pub(crate) fn new(sources: &'a Sources, args: &'a Args) -> Self {
        Self { sources, args }
    }
}

impl runtime::Bodies for Bodies<'_> {
    fn stack_bytes(&self) -> usize {
        lang::STACK_BYTES
    }

    fn job(
        &self,
        body: &ir::Body,
        keywords: &Map<String, Value>,
        target: &mut dyn runtime::Target,
    ) -> Result<Vec<(Name, Value)>, String> {
        let body = self::body(body, keywords);
        let host = Host::Job(&mut Driven(target));
        let mut sources = self.sources;
        let returned = lang::call(&body, host, &mut sources, self.args, Bounds::WORKFLOW);
        returned.map_err(|problems| problems.to_string())
    }
}

/// The runtime's target of a call, as a job's body drives it.
struct Driven<'t>(&'t mut dyn runtime::Target);

impl lang::Target for Driven<'_> {
    fn put(&mut self, path: &AbsolutePath) -> Result<(), String> {
        self.0.put(path)
    }

    fn run(&mut self, argv: &Argv) -> Result<(), String> {
        self.0.run(argv)
    }

    fn get(&mut self, path: &AbsolutePath) -> Result<String, String> {
        self.0.get(path)
    }
}
