//! Sending a closed model request to the model provider, and streaming its response back.

use std::collections::BTreeMap;
use std::io::{self, Cursor, Read};
use std::net::IpAddr;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;
use std::{fmt, fs};

use gateway::{IncomingModelRequest, ModelRequest, ModelResponse};
use schema::frame;
use schema::ir::{Api, ModelBudget, ModelName, Name};
use serde_json::{Map, Value};
use stdx::time::{self, Clock};
use ureq::http::header::{AUTHORIZATION, CONTENT_ENCODING};
use ureq::http::{HeaderMap, HeaderName, HeaderValue, Uri};

use super::Keep;
use super::closure::{Closed, Headers, Refusal, Route, Status, parsed, refusal};
use super::usage::{Failure, Scan, Usage};

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

/// How much of a 400's body the upstream reads before its client hears the head, of an API whose
/// 400 may refuse the key.
// A refusal of the key is one short JSON error. A body past this refuses none, and is relayed.
const MAX_KEY_REFUSAL_BYTES: u64 = 16 * 1024;

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
    authorization: (HeaderName, HeaderValue),
    // One for the upstream's every exchange, so that `shutdown` ends them all at once.
    agent: ureq::Agent,
    // Set before `shutdown` ends the exchanges in flight, so that one it ends before the head is
    // refused for the route's end, not as unanswered, and a later one never goes upstream.
    shut_down: OnceLock<()>,
    // Set once the provider refuses the key, which no later request can mend: each carries the
    // same key, read once for the run. Exchanges run on threads of their own, which share the
    // upstream, so the first to hear the refusal sets it for the others to see.
    refused: OnceLock<()>,
    // Whether a 400's body refuses the key, of an API that may refuse one so rather than with a
    // 401.
    refuses_key: Option<fn(&[u8]) -> bool>,
}

/// How the route answered a model request: never with what the request or the response held.
///
/// An upstream's response comes with the provider's id for the request, if it gave one, which an
/// operator names to ask the provider about the request, the usage that its body reported, if it
/// did ([`Usage`]), and the failure that it stated, if it did ([`Failure`]). A request that the
/// route closed comes with whether it asked for its answer as a `stream`.
#[derive(Debug)]
pub enum Answer {
    /// With the upstream's response, whole, which had `status`, from `model`, as the route
    /// allows it.
    Forwarded {
        model: ModelName,
        stream: bool,
        status: u16,
        request_id: Option<String>,
        usage: Option<Usage>,
        failed: Option<Failure>,
    },
    /// With the upstream's response, which had `status`, from `model`, as the route allows it,
    /// cut short: it broke off, was given up on, or the session stopped taking it.
    Cut {
        model: ModelName,
        stream: bool,
        status: u16,
        request_id: Option<String>,
        usage: Option<Usage>,
        failed: Option<Failure>,
    },
    /// With a refusal of the route's own, with `status` and `message`, before the request
    /// reached the upstream, for the reason `bar`.
    Barred {
        bar: Bar,
        status: u16,
        message: String,
    },
    /// With a refusal of the route's own, with `status` and `message`, in place of the
    /// upstream's response for `model` with the status `upstream_status`, if one came.
    Refused {
        model: ModelName,
        stream: bool,
        upstream_status: Option<u16>,
        request_id: Option<String>,
        status: u16,
        message: String,
    },
    /// With the route's end, after the request for `model` was handed to the upstream's
    /// connection, which may have sent it whole, and before the response's head came.
    Abandoned { model: ModelName, stream: bool },
}

/// Why the route barred a request before it reached the upstream.
#[derive(Debug)]
pub enum Bar {
    /// The route's rules refused it: its endpoint, its model or its body.
    Rules,
    /// The route had ended, or the model provider had refused its key, which ends it.
    Ended,
    /// The model budget of the entry that took it was spent ([`Admission::Spent`]).
    Budget,
}

/// How a run serves a model route's entry for one API: the upstream that its requests reach.
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
    /// The headers that the entry passes besides its API's own.
    pub headers: Headers,
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
            headers,
        } = self;
        (f.debug_struct("Serving")
            .field("name", name)
            .field("api", api))
        .field("url", url)
        .field("models", models)
        .field("headers", headers)
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
        let Some(mut key) = key.and_then(|key| HeaderValue::try_from(key).ok()) else {
            return Err(format!(
                "the key file {} holds no API key: write the key alone, on one line",
                path.display()
            ));
        };
        // The HTTP client's types show a sensitive value as such, never the value.
        key.set_sensitive(true);
        Ok(Self(key))
    }
}

impl Upstream {
    /// The endpoint of `api` at `url`, which takes `key` in the header that `api` names, with
    /// connections of its own.
    pub fn new(api: Api, url: &UpstreamUrl, key: &Key) -> Self {
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
        let (UpstreamUrl(url), Key(key)) = (url, key);
        let mut authorization = match api {
            Api::Responses | Api::Chat => {
                let key = key.to_str().unwrap_or_else(|_| {
                    unreachable!("a key read is printable ASCII, which is a header's value")
                });
                let bearer = HeaderValue::try_from(format!("Bearer {key}")).unwrap_or_else(|_| {
                    unreachable!("printable ASCII after `Bearer ` is a header's value")
                });
                (AUTHORIZATION, bearer)
            }
            Api::Messages => (HeaderName::from_static("x-api-key"), key.clone()),
            Api::GenerateContent => (HeaderName::from_static("x-goog-api-key"), key.clone()),
        };
        authorization.1.set_sensitive(true);
        let refuses_key: Option<fn(&[u8]) -> bool> = match api {
            Api::Responses | Api::Chat | Api::Messages => None,
            Api::GenerateContent => Some(generate_content_refuses_key),
        };
        Self {
            url: url.clone(),
            authorization,
            agent,
            shut_down: OnceLock::new(),
            refused: OnceLock::new(),
            refuses_key,
        }
    }

    /// Sends `closed` upstream and streams the response back to `client` by its deadline, with
    /// only the headers its entry passes back, keeping both bodies ([`Keep`]), giving up at it or
    /// once the upstream stalls. The client hears 504 if it gives up before the head, the route's
    /// end if shut down by then ([`Refusal::ended`]), 502 for no answer or a coded body, 400 if
    /// the key or the request is refused, and else the response, cut short if it breaks off.
    /// It calls `forwarding` just before it sends the request. Fails if the head is too long for
    /// a frame or the deadline passes.
    pub(crate) fn forward(
        &self,
        closed: &Closed<'_>,
        client: Client<'_, impl Clock>,
        forwarding: impl FnOnce(),
        answered: impl FnOnce(Answer),
    ) -> io::Result<()> {
        client.keep.dropped_headers(closed.dropped_headers());
        client.keep.request(closed.body(), Some(closed.apart()));
        let call = match self.url(closed) {
            Ok(url) => self.agent.post(url).config(),
            Err(refusal) => {
                let answer = refusal.answered(closed, None, None);
                return refuse(refusal, answer, closed, client, answered);
            }
        };
        let call = call.timeout_global(bound(client.by)).build();
        let call =
            (closed.headers().iter()).fold(call, |call, (name, value)| call.header(name, value));
        let (name, key) = &self.authorization;
        let call = call.header(name, key);
        forwarding();
        let response = match call.send(closed.body()) {
            Ok(response) => response,
            Err(error) => {
                let (refusal, answer) = self.without_head(&error, closed);
                return refuse(refusal, answer, closed, client, answered);
            }
        };
        let status = response.status().as_u16();
        let request_id = request_id(response.headers());
        let (head, body) = response.into_parts();
        let (key_refused, mut body) = key_refusal(self.refuses_key, status, body.into_reader());
        if let Some(refusal) = self.refusal(status, key_refused, &head.headers) {
            let upstream = (status, request_id);
            return replaced(refusal, upstream, closed, &mut body, client, answered);
        }
        let headers = returned(&head.headers, closed);
        let Client { request, by, keep } = client;
        let mut client = match request.respond(status, headers, by) {
            Ok(client) => client,
            Err(error) => {
                answered(told(closed, false, (status, request_id), (None, None)));
                return Err(error);
            }
        };
        let mut scan = closed.scan();
        let relayed = relay(&mut body, &mut client, &mut scan, keep);
        let whole = matches!(relayed, Relayed::Whole);
        answered(told(closed, whole, (status, request_id), scan.finish()));
        match relayed {
            Relayed::Whole => client.end(),
            Relayed::BrokeOff => client.cut(),
            Relayed::Unsent(error) => Err(error),
        }
    }

    /// The refusal that answers `closed` once sending it failed with `error`, before the
    /// response's head, and the answer told for it: the route's end if the route has shut down,
    /// which abandoned the request, and else no answer from the provider.
    fn without_head(&self, error: &ureq::Error, closed: &Closed<'_>) -> (Refusal, Answer) {
        if self.was_shut_down() {
            let (model, stream) = (closed.model().clone(), closed.stream());
            (Refusal::ended(), Answer::Abandoned { model, stream })
        } else {
            let refusal = unanswered(error);
            let answer = refusal.answered(closed, None, None);
            (refusal, answer)
        }
    }

    /// The refusal that answers in place of the upstream's response with `status` and
    /// `headers`, if one does: a 401 or a body that refuses the key (`key_refused`), which the
    /// upstream keeps, a 403, or a coded body.
    fn refusal(&self, status: u16, key_refused: bool, headers: &HeaderMap) -> Option<Refusal> {
        // Passed on, either would be a status that a harness tries again until its deadline. A
        // refusal of the key refuses the deployment's key, which every later request carries; a
        // 403 may turn on what the request asked for, which the agent chose, so it ends no route.
        if status == 401 || key_refused {
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

    fn close<'r>(
        &self,
        route: &'r Route,
        request: &ModelRequest,
        body: Result<Map<String, Value>, Refusal>,
    ) -> Result<Closed<'r>, (Bar, Refusal)> {
        if self.was_shut_down() {
            Err((Bar::Ended, Refusal::ended()))
        } else if self.key_was_refused() {
            Err((Bar::Ended, Refusal::key_refused()))
        } else {
            (route.close(request, body)).map_err(|refusal| (Bar::Rules, refusal))
        }
    }

    fn serve(
        &self,
        route: &Route,
        client: Client<'_, impl Clock>,
        body: Result<Map<String, Value>, Refusal>,
        admission: Admission<'_>,
        forwarding: impl FnOnce(),
        answered: impl FnOnce(Answer),
    ) {
        let taken = match admission {
            Admission::Ended => Err((Bar::Ended, Refusal::ended())),
            Admission::Open => self.close(route, &client.request.request, body),
            Admission::Spent { .. } if self.was_shut_down() => Err((Bar::Ended, Refusal::ended())),
            Admission::Spent { budget } => Err((Bar::Budget, Refusal::spent(budget, route))),
        };
        let _ = match taken {
            Ok(closed) => self.forward(&closed, client, forwarding, answered),
            Err((bar, refusal)) => barred((bar, refusal), route.api(), client, answered),
        };
    }

    /// Whether the model provider has refused the key, which ends the route.
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

    /// The URL that `closed` goes to: the endpoint's, with the path and the query that `closed`
    /// gives it.
    fn url(&self, closed: &Closed<'_>) -> Result<Uri, Refusal> {
        let (path, query) = (closed.path(), closed.query());
        if path.is_none() && query.is_none() {
            return Ok(self.url.clone());
        }
        let path = path.unwrap_or_default();
        let query = query.map(|query| format!("?{query}")).unwrap_or_default();
        // A `?` or a `#` in the path, which holds the model's name as the upstream takes it,
        // would end the path early.
        let url = (!path.contains(['?', '#']))
            .then(|| format!("{}{path}{query}", self.url))
            .ok_or_else(|| "the model's name holds `?` or `#`".to_owned())
            .and_then(|url| Uri::try_from(url).map_err(|error| error.to_string()));
        url.map_err(|error| {
            let message = format!(
                "the upstream's URL for the request is invalid ({error}): check the configured \
                 upstream URL and the model's name"
            );
            refusal(Status::BadGateway, message)
        })
    }

    pub fn was_shut_down(&self) -> bool {
        self.shut_down.get().is_some()
    }
}

/// Whether the caller still takes a role's model requests through an entry of its route.
#[derive(Clone, Copy, Debug)]
pub enum Admission<'b> {
    Open,
    /// The caller has stopped taking them: the route's end.
    Ended,
    /// The entry's `budget` is spent ([`ModelBudget`]).
    Spent {
        budget: &'b ModelBudget,
    },
}

/// Answers `request` for its sandbox by `by`, a time on `clock`, through an entry of `entries`.
///
/// Each is a route's entry and its upstream. The one whose endpoint the request names takes it,
/// and of several, the one that allows the model it names; else it is refused with a 404 or a 400
/// that names what they serve. The entry's upstream refuses it unless `admission` of the entry's
/// index is open, or once it has shut down ([`Upstream::shutdown`]) or its key was refused, and
/// else forwards it closed, giving up at `by` too, calling `forwarding` just before it sends it.
/// It gives `keep` the exchange ([`Keep`]), and tells `answered` how it answered ([`Answer`]) and
/// through which entry's index, once `keep` has the whole answer and before its end is written.
// Whether the answer reached the sandbox isn't told: writing it fails only for a response's head
// too long for a frame, or as the sandbox's session ends.
pub fn serve<'b, C: Clock>(
    entries: &[(Route, Upstream)],
    request: IncomingModelRequest,
    admission: impl FnOnce(usize) -> Admission<'b>,
    (clock, by): (&C, Duration),
    keep: &dyn Keep,
    forwarding: impl FnOnce(),
    answered: impl FnOnce(Option<usize>, Answer),
) {
    let client = Client {
        request,
        by: (clock, by),
        keep,
    };
    let request = &client.request.request;
    let taking = || (entries.iter().enumerate()).filter(|(_, (route, _))| route.takes(request));
    let Some((first, _)) = taking().next() else {
        let routes = || entries.iter().map(|(route, _)| route);
        let refusal = Refusal::unserved(&request.path, routes());
        // In the form of the entries' API, if they are of one, so that a client of that API
        // alone, which may try its other endpoints, reads it.
        let api = match entries.split_first() {
            Some(((first, _), rest))
                if rest.iter().all(|(route, _)| route.api() == first.api()) =>
            {
                first.api()
            }
            Some(_) | None => Api::Responses,
        };
        let _ = barred((Bar::Rules, refusal), api, client, |answer| {
            answered(None, answer);
        });
        return;
    };
    let body = parsed(&request.body);
    let index = if taking().nth(1).is_none() {
        first
    } else {
        let named = entries[first].0.named(request, body.as_ref().ok());
        let allows = |route: &Route| named.is_some_and(|named| route.allows(named).is_some());
        let Some((index, _)) = taking().find(|(_, (route, _))| allows(route)) else {
            let allowed = taking().flat_map(|(_, (route, _))| route.models());
            // A target that names no method, or a body that is no JSON object, names no model:
            // say what is wrong with it instead.
            let refusal = match entries[first].0.addressed(&request.path) {
                Err(refusal) => refusal,
                Ok(_) => match body {
                    Err(refusal) => refusal,
                    Ok(_) => Refusal::unallowed(named, allowed),
                },
            };
            let api = entries[first].0.api();
            let _ = barred((Bar::Rules, refusal), api, client, |answer| {
                answered(None, answer);
            });
            return;
        };
        index
    };
    let (route, upstream) = &entries[index];
    let answered = |answer| answered(Some(index), answer);
    upstream.serve(route, client, body, admission(index), forwarding, answered);
}

pub(crate) struct Client<'k, C> {
    request: IncomingModelRequest,
    // The deadline, as a time on the clock.
    by: (&'k C, Duration),
    keep: &'k dyn Keep,
}

/// Answers `client`'s request, of `api`, with `refusal`, which bars it for `bar` before it reached
/// the upstream, once the request as its client sent it and the refusal are kept, and `answered`
/// is told so.
fn barred(
    (bar, refusal): (Bar, Refusal),
    api: Api,
    client: Client<'_, impl Clock>,
    answered: impl FnOnce(Answer),
) -> io::Result<()> {
    let Client { request, by, keep } = client;
    keep.request(&request.request.body, None);
    keep.response(refusal.body(api).as_bytes());
    answered(refusal.barred(bar));
    refusal.answer(api, request, by)
}

/// The time that `until`, a time on its clock, leaves the exchange: none once it has passed, and
/// no bound if it is never reached.
fn bound((clock, until): (&impl Clock, Duration)) -> Option<Duration> {
    match time::left(clock, until) {
        None => Some(Duration::ZERO),
        Some(Duration::MAX) => None,
        left @ Some(_) => left,
    }
}

/// Answers `client`'s request with `refusal`, which came in place of any response of the
/// upstream's, once the refusal is kept and `answered` is told `answer`.
fn refuse(
    refusal: Refusal,
    answer: Answer,
    closed: &Closed<'_>,
    client: Client<'_, impl Clock>,
    answered: impl FnOnce(Answer),
) -> io::Result<()> {
    let Client { request, by, keep } = client;
    let api = closed.route().api();
    keep.response(refusal.body(api).as_bytes());
    answered(answer);
    refusal.answer(api, request, by)
}

/// Answers `client`'s request with `refusal` in place of the upstream's response with the status
/// and request id `upstream`, once `body`, the upstream's, is kept as it came, so that the record
/// says what the provider answered, and `answered` is told.
fn replaced(
    refusal: Refusal,
    (status, request_id): (u16, Option<String>),
    closed: &Closed<'_>,
    body: &mut impl Read,
    client: Client<'_, impl Clock>,
    answered: impl FnOnce(Answer),
) -> io::Result<()> {
    let Client { request, by, keep } = client;
    let mut buffer = vec![0; frame::MAX_BODY_BYTES];
    // A body that breaks off, or is given up on, is kept as far as it came.
    while let Ok(read @ 1..) = body.read(&mut buffer) {
        keep.response(&buffer[..read]);
    }
    answered(refusal.answered(closed, Some(status), request_id));
    refusal.answer(closed.route().api(), request, by)
}

/// Whether `body`, a response's with `status`, refuses the key by `refuses_key`, which judges a
/// 400's, and `body` as it came. It reads what it judges before the client hears the head, which
/// a refusal of the key replaces.
fn key_refusal(
    refuses_key: Option<fn(&[u8]) -> bool>,
    status: u16,
    mut body: impl Read,
) -> (bool, impl Read) {
    let refuses_key = refuses_key.filter(|_| status == 400);
    let mut opening = Vec::new();
    let mut broke = None;
    if refuses_key.is_some() {
        let read = (&mut body)
            .take(MAX_KEY_REFUSAL_BYTES)
            .read_to_end(&mut opening);
        broke = read.err();
    }
    let refused = refuses_key.is_some_and(|refuses| broke.is_none() && refuses(&opening));
    (
        refused,
        Cursor::new(opening).chain(Broken(broke)).chain(body),
    )
}

/// Whether `body`, a response's with a 400 from the Gemini API, refuses the key.
// The Gemini API answers a key that it doesn't take with a 400, as
// https://ai.google.dev/gemini-api/docs/troubleshooting has it, rather than with a 401. It names
// why in an `ErrorInfo`, whose reason and domain identify the error: https://google.aip.dev/193
fn generate_content_refuses_key(body: &[u8]) -> bool {
    let Ok(body) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let details = body.pointer("/error/details").and_then(Value::as_array);
    details.into_iter().flatten().any(|detail| {
        let field = |name: &str| detail.get(name).and_then(Value::as_str);
        field("@type") == Some("type.googleapis.com/google.rpc.ErrorInfo")
            && field("reason") == Some("API_KEY_INVALID")
            && field("domain") == Some("googleapis.com")
    })
}

/// A reader that fails once with the error, if any, that cut a body's read short, and then ends.
struct Broken(Option<io::Error>);

impl Read for Broken {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        self.0.take().map_or(Ok(0), Err)
    }
}

/// The answer that the upstream's response to `closed` gave, with its `status` and `request_id`
/// and the usage and failure that it reported: `whole`, or cut short.
fn told(
    closed: &Closed<'_>,
    whole: bool,
    (status, request_id): (u16, Option<String>),
    (usage, failed): (Option<Usage>, Option<Failure>),
) -> Answer {
    let (model, stream) = (closed.model().clone(), closed.stream());
    if whole {
        Answer::Forwarded {
            model,
            stream,
            status,
            request_id,
            usage,
            failed,
        }
    } else {
        Answer::Cut {
            model,
            stream,
            status,
            request_id,
            usage,
            failed,
        }
    }
}

enum Relayed {
    Whole,
    /// Until it broke off, or was given up on.
    BrokeOff,
    /// Until sending it failed so: the deadline passed, or the session ended.
    Unsent(io::Error),
}

/// Streams `body` to `client` as it arrives, showing it to `scan` and giving it to `keep`, and
/// telling `keep` where its output begins, until it ends, without writing its end, and returns
/// how it went.
fn relay(
    body: &mut impl Read,
    client: &mut ModelResponse<impl Clock>,
    scan: &mut Scan,
    keep: &dyn Keep,
) -> Relayed {
    // A read fills at most one frame of the session.
    let mut buffer = vec![0; frame::MAX_BODY_BYTES];
    loop {
        match body.read(&mut buffer) {
            Ok(0) => return Relayed::Whole,
            Ok(read) => {
                let began = scan.feed(&buffer[..read]);
                keep.response(&buffer[..read]);
                // After the bytes, so that the first output is timed no earlier than they are.
                if began {
                    keep.first_output();
                }
                if let Err(error) = client.send(&buffer[..read]) {
                    return Relayed::Unsent(error);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Relayed::BrokeOff,
        }
    }
}

/// The provider's id for the request that `headers` answer, if it gave one: `x-request-id`, as
/// OpenAI's APIs give it, `request-id`, as the Messages API does, or else `x-oai-request-id`, as
/// Codex keeps them for an error and Inspect for a model call.
fn request_id(headers: &HeaderMap) -> Option<String> {
    ["x-request-id", "request-id", "x-oai-request-id"]
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
fn returned(headers: &HeaderMap, closed: &Closed<'_>) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| closed.route().returns(name.as_str()))
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
