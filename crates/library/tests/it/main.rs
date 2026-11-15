//! The standard library as `pumpkin` holds it: the files it bundles, and what each harness's
//! function gives, evaluated through `lang` with a loader over the bundled files.

#![expect(
    clippy::panic,
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

mod claude_code;
mod codex;
mod docker;
mod pi;
mod qwen_code;
mod zcode;

use std::fmt::Write as _;
use std::num::NonZeroUsize;
use std::path::Path;
use std::{fs, thread};

use expect_test::{Expect, expect};
use lang::{Args, Bounds, Declared, Label, Loader, Recipe, Root};
use schema::ir::{self, AgentRuntime, EnvVar, ModelName};
use schema::workflow::Workflow;
use sha2::{Digest as _, Sha256};

fn check(actual: &str, expect: &Expect) {
    expect.assert_eq(actual);
}

#[test]
fn bundled() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../library");
    let mut found = Vec::new();
    walk(&root, "", &mut found);
    found.sort();
    let bundled: Vec<String> = (library::FILES.iter())
        .map(|(path, bytes)| {
            assert_eq!(*bytes, fs::read(root.join(path)).unwrap(), "{path}");
            (*path).to_owned()
        })
        .collect();
    // Sorted, so that a loader may search it.
    assert_eq!(
        bundled, found,
        "the table lists each file below `library/`, sorted"
    );
    check(
        &bundled.join("\n"),
        &expect![[r#"
            harness/claude_code/claude_code.star
            harness/claude_code/layer/Dockerfile
            harness/codex/LICENSE
            harness/codex/catalog.star
            harness/codex/codex.star
            harness/codex/layer/Dockerfile
            harness/node/layer/Dockerfile
            harness/node/node.star
            harness/pi/layer/Dockerfile
            harness/pi/layer/package-lock.json
            harness/pi/layer/package.json
            harness/pi/pi.star
            harness/qwen_code/layer/Dockerfile
            harness/qwen_code/qwen_code.star
            harness/zcode/layer/Dockerfile
            harness/zcode/zcode.star"#]],
    );
}

/// Adds the path of each file below `dir` to `found`, after `below`, its path below the root.
fn walk(dir: &Path, below: &str, found: &mut Vec<String>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        let path = format!("{below}{name}");
        if entry.file_type().unwrap().is_dir() {
            walk(&entry.path(), &format!("{path}/"), found);
        } else {
            found.push(path);
        }
    }
}

/// The text of the library's file at `path`.
fn file(path: &str) -> String {
    let found = library::FILES.iter().find(|(file, _)| *file == path);
    let (_, bytes) = found.unwrap_or_else(|| panic!("the library holds no `{path}`"));
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// The workflow that `entry` gives, given `args`, its images standing as IDs of their number,
/// with what each image's build is, as `shown` has it; or the problems of its evaluation.
fn evaluated(entry: &str, args: &Args) -> Result<(Workflow, Vec<Shown>), String> {
    let mut loader = Bundled(entry);
    let label = Label::parse("//task.star").unwrap();
    // As `pumpkin` evaluates, on a stack of the size `lang` asks.
    let thread = thread::Builder::new().stack_size(lang::STACK_BYTES);
    thread::scope(|scope| {
        let evaluation = thread.spawn_scoped(scope, || {
            let plan = lang::evaluate(&label, &mut loader, args, Bounds::WORKFLOW);
            let plan = plan.map_err(|problems| problems.to_string())?;
            plan.check(checked_recipe)
                .map_err(|problems| problems.to_string())?;
            let mut images = Vec::new();
            let built = plan.build(NonZeroUsize::MIN, |ready| {
                (ready.into_iter())
                    .map(|(index, declared)| {
                        images.push(shown(&declared));
                        let number = index + 1;
                        let image = ir::Image::try_from(format!("sha256:{number:064x}")).unwrap();
                        (index, Ok(Built { number, image }))
                    })
                    .collect()
            });
            let built = built.unwrap_or_else(|problems| panic!("{problems}"));
            Ok((built, images))
        });
        evaluation.unwrap().join().unwrap()
    })
}

/// The problems `recipe::check` finds in a library's recipe, given `contexts`; a task's passes.
fn checked_recipe(declared: &Recipe, contexts: &[&str]) -> Vec<String> {
    if declared.context.root() == Root::Task {
        return Vec::new();
    }
    let given = recipe::Given {
        target: declared.target.clone(),
        args: declared.args.clone(),
        contexts: contexts
            .iter()
            .map(|name| ((*name).to_owned(), ()))
            .collect(),
    };
    let path = format!("{}/{}", declared.context.path(), declared.recipe);
    Vec::from_iter(recipe::check(&path, file(&path).as_bytes(), &given).err())
}

/// What an image's build is, as [`shown`] has it.
type Shown = (String, Vec<String>);

/// What `declared` builds, as a line, and its recipe's lines but its comments and blank ones,
/// each image by its number.
fn shown(declared: &Declared<'_, Built>) -> Shown {
    let Declared { recipe, contexts } = declared;
    let text = match recipe.context.root() {
        Root::Task => String::new(),
        Root::Library => file(&format!("{}/{}", recipe.context.path(), recipe.recipe)),
    };
    let lines = (text.lines())
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned);
    let contexts: Vec<String> = (contexts.iter())
        .map(|(name, built)| format!(", image {} as {name}", built.number))
        .collect();
    let line = format!(
        "{}'s {}{}, in {}",
        recipe.context,
        recipe.recipe,
        contexts.concat(),
        recipe.repository
    );
    (line, lines.collect())
}

/// `runtime` but its standard input and its times, a line for each part, each word of the command
/// that is empty or holds a space quoted; each file by its mode, path and digest, followed by
/// what `excerpt` shows of its content.
fn show(runtime: &AgentRuntime, excerpt: impl Fn(&ir::File) -> String) -> String {
    let AgentRuntime {
        command,
        dirs,
        files,
        stdin: _,
        deadline_seconds: _,
        finalize_grace_seconds: _,
    } = runtime;
    let quoted = |word: String| {
        if word.is_empty() || word.contains(char::is_whitespace) {
            format!("{word:?}")
        } else {
            word
        }
    };
    let argv: Vec<String> = command.argv.clone().into();
    let argv: Vec<String> = argv.into_iter().map(quoted).collect();
    let env: Vec<EnvVar> = command.env.clone().into();
    let env: Vec<String> = env.into_iter().map(String::from).map(quoted).collect();
    let mut shown = format!(
        "argv: {}\nenv: {}\ncwd: {}\n",
        argv.join(" "),
        env.join(" "),
        command.cwd,
    );
    for dir in dirs {
        let _ = writeln!(shown, "dir {} {}", dir.mode, dir.path);
    }
    for file in files {
        let mut digest = String::new();
        for byte in Sha256::digest(file.content.as_bytes()) {
            let _ = write!(digest, "{byte:02x}");
        }
        let _ = writeln!(shown, "file {} {}, sha256 {digest}:", file.mode, file.path);
        shown.push_str(&excerpt(file));
    }
    shown
}

/// The line of `agent`'s model route: each entry's API and models.
fn route(agent: &ir::Role) -> String {
    let route = agent.egress.model.as_ref().unwrap();
    let route: Vec<String> = (route.as_slice().iter())
        .map(|entry| {
            let models: Vec<ModelName> = entry.models.clone().into();
            let models: Vec<String> = models.into_iter().map(String::from).collect();
            format!("{} {}", entry.api, models.join(" "))
        })
        .collect();
    format!("route: {}\n", route.join(", "))
}

/// The header lists of `agent`'s route entries, and the library's `images`, all but the first,
/// the task's: what builds each and its recipe's lines, as [`shown`] has them.
fn layer(agent: &ir::Role, images: Vec<Shown>) -> String {
    let mut shown = String::new();
    for entry in agent.egress.model.as_ref().unwrap().as_slice() {
        let names = |names: &[ir::HeaderName]| {
            if names.is_empty() {
                return "none".to_owned();
            }
            let names: Vec<&str> = names.iter().map(ir::HeaderName::as_str).collect();
            names.join(" ")
        };
        let _ = writeln!(shown, "request headers: {}", names(&entry.request_headers));
        let _ = writeln!(
            shown,
            "response headers: {}",
            names(&entry.response_headers)
        );
    }
    for (image, recipe) in images.into_iter().skip(1) {
        let _ = writeln!(shown, "image: {image}");
        for line in recipe {
            let _ = writeln!(shown, "  {line}");
        }
    }
    shown
}

/// Reads the entry, `//task.star`, from its text, and the standard library's files from the
/// bundle.
struct Bundled<'e>(&'e str);

impl Loader for Bundled<'_> {
    fn load(&mut self, label: &Label) -> Result<String, String> {
        match label.root() {
            Root::Task if label.path() == "task.star" => Ok(self.0.to_owned()),
            Root::Task => Err(format!("the test holds no `{label}`")),
            Root::Library => Ok(file(label.path())),
        }
    }
}

struct Built {
    number: usize,
    image: ir::Image,
}

impl AsRef<ir::Image> for Built {
    fn as_ref(&self) -> &ir::Image {
        &self.image
    }
}
