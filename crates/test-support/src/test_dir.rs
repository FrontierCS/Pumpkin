use std::fs::{self, DirBuilder, Permissions};
use std::io::{self, ErrorKind};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::{env, thread};

use sha2::{Digest as _, Sha256};

/// The calling test's own directory, empty and private to this user, in the system's temporary
/// directory, at a short path that names this checkout and the test.
///
/// A socket of a short name below it fits the 107 bytes of a socket's path. Making it removes the
/// test's last one, and prints where it is, which a failed test shows. Dropping it removes it, or
/// fails the test if it can't, but keeps a failed test's for a look.
///
/// A test holds one at a time, made on the thread that the harness names after the test. Two runs
/// of one test in one checkout at once share it, and wipe each other's.
pub struct TestDir(PathBuf);

impl TestDir {
    /// # Panics
    ///
    /// Panics if the thread is no test's, if neither `cargo test` nor `cargo nextest` runs the
    /// test, or if the directory can't be made.
    #[expect(
        clippy::print_stderr,
        reason = "a failed test shows where its directory is"
    )]
    #[expect(
        clippy::new_without_default,
        reason = "making one removes what the test's last run left"
    )]
    #[expect(
        clippy::disallowed_methods,
        reason = "it is the scratch in the system's temporary directory that the ban leads to"
    )]
    pub fn new() -> Self {
        // As Cargo's tests have theirs, but at a path no longer than a `tempfile` directory's, such
        // as `/tmp/route-XXXXXX`, so that the tests' sockets fit below it; Cargo's is in `target`,
        // and named by the test's path.
        // https://github.com/rust-lang/cargo/blob/111443f3dc4d82a65e2306a93028f122406401c1/crates/cargo-test-support/src/paths.rs#L64-L77
        // Each name is a hash, of 25 bits for the checkout and 30 for the test, so that two of
        // some hundreds share one about once in a thousand.
        let root = env::temp_dir().join(short_hash(env!("CARGO_MANIFEST_DIR"), 5));
        // Any local user can know its name and make it first, so it must be private, as tmux's
        // socket directory must.
        // https://github.com/tmux/tmux/blob/a73616d781f2af13085691c741cfb0780d365328/tmux.c#L271-L288
        if let Err(error) = DirBuilder::new().mode(0o700).create(&root)
            && error.kind() != ErrorKind::AlreadyExists
        {
            panic!(
                "the tests' directory {} can't be made: {error}",
                root.display()
            );
        }
        let metadata = fs::symlink_metadata(&root).unwrap();
        assert!(
            metadata.is_dir() && metadata.mode() & 0o777 == 0o700,
            "the tests' directory {} is no directory of mode 700: expected one that its owner \
             alone may enter",
            root.display()
        );
        let dir = root.join(short_hash(&test_name(), 6));
        if let Err(error) = wipe(&dir) {
            panic!(
                "the test's last directory {} can't be removed: {error}",
                dir.display()
            );
        }
        DirBuilder::new().mode(0o700).create(&dir).unwrap();
        eprintln!("the test's directory is {}", dir.display());
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if !thread::panicking()
            && let Err(error) = wipe(&self.0)
        {
            panic!(
                "the test's directory {} can't be removed: {error}",
                self.0.display()
            );
        }
    }
}

/// The calling test's package, test binary and name, which name it in a checkout.
fn test_name() -> String {
    // Each test runs on a thread of its own, named after it, as libtest runs it, under nextest too.
    let Some(test) = thread::current().name().map(str::to_owned) else {
        panic!("the thread has no name: expected a test's, which libtest names after the test");
    };
    let Ok(package) = env::var("CARGO_PKG_NAME") else {
        panic!(
            "`CARGO_PKG_NAME` is unset: expected the test run by `cargo test` or `cargo nextest`"
        );
    };
    let exe = env::current_exe().unwrap();
    let exe = exe.file_name().unwrap().to_string_lossy();
    // A test binary's file is its name, then `-` and a hash.
    let (binary, _) = exe.rsplit_once('-').unwrap();
    format!("{package} {binary} {test}")
}

/// A character of 32 for each of the first `chars` bytes of `text`'s SHA-256.
fn short_hash(text: &str, chars: usize) -> String {
    const DIGITS: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let digest = Sha256::digest(text);
    (digest.iter().take(chars))
        .map(|byte| char::from(DIGITS[usize::from(byte % 32)]))
        .collect()
}

/// Removes `dir`, if there is one, and all below it, opening to this user each directory below it
/// first if one's mode keeps it from being emptied.
fn wipe(dir: &Path) -> io::Result<()> {
    // As Cargo's test support does on a refusal, but for all below, as the error doesn't say where.
    // https://github.com/rust-lang/cargo/blob/111443f3dc4d82a65e2306a93028f122406401c1/crates/cargo-test-support/src/paths.rs#L357-L383
    match fs::remove_dir_all(dir) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            open(dir)?;
            fs::remove_dir_all(dir)
        }
        removed => removed,
    }
}

/// Gives this user every permission on `dir` and on each directory below it.
fn open(dir: &Path) -> io::Result<()> {
    fs::set_permissions(dir, Permissions::from_mode(0o700))?;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            open(&entry.path())?;
        }
    }
    Ok(())
}
