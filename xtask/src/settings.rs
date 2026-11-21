//! What `cargo xtask live` reads of a configuration that sends runs to the person's controller,
//! `pumpkin`'s, for a smoke run on one of its providers: the controller's region, and, for Modal,
//! the profile that reaches the controller's workspace and the GPU of a class there, which its
//! flags give.

use std::error::Error;
use std::path::Path;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Provider {
    Ec2,
    Modal(Modal),
}

/// The settings of a smoke run on `Provider`, as the configuration gives them.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    pub(crate) name: String,
    /// The controller's region, which its EC2 sandboxes and their timers are in.
    pub(crate) region: String,
    pub(crate) modal: Option<Modal>,
}

/// For a run on Modal, the profile of Modal's own configuration that reaches the workspace of the
/// controller's runs, which the check that a run left no sandbox there lists it with, and the
/// model of the GPU that the smoke's GPU runner needs.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Modal {
    pub(crate) profile: String,
    pub(crate) gpu_model: String,
}

impl Modal {
    /// The settings of a run on Modal, as `--gpu-model` and `--modal-profile` give them.
    pub(crate) fn new(
        gpu_model: Option<String>,
        profile: Option<String>,
    ) -> Result<Self, Box<dyn Error>> {
        let gpu_model = gpu_model.ok_or(
            "a live run on Modal needs the model of a GPU that a class of the controller's \
             `modal_classes` has, the cheapest best: give it with `--gpu-model`",
        )?;
        // A profile of another workspace would list none of the run's sandboxes, so the check
        // would pass whatever the run left.
        let profile = profile.ok_or(
            "a live run on Modal needs the profile of Modal's configuration that reaches the \
             controller's workspace, `modal_workspace`, to check that the run left no sandbox \
             there: give it with `--modal-profile`",
        )?;
        Ok(Self { profile, gpu_model })
    }
}

impl Settings {
    /// Reads `text`, the configuration at `path`, for a smoke run on `provider`.
    pub(crate) fn parse(
        text: &str,
        path: &Path,
        provider: Provider,
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
        let text = |key: &str| {
            (controller.get(key))
                .and_then(toml::Value::as_str)
                .map(str::to_owned)
        };
        let name = text("name")
            .ok_or_else(|| format!("{shown}'s `[controller]` names no `name`: name it"))?;
        let region = text("region")
            .ok_or_else(|| format!("{shown}'s `[controller]` names no `region`: name it"))?;
        let modal = match provider {
            Provider::Ec2 => None,
            Provider::Modal(modal) => Some(modal),
        };
        Ok(Self {
            name,
            region,
            modal,
        })
    }
}
