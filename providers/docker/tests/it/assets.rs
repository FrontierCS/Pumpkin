//! The asset cache's fetch, from a stand-in HTTPS server behind the stand-in TLS edge
//! ([`test_support::edge`]), on the loopback. A file's server is HTTP's documented API.

use std::fs::{DirBuilder, Permissions};
use std::io::{BufReader, Write as _};
use std::net::TcpStream;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::{fs, thread};

use docker::AssetCache;
use expect_test::{Expect, expect};
use provider::{AssetDigest, Assets as _, PublishedAsset, RunTag};
use schema::ir::{AssetFiles, Sha256Digest};
use serde_json::json;
use test_support::{TestDir, edge, http};

// `{}\n`, `1\n` and `b\n`, and their SHA-256s, as `sha256sum` prints them.
const CONFIG: (&str, &[u8], &str) = (
    "config.json",
    b"{}\n",
    "ca3d163bab055381827226140568f3bef7eaac187cebd76878e0b63e9e442356",
);
const SHARD: (&str, &[u8], &str) = (
    "shards/1.bin",
    b"1\n",
    "4355a46b19d348dc2f57c046f8ef63d4538ebb936000f3c9ee954a27460dd865",
);
const B: &[u8] = b"b\n";
const B_SHA256: &str = "0263829989b6fd954f72baaf2fc64bc2e2f01d692d4de72986ea808f6e99813f";

/// Any digest: the runtime computes it, and the cache only names an entry by it.
const DIGEST: &str = "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

/// Serves each of `files`, a path and its bytes, at `/<path>`, one connection at a time, each
/// once `gate` lets it if one is given. It tells `asked` of each request first, if given.
/// Returns its port on the loopback.
fn serving(
    files: Vec<(&'static str, &'static [u8])>,
    asked: Option<Sender<()>>,
    gate: Option<Receiver<()>>,
) -> u16 {
    edge::front_of(move |stream: TcpStream| {
        let (line, _, _) = http::read_request(&mut BufReader::new(&stream));
        if let Some(asked) = &asked {
            asked.send(()).unwrap();
        }
        if let Some(gate) = &gate {
            gate.recv().unwrap();
        }
        let path = line.split(' ').nth(1).unwrap().trim_start_matches('/');
        let (_, bytes) = files.iter().find(|(served, _)| *served == path).unwrap();
        let mut stream = &stream;
        let head = "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length";
        write!(stream, "{head}: {}\r\n\r\n", bytes.len()).unwrap();
        stream.write_all(bytes).unwrap();
    })
}

/// The files of an asset, each a path, the SHA-256 that it pins and its size, at the stand-in on
/// `port`.
fn files(port: u16, pinned: &[(&str, &str, u64)]) -> AssetFiles {
    let files = (pinned.iter()).map(|(path, sha256, size)| {
        json!({
            "path": path, "url": format!("https://{}:{port}/{path}", edge::NAME),
            "size": size, "sha256": format!("sha256:{sha256}"),
        })
    });
    serde_json::from_value(files.collect()).unwrap()
}

fn digest() -> AssetDigest {
    AssetDigest::from(Sha256Digest::try_from(DIGEST.to_owned()).unwrap())
}

/// What a publish of `files` into the cache at `dir` gave: whether it fetched them, and where it
/// keeps them, or why it failed.
fn published(dir: &Path, files: &AssetFiles) -> String {
    let cache = AssetCache::trusting(dir, &edge::root());
    let run = RunTag::try_from("run-1".to_owned()).unwrap();
    match cache.publish_asset(&run, &digest(), files) {
        Ok(PublishedAsset { reference, fetched }) => {
            let kept = reference
                .as_str()
                .replace(&dir.display().to_string(), "<cache>");
            format!("fetched: {fetched}, at {kept}")
        }
        Err(error) => error,
    }
}

/// Checks against `expect` each of `outcomes`, then what the cache at `dir` holds, each with its
/// mode. A port, the digest and each of `masked` are masked.
fn check(dir: &Path, outcomes: &[String], masked: &[(&str, &str)], expect: &Expect) {
    let mut lines = outcomes.to_vec();
    lines.push("the cache holds:".to_owned());
    if dir.exists() {
        listed(dir, dir, &mut lines);
    }
    let shown = lines
        .join("\n")
        .replace(&DIGEST["sha256:".len()..], "<digest>");
    let shown = (masked.iter()).fold(shown, |shown, (from, to)| shown.replace(from, to));
    let mut unported = String::new();
    let mut rest = shown.as_str();
    while let Some(at) = rest.find("localhost:") {
        let (before, after) = rest.split_at(at + "localhost:".len());
        unported.push_str(before);
        unported.push_str("PORT");
        rest = after.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    unported.push_str(rest);
    expect.assert_eq(&unported);
}

/// Each entry below `dir`, sorted, by its path from `base`, with its mode, and a file's bytes.
fn listed(base: &Path, dir: &Path, lines: &mut Vec<String>) {
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o7777;
    if dir == base {
        lines.push(format!("  {:04o} .", mode(dir)));
    }
    let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(Result::unwrap).collect();
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let shown = path.strip_prefix(base).unwrap().display().to_string();
        if path.is_dir() {
            lines.push(format!("  {:04o} {shown}/", mode(&path)));
            listed(base, &path, lines);
        } else {
            let bytes = String::from_utf8_lossy(&fs::read(&path).unwrap())
                .escape_debug()
                .to_string();
            let held = format!("  {:04o} {shown}: {bytes}", mode(&path));
            lines.push(held.trim_end().to_owned());
        }
    }
}

// The first publish of a digest fetches its files into an entry, and a later one finds them there.
// Only the cache's owner reaches them. No mode keeps a sandbox from writing them: its mount does.
#[test]
fn fetches_each_digest_once() {
    let dir = TestDir::new();
    let cache = dir.path().join("assets");
    let port = serving(vec![(CONFIG.0, CONFIG.1), (SHARD.0, SHARD.1)], None, None);
    let model = files(port, &[(CONFIG.0, CONFIG.2, 3), (SHARD.0, SHARD.2, 2)]);
    let outcomes = [published(&cache, &model), published(&cache, &model)];
    check(
        &cache,
        &outcomes,
        &[],
        &expect![[r#"
        fetched: true, at <cache>/<digest>
        fetched: false, at <cache>/<digest>
        the cache holds:
          0700 .
          0600 .<digest>.lock:
          0555 <digest>/
          0444 <digest>/config.json: {}\n
          0555 <digest>/shards/
          0444 <digest>/shards/1.bin: 1\n"#]],
    );
}

// A file that isn't what its asset pins fails the publish, which keeps none of the asset's files.
#[test]
fn refuses_what_isnt_pinned() {
    let dir = TestDir::new();
    let cache = dir.path().join("assets");
    let served = vec![
        (CONFIG.0, CONFIG.1),
        ("b", B),
        ("long", b"bb\n"),
        ("short", b"b"),
    ];
    let port = serving(served, None, None);
    let pinned = |path| files(port, &[(CONFIG.0, CONFIG.2, 3), (path, B_SHA256, 2)]);
    let mut wrong = vec![(CONFIG.0, CONFIG.2, 3), ("b", B_SHA256, 2)];
    wrong[1].1 = SHARD.2;
    let outcomes = [
        published(&cache, &files(port, &wrong)),
        published(&cache, &pinned("short")),
        published(&cache, &pinned("long")),
    ];
    check(
        &cache,
        &outcomes,
        &[],
        &expect![[r#"
        `b` from https://localhost:PORT/b has the SHA-256 sha256:0263829989b6fd954f72baaf2fc64bc2e2f01d692d4de72986ea808f6e99813f, not the pinned sha256:4355a46b19d348dc2f57c046f8ef63d4538ebb936000f3c9ee954a27460dd865: give the SHA-256 of the file that the URL serves
        `short` from https://localhost:PORT/short ends after 1 of its 2 bytes: give the size of the file that the URL serves
        `long` from https://localhost:PORT/long holds more than its 2 bytes: give the size of the file that the URL serves
        the cache holds:
          0700 .
          0600 .<digest>.lock:"#]],
    );
}

// Two fetches of a digest at once fetch its files once. The second begins while the first holds
// the digest's lock and waits on its server.
#[test]
fn fetches_at_once() {
    let dir = TestDir::new();
    let cache = dir.path().join("assets");
    let (asked, first_asked) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let held = serving(vec![(CONFIG.0, CONFIG.1)], Some(asked), Some(gate));
    let (asked, second_asked) = mpsc::channel();
    let free = serving(vec![(CONFIG.0, CONFIG.1)], Some(asked), None);
    let publishing = |port| {
        let (cache, files) = (cache.clone(), files(port, &[(CONFIG.0, CONFIG.2, 3)]));
        thread::spawn(move || published(&cache, &files))
    };
    let first = publishing(held);
    first_asked.recv().unwrap();
    let second = publishing(free);
    // Whether the second waits for the lock by then or takes it later, it finds the entry.
    release.send(()).unwrap();
    let (first, second) = (first.join().unwrap(), second.join().unwrap());
    let asked = format!(
        "the second's server was asked {} times",
        second_asked.try_iter().count()
    );
    check(
        &cache,
        &[first, second, asked],
        &[],
        &expect![[r#"
        fetched: true, at <cache>/<digest>
        fetched: false, at <cache>/<digest>
        the second's server was asked 0 times
        the cache holds:
          0700 .
          0600 .<digest>.lock:
          0555 <digest>/
          0444 <digest>/config.json: {}\n"#]],
    );
}

// A fetch replaces what a killed fetch of its digest left in its staging directory, though
// read-only.
#[test]
fn replaces_what_a_killed_fetch_left() {
    let dir = TestDir::new();
    let cache = dir.path().join("assets");
    // As the cache makes itself.
    DirBuilder::new().mode(0o700).create(&cache).unwrap();
    let left = cache.join(format!(".{}.part", &DIGEST["sha256:".len()..]));
    let shards = left.join("shards");
    fs::create_dir_all(&shards).unwrap();
    fs::write(shards.join("1.bin"), SHARD.1).unwrap();
    fs::set_permissions(shards.join("1.bin"), Permissions::from_mode(0o444)).unwrap();
    for read_only in [&shards, &left] {
        fs::set_permissions(read_only, Permissions::from_mode(0o555)).unwrap();
    }
    let port = serving(vec![(CONFIG.0, CONFIG.1)], None, None);
    let outcomes = [published(&cache, &files(port, &[(CONFIG.0, CONFIG.2, 3)]))];
    check(
        &cache,
        &outcomes,
        &[],
        &expect![[r#"
        fetched: true, at <cache>/<digest>
        the cache holds:
          0700 .
          0600 .<digest>.lock:
          0555 <digest>/
          0444 <digest>/config.json: {}\n"#]],
    );
}
