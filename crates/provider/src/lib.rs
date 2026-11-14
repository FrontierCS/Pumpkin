//! The interface every provider implements, [`Provider`], the part of it that finds and
//! deletes a run's sandboxes, [`Sweep`], and the create of its sandboxes, by how they meet the
//! gateway ([`Meeting`]). The values they take and return are this crate's, such as
//! [`Create`], the IR's [`Image`], and the sandbox protocol's dial ([`SocketDial`]).

use std::path::Path;

use schema::ir::Image;
use schema::sandbox::SocketDial;
pub use values::{
    Console, Create, Created, ImageRef, Listed, Ready, Role, RunTag, SandboxId, SandboxdRef,
    UnixTime,
};

mod values;

/// What a sweep of a run's sandboxes needs of a provider: to list and delete them.
///
/// An instance may implement it alone, reached for a sweep without being fit to run on; its
/// calls are bounded, and rest only on what the platform holds, as [`Provider`]'s are.
pub trait Sweep {
    /// Deletes the sandboxes of `run` that `ids` name, running or not, and returns once each has
    /// ended and been freed: nothing runs in it, and the platform no longer holds it. An id of
    /// no sandbox, as of one deleted already, counts as deleted.
    ///
    /// # Errors
    ///
    /// Fails if the provider can't tell whose sandbox an id names, or if one is another run's,
    /// leaving either untouched, or if one can't be deleted, which may then still run. Each id is
    /// tried on its own: the error names each that failed, and every other is deleted all the same.
    fn delete(&self, run: &RunTag, ids: &[SandboxId]) -> Result<(), String>;

    /// Lists the sandboxes tagged with `run`, whatever created them, each with whether it has ended
    /// ([`Listed::ended`]): every one, from when its create returns, or, for one that a create made
    /// after giving up ([`create`](Creator::create)), from when the platform holds it, until it is
    /// deleted. It may leave out only one that has ended and that the provider has freed itself, as
    /// a delete would (a finished Modal sandbox, not a stopped EC2 instance).
    ///
    /// # Errors
    ///
    /// Fails if the provider can't list them.
    fn list(&self, run: &RunTag) -> Result<Vec<Listed>, String>;
}

/// A provider that a run has connected to, and the one way the run acts on it.
///
/// It publishes what sandboxes run, and creates, lists, reads and deletes sandboxes; a sandbox's
/// session doesn't pass through it. Each call returns within a bound: a [create](Creator::create) by
/// the deadline it is given, any other by one the provider's doc states. No call's outcome rests
/// on anything an earlier call left in the instance, only on what it is given and what the
/// platform holds: so another instance, connected to the same platform, may make any call in its
/// place, and a sandbox outlives the instance that created it.
pub trait Provider: Sweep + Send + Sync {
    /// What the provider declared when the run connected to it.
    fn ready(&self) -> &Ready;

    /// Publishes `image`, a role's, so that this provider's sandboxes can run it.
    ///
    /// # Errors
    ///
    /// Fails if it can't be published, such as an image the provider can't reach.
    fn publish_image(&self, image: &Image) -> Result<ImageRef, String>;

    /// Publishes the static `sandboxd` binary at `path`, which a role's user can execute, so that
    /// this provider's sandboxes can run it.
    ///
    /// # Errors
    ///
    /// Fails if it can't be published, such as a path that names no file.
    fn publish_sandboxd(&self, path: &Path) -> Result<SandboxdRef, String>;

    /// How its sandboxes meet the gateway, with the create that takes what each is given to.
    fn meeting(&self) -> Meeting<'_>;

    /// Reads what sandbox `id` of `run` has printed, its [`Console`].
    ///
    /// # Errors
    ///
    /// Fails if `id` isn't one of `run`'s sandboxes, such as another run's, or if the provider
    /// can't read what it printed.
    fn console(&self, run: &RunTag, id: &SandboxId) -> Result<Console, String>;
}

/// How a provider's sandboxes meet the gateway, each way with the create of a sandbox that takes
/// what that way gives it ([`Provider::meeting`]).
pub enum Meeting<'p> {
    /// On the gateway's host, each dialing a socket of its own.
    Local(&'p dyn Creator<SocketDial>),
}

/// The create of a provider's sandboxes that are given `D` to meet the gateway by, which returns
/// `C`.
pub trait Creator<D, C = Created>: Sync {
    /// Creates the sandbox that `create` describes, given `dial`, once per [name](Create::name),
    /// and starts its `sandboxd`, by [`create.by`](Create::by). Should the run's driver die before
    /// deleting it, it ends by [`create.hard_end`](Create::hard_end), or with the driver if later:
    /// one on the driver's machine with its `sandboxd`, one elsewhere by itself.
    ///
    /// # Errors
    ///
    /// Fails if it can't be created or started by `create.by`, deleting what it made or saying
    /// what is left; [`list`](Sweep::list) finds what it can't name once the platform holds it.
    fn create(&self, create: &Create, dial: D) -> Result<C, String>;
}
