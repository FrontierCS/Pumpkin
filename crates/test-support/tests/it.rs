use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use expect_test::expect;
use stdx::io::Backend;

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
