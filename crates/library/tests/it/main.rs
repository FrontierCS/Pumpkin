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

/// The arguments every harness's function takes by keyword, each with the Starlark that gives
/// its value from the `--arg` of its name.
const SHARED: [(&str, &str); 6] = [
    ("model", r#"arg("model", str)"#),
    ("effort", r#"arg("effort", str)"#),
    ("workspace", r#"arg("workspace", str)"#),
    ("prompt", r#"arg("prompt", str)"#),
    ("deadline_seconds", r#"arg("deadline_seconds", int)"#),
    (
        "finalize_grace_seconds",
        r#"arg("finalize_grace_seconds", int)"#,
    ),
];

/// A task's entry that calls `function`, the library's harness of that name, with each of
/// `SHARED` and then `own`, and builds its agent on the image of `//images/Dockerfile`, which the
/// judge runs on too.
fn entry(function: &str, own: &[(&str, &str)]) -> String {
    let arguments: Vec<&(&str, &str)> = SHARED.iter().chain(own).collect();
    let mut entry = String::new();
    let _ = writeln!(
        entry,
        r#"load("@pumpkin//harness/{function}/{function}.star", "{function}")"#
    );
    let _ = writeln!(entry);
    for (keyword, value) in &arguments {
        let _ = writeln!(entry, "{} = {value}", keyword.to_uppercase());
    }
    let _ = writeln!(entry);
    let _ = writeln!(entry, r#"BASE = image("Dockerfile", context = "//images")"#);
    let _ = writeln!(entry);
    let _ = writeln!(entry, "agent = {function}(");
    let _ = writeln!(entry, "    BASE,");
    for (keyword, _) in &arguments {
        let _ = writeln!(entry, "    {keyword} = {},", keyword.to_uppercase());
    }
    let _ = writeln!(entry, ")");
    entry.push_str(WORKFLOW);
    entry
}

const WORKFLOW: &str = r#"
workflow(
    name = "harness",
    roles = [
        {
            "name": "agent",
            "image": agent["image"],
            "user": {"uid": 1000, "gid": 1000},
            "needs": {"cpu_millis": 1000, "memory_bytes": GiB},
            "runtime": agent["runtime"],
            "egress": agent["egress"],
            "grants": [{"submit": {"name": "final"}}],
        },
        {
            "name": "judge",
            "image": BASE,
            "user": {"uid": 65532, "gid": 65532},
            "needs": {"cpu_millis": 500, "memory_bytes": 256 * MiB},
            "runtime": {"judge": {
                "command": {"argv": ["true"], "cwd": "/pumpkin/home"},
                "slots": [{"name": "payload", "max_bytes": KiB}],
                "result": {"shows": {"score": "/score"}, "returns": ["/score"]},
            }},
        },
    ],
    score = {"role": "judge", "slot": "payload", "timeout_seconds": 60},
)
"#;

/// The arguments of `case`, a value for each of `SHARED` and then `own`, by their keywords, as
/// [`entry`] takes them.
fn args(own: &[(&str, &str)], case: &[&str]) -> Args {
    let keywords: Vec<&str> = (SHARED.iter().chain(own))
        .map(|(keyword, _)| *keyword)
        .collect();
    assert_eq!(keywords.len(), case.len(), "a value for each argument");
    let given: Vec<String> = (keywords.iter().zip(case))
        .map(|(keyword, value)| format!("{keyword}={value}"))
        .collect();
    Args::parse(given.iter().map(String::as_str)).unwrap()
}

/// A prompt of the task repository's form, shortened.
const FCS_PROMPT: &str = "Solve the evaluation task in /frontier/task/README.md.

When the answer is ready, run `/.pumpkin/bin/sandboxctl submit final <tar> --finalize`.
";

/// The agent's role of `workflow`, the first, and its runtime, whose deadline and grace are
/// those of `case`, as [`args`] takes it.
fn agent<'w>(workflow: &'w Workflow, case: &[&str]) -> (&'w ir::Role, &'w AgentRuntime) {
    let agent = &workflow.ir().roles[0];
    let ir::Runtime::Agent(runtime) = &agent.runtime else {
        panic!("the first role is the agent");
    };
    assert_eq!(runtime.deadline_seconds.to_string(), case[4]);
    assert_eq!(runtime.finalize_grace_seconds.to_string(), case[5]);
    (agent, runtime)
}

/// The workflow that `entry` gives, given `args`, its images standing as IDs of their number,
/// with what each image's build is, as `shown` has it; or the problems of its evaluation.
fn evaluated(entry: &str, args: &Args) -> Result<(Workflow, Vec<String>), String> {
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
                        let number = index + 1;
                        images.push(shown(number, &declared));
                        let image = image(number);
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

/// The ID that stands for the image of `number`.
fn image(number: usize) -> ir::Image {
    ir::Image::try_from(format!("sha256:{number:064x}")).unwrap()
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

/// What `declared`, the image of `number`, builds, as a line: its recipe and context, each
/// directory it mounts, its stage and build arguments, each named context by its image's number,
/// and its repository.
fn shown(number: usize, declared: &Declared<'_, Built>) -> String {
    let Declared { recipe, contexts } = declared;
    let Recipe {
        context,
        recipe,
        mounts,
        target,
        args,
        repository,
    } = recipe;
    let mut shown = format!("image {number}: {context}'s {recipe}");
    for (path, label) in mounts {
        let _ = write!(shown, ", {label} at {path}");
    }
    if let Some(target) = target {
        let _ = write!(shown, ", stage {target}");
    }
    for (name, value) in args {
        let _ = write!(shown, ", {name}={value}");
    }
    for (name, built) in contexts {
        let _ = write!(shown, ", image {} as {name}", built.number);
    }
    let _ = write!(shown, ", in {repository}");
    shown
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
    let route = &agent.egress.model.as_ref().unwrap().route;
    let route: Vec<String> = (route.as_slice().iter())
        .map(|entry| {
            let models: Vec<ModelName> = entry.models.clone().into();
            let models: Vec<String> = models.into_iter().map(String::from).collect();
            format!("{} {}", entry.api, models.join(" "))
        })
        .collect();
    format!("route: {}\n", route.join(", "))
}

/// The header lists of `agent`'s route entries; what builds each of `images`, as [`shown`] has
/// it; and the number of the image `agent` runs on.
fn layer(agent: &ir::Role, images: &[String]) -> String {
    let mut shown = String::new();
    for entry in agent.egress.model.as_ref().unwrap().route.as_slice() {
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
    for line in images {
        let _ = writeln!(shown, "{line}");
    }
    let number = (1..=images.len()).find(|number| image(*number) == agent.image);
    let _ = writeln!(shown, "the agent's image: image {}", number.unwrap());
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
