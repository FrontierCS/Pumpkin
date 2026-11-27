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
//! and Modal ends it at its hard end, if nothing ends it first.
//!
//! A runner's companions are the sandbox's sidecars, which share its CPU, memory, tag and end.
//! A sidecar has no tunnel and no connection out. The sandbox's containers reach one another by
//! name, and nothing separates them.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use provider::{
    Console, Create, Listed, ListenDials, Listened, RunTag, SandboxId, SandboxdRef, Sweep,
    Undeleted,
};
use schema::ir::Sha256Digest;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use stdx::Deadline;
use stdx::time::{Clock as _, Machine};

use crate::create::{self, Made};
use crate::helper::{ASK_TIMEOUT, Request};
use crate::{Modal, Reached};

/// How long a delete may run before it is killed: Modal stops a sandbox in seconds, and the
/// helper waits for each of the delete's in turn.
const DELETE_TIMEOUT: Duration = Duration::from_mins(10);

/// What every id of a sandbox of Modal's starts with, so an id without it names none.
const ID_PREFIX: &str = "sb-";

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
    /// Creates the sandbox that `create` describes, each of its containers listening as `dials`
    /// says, as `provider::Creator::create` asks. It gives up at the create's deadline, when the
    /// helper is killed.
    pub(crate) fn create_sandbox(
        &self,
        create: &Create,
        dials: &ListenDials,
    ) -> Result<Listened, String> {
        let began = Machine.now();
        let Some(class) = self.classes.get(&create.class) else {
            return Err(format!(
                "the provider has no class `{}`: expected one of those it declared",
                create.class
            ));
        };
        let gpu = class.gpu.as_ref().map(|gpu| gpu.written.as_str());
        let request = Request::Create(create::request(create, dials, gpu, self.dials_from)?);
        let made = self
            .reached
            .helper
            .call::<Made>(&request, Deadline::at_unix(create.by.since_epoch()));
        let made = made.map_err(|error| {
            format!("{error}; what Modal made by then, if anything, the run's list finds")
        })?;
        // The sandbox is made by now, whatever its answer lacks.
        (made.listened(began, !dials.companions.is_empty()))
            .map_err(|error| format!("{error}; the run's list finds the sandbox"))
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
    fn delete(&self, run: &RunTag, ids: &[SandboxId]) -> Result<(), Undeleted> {
        let ids: Vec<&SandboxId> = (ids.iter())
            .filter(|id| id.as_str().starts_with(ID_PREFIX))
            .collect();
        if ids.is_empty() {
            return Ok(());
        }
        let request = Request::Delete {
            run: run.as_str(),
            ids: ids.iter().map(|id| id.as_str()).collect(),
        };
        let called = self.helper.call(&request, Deadline::after(DELETE_TIMEOUT));
        let Deleted { failed } = match called {
            Ok(deleted) => deleted,
            Err(why) => {
                let failed = ids.into_iter().map(|id| (id.clone(), why.clone()));
                return Undeleted::of(failed.collect());
            }
        };
        let failed = (failed.into_iter()).map(|(id, why)| {
            let why = format!("Modal failed to delete sandbox {id}: {why}");
            (SandboxId::from(id), why)
        });
        let mut failed: Vec<(SandboxId, String)> = failed.collect();
        // The last why ends the message, so the advice is said once.
        if let Some((_, why)) = failed.last_mut() {
            why.push_str(": fix what Modal reports");
        }
        Undeleted::of(failed)
    }
}
