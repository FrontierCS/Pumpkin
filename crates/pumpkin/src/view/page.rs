//! The page that reads the view's data in a browser, its files compiled into `pumpkin`: an
//! `index.html` at `/`, and its modules of script, its style and its font, each at its own path
//! below a directory named by their digest, so that a browser keeps each as long as it likes and
//! a build with other files names other paths. The page carries no data, so no request of it
//! needs the token: the page takes the token from the address that `pumpkin view` printed, and
//! sends it with each request of the data.

use std::fmt::Write as _;

use miniz_oxide::deflate::CompressionLevel;
use schema::ir::Sha256Digest;
use sha2::{Digest as _, Sha256};

use super::http::{self, Response};

/// The page's files, by the path each is served at, with its type.
const FILES: [(&str, &str, &[u8]); 22] = [
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
        "/payload.js",
        "text/javascript; charset=utf-8",
        include_bytes!("page/payload.js"),
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
        "/search.js",
        "text/javascript; charset=utf-8",
        include_bytes!("page/search.js"),
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
        "/talk.js",
        "text/javascript; charset=utf-8",
        include_bytes!("page/talk.js"),
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

/// The one script of [`FILES`] that is not a module: `index.html` runs it as a classic script,
/// before it draws, so a module's preload would fetch it a second time, as a module.
const CLASSIC_SCRIPT: &str = "/scheme.js";

/// The page's files, each with its form deflated ([`http::deflated`]).
pub(super) struct Page {
    /// The directory that the files but `/` are served below: `/` and the SHA-256 of their paths
    /// and bodies.
    dir: String,
    /// Each file of [`FILES`], by its order: its body as served, and its form deflated.
    files: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

impl Page {
    /// The page, each file deflated once, so at the level that makes it shortest.
    pub(super) fn new() -> Self {
        let mut digest = Sha256::new();
        for (path, _, body) in FILES.iter().filter(|(path, ..)| *path != "/") {
            digest.update(format!("{path}\0{}\0", body.len()));
            digest.update(body);
        }
        let digest: [u8; 32] = digest.finalize().into();
        let dir = format!("/{}", Sha256Digest::from(digest).hex());
        let files = FILES.iter().map(|(path, _, body)| {
            let body = if *path == "/" {
                entry(body, &dir)
            } else {
                body.to_vec()
            };
            let deflated = http::deflated(&body, CompressionLevel::UberCompression);
            (body, deflated)
        });
        let files = files.collect();
        Self { dir, files }
    }

    /// The page's file at `path`, if it is one.
    pub(super) fn file(&self, path: &str) -> Option<Response> {
        let at = match path {
            "/" => "/",
            _ => path.strip_prefix(&self.dir).filter(|at| *at != "/")?,
        };
        let index = FILES.iter().position(|(path, ..)| *path == at)?;
        let (body, deflated) = &self.files[index];
        let file = Response::file(FILES[index].1, body.clone(), deflated.clone());
        Some(if at == "/" { file } else { file.immutable() })
    }
}

impl Page {
    /// The page's files as a static copy serves them, each with its path there, relative, and
    /// whether it is the same at that path in every build: `index.html`, which reads the data
    /// from files beside it, as `data-source="static"` on its `<html>` says, and the rest, below
    /// the directory named by their digest.
    pub(super) fn exported(&self) -> Vec<(String, Vec<u8>, bool)> {
        let (html, marked) = (
            "<html lang=\"en\">",
            "<html lang=\"en\" data-source=\"static\">",
        );
        let files = FILES.iter().zip(&self.files);
        let files = files.map(|((path, ..), (body, _))| {
            if *path != "/" {
                return (format!("{}{path}", &self.dir[1..]), body.clone(), true);
            }
            let index = String::from_utf8_lossy(body);
            assert!(
                index.contains(html),
                "the page's index.html opens with {html}"
            );
            assert!(
                index.contains(CHARSET),
                "the page's index.html names its {CHARSET}"
            );
            let index = index.replacen(html, marked, 1);
            let index = index.replacen(CHARSET, &format!("{CHARSET}\n{}", policy_meta()), 1);
            ("index.html".to_owned(), index.into_bytes(), false)
        });
        files.collect()
    }
}

/// The line of `index.html` that the policy's `<meta>` follows, as early as the page allows.
const CHARSET: &str = "<meta charset=\"utf-8\">";

/// The view's content security policy ([`http::CONTENT_SECURITY_POLICY`]) as a `<meta>`, but for
/// `frame-ancestors`, which a `<meta>` can't say.
fn policy_meta() -> String {
    let policy = http::CONTENT_SECURITY_POLICY.split("; ");
    let policy: Vec<&str> = policy
        .filter(|directive| !directive.starts_with("frame-ancestors"))
        .collect();
    format!(
        "<meta http-equiv=\"Content-Security-Policy\" content=\"{}\">",
        policy.join("; ")
    )
}

/// `index.html`, `body`, with each file of [`FILES`] that it names named below `dir`, and each
/// module preloaded, so that the browser asks for every module at once instead of a level of
/// imports at a time.
fn entry(body: &[u8], dir: &str) -> Vec<u8> {
    let mut entry = String::from_utf8_lossy(body).into_owned();
    let mut preloads = String::new();
    for (path, content_type, _) in FILES.iter().filter(|(path, ..)| *path != "/") {
        entry = entry.replace(&format!("\"{path}\""), &format!("\"{dir}{path}\""));
        if content_type.starts_with("text/javascript") && *path != CLASSIC_SCRIPT {
            let _ = writeln!(
                preloads,
                "<link rel=\"modulepreload\" href=\"{dir}{path}\">"
            );
        }
    }
    entry
        .replacen("</head>", &format!("{preloads}</head>"), 1)
        .into_bytes()
}
