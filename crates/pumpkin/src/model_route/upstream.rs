//! Sending a closed model request to the model provider, and streaming its response back.

use std::fs;
use std::io::{self, Read};
use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

use pumpkin_gateway::IncomingModelRequest;
use pumpkin_schema::sandbox::frame;
use stdx::Deadline;
use ureq::http::header::{AUTHORIZATION, CONTENT_ENCODING};
use ureq::http::{HeaderMap, HeaderValue, Uri};

use super::closure::{Closed, Refusal, Status, refusal};

/// The response headers that reach the client, besides the rate-limit headers that
/// [`RATE_LIMIT_SUFFIXES`] gives.
// Each is one that the runs' harness reads from a response: the model that answered, the
// request's ids, its credits and why a limit was reached, when to try again, and an error's
// details. Any other stays behind: one that names the account or the project that the
// deployment's key belongs to, which are not the agent's to know, and those that frame the body
// on the upstream's connection, since the session carries it in frames of its own.
const RESPONSE_HEADERS: [&str; 19] = [
    "content-type",
    "openai-model",
    "x-reasoning-included",
    "x-request-id",
    "x-models-etag",
    "x-codex-turn-state",
    "x-codex-safety-buffering-enabled",
    "x-codex-safety-buffering-faster-model",
    "x-codex-credits-has-credits",
    "x-codex-credits-unlimited",
    "x-codex-credits-balance",
    "x-codex-promo-message",
    "x-codex-rate-limit-reached-type",
    "x-codex-active-limit",
    "x-oai-request-id",
    "cf-ray",
    "x-openai-authorization-error",
    "x-error-json",
    "retry-after",
];

/// The ends of the rate-limit headers that reach the client: `x-<limit><end>`, for any limit.
const RATE_LIMIT_SUFFIXES: [&str; 7] = [
    "-primary-used-percent",
    "-primary-window-minutes",
    "-primary-reset-at",
    "-secondary-used-percent",
    "-secondary-window-minutes",
    "-secondary-reset-at",
    "-limit-name",
];

/// How long the upstream may take to resolve the provider's host name.
// Resolving the name and connecting to it (`CONNECT_TIMEOUT`) are the waits that `IDLE_TIMEOUT`
// doesn't bound, and the harness waits for a response's head without a bound of its own. A lookup
// or a connection this slow has failed: its client hears so, and may try again, rather than wait
// for whatever bound the host's resolver or kernel sets.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the upstream may take to take a connection once the name is resolved, its TLS
/// handshake included.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the upstream may keep the gateway waiting on any one read or write, for its
/// response's head and within its body.
// Within the body, the harness gives up on a stream that stays silent this long, and asks again,
// so a live provider never keeps it waiting longer; waiting on past it, a request would hold one
// of its sandbox's places in flight (`MAX_MODEL_REQUESTS_IN_FLIGHT`) for a client that has gone.
// Before the head, the harness waits without a bound of its own, so this bound and `until` end
// that wait: the harness asks for a stream, whose head the provider sends before any of the
// model's output, so a head this late is a stall as well. A provider that takes none of the
// request for as long has stalled too. A long response that keeps coming is never cut by it.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// The model provider's endpoint: an `https` URL that names no user, or an `http` URL to a
/// loopback IP address. A port, if it names one, is a number up to 65535.
#[derive(Debug)]
pub struct UpstreamUrl(Uri);

/// The API key that the model provider takes, read from its file. It is never shown: it has no
/// `Debug` or `Display`, and goes nowhere but upstream.
pub struct Key(HeaderValue);

/// The model provider's endpoint, with its key, which forwards closed requests.
pub struct Upstream {
    url: Uri,
    authorization: HeaderValue,
    agent: ureq::Agent,
}

impl TryFrom<String> for UpstreamUrl {
    type Error = String;

    // Plain HTTP beyond the host would show the key, and every request, to the network it
    // crosses; on the loopback it never leaves the host. A user in the URL is a credential,
    // which would go wherever the URL is shown, so it is never repeated here either.
    fn try_from(url: String) -> Result<Self, String> {
        let refused = |what: &str| {
            format!(
                "the upstream's URL {what}: name an https URL without a user, or an http URL to \
                 a loopback IP address such as 127.0.0.1"
            )
        };
        let url =
            Uri::try_from(url).map_err(|error| refused(&format!("is not a URL ({error})")))?;
        let (secure, authority) = match (url.scheme_str(), url.authority()) {
            (Some("https"), Some(authority)) => (true, authority),
            (Some("http"), Some(authority)) => (false, authority),
            _ => return Err(refused("is not an http URL")),
        };
        if authority.as_str().contains('@') {
            return Err(refused("names a user"));
        }
        let host = authority.host();
        // The HTTP client would take a port that doesn't parse as none, and reach the scheme's
        // default port instead.
        if authority.as_str() != host && authority.port_u16().is_none() {
            let message = "the upstream's URL names a port that is not a number up to 65535: \
                           name one, or none for the scheme's default";
            return Err(message.to_owned());
        }
        let ip = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        if !secure && !ip.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()) {
            return Err(refused("is plain http to a host beyond the loopback"));
        }
        Ok(Self(url))
    }
}

impl Key {
    /// Reads the key in the file at `path`: one line of printable ASCII, without spaces.
    ///
    /// # Errors
    ///
    /// Fails, without showing what the file holds, if it can't be read or holds no key.
    pub fn read(path: &Path) -> Result<Self, String> {
        let text = fs::read(path).map_err(|error| {
            format!(
                "the key file {} can't be read ({error}): name a file that holds the model \
                 provider's key",
                path.display()
            )
        })?;
        let key = String::from_utf8(text).ok();
        let key = key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty() && key.bytes().all(|byte| byte.is_ascii_graphic()));
        let Some(mut authorization) =
            key.and_then(|key| HeaderValue::try_from(format!("Bearer {key}")).ok())
        else {
            return Err(format!(
                "the key file {} holds no API key: write the key alone, on one line",
                path.display()
            ));
        };
        // The HTTP client's types show a sensitive value as such, never the value.
        authorization.set_sensitive(true);
        Ok(Self(authorization))
    }
}

impl Upstream {
    /// The endpoint at `url`, which takes `key`.
    pub fn new(url: UpstreamUrl, key: Key) -> Self {
        let agent = ureq::Agent::config_builder()
            // The provider's own errors reach the client as the provider gave them.
            .http_status_as_error(false)
            // The route reaches one endpoint: a redirect reaches the client as it is.
            .max_redirects(0)
            // Runs are hermetic, and the key goes to the endpoint alone: otherwise a proxy that
            // the environment names would carry every request, and would take an `http` loopback
            // endpoint's key off the host in plain text.
            .proxy(None)
            // The body reaches the client as it arrives, without the response's
            // `content-encoding`, so it must arrive uncoded.
            .accept_encoding("identity")
            .timeout_resolve(Some(RESOLVE_TIMEOUT))
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_per_read(Some(IDLE_TIMEOUT))
            .timeout_per_write(Some(IDLE_TIMEOUT))
            .build()
            .into();
        let UpstreamUrl(url) = url;
        let Key(authorization) = key;
        Self {
            url,
            authorization,
            agent,
        }
    }

    /// Sends `closed`, the closed form of `request`, to the model provider with the key, and
    /// streams the response back to `request`'s client as it arrives, by `by`
    /// ([`IncomingModelRequest::respond`]): its status and body as they are, with only the
    /// headers that a harness reads from a response. The upstream is given up on at `until`,
    /// or once it stalls (`RESOLVE_TIMEOUT`, `CONNECT_TIMEOUT`, `IDLE_TIMEOUT`). Its client gets
    /// a 504 if the upstream is given up on before the response's head, a 502 if it gives no
    /// answer otherwise or codes the body, and a response cut short if it breaks off, or is
    /// given up on, within the body.
    ///
    /// # Errors
    ///
    /// Fails if the response's head is too long for a frame, which cuts the request short
    /// without a response, if `by` passes first, or if the session has ended, which ends the
    /// exchange.
    pub fn forward(
        &self,
        closed: &Closed,
        request: IncomingModelRequest,
        until: Deadline,
        by: Deadline,
    ) -> io::Result<()> {
        let call = self.agent.post(self.url.clone()).config();
        let call = call.timeout_global(bound(until)).build();
        let call = closed
            .headers()
            .iter()
            .fold(call, |call, (name, value)| call.header(name, value));
        let call = call.header(AUTHORIZATION, &self.authorization);
        let response = match call.send(closed.body()) {
            Ok(response) => response,
            Err(error) => return unanswered(&error).answer(request, by),
        };
        if coded(response.headers()) {
            // The request accepts no coding, and the harness decodes none: a coded body would
            // reach it as bytes it can't read, whether `content-encoding` stayed behind or not.
            let message = "the model provider coded its answer, though the request asked for it \
                           uncoded: try again";
            return refusal(Status::BadGateway, message.to_owned()).answer(request, by);
        }
        let status = response.status().as_u16();
        let headers = returned(response.headers());
        let mut client = request.respond(status, headers, by)?;
        let mut body = response.into_body().into_reader();
        // A read fills at most one frame of the session.
        let mut buffer = vec![0; frame::MAX_BODY_BYTES];
        loop {
            match body.read(&mut buffer) {
                Ok(0) => return client.end(),
                Ok(read) => client.send(&buffer[..read])?,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                // A body that broke off, or was given up on, isn't whole.
                Err(_) => return client.cut(),
            }
        }
    }
}

/// The time that `until` leaves the exchange: none once it has passed, and no bound if it is
/// never reached.
fn bound(until: Deadline) -> Option<Duration> {
    match until.remaining() {
        None => Some(Duration::ZERO),
        Some(Duration::MAX) => None,
        left @ Some(_) => left,
    }
}

/// The refusal of a request that the upstream gave no answer to, for `error`.
// The client hears that the provider gave no answer, but not why: the reason describes the
// host's network, which is not the agent's to know.
fn unanswered(error: &ureq::Error) -> Refusal {
    if matches!(error, ureq::Error::Timeout(_)) {
        let message = "the model provider gave no answer in time: try again";
        refusal(Status::GatewayTimeout, message.to_owned())
    } else {
        let message = "the model provider gave no answer: try again";
        refusal(Status::BadGateway, message.to_owned())
    }
}

/// Whether `headers` name a content coding of the body other than `identity`.
fn coded(headers: &HeaderMap) -> bool {
    headers.get_all(CONTENT_ENCODING).iter().any(|value| {
        value.as_bytes().split(|&byte| byte == b',').any(|coding| {
            let coding = coding.trim_ascii();
            !coding.is_empty() && !coding.eq_ignore_ascii_case(b"identity")
        })
    })
}

/// The response headers that reach the client, those that it reads whose values are text.
fn returned(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| reaches_client(name.as_str()))
        .filter_map(|(name, value)| {
            Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
        })
        .collect()
}

/// Whether the response header `name`, in lowercase, reaches the client: one that
/// [`RESPONSE_HEADERS`] lists, or a rate-limit header.
fn reaches_client(name: &str) -> bool {
    RESPONSE_HEADERS.contains(&name)
        || RATE_LIMIT_SUFFIXES.iter().any(|suffix| {
            name.strip_prefix("x-")
                .and_then(|name| name.strip_suffix(suffix))
                .is_some_and(|limit| !limit.is_empty())
        })
}
