//! `stdx`'s helpers: `process::wait_any` waits for whichever of two children exits first.

use std::process::Stdio;

use expect_test::expect;
use stdx::process::Environment;

#[test]
fn first_exited() {
    let spawn = |script: &str| {
        let mut command = stdx::process::command("sh", Environment::Inherited);
        command.args(["-c", script]).stdin(Stdio::piped());
        command.spawn().unwrap()
    };
    // `cat` exits only once its input closes.
    let (mut waiting, mut exiting) = (spawn("cat >/dev/null; exit 4"), spawn("exit 3"));
    let mut shown = Vec::new();
    let (index, status) = stdx::process::wait_any(&mut [&mut waiting, &mut exiting]);
    shown.push(format!("child {index}: {}", status.unwrap()));
    drop(waiting.stdin.take());
    let (index, status) = stdx::process::wait_any(&mut [&mut waiting]);
    shown.push(format!("child {index}: {}", status.unwrap()));
    expect![[r#"
        child 1: exit status: 3
        child 0: exit status: 4"#]]
    .assert_eq(&shown.join("\n"));
}
