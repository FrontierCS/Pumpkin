//! The machine's configuration, `pumpkin.toml`: [`Config`] for a machine whose runs go to its
//! own providers, or [`Client`] for one whose runs go to a controller.
//!
//! `pumpkin help config` is the home of what each key means and which are optional; this module
//! refuses any other key, and a file that mixes the two forms. A client's configuration names each
//! of its controllers in a table of its own, `[controllers.<name>]`, as cargo names each registry
//! in `[registries.<name>]` and takes one by `--registry`
//! ([config.md](https://github.com/rust-lang/cargo/blob/4bc8f24d3e899462e43621aab981f6383a370365/src/doc/src/reference/config.md#L870-L910)).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use engine::ContextName;
use gateway::GatewayAddress;
use route::{Headers, Key, Serving, UpstreamUrl};
use runtime::{Budgets, TokenPrices};
use schema::ir::{Api, ApiRoute, ModelName, ModelRoute, Name};
use schema::workflow::{ByRole, Workflow};
use serde::{Deserialize, Deserializer, Serialize, de};
use stdx::Context;

pub(crate) struct Config {
    pub(crate) docker: Docker,
    pub(crate) providers: Providers<Kind>,
    pub(crate) gateway: Gateway,
    pub(crate) upstreams: Upstreams,
}

/// A machine whose runs go to a controller: the one of its `[controllers.<name>]` that a command
/// goes to, and the `[upstreams]` and that controller's `modal` that each job is sent.
pub(crate) struct Client {
    pub(crate) controller: Controller,
    pub(crate) upstreams: Upstreams,
    /// The file that it was read from.
    pub(crate) path: PathBuf,
}

enum Target {
    Machine(Config),
    Controllers(Controllers),
}

/// A client's configuration: each controller that it names, at least one, by its name.
struct Controllers {
    by_name: BTreeMap<crate::controller::Name, Controller>,
    upstreams: Upstreams,
    path: PathBuf,
}

// No table that a controller's settings hold has `deny_unknown_fields`. So a job can ignore a key
// of a newer envelope's (`remove_unknown`). A machine still refuses it (`target`).
#[derive(Deserialize)]
struct Tables {
    docker: Option<Docker>,
    providers: Option<BTreeMap<Name, Kind>>,
    gateway: Option<Gateway>,
    #[serde(default)]
    upstreams: Upstreams,
    controllers: Option<BTreeMap<crate::controller::Name, ControllerTable>>,
}

/// A controller that a client names, `[controllers.<name>]`.
pub(crate) struct Controller {
    pub(crate) name: crate::controller::Name,
    pub(crate) find: Find,
    pub(crate) modal: Option<ClientModal>,
}

/// How a client finds its controller.
// The client names the server, as Coder's, Teleport's and GitLab's do:
// https://github.com/coder/coder/blob/3d58e6912a48f923e29a0739fbce5f8791ce0e53/cli/root.go#L62
// https://github.com/gravitational/teleport/blob/0c2973dbae77a1a19ebb8bb1d7f059659a674038/tool/tsh/tsh.go#L520
// https://gitlab.com/gitlab-org/omnibus-gitlab/-/blob/9b9a2e883910b2f996fd23f4845435e0cdc71550/doc/docker/README.md#L164-L186
#[derive(Clone)]
pub(crate) enum Find {
    /// By the tag of its EC2 instance in `region`, which a command starts if it is stopped.
    Ec2 { region: aws::Region },
    /// At a fixed address, which no command starts.
    Address(crate::controller::Address),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControllerTable {
    region: Option<aws::Region>,
    address: Option<crate::controller::Address>,
    modal: Option<ClientModal>,
}

impl Controller {
    fn of(name: crate::controller::Name, table: ControllerTable) -> Result<Self, String> {
        let ControllerTable {
            region,
            address,
            modal,
        } = table;
        let find = match (region, address) {
            (Some(region), None) => Find::Ec2 { region },
            (None, Some(address)) => Find::Address(address),
            (Some(_), Some(_)) | (None, None) => {
                return Err(format!(
                    "`[controllers.{name}]` names both or neither of `region` and `address`: name \
                     the `region` of a controller that deploy/aws made, or the `address` of one \
                     elsewhere"
                ));
            }
        };
        // A job gives Modal's sandboxes the address they dial from the instance's metadata, which
        // only an EC2 controller has.
        if let (Find::Address(_), Some(_)) = (&find, &modal) {
            return Err(format!(
                "`[controllers.{name}]` names `modal` beside `address`, but a Modal provider needs \
                 a controller that deploy/aws made: remove `[controllers.{name}.modal]`"
            ));
        }
        Ok(Self { name, find, modal })
    }
}

/// The Modal provider of a client's jobs. A job completes it with `images`, which the
/// controller's settings give, and `dials_from` ([`crate::job`]).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClientModal {
    profile: String,
    classes: BTreeMap<Name, modal::SandboxClass>,
}

/// What a client sends of its configuration with each job, which the job reads: its name for the
/// controller, which the job's refusals name, its upstreams, each with its key in a file of its own
/// beside this, named by [`key_file`], and its Modal provider's `profile` and `classes`.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Sent {
    pub(crate) controller: crate::controller::Name,
    #[serde(default)]
    pub(crate) upstreams: Upstreams,
    pub(crate) modal: Option<SentModal>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SentModal {
    profile: String,
    classes: BTreeMap<Name, modal::SandboxClass>,
}

// Without `deny_unknown_fields`, as `Tables`.
#[derive(Deserialize)]
pub(crate) struct Docker {
    pub(crate) context: ContextName,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    Docker(Empty),
    Ec2(ec2::Settings),
    Modal(modal::Settings),
}

// Without `deny_unknown_fields`, as `Tables`.
#[derive(Deserialize)]
#[serde(expecting = "an empty table, as the provider's kind takes no settings")]
pub(crate) struct Empty {}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Gateway {
    #[serde(default, deserialize_with = "address")]
    pub(crate) private: Option<GatewayAddress>,
}

pub(crate) struct Providers<T>(BTreeMap<Name, T>);

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct Upstreams(BTreeMap<Name, Upstream>);

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Upstream {
    #[serde(deserialize_with = "base")]
    url: String,
    // The configuration names a key by its file, and holds none, so that it can be shown and kept
    // whole.
    key_file: PathBuf,
    models: Vec<Model>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Model {
    name: ModelName,
    upstream_name: ModelName,
    #[serde(skip_serializing_if = "Option::is_none")]
    usd_per_mtok: Option<TokenPrices>,
}

impl Providers<Kind> {
    fn new(providers: BTreeMap<Name, Kind>) -> Result<Self, String> {
        let (mut docker, mut remote) = (Vec::new(), Vec::new());
        for (name, kind) in &providers {
            match kind {
                Kind::Docker(Empty {}) => docker.push(format!("`{name}`")),
                Kind::Ec2(_) | Kind::Modal(_) => remote.push(format!("`{name}`")),
            }
        }
        // Each would be the machine's one engine, under another name, and would list the others'
        // sandboxes of a run as its own.
        if let Some((last, others)) = docker.split_last()
            && !others.is_empty()
        {
            return Err(format!(
                "providers {} and {last} are each of kind `docker`, so each would be the machine's \
                 one Docker engine, `[docker]`'s: keep one of them",
                others.join(", ")
            ));
        }
        // A run places each role in the smallest class that fits it, of every provider's, so the
        // engine's one class beside other providers' would take whichever roles this machine's
        // memory happens to fit.
        if let [local] = &docker[..]
            && !remote.is_empty()
        {
            return Err(format!(
                "provider {local} is of kind `docker`, beside {}: a machine runs its roles either \
                 on its own engine or on other providers, so keep {local} alone, or remove it",
                remote.join(" and ")
            ));
        }
        Ok(Self(providers))
    }
}

impl<T> Providers<T> {
    pub(crate) fn by_name(&self) -> &BTreeMap<Name, T> {
        &self.0
    }

    pub(crate) fn into_by_name(self) -> BTreeMap<Name, T> {
        self.0
    }

    pub(crate) fn get(&self, name: &Name) -> Option<&T> {
        self.0.get(name)
    }
}

/// What the model routes of a workflow get of a configuration's upstreams.
pub(crate) struct Models {
    /// The upstream for each entry of each role's route, in the route's order.
    pub(crate) servings: ByRole<Vec<Serving>>,
    pub(crate) budgets: Budgets,
    pub(crate) prices: BTreeMap<ModelName, TokenPrices>,
}

impl Upstreams {
    /// What the model routes of `workflow` get of these upstreams. Fails unless an upstream
    /// serves every model of each entry of each role's route, and prices every kind of token of
    /// each model that a budget counts.
    pub(crate) fn models(&self, workflow: &Workflow) -> Result<Models, String> {
        let servings = workflow.by_role(|(role, _)| match workflow.model(role) {
            Some(route) => self.serving(route),
            None => Ok(Vec::new()),
        })?;
        let prices = self.prices();
        let budgets = runtime::budgets(workflow, &prices)?;
        Ok(Models {
            servings,
            budgets,
            prices,
        })
    }

    /// The upstream for each entry of `route`, in its order: the one that serves every model the
    /// entry allows. Fails if there is none.
    fn serving(&self, route: &ModelRoute) -> Result<Vec<Serving>, String> {
        route
            .as_slice()
            .iter()
            .map(|entry| self.serves(entry))
            .collect()
    }

    fn prices(&self) -> BTreeMap<ModelName, TokenPrices> {
        let models = self.0.values().flat_map(|upstream| &upstream.models);
        let priced = models.filter_map(|model| Some((model.name.clone(), model.usd_per_mtok?)));
        priced.collect()
    }

    /// The key files that arrive with a job, in upstream order.
    pub(crate) fn key_files(&self) -> impl ExactSizeIterator<Item = String> {
        self.0.keys().map(key_file)
    }

    /// Points each upstream's `key_file` at its key in `dir`, where a client sends it with a job.
    pub(crate) fn keys_in(&mut self, dir: &Path) {
        for (name, upstream) in &mut self.0 {
            upstream.key_file = dir.join(key_file(name));
        }
    }

    fn serves(&self, entry: &ApiRoute) -> Result<Serving, String> {
        let ApiRoute {
            api,
            models,
            request_headers,
            response_headers,
            budget: _,
        } = entry;
        let serves = |upstream: &Upstream| {
            let served =
                |model: &ModelName| upstream.models.iter().any(|served| served.name == *model);
            models.as_slice().iter().all(served)
        };
        let fix = if self.0.is_empty() {
            "add an `[upstreams.<name>]` that lists them"
        } else {
            "list them all under one"
        };
        // At most one does: each model is listed once, and an entry allows one at least.
        let Some((name, upstream)) = self.0.iter().find(|(_, upstream)| serves(upstream)) else {
            let named: Vec<String> = models
                .as_slice()
                .iter()
                .map(|model| format!("`{model}`"))
                .collect();
            return Err(format!(
                "no upstream serves every model that the workflow's route allows, {}: {fix}",
                named.join(", ")
            ));
        };
        let Upstream {
            url,
            key_file,
            models,
        } = upstream;
        let models = models
            .iter()
            .map(|model| (model.name.clone(), model.upstream_name.clone()));
        Ok(Serving {
            name: name.clone(),
            api: *api,
            url: UpstreamUrl::try_from(endpoint(url, *api))?,
            key: Key::read(key_file)?,
            models: models.collect(),
            headers: Headers {
                request: request_headers.clone(),
                response: response_headers.clone(),
            },
        })
    }
}

fn base<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let url = String::deserialize(deserializer)?;
    // The path added would end up within a query or a fragment. The URL itself isn't repeated, as
    // `UpstreamUrl` doesn't repeat it, since it may name a user's credential.
    if url.contains(['?', '#']) {
        return Err(de::Error::custom(
            "the upstream's URL has a query or a fragment: name the API's base URL alone, below \
             which the route reaches each API's endpoint",
        ));
    }
    let base = url.strip_suffix('/').unwrap_or(&url).to_owned();
    UpstreamUrl::try_from(base.clone()).map_err(de::Error::custom)?;
    Ok(base)
}

fn endpoint(base: &str, api: Api) -> String {
    let path = match api {
        Api::Responses => "/responses",
        Api::Chat => "/chat/completions",
        Api::Messages => "/messages",
    };
    format!("{base}{path}")
}

fn address<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<GatewayAddress>, D::Error> {
    let address = String::deserialize(deserializer)?;
    let parsed: SocketAddr = address.parse().map_err(|_| {
        de::Error::custom(format!(
            "`{}` is not an IP address and a port: name one such as `172.31.0.10:7433`",
            address.escape_debug()
        ))
    })?;
    GatewayAddress::try_from(parsed)
        .map(Some)
        .map_err(de::Error::custom)
}

/// The machine's configuration in the file at `path`. Fails, naming the file, if it isn't one, as
/// when it names `[controllers]`.
pub(crate) fn read(path: &Path) -> Result<Config, String> {
    parse(path, &text(path)?)
}

fn text(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|error| {
        format!(
            "the configuration {} can't be read ({error}): write one there, as \
             `pumpkin help config` says, or name another file with `--config`",
            path.display()
        )
    })
}

/// The upstreams of the configuration in the file at `path`, whether its runs stay on this machine
/// or go to a controller. Fails, naming the file, if it isn't a configuration.
pub(crate) fn upstreams(path: &Path) -> Result<Upstreams, String> {
    match target(path, &text(path)?)? {
        Target::Machine(Config { upstreams, .. })
        | Target::Controllers(Controllers { upstreams, .. }) => Ok(upstreams),
    }
}

/// The machine's configuration that `text`, the file at `path`, holds, failing as [`read`] does.
pub(crate) fn parse(path: &Path, text: &str) -> Result<Config, String> {
    match target(path, text)? {
        Target::Machine(config) => Ok(config),
        Target::Controllers(_) => Err(format!(
            "{}: it names `[controllers]`, and so no Docker engine of this machine's: name a \
             configuration with `[docker]` with `--config`",
            path.display()
        )),
    }
}

/// The client's configuration at `config`, or at the default path, with the controller that
/// `controller` names, or the one it names if it names one alone. `None` for a machine whose runs
/// don't go to a controller: the file can't be read, or names no `[controllers]`, and `controller`
/// is `None`. Fails, naming the file, if it names `[controllers]` but isn't a client's
/// configuration, if it names several and `controller` is `None`, or if it names none that
/// `controller` names.
pub(crate) fn client(
    config: Option<&str>,
    controller: Option<&crate::controller::Name>,
) -> Result<Option<Client>, String> {
    let path = match (config, controller) {
        (Some(config), _) => PathBuf::from(config),
        (None, Some(_)) => crate::default_config()?,
        // Without `HOME` there is no default one to send runs anywhere.
        (None, None) => match crate::default_config() {
            Ok(path) => path,
            Err(_) => return Ok(None),
        },
    };
    let text = match (fs::read_to_string(&path), controller) {
        (Ok(text), _) => text,
        (Err(error), Some(name)) => {
            return Err(format!(
                "the configuration {} can't be read ({error}): write one there that names \
                 `[controllers.{name}]`, as `pumpkin help config` says",
                path.display()
            ));
        }
        // What can't be read is left to the command that reads it, which says why.
        (Err(_), None) => return Ok(None),
    };
    let target = match target(&path, &text) {
        Ok(target) => target,
        Err(error) => {
            let raw = toml::from_str::<toml::Table>(&text);
            let named = |raw: &toml::Table| {
                ["controllers", "controller"]
                    .iter()
                    .any(|key| raw.contains_key(*key))
            };
            if controller.is_some() || raw.is_ok_and(|raw| named(&raw)) {
                return Err(error);
            }
            return Ok(None);
        }
    };
    match (target, controller) {
        (Target::Controllers(controllers), named) => chosen(controllers, named).map(Some),
        (Target::Machine(_), None) => Ok(None),
        (Target::Machine(_), Some(name)) => Err(format!(
            "{}: it names no `[controllers]`, so no controller `{name}`, as its runs go to its own \
             providers: leave out `--controller`, or name `[controllers.{name}]` there",
            path.display()
        )),
    }
}

/// The client of the controller of `controllers` that `name` names, or of its one controller
/// without a name. Fails, listing the names, if it names several and `name` is `None`, or none
/// that `name` names.
// As kubectl and docker take a context by `--context`. With several and no flag, no default is
// guessed, and no variable of the environment names one.
// https://github.com/kubernetes/cli-runtime/blob/e328bc3797cf67493003ce68c56c98d405cc63e4/pkg/genericclioptions/config_flags.go#L418
// https://github.com/docker/cli/blob/7287ab389066e1e8eefa94e240d013fd93d37ea0/cli/flags/common.go#L74-L75
fn chosen(
    controllers: Controllers,
    name: Option<&crate::controller::Name>,
) -> Result<Client, String> {
    let Controllers {
        mut by_name,
        upstreams,
        path,
    } = controllers;
    let names: Vec<String> = by_name.keys().map(|name| format!("`{name}`")).collect();
    let picked = match name {
        Some(name) => by_name.remove(name),
        None if by_name.len() == 1 => by_name.into_values().next(),
        None => None,
    };
    let Some(controller) = picked else {
        return Err(match name {
            Some(name) => format!(
                "{}: it names no controller `{name}`: name one of {} with `--controller`",
                path.display(),
                names.join(", ")
            ),
            None => format!(
                "{}: it names several controllers, {}: name one with `--controller`",
                path.display(),
                names.join(", ")
            ),
        });
    };
    Ok(Client {
        controller,
        upstreams,
        path,
    })
}

fn target(path: &Path, text: &str) -> Result<Target, String> {
    // `toml` ends its message, which quotes the line, with a newline of its own.
    let shown = |error: toml::de::Error| error.to_string().trim_end().to_owned();
    let parsed = toml::Deserializer::parse(text).and_then(tables);
    let (
        Tables {
            docker,
            providers,
            gateway,
            mut upstreams,
            controllers,
        },
        unknown,
    ) = parsed.map_err(shown).context(path.display())?;
    known(path, text, &unknown)?;
    unique(&upstreams).context(path.display())?;
    // Below the file's directory, rather than the working directory or `HOME`, so that a
    // configuration and its keys move together, and no variable of the environment changes which
    // file a key is in. A file that was read has a parent: `""` for a bare file name.
    let dir = path.parent().unwrap_or(Path::new(""));
    for upstream in upstreams.0.values_mut() {
        upstream.key_file = dir.join(&upstream.key_file);
    }
    if let Some(controllers) = controllers {
        let beside: Vec<&str> = [
            ("`[docker]`", docker.is_some()),
            ("`[providers]`", providers.is_some()),
            ("`[gateway]`", gateway.is_some()),
        ]
        .into_iter()
        .filter_map(|(table, given)| given.then_some(table))
        .collect();
        if !beside.is_empty() {
            return Err(format!(
                "{}: it names `[controllers]` beside {}: a machine's runs go either to its \
                 controllers or to its own providers, so keep `[controllers]`, beside \
                 `[upstreams]`, or remove it",
                path.display(),
                beside.join(" and ")
            ));
        }
        if controllers.is_empty() {
            return Err(format!(
                "{}: `[controllers]` names no controller: name each in `[controllers.<name>]`, \
                 as `pumpkin help config` says",
                path.display()
            ));
        }
        let controllers = (controllers.into_iter())
            .map(|(name, table)| Ok((name.clone(), Controller::of(name, table)?)))
            .collect::<Result<_, String>>()
            .context(path.display())?;
        return Ok(Target::Controllers(Controllers {
            by_name: controllers,
            upstreams,
            path: path.to_owned(),
        }));
    }
    let missing = |table: &str| {
        format!(
            "{}: `[{table}]` is missing: name it, or `[controllers.<name>]`, as `pumpkin help \
             config` says",
            path.display()
        )
    };
    let docker = docker.ok_or_else(|| missing("docker"))?;
    let providers = providers.ok_or_else(|| missing("providers.<name>.<kind>"))?;
    let providers = Providers::new(providers).context(path.display())?;
    Ok(Target::Machine(Config {
        docker,
        providers,
        gateway: gateway.unwrap_or_default(),
        upstreams,
    }))
}

/// Fails if `unknown` names any key, naming `path` and each key. `unknown` holds the keys that no
/// table of the configuration `text` takes.
fn known(path: &Path, text: &str, unknown: &[Vec<Step>]) -> Result<(), String> {
    if unknown.is_empty() {
        return Ok(());
    }
    let document = toml::from_str(text).map_err(|error| error.to_string().trim_end().to_owned());
    let mut document = document.context(path.display())?;
    if let toml::Value::Table(tables) = &document
        && let Some(single) = tables.get("controller")
    {
        return Err(format!("{}: {}", path.display(), single_controller(single)));
    }
    let names = removed(&mut document, unknown);
    let them = if names.len() == 1 { "it" } else { "them" };
    Err(format!(
        "{}: no table of a configuration takes {}: remove {them}, as `pumpkin help config` says \
         what each table takes",
        path.display(),
        names.join(", ")
    ))
}

/// Removes from `tables` each key that no table of a configuration takes, and returns their names,
/// as [`removed`] gives them. Fails if `tables` isn't a configuration once they are gone. The error
/// says where as `toml` does: by the keys that lead there, without an enum's variant, such as a
/// provider's kind.
pub(crate) fn remove_unknown(tables: &mut toml::Table) -> Result<Vec<String>, String> {
    // `toml` puts the keys on a line of their own, and ends with a newline.
    let shown = |error: toml::de::Error| error.to_string().trim_end().replace('\n', " ");
    let (_, unknown) = self::tables(tables.clone()).map_err(shown)?;
    let mut document = toml::Value::Table(std::mem::take(tables));
    let names = removed(&mut document, &unknown);
    let toml::Value::Table(document) = document else {
        unreachable!("`removed` keeps the top of the document a table")
    };
    *tables = document;
    Ok(names)
}

/// The edit that a configuration with `[controller]`, the table `single`, needs.
fn single_controller(single: &toml::Value) -> String {
    let name = single.get("name").and_then(toml::Value::as_str);
    let name = name.filter(|name| crate::controller::Name::try_from((*name).to_owned()).is_ok());
    let name = name.unwrap_or("<name>");
    format!(
        "a configuration names each controller in `[controllers.<name>]`, not `[controller]`: \
         rename `[controller]` to `[controllers.{name}]`, `[controller.modal]` to \
         `[controllers.{name}.modal]` and so on, and remove `name`"
    )
}

// A step from a value into one it holds. It goes by a table's key, by an array's index, or into the
// value of an enum's variant. An enum's variant is its table's one key.
enum Step {
    Key(String),
    Index(usize),
    Variant,
}

/// The tables of the configuration that `deserializer` reads, and each key in it that no table
/// takes, as the steps that lead to it from the top.
fn tables<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<(Tables, Vec<Vec<Step>>), D::Error> {
    let mut unknown = Vec::new();
    // Cargo reads a manifest this way, and warns of the keys it doesn't know rather than refusing
    // them:
    // https://github.com/rust-lang/cargo/blob/111443f3dc4d82a65e2306a93028f122406401c1/src/workspace/parser/mod.rs#L192-L206
    let tables = serde_ignored::deserialize(deserializer, |path| {
        let mut steps = Vec::new();
        let mut at = &path;
        loop {
            at = match at {
                serde_ignored::Path::Root => break,
                serde_ignored::Path::Map { parent, key } => {
                    steps.push(Step::Key(key.clone()));
                    parent
                }
                serde_ignored::Path::Seq { parent, index } => {
                    steps.push(Step::Index(*index));
                    parent
                }
                serde_ignored::Path::NewtypeVariant { parent } => {
                    steps.push(Step::Variant);
                    parent
                }
                serde_ignored::Path::Some { parent }
                | serde_ignored::Path::NewtypeStruct { parent } => parent,
            };
        }
        steps.reverse();
        unknown.push(steps);
    })?;
    Ok((tables, unknown))
}

/// Removes from `document` each key that `unknown` leads to, as [`tables`] found them there.
/// Returns their names, each set as code. A name joins the keys from the top by `.`, with an
/// array's index in `[]`.
fn removed(document: &mut toml::Value, unknown: &[Vec<Step>]) -> Vec<String> {
    let mut names = Vec::new();
    for steps in unknown {
        let mut name = String::new();
        let dotted = |name: &mut String, key: &str| {
            if !name.is_empty() {
                name.push('.');
            }
            name.push_str(key);
        };
        let Some((Step::Key(last), steps)) = steps.split_last() else {
            unreachable!("serde_ignored names only a table's entry as ignored, by its key")
        };
        let mut value = &mut *document;
        for step in steps {
            let next = match (step, value) {
                (Step::Key(key), toml::Value::Table(table)) => {
                    dotted(&mut name, key);
                    table.get_mut(key)
                }
                (Step::Variant, toml::Value::Table(table)) => {
                    table.iter_mut().next().map(|(key, value)| {
                        dotted(&mut name, key);
                        value
                    })
                }
                (Step::Index(index), toml::Value::Array(array)) => {
                    let _ = write!(name, "[{index}]");
                    array.get_mut(*index)
                }
                _ => None,
            };
            value = next.unwrap_or_else(|| {
                unreachable!("each step that serde_ignored took is in the document it read")
            });
        }
        let toml::Value::Table(table) = value else {
            unreachable!("serde_ignored names only a table's entry as ignored")
        };
        table.remove(last);
        dotted(&mut name, last);
        names.push(format!("`{name}`"));
    }
    names
}

impl Client {
    /// What a job is sent of this configuration and the source of each key in upstream order.
    pub(crate) fn sent(&self) -> (Sent, Vec<PathBuf>) {
        let mut upstreams = self.upstreams.clone();
        let mut keys = Vec::new();
        for (name, upstream) in &mut upstreams.0 {
            keys.push(std::mem::replace(
                &mut upstream.key_file,
                key_file(name).into(),
            ));
        }
        let modal = (self.controller.modal.as_ref()).map(|modal| {
            let ClientModal { profile, classes } = modal;
            SentModal {
                profile: profile.clone(),
                classes: classes.clone(),
            }
        });
        let controller = self.controller.name.clone();
        let sent = Sent {
            controller,
            upstreams,
            modal,
        };
        (sent, keys)
    }

    /// The contents of Modal's configuration file, `~/.modal.toml`. `None` if the controller has
    /// no `modal` or the file doesn't exist.
    pub(crate) fn modal_file(&self) -> Result<Option<Vec<u8>>, String> {
        if self.controller.modal.is_none() {
            return Ok(None);
        }
        let name = &self.controller.name;
        let path =
            crate::home(&format!("remove `[controllers.{name}.modal]`"))?.join(".modal.toml");
        if !path.try_exists().context(path.display())? {
            return Ok(None);
        }
        let read = fs::read(&path).map_err(|error| {
            format!(
                "Modal's configuration file {} can't be read ({error}): let this user read it",
                path.display()
            )
        })?;
        Ok(Some(read))
    }
}

/// The name of the file that a job is sent the key of upstream `name` in.
fn key_file(name: &Name) -> String {
    format!("{name}.key")
}

fn unique(upstreams: &Upstreams) -> Result<(), String> {
    let mut models = BTreeSet::new();
    for upstream in upstreams.0.values() {
        let listed = &upstream.models;
        if let Some(model) = listed.iter().find(|model| !models.insert(&model.name)) {
            return Err(format!(
                "`{}` is listed more than once: list each model once, under the upstream that \
                 serves it",
                model.name.as_str().escape_debug()
            ));
        }
    }
    Ok(())
}
