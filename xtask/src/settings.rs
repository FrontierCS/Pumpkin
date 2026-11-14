//! What `cargo xtask live` reads of a configuration that sends runs to the person's controller,
//! `pumpkin`'s, for a smoke run on one of its providers: the controller's region, and, for Modal,
//! the profile and configuration file that reach the workspace and the GPU of a class there.

use std::error::Error;
use std::path::{Path, PathBuf};

/// The provider of a smoke run on the controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Provider {
    Ec2,
    Modal,
}

/// The settings of a smoke run on `Provider`, as the configuration gives them.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    /// The controller's name.
    pub(crate) name: String,
    /// The controller's region, which its EC2 sandboxes and their timers are in.
    pub(crate) region: String,
    /// For a run on Modal, the workspace's profile, the configuration file that holds it, if the
    /// configuration names one, and the model of the GPU that the smoke's GPU runner needs.
    pub(crate) modal: Option<Modal>,
}

/// What reaches the Modal workspace of the controller's runs, and the GPU the smoke needs.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Modal {
    pub(crate) profile: String,
    pub(crate) config: Option<PathBuf>,
    pub(crate) gpu_model: String,
}

impl Settings {
    /// Reads `text`, the configuration at `path`, for a smoke run on `provider`, whose GPU
    /// runner, on Modal, needs `gpu_model`, which must be the model of one of the classes of
    /// `[controller.modal]`.
    pub(crate) fn parse(
        text: &str,
        path: &Path,
        provider: Provider,
        gpu_model: Option<&str>,
    ) -> Result<Self, Box<dyn Error>> {
        let shown = path.display();
        let table: toml::Table = toml::from_str(text)
            .map_err(|error| format!("{shown} isn't a configuration: {error}"))?;
        let Some(controller) = table.get("controller").and_then(toml::Value::as_table) else {
            return Err(format!(
                "{shown} names no `[controller]`, which a live run goes through: name the one \
                 `deploy/aws` made, as the README says"
            )
            .into());
        };
        let text = |table: &toml::Table, key: &str| {
            table
                .get(key)
                .and_then(toml::Value::as_str)
                .map(str::to_owned)
        };
        let name = text(controller, "name")
            .ok_or_else(|| format!("{shown}'s `[controller]` names no `name`: name it"))?;
        let region = text(controller, "region")
            .ok_or_else(|| format!("{shown}'s `[controller]` names no `region`: name it"))?;
        let modal = match provider {
            Provider::Ec2 => None,
            Provider::Modal => Some(Modal::parse(controller, path, gpu_model)?),
        };
        Ok(Self {
            name,
            region,
            modal,
        })
    }
}

impl Modal {
    /// Reads `controller`, the `[controller]` table of the configuration at `path`, for a smoke
    /// run on Modal whose GPU runner needs `gpu_model`.
    fn parse(
        controller: &toml::Table,
        path: &Path,
        gpu_model: Option<&str>,
    ) -> Result<Self, Box<dyn Error>> {
        let shown = path.display();
        let Some(modal) = controller.get("modal").and_then(toml::Value::as_table) else {
            return Err(format!(
                "{shown} names no `[controller.modal]`, which a live run on Modal needs: name the \
                 workspace's profile and classes there, as the README says"
            )
            .into());
        };
        let profile = text(modal, "profile")
            .ok_or_else(|| format!("{shown}'s `[controller.modal]` names no `profile`: name it"))?;
        let config = text(modal, "config").map(|config| {
            let dir = path.parent().unwrap_or(Path::new(""));
            dir.join(config)
        });
        let classes = modal.get("classes").and_then(toml::Value::as_table);
        let mut models: Vec<String> = (classes.into_iter().flatten())
            .filter_map(|(_, class)| class.as_table().and_then(|class| text(class, "gpu_model")))
            .collect();
        models.sort();
        models.dedup();
        let offered = models.join("`, `");
        let Some(gpu_model) = gpu_model else {
            return Err(format!(
                "a live run on Modal needs the model of a GPU that a class of \
                 `[controller.modal]` has, the cheapest best: give one of `{offered}` with \
                 `--gpu-model`"
            )
            .into());
        };
        if !models.iter().any(|model| model == gpu_model) {
            return Err(format!(
                "no class of {shown}'s `[controller.modal]` has the GPU `{gpu_model}`: give one \
                 of `{offered}`"
            )
            .into());
        }
        Ok(Self {
            profile,
            config,
            gpu_model: gpu_model.to_owned(),
        })
    }
}

fn text(table: &toml::Table, key: &str) -> Option<String> {
    table
        .get(key)
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
}
