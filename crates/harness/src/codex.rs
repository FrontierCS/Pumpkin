//! Codex as a harness: [`CODEX_0_167_0`] pins the Codex that agents run, and [`Codex`] is one
//! unattended run of it, which [`Codex::runtime`] writes as the agent's runtime.
//!
//! Codex works in its workspace, and keeps its home (`CODEX_HOME`), with its configuration and
//! its databases, and its temporary files (`TMPDIR`) in two directories of their own there. Its
//! record of the run is the events it writes to its standard output as JSON lines, which the
//! teardown collects as `command.stdout` ([`command_stdout()`]). It reaches the model only at
//! the endpoint `sandboxd` serves ([`MODEL_PORT`]), with no credential: the host holds the key.
//!
//! [`command_stdout()`]: schema::sandbox::command_stdout

use std::fmt::Write as _;

use schema::ir::{
    self, AbsolutePath, AgentRuntime, Argv, Command, Dir, Env, EnvVar, ModelName, RolePath,
};
use schema::sandbox::{MAX_MODEL_REQUESTS_IN_FLIGHT, MODEL_PORT, harness_dir};
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

/// The catalog entries the crate carries: `gpt-6.1-terra`'s, unchanged, from Codex 0.167.0's own
/// model catalog, `codex-rs/models-manager/models.json` at its tag `rust-v0.167.0`, which a new
/// pin takes again from its own version; and `glm-5.3`'s, for a model Codex's own catalog lacks,
/// from Z.ai's `models.json` for Codex (<https://docs.z.ai/devpack/tool/codex.md>), the entry
/// Z.ai's API serves at `/api/v1/models`, with one key added: `model_messages`, holding as
/// `instructions_template` the base instructions Codex sends a model with none of its own,
/// since Z.ai's entry gives Codex none: `codex-rs/models-manager/prompt.md` at the same tag
/// (`BASE_INSTRUCTIONS`, used by `model_info_from_slug`, in `src/model_info.rs`), run through
/// `without_update_plan_instructions` (`codex-rs/prompts/src/update_plan_instructions.rs`), as
/// Codex does when it has no catalog file and `update_plan` is off, its default.
///
/// OpenAI Codex, Copyright 2025 OpenAI. Its entries are under the Apache License, Version 2.0,
/// a copy of which is `codex/LICENSE`.
const CATALOG: &str = include_str!("codex/catalog.json");

/// One unattended run of Codex, `codex exec`, in a workspace, on the prompt it reads on its
/// standard input, until it ends its turn.
#[derive(Clone, Debug)]
pub struct Codex {
    workspace: AbsolutePath,
    /// `CODEX_HOME`, `.codex` in the workspace. It can't be in the agent's [`log_dir()`], where
    /// a workflow puts nothing, and where the links Codex keeps in it would fail the collect.
    ///
    /// [`log_dir()`]: schema::sandbox::log_dir
    home: RolePath,
    /// `TMPDIR`, `.tmp` in the workspace, beside the home: Codex refuses to make its helpers'
    /// links in a home below its temporary directory.
    tmp: RolePath,
    /// `config.toml` in the home.
    config: ir::File,
    /// The model catalog in the home: the model's carried entry, which [`Tools::Functions`]
    /// needs, and which [`Tools::Native`] takes unchanged if the crate carries it.
    catalog: Option<ir::File>,
}

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

impl Codex {
    /// Codex working in `workspace`, asking for `model` at `effort`, and sending its tools as
    /// `tools` says.
    ///
    /// # Errors
    ///
    /// If `workspace` can't hold Codex's directories, which must be [`RolePath`]s, or if `tools`
    /// is [`Tools::Functions`] and the crate carries no entry of Codex's catalog for `model`.
    pub fn new(
        workspace: AbsolutePath,
        model: &ModelName,
        effort: ReasoningEffort,
        tools: Tools,
    ) -> Result<Self, String> {
        let home = below(&workspace, ".codex")?;
        let tmp = below(&workspace, ".tmp")?;
        let content = match tools {
            Tools::Native => carried(model).ok().as_ref().map(catalog_of),
            Tools::Functions => Some(function_catalog(model)?),
        };
        let catalog = match content {
            Some(content) => Some(ir::File {
                path: below(&workspace, ".codex/model-catalog.json")?,
                mode: mode("0600"),
                content,
            }),
            None => None,
        };
        let config = ir::File {
            path: below(&workspace, ".codex/config.toml")?,
            mode: mode("0600"),
            content: config(model, effort, catalog.as_ref().map(|file| &file.path)),
        };
        Ok(Self {
            workspace,
            home,
            tmp,
            config,
            catalog,
        })
    }

    /// The agent's runtime that runs Codex once on `prompt`: Codex's directories and files, which
    /// only the agent's user may read, and the command, which ends when Codex ends its turn.
    pub fn runtime(&self, prompt: &str) -> AgentRuntime {
        let Self {
            workspace,
            home,
            tmp,
            config,
            catalog,
        } = self;
        // `exec` runs one turn, unattended, on the prompt it reads on its standard input (`-`),
        // and writes its events to its standard output as JSON lines (`--json`), which the
        // run's record keeps; it writes no session files (`--ephemeral`), reads no command rules
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
        let env = [("CODEX_HOME", home), ("TMPDIR", tmp)]
            .map(|(name, path)| checked(EnvVar::try_from(format!("{name}={path}"))));
        let dirs = [home, tmp].map(|path| Dir {
            path: path.clone(),
            mode: mode("0700"),
        });
        AgentRuntime {
            command: Command {
                argv: checked(Argv::try_from(argv.map(str::to_owned).collect::<Vec<_>>())),
                env: checked(Env::try_from(env.to_vec())),
                cwd: workspace.clone(),
            },
            dirs: dirs.to_vec(),
            files: [config].into_iter().chain(catalog).cloned().collect(),
            stdin: Some(prompt.to_owned()),
        }
    }
}

/// The text of Codex's `config.toml`, for `model` at `effort`, with the model catalog at
/// `catalog`, if it has one.
fn config(model: &ModelName, effort: ReasoningEffort, catalog: Option<&RolePath>) -> String {
    let catalog = catalog.map_or_else(String::new, |path| {
        format!("model_catalog_json = {}\n", toml_string(path.as_str()))
    });
    let threads = MAX_MODEL_REQUESTS_IN_FLIGHT - 1;
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
base_url = "http://127.0.0.1:{MODEL_PORT}"
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

/// A Codex catalog that lists `entry` alone.
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

/// `path` in `workspace`.
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

/// The permission bits `text` writes, one of this module's.
fn mode(text: &str) -> ir::Mode {
    checked(ir::Mode::try_from(text.to_owned()))
}

/// `value`, built from parsed paths and this module's constants, as valid.
fn checked<T>(value: Result<T, String>) -> T {
    match value {
        Ok(value) => value,
        Err(error) => unreachable!(
            "each value is built from parsed paths, which hold no NUL byte, and from this \
             module's constants: a mode, `0` and three octal digits; two variables' names, each \
             set once; and a program that is not empty; the tests build each; yet {error}"
        ),
    }
}
