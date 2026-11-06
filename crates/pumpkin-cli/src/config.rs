//! The deployment's configuration, which `pumpkin run` reads, as TOML: the model upstreams that
//! a workflow's model route may reach, each a table of its own, `[upstreams.<name>]`, of which
//! [`serving`](Config::serving) picks the one that serves it. [`Config`] and the types it holds
//! say what each key means; every key is required, and any other refused.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use pumpkin::model_route::{self, Key, Serving, UpstreamUrl};
use pumpkin_schema::ir::{ModelName, Models, Name};
use serde::{Deserialize, Deserializer, de};
use stdx::Context;

/// The model upstreams of a deployment, by name, in which each model's name reaches one upstream:
/// no model is listed twice, under one upstream or two. TOML refuses two upstreams of one name.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    upstreams: BTreeMap<Name, Upstream>,
}

/// An API that serves models: a provider's own, or a gateway's to several.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Upstream {
    /// The endpoint of the API, read from its base URL ([`endpoint`]).
    #[serde(deserialize_with = "endpoint")]
    url: UpstreamUrl,
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

impl Config {
    /// How a run serves a model route that allows `models`: through the upstream that serves
    /// every one of them, with the key read from its file.
    ///
    /// # Errors
    ///
    /// Fails if no upstream serves every model, or if the key can't be read.
    pub(crate) fn serving(self, models: &Models) -> Result<Serving, String> {
        let serves = |upstream: &Upstream| {
            let served =
                |model: &ModelName| upstream.models.iter().any(|served| served.name == *model);
            models.as_slice().iter().all(served)
        };
        // At most one does: each model is listed once, and a route allows one at least.
        let Some((name, upstream)) = self
            .upstreams
            .into_iter()
            .find(|(_, upstream)| serves(upstream))
        else {
            let named: Vec<String> = models
                .as_slice()
                .iter()
                .map(|model| format!("`{model}`"))
                .collect();
            return Err(format!(
                "no upstream serves every model that the workflow's route allows, {}: list them \
                 all under one",
                named.join(", ")
            ));
        };
        let Upstream {
            url,
            key_file,
            models,
        } = upstream;
        let upstream = model_route::Upstream::new(url, Key::read(&key_file)?);
        let models = models
            .into_iter()
            .map(|model| (model.name, model.upstream_name));
        let models = models.collect();
        Ok(Serving {
            name,
            upstream,
            models,
        })
    }
}

/// The endpoint of an upstream's API, read from its base URL: `/responses` below it, once the
/// URL's ending `/`, if it has one, is dropped. A base URL with a query or a fragment is refused.
fn endpoint<'de, D: Deserializer<'de>>(deserializer: D) -> Result<UpstreamUrl, D::Error> {
    let url = String::deserialize(deserializer)?;
    // The path added would end up within a query or a fragment. The URL itself isn't repeated,
    // as `UpstreamUrl` doesn't repeat it, since it may name a user's credential.
    if url.contains(['?', '#']) {
        return Err(de::Error::custom(
            "the upstream's URL has a query or a fragment: name the API's base URL alone, below \
             which the route reaches `/responses`",
        ));
    }
    let endpoint = format!("{}/responses", url.strip_suffix('/').unwrap_or(&url));
    UpstreamUrl::try_from(endpoint).map_err(de::Error::custom)
}

/// The configuration in the file at `path`.
///
/// # Errors
///
/// Fails if the file can't be read, or, naming the file, if it isn't a configuration. A TOML
/// error is `toml`'s own, with the line it points at.
pub(crate) fn read(path: &Path) -> Result<Config, String> {
    let text = fs::read_to_string(path).map_err(|error| {
        format!(
            "the configuration {} can't be read ({error}): write the upstreams of the \
             workflow's model route there, or name another file with `--config`",
            path.display()
        )
    })?;
    // `toml` ends its message, which quotes the line, with a newline of its own.
    let parsed = toml::from_str(&text);
    let mut config: Config = parsed
        .map_err(|error| error.to_string().trim_end().to_owned())
        .context(path.display())?;
    unique(&config).context(path.display())?;
    // Below the file's directory, rather than the working directory or `HOME`, so that a
    // configuration and its keys move together, and no variable of the environment changes
    // which file a key is in. A file that was read has a parent: `""` for a bare file name.
    let dir = path.parent().unwrap_or(Path::new(""));
    for upstream in config.upstreams.values_mut() {
        upstream.key_file = dir.join(&upstream.key_file);
    }
    Ok(config)
}

/// Checks that no model of `config`'s is listed twice.
fn unique(config: &Config) -> Result<(), String> {
    let mut models = BTreeSet::new();
    for upstream in config.upstreams.values() {
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
