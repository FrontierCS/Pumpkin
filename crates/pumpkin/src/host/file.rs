//! The host file, [`FILE`], says what the controller's host knows of its deployment.
//! `pumpkin-admin install` writes it, and the host's units and its jobs read it. It holds the ARN
//! of the SSM parameter of the controller's settings. It holds no secret, so any user may read it.
// Installers keep their units' inputs in a file at a fixed path. k3s keeps its environment file,
// and kubeadm the kubelet's configuration:
// https://github.com/k3s-io/k3s/blob/647ad6706757349803c34d1e33bc3e7384c68544/install.sh#L1027-L1033
// https://github.com/kubernetes/kubernetes/blob/8db047b9682a763d9110f321919afdd6e522fe72/cmd/kubeadm/app/constants/constants.go#L303-L308

use std::path::Path;
use std::{fs, io};

use aws::ParameterArn;
use serde::{Deserialize, Serialize};
use stdx::Context as _;

/// The directory of [`FILE`].
const DIR: &str = "/etc/pumpkin";

/// Where the host file is.
const FILE: &str = "/etc/pumpkin/controller.toml";

#[derive(Deserialize, Serialize)]
struct HostFile {
    /// The SSM parameter of the controller's settings.
    settings: ParameterArn,
}

/// The SSM parameter of the controller's settings, as the host file names it.
pub(crate) fn settings() -> Result<ParameterArn, String> {
    let path = Path::new(FILE);
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(format!(
                "{} is missing, and it names the controller's settings: the controller's admin \
                 writes it with `sudo pumpkin-admin install --settings <arn>`",
                path.display()
            ));
        }
        Err(error) => return Err(error).context(path.display()),
    };
    let HostFile { settings } = toml::from_str(&text).map_err(|error| {
        let error = error.to_string();
        format!(
            "{} is refused: {}: the controller's admin writes it again with `sudo \
             pumpkin-admin install --settings <arn>`",
            path.display(),
            error.trim_end()
        )
    })?;
    Ok(settings)
}

/// Writes the host file to name `settings`, as [`super::wrote`] does. Returns true if it wrote.
pub(super) fn write(settings: &ParameterArn) -> Result<bool, String> {
    let path = Path::new(FILE);
    // Searchable by the controller's user, whose jobs read it.
    super::searchable(Path::new(DIR))?;
    let file = HostFile {
        settings: settings.clone(),
    };
    let text = toml::to_string(&file).map_err(|error| format!("writing the host file: {error}"))?;
    super::wrote(path, text.as_bytes(), 0o644)
}
