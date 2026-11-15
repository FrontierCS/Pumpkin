//! The model route: what the host does with the model requests that a role with a route makes
//! over its sandbox's session, each a [`ModelRequest`](gateway::ModelRequest).
//!
//! A sandbox's only way out is its session, so the route is the agent's only way to a model. The
//! route is to stay *closed*: a request reaches the models its workflow allows and nothing else
//! of the model provider's, neither the state it keeps nor the tools it runs nor the URLs it
//! fetches, so that a response carries nothing but what its own request sent.
//!
//! [`serve`] answers each request through the entry of the route, one for each API, whose
//! endpoint it names: that entry's [`Route`] admits it, or the client hears why it is refused.
//! The rules of each API are the `closure` module's.
//!
//! [`serve`] sends an admitted request upstream ([`Upstream`]), streams back its response, and
//! says how it answered ([`Answer`]). A run serves each entry as a [`Serving`] says.

mod closure;
mod upstream;
mod usage;

pub use closure::Route;
pub use upstream::{Answer, Key, Serving, Upstream, UpstreamUrl, serve};

/// Where [`serve`] keeps a model exchange as it passes, for the run's record.
pub trait Keep {
    /// The request's body as it went upstream, with the field that holds its conversation, if
    /// it is one the route read; or, if it never went upstream, as its client sent it.
    fn request(&self, body: &[u8], conversation: Option<&'static str>);
    /// The next bytes of the answer's body: the upstream's, as each part arrives, even where the
    /// route answers its client with a refusal in its place, or, if none came, the refusal.
    fn response(&self, bytes: &[u8]);
    /// The upstream's stream has just begun the model's output: of the bytes given
    /// [`Keep::response`] so far, the last hold the end of the line of the stream's first event
    /// that its API counts as output, such as a delta of its text. Called once at most, and
    /// never for a body that is no stream.
    fn first_output(&self);
}
pub use usage::{Failure, Usage};
