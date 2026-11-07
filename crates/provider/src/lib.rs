//! The interface every provider implements, [`Provider`]. The values it takes and returns are
//! [`schema::provider`]'s.

use schema::provider::{
    Console, Create, Created, Listed, Local, Published, Ready, RunTag, SandboxId,
};

/// A provider that a run has connected to, and the one way the run acts on it.
///
/// It publishes what sandboxes run, and creates, lists, reads and deletes sandboxes. A
/// sandbox's session doesn't pass through it: the caller accepts each session where the
/// sandbox's [`Create`] says its `sandboxd` dials.
pub trait Provider: Send + Sync {
    /// What the provider declared when the run connected to it.
    fn ready(&self) -> &Ready;

    /// Publishes each of `locals` so that this provider's sandboxes can run it, and returns, in
    /// the same order, the name a [`Create`] gives each.
    ///
    /// # Errors
    ///
    /// Fails if one can't be published, such as an image the provider can't reach, and names it.
    fn publish(&self, locals: &[Local]) -> Result<Vec<Published>, String>;

    /// Creates the sandbox that `create` describes, and starts its `sandboxd`.
    ///
    /// # Errors
    ///
    /// Fails if the sandbox can't be created or started. A failed create deletes what it made,
    /// or its error says what is left; anything it can't name, [`list`](Self::list) still finds.
    fn create(&self, create: &Create) -> Result<Created, String>;

    /// Deletes those of `ids` that are `run`'s sandboxes, running or not, and returns the others
    /// untouched: ids of another run's sandboxes, and ids of no sandbox.
    ///
    /// # Errors
    ///
    /// Fails if the provider can't tell which of `ids` are `run`'s, or can't delete one; it
    /// still deletes the others.
    fn delete(&self, run: &RunTag, ids: &[SandboxId]) -> Result<Vec<SandboxId>, String>;

    /// Lists the sandboxes tagged with `run`, running or not, whatever created them.
    ///
    /// # Errors
    ///
    /// Fails if the provider can't list them.
    fn list(&self, run: &RunTag) -> Result<Vec<Listed>, String>;

    /// Reads what sandbox `id` of `run` has printed, its [`Console`].
    ///
    /// # Errors
    ///
    /// Fails if `id` isn't one of `run`'s sandboxes, such as another run's, or if the provider
    /// can't read what it printed.
    fn console(&self, run: &RunTag, id: &SandboxId) -> Result<Console, String>;
}
