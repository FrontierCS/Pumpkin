//! The machine's configuration, `pumpkin.toml`: [`Config`] for a machine whose runs go to its
//! own providers, or [`Client`] for one whose runs go to a controller.
//!
//! `pumpkin help config` is the home of what each key means and which are optional; this module
//! refuses any other key, and a file that mixes the two forms.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use docker::CpuList;
use engine::ContextName;
use gateway::GatewayAddress;
use route::{Headers, Key, Serving, UpstreamUrl};
use runtime::TokenPrices;
use schema::ir::{Api, ApiRoute, ModelName, ModelRoute, Name};
use serde::{Deserialize, Deserializer, de};
use stdx::Context;

pub(crate) struct Config {
    pub(crate) docker: Docker,
    pub(crate) providers: Providers<Kind>,
    pub(crate) gateway: Gateway,
    pub(crate) upstreams: Upstreams,
    pub(crate) publish: Option<crate::publish::Settings>,
}

/// A machine whose runs go to a controller: its `[controller]`, and the `[upstreams]` and
/// `[controller.modal]` that each job is sent.
pub(crate) struct Client {
    pub(crate) controller: Controller,
    /// The `[upstreams]` tables as the file holds them.
    upstreams: toml::Table,
    /// The file's directory, which each `key_file` is below unless absolute.
    dir: PathBuf,
}

enum Target {
    Machine(Config),
    Controller(Client),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tables {
    docker: Option<Docker>,
    providers: Option<BTreeMap<Name, Kind>>,
    gateway: Option<Gateway>,
    #[serde(default)]
    upstreams: Upstreams,
    controller: Option<Controller>,
    publish: Option<crate::publish::Settings>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Controller {
    pub(crate) name: crate::controller::Name,
    pub(crate) region: aws::Region,
    pub(crate) modal: Option<ClientModal>,
}

/// The Modal provider of a client's jobs. A job completes it with `images`, which the
/// controller's settings give, and `dials_from` ([`crate::job`]), and so checks `profile` and
/// `classes` there, where they are the provider's own settings.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClientModal {
    config: Option<PathBuf>,
    profile: String,
    classes: toml::Table,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Docker {
    pub(crate) context: ContextName,
    pub(crate) cpus: Option<CpuList>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub(crate) enum Kind {
    Docker(Empty),
    Ec2(ec2::Settings),
    Modal(modal::Settings),
}

#[derive(Deserialize)]
#[serde(
    deny_unknown_fields,
    expecting = "an empty table, as the provider's kind takes no settings"
)]
pub(crate) struct Empty {}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Gateway {
    #[serde(default, deserialize_with = "address")]
    pub(crate) private: Option<GatewayAddress>,
}

pub(crate) struct Providers<T>(BTreeMap<Name, T>);

#[derive(Default, Deserialize)]
#[serde(transparent)]
pub(crate) struct Upstreams(BTreeMap<Name, Upstream>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Upstream {
    #[serde(deserialize_with = "base")]
    url: String,
    // The configuration names a key by its file, and holds none, so that it can be shown and kept
    // whole.
    key_file: PathBuf,
    models: Vec<Model>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Model {
    name: ModelName,
    upstream_name: ModelName,
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

    /// The providers, each made a `T` by `connect` from its name and kind, in the order of their
    /// names. Fails as the first `connect` that fails, making no more.
    pub(crate) fn connect<T>(
        self,
        mut connect: impl FnMut(&Name, Kind) -> Result<T, String>,
    ) -> Result<Providers<T>, String> {
        let connected =
            (self.0.into_iter()).map(|(name, kind)| Ok((name.clone(), connect(&name, kind)?)));
        connected.collect::<Result<_, String>>().map(Providers)
    }
}

impl<T> Providers<T> {
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

    fn serves(&self, entry: &ApiRoute) -> Result<Serving, String> {
        let ApiRoute {
            api,
            models,
            request_headers,
            response_headers,
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
    let parsed = toml::from_str(text);
    let Tables {
        docker,
        providers,
        gateway,
        mut upstreams,
        controller,
        publish,
    } = parsed
        .map_err(|error| error.to_string().trim_end().to_owned())
        .context(path.display())?;
    unique(&upstreams).context(path.display())?;
    // Below the file's directory, rather than the working directory or `HOME`, so that a
    // configuration and its keys move together, and no variable of the environment changes which
    // file a key is in. A file that was read has a parent: `""` for a bare file name.
    let dir = path.parent().unwrap_or(Path::new(""));
    if let Some(mut controller) = controller {
        let beside: Vec<&str> = [
            ("`[docker]`", docker.is_some()),
            ("`[providers]`", providers.is_some()),
            ("`[gateway]`", gateway.is_some()),
            ("`[publish]`", publish.is_some()),
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
        if let Some(modal) = &mut controller.modal {
            modal.config = modal.config.take().map(|config| dir.join(config));
        }
        let mut raw: toml::Table = toml::from_str(text).context(path.display())?;
        let upstreams = match raw.remove("upstreams") {
            Some(toml::Value::Table(upstreams)) => upstreams,
            _ => toml::Table::new(),
        };
        return Ok(Target::Controller(Client {
            controller,
            upstreams,
            dir: dir.to_owned(),
        }));
    }
    for upstream in upstreams.0.values_mut() {
        upstream.key_file = dir.join(&upstream.key_file);
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
        publish,
    }))
}

impl Client {
    /// The configuration that a job reads, as TOML, and the key files to send with it: each with
    /// the name that the text gives it, and the path of its source.
    pub(crate) fn sent(&self) -> Result<(String, Vec<(String, PathBuf)>), String> {
        let mut upstreams = self.upstreams.clone();
        let mut keys = Vec::new();
        for (name, upstream) in &mut upstreams {
            let key = upstream
                .get("key_file")
                .and_then(toml::Value::as_str)
                .map(|key| self.dir.join(key));
            if let (Some(table), Some(key)) = (upstream.as_table_mut(), key) {
                let file = format!("{name}.key");
                table.insert("key_file".to_owned(), file.clone().into());
                keys.push((file, key));
            }
        }
        let mut sent = toml::Table::from_iter([("upstreams".to_owned(), upstreams.into())]);
        if let Some(ClientModal {
            config: _,
            profile,
            classes,
        }) = &self.controller.modal
        {
            let modal = toml::Table::from_iter([
                ("profile".to_owned(), profile.clone().into()),
                ("classes".to_owned(), classes.clone().into()),
            ]);
            sent.insert("modal".to_owned(), modal.into());
        }
        let text = toml::to_string(&sent)
            .map_err(|error| format!("writing what the job is sent: {error}"))?;
        Ok((text, keys))
    }

    /// The contents of Modal's configuration file, or `None` if `[controller.modal]` is absent or
    /// names no file and `~/.modal.toml` doesn't exist.
    pub(crate) fn modal_file(&self) -> Result<Option<Vec<u8>>, String> {
        let Some(modal) = &self.controller.modal else {
            return Ok(None);
        };
        let path = if let Some(config) = &modal.config {
            config.clone()
        } else {
            let home =
                crate::home("name Modal's configuration file in `[controller.modal] config`")?;
            let default = home.join(".modal.toml");
            if !default.try_exists().context(default.display())? {
                return Ok(None);
            }
            default
        };
        let read = fs::read(&path).map_err(|error| {
            format!(
                "Modal's configuration file {} can't be read ({error}): put it there, or name \
                 another in `[controller.modal] config`",
                path.display()
            )
        })?;
        Ok(Some(read))
    }
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
