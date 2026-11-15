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

pub(crate) const RESPONSES_FIELDS: Fields = [
    "input_tokens",
    "output_tokens",
    "/input_tokens_details/cached_tokens",
    "/output_tokens_details/reasoning_tokens",
];

pub(crate) const CHAT_FIELDS: Fields = [
    "prompt_tokens",
    "completion_tokens",
    "/prompt_tokens_details/cached_tokens",
    "/completion_tokens_details/reasoning_tokens",
];

/// Whether an event of an API's stream is the model's output: the first that is marks the
/// stream's first token.
// As deepseek-harness's `isTokenDelta` takes a chunk for the first token: a delta of text,
// reasoning or a call's arguments that is not empty, or a call's delta that names it,
// https://github.com/deepseek-ai/deepseek-harness/blob/39163dd30f703a79079e2d2446545cff3c453738/packages/llm/llm/src/assistant-stream.ts#L242-L258
pub(crate) type Output = fn(&Value) -> bool;

/// A Responses event that is output: a delta that is not empty, of text, a refusal, reasoning,
/// or a call's arguments or input, or the item added of a call, which names it before its
/// arguments come.
// A custom tool call, such as Codex's `apply_patch`, streams as its item added and its input's
// deltas, as Codex reads them:
// https://github.com/openai/codex/blob/09db085f9456c608a7960fdd4c5c55a34c6a84ea/codex-rs/codex-api/src/sse/responses.rs#L399-L408
pub(crate) fn responses_output(event: &Value) -> bool {
    match event["type"].as_str() {
        Some(
            "response.output_text.delta"
            | "response.refusal.delta"
            | "response.reasoning_text.delta"
            | "response.reasoning_summary_text.delta"
            | "response.function_call_arguments.delta"
            | "response.custom_tool_call_input.delta",
        ) => event["delta"]
            .as_str()
            .is_some_and(|delta| !delta.is_empty()),
        Some("response.output_item.added") => {
            matches!(
                event["item"]["type"].as_str(),
                Some("function_call" | "custom_tool_call")
            )
        }
        _ => false,
    }
}

/// A Chat Completions chunk that is output: one whose choice's delta holds content or reasoning
/// that is not empty, or a tool call that is named or has arguments; a delta that only names its
/// role, or holds empty content, isn't.
// Providers name the reasoning `reasoning_content` (DeepSeek, Z.ai) or `reasoning` (OpenRouter).
pub(crate) fn chat_output(event: &Value) -> bool {
    let said = |value: &Value| value.as_str().is_some_and(|text| !text.is_empty());
    let choices = event["choices"].as_array().into_iter().flatten();
    choices.map(|choice| &choice["delta"]).any(|delta| {
        let calls = delta["tool_calls"].as_array().into_iter().flatten();
        let call = |call: &Value| {
            call["function"]["name"].is_string() || said(&call["function"]["arguments"])
        };
        ["content", "reasoning_content", "reasoning"]
            .into_iter()
            .any(|field| said(&delta[field]))
            || calls.into_iter().any(call)
    })
}

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
/// that holds one, or from a response that is one JSON value, the failure that a stream states
/// ([`Failure`]), and where a stream's output begins ([`Output`]).
pub(crate) struct Scan {
    fields: Fields,
    output: Output,
    stream: bool,
    /// Whether an event of the stream was output.
    began: bool,
    // A stream's current line, or a plain body; `None` once it passed its bound: a stream's line
    // is skipped to its end, and a plain body is given up on.
    held: Option<Vec<u8>>,
    usage: Option<Usage>,
    failed: Option<Failure>,
}

impl Scan {
    pub(crate) fn new(content_type: Option<&str>, fields: Fields, output: Output) -> Self {
        let stream = content_type.is_some_and(|value| value.contains("text/event-stream"));
        Self {
            fields,
            output,
            stream,
            began: false,
            held: Some(Vec::new()),
            usage: None,
            failed: None,
        }
    }

    /// Reads `bytes`, the body's next, and returns whether the stream's output began in them:
    /// whether a line they end holds its first event of output.
    pub(crate) fn feed(&mut self, mut bytes: &[u8]) -> bool {
        if !self.stream {
            self.hold(bytes);
            return false;
        }
        let began = self.began;
        while let Some(end) = bytes.iter().position(|&byte| byte == b'\n') {
            self.hold(&bytes[..end]);
            self.line();
            bytes = &bytes[end + 1..];
        }
        self.hold(bytes);
        !began && self.began
    }

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

    fn line(&mut self) {
        let Some(held) = self.held.replace(Vec::new()) else {
            return;
        };
        let data = held.strip_prefix(b"data:").map(<[u8]>::trim_ascii);
        let Some(event) = data.and_then(|data| serde_json::from_slice::<Value>(data).ok()) else {
            return;
        };
        self.usage = Usage::of(&event, self.fields).or(self.usage);
        self.began |= (self.output)(&event);
        if let Some(failure) = Failure::of(&event) {
            let code = self.failed.take().and_then(|failed| failed.code);
            self.failed = Some(Failure {
                code: code.or(failure.code),
            });
        }
    }
}
