//! The view's HTTP, as little of HTTP/1.1 as a browser's page needs: a `GET` of a path and a
//! query, or a `POST` with no body, answered whole, with its length, and the connection closed,
//! one at a time.

use std::borrow::Borrow;
use std::io::{self, BufRead as _, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// The most bytes of a request's line and headers that are read: a browser's `GET`, with the
/// cookies it keeps for `localhost`, takes a few KiB.
const HEAD_MAX_BYTES: u64 = 64 * 1024;

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
    pub(super) path: String,
    query: Vec<(String, String)>,
    /// The site of the page that sent it, as a browser names it in `Origin`.
    pub(super) origin: Option<String>,
    /// The view's token, as its [`TOKEN_HEADER`] gives it.
    pub(super) token: Option<String>,
}

/// The methods that the view takes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Method {
    Get,
    Post,
}

impl Request {
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

/// An answer: its status line's code and reason, its type (with its charset, if text), and its
/// body.
pub(super) struct Response {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    pub(super) fn json(value: &serde_json::Value) -> Self {
        Self {
            status: "200 OK",
            content_type: "application/json; charset=utf-8",
            body: value.to_string().into_bytes(),
        }
    }

    /// This answer with the status `status`, such as `202 Accepted`.
    pub(super) fn with_status(self, status: &'static str) -> Self {
        Self { status, ..self }
    }

    /// The bytes `body`, of the type `content_type`, as a record keeps them.
    pub(super) fn bytes(content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            status: "200 OK",
            content_type,
            body,
        }
    }

    /// A file of the page's, of the type `content_type`.
    pub(super) fn file(content_type: &'static str, body: &[u8]) -> Self {
        Self {
            status: "200 OK",
            content_type,
            body: body.to_vec(),
        }
    }

    /// A refusal, `status`, with `message`, what is wrong and what is expected, as JSON
    /// `{"error": message}`.
    pub(super) fn refused(status: &'static str, message: &str) -> Self {
        let body = serde_json::json!({ "error": message }).to_string();
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body: body.into_bytes(),
        }
    }
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
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let pairs = match pairs(query) {
        Ok(pairs) => pairs,
        Err(refused) => return Ok(Err(refused)),
    };
    Ok(Ok(Request {
        method,
        path: path.to_owned(),
        query: pairs,
        origin,
        token,
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

/// Writes `response` on `stream`, whole, and closes it.
///
/// # Errors
///
/// Fails if the connection fails, or has yet to take it all at `due`.
pub(super) fn write(stream: TcpStream, response: &Response, due: Instant) -> io::Result<()> {
    let mut stream = Due { stream, due };
    let Response {
        status,
        content_type,
        body,
    } = response;
    // A 401 must name how to authenticate (RFC 9110, section 15.5.2): by the token's header.
    let challenge = if status.starts_with("401 ") {
        format!("WWW-Authenticate: {TOKEN_HEADER}\r\n")
    } else {
        String::new()
    };
    // A live record is read again and again, so nothing is kept to be shown stale.
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         Content-Security-Policy: {CONTENT_SECURITY_POLICY}\r\n{challenge}Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
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
