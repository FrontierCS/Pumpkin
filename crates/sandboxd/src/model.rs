//! The model endpoint: an HTTP/1.1 server on the sandbox's loopback, at [`MODEL_PORT`], through
//! which a harness in the sandbox reaches its role's model route. Each request goes over the
//! session whole, as a model request, by [the protocol's rules](schema::sandbox), with
//! its method, its target and every header, names in lowercase, and its response comes back as
//! it arrives.
//!
//! It serves one request per connection, and answers these itself, so they never reach the
//! session, whose bounds they would break:
//!
//! - a head over [`MAX_HEAD_BYTES`], with 431, or one malformed, with 400;
//! - HTTP/1.0, whose clients don't read a chunked response, with 426, which names HTTP/1.1 to
//!   send instead, and another major version, with 505, while a later minor version of HTTP/1
//!   is served as HTTP/1.1;
//! - a chunked body, with 411: a body's length is its `content-length`, 0 without one;
//! - a body over [`MAX_MODEL_REQUEST_BYTES`], with 413;
//! - a request while [`MAX_MODEL_REQUESTS_IN_FLIGHT`] are in flight, with 503.
//!
//! It writes a response as the session's owner sends it: the status and headers, but those that
//! frame a message, which are its own, then the body chunked (`transfer-encoding: chunked`), then
//! the last chunk, and closes the connection. A response cut short, ended without a head, or with a
//! status of 1xx, which is interim, ends with the connection, before any last chunk, so that its
//! client doesn't take it as whole. A response to `HEAD`, or with a status of 204 or 304, has no
//! body: its head alone goes out, once the response ends whole, and its data is dropped. A refusal
//! to `HEAD` is likewise its head alone. To close, it stops writing, then reads what the client
//! still sends until the client closes too: the client gets the whole response even when it sent
//! more than `sandboxd` read.
//!
//! As with [ops](crate::ops), every client is the sandbox's own code, which `sandboxd` doesn't
//! guard against: a client that connects and sends nothing, reads nothing of its response, or
//! never closes, holds a thread, with no timeout, and the parts of a response it hasn't read yet.
//! That harms only the sandbox's own run.

use std::collections::BTreeMap;
use std::fmt::{Display, Write as _};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender as PartSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use schema::sandbox::frame::{MAX_BODY_BYTES, MAX_MESSAGE_BYTES};
use schema::sandbox::{
    FromSandbox, MAX_MODEL_REQUEST_BYTES, MAX_MODEL_REQUESTS_IN_FLIGHT, MODEL_PORT,
};
use stdx::{Context, unpoisoned};

use crate::session::Sender;

/// The longest request head `sandboxd` reads: the request line and the headers. As JSON, a head
/// grows to at most ten thirds of its size, from its shortest header line, `":` and a line feed,
/// to `["\"",""],`, so a request's opening stays within a frame's message.
const MAX_HEAD_BYTES: usize = 16 * 1024;

const _: () = assert!(MAX_HEAD_BYTES * 10 / 3 + 1024 <= MAX_MESSAGE_BYTES);

/// The names of the response fields that frame a message or concern its connection, which
/// `sandboxd` writes itself, so it drops any the session's owner sends.
const FRAMING: [&str; 5] = [
    "content-length",
    "transfer-encoding",
    "connection",
    "trailer",
    "upgrade",
];

/// The model requests in flight, which the threads carrying them and the session, which
/// receives their responses, share.
#[derive(Clone, Default)]
pub(crate) struct Model(Arc<Mutex<Flight>>);

#[derive(Default)]
struct Flight {
    /// The number the next request takes, unless one in flight has it.
    next: u32,
    /// Where the parts of each response go, by request: to its client's thread, which may have
    /// gone. They queue without a bound, so that a client slow to read never holds up the
    /// session, which reads every other request's frames too.
    waiting: BTreeMap<u32, PartSender<Part>>,
}

/// A part of a response, from the gateway.
pub(crate) enum Part {
    Head {
        status: u16,
        headers: Vec<(String, String)>,
    },
    Data(Vec<u8>),
    /// The end: whole, or `cut` short.
    End {
        cut: bool,
    },
}

/// A request read whole from a client.
struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Why a request can't be carried: its status, and what to tell the client.
struct Refusal(u16, String);

/// What [`relay`] holds, or has written, of a response.
enum Relayed {
    Nothing,
    /// The status and headers of a response without a body, held until it ends whole, then
    /// written alone; its data is dropped.
    // Nothing after the head could show its client that the response was cut short.
    Bodiless(u16, Vec<(String, String)>),
    /// The head of a chunked response, and any chunks after it.
    Chunked,
}

impl Model {
    /// Serves clients on the loopback, at [`MODEL_PORT`], on a thread of its own. The port is
    /// bound before this returns, so before the session serves its first request, and so before
    /// any of the role's commands can take it.
    pub(crate) fn serve(&self, sender: Sender) -> Result<(), String> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, MODEL_PORT))
            .context(format_args!("127.0.0.1:{MODEL_PORT}"))?;
        let model = self.clone();
        // Accepts clients while the session's thread serves the gateway.
        thread::Builder::new()
            .name("model".to_owned())
            .spawn(move || model.accept(&listener, &sender))
            .context("the thread that accepts clients didn't start")?;
        Ok(())
    }

    fn accept(&self, listener: &TcpListener, sender: &Sender) {
        for connection in listener.incoming() {
            let Ok(connection) = connection else {
                // Out of descriptors, say: wait for clients to finish rather than spin.
                thread::sleep(Duration::from_millis(10));
                continue;
            };
            let (model, sender) = (self.clone(), sender.clone());
            // A client that can't get a thread sees its connection close.
            let _ = thread::Builder::new()
                .name("model-request".to_owned())
                .spawn(move || model.carry(connection, &sender));
        }
    }

    /// Hands a part of the response to request `request` to its client. The end takes the
    /// request out of flight.
    pub(crate) fn part(&self, request: u32, part: Part) -> io::Result<()> {
        let mut flight = self.lock();
        let client = match part {
            Part::End { .. } => flight.waiting.remove(&request),
            Part::Head { .. } | Part::Data(_) => flight.waiting.get(&request).cloned(),
        };
        drop(flight);
        let Some(client) = client else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "the gateway broke the protocol: it answered model request {request}, which \
                     is not in flight"
                ),
            ));
        };
        // The client's thread may have gone, leaving nobody to tell.
        let _ = client.send(part);
        Ok(())
    }

    /// Carries the request a client sends on `connection`, and writes its response.
    fn carry(&self, mut connection: TcpStream, sender: &Sender) {
        let request = match read(&mut BufReader::new(&connection)) {
            Ok(Ok(request)) => request,
            Ok(Err((refusal, to_head))) => return refuse(&mut connection, refusal, to_head),
            // The client went away, or sent something unreadable.
            Err(_) => return,
        };
        let to_head = request.method == "HEAD";
        // Opened only once read whole, since an open request can't be withdrawn, and gives its
        // body's size first.
        let (number, parts) = match self.open() {
            Ok(opened) => opened,
            Err(refusal) => return refuse(&mut connection, refusal, to_head),
        };
        // A failed send means the session has failed, and is ending.
        if send(sender, number, request).is_err() {
            self.lock().waiting.remove(&number);
            return;
        }
        // A response streams in small chunks, which Nagle's algorithm would hold back.
        let _ = connection.set_nodelay(true);
        relay(&parts, &mut connection, to_head);
        close(&mut connection);
    }

    /// Puts a new request in flight, and returns its number and where its response will
    /// arrive.
    fn open(&self) -> Result<(u32, Receiver<Part>), Refusal> {
        let mut flight = self.lock();
        if flight.waiting.len() >= MAX_MODEL_REQUESTS_IN_FLIGHT {
            return Err(Refusal(
                503,
                format!(
                    "{MAX_MODEL_REQUESTS_IN_FLIGHT} model requests are in flight already: wait \
                     for one to finish"
                ),
            ));
        }
        // A request held in flight while the numbers wrap around keeps its own.
        let mut number = flight.next;
        while flight.waiting.contains_key(&number) {
            number = number.wrapping_add(1);
        }
        flight.next = number.wrapping_add(1);
        let (client, parts) = mpsc::channel();
        flight.waiting.insert(number, client);
        Ok((number, parts))
    }

    fn lock(&self) -> MutexGuard<'_, Flight> {
        unpoisoned(self.0.lock())
    }
}

/// Reads a request whole, or why it can't be carried, with whether it is of `HEAD`: false for a
/// head refused before its method is parsed.
fn read(reader: &mut impl BufRead) -> io::Result<Result<Request, (Refusal, bool)>> {
    let mut head = Vec::new();
    let mut lines = reader.take(MAX_HEAD_BYTES as u64);
    loop {
        let start = head.len();
        lines.read_until(b'\n', &mut head)?;
        let line = &head[start..];
        // A line without its end, or none at all, ends where the client or the bound stopped it.
        if !line.ends_with(b"\n") {
            if lines.limit() > 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let message = format!(
                "the request's head holds more than {MAX_HEAD_BYTES} bytes: send at most that"
            );
            return Ok(Err((Refusal(431, message), false)));
        }
        if matches!(line, b"\r\n" | b"\n") {
            break;
        }
    }
    let reader = lines.into_inner();
    let (method, target, headers) = match parse_head(&head) {
        Ok(parsed) => parsed,
        Err(refused) => return Ok(Err(refused)),
    };
    let length = match body_length(&headers) {
        Ok(length) => length,
        Err(refusal) => return Ok(Err((refusal, method == "HEAD"))),
    };
    let mut body = vec![0; usize::try_from(length).map_err(io::Error::other)?];
    reader.read_exact(&mut body)?;
    Ok(Ok(Request {
        method,
        target,
        headers,
        body,
    }))
}

type Head = (String, String, Vec<(String, String)>);

/// The method, the target and the headers, names in lowercase, of a head of HTTP/1.1, or of a
/// later minor version, that ends in an empty line; or why not, with whether the head is of
/// `HEAD`, as [`read`] gives it. A head holds no control characters but tabs, so that its JSON
/// escapes no character with more than two bytes.
fn parse_head(head: &[u8]) -> Result<Head, (Refusal, bool)> {
    // A head that isn't text, or opens with no request line, is refused before its method is
    // parsed.
    let malformed = |message: String| (Refusal(400, message), false);
    let head = std::str::from_utf8(head)
        .ok()
        .filter(|head| !head.contains(|c: char| c.is_control() && !"\t\r\n".contains(c)))
        .ok_or("the request's head is not text: send UTF-8 without control characters")
        .map_err(|message| malformed(message.to_owned()))?;
    let mut lines = head.lines();
    let line = lines.next().unwrap_or_default();
    let not_a_request_line = || {
        malformed(format!(
            "`{}` is not a request line: send `<method> <target> HTTP/1.1`",
            line.escape_debug()
        ))
    };
    let [method, target, version] = line.split(' ').collect::<Vec<_>>()[..] else {
        return Err(not_a_request_line());
    };
    let Some(&[major, b'.', minor]) = version.strip_prefix("HTTP/").map(str::as_bytes) else {
        return Err(not_a_request_line());
    };
    if method.is_empty() || target.is_empty() || !major.is_ascii_digit() || !minor.is_ascii_digit()
    {
        return Err(not_a_request_line());
    }
    let to_head = method == "HEAD";
    let unserved = |status| {
        let message = format!("the request's version is {version}: send HTTP/1.1");
        (Refusal(status, message), to_head)
    };
    match (major, minor) {
        // A later minor version is compatible with 1.1, the latest this server implements, so
        // it is served as 1.1.
        (b'1', b'1'..=b'9') => {}
        // A client of HTTP/1.0 doesn't read a chunked response, as most responses here are, so
        // it is told the version to send instead.
        (b'1', _) => return Err(unserved(426)),
        _ => return Err(unserved(505)),
    }
    let mut headers = Vec::new();
    for line in lines.take_while(|line| !line.is_empty()) {
        let Some((name, value)) = line
            .split_once(':')
            .filter(|(name, _)| !name.is_empty() && !name.contains(char::is_whitespace))
        else {
            let message = format!(
                "`{}` is not a header: send `<name>: <value>`",
                line.escape_debug()
            );
            return Err((Refusal(400, message), to_head));
        };
        headers.push((
            name.to_ascii_lowercase(),
            value.trim_matches([' ', '\t']).to_owned(),
        ));
    }
    Ok((method.to_owned(), target.to_owned(), headers))
}

/// The length of the body that `headers` announce: 0 without a `content-length`.
fn body_length(headers: &[(String, String)]) -> Result<u64, Refusal> {
    if headers.iter().any(|(name, _)| name == "transfer-encoding") {
        let message = "a chunked body is refused: send its content-length instead";
        return Err(Refusal(411, message.to_owned()));
    }
    // Digits alone, as a length is written: parsing alone would take a sign too.
    let mut lengths = headers
        .iter()
        .filter(|(name, _)| name == "content-length")
        .map(|(_, value)| {
            value
                .bytes()
                .all(|byte| byte.is_ascii_digit())
                .then(|| value.parse::<u64>().ok())
                .flatten()
        });
    let length = match lengths.next() {
        None => 0,
        Some(Some(length)) if lengths.all(|other| other == Some(length)) => length,
        Some(_) => {
            let message = "the content-length is not one number: send one";
            return Err(Refusal(400, message.to_owned()));
        }
    };
    if length > MAX_MODEL_REQUEST_BYTES {
        let message = format!("the body is {length} bytes: send at most {MAX_MODEL_REQUEST_BYTES}");
        return Err(Refusal(413, message));
    }
    Ok(length)
}

/// Sends request `number`: its opening, with its body's size, then the body's bytes.
fn send(sender: &Sender, number: u32, request: Request) -> io::Result<()> {
    let Request {
        method,
        target,
        headers,
        body,
    } = request;
    let opening = FromSandbox::ModelRequest {
        request: number,
        method,
        path: target,
        headers,
        size: body.len() as u64,
    };
    sender.send(&opening, &[])?;
    for chunk in body.chunks(MAX_BODY_BYTES) {
        sender.send(&FromSandbox::ModelData { request: number }, chunk)?;
    }
    Ok(())
}

/// Writes the parts of a response to the client as they arrive, until its end, the client's
/// going, or the session's end. `to_head` says the request's method is `HEAD`, whose response,
/// as some statuses' do, goes without a body ([`Relayed::Bodiless`]).
fn relay(parts: &Receiver<Part>, connection: &mut TcpStream, to_head: bool) {
    let mut relayed = Relayed::Nothing;
    for part in parts {
        let written = match (part, &relayed) {
            // A request gets one head, so no final response can follow an interim one: it ends
            // with the connection, with nothing written.
            (
                Part::Head {
                    status: 100..=199, ..
                },
                Relayed::Nothing,
            ) => return,
            (
                Part::Head {
                    status,
                    mut headers,
                },
                Relayed::Nothing,
            ) => {
                headers.retain(|(name, _)| {
                    !FRAMING
                        .iter()
                        .any(|framing| name.eq_ignore_ascii_case(framing))
                });
                if to_head || matches!(status, 204 | 304) {
                    relayed = Relayed::Bodiless(status, headers);
                    Ok(())
                } else {
                    relayed = Relayed::Chunked;
                    headers.push(("transfer-encoding".to_owned(), "chunked".to_owned()));
                    write_head(connection, status, &headers)
                }
            }
            (Part::Data(_), Relayed::Bodiless(..)) => Ok(()),
            // An empty chunk would end the body.
            (Part::Data(bytes), Relayed::Chunked) if bytes.is_empty() => Ok(()),
            (Part::Data(bytes), Relayed::Chunked) => {
                let mut chunk = format!("{:x}\r\n", bytes.len()).into_bytes();
                chunk.extend_from_slice(&bytes);
                chunk.extend_from_slice(b"\r\n");
                connection.write_all(&chunk)
            }
            (Part::End { cut: false }, Relayed::Bodiless(status, headers)) => {
                let _ = write_head(connection, *status, headers);
                return;
            }
            (Part::End { cut: false }, Relayed::Chunked) => {
                let _ = connection.write_all(b"0\r\n\r\n");
                return;
            }
            // A response cut short, or ended without a head, ends with the connection, and so
            // does one out of order, which the gateway never sends.
            (Part::Data(_) | Part::End { .. }, Relayed::Nothing)
            | (
                Part::End { cut: true } | Part::Head { .. },
                Relayed::Bodiless(..) | Relayed::Chunked,
            ) => {
                return;
            }
        };
        if written.is_err() {
            return;
        }
    }
}

/// Answers a request that can't be carried, with a JSON error, and closes the connection.
/// `to_head` says the request's method is `HEAD`, whose response has no body: it gets the head
/// alone.
fn refuse(connection: &mut TcpStream, Refusal(status, message): Refusal, to_head: bool) {
    let body = serde_json::json!({ "error": { "message": message } }).to_string();
    let length = body.len().to_string();
    let mut headers = vec![("content-type", "application/json")];
    if status == 426 {
        // A 426 names the version to send instead, in a field that the connection's options
        // must then keep to this connection.
        headers.extend([("upgrade", "HTTP/1.1"), ("connection", "upgrade")]);
    }
    // To `HEAD`, it must be the length of the body that the same request of `GET` would get,
    // which it is: no message refusing a request whose method is parsed names the method.
    headers.push(("content-length", &length));
    let body = if to_head { "" } else { &body };
    // A client that has gone has nobody to tell.
    let _ = write_head(connection, status, &headers)
        .and_then(|()| connection.write_all(body.as_bytes()));
    close(connection);
}

/// Closes `connection` once its response is written: stops writing, then reads what the client
/// still sends, until the client closes too.
fn close(connection: &mut TcpStream) {
    // A client that has gone has closed already.
    let _ = connection.shutdown(Shutdown::Write);
    // A close with bytes unread resets the connection, and the reset discards what of the
    // response is still queued to send, as the end of one larger than the client's buffers.
    let _ = io::copy(connection, &mut io::sink());
}

/// Writes a response's status line and `headers`, then `connection: close`.
fn write_head(
    connection: &mut TcpStream,
    status: u16,
    headers: &[(impl Display, impl Display)],
) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    for (name, value) in headers {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    head.push_str("connection: close\r\n\r\n");
    connection.write_all(head.as_bytes())
}

/// A status's reason phrase, which clients ignore; empty for the rarer ones.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Content Too Large",
        426 => "Upgrade Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        505 => "HTTP Version Not Supported",
        _ => "",
    }
}
