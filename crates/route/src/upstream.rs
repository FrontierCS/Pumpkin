//! Sending a closed model request to the model provider, and streaming its response back.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::net::IpAddr;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;
use std::{fmt, fs};

use gateway::{IncomingModelRequest, ModelResponse};
use schema::ir::{Api, ModelName, Name};
use schema::sandbox::frame;
use stdx::Deadline;
use ureq::http::header::{AUTHORIZATION, CONTENT_ENCODING};
use ureq::http::{HeaderMap, HeaderValue, Uri};

use super::Keep;
use super::closure::{Closed, Refusal, Route, Status, refusal};
use super::usage::{Failure, Scan, Usage};

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
// Before the head, the harness waits without a bound of its own, so this bound and the deadline end
// that wait: the harness asks for a stream, whose head the provider sends before any of the
// model's output, so a head this late is a stall as well. A provider that takes none of the
// request for as long has stalled too. A long response that keeps coming is never cut by it.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// The model provider's endpoint: an `https` URL that names no user, or an `http` URL to a
/// loopback IP address. A port, if it names one, is a number up to 65535.
#[derive(Clone, Debug)]
pub struct UpstreamUrl(Uri);

/// The API key that the model provider takes, read from its file. It is never shown: it has no
/// `Debug` or `Display`, and goes nowhere but upstream.
#[derive(Clone)]
pub struct Key(HeaderValue);

/// The model provider's endpoint, with its key, which forwards closed requests.
pub struct Upstream {
    url: Uri,
    authorization: HeaderValue,
    // One for the upstream's every exchange, so that `shutdown` ends them all at once.
    agent: ureq::Agent,
    // Set before `shutdown` ends the exchanges in flight, so that one it ends before the head is
    // refused for the route's end, not as unanswered, and a later one never goes upstream.
    shut_down: OnceLock<()>,
    // Set once the provider refuses the key with a 401, which no later request can mend: each
    // carries the same key, read once for the run. Exchanges run on threads of their own, which
    // share the upstream, so the first to hear the refusal sets it for the others to see.
    refused: OnceLock<()>,
}

/// How the route answered a model request: never with what the request or the response held.
///
/// An upstream's response comes with the provider's id for the request, from its `x-request-id`
/// or else `x-oai-request-id`, if it gave one, which an operator names to ask the provider about
/// the request, the usage that its body reported, if it did ([`Usage`]), and the failure that it
/// stated, if it did ([`Failure`]).
#[derive(Debug)]
pub enum Answer {
    /// With the upstream's response, whole, which had `status`.
    Forwarded {
        status: u16,
        request_id: Option<String>,
        usage: Option<Usage>,
        failed: Option<Failure>,
    },
    /// With the upstream's response, which had `status`, cut short: it broke off, was given up
    /// on, or the session stopped taking it.
    Cut {
        status: u16,
        request_id: Option<String>,
        usage: Option<Usage>,
        failed: Option<Failure>,
    },
    /// With a refusal of the route's own, with `status` and `message`, before the request
    /// reached the upstream: the route's rules refused it, or the route had ended.
    Barred { status: u16, message: String },
    /// With a refusal of the route's own, with `status` and `message`, in place of the
    /// upstream's response with the status `upstream_status`, if one came.
    Refused {
        upstream_status: Option<u16>,
        request_id: Option<String>,
        status: u16,
        message: String,
    },
}

/// How a run serves a model route's entry for one API: the upstream that its requests reach, of
/// which the run makes an [`Upstream`] for each sandbox that it serves.
pub struct Serving {
    /// The name that the deployment gives the upstream.
    pub name: Name,
    pub api: Api,
    /// The upstream's endpoint for `api`.
    pub url: UpstreamUrl,
    pub key: Key,
    /// The name that the upstream takes for each model it serves, by the name that a route
    /// allows.
    pub models: BTreeMap<ModelName, ModelName>,
}

impl fmt::Debug for Serving {
    // The key shows nothing.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            name,
            api,
            url,
            key: _,
            models,
        } = self;
        (f.debug_struct("Serving")
            .field("name", name)
            .field("api", api))
        .field("url", url)
        .field("models", models)
        .finish_non_exhaustive()
    }
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
    /// Reads the key in the file at `path`: one line of printable ASCII, without spaces. A file
    /// that holds no such line fails, without showing what it holds.
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
    /// The endpoint at `url`, which takes `key`, with connections of its own.
    pub fn new(url: &UpstreamUrl, key: &Key) -> Self {
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
        let (UpstreamUrl(url), Key(authorization)) = (url, key);
        Self {
            url: url.clone(),
            authorization: authorization.clone(),
            agent,
            shut_down: OnceLock::new(),
            refused: OnceLock::new(),
        }
    }

    /// Sends `closed` upstream and streams the response back to `client` by its deadline, with
    /// only the headers a harness reads, keeping both bodies ([`Keep`]), giving up at it or
    /// once the upstream stalls. The client hears 504 if it gives up before the head, the route's
    /// end if shut down by then ([`Refusal::ended`]), 502 for no answer or a coded body, 400 if
    /// the key or the request is refused, and else the response, cut short if it breaks off.
    /// Fails if the head is too long for a frame or the deadline passes.
    pub(crate) fn forward(
        &self,
        closed: &Closed,
        client: Client<'_>,
        answered: impl FnOnce(Answer),
    ) -> io::Result<()> {
        client
            .keep
            .request(closed.body(), Some(closed.conversation()));
        let call = self.agent.post(self.url.clone()).config();
        let call = call.timeout_global(bound(client.by)).build();
        let call = closed
            .headers()
            .iter()
            .fold(call, |call, (name, value)| call.header(name, value));
        let call = call.header(AUTHORIZATION, &self.authorization);
        let response = match call.send(closed.body()) {
            Ok(response) => response,
            Err(error) => {
                let refusal = if self.was_shut_down() {
                    Refusal::ended()
                } else {
                    unanswered(&error)
                };
                return refuse(refusal, (None, None), client, answered);
            }
        };
        let status = response.status().as_u16();
        let request_id = request_id(response.headers());
        if let Some(refusal) = self.refusal(status, response.headers()) {
            let mut body = response.into_body().into_reader();
            return replaced(refusal, (status, request_id), &mut body, client, answered);
        }
        let headers = returned(response.headers(), closed);
        let Client { request, by, keep } = client;
        let mut client = match request.respond(status, headers, by) {
            Ok(client) => client,
            Err(error) => {
                answered(Answer::Cut {
                    status,
                    request_id,
                    usage: None,
                    failed: None,
                });
                return Err(error);
            }
        };
        let content_type = (response.headers().get("content-type")).and_then(|v| v.to_str().ok());
        let mut scan = Scan::new(content_type, closed.usage());
        let mut body = response.into_body().into_reader();
        let relayed = relay(&mut body, &mut client, &mut scan, keep);
        let (usage, failed) = scan.finish();
        answered(match relayed {
            Relayed::Whole => Answer::Forwarded {
                status,
                request_id,
                usage,
                failed,
            },
            Relayed::BrokeOff | Relayed::Unsent(_) => Answer::Cut {
                status,
                request_id,
                usage,
                failed,
            },
        });
        match relayed {
            Relayed::Whole => client.end(),
            Relayed::BrokeOff => client.cut(),
            Relayed::Unsent(error) => Err(error),
        }
    }

    /// The refusal that answers in place of the upstream's response with `status` and
    /// `headers`, if one does: a 401, which refuses the key, and which the upstream keeps, a 403,
    /// or a coded body.
    fn refusal(&self, status: u16, headers: &HeaderMap) -> Option<Refusal> {
        // Passed on, either would be a status that a harness tries again until its deadline. A
        // 401 refuses the deployment's key, which every later request carries; a 403 may turn on
        // what the request asked for, which the agent chose, so it ends no route.
        if status == 401 {
            let _ = self.refused.set(());
            Some(Refusal::key_refused())
        } else if status == 403 {
            Some(Refusal::forbidden())
        } else if coded(headers) {
            // The request accepts no coding, and the harness decodes none: a coded body would
            // reach it as bytes it can't read, whether `content-encoding` stayed behind or not.
            let message = "the model provider coded its answer, though the request asked for it \
                           uncoded: try again";
            Some(refusal(Status::BadGateway, message.to_owned()))
        } else {
            None
        }
    }

    fn serve(
        &self,
        route: &Route,
        client: Client<'_>,
        allowed: bool,
        forwarding: impl FnOnce(),
        answered: impl FnOnce(Answer),
    ) {
        let taken = if !allowed || self.was_shut_down() {
            Err(Refusal::ended())
        } else if self.key_was_refused() {
            Err(Refusal::key_refused())
        } else {
            route.close(&client.request.request)
        };
        let _ = match taken {
            Ok(closed) => {
                forwarding();
                self.forward(&closed, client, answered)
            }
            Err(refusal) => barred(refusal, client, answered),
        };
    }

    /// Whether the model provider has refused the key with a 401, which ends the route.
    pub fn key_was_refused(&self) -> bool {
        self.refused.get().is_some()
    }

    /// Gives up on every exchange in flight and every later one, which ends the route: before
    /// the response's head, its client gets the refusal for the route's end (`Refusal::ended`),
    /// as does a later request; within the body, it gets the response cut short. One still
    /// resolving the provider's name, or connecting to it, is given up on once that ends
    /// (`RESOLVE_TIMEOUT`, `CONNECT_TIMEOUT`).
    pub fn shutdown(&self) {
        let _ = self.shut_down.set(());
        // The agent's clones share its pool, whose shutdown closes every connection that any of
        // them reads or writes.
        self.agent.clone().shutdown();
    }

    pub fn was_shut_down(&self) -> bool {
        self.shut_down.get().is_some()
    }
}

/// Answers `request` for the sandbox that made it, by `by`, through the entry of `entries`, each
/// a route's entry for one API and its upstream, whose endpoint it names.
///
/// One that names none is refused with a 404 that names each. The entry's upstream refuses it
/// unless `allowed`, or once it has shut down ([`Upstream::shutdown`]) or its key was refused,
/// and else forwards it closed, giving up at `by` too. It calls `forwarding` first, gives `keep`
/// the exchange ([`Keep`]), and tells `answered` how it answered ([`Answer`]) and through which
/// entry's index, once `keep` has the whole answer and before the answer's end is written.
// Whether the answer reached the sandbox isn't told: writing it fails only for a response's head
// too long for a frame, or as the sandbox's session ends, which the run log shows.
pub fn serve(
    entries: &[(Route, Upstream)],
    request: IncomingModelRequest,
    allowed: bool,
    by: Deadline,
    keep: &dyn Keep,
    forwarding: impl FnOnce(),
    answered: impl FnOnce(Option<usize>, Answer),
) {
    let taken = entries
        .iter()
        .position(|(route, _)| route.takes(&request.request));
    let client = Client { request, by, keep };
    let Some(index) = taken else {
        let path = &client.request.request.path;
        let refusal = Refusal::unserved(path, entries.iter().map(|(route, _)| route));
        let _ = barred(refusal, client, |answer| answered(None, answer));
        return;
    };
    let (route, upstream) = &entries[index];
    let answered = |answer| answered(Some(index), answer);
    upstream.serve(route, client, allowed, forwarding, answered);
}

pub(crate) struct Client<'k> {
    request: IncomingModelRequest,
    by: Deadline,
    keep: &'k dyn Keep,
}

/// Answers `client`'s request with `refusal`, before it reached the upstream, once the request as
/// its client sent it and the refusal are kept, and `answered` is told so.
fn barred(refusal: Refusal, client: Client<'_>, answered: impl FnOnce(Answer)) -> io::Result<()> {
    let Client { request, by, keep } = client;
    keep.request(&request.request.body, None);
    keep.response(refusal.body().as_bytes());
    answered(refusal.barred());
    refusal.answer(request, by)
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

/// Answers `client`'s request with `refusal`, in place of the upstream's response with the
/// status and request id `upstream`, if one came, once the refusal is kept and `answered` is
/// told so.
fn refuse(
    refusal: Refusal,
    (upstream_status, request_id): (Option<u16>, Option<String>),
    client: Client<'_>,
    answered: impl FnOnce(Answer),
) -> io::Result<()> {
    let Client { request, by, keep } = client;
    keep.response(refusal.body().as_bytes());
    answered(refusal.answered(upstream_status, request_id));
    refusal.answer(request, by)
}

/// Answers `client`'s request with `refusal` in place of the upstream's response with the status
/// and request id `upstream`, once `body`, the upstream's, is kept as it came, so that the record
/// says what the provider answered, and `answered` is told.
fn replaced(
    refusal: Refusal,
    (status, request_id): (u16, Option<String>),
    body: &mut impl Read,
    client: Client<'_>,
    answered: impl FnOnce(Answer),
) -> io::Result<()> {
    let Client { request, by, keep } = client;
    let mut buffer = vec![0; frame::MAX_BODY_BYTES];
    // A body that breaks off, or is given up on, is kept as far as it came.
    while let Ok(read @ 1..) = body.read(&mut buffer) {
        keep.response(&buffer[..read]);
    }
    answered(refusal.answered(Some(status), request_id));
    refusal.answer(request, by)
}

enum Relayed {
    Whole,
    /// Until it broke off, or was given up on.
    BrokeOff,
    /// Until sending it failed so: the deadline passed, or the session ended.
    Unsent(io::Error),
}

/// Streams `body` to `client` as it arrives, showing it to `scan`, until it ends, without writing
/// its end, and returns how it went.
fn relay(
    body: &mut impl Read,
    client: &mut ModelResponse,
    scan: &mut Scan,
    keep: &dyn Keep,
) -> Relayed {
    // A read fills at most one frame of the session.
    let mut buffer = vec![0; frame::MAX_BODY_BYTES];
    loop {
        match body.read(&mut buffer) {
            Ok(0) => return Relayed::Whole,
            Ok(read) => {
                scan.feed(&buffer[..read]);
                keep.response(&buffer[..read]);
                if let Err(error) = client.send(&buffer[..read]) {
                    return Relayed::Unsent(error);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Relayed::BrokeOff,
        }
    }
}

/// The provider's id for the request that `headers` answer, if it gave one: `x-request-id`, or
/// else `x-oai-request-id`, as Codex keeps them for an error and Inspect for a model call.
fn request_id(headers: &HeaderMap) -> Option<String> {
    ["x-request-id", "x-oai-request-id"]
        .into_iter()
        .find_map(|name| headers.get(name)?.to_str().ok())
        .map(str::to_owned)
}

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

/// The response headers that reach the client, those that it reads whose values are text, with
/// the model that answered named as `closed`'s client named it ([`Closed::named`]).
fn returned(headers: &HeaderMap, closed: &Closed) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| reaches_client(name.as_str()))
        .filter_map(|(name, value)| {
            let value = value.to_str().ok()?;
            let value = if name == "openai-model" {
                closed.named(value)
            } else {
                value
            };
            Some((name.as_str().to_owned(), value.to_owned()))
        })
        .collect()
}

/// Whether `name`, a response header in lowercase, reaches the client.
fn reaches_client(name: &str) -> bool {
    RESPONSE_HEADERS.contains(&name)
        || RATE_LIMIT_SUFFIXES.iter().any(|suffix| {
            name.strip_prefix("x-")
                .and_then(|name| name.strip_suffix(suffix))
                .is_some_and(|limit| !limit.is_empty())
        })
}
