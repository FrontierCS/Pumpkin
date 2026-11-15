//! The page that reads the view's data in a browser, its files compiled into `pumpkin`: an
//! `index.html` at `/`, its modules of script, its style and its font, each at its own path. The
//! page carries no data, so no request of it needs the token: the page takes the token from the
//! address that `pumpkin view` printed, and sends it with each request of the data.

use miniz_oxide::deflate::CompressionLevel;

use super::http::{self, Response};

/// The page's files, by the path each is served at, with its type.
const FILES: [(&str, &str, &[u8]); 19] = [
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
        "/diff.js",
        "text/javascript; charset=utf-8",
        include_bytes!("page/diff.js"),
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
        "/key.js",
        "text/javascript; charset=utf-8",
        include_bytes!("page/key.js"),
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
        "/log.js",
        "text/javascript; charset=utf-8",
        include_bytes!("page/log.js"),
    ),
    (
        "/records.js",
        "text/javascript; charset=utf-8",
        include_bytes!("page/records.js"),
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
        "/scheme.js",
        "text/javascript; charset=utf-8",
        include_bytes!("page/scheme.js"),
    ),
    (
        "/stats.js",
        "text/javascript; charset=utf-8",
        include_bytes!("page/stats.js"),
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
        "/timeline.js",
        "text/javascript; charset=utf-8",
        include_bytes!("page/timeline.js"),
    ),
    (
        "/trace.js",
        "text/javascript; charset=utf-8",
        include_bytes!("page/trace.js"),
    ),
];

/// The page's files, each with its form deflated ([`http::deflated`]).
pub(super) struct Page {
    /// By the order of [`FILES`].
    deflated: Vec<Option<Vec<u8>>>,
}

impl Page {
    /// The page, each file deflated once, so at the level that makes it shortest.
    pub(super) fn new() -> Self {
        let deflated = FILES
            .iter()
            .map(|(_, _, body)| http::deflated(body, CompressionLevel::UberCompression));
        Self {
            deflated: deflated.collect(),
        }
    }

    /// The page's file at `path`, if it is one.
    pub(super) fn file(&self, path: &str) -> Option<Response> {
        let index = FILES.iter().position(|(at, ..)| *at == path)?;
        let (_, content_type, body) = FILES[index];
        Some(Response::file(
            content_type,
            body,
            self.deflated[index].clone(),
        ))
    }
}
