//! The part of one archive that the image's apt would read for the install. Apt selects the
//! packages and authenticates the indexes. The signed `InRelease` pins the compressed files
//! that the offline `file:` mirror needs, even when apt's local lists use another compression.

use std::collections::BTreeMap;

use schema::ir::Sha256Digest;
use sha2::{Digest as _, Sha256};

use super::Container;

pub(super) fn resolve(container: &Container<'_>, packages: &[String]) -> Result<String, String> {
    container.exec("apt-get", ["update", "--quiet=2", "--error-on=any"])?;
    let targets = container.apt_get([
        "indextargets",
        "--no-release-info",
        "--format",
        "$(REPO_URI) $(BASE_URI) $(METAKEY) $(COMPRESSIONTYPES)",
    ])?;
    let archive = Archive::read(&targets)?;
    let mut pins = Vec::new();
    let install = [
        "install",
        "--yes",
        "--no-install-recommends",
        "--print-uris",
        "--option",
        "Acquire::ForceHash=SHA256",
    ];
    let install = container.apt_get(
        install
            .into_iter()
            .chain(packages.iter().map(String::as_str)),
    )?;
    for entry in uris(&install) {
        let (url, fields) = entry?;
        let hash = fields
            .split_ascii_whitespace()
            .nth(2)
            .and_then(|hash| hash.strip_prefix("SHA256:"))
            .ok_or_else(|| format!("apt-get printed no SHA-256 for {url}"))?;
        // Apt escapes a '+' or '~' as '%2b' or '%7e', which an ADD source can't hold.
        let url = url
            .replace("%2b", "+")
            .replace("%2B", "+")
            .replace("%7e", "~")
            .replace("%7E", "~");
        let path = archive.path(&url)?;
        pins.push(format!("{hash} {url} {path}"));
    }
    archive.index_pins(container, &mut pins)?;
    Ok(pins.join("\n"))
}

struct Archive<'a> {
    root: &'a str,
    /// The suite's base URL to each index's key and its compression types, in apt's order.
    suites: BTreeMap<&'a str, Vec<(&'a str, &'a str)>>,
}

impl<'a> Archive<'a> {
    fn read(targets: &'a str) -> Result<Self, String> {
        let root = targets
            .split_ascii_whitespace()
            .next()
            .ok_or("the sources name no archive: name one https:// archive")?;
        if !root.starts_with("https://") {
            return Err(format!(
                "the sources' root, {root}, isn't an `https://` URL: name the archive by one"
            ));
        }
        let mut suites = BTreeMap::<_, Vec<_>>::new();
        for line in targets.lines() {
            let [repo, base, key, types] = line.splitn(4, ' ').collect::<Vec<_>>()[..] else {
                return Err(format!(
                    "apt-get printed an incomplete index target: {line}"
                ));
            };
            if repo != root {
                return Err(format!(
                    "the sources name other roots than one: {root} and {repo}: name one archive"
                ));
            }
            suites.entry(base).or_default().push((key, types));
        }
        Ok(Self { root, suites })
    }

    fn path<'u>(&self, url: &'u str) -> Result<&'u str, String> {
        url.strip_prefix(self.root).ok_or_else(|| {
            format!(
                "apt-get named {url} outside the sources' root, {}",
                self.root
            )
        })
    }

    fn index_pins(
        mut self,
        container: &Container<'_>,
        pins: &mut Vec<String>,
    ) -> Result<(), String> {
        // Ask apt for the cache filenames: its URL escaping differs from the archive's. A
        // --print-uris update lists possible indexes too, including ones the archive lacks.
        let update = container.apt_get(["update", "--print-uris"])?;
        for entry in uris(&update) {
            let (url, fields) = entry?;
            let Some(base) = url.strip_suffix("InRelease") else {
                continue;
            };
            let Some(indexes) = self.suites.remove(base) else {
                continue;
            };
            let filename = fields
                .split_ascii_whitespace()
                .next()
                .ok_or_else(|| format!("apt-get printed no filename for {url}"))?;
            let cached = format!("/var/lib/apt/lists/{filename}");
            let bytes = container.exec("cat", ["--", &cached])?;
            let hash = Sha256Digest::from(<[u8; 32]>::from(Sha256::digest(&bytes)));
            pins.push(format!("{} {url} {}", hash.hex(), self.path(url)?));
            let release = String::from_utf8_lossy(&bytes);
            let hashes = index_hashes(&release, url)?;
            for (key, types) in indexes {
                for kind in types.split_ascii_whitespace() {
                    let file = if kind == "uncompressed" {
                        key.to_owned()
                    } else {
                        format!("{key}.{kind}")
                    };
                    if let Some(hash) = hashes.get(file.as_str()) {
                        let dir = key.rfind('/').map_or("", |end| &key[..=end]);
                        let path = self.path(base)?;
                        pins.push(format!(
                            "{hash} {base}{dir}by-hash/SHA256/{hash} {path}{file}"
                        ));
                        break;
                    }
                }
            }
        }
        if let Some(base) = self.suites.keys().next() {
            return Err(format!("apt-get printed no InRelease for {base}"));
        }
        Ok(())
    }
}

fn index_hashes<'a>(release: &'a str, url: &str) -> Result<BTreeMap<&'a str, &'a str>, String> {
    let mut lines = release.lines();
    if !lines.any(|line| line == "SHA256:") {
        return Err(format!("{url} has no SHA256 section"));
    }
    let mut hashes = BTreeMap::new();
    for line in lines.take_while(|line| line.starts_with(' ')) {
        let [hash, _size, path] = line.split_ascii_whitespace().collect::<Vec<_>>()[..] else {
            return Err(format!("{url} has a malformed SHA256 entry: {line}"));
        };
        hashes.insert(path, hash);
    }
    Ok(hashes)
}

// Each download record begins with its URL in single quotes. Apt's other lines describe the
// install to the user, even with --print-uris.
fn uris(output: &str) -> impl Iterator<Item = Result<(&str, &str), String>> {
    output
        .lines()
        .filter_map(|line| line.strip_prefix('\''))
        .map(|line| {
            line.split_once("' ")
                .ok_or_else(|| format!("apt-get printed an incomplete URI record: '{line}"))
        })
}
