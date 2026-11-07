//! The model route: what the host does with the model requests that a role with a route makes
//! over its sandbox's session, each a [`ModelRequest`].
//!
//! A sandbox's only way out is its session, so the route is the agent's only way to a model,
//! and what the route admits is all that the agent can reach through one. The route is to stay
//! *closed*: a request reaches the models its workflow allows and nothing else of the model
//! provider's, neither the state it keeps nor the tools it runs nor the URLs it fetches, so that
//! a response carries nothing but what its own request sent.
//!
//! [`Upstream::serve`] answers each request: the [`Route`] admits a request, or the client hears
//! why it is refused. The route admits a request that:
//!
//! - is `POST /responses`, the one endpoint through which a harness samples its model;
//! - passes on only the headers that the harness that runs use sends, each with a value that
//!   holds no control character but tab: `content-type`, `accept`, `originator`, `user-agent`,
//!   `session-id`, `thread-id`, `x-codex-beta-features`, `x-codex-window-id`,
//!   `x-codex-turn-metadata` and `x-openai-internal-codex-responses-lite`. Every other header
//!   stays behind, any credential included;
//! - holds a JSON object that sets `store` to `false` and refers to no state kept on the
//!   provider's side: no `previous_response_id`, `conversation`, `prompt` or `background`, no
//!   `input` item that is an `item_reference` or names no `type`, and no `file_id`,
//!   `container_id` or `comparison_response_id` anywhere;
//! - names a model the route allows, and no `access_programs` but `null`;
//! - declares client-side tools only, in every list of tools anywhere in the body (every
//!   `tools` field that holds an array): `function`, `custom`, `namespace`, and `tool_search`
//!   with client execution; and chooses no other kind of tool as its `tool_choice` than
//!   `function`, `custom` or `allowed_tools`;
//! - holds no URL but inline data: every `url`, `image_url` or `file_url` begins with `data:`.
//!
//! These rules refuse the forms they name and pass every other field: an allow-list of the
//! fields a harness needs, which would refuse any other, is not part of the route yet. They read
//! the body's JSON fields, never the text of its context, and the body that goes upstream is the
//! one they read, its model named as the upstream takes it. A refusal's status is 404 if the
//! target is not `/responses`, 405 if the method is not `POST`, and 400 for any other rule
//! broken.
//!
//! [`Upstream::serve`] sends an admitted request to the model provider, streams back its
//! response, and says how it answered ([`Answer`]). A run serves its route as [`Serving`] says.
//!
//! [`ModelRequest`]: pumpkin_gateway::ModelRequest

mod closure;
mod upstream;

pub use closure::Route;
pub use upstream::{Answer, Key, Serving, Upstream, UpstreamUrl};
