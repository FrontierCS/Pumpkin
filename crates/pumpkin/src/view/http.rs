//! The view's HTTP, as little of HTTP/1.1 as a browser's page needs: a `GET` of a path and a query,
//! or a `POST` with no body, answered whole, with its length, deflated if the request accepts it
//! and that is shorter, and the connection closed, one at a time.

use std::borrow::{Borrow, Cow};
use std::io::{self, BufRead as _, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use miniz_oxide::deflate::{CompressionLevel, compress_to_vec_zlib};

/// Most bytes of a request's head that `read` takes in; a longer head is refused. A browser's
/// `GET`, with the cookies it keeps for `localhost`, takes a few KiB.
pub(super) const HEAD_MAX_BYTES: u64 = 64 * 1024;

/// Most bytes of a request's body that `read` takes in and drops; the view's only `POST` has none.
const BODY_MAX_BYTES: u64 = 1024;

/// How long a connection may take to send its request, from its first byte, and to take its
/// answer, from the first byte written: no other connection is answered meanwhile.
pub(super) const STALL: Duration = Duration::from_secs(10);

/// How long an accepted connection, which holds back no other, may send nothing before the server
/// drops it: as long as nginx's default `client_header_timeout`
/// (<https://github.com/nginx/nginx/blob/d17f1d249d386de1282222172f526973fcd8a702/src/http/ngx_http_core_module.c#L3626-L3627>).
pub(super) const IDLE: Duration = Duration::from_secs(60);

/// The `Host` names of this machine's loopback, the only ones a request may name: a page of
/// another site that has its own name resolve to `127.0.0.1` names its own host, and is refused.
pub(super) const HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "[::1]"];

/// Every response's content security policy: a page may load and run only the view's own scripts,
/// style and font, never an inline script, a style attribute or another site's file, and no
/// frame may hold it, so that text of a record that a page shows can never run as code.
pub(super) const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
     font-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; \
     frame-ancestors 'none'";

/// The request header that carries the view's token.
pub(super) const TOKEN_HEADER: &str = "X-Pumpkin-Token";

/// A request read by `read`, or a `GET` built by `Request::get`.
pub(super) struct Request {
    pub(super) method: Method,
    pub(super) target: String,
    pub(super) path: String,
    query: Vec<(String, String)>,
    pub(super) origin: Option<String>,
    pub(super) token: Option<String>,
    pub(super) deflate: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Method {
    Get,
    Post,
}

impl Request {
    /// A `GET` of `target`, a path with an optional query, naming no origin or token.
    ///
    /// Refuses a query that no form encodes.
    pub(super) fn get(target: &str, deflate: bool) -> Result<Self, Response> {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        Ok(Self {
            method: Method::Get,
            target: target.to_owned(),
            path: path.to_owned(),
            query: pairs(query)?,
            origin: None,
            token: None,
            deflate,
        })
    }

    /// The first value of the query's `name`, percent-decoded.
    pub(super) fn query(&self, name: &str) -> Option<&str> {
        let mut values = self.query.iter().filter(|(named, _)| named == name);
        values.next().map(|(_, value)| value.as_str())
    }

    /// The query's `name` as a whole number, or `None` where it is absent.
    ///
    /// Refuses a value that is no whole number.
    pub(super) fn number(&self, name: &str) -> Result<Option<u64>, Response> {
        let Some(value) = self.query(name) else {
            return Ok(None);
        };
        value.parse().map(Some).map_err(|_| {
            Response::refused(
                "400 Bad Request",
                &format!("`{name}` is `{value}`: expected a whole number"),
            )
        })
    }
}

/// What the server answers: a status, a body and how the body is encoded.
pub(super) struct Response {
    status: Cow<'static, str>,
    content_type: Cow<'static, str>,
    body: Body,
    immutable: bool,
}

/// An answer's body.
enum Body {
    /// Made for one request, so deflated as it is written, at the fastest level.
    Made(Vec<u8>),
    /// Kept for every request, so deflated once beforehand: `deflated` is that form, if shorter.
    Held {
        plain: Vec<u8>,
        deflated: Option<Vec<u8>>,
    },
    /// As another view sent it, deflated if `deflated`, which the request accepted.
    Relayed { body: Vec<u8>, deflated: bool },
}

impl Response {
    /// A `200 OK` with `value` as its body.
    pub(super) fn json(value: &serde_json::Value) -> Self {
        Self {
            status: Cow::Borrowed("200 OK"),
            content_type: Cow::Borrowed("application/json; charset=utf-8"),
            body: Body::Made(value.to_string().into_bytes()),
            immutable: false,
        }
    }

    /// This response under another status.
    pub(super) fn with_status(self, status: &'static str) -> Self {
        Self {
            status: Cow::Borrowed(status),
            ..self
        }
    }

    /// An upstream view's response, passed on with the status, type and encoding it came with.
    pub(super) fn relayed(
        status: String,
        content_type: String,
        body: Vec<u8>,
        deflated: bool,
    ) -> Self {
        Self {
            status: Cow::Owned(status),
            content_type: Cow::Owned(content_type),
            body: Body::Relayed { body, deflated },
            immutable: false,
        }
    }

    /// A `200 OK` with `body`, of `content_type`.
    pub(super) fn bytes(content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            status: Cow::Borrowed("200 OK"),
            content_type: Cow::Borrowed(content_type),
            body: Body::Made(body),
            immutable: false,
        }
    }

    /// A `200 OK` for a file whose compressed form, if smaller, is made once by the caller.
    pub(super) fn file(
        content_type: &'static str,
        body: Vec<u8>,
        deflated: Option<Vec<u8>>,
    ) -> Self {
        Self {
            status: Cow::Borrowed("200 OK"),
            content_type: Cow::Borrowed(content_type),
            body: Body::Held {
                plain: body,
                deflated,
            },
            immutable: false,
        }
    }

    /// This response marked as one that never changes, which a browser may keep.
    pub(super) fn immutable(self) -> Self {
        Self {
            immutable: true,
            ..self
        }
    }

    /// A response of `status` whose JSON body names `message`, for a request that can't be served.
    pub(super) fn refused(status: &'static str, message: &str) -> Self {
        let body = serde_json::json!({ "error": message }).to_string();
        Self {
            status: Cow::Borrowed(status),
            content_type: Cow::Borrowed("application/json; charset=utf-8"),
            body: Body::Made(body.into_bytes()),
            immutable: false,
        }
    }
}

pub(super) struct Sent {
    pub(super) status: Cow<'static, str>,
    pub(super) content_type: Cow<'static, str>,
    pub(super) body: Vec<u8>,
    pub(super) deflated: bool,
    pub(super) immutable: bool,
}

impl Response {
    pub(super) fn sent(self, deflate: bool) -> Sent {
        let Self {
            status,
            content_type,
            body,
            immutable,
        } = self;
        let either = |plain, deflated: Option<Vec<u8>>| match deflated {
            Some(deflated) => (deflated, true),
            None => (plain, false),
        };
        let (body, deflated) = match body {
            Body::Made(plain) => {
                let deflated = deflate.then(|| deflated(&plain, CompressionLevel::BestSpeed));
                either(plain, deflated.flatten())
            }
            Body::Held { plain, deflated } => either(plain, deflated.filter(|_| deflate)),
            Body::Relayed { body, deflated } => (body, deflated),
        };
        Sent {
            status,
            content_type,
            body,
            deflated,
            immutable,
        }
    }
}

/// `body` deflated at `level`, in zlib's format, which HTTP's `deflate` names (RFC 9110, section
/// 8.4.1.2), or `None` where that is no shorter.
pub(super) fn deflated(body: &[u8], level: CompressionLevel) -> Option<Vec<u8>> {
    let deflated = compress_to_vec_zlib(body, level as u8);
    (deflated.len() < body.len()).then_some(deflated)
}

/// Whether `codings`, a request's `Accept-Encoding`, names `deflate` with a weight other than 0
/// (RFC 9110, section 12.5.3).
fn accepts_deflate(codings: &str) -> bool {
    codings.split(',').any(|coding| {
        let mut parts = coding.split(';').map(str::trim);
        let named = parts
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case("deflate"));
        let refused =
            |part: &str| part.strip_prefix("q=").and_then(|q| q.parse().ok()) == Some(0.0);
        named && !parts.any(refused)
    })
}

/// Reads one request from `stream` before `due`.
///
/// The inner `Err` is the refusal to answer with: a head that is cut short or too long, a method
/// other than `GET` or `POST`, a body over `BODY_MAX_BYTES`, a `Host` that isn't this machine's
/// loopback, or a query that is no form encoding. The body, if any, is read and dropped.
///
/// # Errors
///
/// The connection's failure, or a read that passes `due`.
pub(super) fn read(stream: &TcpStream, due: Instant) -> io::Result<Result<Request, Response>> {
    let mut head = BufReader::new(Due { stream, due }.take(HEAD_MAX_BYTES));
    let mut line = String::new();
    head.read_line(&mut line)?;
    let (mut host, mut origin, mut token, mut length) = (None, None, None, None);
    let mut deflate = false;
    loop {
        let mut header = String::new();
        if head.read_line(&mut header)? == 0 {
            return Ok(Err(Response::refused(
                "400 Bad Request",
                "the request's head is cut short, or longer than 64 KiB: expected a browser's \
                 request",
            )));
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            let value = Some(value.trim().to_owned());
            if name.eq_ignore_ascii_case("host") {
                host = value;
            } else if name.eq_ignore_ascii_case("origin") {
                origin = value;
            } else if name.eq_ignore_ascii_case("content-length") {
                length = value;
            } else if name.eq_ignore_ascii_case(TOKEN_HEADER) {
                token = value;
            } else if name.eq_ignore_ascii_case("accept-encoding") {
                deflate = value.as_deref().is_some_and(accepts_deflate);
            }
        }
    }
    let mut words = line.split_whitespace();
    let (Some(method), Some(target)) = (words.next(), words.next()) else {
        return Ok(Err(Response::refused(
            "400 Bad Request",
            "the request has no method and path: expected `GET <path> HTTP/1.1`",
        )));
    };
    let (method, body_bytes) = match parsed(method, length.as_deref()) {
        Ok(parsed) => parsed,
        Err(refused) => return Ok(Err(refused)),
    };
    // Read, so that closing the connection doesn't reset it before the answer is taken.
    head.get_mut().set_limit(body_bytes);
    io::copy(&mut head.take(body_bytes), &mut io::sink())?;
    if let Err(refused) = on_loopback(host.as_deref()) {
        return Ok(Err(refused));
    }
    let request = match Request::get(target, deflate) {
        Ok(request) => request,
        Err(refused) => return Ok(Err(refused)),
    };
    Ok(Ok(Request {
        method,
        origin,
        token,
        ..request
    }))
}

fn pairs(query: &str) -> Result<Vec<(String, String)>, Response> {
    let mut pairs = Vec::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        let Some(value) = decoded(value) else {
            return Err(Response::refused(
                "400 Bad Request",
                &format!(
                    "the query's `{name}` is `{value}`, which is no text as a form encodes it: \
                     expected `%` before two hexadecimal digits, of UTF-8"
                ),
            ));
        };
        pairs.push((name.to_owned(), value));
    }
    Ok(pairs)
}

fn parsed(method: &str, length: Option<&str>) -> Result<(Method, u64), Response> {
    let method = match method {
        "GET" => Method::Get,
        "POST" => Method::Post,
        _ => {
            return Err(Response::refused(
                "405 Method Not Allowed",
                &format!(
                    "the method is `{method}`: the view is read by `GET`, and cancels a run by \
                     `POST`"
                ),
            ));
        }
    };
    match length.map(str::parse::<u64>) {
        None => Ok((method, 0)),
        Some(Ok(bytes)) if bytes <= BODY_MAX_BYTES => Ok((method, bytes)),
        Some(_) => Err(Response::refused(
            "413 Content Too Large",
            &format!(
                "the request's `Content-Length` is `{}`: expected a body of at most \
                 {BODY_MAX_BYTES} bytes",
                length.unwrap_or_default(),
            ),
        )),
    }
}

fn decoded(value: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut rest = value.as_bytes();
    while let [byte, after @ ..] = rest {
        rest = after;
        match byte {
            b'+' => bytes.push(b' '),
            b'%' => {
                let digits = rest
                    .get(..2)
                    .filter(|digits| digits.iter().all(u8::is_ascii_hexdigit))?;
                let digits = std::str::from_utf8(digits).ok()?;
                bytes.push(u8::from_str_radix(digits, 16).ok()?);
                rest = &rest[2..];
            }
            _ => bytes.push(*byte),
        }
    }
    String::from_utf8(bytes).ok()
}

fn on_loopback(host: Option<&str>) -> Result<(), Response> {
    let name = host.map(|host| match host.rsplit_once(':') {
        Some((name, port)) if !port.contains(']') => name,
        _ => host,
    });
    if name.is_some_and(|name| HOSTS.contains(&name)) {
        return Ok(());
    }
    let named = match host {
        Some(host) => format!("names the host `{host}`"),
        None => "names no host".to_owned(),
    };
    Err(Response::refused(
        "403 Forbidden",
        &format!(
            "the request {named}: expected one of {}, this machine's loopback, at which the view \
             is served",
            HOSTS.join(", ")
        ),
    ))
}

/// Writes `response` to `stream`, compressed if `deflate` and the response has a smaller form,
/// within `STALL`, and closes the connection.
pub(super) fn write(stream: TcpStream, response: Response, deflate: bool) -> io::Result<()> {
    let Sent {
        status,
        content_type,
        body,
        deflated,
        immutable,
    } = response.sent(deflate);
    let encoding = if deflated {
        "Content-Encoding: deflate\r\n"
    } else {
        ""
    };
    // The deadline starts here: deflating is part of making the answer.
    let mut stream = Due {
        stream,
        due: Instant::now() + STALL,
    };
    // A 401 must name how to authenticate (RFC 9110, section 15.5.2): by the token's header.
    let challenge = if status.starts_with("401 ") {
        format!("WWW-Authenticate: {TOKEN_HEADER}\r\n")
    } else {
        String::new()
    };
    // A live record is read again and again, so nothing else is kept to be shown stale. What is
    // immutable is kept a year and never asked for again (RFC 8246), as jupyter_server keeps a
    // file whose path names its version:
    // https://github.com/jupyter-server/jupyter_server/blob/0d1a13b2f5212a9f9da0122a9e7dbaf23f6cfcb7/jupyter_server/base/handlers.py#L998-L1012
    let cache = if immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-store"
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\n{encoding}Vary: Accept-Encoding\r\nCache-Control: {cache}\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Content-Security-Policy: {CONTENT_SECURITY_POLICY}\r\n{challenge}Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&body)?;
    stream.flush()
}

/// A stream whose every read and write is cut off at `due`, so that a peer that sends or takes
/// bytes slowly can't hold the server past it.
struct Due<S> {
    stream: S,
    due: Instant,
}

impl<S: Borrow<TcpStream>> Due<S> {
    fn left(&self) -> io::Result<Duration> {
        let left = self.due.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "the connection took longer than {STALL:?} to send its request, or to take its answer"
                ),
            ));
        }
        Ok(left)
    }
}

impl<S: Borrow<TcpStream>> Read for Due<S> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let mut stream = self.stream.borrow();
        stream.set_read_timeout(Some(self.left()?))?;
        stream.read(bytes)
    }
}

impl<S: Borrow<TcpStream>> Write for Due<S> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut stream = self.stream.borrow();
        stream.set_write_timeout(Some(self.left()?))?;
        stream.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.borrow().flush()
    }
}
