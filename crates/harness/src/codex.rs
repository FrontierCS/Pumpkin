//! Codex as a harness: [`CODEX_0_167_0`] pins the Codex that agents run, and [`runtime`] writes
//! the agent's runtime for one unattended run of it.
//!
//! Codex works in its [`Workspace`], and keeps its home (`CODEX_HOME`), with its configuration and
//! its databases, and its temporary files (`TMPDIR`) in two directories of their own there. Its
//! record of the run is the events it writes to its standard output as JSON lines, which the
//! teardown collects as `command.stdout` ([`command_stdout()`]). It reaches the model only at the
//! endpoint `sandboxd` serves ([`model_url()`]), with no credential: the host holds the key.
//!
//! [`command_stdout()`]: schema::sandbox::command_stdout

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::num::NonZeroU64;

use schema::ir::{
    self, AbsolutePath, AgentRuntime, Argv, Command, Dir, Env, EnvVar, ModelName, RolePath,
};
use schema::sandbox::{MAX_MODEL_REQUESTS_IN_FLIGHT, harness_dir, model_url};
use serde_json::{Value, json};

use crate::{Mode, Pin, PinnedFile};

/// Codex 0.167.0 for x86-64 Linux, from npm's platform tarball, `@openai/codex@0.167.0-linux-x64`.
///
/// It keeps `bin/codex`, Codex itself, which finds its other files from its own executable's
/// path: `codex-package.json`, beside its `bin` directory, marks the package's layout.
/// `bin/codex-code-mode-host` is the process Codex starts for code mode, and `codex-path/rg` is
/// ripgrep, which Codex puts on its tools' `PATH`. Every binary is static, so the harness needs
/// nothing from the image it is added to.
pub const CODEX_0_167_0: Pin = Pin {
    name: "codex-0.167.0",
    url: "https://registry.npmjs.org/@openai/codex/-/codex-0.167.0-linux-x64.tgz",
    sha256: "sha256:2dc9aa871abb513c3539d20d025f701faf1974c46ee906bda3a1e1f5026d4434",
    dir: "package/vendor/x86_64-unknown-linux-musl",
    // The tarball's `codex-resources` stays out: its `bwrap` is Codex's own sandbox, for which
    // the agent's sandbox stands in, and its voice host serves spoken input, which an unattended
    // agent has none of.
    files: &[
        PinnedFile {
            path: "bin/codex",
            mode: Mode::Executable,
        },
        PinnedFile {
            path: "bin/codex-code-mode-host",
            mode: Mode::Executable,
        },
        PinnedFile {
            path: "codex-package.json",
            mode: Mode::Readable,
        },
        PinnedFile {
            path: "codex-path/rg",
            mode: Mode::Executable,
        },
    ],
};

/// The catalog entries the crate carries: `gpt-6.1-terra`'s, unchanged, from Codex's
/// `codex-rs/models-manager/models.json` at `rust-v0.167.0`, which a new pin takes again from its
/// own version; and `glm-5.3`'s, which Codex's catalog lacks, from Z.ai's `models.json` for Codex
/// (<https://docs.z.ai/devpack/tool/codex.md>), plus `model_messages`, holding as
/// `instructions_template` Codex's base instructions (`codex-rs/models-manager/prompt.md`,
/// without its `update_plan` instructions), since Z.ai's entry gives Codex none.
///
/// OpenAI Codex, Copyright 2025 OpenAI. Its entries are under the Apache License, Version 2.0,
/// a copy of which is `codex/LICENSE`.
const CATALOG: &str = include_str!("codex/catalog.json");

/// How Codex sends the model its tools, which the model upstream must forward.
#[derive(Clone, Copy, Debug)]
pub enum Tools {
    /// As the model's catalog entry has it, the crate's if it carries one and Codex's own
    /// otherwise, in forms only some upstreams forward: an `additional_tools` input item in
    /// place of `tools`, and `custom` and `namespace` tools.
    Native,
    /// Each as a `function` tool in the request's `tools`, the form every upstream of the
    /// Responses API forwards: for an upstream that forwards no other, through which Codex would
    /// have no tools at all. Codex then offers no tool it has only other forms for: not code
    /// mode's or sub-agents', and not `apply_patch`, which the model runs through the shell
    /// instead.
    Functions,
}

impl Tools {
    const ALL: [Self; 2] = [Self::Native, Self::Functions];

    fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Functions => "functions",
        }
    }
}

impl TryFrom<&str> for Tools {
    type Error = String;

    /// Parses `native` or `functions`.
    fn try_from(tools: &str) -> Result<Self, Self::Error> {
        (Self::ALL.into_iter())
            .find(|known| known.as_str() == tools)
            .ok_or_else(|| {
                let all: Vec<&str> = Self::ALL.into_iter().map(Self::as_str).collect();
                format!(
                    "`{}` is not a way to send Codex's tools: use one of {}",
                    tools.escape_debug(),
                    all.join(", ")
                )
            })
    }
}

/// How hard the model reasons, Codex's `model_reasoning_effort`, such as `xhigh`.
#[derive(Clone, Copy, Debug)]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Ultra,
}

impl ReasoningEffort {
    const ALL: [Self; 8] = [
        Self::None,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
        Self::Ultra,
    ];

    /// The value Codex takes.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
        }
    }
}

impl TryFrom<&str> for ReasoningEffort {
    type Error = String;

    fn try_from(effort: &str) -> Result<Self, Self::Error> {
        (Self::ALL.into_iter())
            .find(|known| known.as_str() == effort)
            .ok_or_else(|| {
                let all: Vec<&str> = Self::ALL.into_iter().map(Self::as_str).collect();
                format!(
                    "`{}` is not a reasoning effort: use one of {}",
                    effort.escape_debug(),
                    all.join(", ")
                )
            })
    }
}

/// A directory that Codex can work in: an absolute path below which a workflow can put
/// Codex's home and temporary directory, and the files in its home.
#[derive(Clone, Debug)]
pub struct Workspace {
    path: AbsolutePath,
    /// `CODEX_HOME`, `.codex` in the workspace. It can't be in the agent's [`log_dir()`], where
    /// a workflow puts nothing, and where the links Codex keeps in it would fail the collect.
    ///
    /// [`log_dir()`]: schema::sandbox::log_dir
    home: RolePath,
    /// `TMPDIR`, `.tmp` in the workspace, beside the home: Codex refuses to make its helpers'
    /// links in a home below its temporary directory.
    tmp: RolePath,
    config: RolePath,
    /// The model catalog in the home: the model's carried entry, which [`Tools::Functions`]
    /// needs, and which [`Tools::Native`] takes unchanged if the crate carries it.
    catalog: RolePath,
}

impl TryFrom<AbsolutePath> for Workspace {
    type Error = String;

    /// Parses a workspace in which Codex's home, its temporary directory and the files in its
    /// home are [`RolePath`]s.
    fn try_from(path: AbsolutePath) -> Result<Self, Self::Error> {
        Ok(Self {
            home: below(&path, ".codex")?,
            tmp: below(&path, ".tmp")?,
            config: below(&path, ".codex/config.toml")?,
            catalog: below(&path, ".codex/model-catalog.json")?,
            path,
        })
    }
}

/// The agent's runtime that runs Codex once, as `codex exec`, on `prompt`.
///
/// Codex reads `prompt` on its standard input, in `workspace`, on `model` at `effort`, and sends
/// its tools as `tools` says; the runtime holds Codex's files, readable by the agent's user
/// alone, a command that ends with Codex's turn, and the deadline and grace. It refuses `tools`
/// of [`Tools::Functions`] for a `model` that the crate carries no catalog entry for.
pub fn runtime(
    workspace: Workspace,
    model: &ModelName,
    effort: ReasoningEffort,
    tools: Tools,
    prompt: &str,
    deadline_seconds: NonZeroU64,
    finalize_grace_seconds: u64,
) -> Result<AgentRuntime, String> {
    let Workspace {
        path,
        home,
        tmp,
        config: config_path,
        catalog: catalog_path,
    } = workspace;
    let content = match tools {
        Tools::Native => carried(model).ok().as_ref().map(catalog_of),
        Tools::Functions => Some(function_catalog(model)?),
    };
    let catalog = content.map(|content| ir::File {
        path: catalog_path,
        mode: ir::Mode::new(0o600),
        content,
    });
    let config = ir::File {
        path: config_path,
        mode: ir::Mode::new(0o600),
        content: config(model, effort, catalog.as_ref().map(|file| &file.path)),
    };
    // `exec` runs one turn, unattended, on the prompt it reads on its standard input (`-`), and
    // writes its events to its standard output as JSON lines (`--json`), which the run's record
    // keeps; it writes no session files (`--ephemeral`), reads no command rules
    // (`--ignore-rules`), and works in any directory, a Git repository or not.
    let codex = format!("{}/{}/bin/codex", harness_dir(), CODEX_0_167_0.name);
    let flags = [
        "--ephemeral",
        "--ignore-rules",
        "--skip-git-repo-check",
        "--json",
    ];
    let argv = [codex.as_str(), "exec"]
        .into_iter()
        .chain(flags)
        .chain(["-"]);
    let env = [("CODEX_HOME", &home), ("TMPDIR", &tmp)]
        .map(|(name, path)| checked(EnvVar::try_from(format!("{name}={path}"))));
    let dirs = [home, tmp].map(|path| Dir {
        path,
        mode: ir::Mode::new(0o700),
    });
    Ok(AgentRuntime {
        command: Command {
            argv: checked(Argv::try_from(argv.map(str::to_owned).collect::<Vec<_>>())),
            env: checked(Env::try_from(env.to_vec())),
            cwd: path,
        },
        dirs: dirs.to_vec(),
        files: [config].into_iter().chain(catalog).collect(),
        stdin: Some(prompt.to_owned()),
        deadline_seconds,
        finalize_grace_seconds,
    })
}

/// The text of Codex's `config.toml`, for `model` at `effort`, with the model catalog at
/// `catalog`, if it has one.
fn config(model: &ModelName, effort: ReasoningEffort, catalog: Option<&RolePath>) -> String {
    let catalog = catalog.map_or_else(String::new, |path| {
        format!("model_catalog_json = {}\n", toml_string(path.as_str()))
    });
    let threads = MAX_MODEL_REQUESTS_IN_FLIGHT - 1;
    let model_url = model_url();
    format!(
        r#"model = {model}
model_reasoning_effort = "{effort}"
{catalog}model_provider = "pumpkin"
# Nobody is there to approve a command, and the agent's sandbox confines Codex, whose own
# sandbox needs `bwrap`, which its pin leaves out.
approval_policy = "never"
sandbox_mode = "danger-full-access"
# A tool's shell reads no login profile, which could change the environment the command sets.
allow_login_shell = false
# Nothing reaches past the model route, the only way out of the sandbox.
web_search = "disabled"

# Codex's record of the run is the events it writes to its standard output, which the teardown
# collects. The databases it makes in its home as it starts stay there, uncollected: `exec`
# writes them no log, and its session, ephemeral, keeps nothing in them.

# The model endpoint `sandboxd` serves, which needs no credential.
[model_providers.pumpkin]
# Any name but `OpenAI` keeps off the features Codex keeps for the provider of that name.
name = "Pumpkin"
base_url = "{model_url}"
wire_api = "responses"
requires_openai_auth = false
# A run of hours meets transient failures.
request_max_retries = 100
stream_max_retries = 100

# The lead agent and as many spawned ones at most, each with one model request in flight at a
# time: as many requests as the model route takes from a sandbox at once.
[agents]
max_concurrent_threads_per_session = {threads}

[analytics]
enabled = false

[features]
plugins = false
apps = false
image_generation = false

[plugins._default]
enabled = false
"#,
        model = toml_string(model.as_str()),
        effort = effort.as_str(),
    )
}

/// Codex's catalog of `model` alone, its entry from [`CATALOG`] set to send every tool as a
/// `function` tool in the request's `tools`, as [`Tools::Functions`] says.
fn function_catalog(model: &ModelName) -> Result<String, String> {
    let mut entry = carried(model).map_err(|slugs| {
        format!(
            "Codex's catalog entry for `{model}` isn't carried, and sending its tools as \
             functions needs it: use {slugs}"
        )
    })?;
    // An `additional_tools` input item lists the tools in place of `tools`.
    entry["use_responses_lite"] = Value::Bool(false);
    // Code mode offers one `custom` tool, `exec`, which runs the others.
    entry["tool_mode"] = json!("direct");
    // `apply_patch` has no other form than `freeform`, a `custom` tool; the model runs it
    // through the shell, on whose `PATH` Codex puts it.
    entry["apply_patch_tool_type"] = Value::Null;
    // Sub-agents' tools are a `namespace` under either multi-agent version.
    entry["multi_agent_version"] = json!("disabled");
    // `clock` is a `namespace`, and `send_user_message_async` adds a second way to ask a user,
    // beside `request_user_input`, which stays.
    entry["experimental_supported_tools"] = json!([]);
    Ok(catalog_of(&entry))
}

fn catalog_of(entry: &Value) -> String {
    json!({ "models": [entry] }).to_string()
}

/// `model`'s entry in [`CATALOG`], or, if it has none, every slug it has, quoted and joined
/// with `or`.
fn carried(model: &ModelName) -> Result<Value, String> {
    let catalog: Option<Value> = serde_json::from_str(CATALOG).ok();
    let entries = (catalog.as_ref())
        .and_then(|catalog| catalog["models"].as_array())
        .and_then(|entries| {
            (entries.iter())
                .map(|entry| Some((entry["slug"].as_str()?, entry)))
                .collect::<Option<Vec<_>>>()
        })
        .filter(|entries| !entries.is_empty());
    let Some(entries) = entries else {
        unreachable!(
            "Codex's catalog is not a list of models, each with a slug, though it is a file of \
             this crate, built into it, which the tests read: they show its models' slugs and \
             the refusal that lists every slug"
        )
    };
    if let Some(&(_, entry)) = entries.iter().find(|(slug, _)| *slug == model.as_str()) {
        return Ok(entry.clone());
    }
    let slugs: Vec<String> = (entries.iter())
        .map(|(slug, _)| format!("`{slug}`"))
        .collect();
    Err(slugs.join(" or "))
}

fn below(workspace: &AbsolutePath, path: &str) -> Result<RolePath, String> {
    RolePath::try_from(format!(
        "{}/{path}",
        workspace.as_str().trim_end_matches('/')
    ))
}

/// `text` as a TOML basic string, in quotes, with each quote, backslash and control character
/// escaped.
fn toml_string(text: &str) -> String {
    let mut string = String::from('"');
    for char in text.chars() {
        match char {
            '"' | '\\' => {
                string.push('\\');
                string.push(char);
            }
            '\0'..='\x1f' | '\x7f' => {
                let _ = write!(string, "\\u{:04X}", u32::from(char));
            }
            _ => string.push(char),
        }
    }
    string.push('"');
    string
}

/// `value`, built from parsed paths and this module's constants, as valid.
fn checked<T>(value: Result<T, String>) -> T {
    match value {
        Ok(value) => value,
        Err(error) => unreachable!(
            "each value is built from parsed paths, which hold no NUL byte, and from this \
             module's constants: two variables' names, each set once, and a program that is not \
             empty; the tests build each; yet {error}"
        ),
    }
}
