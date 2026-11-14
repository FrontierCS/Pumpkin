//! The engine, against a real Docker daemon. The tests that need Docker are ignored.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]
#![expect(
    clippy::print_stderr,
    reason = "a test prints the image tags it can't remove"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::OnceLock;

use engine::{BuildContext, Built, ContextName, Engine, Recipe, Report};
use expect_test::{Expect, expect};
use recipe::Given;
use schema::ir::Image;
use schema::sandbox::RelativePath;
use stdx::process::Environment;
use tempfile::TempDir;
use test_support::{BUSYBOX, docker};

/// The scratch of one test: a directory for its builds' contexts.
struct Fixture {
    dir: TempDir,
}

/// Runs `scenario` against a new fixture, and checks what it reported against `expect`.
fn check(name: &str, scenario: impl FnOnce(&Fixture) -> Vec<String>, expect: &Expect) {
    let dir = tempfile::Builder::new()
        .prefix(&format!("engine-{name}-"))
        .tempdir()
        .unwrap();
    expect.assert_eq(&scenario(&Fixture { dir }).join("\n"));
}

/// The engine the tests reach Docker through, by the context `PUMPKIN_DOCKER_CONTEXT` names,
/// or `default`, or why it can't be reached.
fn connected() -> &'static Result<Engine, String> {
    static ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let context = ContextName::try_from(test_support::docker_context())?;
        Engine::connect(context)
    })
}

/// The engine the tests reach Docker through.
fn engine() -> &'static Engine {
    connected().as_ref().unwrap()
}

fn relative(path: &str) -> RelativePath {
    RelativePath::try_from(path.to_owned()).unwrap()
}

/// `message`, with the fixture's directory as `<dir>` and the tests' context as `<context>`.
fn redact(fixture: &Fixture, message: &str) -> String {
    let context = format!("context `{}`", engine().report().context.as_str());
    (message.replace(&fixture.dir.path().display().to_string(), "<dir>"))
        .replace(&context, "context `<context>`")
}

// What the engine reports of itself before anything starts, as far as every engine shows the
// same, and how it refuses an empty context name, and names no context has, among them
// `--help`, which `docker context inspect` mustn't take for its own option, which prints its
// help and succeeds.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn report() {
    let Report {
        context: _,
        endpoint,
        client_version,
        engine_version,
        security: _,
        kernel,
        os,
    } = engine().report();
    let named = [endpoint, client_version, engine_version, kernel, os];
    let mut lines = vec![
        format!(
            "endpoint, versions, kernel and OS named: {}",
            named.iter().all(|named| !named.is_empty())
        ),
        ContextName::try_from(String::new()).unwrap_err(),
    ];
    for name in ["pumpkin-missing", "--help"] {
        let context = ContextName::try_from(name.to_owned()).unwrap();
        let refused = Engine::connect(context).unwrap_err();
        let (wrong, _) = refused.split_once(": ").unwrap();
        let (_, todo) = refused.rsplit_once(": ").unwrap();
        lines.push(format!("{wrong}: …: {todo}"));
    }
    expect![[r#"
        endpoint, versions, kernel and OS named: true
        the Docker context's name is empty: name one that `docker context ls` lists
        the Docker context `pumpkin-missing` can't be read: …: name one that `docker context ls` lists
        the Docker context `--help` can't be read: …: name one that `docker context ls` lists"#]]
    .assert_eq(&lines.join("\n"));
}

// A build is tagged by its inputs, and found by that tag rather than built again: another
// file's content, another mode or another recipe gives another tag, and the same inputs give
// the image their tag names, even one another build made. A recipe, here a text, may build on
// an image it is given as a named context, and is refused without it; and it gets that image
// even once the image's tag has moved to another. A context that can't name its inputs is
// refused before anything is built, and so is a repository that Docker can't tag, whose tag
// pins the digest's text of a build given nothing. A build removes what a build of a process
// that has ended left in the system's temporary directory, but not a running one's.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn builds() {
    check(
        "builds",
        |fixture| {
            let mut builds = Builds::new(fixture);
            let dir = fixture.dir.path().join("context");
            // What a build made for a process that has ended, whose ID is never one, and for one
            // that runs, the first.
            let pid_max = fs::read_to_string("/proc/sys/kernel/pid_max").unwrap();
            let left = |pid: &str| std::env::temp_dir().join(format!("pumpkin-build-{pid}-0"));
            let (ended, running) = (left(pid_max.trim()), left("1"));
            for dir in [&ended, &running] {
                fs::create_dir_all(dir.join("context")).unwrap();
            }
            let mut lines = vec![format!("built: {}", builds.build("Dockerfile"))];
            lines.push(format!(
                "left by an ended process, then: {}; by a running one: {}",
                ended.exists(),
                running.exists()
            ));
            fs::remove_dir_all(&running).unwrap();
            let [first] = <[String; 1]>::try_from(builds.tags(&builds.seen[0])).unwrap();
            run(&["image", "rm", &first]);
            put(&dir, "file", "two", 0o644);
            lines.push(format!("another content: {}", builds.build("Dockerfile")));
            let second = builds.seen[1].clone();
            run(&["tag", &second, &first]);
            put(&dir, "file", "one", 0o644);
            let found = builds.build("Dockerfile");
            lines.push(format!("the first inputs, their tag moved to B: {found}"));
            put(&dir, "file", "one", 0o755);
            lines.push(format!("another mode: {}", builds.build("Dockerfile")));
            lines.push(format!(
                "another recipe: {}",
                builds.build("other/Dockerfile")
            ));
            let (context, dockerfile) = (builds.context(), Recipe::File(relative("Dockerfile")));
            let base = engine().build(&context, &dockerfile, &Given::default(), &builds.repository);
            let base = base.unwrap();
            let on_base = Recipe::Text("FROM base\nCOPY file /on-base\n".to_owned());
            let given = Given {
                contexts: BTreeMap::from([("base".to_owned(), &base)]),
                ..Given::default()
            };
            let built = builds.build_in(&context, &on_base, &given);
            lines.push(format!("a text on its base: {built}"));
            let refused = builds.build_in(&context, &on_base, &Given::default());
            lines.push(format!("without its base: {refused}"));
            // A recipe of its base alone builds that very image.
            run(&["tag", &second, base.tag()]);
            let alone = Recipe::Text("FROM base\n".to_owned());
            let built = builds.build_in(&context, &alone, &given);
            lines.push(format!(
                "its base alone, the base's tag moved to B: {built}"
            ));
            lines.push(format!(
                "an absent recipe: {}",
                builds.build("absent/Dockerfile")
            ));
            std::os::unix::fs::symlink("file", dir.join("link")).unwrap();
            lines.push(format!("a symbolic link: {}", builds.build("Dockerfile")));
            fs::remove_file(dir.join("link")).unwrap();
            for (what, name) in [("a space", "a b"), ("a tab", "a\tb")] {
                put(&dir, name, "", 0o644);
                lines.push(format!(
                    "a name with {what}: {}",
                    builds.build("Dockerfile")
                ));
                fs::remove_file(dir.join(name)).unwrap();
            }
            // Without `id`, the inputs are the same in every process, so the tag the refusal
            // names pins the digest's text; the lookup fails before anything is built.
            fs::remove_file(dir.join("id")).unwrap();
            let repository = "pumpkin-docker-tests/Builds";
            let uppercase = engine().build(&context, &dockerfile, &Given::default(), repository);
            let refused = redact(fixture, &uppercase.unwrap_err());
            lines.push(format!("an uppercase repository: {refused}"));
            lines
        },
        &expect![[r#"
            built: A, tagged :1
            left by an ended process, then: false; by a running one: true
            another content: B, tagged :2
            the first inputs, their tag moved to B: B, tagged :1 :2
            another mode: C, tagged :3
            another recipe: C, tagged :3 :4
            a text on its base: D, tagged :5
            without its base: Dockerfile:1: `base` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a named context the build is given
            its base alone, the base's tag moved to B: C, tagged :4 :6
            an absent recipe: `absent/Dockerfile` is no regular file in the build's context: name the build's recipe by its path there
            a symbolic link: <dir>/context/link is neither a directory nor a regular file: give a build's context only those
            a name with a space: `<dir>/context/a b` holds whitespace or a control character: name a build context's files without them
            a name with a tab: `<dir>/context/a\tb` holds whitespace or a control character: name a build context's files without them
            an uppercase repository: the image tagged `pumpkin-docker-tests/Builds:700c07c0f69ca70bceeae61d7f8c0fb3d3bc44e9e3f4d8a084cf9f706de50dc9` can't be looked up: `docker image inspect` failed with exit status: 1: Error response from daemon: invalid reference format: repository name (pumpkin-docker-tests/Builds) must be lowercase: fix what Docker reports"#]],
    );
}

// A directory mounted in a build's context is there for its recipe to copy from, and its files
// are in the build's inputs. One to be mounted where the context holds something, or holds no
// directory, or at a path with whitespace, is refused before anything is built.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn mounts() {
    check(
        "mounts",
        |fixture| {
            let mut builds = Builds::new(fixture);
            let shared = fixture.dir.path().join("shared");
            put(&shared, "tool", "one", 0o755);
            let dir = builds.context().dir;
            let at = |path: &str| BuildContext {
                dir: dir.clone(),
                mounts: BTreeMap::from([(relative(path), shared.clone())]),
            };
            let recipe = Recipe::Text("FROM scratch\nCOPY file id shared/tool /\n".to_owned());
            let built = builds.build_in(&at("shared"), &recipe, &Given::default());
            let mut lines = vec![format!("mounted: {built}")];
            put(&shared, "tool", "two", 0o755);
            let built = builds.build_in(&at("shared"), &recipe, &Given::default());
            lines.push(format!("a mounted file changed: {built}"));
            for (what, path) in [
                ("over a file", "file"),
                ("in a file", "file/shared"),
                ("in nothing", "absent/shared"),
                ("at a name with a space", "a b"),
            ] {
                let refused = builds.build_in(&at(path), &recipe, &Given::default());
                lines.push(format!("mounted {what}: {refused}"));
            }
            lines
        },
        &expect![[r#"
            mounted: A, tagged :1
            a mounted file changed: B, tagged :2
            mounted over a file: the build's context holds `file` already, where <dir>/shared is to be mounted: mount it where the context holds nothing
            mounted in a file: the build's context holds no directory `file` for <dir>/shared to be mounted in: mount it in a directory the context holds
            mounted in nothing: the build's context holds no directory `absent` for <dir>/shared to be mounted in: mount it in a directory the context holds
            mounted at a name with a space: `a b` holds whitespace or a control character: mount <dir>/shared at a path without them"#]],
    );
}

// A recipe's `RUN` step may bind-mount the build's context, whose files are in the build's
// inputs, so another content of a file it mounts gives another tag, and another image, which
// holds what was mounted; and it may bind-mount a named context it is given, whose tag is in the
// inputs too, so another image there gives another tag.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn bind_mounts() {
    check(
        "bind-mounts",
        |fixture| {
            let mut builds = Builds::new(fixture);
            let dir = fixture.dir.path().join("context");
            let context = builds.context();
            let bound = Recipe::Text(format!(
                "FROM {BUSYBOX}\nRUN --mount=source=file,target=/mnt/file \
                 --mount=type=bind,source=id,target=/mnt/id cat /mnt/file /mnt/id > /bound\n"
            ));
            let id = std::process::id();
            // Built only if the named context `bound`'s `/bound` holds `content`.
            let holds = |content: &str| {
                Recipe::Text(format!(
                    "FROM {BUSYBOX}\nRUN --mount=from=bound,source=/bound,target=/bound \
                     test \"$(cat /bound)\" = {content}{id}\n"
                ))
            };
            let mut lines = Vec::new();
            for content in ["one", "two", "one"] {
                put(&dir, "file", content, 0o644);
                let built = builds.build_in(&context, &bound, &Given::default());
                lines.push(format!("the bound file holds {content}: {built}"));
                // Found by its tag, as `build_in` has just built it.
                let image = engine().build(&context, &bound, &Given::default(), &builds.repository);
                let image = image.unwrap();
                let given = Given {
                    contexts: BTreeMap::from([("bound".to_owned(), &image)]),
                    ..Given::default()
                };
                let built = builds.build_in(&context, &holds(content), &given);
                lines.push(format!(
                    "  its image bound as a named context holds it: {built}"
                ));
            }
            lines
        },
        &expect![[r#"
            the bound file holds one: A, tagged :1
              its image bound as a named context holds it: B, tagged :2
            the bound file holds two: C, tagged :3
              its image bound as a named context holds it: D, tagged :4
            the bound file holds one: A, tagged :1
              its image bound as a named context holds it: B, tagged :2"#]],
    );
}

// A build's stage gives its files and directories git's modes, so files that differ only in
// other permission bits, as checkouts under other umasks do, have one tag; and the image holds
// them with those modes, a file its owner may execute still executable.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn modes() {
    check(
        "modes",
        |fixture| {
            let mut builds = Builds::new(fixture);
            let dir = fixture.dir.path().join("context");
            // Built only if the image holds each file with the mode the stage gives it.
            let recipe = Recipe::Text(format!(
                "FROM {BUSYBOX}\nCOPY . /context\nRUN cd /context && test \"$(stat -c %a file \
                 tool other other/Dockerfile | xargs)\" = '644 755 755 644'\n"
            ));
            let mut lines = Vec::new();
            for (what, [file, tool, dirs]) in [
                ("group-writable", [0o664, 0o775, 0o775]),
                ("for their owners alone", [0o600, 0o700, 0o700]),
                ("as git checks them out", [0o644, 0o755, 0o755]),
            ] {
                put(&dir, "file", "one", file);
                put(&dir, "tool", "", tool);
                for dir in [dir.join("other"), dir.clone()] {
                    fs::set_permissions(dir, fs::Permissions::from_mode(dirs)).unwrap();
                }
                let built = builds.build_in(&builds.context(), &recipe, &Given::default());
                lines.push(format!("{what}: {built}"));
            }
            lines
        },
        &expect![[r#"
            group-writable: A, tagged :1
            for their owners alone: A, tagged :1
            as git checks them out: A, tagged :1"#]],
    );
}

// A build's stage gives its files and directories one time, and the build gives it to its
// image's history, so builds of the same inputs that share no cache make the same layer of each
// copy of staged files into `/`, and the same image when its every layer is one: a registry that
// holds the first has the second. Another content of a file, of the same size, changes only the
// layer that copies it. A layer that a copy into a directory of the base makes is another in
// each build, as Docker gives that directory the build's own time. A `RUN` isn't given the
// build's time as `SOURCE_DATE_EPOCH`, which would change how Python compiles what it installs.
// Each image's layers, its base's too, are zstd's, in an OCI manifest.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn reproducible() {
    check(
        "reproducible",
        |fixture| {
            let mut builds = Builds::new(fixture);
            let dir = fixture.dir.path().join("context");
            put(&dir, "tree/deep/leaf", "leaf", 0o644);
            let context = builds.context();
            // With the directories that the `RUN` below mounts over, so that it makes no layer:
            // its own `RUN` leaves those that BuildKit mounts, `/proc` and `/sys`, made.
            let base = Recipe::Text(format!("FROM {BUSYBOX}\nRUN mkdir /nonce\n"));
            let base = engine().build(&context, &base, &Given::default(), &builds.repository);
            let base = base.unwrap();
            let given = Given {
                contexts: BTreeMap::from([("base".to_owned(), &base)]),
                ..Given::default()
            };
            // Each build mounts a `nonce` of its own in a `RUN` that changes nothing, so that each
            // step after it misses the cache, as on a machine of its own.
            let recipe = |last: &str| {
                Recipe::Text(format!(
                    "FROM {BUSYBOX} AS nonce\nCOPY nonce /\nFROM base\nRUN \
                     --mount=from=nonce,target=/nonce test -z \"${{SOURCE_DATE_EPOCH-}}\"\nCOPY \
                     file id /\nCOPY \
                     tree /tree\nWORKDIR /work\n{last}"
                ))
            };
            let (mut layers, mut types) = (Vec::new(), BTreeSet::new());
            let mut lines = Vec::new();
            for (nonce, what, file, last) in [
                (1, "built", "one", ""),
                (2, "built again", "one", ""),
                (3, "a copied file changed", "two", ""),
                (
                    4,
                    "copied into a directory of the base",
                    "two",
                    "COPY file /etc/\n",
                ),
                (5, "  built again", "two", "COPY file /etc/\n"),
            ] {
                put(&dir, "nonce", &nonce.to_string(), 0o644);
                put(&dir, "file", file, 0o644);
                let shown = builds.build_in(&context, &recipe(last), &given);
                // Found by its tag, as `build_in` has just built it.
                let built = engine().build(&context, &recipe(last), &given, &builds.repository);
                let manifest = builds.manifest(built.unwrap().image());
                types.insert(manifest["mediaType"].as_str().unwrap().to_owned());
                let numbers: Vec<String> = (manifest["layers"].as_array().unwrap().iter())
                    .map(|layer| {
                        types.insert(layer["mediaType"].as_str().unwrap().to_owned());
                        let digest = layer["digest"].as_str().unwrap();
                        (1 + first_seen(&mut layers, digest)).to_string()
                    })
                    .collect();
                lines.push(format!("{what}: {shown}; layers {}", numbers.join(" ")));
            }
            let types: Vec<String> = types.into_iter().collect();
            lines.push(format!("of the media types: {}", types.join(", ")));
            lines
        },
        &expect![[r#"
            built: A, tagged :1; layers 1 2 3 4 5 6
            built again: A, tagged :1 :2; layers 1 2 3 4 5 6
            a copied file changed: B, tagged :3; layers 1 2 3 7 5 6
            copied into a directory of the base: C, tagged :4; layers 1 2 3 7 5 6 8
              built again: D, tagged :5; layers 1 2 3 7 5 6 9
            of the media types: application/vnd.oci.image.layer.v1.tar+zstd, application/vnd.oci.image.manifest.v1+json"#]],
    );
}

/// Runs `docker` with `args`, which must succeed.
fn run(args: &[&str]) {
    let output = docker().args(args).output().unwrap();
    assert!(output.status.success(), "{output:?}");
}

/// Writes `text` to the file at `path` in `dir`, with permission bits `mode`, and makes each
/// directory it needs with bits 0755, whatever this process's umask.
fn put(dir: &Path, path: &str, text: &str, mode: u32) {
    let file = dir.join(path);
    for parent in file.ancestors().skip(1) {
        if parent == dir.parent().unwrap() {
            break;
        }
        fs::create_dir_all(parent).unwrap();
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(&file, text).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
}

/// The images a test builds in `repository`, by their IDs in the order they were first built,
/// and the tags it has shown, in the order they were first shown. Dropping it removes each tag
/// the images have in `repository`, and so each image no other repository tags.
struct Builds<'f> {
    fixture: &'f Fixture,
    repository: String,
    seen: Vec<String>,
    shown: Vec<String>,
}

impl<'f> Builds<'f> {
    /// The images of a test that builds with `context`, in the fixture's directory, as its
    /// context: a recipe that copies `file`, which holds `one`, and `id`, which holds this
    /// process's ID, into an empty image, at `Dockerfile` and at `other/Dockerfile`; each in a
    /// repository of this process's own.
    fn new(fixture: &'f Fixture) -> Self {
        let dir = fixture.dir.path().join("context");
        let recipe = "FROM scratch\nCOPY file id /\n";
        put(&dir, "Dockerfile", recipe, 0o644);
        put(&dir, "other/Dockerfile", recipe, 0o644);
        put(&dir, "file", "one", 0o644);
        // An image is named by its content, so another process's test that built the same
        // inputs would have the same image, which removing its last tag here would delete, even
        // between that test's build and its `docker tag`. With this process's ID, the images
        // are its own.
        put(&dir, "id", &std::process::id().to_string(), 0o644);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            fixture,
            repository: format!("pumpkin-docker-tests/builds-{}", std::process::id()),
            seen: Vec::new(),
            shown: Vec::new(),
        }
    }

    /// The fixture's `context`, with nothing mounted in it.
    fn context(&self) -> BuildContext {
        BuildContext {
            dir: Some(self.fixture.dir.path().join("context")),
            mounts: BTreeMap::new(),
        }
    }

    /// [`Builds::build_in`] of the file `recipe` of the fixture's `context`, given nothing.
    fn build(&mut self, recipe: &str) -> String {
        let recipe = Recipe::File(relative(recipe));
        self.build_in(&self.context(), &recipe, &Given::default())
    }

    /// Builds `recipe` with `context`, given `given`, and shows the image, by the letter of its
    /// first build, and its tags in the repository, and the tag the build returned if it is
    /// none of them, each by the number of its first showing, as their digests cover `id`; or
    /// why the build failed.
    fn build_in(
        &mut self,
        context: &BuildContext,
        recipe: &Recipe,
        given: &Given<&Built>,
    ) -> String {
        let built = engine().build(context, recipe, given, &self.repository);
        let (image, returned) = match built {
            Ok(built) => (built.image().as_str().to_owned(), built.tag().to_owned()),
            Err(error) => return redact(self.fixture, &error),
        };
        let index = first_seen(&mut self.seen, &image);
        let letter = char::from(b'A' + u8::try_from(index).unwrap());
        // The returned tag first, so that a new tag's number doesn't rest on how tags sort.
        let returned = 1 + first_seen(&mut self.shown, &returned);
        let tags = self.tags(&image);
        let mut tags: Vec<usize> = (tags.iter())
            .map(|tag| 1 + first_seen(&mut self.shown, tag))
            .collect();
        tags.sort_unstable();
        let stray = (!tags.contains(&returned)).then(|| format!(", but returned :{returned}"));
        let tags: Vec<String> = tags.iter().map(|number| format!(":{number}")).collect();
        format!(
            "{letter}, tagged {}{}",
            tags.join(" "),
            stray.unwrap_or_default()
        )
    }

    /// The manifest of `image`, as `docker image save` writes it.
    fn manifest(&self, image: &Image) -> serde_json::Value {
        let saved = self.fixture.dir.path().join("saved");
        let _ = fs::remove_dir_all(&saved);
        fs::create_dir(&saved).unwrap();
        let archive = saved.join("image.tar");
        let output = docker()
            .args(["image", "save", "--output"])
            .args([archive.as_os_str(), OsStr::new(image.as_str())])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let mut tar = stdx::process::command("tar", Environment::Only(&["PATH"]));
        let output = (tar.arg("--extract").arg("--file").arg(&archive))
            .arg("--directory")
            .arg(&saved)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let hex = image.as_str().strip_prefix("sha256:").unwrap();
        let manifest = fs::read(saved.join("blobs/sha256").join(hex)).unwrap();
        serde_json::from_slice(&manifest).unwrap()
    }

    /// The tags `image` has in the repository, sorted, or none if the engine has no such image.
    fn tags(&self, image: &str) -> Vec<String> {
        let format = "{{json .RepoTags}}";
        let inspect = ["image", "inspect", "--format", format, image];
        let Ok(output) = docker().args(inspect).output() else {
            return Vec::new();
        };
        let tags: Vec<String> = serde_json::from_slice(&output.stdout).unwrap_or_default();
        let prefix = format!("{}:", self.repository);
        let mut tags: Vec<String> = (tags.into_iter())
            .filter(|tag| tag.starts_with(&prefix))
            .collect();
        tags.sort();
        tags
    }
}

impl Drop for Builds<'_> {
    // Nothing here panics, for the reason the fixture's drop gives; a tag that can't be
    // removed is printed.
    fn drop(&mut self) {
        for image in &self.seen {
            for tag in self.tags(image) {
                let removed = docker().args(["image", "rm", &tag]).output();
                if !removed.is_ok_and(|removed| removed.status.success()) {
                    eprintln!("the test's image tag {tag} can't be removed");
                }
            }
        }
    }
}

/// The index of `item` in `list`, which gets it at its end if it lacks it.
fn first_seen(list: &mut Vec<String>, item: &str) -> usize {
    if let Some(index) = list.iter().position(|listed| listed == item) {
        return index;
    }
    list.push(item.to_owned());
    list.len() - 1
}
