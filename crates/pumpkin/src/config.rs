//! The configuration of the machine that `pumpkin run` runs on, as TOML: the machine's Docker
//! engine, by the context that reaches it, in `[docker]`; the providers that a run may place its
//! roles on, each a table of its own, `[providers.<name>]`, which holds one table, named for the
//! provider's kind, and holding that kind's own settings ([`Kind`]), whose classes of sandbox a
//! run's roles are placed in; the addresses where the gateway meets the sandboxes that dial it
//! over a network, in `[gateway]`; and the model upstreams that a workflow's model route may
//! reach, each a table of its own, `[upstreams.<name>]`. [`Config`] and the types it holds say
//! what each key means; every key is required, but for `gateway`'s, `upstreams` and
//! `docker.cpus`, and any other refused. A machine whose runs go to the person's controller has
//! `[controller]` instead of `[docker]`, `[providers]` and `[gateway]`, beside its upstreams
//! ([`Client`]), so where its runs go has one home.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use docker::CpuList;
use engine::ContextName;
use gateway::GatewayAddress;
use route::{Headers, Key, Serving, UpstreamUrl};
use schema::ir::{Api, ApiRoute, ModelName, ModelRoute, Name};
use serde::{Deserialize, Deserializer, de};
use stdx::Context;

/// The configuration of a machine, which every run on it reads.
pub(crate) struct Config {
    pub(crate) docker: Docker,
    pub(crate) providers: Providers<Kind>,
    pub(crate) gateway: Gateway,
    pub(crate) upstreams: Upstreams,
}

/// The configuration of a machine whose runs go to the person's controller, which `run` and
/// `build` send what of it each job needs.
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
    /// TOML refuses two tables of one name.
    providers: Option<BTreeMap<Name, Kind>>,
    gateway: Option<Gateway>,
    /// Empty if absent.
    #[serde(default)]
    upstreams: Upstreams,
    controller: Option<Controller>,
}

/// The person's controller, which `deploy/aws` made, and to which the machine's runs go.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Controller {
    /// The `name` that the module was applied with.
    pub(crate) name: crate::controller::Name,
    /// The region it was applied in.
    pub(crate) region: aws::Region,
    /// The Modal provider that its jobs may place roles on, if any.
    pub(crate) modal: Option<ClientModal>,
}

/// The Modal provider of a client's jobs: the settings of a provider of kind
/// [`modal`](Kind::Modal) but `images`, which the controller's settings give, and `dials_from`,
/// which each job sets ([`crate::job`]), so its `profile` and `classes` are checked there.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClientModal {
    /// Modal's configuration file, which each job is sent, below the configuration's directory
    /// unless absolute; if absent, `$HOME/.modal.toml`, sent only if it is there.
    config: Option<PathBuf>,
    profile: String,
    classes: toml::Table,
}

/// The machine's Docker engine, which every provider of kind [`docker`](Kind::Docker) runs its
/// sandboxes on.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Docker {
    /// The context that reaches it, as `docker context ls` lists it.
    pub(crate) context: ContextName,
    /// The CPUs that its sandboxes may be given, such as all but those the machine's other work
    /// runs on; absent, all that are online.
    pub(crate) cpus: Option<CpuList>,
}

/// A provider that the machine's runs may place roles on: what kind it is, as the name of its
/// table's one table, whose keys are that kind's own settings.
#[derive(Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub(crate) enum Kind {
    /// The machine's Docker engine, [`Docker`], which takes no settings of its own.
    Docker(Empty),
    /// An envelope on EC2, which its settings name, whose sandboxes pull what they run from it
    /// and dial the gateway at [`private`](Gateway::private).
    Ec2(ec2::Settings),
    /// A Modal workspace, which its settings' profile reaches, whose sandboxes run images that
    /// Modal imports from an ECR repository, and listen behind Modal's tunnels, which the
    /// gateway dials.
    Modal(modal::Settings),
}

/// The table of a kind that takes no settings, which holds no key.
#[derive(Deserialize)]
#[serde(
    deny_unknown_fields,
    expecting = "an empty table, as the provider's kind takes no settings"
)]
pub(crate) struct Empty {}

/// Where the gateway meets the sandboxes that dial it over a network, by where their providers
/// reach it ([`Meeting`](provider::Meeting)).
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Gateway {
    /// The address, `ip:port`, that a private provider's sandboxes dial, read by [`address`]: an
    /// address of this machine's on the network it shares with them, which a run that places a
    /// role on such a provider listens at. `None` if absent.
    #[serde(default, deserialize_with = "address")]
    pub(crate) private: Option<GatewayAddress>,
}

/// The providers of a machine, by name, each a `T`, such as its [`Kind`]: one of kind
/// [`docker`](Kind::Docker) at most.
pub(crate) struct Providers<T>(BTreeMap<Name, T>);

/// The model upstreams of a machine, by name, in which each model's name reaches one upstream:
/// no model is listed twice, under one upstream or two. TOML refuses two upstreams of one name.
#[derive(Default, Deserialize)]
#[serde(transparent)]
pub(crate) struct Upstreams(BTreeMap<Name, Upstream>);

/// An API that serves models: a provider's own, or a gateway's to several.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Upstream {
    /// The base URL of the API, below which each API's endpoint is ([`endpoint`]).
    #[serde(deserialize_with = "base")]
    url: String,
    /// The file that holds the API's key, which [`read`] puts below the configuration's
    /// directory unless it is absolute.
    // The configuration names a key by its file, and holds none, so that it can be shown and
    // kept whole.
    key_file: PathBuf,
    models: Vec<Model>,
}

/// A model that the upstream that lists it serves, by the name with which a workflow's route
/// allows it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Model {
    name: ModelName,
    /// The name that the upstream takes for it.
    upstream_name: ModelName,
}

impl Providers<Kind> {
    /// The providers that `providers`, a configuration's tables, name. Fails, naming them, if
    /// more than one provider is of kind `docker`, or one is beside a provider of another kind.
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

    /// The providers, each made a `T` by `connect`, from its name and kind, in the order of their
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
    /// How a run serves `route`: each of its entries, in its order, through the upstream that
    /// serves every model the entry allows, with the key read from its file. Fails if no upstream
    /// serves every model of an entry.
    pub(crate) fn serving(&self, route: &ModelRoute) -> Result<Vec<Serving>, String> {
        route
            .as_slice()
            .iter()
            .map(|entry| self.serves(entry))
            .collect()
    }

    /// How a run serves `entry`, an entry of a route, as [`Upstreams::serving`] says.
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

/// An upstream's base URL, without its ending `/`, if it has one, and refused if it has a query
/// or a fragment, or as [`UpstreamUrl`] refuses it.
fn base<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let url = String::deserialize(deserializer)?;
    // The path added would end up within a query or a fragment. The URL itself isn't repeated,
    // as `UpstreamUrl` doesn't repeat it, since it may name a user's credential.
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

/// The endpoint of `api` below `base`, an upstream's base URL: `/responses` for
/// [`Api::Responses`], and `/chat/completions` for [`Api::Chat`].
fn endpoint(base: &str, api: Api) -> String {
    let path = match api {
        Api::Responses => "/responses",
        Api::Chat => "/chat/completions",
    };
    format!("{base}{path}")
}

/// A [`Gateway`] address, read from `ip:port`, such as `172.31.0.10:7433`, and refused if
/// unspecified, as [`GatewayAddress`] says.
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

/// The configuration in the file at `path`, a machine's. Fails, naming the file, if it isn't a
/// machine's configuration, as one that names `[controller]`; a TOML error is `toml`'s own, with
/// the line it points at.
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

/// The configuration that `text`, the file at `path`, holds, a machine's. Fails, naming the file,
/// if `text` isn't a machine's configuration, as one that names `[controller]`; a TOML error is
/// `toml`'s own, with the line it points at.
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

/// The client's configuration at `config`, or the default one, if it parses as one that names
/// `[controller]`; `None` if it can't be read, as when there is none, or names no
/// `[controller]`. Fails, naming the file, if it names `[controller]` but isn't a client's
/// configuration.
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
    } = parsed
        .map_err(|error| error.to_string().trim_end().to_owned())
        .context(path.display())?;
    unique(&upstreams).context(path.display())?;
    // Below the file's directory, rather than the working directory or `HOME`, so that a
    // configuration and its keys move together, and no variable of the environment changes
    // which file a key is in. A file that was read has a parent: `""` for a bare file name.
    let dir = path.parent().unwrap_or(Path::new(""));
    if let Some(mut controller) = controller {
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
    }))
}

impl Client {
    /// What a job is sent of the configuration, as TOML: the `[upstreams]` tables, each
    /// `key_file` a file sent beside it, `<upstream>.key`, and the Modal provider's `profile` and
    /// `classes`, in `[modal]`, if it names one; and the key file, below the configuration's
    /// directory unless absolute, that each such name stands for.
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

    /// What the client sends of Modal's configuration file, if there is one: with a Modal
    /// provider, its `config`, or `$HOME/.modal.toml` if it is there.
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
