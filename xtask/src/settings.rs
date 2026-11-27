//! What `cargo xtask live` reads of `pumpkin`'s configuration when it sends runs to the person's
//! controller that `deploy/aws` made, the one of its `[controllers.<name>]` that names a `region`.
//! A smoke run on one of its providers reads the controller's name and region. A run on Modal also
//! reads the profile that reaches the workspace, and the GPU of a class there. It checks that
//! another class there has no GPU.

use std::error::Error;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Provider {
    Ec2,
    Modal,
}

/// The settings of a smoke run on `Provider`, as the configuration gives them.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    pub(crate) name: String,
    /// The controller's region, which its EC2 sandboxes and their timers are in.
    pub(crate) region: String,
    /// For a run on Modal: the workspace's profile, and the model of the GPU that the smoke's GPU
    /// runner needs.
    pub(crate) modal: Option<Modal>,
}

/// What reaches the Modal workspace of the controller's runs, and the GPU the smoke needs.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Modal {
    pub(crate) profile: String,
    pub(crate) gpu_model: String,
}

impl Settings {
    /// Reads `text`, the configuration at `path`, for a smoke run on `provider`, whose GPU
    /// runner, on Modal, needs `gpu_model`, which must be the model of one of the classes of the
    /// controller's `modal`.
    pub(crate) fn parse(
        text: &str,
        path: &Path,
        provider: Provider,
        gpu_model: Option<&str>,
    ) -> Result<Self, Box<dyn Error>> {
        let shown = path.display();
        let table: toml::Table = toml::from_str(text)
            .map_err(|error| format!("{shown} isn't a configuration: {error}"))?;
        let controllers = table.get("controllers").and_then(toml::Value::as_table);
        let regional: Vec<(&String, &toml::Table, String)> = (controllers.into_iter().flatten())
            .filter_map(|(name, controller)| {
                let controller = controller.as_table()?;
                Some((name, controller, self::text(controller, "region")?))
            })
            .collect();
        let (name, controller, region) = match &regional[..] {
            [one] => one.clone(),
            [] => {
                return Err(format!(
                    "{shown} names no `[controllers.<name>]` with a `region`, which a live run \
                     goes through: name the one `deploy/aws` made, as the README says"
                )
                .into());
            }
            several => {
                let names: Vec<String> = several
                    .iter()
                    .map(|(name, ..)| format!("`{name}`"))
                    .collect();
                return Err(format!(
                    "{shown} names several controllers with a `region`, {}: keep there the one \
                     that a live run goes through",
                    names.join(", ")
                )
                .into());
            }
        };
        let modal = match provider {
            Provider::Ec2 => None,
            Provider::Modal => Some(Modal::parse(name, controller, path, gpu_model)?),
        };
        let name = name.clone();
        Ok(Self {
            name,
            region,
            modal,
        })
    }
}

impl Modal {
    /// Reads `controller`, the `[controllers.<name>]` table of the configuration at `path`, of
    /// controller `name`, for a smoke run on Modal whose GPU runner needs `gpu_model`.
    fn parse(
        name: &str,
        controller: &toml::Table,
        path: &Path,
        gpu_model: Option<&str>,
    ) -> Result<Self, Box<dyn Error>> {
        let shown = path.display();
        let table = format!("[controllers.{name}.modal]");
        let Some(modal) = controller.get("modal").and_then(toml::Value::as_table) else {
            return Err(format!(
                "{shown} names no `{table}`, which a live run on Modal needs: name the \
                 workspace's profile and classes there, as the README says"
            )
            .into());
        };
        let profile = text(modal, "profile")
            .ok_or_else(|| format!("{shown}'s `{table}` names no `profile`: name it"))?;
        let classes = modal.get("classes").and_then(toml::Value::as_table);
        // A run on Modal also runs `examples/companions`, whose runner needs no GPU, and so only a
        // class without one holds it.
        let without_gpu = (classes.into_iter().flatten()).any(|(_, class)| {
            class
                .as_table()
                .is_some_and(|class| !class.contains_key("gpu"))
        });
        if !without_gpu {
            return Err(format!(
                "a live run on Modal runs examples/companions on a class without a GPU, and \
                 {shown}'s `{table}` has none: add one, such as `classes.cpu = {{ cpu_millis = \
                 2000, memory_bytes = 4294967296, startup_seconds = 600 }}`"
            )
            .into());
        }
        let mut models: Vec<String> = (classes.into_iter().flatten())
            .filter_map(|(_, class)| class.as_table().and_then(|class| text(class, "gpu_model")))
            .collect();
        models.sort();
        models.dedup();
        let offered = models.join("`, `");
        let Some(gpu_model) = gpu_model else {
            return Err(format!(
                "a live run on Modal needs the model of a GPU that a class of `{table}` has, the \
                 cheapest best: give one of `{offered}` with `--gpu-model`"
            )
            .into());
        };
        if !models.iter().any(|model| model == gpu_model) {
            return Err(format!(
                "no class of {shown}'s `{table}` has the GPU `{gpu_model}`: give one of \
                 `{offered}`"
            )
            .into());
        }
        Ok(Self {
            profile,
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
