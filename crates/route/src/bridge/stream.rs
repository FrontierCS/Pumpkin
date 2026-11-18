//! Chat data events become Responses events as they arrive. A finished choice can still be
//! followed by usage or an error: its terminal response waits for both `[DONE]` and HTTP EOF.
//!
//! <https://developers.openai.com/api/reference/resources/responses/streaming-events>.

mod output;

use std::fs::File;
use std::io::{self, Read};
use std::time::{SystemTime, UNIX_EPOCH};

use output::Output;
use serde_json::{Value, json};

use super::response::{model_name, provider_error, response, usage};
use crate::sse::{Frame, Frames};
use crate::usage::{CHAT, Failure, Usage};

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
    fn start(&mut self, response: Value) -> io::Result<()> {
        let mut event = json!({});
        event["response"] = response;
        self.send("response.created", event.clone())?;
        self.send("response.in_progress", event)
    }

    fn send(&mut self, kind: &str, mut event: Value) -> io::Result<()> {
        event["type"] = json!(kind);
        event["sequence_number"] = json!(*self.sequence);
        *self.sequence += 1;
        (self.emit)(format!("event: {kind}\ndata: {event}\n\n").as_bytes())
    }
}

pub(crate) enum End {
    Forwarded {
        usage: Option<Usage>,
        failed: Option<Failure>,
    },
    Cut {
        usage: Option<Usage>,
        failed: Failure,
    },
}

enum Failed {
    Provider(Failure),
    Cut(Failure),
}

impl Failed {
    fn end(self, usage: Option<Usage>, broken: bool) -> End {
        match self {
            Self::Provider(failed) if !broken => End::Forwarded {
                usage,
                failed: Some(failed),
            },
            Self::Provider(failed) | Self::Cut(failed) => End::Cut { usage, failed },
        }
    }
}

pub(crate) struct Stream {
    echo: Value,
    model: String,
    upstream_name: String,
    identity: Option<(String, u64, String)>,
    sequence: u64,
    frames: Frames,
    began: bool,
    output: Output,
    usage: Option<Usage>,
    reason: Option<&'static str>,
    done: bool,
    failed: Option<Failed>,
}

impl Stream {
    pub(crate) fn new(echo: &Value, model: &str, upstream_name: &str) -> Self {
        Self {
            echo: echo.clone(),
            model: model.to_owned(),
            upstream_name: upstream_name.to_owned(),
            identity: None,
            sequence: 0,
            frames: Frames::default(),
            began: false,
            output: Output::default(),
            usage: None,
            reason: None,
            done: false,
            failed: None,
        }
    }

    pub(crate) fn feed(
        &mut self,
        mut bytes: &[u8],
        mut emit: impl FnMut(&[u8]) -> io::Result<()>,
        mut first_output: impl FnMut(),
    ) -> io::Result<()> {
        while !bytes.is_empty() {
            let result = match self.frames.next(&mut bytes) {
                Ok(Some(frame)) => self.chunk(&frame, &mut emit, &mut first_output),
                Ok(None) => break,
                Err(message) => Err(Error::Invalid(message)),
            };
            match result {
                Err(Error::Invalid(message)) if self.failed.is_none() => {
                    self.cut("invalid_chat_stream", message, &mut emit)?;
                }
                Err(Error::Unsent(error)) => return Err(error),
                Ok(()) | Err(Error::Invalid(_)) => {}
            }
        }
        Ok(())
    }

    fn chunk(
        &mut self,
        frame: &Frame,
        emit: &mut Sink<'_>,
        first_output: &mut dyn FnMut(),
    ) -> Result<(), Error> {
        let data = frame.data.trim_ascii();
        if data == b"[DONE]" {
            if self.done || self.reason.is_none() {
                return Err(Error::Invalid("unexpected end marker in Chat stream"));
            }
            self.done = true;
            return Ok(());
        }
        let mut chunk: Value = serde_json::from_slice(data)
            .map_err(|_| Error::Invalid("invalid JSON in Chat stream"))?;
        self.usage = CHAT(self.usage, &chunk);
        if !self.began && (crate::read::dialect(schema::ir::Api::Chat).output)(&chunk) {
            self.began = true;
            first_output();
        }
        // The provider may report its usage after the caller has heard a terminal failure.
        if self.failed.is_some() {
            return Ok(());
        }
        if self.done {
            return Err(Error::Invalid("Chat data followed [DONE]"));
        }
        if frame.error_event && chunk.is_object() && chunk["error"].is_null() {
            chunk["type"] = json!("error");
        }
        if !chunk["error"].is_null() || chunk["type"] == "error" {
            return self.upstream_error(&chunk, emit);
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

    fn upstream_error(&mut self, chunk: &Value, emit: &mut Sink<'_>) -> Result<(), Error> {
        let raw = chunk
            .get("error")
            .filter(|error| !error.is_null())
            .unwrap_or(chunk);
        if !raw.is_object() {
            return Err(Error::Invalid("expected a Chat error object"));
        }
        let Some(failed) = Failure::of(chunk, "code") else {
            unreachable!("a Chat error has a nested error or the error event type")
        };
        self.fail(Failed::Provider(failed), provider_error(raw), emit)?;
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
            events.start(start)?;
        }
        Ok(id)
    }

    pub(crate) fn observed(&self) -> (Option<Usage>, Option<Failure>) {
        let failed = self.failed.as_ref().map(|failed| match failed {
            Failed::Provider(failure) | Failed::Cut(failure) => failure.clone(),
        });
        (self.usage, failed)
    }

    fn cut(&mut self, code: &'static str, message: &str, emit: &mut Sink<'_>) -> io::Result<()> {
        self.fail(
            Failed::Cut(Failure::new(code)),
            json!({"code":"server_error","message":message}),
            emit,
        )
    }

    fn fail(&mut self, failed: Failed, error: Value, emit: &mut Sink<'_>) -> io::Result<()> {
        self.failed = Some(failed);
        let mut events = Events {
            sequence: &mut self.sequence,
            emit,
        };
        // An error may precede every Chat chunk. The Responses lifecycle then needs an ID
        // of its own, distinct from an upstream completion.
        let local;
        let (id, created) = if let Some((id, at, _)) = &self.identity {
            (id, *at)
        } else {
            local = local_identity()?;
            let (id, at) = &local;
            events.start(response(
                &self.echo,
                id,
                &self.model,
                *at,
                "in_progress",
                json!([]),
                Value::Null,
            ))?;
            (id, *at)
        };
        let mut body = response(
            &self.echo,
            id,
            &self.model,
            created,
            "failed",
            self.output.snapshot("incomplete"),
            usage(self.usage),
        );
        body["error"] = error;
        events.send("response.failed", json!({"response":body}))
    }

    pub(crate) fn broken(
        mut self,
        mut emit: impl FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<End> {
        if self.failed.is_none() {
            self.cut(
                "incomplete_chat_stream",
                "the model provider's Chat stream broke off",
                &mut emit,
            )?;
        }
        let Some(failed) = self.failed else {
            unreachable!("cut set the stream's ending")
        };
        Ok(failed.end(self.usage, true))
    }

    pub(crate) fn finish(
        mut self,
        mut emit: impl FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<End> {
        if let Some(failed) = self.failed.take() {
            return Ok(failed.end(self.usage, false));
        }
        if !self.done || self.frames.pending() {
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
                self.cut("invalid_chat_stream", message, &mut emit)?;
                let Some(failed) = self.failed else {
                    unreachable!("cut set the stream's ending")
                };
                return Ok(failed.end(self.usage, false));
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
        Ok(End::Forwarded {
            usage: self.usage,
            failed: incomplete.then(|| Failure::new(reason)),
        })
    }
}

fn local_identity() -> io::Result<(String, u64)> {
    let mut random = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    Ok((
        format!("resp_pumpkin_{:032x}", u128::from_be_bytes(random)),
        created.as_secs(),
    ))
}

fn optional<'a>(value: &'a Value, key: &str) -> Result<Option<&'a str>, Error> {
    match &value[key] {
        Value::String(text) => Ok(Some(text)),
        Value::Null => Ok(None),
        _ => Err(Error::Invalid("expected a text field in a Chat chunk")),
    }
}
