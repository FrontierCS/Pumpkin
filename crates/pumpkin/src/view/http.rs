//! The view's HTTP, as little of HTTP/1.1 as a browser's page needs: a `GET` of a path and a
//! query, or a `POST` with no body, answered whole, with its length, deflated if the request
//! accepts it and that is shorter, and the connection closed, one at a time.

use std::borrow::{Borrow, Cow};
use std::io::{self, BufRead as _, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use miniz_oxide::deflate::{CompressionLevel, compress_to_vec_zlib};

/// The most bytes of a request's line and headers that are read: a browser's `GET`, with the
/// cookies it keeps for `localhost`, takes a few KiB.
pub(super) const HEAD_MAX_BYTES: u64 = 64 * 1024;

/// The most bytes of a `POST`'s body that are read, and dropped: a cancel's has none.
const BODY_MAX_BYTES: u64 = 1024;

/// How long a connection may take to send its request, from its first byte, and to take its
/// answer, from the first byte written: no other connection is answered meanwhile.
pub(super) const STALL: Duration = Duration::from_secs(10);

/// How long a connection, which holds back no other, may wait to send its first byte before it is
/// closed: as long as nginx's default `client_header_timeout`
/// (<https://github.com/nginx/nginx/blob/d17f1d249d386de1282222172f526973fcd8a702/src/http/ngx_http_core_module.c#L3626-L3627>).
pub(super) const IDLE: Duration = Duration::from_secs(60);

/// The hosts a request may name, each the machine's loopback: a page of another site that has
/// its own name resolve to `127.0.0.1` names its own host, and is refused.
pub(super) const HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "[::1]"];

/// What a page that an answer is may load and run: the view's own scripts, style and font alone,
/// never an inline script, a style attribute or another site's file, and no frame may hold it, so
/// that text of a record a page shows can never run as code.
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
     font-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; \
     frame-ancestors 'none'";

/// The header that carries the view's token ([`Request::token`]).
pub(super) const TOKEN_HEADER: &str = "X-Pumpkin-Token";

/// A request: its method, its path, its query's `name=value` pairs, each value decoded as a form
/// encodes it, and the headers that the view's checks read.
pub(super) struct Request {
    pub(super) method: Method,
    /// Its path and query, as its line gives them.
    pub(super) target: String,
    pub(super) path: String,
    query: Vec<(String, String)>,
    /// The site of the page that sent it, as a browser names it in `Origin`.
    pub(super) origin: Option<String>,
    /// The view's token, as its [`TOKEN_HEADER`] gives it.
    pub(super) token: Option<String>,
    /// Whether its `Accept-Encoding` accepts `deflate` ([`accepts_deflate`]).
    pub(super) deflate: bool,
}

/// The methods that the view takes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Method {
    Get,
    Post,
}

impl Request {
    /// A `GET` of `target`, a path and its query as a request's line gives them, that carries
    /// neither an `Origin` nor the token, and accepts a deflated body if `deflate`; or the refusal
    /// of a query that is no text as a form encodes it.
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

    /// The value of the query's `name`, if it has one.
    pub(super) fn query(&self, name: &str) -> Option<&str> {
        let mut values = self.query.iter().filter(|(named, _)| named == name);
        values.next().map(|(_, value)| value.as_str())
    }

    /// The value of the query's `name` as a whole number, if it has one, or the refusal of a
    /// request whose value is none.
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

/// An answer: its status line's code and reason, its type (with its charset, if text), its body,
/// and whether a browser may keep it ([`Response::immutable`]).
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
    pub(super) fn json(value: &serde_json::Value) -> Self {
        Self {
            status: Cow::Borrowed("200 OK"),
            content_type: Cow::Borrowed("application/json; charset=utf-8"),
            body: Body::Made(value.to_string().into_bytes()),
            immutable: false,
        }
    }

    /// This answer with the status `status`, such as `202 Accepted`.
    pub(super) fn with_status(self, status: &'static str) -> Self {
        Self {
            status: Cow::Borrowed(status),
            ..self
        }
    }

    /// A reply that another view sent to the request that this one relays: its status, such as
    /// `200 OK`, and type, as its head gave them, and its body, deflated if `deflated`.
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

    /// The bytes `body`, of the type `content_type`, as a record keeps them.
    pub(super) fn bytes(content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            status: Cow::Borrowed("200 OK"),
            content_type: Cow::Borrowed(content_type),
            body: Body::Made(body),
            immutable: false,
        }
    }

    /// A file of the page's, `body`, of the type `content_type`; `deflated` is its [`deflated`]
    /// form, if shorter.
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

    /// This answer, as what its path always gives, so that a browser keeps it and uses it again
    /// without asking.
    pub(super) fn immutable(self) -> Self {
        Self {
            immutable: true,
            ..self
        }
    }

    /// A refusal, `status`, with `message`, what is wrong and what is expected, as JSON
    /// `{"error": message}`.
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

/// What an answer sends, its body deflated as the module's doc says if the request accepts it.
pub(super) struct Sent {
    pub(super) status: Cow<'static, str>,
    pub(super) content_type: Cow<'static, str>,
    pub(super) body: Vec<u8>,
    pub(super) deflated: bool,
    /// Whether a browser may keep it ([`Response::immutable`]).
    pub(super) immutable: bool,
}

impl Response {
    /// What this answer sends to a request that accepts `deflate` if `deflate`.
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
/// 8.4.1.2), if that is shorter.
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

/// Reads the request on `stream`: `Ok(Err(response))` for one that is refused, with the refusal.
///
/// # Errors
///
/// Fails if the connection fails, or is still sending at `due`.
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

/// The `name=value` pairs of `query`, each value decoded as a form encodes it; or the refusal of
/// a value that isn't text so encoded.
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

/// The request's method, `method`, and the bytes of its body, as its `Content-Length`, `length`,
/// gives them; or its refusal, for a method that the view doesn't take or a body past
/// [`BODY_MAX_BYTES`].
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

/// `value`, a query's, with each `+` a space and each `%` and two hexadecimal digits the byte
/// they give; `None` unless it decodes so to UTF-8.
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

/// Refuses a request that names `host` unless it is one of [`HOSTS`].
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

/// Writes `response` on `stream`, whole, deflated if `deflate`, which the request accepts, and
/// that is shorter; and closes it.
///
/// # Errors
///
/// Fails if the connection fails, or has yet to take it all [`STALL`] after it is ready to write.
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

/// A connection, owned or borrowed, each read and write of which fails once `due` has passed, so
/// that a client that sends or takes a byte at a time can't hold it past then.
struct Due<S> {
    stream: S,
    due: Instant,
}

impl<S: Borrow<TcpStream>> Due<S> {
    /// The time left until `due`, or the error of a connection that is past it.
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
