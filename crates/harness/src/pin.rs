//! A harness's pin, and the recipe that adds the harness to an image, as [`Pin::recipe`] says.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use schema::ir::{AbsolutePath, Sha256Digest};
use schema::sandbox::{RelativePath, harness_dir};

/// The image whose `tar` unpacks a harness's tarball, and whose `chmod` gives each of its files
/// and directories its mode: busybox 1.37, by the digest of its index, which names an image for
/// each platform.
const BUSYBOX: &str =
    "busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e";

/// Where the stage that unpacks a harness puts its tarball.
const TARBALL: &str = "/harness.tgz";

/// Where the stage that sets a harness's modes puts the tree the image gets: what the image gets
/// at `/<path>` is at `/out/<path>` there.
const OUT: &str = "/out";

/// The permission bits of each directory of a harness's tree: anyone may list and enter it.
const DIRECTORY: u32 = 0o555;

/// A harness, pinned: a gzip-compressed tarball, fetched over HTTPS and checked by its SHA-256,
/// and the files kept from one of its directories, each with its [`Mode`].
#[derive(Clone, Copy, Debug)]
pub struct Pin {
    /// The harness's name and version, such as `codex-0.167.0`, which name its directory below
    /// [`harness_dir()`] and its recipe's stage that unpacks it: from a-z, 0-9, `.`, `_` and
    /// `-`, starting with a letter.
    pub name: &'static str,
    /// The tarball's URL: `https://`, then ASCII letters and digits and `-._~:/@%+`.
    pub url: &'static str,
    /// The tarball's digest, as a [`Sha256Digest`] is written.
    pub sha256: &'static str,
    /// The tarball's directory that holds the files kept.
    pub dir: &'static str,
    /// The files kept, at least one, sorted by their paths' bytes.
    pub files: &'static [PinnedFile],
}

/// A file a [`Pin`] keeps.
#[derive(Clone, Copy, Debug)]
pub struct PinnedFile {
    /// Its path below the pin's [`dir`](Pin::dir), which it keeps below the harness's root: from
    /// ASCII letters and digits and `-._/`.
    pub path: &'static str,
    pub mode: Mode,
}

/// The permission bits of a file a [`Pin`] keeps, which root owns, so that only root may change
/// it.
#[derive(Clone, Copy, Debug)]
pub enum Mode {
    /// 0555: anyone may read and run it.
    Executable,
    /// 0444: anyone may read it.
    Readable,
}

impl Mode {
    fn bits(self) -> u32 {
        match self {
            Self::Executable => 0o555,
            Self::Readable => 0o444,
        }
    }
}

impl Pin {
    /// A Dockerfile that builds `base`, an image the engine has, named as `FROM` takes it, in
    /// ASCII letters and digits and `-._/:@`, as every image reference is, with the harness added
    /// at its root, [`harness_dir()`]`/<name>`, in a layer of its own: each file kept, at its
    /// path there, with its [`Mode`], in directories of mode 0555, all owned by root. The engine
    /// fetches the tarball, which `ADD --checksum` checks, and so needs Docker 25.0 or later.
    ///
    /// # Panics
    ///
    /// If the pin fails a check that its fields' docs ask for, as only a bug in the code that
    /// wrote it can, since the tests make each pin's recipe; or if `base` isn't written as above,
    /// as only a bug in the caller can, since the engine has accepted it.
    pub fn recipe(&self, base: &str) -> String {
        match self.check(base) {
            Ok(()) => {}
            #[expect(
                clippy::unreachable,
                reason = "a pin is written in code, never read from input, and the tests make the \
                          recipe of each, and a base is named as the engine has accepted it, in \
                          the characters of every image reference; checking pins as the crate \
                          builds would take a `const fn` copy of each check"
            )]
            Err(error) => unreachable!("pin {}: {error}", self.name),
        }
        let Self {
            name,
            url,
            sha256,
            dir,
            files,
        } = self;
        let root = harness_dir().join(&valid(name));
        let mut recipe = format!(
            "FROM {BUSYBOX} AS {name}\nADD --checksum={sha256} {url} {TARBALL}\nRUN tar -x -z -f \
             {TARBALL} -C /"
        );
        for file in *files {
            let _ = write!(recipe, " {dir}/{}", file.path);
        }
        // A `COPY` gives each file and directory it copies the permission bits it has where it
        // is copied from, unless `--chmod` gives them all one, but the directories it makes for
        // its destination get bits that differ from engine to engine. So a second stage puts
        // the files in the tree the image gets, and gives each file and directory its mode, and
        // one `COPY` copies that tree to `/`, which every image has.
        let chmods: Vec<String> = (modes(&root, files).into_iter())
            .map(|(bits, paths)| {
                let paths: Vec<String> = paths.iter().map(|path| format!("{OUT}{path}")).collect();
                format!("chmod {bits:04o} {}", paths.join(" "))
            })
            .collect();
        let _ = write!(
            recipe,
            "\nFROM {BUSYBOX} AS {name}-tree\nCOPY --from={name} /{dir}/ {OUT}{root}/\nRUN {}\nFROM \
             {base}\nCOPY --from={name}-tree --chown=0:0 {OUT}/ /\n",
            chmods.join(" && ")
        );
        recipe
    }

    /// Fails, saying what is wrong, unless each field is as its doc asks, and `base` as
    /// [`Pin::recipe`]'s doc does.
    fn check(&self, base: &str) -> Result<(), String> {
        let named = self
            .name
            .starts_with(|char: char| char.is_ascii_lowercase())
            && (self.name.bytes()).all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
            });
        if !named {
            return Err(format!(
                "the name `{}` is not a harness's name: use a-z, 0-9, `.`, `_` and `-`, starting \
                 with a letter",
                self.name.escape_debug()
            ));
        }
        let fetched =
            (self.url.strip_prefix("https://")).is_some_and(|rest| plain(rest, b"-._~:/@%+"));
        if !fetched {
            return Err(format!(
                "the URL `{}` is not one a pin can hold: use `https://`, then ASCII letters and \
                 digits and `-._~:/@%+`",
                self.url.escape_debug()
            ));
        }
        Sha256Digest::try_from(self.sha256.to_owned())?;
        path(self.dir)?;
        for file in self.files {
            path(file.path)?;
        }
        if self.files.is_empty() {
            return Err("the pin keeps no file: list the files it keeps".to_owned());
        }
        if let Some(pair) = (self.files.windows(2)).find(|pair| pair[0].path >= pair[1].path) {
            return Err(format!(
                "`{}` comes after `{}`: list the files once each, sorted by their paths' bytes",
                pair[1].path, pair[0].path
            ));
        }
        if base.is_empty() || !plain(base, b"-._/:@") {
            return Err(format!(
                "the base `{}` is not an image reference a recipe can hold: use ASCII letters and \
                 digits and `-._/:@`",
                base.escape_debug()
            ));
        }
        Ok(())
    }
}

/// The path of each file of `files` at `root`, and of each directory that holds one, down from
/// the one below `/`, by the permission bits the recipe gives it: [`DIRECTORY`]'s, or the file's
/// mode's.
fn modes(root: &AbsolutePath, files: &[PinnedFile]) -> BTreeMap<u32, BTreeSet<String>> {
    let mut modes: BTreeMap<u32, BTreeSet<String>> = BTreeMap::new();
    for file in files {
        let path = root.join(&valid(file.path)).to_string();
        // Every `/` but the first, which is `/` itself, ends a directory's path.
        for (end, _) in path.match_indices('/').skip(1) {
            let dirs = modes.entry(DIRECTORY).or_default();
            dirs.insert(path[..end].to_owned());
        }
        modes.entry(file.mode.bits()).or_default().insert(path);
    }
    modes
}

/// Fails unless `path` is a relative path of ASCII letters and digits and `-._/`, which a
/// recipe holds as it is.
fn path(path: &str) -> Result<(), String> {
    RelativePath::try_from(path.to_owned())?;
    if plain(path, b"-._/") {
        Ok(())
    } else {
        Err(format!(
            "the path `{}` is not one a pin can hold: use ASCII letters and digits and `-._/`",
            path.escape_debug()
        ))
    }
}

/// Whether `text` holds only ASCII letters and digits and the bytes of `others`, none of which a
/// recipe quotes, escapes, expands or splits at.
fn plain(text: &str, others: &[u8]) -> bool {
    (text.bytes()).all(|byte| byte.is_ascii_alphanumeric() || others.contains(&byte))
}

/// `path`, a relative path [`Pin::check`] has accepted, as the type it is.
fn valid(path: &str) -> RelativePath {
    match RelativePath::try_from(path.to_owned()) {
        Ok(path) => path,
        #[expect(
            clippy::unreachable,
            reason = "`Pin::check` accepted the pin's name, which starts with a letter and holds \
                      no `/`, and each file's path, which is a `RelativePath`"
        )]
        Err(error) => unreachable!("{error}"),
    }
}
