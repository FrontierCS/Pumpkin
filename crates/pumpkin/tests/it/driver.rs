//! The driver of a run, the `pumpkin run` that records it: a run whose driver ends before it does
//! is lost. The test is on Docker, and ignored, as the Docker provider's tests are.

use std::process::{Child, Stdio};
use std::{env, fs};

use docker::{ContextName, Engine};
use expect_test::expect;
use provider::Provider as _;
use schema::provider::RunTag;

use super::{Scratch, check, image, poll, sandboxd, workflow};

#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn lost_on_docker() {
    let scratch = Scratch::new("lost");
    let sandboxd = sandboxd();
    let context = env::var("PUMPKIN_DOCKER_CONTEXT").unwrap_or_else(|_| "default".to_owned());
    let engine = Engine::connect(ContextName::try_from(context.clone()).unwrap()).unwrap();
    let image = image(&engine);
    scratch.write("sleeps.json", &workflow(&image, &["sleep", "300"]));
    let mut run = scratch.pumpkin(&["run", "sleeps.json", "--docker-context", &context]);
    run.args(["--sandboxd", &sandboxd, "--runs", "runs"]);
    let run = run.stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    let mut killed = Killed {
        run: run.unwrap(),
        engine: &engine,
        id: None,
    };
    // A record appears locked, so its directory is the run's.
    let record = scratch.0.join("runs");
    let id = poll(|| {
        let entries = fs::read_dir(&record).ok()?;
        entries
            .filter_map(|entry| entry.unwrap().file_name().into_string().ok())
            .find(|name| !name.starts_with('.'))
    });
    killed.id = Some(id.clone());
    let tag = RunTag::try_from(id.clone()).unwrap();
    // Once a sandbox is listed, the run's `docker` children have run: they must hold no lock.
    poll(|| Some(()).filter(|()| !engine.list(&tag).unwrap().is_empty()));
    killed.run.kill().unwrap();
    killed.run.wait().unwrap();
    let wait = scratch.pumpkin(&["wait", &id, "--runs", "runs"]);
    check(
        &scratch,
        [wait],
        false,
        &[(&id, "<id>")],
        &expect![[r#"
        $ pumpkin wait <id> --runs runs
        exit status: Some(1)
        pumpkin: run <id> is lost: its driver ended without writing outcome.json, so nothing says how it ended: remove what it left, which is its containers, labelled `pumpkin.run=<id>`, and its socket directory, `pumpkin-<id>` in the temporary directory"#]],
    );
}

/// A `pumpkin run` to kill, and what it leaves on Docker to remove, once the test is over,
/// however it ends: the containers of the run `id` and its socket directory.
struct Killed<'a> {
    run: Child,
    engine: &'a Engine,
    id: Option<String>,
}

impl Drop for Killed<'_> {
    fn drop(&mut self) {
        let _ = self.run.kill();
        let _ = self.run.wait();
        let Some(id) = self.id.take() else { return };
        let _ = fs::remove_dir_all(env::temp_dir().join(format!("pumpkin-{id}")));
        let Ok(tag) = RunTag::try_from(id) else {
            return;
        };
        if let Ok(listed) = self.engine.list(&tag) {
            let ids: Vec<_> = listed.into_iter().map(|listed| listed.id).collect();
            let _ = self.engine.delete(&tag, &ids);
        }
    }
}
