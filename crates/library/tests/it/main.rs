//! The standard library as `pumpkin` holds it: the files it bundles, and what each harness's
//! function gives, evaluated through `lang` with a loader over the bundled files.

#![expect(
    clippy::panic,
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

mod codex;
mod docker;

use std::fs;
use std::path::Path;

use expect_test::{Expect, expect};

fn check(actual: &str, expect: &Expect) {
    expect.assert_eq(actual);
}

#[test]
fn bundled() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../library");
    let mut found = Vec::new();
    walk(&root, "", &mut found);
    found.sort();
    let bundled: Vec<String> = (library::FILES.iter())
        .map(|(path, bytes)| {
            assert_eq!(*bytes, fs::read(root.join(path)).unwrap(), "{path}");
            (*path).to_owned()
        })
        .collect();
    // Sorted, so that a loader may search it.
    assert_eq!(
        bundled, found,
        "the table lists each file below `library/`, sorted"
    );
    check(
        &bundled.join("\n"),
        &expect![[r#"
            harness/codex/LICENSE
            harness/codex/catalog.star
            harness/codex/codex.star
            harness/codex/layer/Dockerfile"#]],
    );
}

/// Adds the path of each file below `dir` to `found`, after `below`, its path below the root.
fn walk(dir: &Path, below: &str, found: &mut Vec<String>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        let path = format!("{below}{name}");
        if entry.file_type().unwrap().is_dir() {
            walk(&entry.path(), &format!("{path}/"), found);
        } else {
            found.push(path);
        }
    }
}

/// The text of the library's file at `path`.
fn file(path: &str) -> String {
    let found = library::FILES.iter().find(|(file, _)| *file == path);
    let (_, bytes) = found.unwrap_or_else(|| panic!("the library holds no `{path}`"));
    String::from_utf8(bytes.to_vec()).unwrap()
}
