//! Chat data events become Responses events as they arrive. A finished choice can still be
//! followed by usage or an error: its terminal response waits for both `[DONE]` and HTTP EOF.
//!
//! <https://developers.openai.com/api/reference/resources/responses/streaming-events>.

mod output;

use std::io;

use output::Output;
use serde_json::{Value, json};

use super::response::{model_name, response, usage};
use crate::usage::{CHAT, Failure, MAX_HELD_BYTES, Usage};

enum Error {
    Invalid(&'static str),
    Unsent(io::Error),
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Unsent(error)
    }
}

type Sink<'a> = dyn FnMut(&[u8]) -> io::Result<()> + 'a;

struct Events<'a> {
    sequence: &'a mut u64,
    emit: &'a mut Sink<'a>,
}

impl Events<'_> {
    fn send(&mut self, kind: &str, mut event: Value) -> io::Result<()> {
        event["type"] = json!(kind);
        event["sequence_number"] = json!(*self.sequence);
        *self.sequence += 1;
        (self.emit)(format!("event: {kind}\ndata: {event}\n\n").as_bytes())
    }
}

pub(crate) struct Stream {
    echo: Value,
    model: String,
    upstream_name: String,
    identity: Option<(String, u64, String)>,
    sequence: u64,
    line: Vec<u8>,
    data: Option<Vec<u8>>,
    output: Output,
    usage: Option<Usage>,
    reason: Option<&'static str>,
    done: bool,
    failed: Option<Failure>,
}

impl Stream {
    pub(crate) fn new(echo: &Value, model: &str, upstream_name: &str) -> Self {
        Self {
            echo: echo.clone(),
            model: model.to_owned(),
            upstream_name: upstream_name.to_owned(),
            identity: None,
            sequence: 0,
            line: Vec::new(),
            data: None,
            output: Output::default(),
            usage: None,
            reason: None,
            done: false,
            failed: None,
        }
    }

    pub(crate) fn feed(
        &mut self,
        bytes: &[u8],
        mut emit: impl FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        match self.read(bytes, &mut emit) {
            Ok(()) => Ok(()),
            Err(Error::Invalid(message)) => self.fail("invalid_chat_stream", message, &mut emit),
            Err(Error::Unsent(error)) => Err(error),
        }
    }

    fn read(&mut self, bytes: &[u8], emit: &mut Sink<'_>) -> Result<(), Error> {
        for part in bytes.split_inclusive(|&byte| byte == b'\n') {
            if self.failed.is_some() {
                break;
            }
            let line = part.strip_suffix(b"\n");
            let piece = line.unwrap_or(part);
            if piece.len() > MAX_HELD_BYTES - self.line.len() {
                return Err(Error::Invalid("Chat event exceeds the route's byte limit"));
            }
            self.line.extend_from_slice(piece);
            if line.is_none() {
                continue;
            }
            let raw = std::mem::take(&mut self.line);
            let line = raw.strip_suffix(b"\r").unwrap_or(&raw);
            if line.is_empty() {
                if let Some(data) = self.data.take() {
                    self.chunk(data[b"data:".len()..].trim_ascii(), emit)?;
                }
            } else if line.starts_with(b"data:") {
                if self.data.is_some() {
                    return Err(Error::Invalid("expected one Chat data line per event"));
                }
                self.data = Some(raw);
            } else if !(line.starts_with(b":") || line.starts_with(b"event:")) {
                return Err(Error::Invalid("expected a Chat data event"));
            }
        }
        Ok(())
    }

    fn chunk(&mut self, data: &[u8], emit: &mut Sink<'_>) -> Result<(), Error> {
        if self.done {
            return Err(Error::Invalid("Chat data followed [DONE]"));
        }
        if data == b"[DONE]" {
            if self.reason.is_none() {
                return Err(Error::Invalid("Chat stream ended without a finish reason"));
            }
            self.done = true;
            return Ok(());
        }
        let chunk: Value = serde_json::from_slice(data)
            .map_err(|_| Error::Invalid("invalid JSON in Chat stream"))?;
        if !chunk["error"].is_null() || chunk["type"] == "error" {
            self.fail(
                "upstream_error",
                "the model provider reported a stream error",
                emit,
            )?;
            return Ok(());
        }
        if !chunk["moderation"].is_null() {
            return Err(Error::Invalid(
                "Chat moderation output cannot be translated",
            ));
        }
        if chunk["object"] != "chat.completion.chunk" {
            return Err(Error::Invalid("expected a Chat completion chunk"));
        }
        let id = self.begin(&chunk, emit)?;
        self.usage = CHAT(self.usage, &chunk);
        let choices = chunk["choices"]
            .as_array()
            .ok_or(Error::Invalid("Chat chunk has no choices array"))?;
        let choice = match &choices[..] {
            [] if !chunk["usage"].is_null() && self.reason.is_some() => return Ok(()),
            [choice] if choice["index"].as_u64() == Some(0) && self.reason.is_none() => choice,
            _ => return Err(Error::Invalid("expected one Chat choice, then final usage")),
        };
        if !choice["logprobs"].is_null() {
            return Err(Error::Invalid(
                "Chat log probabilities cannot be translated",
            ));
        }
        self.output.delta(
            &choice["delta"],
            id,
            &mut Events {
                sequence: &mut self.sequence,
                emit,
            },
        )?;
        self.reason = match optional(choice, "finish_reason")? {
            None => None,
            Some("stop") => Some("stop"),
            Some("tool_calls") if self.output.has_calls() => Some("tool_calls"),
            Some("length") => Some("max_output_tokens"),
            Some("content_filter") => Some("content_filter"),
            _ => {
                return Err(Error::Invalid(
                    "unsupported or inconsistent Chat finish reason",
                ));
            }
        };
        Ok(())
    }

    fn begin<'a>(&mut self, chunk: &'a Value, emit: &mut Sink<'_>) -> Result<&'a str, Error> {
        let id = chunk["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or(Error::Invalid("Chat chunk has no completion ID"))?;
        let created = chunk["created"]
            .as_u64()
            .ok_or(Error::Invalid("Chat chunk has no creation time"))?;
        let answered = chunk["model"]
            .as_str()
            .ok_or(Error::Invalid("Chat chunk has no model"))?;
        if let Some((seen, at, model)) = &self.identity {
            if seen != id || *at != created || model != answered {
                return Err(Error::Invalid(
                    "Chat completion identity changed within the stream",
                ));
            }
        } else {
            self.identity = Some((id.to_owned(), created, answered.to_owned()));
            self.model = model_name(&self.model, &self.upstream_name, answered).to_owned();
            let start = response(
                &self.echo,
                id,
                &self.model,
                created,
                "in_progress",
                json!([]),
                Value::Null,
            );
            let mut events = Events {
                sequence: &mut self.sequence,
                emit,
            };
            events.send("response.created", json!({"response":start}))?;
            events.send("response.in_progress", json!({"response":start}))?;
        }
        Ok(id)
    }

    fn fail(&mut self, code: &'static str, message: &str, emit: &mut Sink<'_>) -> io::Result<()> {
        if self.failed.is_some() {
            return Ok(());
        }
        self.failed = Some(Failure::new(code));
        let mut events = Events {
            sequence: &mut self.sequence,
            emit,
        };
        let Some((id, created, _)) = &self.identity else {
            return events.send("error", json!({"code":code,"message":message,"param":null}));
        };
        let mut body = response(
            &self.echo,
            id,
            &self.model,
            *created,
            "failed",
            self.output.snapshot("incomplete"),
            usage(self.usage),
        );
        body["error"] = json!({"code":"server_error","message":message});
        events.send("response.failed", json!({"response":body}))
    }

    pub(crate) fn broken(
        mut self,
        mut emit: impl FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<Option<Failure>> {
        self.fail(
            "incomplete_chat_stream",
            "the model provider's Chat stream broke off",
            &mut emit,
        )?;
        Ok(self.failed)
    }

    pub(crate) fn finish(
        mut self,
        mut emit: impl FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<Option<Failure>> {
        if self.failed.is_some() {
            return Ok(self.failed);
        }
        if !self.done || !self.line.is_empty() || self.data.is_some() {
            return self.broken(emit);
        }
        let Some(reason) = self.reason else {
            unreachable!("[DONE] requires a finish reason")
        };
        let incomplete = matches!(reason, "max_output_tokens" | "content_filter");
        let status = if incomplete {
            "incomplete"
        } else {
            "completed"
        };
        let mut events = Events {
            sequence: &mut self.sequence,
            emit: &mut emit,
        };
        let Some((id, created, _)) = &self.identity else {
            unreachable!("a finish reason came from a choice")
        };
        match self.output.close(status, id, &mut events) {
            Err(Error::Invalid(message)) => {
                self.fail("invalid_chat_stream", message, &mut emit)?;
                return Ok(self.failed);
            }
            Err(Error::Unsent(error)) => return Err(error),
            Ok(()) => {}
        }
        let mut body = response(
            &self.echo,
            id,
            &self.model,
            *created,
            status,
            self.output.snapshot(status),
            usage(self.usage),
        );
        if incomplete {
            body["incomplete_details"] = json!({"reason":reason});
        }
        events.send(&format!("response.{status}"), json!({"response":body}))?;
        Ok(incomplete.then(|| Failure::new(reason)))
    }
}

fn optional<'a>(value: &'a Value, key: &str) -> Result<Option<&'a str>, Error> {
    match &value[key] {
        Value::String(text) => Ok(Some(text)),
        Value::Null => Ok(None),
        _ => Err(Error::Invalid("expected a text field in a Chat chunk")),
    }
}
