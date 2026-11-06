//! Taking a ported task from its directory through [`Task::load`]: the task a directory's name
//! gives, and the directories the factory refuses.

use std::path::Path;

use expect_test::{Expect, expect};
use pumpkin_frontier_cs::Task;

/// Loads the task in each of `dirs`, and compares what each gives, the task or why the factory
/// refuses it, with `expect`.
fn check(dirs: &[&str], expect: &Expect) {
    let outcomes: Vec<String> = (dirs.iter())
        .map(|dir| match Task::load(Path::new(dir)) {
            Ok(task) => format!("`{dir}`: {task:?}"),
            Err(error) => format!("`{dir}`: {error}"),
        })
        .collect();
    expect.assert_eq(&outcomes.join("\n"));
}

/// A task's problem id is its directory's name, read from the path alone, and a path whose last
/// name isn't a problem id is refused.
#[test]
fn load() {
    check(
        &["tasks/frontier-cs/tiny", "tiny/", "LWE", "tiny/.."],
        &expect![[r#"
            `tasks/frontier-cs/tiny`: Task { dir: "tasks/frontier-cs/tiny", problem_id: Name("tiny") }
            `tiny/`: Task { dir: "tiny/", problem_id: Name("tiny") }
            `LWE`: LWE: the directory's name, the task's problem id: `LWE` is not a valid name: use 1 to 64 characters from a-z, 0-9, `_` and `-`, starting with a letter
            `tiny/..`: `tiny/..` doesn't end in a directory's name: give the task's directory by a path that ends in its name, the task's problem id"#]],
    );
}
