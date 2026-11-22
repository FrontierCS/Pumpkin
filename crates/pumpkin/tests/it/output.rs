//! What `pumpkin` prints to a standard output that is a non-blocking pipe, as a process that shares
//! its open file description may make it: an `ssh` given it as its standard error does.

use std::io::{self, Read as _};
use std::process::Stdio;

use expect_test::{Expect, expect};
use rustix::fs::{OFlags, fcntl_setfl};
use rustix::io::ioctl_fionread;
use rustix::pipe::fcntl_setpipe_size;

use super::{Scratch, poll};

/// Runs `pumpkin` with `args` and its standard output on a full non-blocking pipe, which is read
/// only once full, and compares its exit status and standard error with `expect`.
///
/// The command prints more than a page in one call, which fills a pipe of one page whole.
fn check(args: &[&str], expect: &Expect) {
    let scratch = Scratch::new();
    let whole = scratch.pumpkin(args).output().unwrap();
    assert!(whole.status.success(), "{whole:?}");
    let (mut reader, writer) = io::pipe().unwrap();
    // Linux rounds a pipe's size up to its smallest, one page.
    let capacity = fcntl_setpipe_size(&writer, 1).unwrap();
    assert!(whole.stdout.len() > capacity);
    fcntl_setfl(&writer, OFlags::NONBLOCK).unwrap();
    let mut pumpkin = scratch.pumpkin(args);
    pumpkin.stdout(writer).stderr(Stdio::piped());
    let mut child = pumpkin.spawn().unwrap();
    // The command holds a copy of the pipe's write end, which would keep the pipe from its end.
    drop(pumpkin);
    let full = || ioctl_fionread(&reader).unwrap() == u64::try_from(capacity).unwrap();
    // A `pumpkin` that ends before the pipe is full fails at the snapshot, not at the bound.
    poll(|| (full() || child.try_wait().unwrap().is_some()).then_some(()));
    let mut stdout = Vec::new();
    reader.read_to_end(&mut stdout).unwrap();
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    expect.assert_eq(&format!(
        "exit status: {:?}\n{stderr}",
        output.status.code()
    ));
    assert!(stdout == whole.stdout, "printed {} bytes", stdout.len());
}

#[test]
fn waits_while_the_pipe_is_full() {
    check(
        &["help", "config"],
        &expect![[r#"
        exit status: Some(0)
    "#]],
    );
}
