use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use expect_test::expect;
use stdx::io::Backend;
use test_support::world::World;

fn check<P: Read + Write + Send + 'static>(
    backend: impl Fn() -> Box<dyn Backend>,
    connect: impl Fn(&Path) -> io::Result<P>,
) {
    let outcomes = test_support::io::outcomes(backend, connect);
    let expected = expect![[r#"
        an accept, then a recv and a send:
            accepted
            received "ping"
            sent "pong"
            the peer read "pong"
        a recv that waits, beside a stream whose recvs never wait:
            it waits while its peer sends nothing
            once its peer sends, it received "hi", as the other stream had more to receive
        a recv at the peer's end of the stream:
            received "bye", then 0 bytes
        a shutdown with a recv in flight:
            the recv in flight received 0 bytes
            a later recv received 0 bytes
            the peer read 0 bytes
        a close with an accept and a recv in flight:
            the accept failed: ConnectionAborted
            the recv failed: ConnectionAborted
        a send to a peer that doesn't read:
            the first send of 1048576 bytes sent a part of them
            the rest waits while the peer doesn't read
            once it reads, the rest goes, and the peer reads every byte as sent
        a send to a peer that reads on a thread of its own:
            the peer reads every byte as sent
        a file's sync, whose callback syncs it again:
            its sync of the data completed
            the sync of all that its callback submitted completed
            the file holds "hello"
        a next tick, whose callback submits another:
            ran in batch 1
            the tick that it submitted ran in batch 2
        events from other threads:
            7 sent through `Events`: batch 1 took 7
            8 sent through an `Edge`: batch 2 took 8
            an `Edge` dropped unsent: batch 3 took nothing
        a file's lock, which another open file of its path tries for:
            the first file took it
            the first file again took it
            the second file found it held
            the second file, once the first closed, took it
        a timeout:
            ran at its time or later
    "#]];
    expected.assert_eq(&outcomes);
}

#[test]
fn machine() {
    check(
        || stdx::io::machine().unwrap(),
        |path| UnixStream::connect(path),
    );
}

#[test]
fn world() {
    let world = World::new();
    check(|| world.backend(), |path| world.connect(path));
}

#[test]
fn world_clock_script_and_peers() {
    expect![[r#"
        a sync that fails: at 0s, the sync of failed failed: Other
        a sync due at 10s, in a wait until 4s: at 4s, nothing
        then in a wait until 20s: at 10s, the sync of late completed
        a write that waited until 10s: at 10s, the recv of "after 10s"
        a sync due at 12s, with an event owed: at 10s, nothing
        the edge's event: at 10s, took 1
        then: at 12s, the sync of owed completed
        a write that moves time to 30s: at 30s, the recv of "moved"
        a send once the peer reads no more: at 30s, the send failed: BrokenPipe
        the peer's shutdown, then its drop: at 30s, the recv of 0 bytes
        an event: at 30s, the sync of counted completed, took 2
        a call timed at 45s, which sends 3: at 45s, took 3
        a held sync, once the world reached 50s: at 50s, nothing
        once its function says so: at 50s, the sync of held completed
        a stream that the loop held as its backend dropped: its peer read 0 bytes
    "#]]
    .assert_eq(&test_support::io::world_outcomes());
}
