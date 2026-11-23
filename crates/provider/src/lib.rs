//! The interface every provider implements.
//!
//! A provider is a crate `<name>` at `providers/<name>`.

use std::fmt;
use std::net::IpAddr;
use std::path::Path;

use schema::ir::Image;
use schema::sandbox::{Attestation, Endpoint, Listening, PrivateDial, SocketDial};
pub use values::{
    Console, Create, Created, ImageRef, Listed, Ready, Role, RunTag, SandboxId, SandboxdRef,
    Undeleted, UnixTime,
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
    /// An id of another run's sandbox, or of one whose run the provider can't tell, is refused and
    /// left untouched; a sandbox whose delete fails may still run. Each id is tried on its own:
    /// [`Undeleted`] holds each of `ids` that failed, with why, and no other id; every other is
    /// deleted all the same.
    fn delete(&self, run: &RunTag, ids: &[SandboxId]) -> Result<(), Undeleted>;

    /// Lists the sandboxes tagged with `run`, whatever created them, each with whether it has ended
    /// ([`Listed::ended`]): every one, from when its create returns, or, for one that a create made
    /// after giving up ([`create`](Creator::create)), from when the platform holds it, until it is
    /// deleted. It may leave out only one that has ended and that the provider has freed itself, as
    /// a delete would.
    fn list(&self, run: &RunTag) -> Result<Vec<Listed>, String>;
}

/// A provider that a run has connected to, and the one way the run acts on it.
///
/// It publishes what sandboxes run, and creates, lists, reads and deletes sandboxes; a sandbox's
/// session doesn't pass through it. Each call returns within a bound: a [create](Creator::create)
/// by the deadline it is given, any other by one the provider's doc states. No call's outcome rests
/// on anything an earlier call left in the instance, only on what it is given and what the
/// platform holds: so another instance, connected to the same platform, may make any call in its
/// place, and a sandbox outlives the instance that created it.
pub trait Provider: Sweep + Send + Sync {
    /// What the provider declared when the run connected to it.
    fn ready(&self) -> &Ready;

    /// Holds for `run` what its roles on this provider need, beside what other runs hold, until the
    /// [`Reservation`] is dropped or its process ends: `phases` are the run's roles, those of a
    /// phase running at once, [`instances`](Role::instances) of each. A shared host gives each
    /// create its [instance](Create::instance)'s share; a host of its own holds nothing. Fails,
    /// holding nothing, with [`Unreserved::Busy`] if the run's needs fit only once other runs end.
    ///
    /// # Panics
    ///
    /// If a role is in more than one phase.
    fn reserve(&self, run: &RunTag, phases: &[Vec<Role>]) -> Result<Reservation, Unreserved>;

    /// Fails, saying why, if [`reserve`](Self::reserve) of `phases` would fail were no other run
    /// to hold anything: a run that fails it never fits, and one that passes it fits once other
    /// runs end. Holds nothing. A provider that gives each sandbox a host of its own fits any.
    fn fits(&self, _phases: &[Vec<Role>]) -> Result<(), String> {
        Ok(())
    }

    /// Publishes `image`, a role's, so that this provider's sandboxes can run it.
    fn publish_image(&self, image: &Image) -> Result<ImageRef, String>;

    /// Publishes the static `sandboxd` binary at `path`, which a role's user can execute, so that
    /// this provider's sandboxes can run it.
    fn publish_sandboxd(&self, path: &Path) -> Result<SandboxdRef, String>;

    /// How its sandboxes meet the gateway, with the create that takes what each is given to.
    fn meeting(&self) -> Meeting<'_>;

    /// Reads what sandbox `id` of `run` has printed, its [`Console`]. Fails if `id` isn't one of
    /// `run`'s sandboxes, such as another run's.
    fn console(&self, run: &RunTag, id: &SandboxId) -> Result<Console, String>;
}

/// How a provider's sandboxes meet the gateway, each way with the create of a sandbox that takes
/// what that way gives it ([`Provider::meeting`]).
pub enum Meeting<'p> {
    /// On the gateway's host, each dialing a socket of its own.
    Local(&'p dyn Creator<SocketDial>),
    /// On a private network that the gateway's host is on, each dialing a TCP address the host
    /// has there, and presenting an attestation that the check its create gives admits.
    Private(&'p dyn Creator<PrivateDial, (Created, Admits)>),
    /// Across the Internet, each listening, and the gateway dialing it at the endpoint its create
    /// gives.
    Listen(&'p dyn Creator<Listening, (Created, Endpoint)>),
}

/// Whether a hello over a private network ([`Meeting::Private`]) is from the sandbox that a create
/// made.
///
/// It is asked of the hello's attestation and the address that the hello comes from, admits that
/// sandbox's alone, and answers at once, the same each time it is asked. Why it refuses a hello is
/// told to the hello's sender, so it names nothing of its sandbox's that the hello doesn't.
pub type Admits = Box<dyn Fn(&Attestation, IpAddr) -> Result<(), String> + Send>;

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

/// Why [`Provider::reserve`] held nothing, each with what it says of why.
#[derive(Debug)]
pub enum Unreserved {
    /// Other runs hold what the run needs, which it fits once they end: which runs, and what.
    Busy(String),
    /// Anything else, such as needs that never fit.
    Failed(String),
}

impl fmt::Display for Unreserved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy(why) | Self::Failed(why) => f.write_str(why),
        }
    }
}

/// What a provider holds for a run ([`Provider::reserve`]), until it is dropped.
pub struct Reservation(
    // Whatever the provider holds it by, such as a locked file; its drop releases it.
    #[expect(
        dead_code,
        reason = "the reservation is held by owning it, and released by dropping it"
    )]
    Box<dyn Send + Sync>,
);

impl Reservation {
    /// A reservation that holds `held`, which releases what it holds when dropped.
    pub fn new(held: impl Send + Sync + 'static) -> Self {
        Self(Box::new(held))
    }
}

impl fmt::Debug for Reservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reservation").finish_non_exhaustive()
    }
}
