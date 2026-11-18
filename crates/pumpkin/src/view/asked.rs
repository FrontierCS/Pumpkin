//! What a request of the view asks for ([`Asked`]), read from its path and query, and whether
//! its method and origin admit it.

use provider::RunTag;

use super::http::{self, Method, Request, Response};
use super::trace;
use crate::next;

/// What a request to `/api/runs` asks for.
pub(super) enum Asked {
    Runs {
        before: Option<String>,
    },
    Run {
        id: RunTag,
    },
    Since {
        id: RunTag,
        log: u64,
        log_epoch: Option<u64>,
    },
    Log {
        id: RunTag,
        at: u64,
        log_epoch: Option<u64>,
    },
    Cancel {
        id: RunTag,
    },
    Cancelling {
        id: RunTag,
    },
    Trace {
        id: RunTag,
        asked: trace::Asked,
    },
}

pub(super) fn asked(request: &Request) -> Result<Asked, Response> {
    let path = request.path.as_str();
    let not_found = || {
        let message = format!(
            "nothing is at `{path}`: expected the page at `/`, or the runs at `/api/runs` and \
             below it"
        );
        Response::refused("404 Not Found", &message)
    };
    let bad = |message: String| Response::refused("400 Bad Request", &message);
    let Some(api) = path.strip_prefix("/api/runs") else {
        return Err(not_found());
    };
    let parts: Vec<&str> = api.split('/').collect();
    let id = || match parts.get(1) {
        Some(id) => RunTag::try_from((*id).to_owned()).map_err(bad),
        None => Err(not_found()),
    };
    let byte = |name: &str| {
        request.number(name)?.ok_or_else(|| {
            bad(format!(
                "give the byte of the log: `?{name}=<byte>`, 0 for its start"
            ))
        })
    };
    match parts[..] {
        [""] => Ok(Asked::Runs {
            before: request.query("before").map(str::to_owned),
        }),
        ["", _] => Ok(Asked::Run { id: id()? }),
        ["", _, "since"] => Ok(Asked::Since {
            id: id()?,
            log: byte("log")?,
            log_epoch: request.number("log_epoch")?,
        }),
        ["", _, "log"] => Ok(Asked::Log {
            id: id()?,
            at: byte("at")?,
            log_epoch: request.number("log_epoch")?,
        }),
        ["", _, "cancel"] => Ok(Asked::Cancel { id: id()? }),
        ["", _, "cancelling"] => Ok(Asked::Cancelling { id: id()? }),
        [
            "",
            _,
            "trace" | "exchange" | "item" | "payload" | "diff",
            ..,
        ] => Ok(Asked::Trace {
            id: id()?,
            asked: trace::asked(&parts[2..], request)?,
        }),
        _ => Err(not_found()),
    }
}

pub(super) fn allowed(
    asked: &Asked,
    request: &Request,
    port: u16,
    named: Option<&str>,
) -> Result<(), Response> {
    let refused = |status, message: String| Err(Response::refused(status, &message));
    match (asked, request.method) {
        (Asked::Cancel { id }, Method::Post) => {
            // A cancel changes a run, so only the view's own page may send it: a page of another
            // site can make the browser send a `POST` here, but the browser names that site in
            // `Origin`, which the page can't change.
            let page = http::HOSTS.map(|host| format!("http://{host}:{port}"));
            let from = match &request.origin {
                Some(origin) if page.contains(origin) => return Ok(()),
                Some(origin) => format!("comes from `{origin}`, a page of another site"),
                None => "carries no `Origin`, which a browser sends".to_owned(),
            };
            refused(
                "403 Forbidden",
                format!(
                    "the cancel of run {} {from}: cancel it from the page that `pumpkin view` \
                     serves, or run: {}",
                    id.as_str(),
                    next::of_run("cancel", named, id.as_str())
                ),
            )
        }
        (Asked::Cancel { id }, Method::Get) => refused(
            "405 Method Not Allowed",
            format!(
                "run {} is cancelled by `POST`, which the page's cancel sends: cancel it there, \
                 or run: {}",
                id.as_str(),
                next::of_run("cancel", named, id.as_str())
            ),
        ),
        (_, Method::Post) => refused(
            "405 Method Not Allowed",
            format!(
                "`{}` is read by `GET`: only a run's cancel, `/api/runs/<id>/cancel`, takes \
                 `POST`",
                request.path
            ),
        ),
        (_, Method::Get) => Ok(()),
    }
}
