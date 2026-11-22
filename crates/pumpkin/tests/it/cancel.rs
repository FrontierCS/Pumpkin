//! `cancel` of a run that has ended, or is lost: it removes what the run left, and says how it
//! ended.

use expect_test::expect;
use serde_json::json;

use super::{Scratch, Step, check, ran, write_docker_record, write_record};

const LOST: &str = "20261108-120000-0b000001";

const FAILED: &str = "20261108-120100-0b000002";

/// A record made before records were locked.
const UNLOCKED: &str = "20261108-120200-0b000003";

/// A lost run whose Docker provider cannot be reached.
const UNREACHED: &str = "20261108-120300-0c000004";

#[test]
fn cancels() {
    let scratch = Scratch::new();
    write_record(&scratch, LOST, true, None);
    let failed = json!({ "state": "failed", "reason": "r" });
    write_record(&scratch, FAILED, true, Some(failed));
    write_record(&scratch, UNLOCKED, false, None);
    write_docker_record(&scratch, UNREACHED, "recorded", None);
    let mut unreached = scratch.pumpkin(&["cancel", UNREACHED, "--runs", "runs"]);
    unreached.env("PATH", "");
    let cancel = |run| {
        ran(
            scratch.pumpkin(&["cancel", run, "--runs", "runs"]),
            &scratch.0,
            false,
        )
    };
    let steps = [
        cancel(LOST),
        cancel(FAILED),
        cancel("0b0000"),
        cancel("nothing"),
        cancel(UNLOCKED),
        ran(unreached, &scratch.0, false),
    ];
    check(
        &scratch,
        steps.map(Step::Ran),
        false,
        &[],
        &expect![[r#"
            $ pumpkin cancel 20261108-120000-0b000001 --runs runs
            exit status: Some(0)
            pumpkin: run 20261108-120000-0b000001 was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run 20261108-120000-0b000001 failed: its driver ended without an outcome; its record is <scratch>/runs/20261108-120000-0b000001
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            $ pumpkin cancel 20261108-120100-0b000002 --runs runs
            exit status: Some(0)
            pumpkin: run 20261108-120100-0b000002 failed: r; its record is <scratch>/runs/20261108-120100-0b000002
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            $ pumpkin cancel 0b0000 --runs runs
            exit status: Some(1)
            pumpkin: `0b0000` matches 3 runs: name one by more of its id
              20261108-120000-0b000001
              20261108-120100-0b000002
              20261108-120200-0b000003
            $ pumpkin cancel nothing --runs runs
            exit status: Some(1)
            pumpkin: no run in runs matches `nothing`: name an id, a unique prefix of an id or of its random digits, or a record's path
            $ pumpkin cancel 20261108-120200-0b000003 --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/20261108-120200-0b000003/driver.pid isn't there: expected the record of a run to have it
            $ PATH= pumpkin cancel 20261108-120300-0c000004 --runs runs
            exit status: Some(3)
            pumpkin: run 20261108-120300-0c000004's sandboxes on provider `local`, if it left any, can't be removed, as it can't be reached: the Docker context `recorded` can't be read: `docker` didn't run: No such file or directory (os error 2): install Docker's command line on `PATH`: name one that `docker context ls` lists; once it can be, `pumpkin reap --runs <scratch>/runs` removes them, and if it is gone for good, remove the record, <scratch>/runs/20261108-120300-0c000004, to stop reaping it
            pumpkin: run 20261108-120300-0c000004 was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run 20261108-120300-0c000004 failed: its driver ended without an outcome; its record is <scratch>/runs/20261108-120300-0c000004
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            pumpkin: to remove what it left once its provider can be reached: pumpkin reap --runs <scratch>/runs"#]],
    );
}
