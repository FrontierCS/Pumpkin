//! What a model response reports of itself, read as the response passes through: the tokens it
//! used, and the failure it states.

use serde_json::Value;

/// The most that a response is held for its usage: one server-sent event's line, or a response's
/// whole body when it is not a stream. What is longer reports no usage.
const MAX_HELD_BYTES: usize = 16 << 20;

/// The most bytes of a failure's code that [`Failure::code`] gives.
// Twice the longest that the Responses API lists for a failed response, 30 bytes, which leaves
// room for another provider's codes, and none for what a response held.
const MAX_CODE_BYTES: usize = 64;

/// Where an API's usage gives its input and output tokens, and the pointers to its cached and
/// reasoning tokens.
pub(crate) type Fields = [&'static str; 4];

/// Where the Responses API's usage gives them.
pub(crate) const RESPONSES_FIELDS: Fields = [
    "input_tokens",
    "output_tokens",
    "/input_tokens_details/cached_tokens",
    "/output_tokens_details/reasoning_tokens",
];

/// Where the Chat Completions API's usage gives them.
pub(crate) const CHAT_FIELDS: Fields = [
    "prompt_tokens",
    "completion_tokens",
    "/prompt_tokens_details/cached_tokens",
    "/completion_tokens_details/reasoning_tokens",
];

/// The tokens that a model response reports it used, as its provider counts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The input tokens that the provider served from its cache, if it says.
    pub cached_tokens: Option<u64>,
    /// The output tokens that went to the model's reasoning, if the provider says.
    pub reasoning_tokens: Option<u64>,
}

impl Usage {
    /// The usage that `value`, a response or an event that holds one, reports, if it does: in
    /// its `usage`, or its `response`'s, in `fields`.
    fn of(value: &Value, fields: Fields) -> Option<Self> {
        let usage = value
            .get("usage")
            .or_else(|| value.get("response")?.get("usage"))?;
        let count = |value: Option<&Value>| value?.as_u64();
        let [input, output, cached, reasoning] = fields;
        Some(Self {
            input_tokens: count(usage.get(input))?,
            output_tokens: count(usage.get(output))?,
            cached_tokens: count(usage.pointer(cached)),
            reasoning_tokens: count(usage.pointer(reasoning)),
        })
    }
}

/// The failure that a streamed response states in one of its events, whatever its status.
///
/// A Responses `response.failed` states one, whose response's `error` gives its code, as does an
/// `error` event, which gives it itself, and an event of either API that holds an `error`, which
/// gives it there, as a Chat Completions stream states one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    code: Option<String>,
}

impl Failure {
    /// The code that the response gave for its failure: the first of its failures' codes that is
    /// an identifier, of at most `MAX_CODE_BYTES` ASCII letters, digits, `_`, `-` and `.`, if one
    /// is.
    pub fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }

    /// The failure that `event`, a stream's, states, if it does.
    fn of(event: &Value) -> Option<Self> {
        let error = match (event.get("error"), event["type"].as_str()) {
            (Some(error), _) if !error.is_null() => Some(error),
            (_, Some("response.failed")) => event.pointer("/response/error"),
            (_, Some("error")) => Some(event),
            _ => return None,
        };
        let code = error.and_then(|error| error.get("code")?.as_str());
        let code = code.filter(|code| {
            (1..=MAX_CODE_BYTES).contains(&code.len())
                && (code.bytes()).all(|byte| byte.is_ascii_alphanumeric() || b"_-.".contains(&byte))
        });
        Some(Self {
            code: code.map(str::to_owned),
        })
    }
}

/// Reads a response's usage from its body as the body goes by, from the last event of a stream
/// that holds one, or from a response that is one JSON value, and the failure that a stream
/// states ([`Failure`]).
pub(crate) struct Scan {
    fields: Fields,
    stream: bool,
    // A stream's current line, or a plain body; `None` once it passed its bound: a stream's line
    // is skipped to its end, and a plain body is given up on.
    held: Option<Vec<u8>>,
    usage: Option<Usage>,
    failed: Option<Failure>,
}

impl Scan {
    /// A scan of a body whose `content-type` is `content_type`, whose usage is in `fields`.
    pub(crate) fn new(content_type: Option<&str>, fields: Fields) -> Self {
        let stream = content_type.is_some_and(|value| value.contains("text/event-stream"));
        Self {
            fields,
            stream,
            held: Some(Vec::new()),
            usage: None,
            failed: None,
        }
    }

    /// Takes the next `bytes` of the body.
    pub(crate) fn feed(&mut self, mut bytes: &[u8]) {
        if !self.stream {
            self.hold(bytes);
            return;
        }
        while let Some(end) = bytes.iter().position(|&byte| byte == b'\n') {
            self.hold(&bytes[..end]);
            self.line();
            bytes = &bytes[end + 1..];
        }
        self.hold(bytes);
    }

    /// The usage that the body reported, and the failure it stated, once it has ended.
    pub(crate) fn finish(mut self) -> (Option<Usage>, Option<Failure>) {
        if self.stream {
            self.line();
        } else if let Some(held) = &self.held {
            self.usage = serde_json::from_slice(held)
                .ok()
                .and_then(|body| Usage::of(&body, self.fields));
        }
        (self.usage, self.failed)
    }

    fn hold(&mut self, bytes: &[u8]) {
        let Some(held) = &mut self.held else {
            return;
        };
        if held.len() + bytes.len() > MAX_HELD_BYTES {
            self.held = None;
        } else {
            held.extend_from_slice(bytes);
        }
    }

    /// Ends the stream's current line, and keeps the usage it holds, and the failure it states,
    /// if any.
    fn line(&mut self) {
        let Some(held) = self.held.replace(Vec::new()) else {
            return;
        };
        let data = held.strip_prefix(b"data:").map(<[u8]>::trim_ascii);
        let Some(event) = data.and_then(|data| serde_json::from_slice::<Value>(data).ok()) else {
            return;
        };
        self.usage = Usage::of(&event, self.fields).or(self.usage);
        if let Some(failure) = Failure::of(&event) {
            let code = self.failed.take().and_then(|failed| failed.code);
            self.failed = Some(Failure {
                code: code.or(failure.code),
            });
        }
    }
}
