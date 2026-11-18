//! Reading a Chat exchange once for the record, meter and Responses client.

use std::io;

use gateway::ModelResponse;
use serde_json::json;
use ureq::http::Response;

use super::{Answer, Client, Closed, Relayed, relay, request_id};
use crate::Keep;
use crate::bridge::stream::{End, Stream};
use crate::bridge::{Request, response};
use crate::usage::{Failure, Usage};

pub(super) fn answer(
    closed: &Closed<'_>,
    request: &Request,
    upstream: Response<ureq::Body>,
    mut headers: Vec<(String, String)>,
    client: Client<'_>,
    answered: impl FnOnce(Answer),
) -> io::Result<()> {
    let status = upstream.status().as_u16();
    let id = request_id(upstream.headers());
    let success = (200..300).contains(&status);
    let stream = success && request.stream;
    headers.retain(|(name, _)| name != "content-type");
    let mime = if stream {
        "text/event-stream"
    } else {
        "application/json"
    };
    headers.push(("content-type".into(), mime.into()));
    let Client {
        request: client,
        by,
        keep,
    } = client;
    let mut scan = closed.scan();
    let mut body = upstream.into_body().into_reader();
    let model = closed.model();
    let upstream_name = closed.upstream_name();
    let report =
        |cut, usage, failed| answered(Answer::upstream(model, status, id, cut, usage, failed));
    if stream {
        let stream = Stream::new(&request.echo, model.as_str(), upstream_name);
        return streamed(
            stream,
            &mut body,
            client.respond(status, headers, by),
            keep,
            report,
        );
    }
    let relayed = relay(&mut body, keep, |bytes| {
        if scan.feed(bytes) {
            keep.first_output();
        }
        Ok(())
    });
    let (usage, _, held) = scan.finish();
    let whole = matches!(relayed, Relayed::Whole);
    let converted = if whole && success {
        held.as_ref().and_then(|chat| {
            response::whole(&request.echo, model.as_str(), upstream_name, chat, usage)
        })
    } else {
        None
    };
    let (client_status, body, cut, failed) = if let Some((body, failed)) = converted {
        (status, body, false, failed)
    } else if success || !whole {
        let message = "the model provider returned an incomplete or unsupported Chat completion";
        (
            502,
            error(message),
            true,
            Some(Failure::new("invalid_chat_completion")),
        )
    } else {
        let mut error = response::provider_error(&held.unwrap_or_default()["error"]);
        error["type"] = json!("upstream_error");
        error["param"] = serde_json::Value::Null;
        (status, json!({"error":error}), false, None)
    };
    let client = client.respond(client_status, headers, by);
    let sent = client.and_then(|mut client| -> io::Result<ModelResponse> {
        client.send(body.to_string().as_bytes())?;
        Ok(client)
    });
    report(cut || sent.is_err(), usage, failed);
    sent?.end()
}

fn error(message: &str) -> serde_json::Value {
    json!({"error":{"message":message,"type":"upstream_error","param":null,"code":null}})
}

fn streamed(
    mut stream: Stream,
    body: &mut impl io::Read,
    client: io::Result<ModelResponse>,
    keep: &dyn Keep,
    report: impl FnOnce(bool, Option<Usage>, Option<Failure>),
) -> io::Result<()> {
    let mut client = match client {
        Ok(client) => client,
        Err(error) => {
            report(true, None, None);
            return Err(error);
        }
    };
    let mut emit = |bytes: &[u8]| client.send(bytes);
    let relayed = relay(body, keep, |bytes| {
        stream.feed(bytes, &mut emit, || keep.first_output())
    });
    let (usage, failed) = stream.observed();
    let terminal = match relayed {
        Relayed::Whole => stream.finish(emit),
        Relayed::BrokeOff => stream.broken(emit),
        Relayed::Unsent(error) => {
            report(true, usage, failed);
            return Err(error);
        }
    };
    match terminal {
        Ok(End::Cut { usage, failed }) => report(true, usage, Some(failed)),
        Ok(End::Forwarded { usage, failed }) => report(false, usage, failed),
        Err(error) => {
            report(true, usage, failed);
            return Err(error);
        }
    }
    // The caller received a complete Responses terminal event, even for an upstream cut.
    client.end()
}
