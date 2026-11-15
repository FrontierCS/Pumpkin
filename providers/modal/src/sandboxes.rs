//! A role's sandbox on Modal, as [`Modal`]'s implementation of `Provider` publishes what it runs,
//! creates it, lists, reads and deletes it.
//!
//! Every run's sandboxes are in Pumpkin's one App, each tagged with its run, by which its run's
//! list finds it, and which every call on a sandbox that names one checks. Its `sandboxd`,
//! attached from this host, is started as root by Modal's own first process there, `dumb-init`,
//! and becomes the role's user before it serves (`--user`), listening behind Modal's TLS
//! tunnel, which takes connections from the settings' `dials_from` address alone. The sandbox has
//! no connection out, though Modal's resolver still answers its DNS queries; it runs under gVisor
//! with its class's GPU, if any, and its role's CPU and memory as both its request and its limit;
//! and Modal ends it at its run's hard end, if nothing ends it first.
//!
//! The provider never stops the App, as an App with no sandbox costs nothing.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use provider::{Console, Create, Created, Listed, RunTag, SandboxId, SandboxdRef, Sweep};
use schema::ir::Sha256Digest;
use schema::sandbox::{Endpoint, Listening};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use stdx::Deadline;

use crate::helper::{ASK_TIMEOUT, Request, Sandboxd};
use crate::{Gpu, MIB, Modal, Reached};

/// How long a delete may run before it is killed: Modal stops a sandbox in seconds, and the
/// helper waits for each of the delete's in turn.
const DELETE_TIMEOUT: Duration = Duration::from_mins(10);

/// What a published `sandboxd`'s reference holds between its path and its digest.
const DIGEST: &str = "@sha256:";

/// What every id of a sandbox of Modal's starts with, so an id without it names none.
const ID_PREFIX: &str = "sb-";

/// What the helper's `create` returns.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Made {
    id: String,
    host: String,
    port: u16,
}

/// What the helper's `list` returns of a sandbox.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Running {
    id: String,
    ended: bool,
}

/// What the helper's `delete` returns: why each sandbox that it failed to delete failed.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Deleted {
    failed: BTreeMap<String, String>,
}

/// Publishes the static `sandboxd` at `path`, as `provider::Provider::publish_sandboxd` asks: as
/// its path and its SHA-256, so that a create attaches it from this machine only while the file
/// is the one published, which nothing else needs.
pub(crate) fn publish_sandboxd(path: &Path) -> Result<SandboxdRef, String> {
    let Some(shown) = path.to_str().filter(|_| path.is_absolute()) else {
        return Err(format!(
            "the sandboxd {} isn't at an absolute path of UTF-8: give one, as Modal's SDK \
             attaches a file by such a path",
            path.display()
        ));
    };
    let bytes = fs::read(path)
        .map_err(|error| format!("the sandboxd {shown} can't be read: {error}: give a file"))?;
    let digest = Sha256Digest::from(<[u8; 32]>::from(Sha256::digest(&bytes)));
    Ok(SandboxdRef::from(format!(
        "{shown}@{}",
        String::from(digest)
    )))
}

impl Modal<'_> {
    /// Creates the sandbox that `create` describes, listening as `dial` says, as
    /// `provider::Creator::create` asks, giving up at its deadline, when the helper is killed;
    /// with the endpoint of its tunnel.
    pub(crate) fn create_sandbox(
        &self,
        create: &Create,
        dial: &Listening,
    ) -> Result<(Created, Endpoint), String> {
        let Some(class) = self.classes.get(&create.class) else {
            return Err(format!(
                "the provider has no class `{}`: expected one of those it declared",
                create.class
            ));
        };
        let Some((image, _)) = create.image.as_str().split_once('@') else {
            return Err(format!(
                "`{}` is no image this provider published: publish one first",
                create.image.as_str()
            ));
        };
        let request = self.creating(create, dial, class.gpu.as_ref(), image)?;
        let made = self
            .reached
            .helper
            .call::<Made>(&request, Deadline::at_unix(create.by.since_epoch()));
        let Made { id, host, port } = made.map_err(|error| {
            format!("{error}; what Modal made by then, if anything, the run's list finds")
        })?;
        let endpoint = Endpoint::new(host, port).map_err(|error| {
            format!("Modal's tunnel to sandbox {id} isn't an endpoint: {error}")
        })?;
        let created = Created {
            id: SandboxId::from(id),
        };
        Ok((created, endpoint))
    }

    /// The helper's request that creates the sandbox `create` describes, of a class with `gpu`,
    /// from `image`, its image's id in Modal, or why Modal can't make that sandbox.
    fn creating<'a>(
        &'a self,
        create: &'a Create,
        dial: &'a Listening,
        gpu: Option<&'a Gpu>,
        image: &'a str,
    ) -> Result<Request<'a>, String> {
        let Create {
            run,
            // Modal's names are unique in an App, which holds every run's sandboxes, and a run's
            // names only in the run, so its sandboxes go unnamed.
            name: _,
            role,
            // Each sandbox has resources of its own, whichever instance of its role it is.
            instance: _,
            // Read by its caller, which gives its GPU and its image's id.
            class: _,
            image: _,
            sandboxd,
            ops,
            model,
            hard_end,
            by,
        } = create;
        let Listening { grant } = dial;
        let Some((path, sha256)) = sandboxd.as_str().rsplit_once(DIGEST) else {
            return Err(format!(
                "`{}` is no sandboxd this provider published: publish one first",
                sandboxd.as_str()
            ));
        };
        let memory_bytes = role.resources.memory_bytes.get();
        if !memory_bytes.is_multiple_of(MIB) {
            return Err(format!(
                "role `{}` asks for {memory_bytes} bytes of memory, which Modal gives in whole \
                 MiB: ask for a multiple of {MIB}",
                role.name
            ));
        }
        Ok(Request::Create {
            run: run.as_str(),
            role: role.name.as_str(),
            image,
            sandboxd: Sandboxd { path, sha256 },
            user: format!("{}:{}", role.user.uid, role.user.gid),
            ops: *ops,
            model: *model,
            grant,
            cpu_millis: role.resources.cpu_millis.get(),
            memory_mib: memory_bytes / MIB,
            gpu,
            hard_end: hard_end.since_epoch().as_secs(),
            by: by.since_epoch().as_secs(),
            source: format!("{}/32", self.dials_from),
        })
    }

    /// What sandbox `id` of `run` has printed, as `provider::Provider::console` asks: the end of
    /// its log, as Modal keeps it, then, once it has ended, its exit status, as the helper's
    /// `console` says.
    pub(crate) fn read_console(&self, run: &RunTag, id: &SandboxId) -> Result<Console, String> {
        if !id.as_str().starts_with(ID_PREFIX) {
            return Err(format!(
                "`{}` is not a Modal sandbox's id: expected one of run {}'s",
                id.as_str(),
                run.as_str()
            ));
        }
        let request = Request::Console {
            run: run.as_str(),
            id: id.as_str(),
        };
        let text = self
            .reached
            .helper
            .call::<String>(&request, Deadline::after(ASK_TIMEOUT))?;
        Ok(Console { text })
    }
}

impl Sweep for Reached {
    /// The sandboxes of `run` that Modal holds, as [`Sweep::list`] asks: those that haven't
    /// finished, as Modal frees one that has.
    fn list(&self, run: &RunTag) -> Result<Vec<Listed>, String> {
        let request = Request::List { run: run.as_str() };
        let running = (self.helper).call::<Vec<Running>>(&request, Deadline::after(ASK_TIMEOUT))?;
        let listed = (running.into_iter()).map(|Running { id, ended }| Listed {
            id: SandboxId::from(id),
            ended,
        });
        Ok(listed.collect())
    }

    /// Deletes `ids` of `run`'s sandboxes, as [`Sweep::delete`] asks: Modal terminates each in
    /// turn, once its tag says it is the run's, and the helper waits until it has finished. An
    /// id that isn't one of Modal's names no sandbox, so it counts as deleted.
    fn delete(&self, run: &RunTag, ids: &[SandboxId]) -> Result<(), String> {
        let ids: Vec<&str> = (ids.iter().map(SandboxId::as_str))
            .filter(|id| id.starts_with(ID_PREFIX))
            .collect();
        if ids.is_empty() {
            return Ok(());
        }
        let request = Request::Delete {
            run: run.as_str(),
            ids,
        };
        let Deleted { failed } = self
            .helper
            .call(&request, Deadline::after(DELETE_TIMEOUT))?;
        if failed.is_empty() {
            return Ok(());
        }
        let failed: Vec<String> = (failed.into_iter())
            .map(|(id, why)| format!("sandbox {id}: {why}"))
            .collect();
        Err(format!(
            "Modal failed to delete {} of the sandboxes: {}: fix what Modal reports",
            failed.len(),
            failed.join("; ")
        ))
    }
}
