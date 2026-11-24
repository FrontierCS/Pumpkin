//! The check of the links to code on GitHub, which cite it.

use std::path::Path;

use super::SOURCES;
use crate::Result;
use crate::repo::Repo;

const GITHUB: &str = "https://github.com/";

/// The problems of the links in `files`, the repository's but for the vendored suites, to code on
/// GitHub at a revision that moves, a branch, a tag or a short SHA, where a citation must show the
/// code that was read. A link to a Markdown document may follow its branch, as the help's links to
/// the guides do.
pub(super) fn check(repo: &Repo<'_>, files: &[String]) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    for path in files {
        let extension = Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str());
        let text = |extension| SOURCES.contains(&extension) || matches!(extension, "md" | "toml");
        if !extension.is_some_and(text) || path.starts_with(super::VENDORED) {
            continue;
        }
        for (index, line) in repo.read(path)?.lines().enumerate() {
            for (at, _) in line.match_indices(GITHUB) {
                let url = &line[at + GITHUB.len()..];
                let end = url.find(|c: char| c.is_whitespace() || "()<>[]\"'`".contains(c));
                let parts: Vec<&str> = url[..end.unwrap_or(url.len())].splitn(5, '/').collect();
                let [_, _, "blob" | "tree", revision, file] = parts[..] else {
                    continue;
                };
                let file = &file[..file.find(['#', '?']).unwrap_or(file.len())];
                let sha =
                    revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit());
                let document = Path::new(file).extension() == Some("md".as_ref());
                if !sha && !file.is_empty() && !document {
                    let (line, column) = (index + 1, line[..at].chars().count() + 1);
                    problems.push(format!(
                        "{path}:{line}:{column}: a link to code at `{revision}`, which can move: \
                         link it at the full SHA of the commit that was read"
                    ));
                }
            }
        }
    }
    Ok(problems)
}
