//! The machine's configuration, `pumpkin.toml`: [`Config`] for a machine whose runs go to its
//! own providers, or [`Client`] for one whose runs go to a controller.
//!
//! `pumpkin help config` is the home of what each key means and which are optional; this module
//! refuses any other key, and a file that mixes the two forms.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use engine::ContextName;
use gateway::GatewayAddress;
use route::{Headers, Key, Serving, UpstreamUrl};
use runtime::TokenPrices;
use schema::ir::{Api, ApiRoute, ModelName, ModelRoute, Name};
use schema::workflow::{ByProvider, Placed};
use serde::{Deserialize, Deserializer, Serialize, de};
use stdx::Context;

pub(crate) struct Config {
    pub(crate) docker: Docker,
    pub(crate) providers: Providers<Kind>,
    pub(crate) gateway: Gateway,
    pub(crate) upstreams: Upstreams,
}

/// A machine whose runs go to a controller: its `[controller]`, and the `[upstreams]` and
/// `[controller.modal]` that each job is sent.
pub(crate) struct Client {
    pub(crate) controller: Controller,
    pub(crate) upstreams: Upstreams,
    /// The file that it was read from.
    pub(crate) path: PathBuf,
}

enum Target {
    Machine(Config),
    Controller(Client),
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
    controller: Option<Controller>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Controller {
    pub(crate) name: crate::controller::Name,
    pub(crate) region: aws::Region,
    pub(crate) modal: Option<ClientModal>,
}

/// The Modal provider of a client's jobs. A job completes it with `images`, which the
/// controller's settings give, and `dials_from` ([`crate::job`]).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClientModal {
    profile: String,
    classes: BTreeMap<Name, modal::SandboxClass>,
}

/// What a client sends of its configuration with each job, which the job reads: its upstreams,
/// each with its key in a file of its own beside this, named by [`key_file`], and its Modal
/// provider's `profile` and `classes`.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Sent {
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
    /// The providers, each made a `U` by `make` from its name and its `T`, in the order of their
    /// names. Fails as the first `make` that fails, making no more.
    pub(crate) fn make<U>(
        self,
        mut make: impl FnMut(&Name, T) -> Result<U, String>,
    ) -> Result<Providers<U>, String> {
        let providers =
            (self.0.into_iter()).map(|(name, provider)| Ok((name.clone(), make(&name, provider)?)));
        providers.collect::<Result<_, String>>().map(Providers)
    }

    /// The providers that `placed` places a role on, each made a `U` by `make` from its name and
    /// its `T`, in the order of their names; the others are dropped unmade. Fails as the first
    /// `make` that fails, making no more.
    ///
    /// # Panics
    ///
    /// `placed` places a role on a provider that these lack.
    pub(crate) fn placed<U>(
        self,
        placed: &Placed,
        mut make: impl FnMut(&Name, T) -> Result<U, String>,
    ) -> Result<ByProvider<U>, String> {
        let mut providers = self.0;
        placed.by_provider(|name| {
            let Some(provider) = providers.remove(name) else {
                unreachable!(
                    "a role is placed on provider `{name}`, which these lack, though the caller \
                     placed the roles on these"
                );
            };
            make(name, provider)
        })
    }

    pub(crate) fn by_name(&self) -> &BTreeMap<Name, T> {
        &self.0
    }

    pub(crate) fn get(&self, name: &Name) -> Option<&T> {
        self.0.get(name)
    }
}

impl Upstreams {
    /// The upstream for each entry of `route`, in its order: the one that serves every model the
    /// entry allows. Fails if there is none.
    pub(crate) fn serving(&self, route: &ModelRoute) -> Result<Vec<Serving>, String> {
        route
            .as_slice()
            .iter()
            .map(|entry| self.serves(entry))
            .collect()
    }

    pub(crate) fn prices(&self) -> BTreeMap<ModelName, TokenPrices> {
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
/// when it names `[controller]`.
pub(crate) fn read(path: &Path) -> Result<Config, String> {
    let text = fs::read_to_string(path).map_err(|error| {
        format!(
            "the configuration {} can't be read ({error}): write one there, as \
             `pumpkin help config` says, or name another file with `--config`",
            path.display()
        )
    })?;
    parse(path, &text)
}

/// The machine's configuration that `text`, the file at `path`, holds, failing as [`read`] does.
pub(crate) fn parse(path: &Path, text: &str) -> Result<Config, String> {
    match target(path, text)? {
        Target::Machine(config) => Ok(config),
        Target::Controller(_) => Err(format!(
            "{}: it names `[controller]`, and so no Docker engine of this machine's: name a \
             configuration with `[docker]` with `--config`",
            path.display()
        )),
    }
}

/// The client's configuration at `config`, or at the default path, and `None` for a machine whose
/// runs don't go to a controller: the file can't be read, or doesn't name `[controller]`. Fails,
/// naming the file, if it names `[controller]` but isn't a client's configuration.
pub(crate) fn client(config: Option<&str>) -> Result<Option<Client>, String> {
    let path = match config {
        Some(config) => PathBuf::from(config),
        // Without `HOME` there is no default one to send runs anywhere.
        None => match crate::default_config() {
            Ok(path) => path,
            Err(_) => return Ok(None),
        },
    };
    // What can't be read is left to the command that reads it, which says why.
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(None);
    };
    match target(&path, &text) {
        Ok(Target::Controller(client)) => Ok(Some(client)),
        Ok(Target::Machine(_)) => Ok(None),
        Err(error) => {
            let raw = toml::from_str::<toml::Table>(&text);
            if raw.is_ok_and(|raw| raw.contains_key("controller")) {
                return Err(error);
            }
            Ok(None)
        }
    }
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
            controller,
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
    if let Some(controller) = controller {
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
                "{}: it names `[controller]` beside {}: a machine's runs go either to its \
                 controller or to its own providers, so keep `[controller]`, beside \
                 `[upstreams]`, or remove it",
                path.display(),
                beside.join(" and ")
            ));
        }
        return Ok(Target::Controller(Client {
            controller,
            upstreams,
            path: path.to_owned(),
        }));
    }
    let missing = |table: &str| {
        format!(
            "{}: `[{table}]` is missing: name it, or `[controller]`, as `pumpkin help config` \
             says",
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
    let names = removed(&mut document.context(path.display())?, unknown);
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
        (Sent { upstreams, modal }, keys)
    }

    /// The contents of Modal's configuration file, `~/.modal.toml`. `None` if `[controller.modal]`
    /// is absent or the file doesn't exist.
    pub(crate) fn modal_file(&self) -> Result<Option<Vec<u8>>, String> {
        if self.controller.modal.is_none() {
            return Ok(None);
        }
        let path = crate::home("remove `[controller.modal]`")?.join(".modal.toml");
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
