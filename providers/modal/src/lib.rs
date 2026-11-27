//! The Modal provider: a sandbox for a role as a sandbox in a Modal workspace.
//!
//! A role's image reaches Modal as Modal imports it from an ECR repository (`Images`), so no AWS
//! credential of the operator's reaches Modal. The provider keeps to a run's own sandboxes by the
//! tag that carries its run.
//!
//! The provider reaches Modal only through its helper (`helper`), a Python program on Modal's
//! SDK, run once for each call, which reaches the workspace with the profile's token, from Modal's
//! own configuration file.
//!
//! # Settings
//!
//! Only deserialization makes [`Settings`], from a table such as this, in TOML:
//!
//! ```toml
//! profile = "pumpkin"
//! images.region = "us-west-1"
//! images.repository = "pumpkin/roles"
//! images.role_arn = "arn:aws:iam::123456789012:role/pumpkin-modal-pull"
//! dials_from = "203.0.113.7"
//! classes.cpu = { cpu_millis = 2000, memory_bytes = 4294967296, startup_seconds = 600 }
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::Path;

use aws::{Cli, Region, Repository, RepositoryName, RoleArn};
use engine::{Platform, Pusher};
use provider::{
    Assets, Console, Create, Creator, ImageRef, Listed, ListenDials, Listened, Listens, Meeting,
    Provider, Ready, Reservation, Role, RunTag, SandboxId, SandboxdRef, Sweep, Undeleted,
};
use schema::ir::{self, GpuModel, Image, Name};
use schema::sandbox::SANDBOXD_PATH;
use schema::workflow::{Class, Offers, UsdPerHour};
use serde::{Deserialize, Serialize};
use serde_json::json;
use stdx::Deadline;

use crate::assets::Fills;
use crate::create::{Sandboxd, companions_held, memory_mib};
use crate::helper::{ASK_TIMEOUT, ConfigFile, Helper, IMPORT_TIMEOUT, Profile, Request};
use crate::iam::ECR;
use crate::images::Batch;

mod assets;
mod create;
pub mod envelope;
mod helper;
pub mod iam;
mod images;
mod sandboxes;

/// The provider's settings, in a table such as [the crate's](crate#settings).
// Without `deny_unknown_fields`, so that a reader of settings may ignore a key that a later
// version adds. A reader that refuses such a key finds it through `serde_ignored`.
#[derive(Debug, Deserialize)]
pub struct Settings {
    profile: Profile,
    /// Modal's configuration file that holds the profile, if not Modal's own default,
    /// `~/.modal.toml`.
    config: Option<ConfigFile>,
    images: Images,
    /// The public IPv4 address of this machine's that its connections to Modal come from, from
    /// which alone a sandbox's tunnel takes them.
    dials_from: Ipv4Addr,
    classes: BTreeMap<Name, SandboxClass>,
}

impl Settings {
    /// The classes of sandbox that the settings state, by name, which [`Provider::ready`] gives
    /// once [`Modal::connect`] has reached the workspace.
    pub fn classes(&self) -> BTreeMap<Name, Class> {
        let classes = self.classes.iter();
        classes
            .map(|(name, sandbox)| (name.clone(), sandbox.class.clone()))
            .collect()
    }
}

/// Where Modal imports the provider's images from: an ECR repository in `region`, which Modal
/// pulls from with the AWS role `role_arn`, which it assumes by its own identity.
// Without `deny_unknown_fields`, as `Settings`.
#[derive(Clone, Debug, Deserialize)]
struct Images {
    region: Region,
    repository: RepositoryName,
    role_arn: RoleArn,
}

/// A class of sandbox as its table holds it: its GPU, if any, as Modal takes it and as
/// `nvidia-smi` names its model, the most CPU and memory of its sandboxes, which Modal gives
/// each as both its request and its limit, and how long one takes to start, such as `{ gpu =
/// "H100!", gpu_model = "NVIDIA H100 80GB HBM3", cpu_millis = 4000, memory_bytes = 8589934592,
/// startup_seconds = 1800 }`; without `gpu` and `gpu_model`, it has none. With `usd_per_hour`,
/// such as `3.95`, it has the price that the operator says a sandbox costs.
// Without `deny_unknown_fields`, as `Settings`.
#[derive(Deserialize, Serialize)]
struct ClassTable {
    gpu: Option<Gpu>,
    gpu_model: Option<GpuModel>,
    cpu_millis: NonZeroU64,
    memory_bytes: NonZeroU64,
    startup_seconds: NonZeroU64,
    usd_per_hour: Option<UsdPerHour>,
}

/// A class of sandbox: what a run may rely on of it, and its GPU, if any. It reads and writes as
/// a table of the provider's settings' `classes`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "ClassTable", into = "ClassTable")]
pub struct SandboxClass {
    class: Class,
    gpu: Option<Gpu>,
}

/// A GPU as Modal's SDK takes it: its type, such as `H100`, `H100!` or `A100-80GB`, which Modal
/// refuses if it lacks it, then `:` and its count, for more than one, such as `A100-80GB:2`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
struct Gpu {
    written: String,
    count: NonZeroU32,
}

impl TryFrom<String> for Gpu {
    type Error = String;

    fn try_from(written: String) -> Result<Self, Self::Error> {
        let count = match written.split_once(':') {
            None => NonZeroU32::MIN,
            Some((_, count)) => count.parse().map_err(|_| {
                format!(
                    "GPU `{}` has the count `{}`, which is not a whole number of GPUs from 1 to \
                     {}: give one, as in `A100-80GB:2`",
                    written.escape_debug(),
                    count.escape_debug(),
                    u32::MAX
                )
            })?,
        };
        Ok(Self { written, count })
    }
}

impl From<Gpu> for String {
    fn from(gpu: Gpu) -> Self {
        gpu.written
    }
}

impl TryFrom<ClassTable> for SandboxClass {
    type Error = String;

    fn try_from(class: ClassTable) -> Result<Self, Self::Error> {
        let ClassTable {
            gpu,
            gpu_model,
            cpu_millis,
            memory_bytes,
            startup_seconds,
            usd_per_hour,
        } = class;
        let offered = match (&gpu, gpu_model) {
            (None, None) => None,
            (Some(gpu), Some(model)) => Some(ir::Gpu {
                count: gpu.count,
                model,
            }),
            (Some(_), None) | (None, Some(_)) => {
                return Err(
                    "a class names one of `gpu` and `gpu_model`: name both, as `gpu = \"H100!\"` \
                     and `gpu_model = \"NVIDIA H100 80GB HBM3\"`, or neither"
                        .to_owned(),
                );
            }
        };
        let offers = Offers {
            cpu_millis,
            memory_bytes,
            gpu: offered,
            cpu_features: BTreeSet::new(),
            cpu_model: None,
            companions: true,
            assets: true,
        };
        let class = Class {
            offers,
            startup_seconds,
            usd_per_hour,
        };
        Ok(Self { class, gpu })
    }
}

impl From<SandboxClass> for ClassTable {
    fn from(sandbox: SandboxClass) -> Self {
        let SandboxClass { class, gpu } = sandbox;
        let Class {
            offers,
            startup_seconds,
            usd_per_hour,
        } = class;
        // Its GPU's count is `gpu`'s, a class of the settings names no CPU's flags or model, and
        // every class holds companions and assets.
        let Offers {
            cpu_millis,
            memory_bytes,
            gpu: offered,
            cpu_features: _,
            cpu_model: _,
            companions: _,
            assets: _,
        } = offers;
        Self {
            gpu,
            gpu_model: offered.map(|offered| offered.model),
            cpu_millis,
            memory_bytes,
            startup_seconds,
            usd_per_hour,
        }
    }
}

/// The Modal workspace, reached, which every sandbox of the provider's runs in, the repository
/// it imports images from, and the operator's Docker engine, which holds them until they are
/// pushed there.
///
/// Each call but an asset's publish returns within a bound, a create's at the create's deadline
/// ([`Create::by`]). A delete waits for each sandbox to finish. `aws` and `docker` are bounded as
/// [`aws::Bound::Call`] and [`engine::Engine`] bound them.
#[derive(Debug)]
pub struct Modal<'e> {
    reached: Reached,
    cli: Cli,
    pusher: Pusher<'e>,
    images: Images,
    repository: Repository,
    dials_from: Ipv4Addr,
    classes: BTreeMap<Name, SandboxClass>,
    ready: Ready,
    fills: Fills,
}

/// What the helper's `ready` returns: the name of the workspace that the profile reaches, if
/// Modal gives it one.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Described {
    workspace: Option<String>,
}

impl<'e> Modal<'e> {
    /// Reaches the workspace that `settings`' profile reaches, through the helper at `script`,
    /// whose lock is beside it, and the settings' repository, with `pusher`, the engine it
    /// publishes from. It writes in `dir` what `uv` fetches, and the ids of its assets' images. It
    /// fails if Modal's configuration lacks the profile, or AWS describes no such repository.
    pub fn connect(
        settings: &Settings,
        script: &Path,
        dir: &Path,
        pusher: Pusher<'e>,
    ) -> Result<Self, String> {
        let Settings {
            profile,
            config,
            images,
            dials_from,
            classes,
        } = settings;
        let helper = Helper::new(script, dir, profile.clone(), config.clone());
        let Described { workspace } = helper.call(&Request::Ready, Deadline::after(ASK_TIMEOUT))?;
        let cli = Cli::connect(images.region.clone())?;
        let [repository] = cli.repositories(&ECR, [&images.repository])?;
        let repository = repository?;
        let ready = Ready {
            report: json!({
                "profile": profile,
                "workspace": workspace,
                "images": { "role_arn": images.role_arn },
                "repository": repository.uri(),
                "dials_from": dials_from,
            }),
            classes: settings.classes(),
        };
        Ok(Self {
            reached: Reached { helper },
            cli,
            pusher,
            images: images.clone(),
            repository,
            dials_from: *dials_from,
            classes: classes.clone(),
            ready,
            fills: Fills::in_dir(&dir.join("assets")),
        })
    }

    /// Reaches the workspace that `settings`' profile reaches, in Modal's configuration file
    /// `config` if given, else theirs, through the helper at `script`, writing in `dir` as
    /// [`connect`](Self::connect) does, for a sweep of a run's sandboxes alone: nothing else of
    /// the settings is reached, and nothing is reported. It fails if `config` is relative, or the
    /// configuration lacks the profile.
    pub fn reach(
        settings: &Settings,
        config: Option<&Path>,
        script: &Path,
        dir: &Path,
    ) -> Result<Reached, String> {
        let config = match config {
            Some(config) => Some(ConfigFile::try_from(config.to_owned())?),
            None => settings.config.clone(),
        };
        let helper = Helper::new(script, dir, settings.profile.clone(), config);
        let Described { .. } = helper.call(&Request::Ready, Deadline::after(ASK_TIMEOUT))?;
        Ok(Reached { helper })
    }
}

impl Sweep for Modal<'_> {
    fn delete(&self, run: &RunTag, ids: &[SandboxId]) -> Result<(), Undeleted> {
        self.reached.delete(run, ids)
    }

    fn list(&self, run: &RunTag) -> Result<Vec<Listed>, String> {
        self.reached.list(run)
    }
}

/// A workspace that a profile reaches, reached for a sweep of a run's sandboxes alone
/// ([`Modal::reach`]): it lists and deletes them as [`Modal`] does, and bounds its calls alike.
#[derive(Debug)]
pub struct Reached {
    helper: Helper,
}

impl Provider for Modal<'_> {
    fn ready(&self) -> &Ready {
        &self.ready
    }

    // Each sandbox has resources of its own, which no other run shares, so a run holds nothing
    // beforehand. A role whose memory Modal can't give is refused here, before anything is
    // published for it, not at each of its creates.
    fn reserve(&self, _run: &RunTag, phases: &[Vec<Role>]) -> Result<Reservation, String> {
        for role in phases.iter().flatten() {
            memory_mib(role)?;
        }
        Ok(Reservation::new(()))
    }

    // Modal refuses a companion past what its sandbox holds only at the create, after every image
    // is published. So a run that declares more is refused here, before any is built.
    fn fits(&self, phases: &[Vec<Role>]) -> Result<(), String> {
        phases.iter().flatten().try_for_each(companions_held)
    }

    /// Publishes `image`, a role's, as [`Provider::publish_image`] asks: the engine must have it,
    /// for `sandboxd`'s platform, checked first; it is pushed to the repository unless held there
    /// ([`aws::Cli::publish`]), and Modal imports it from there by its digest, at every publish,
    /// without its entrypoint. Its reference is its id in Modal and the digest, `im-…@sha256:…`.
    /// It fails if the engine lacks the image or has it for another platform, or if Modal can't
    /// import it, as one its role may not pull.
    fn publish_image(&self, image: &Image) -> Result<ImageRef, String> {
        let imported = self.import(&self.push(&[image])?, None)?;
        let [imported] = <[String; 1]>::try_from(imported).map_err(|imported| {
            format!(
                "the Modal helper's `import` answered {} ids for one image: expected its id",
                imported.len()
            )
        })?;
        Ok(images::reference(&imported, image))
    }

    fn publish_sandboxd(&self, path: &Path) -> Result<SandboxdRef, String> {
        sandboxes::publish_sandboxd(path)
    }

    // A sandbox's only way in is its tunnel, where the gateway dials it.
    fn meeting(&self) -> Meeting<'_> {
        Meeting::Listen(self)
    }

    fn assets(&self) -> Option<&dyn Assets> {
        Some(self)
    }

    fn console(&self, run: &RunTag, id: &SandboxId) -> Result<Console, String> {
        self.read_console(run, id)
    }
}

impl Modal<'_> {
    /// Pushes each of `images`, which are distinct, to the repository, unless held there. Returns
    /// their references there, by their digests, in their order. Fails if the engine lacks one of
    /// them, or has one for another platform, before it pushes any.
    fn push(&self, images: &[&Image]) -> Result<Vec<String>, String> {
        // Modal runs `sandboxd`'s platform alone.
        let runs = "Modal runs sandboxes on";
        for image in images {
            (self.pusher.engine()).check_platform(image, &Platform::sandboxd(), runs)?;
        }
        self.cli.publish(&self.pusher, &self.repository, images)?;
        let references = images.iter().map(|image| self.repository.reference(image));
        Ok(references.collect())
    }

    /// Has Modal import the image at each of `references`, which [`push`](Self::push) gave, with
    /// `sandboxd` built in if given. Returns what the helper answers: their ids in Modal, in the
    /// order of `references`.
    fn import(
        &self,
        references: &[String],
        sandboxd: Option<Sandboxd<'_>>,
    ) -> Result<Vec<String>, String> {
        let Images {
            region, role_arn, ..
        } = &self.images;
        let request = Request::Import {
            references: references.iter().map(String::as_str).collect(),
            role_arn: role_arn.as_str(),
            region: region.as_str(),
            sandboxd,
        };
        let expected = format!(
            "expected Modal to import `{}` within {} s, as long as its credentials to pull from \
             ECR last: an image whose largest layers copy slowly can't finish",
            references.join("`, `"),
            IMPORT_TIMEOUT.as_secs()
        );
        let by = Deadline::after(IMPORT_TIMEOUT);
        (self.reached.helper).call_expecting(&request, by, &expected)
    }
}

impl Creator<ListenDials, Listened> for Modal<'_> {
    fn create(&self, create: &Create, dials: ListenDials) -> Result<Listened, String> {
        self.create_sandbox(create, &dials)
    }
}

impl Listens for Modal<'_> {
    /// Publishes `images` as [`Listens::publish_companion_images`] asks. Each is pushed as a
    /// role's is ([`Provider::publish_image`]), once however often `images` names it. Modal then
    /// imports them all in one call, and builds `sandboxd` into each. A sidecar starts only from an
    /// image that Modal has built. So no create waits for a build.
    fn publish_companion_images(
        &self,
        images: &[Image],
        sandboxd: &SandboxdRef,
    ) -> Result<Vec<ImageRef>, String> {
        let sandboxd = Sandboxd::published(sandboxd, vec![SANDBOXD_PATH])?;
        let batch = Batch::of(images);
        let pushed = self.push(&batch.distinct().collect::<Vec<_>>())?;
        batch.references(self.import(&pushed, Some(sandboxd))?)
    }
}
