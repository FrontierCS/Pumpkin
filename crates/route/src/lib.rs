//! The model route: what the host does with the model requests that a role with a route makes
//! over its sandbox's session, each a [`ModelRequest`](gateway::ModelRequest).
//!
//! A sandbox's only way out is its session, so the route is the agent's only way to a model. The
//! route is to stay *closed*: a request reaches the models its workflow allows and nothing else
//! of the model provider's, neither the state it keeps nor the tools it runs nor the URLs it
//! fetches, so that a response carries nothing but what its own request sent.
//!
//! [`Upstream::serve`] answers each request: the [`Route`] admits it, or the client hears why it
//! is refused, with 404 for a target other than `/responses`, 405 for a method other than `POST`,
//! and 400 for any other rule broken. The route admits a request that:
//!
//! - is `POST /responses`, the one endpoint through which a harness samples its model;
//! - passes on only the headers that the harness sends (`PASSED_HEADERS`), each with a value
//!   that holds no control character but tab, and leaves every other behind;
//! - holds a JSON object that sets `store` to `false` and refers to no state kept on the
//!   provider's side, such as by `previous_response_id` or `file_id`;
//! - names a model the route allows, and no `access_programs` but `null`;
//! - declares and chooses client-side tools only, in every list of tools anywhere in the body;
//! - holds no URL but inline data (`URL_FIELDS`).
//!
//! The rules refuse the forms they name and pass every other field. They read the body's JSON
//! fields, never the text of its context, and the body that goes upstream is the one they read,
//! its model named as the upstream takes it. A run serves its route as [`Serving`] says, and the
//! upstream says how it answered ([`Answer`]).

mod closure;
mod upstream;

pub use closure::Route;
pub use upstream::{Answer, Key, Serving, Upstream, UpstreamUrl};
