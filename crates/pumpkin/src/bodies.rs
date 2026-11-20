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

/// Checks, without calling it, that each body of `workflow`'s functions, and its final call's,
/// takes a call of its kind, on an evaluation of its file from `sources` with `args`, and gives
/// `sources` back.
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
        let body = body(&function.body.file, &function.body.def, &function.keywords);
        lang::check(&body, BodyKind::Job, &mut &sources, args, Bounds::WORKFLOW)?;
    }
    // A now function gives its body no keywords.
    let keywords = Map::new();
    for (_, function) in workflow.now_functions() {
        let body = body(&function.body.file, &function.body.def, &keywords);
        lang::check(&body, BodyKind::Now, &mut &sources, args, Bounds::WORKFLOW)?;
    }
    let final_call = workflow.final_call();
    let body = body(
        &final_call.body.file,
        &final_call.body.def,
        &final_call.keywords,
    );
    lang::check(
        &body,
        BodyKind::Final,
        &mut &sources,
        args,
        Bounds::WORKFLOW,
    )?;
    Ok(sources)
}

fn body<'b>(file: &'b Label, def: &'b Name, keywords: &'b Map<String, Value>) -> Body<'b> {
    Body {
        file,
        def: def.as_str(),
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
        arguments: &BTreeMap<Name, ir::ArgumentValue>,
        target: &mut dyn runtime::Target,
    ) -> Result<Vec<(Name, Value)>, String> {
        let body = self::body(&body.file, &body.def, keywords);
        let host = Host::Job(&mut Driven(target));
        let mut sources = self.sources;
        let bounds = Bounds::WORKFLOW;
        let returned = lang::call(&body, host, arguments, &mut sources, self.args, bounds);
        returned.map_err(|problems| problems.to_string())
    }

    fn final_call(
        &self,
        body: &ir::FinalBody,
        keywords: &Map<String, Value>,
        submissions: &[(Name, Option<usize>)],
        target: &mut dyn runtime::Target,
    ) -> Result<Vec<(Name, Value)>, String> {
        let body = self::body(&body.file, &body.def, keywords);
        let host = Host::Final(&mut Driven(target), submissions);
        let mut sources = self.sources;
        let bounds = Bounds::WORKFLOW;
        let arguments = BTreeMap::new();
        let returned = lang::call(&body, host, &arguments, &mut sources, self.args, bounds);
        returned.map_err(|problems| problems.to_string())
    }

    fn now(
        &self,
        body: &ir::Body,
        arguments: &BTreeMap<Name, ir::ArgumentValue>,
        run: &mut dyn runtime::RunState,
    ) -> Result<Vec<(Name, Value)>, String> {
        let keywords = Map::new();
        let body = self::body(&body.file, &body.def, &keywords);
        let host = Host::Now(&mut Proposing(run));
        let mut sources = self.sources;
        let bounds = Bounds::WORKFLOW;
        let returned = lang::call(&body, host, arguments, &mut sources, self.args, bounds);
        returned.map_err(|problems| problems.to_string())
    }
}

/// The run's state, as a now function's body proposes effects on it.
struct Proposing<'r>(&'r mut dyn runtime::RunState);

impl lang::Run for Proposing<'_> {
    fn set_submission(&mut self) {
        self.0.set_submission();
    }

    fn end_work(&mut self) {
        self.0.end_work();
    }
}

/// The runtime's target of a call, as a job's body drives it.
struct Driven<'t>(&'t mut dyn runtime::Target);

impl lang::Target for Driven<'_> {
    fn caller(&self) -> Option<&str> {
        self.0.caller()
    }

    fn put(&mut self, path: &AbsolutePath, input: usize) -> Result<(), String> {
        self.0.put(path, input)
    }

    fn run(&mut self, argv: &Argv) -> Result<(), String> {
        self.0.run(argv)
    }

    fn get(&mut self, path: &AbsolutePath) -> Result<String, String> {
        self.0.get(path)
    }
}
