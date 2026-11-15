//! The page that reads the view's data in a browser, its files compiled into `pumpkin`: an
//! `index.html` at `/`, its modules of script, its style and its font, each at its own path. The
//! page carries no data, so no request of it needs the token: the page takes the token from the
//! address that `pumpkin view` printed, and sends it with each request of the data.

use super::http::Response;

/// The page's files, by the path each is served at, with its type.
const FILES: [(&str, &str, &[u8]); 12] = [
    (
        "/",
        "text/html; charset=utf-8",
        include_bytes!("page/index.html"),
    ),
    (
        "/app.js",
        "text/javascript; charset=utf-8",
        include_bytes!("page/app.js"),
    ),
    (
        "/h.js",
        "text/javascript; charset=utf-8",
        include_bytes!("page/h.js"),
    ),
    (
        "/inspector.js",
        "text/javascript; charset=utf-8",
        include_bytes!("page/inspector.js"),
    ),
    (
        "/ledger.js",
        "text/javascript; charset=utf-8",
        include_bytes!("page/ledger.js"),
    ),
    (
        "/list.js",
        "text/javascript; charset=utf-8",
        include_bytes!("page/list.js"),
    ),
    (
        "/rows.js",
        "text/javascript; charset=utf-8",
        include_bytes!("page/rows.js"),
    ),
    (
        "/run.js",
        "text/javascript; charset=utf-8",
        include_bytes!("page/run.js"),
    ),
    (
        "/steps.js",
        "text/javascript; charset=utf-8",
        include_bytes!("page/steps.js"),
    ),
    // Hanken Grotesk, the face of frontier-cs.org, as a Latin subset of its variable weights; its
    // licence, the SIL Open Font License, is beside it.
    (
        "/hanken-grotesk.woff2",
        "font/woff2",
        include_bytes!("page/font/hanken-grotesk.woff2"),
    ),
    (
        "/style.css",
        "text/css; charset=utf-8",
        include_bytes!("page/style.css"),
    ),
    (
        "/trace.js",
        "text/javascript; charset=utf-8",
        include_bytes!("page/trace.js"),
    ),
];

/// The page's file at `path`, if it is one.
pub(super) fn file(path: &str) -> Option<Response> {
    let (_, content_type, body) = FILES.iter().find(|(at, ..)| *at == path)?;
    Some(Response::file(content_type, body))
}
