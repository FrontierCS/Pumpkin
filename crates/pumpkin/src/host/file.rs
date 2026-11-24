//! The host file, [`FILE`], says what the controller's host knows of its deployment.
//! `pumpkin-admin install` writes it, and the host's units and its jobs read it. It holds the
//! controller's target ([`Target`]) and where its settings are. It holds no secret, so any user may
//! read it.
// Installers keep their units' inputs in a file at a fixed path. k3s keeps its environment file,
// and kubeadm the kubelet's configuration:
// https://github.com/k3s-io/k3s/blob/647ad6706757349803c34d1e33bc3e7384c68544/install.sh#L1027-L1033
// https://github.com/kubernetes/kubernetes/blob/8db047b9682a763d9110f321919afdd6e522fe72/cmd/kubeadm/app/constants/constants.go#L303-L308

use std::path::{Path, PathBuf};
use std::{fs, io};

use aws::ParameterArn;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use stdx::Context as _;

use crate::job::Source;

const DIR: &str = "/etc/pumpkin";

const FILE: &str = "/etc/pumpkin/controller.toml";

/// What the controller runs on, and where its settings are.
pub(crate) enum Target {
    /// An EC2 instance that `deploy/aws` made, whose settings are an SSM parameter. It stops once
    /// idle.
    Ec2 { settings: ParameterArn },
    /// A container that `deploy/docker` made, whose settings are a file of its own. It never
    /// stops, as nothing would start it again.
    Docker { settings: PathBuf },
}

impl Target {
    /// The target on `kind`, with its settings at `settings`: an SSM parameter's ARN on EC2, an
    /// absolute path on Docker. Fails if `settings` is not one.
    pub(crate) fn new(kind: Kind, settings: String) -> Result<Self, String> {
        match kind {
            Kind::Ec2 => ParameterArn::try_from(settings).map(|settings| Self::Ec2 { settings }),
            Kind::Docker if settings.starts_with('/') => Ok(Self::Docker {
                settings: settings.into(),
            }),
            Kind::Docker => Err(format!(
                "`{}` is no absolute path: name the file of the controller's settings by one",
                settings.escape_debug()
            )),
        }
    }

    /// Where the controller's settings are.
    pub(crate) fn settings(&self) -> Source {
        match self {
            Self::Ec2 { settings } => Source::Parameter(settings.clone()),
            Self::Docker { settings } => Source::File(settings.clone()),
        }
    }

    /// Whether the controller stops once idle.
    pub(super) const fn idles(&self) -> bool {
        match self {
            Self::Ec2 { .. } => true,
            Self::Docker { .. } => false,
        }
    }
}

/// What a controller runs on, as `pumpkin-admin install` and the host file name it.
#[derive(Clone, Copy, Deserialize, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    /// An EC2 instance that deploy/aws made. It stops once idle, and a client starts it again.
    Ec2,
    /// The container that deploy/docker makes. It never stops by itself.
    Docker,
}

// Without `deny_unknown_fields`, so that a later release may add a key that this one ignores.
#[derive(Deserialize, Serialize)]
struct HostFile {
    /// A file that names no target is EC2's.
    target: Option<Kind>,
    /// An SSM parameter's ARN on EC2, a path on Docker.
    settings: String,
}

/// The controller's target, as the host file names it.
pub(crate) fn target() -> Result<Target, String> {
    let path = Path::new(FILE);
    let again = "the controller's admin writes it again with `sudo pumpkin-admin install`";
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(format!(
                "{} is missing, and it names the controller's settings: the controller's admin \
                 writes it with `sudo pumpkin-admin install`",
                path.display()
            ));
        }
        Err(error) => return Err(error).context(path.display()),
    };
    let refused = |error: String| format!("{} is refused: {error}: {again}", path.display());
    let HostFile { target, settings } = toml::from_str(&text)
        .map_err(|error: toml::de::Error| refused(error.to_string().trim_end().to_owned()))?;
    Target::new(target.unwrap_or(Kind::Ec2), settings).map_err(refused)
}

/// Writes the host file to name `target`, as [`super::wrote`] does. Returns true if it wrote.
pub(super) fn write(target: &Target) -> Result<bool, String> {
    let path = Path::new(FILE);
    // Searchable by the controller's user, whose jobs read it.
    super::searchable(Path::new(DIR))?;
    let file = match target {
        Target::Ec2 { settings } => HostFile {
            target: Some(Kind::Ec2),
            settings: settings.to_string(),
        },
        Target::Docker { settings } => HostFile {
            target: Some(Kind::Docker),
            settings: settings.to_string_lossy().into_owned(),
        },
    };
    let text = toml::to_string(&file).map_err(|error| format!("writing the host file: {error}"))?;
    super::wrote(path, text.as_bytes(), 0o644)
}
