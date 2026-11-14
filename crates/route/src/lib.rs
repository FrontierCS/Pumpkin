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

pub use closure::Route;
pub use upstream::{Answer, Key, Serving, Upstream, UpstreamUrl, serve};
