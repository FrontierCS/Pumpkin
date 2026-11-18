//! A closed Responses request and its answer over the Chat Completions API. The run keeps the
//! upstream exchange, while the caller receives Responses objects and events.

mod request;
pub(crate) mod response;
pub(crate) mod stream;

pub(crate) use request::{explicit_messages, request};

/// The request fields that the Responses answer echoes, and how its client reads that answer.
#[derive(Debug)]
pub(crate) struct Request {
    pub(crate) stream: bool,
    pub(crate) echo: serde_json::Value,
}
