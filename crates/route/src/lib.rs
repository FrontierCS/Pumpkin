//! The model route: what the host does with the model requests that a role with a route makes
//! over its sandbox's session, each a [`ModelRequest`](gateway::ModelRequest).
//!
//! A sandbox's only way out is its session, so the route is the agent's only way to a model. The
//! route is to stay *closed*: a request reaches the models its workflow allows and nothing else
//! of the model provider's, neither the state it keeps nor the tools it runs nor the URLs it
//! fetches, so that a response carries nothing but what its own request sent.
//!
//! [`serve`] answers each request through the entry of the route whose endpoint it names, and of
//! several, the one that allows its model: that entry's [`Route`] admits it, or the client hears
//! why it is refused.
//! The rules of each API are the `closure` module's.
//!
//! [`serve`] sends an admitted request upstream ([`Upstream`]), streams back its response, and
//! says how it answered ([`Answer`]). A run serves each entry as a [`Serving`] says.
//!
//! [`read`] reads an exchange as it was kept, whatever its API.

mod bridge;
mod closure;
pub mod read;
mod sse;
mod upstream;
mod usage;

pub use closure::{Headers, MAX_DROPPED_HEADERS, Route};
pub use upstream::{
    Admission, Answer, Bar, Key, RequestRepair, Serving, Upstream, UpstreamModel, UpstreamUrl,
    serve,
};

/// Where [`serve`] keeps a model exchange as it passes, for the run's record.
pub trait Keep {
    /// The API of a translated request, any repair applied, and the headers left behind as it went
    /// upstream, those of them that an entry could list ([`Headers`]): sorted, each once, and at
    /// most [`MAX_DROPPED_HEADERS`]. Called once, before [`Keep::request`], for a request that
    /// goes upstream.
    fn upstream(
        &self,
        translated_to: Option<schema::ir::Api>,
        repair: Option<RequestRepair>,
        dropped_headers: &[String],
    );
    /// The request's body as it went upstream, with the fields of its API that a record may keep
    /// apart, if it is one the route read; or, if it never went upstream, as its client sent it.
    fn request(&self, body: &[u8], apart: Option<Apart>);
    /// The next bytes of the answer's body: the upstream's, as each part arrives, even where the
    /// route answers its client with a refusal in its place, or, if none came, the refusal.
    fn response(&self, bytes: &[u8]);
    /// The upstream's stream has just begun the model's output: of the bytes given
    /// [`Keep::response`] so far, the last hold the end of the stream's first event
    /// that its API counts as output, such as a delta of its text. Called once at most, and
    /// never for a body that is no stream.
    fn first_output(&self);
}
pub use usage::{Failure, Usage};

/// The fields of an API's request body that a client sends again on each request: its
/// conversation, and the fields it sends whole and most often unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Apart {
    /// The field that holds the conversation, an array of its items, the earlier ones first.
    pub conversation: &'static str,
    /// The fields sent whole, such as the list of tools.
    pub whole: &'static [&'static str],
}
