//! The options of a TCP connection that lasts as long as its two ends want it, may sit silent
//! for as long, and carries whole messages: [`set_options`].

use std::io;
use std::net::TcpStream;
use std::time::Duration;

use rustix::net::sockopt;

/// How long a connection sits idle before the kernel first probes its peer.
// Network address translators and load balancers forget a connection that has been silent for
// some minutes, after which neither end reaches the other through it, and neither is told. A
// probe this soon keeps such a path, at a few bytes a probe.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(15);

/// How long the kernel waits between probes.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// How many probes may go unanswered before the kernel ends the connection.
// With the idle wait and the interval, it lets an idle connection's path lose every packet for
// a couple of minutes and recover, while a peer whose machine or network is gone is let go
// within minutes. It is Linux's default too, and set so that a host's own default doesn't
// change that.
const KEEPALIVE_PROBES: u32 = 9;

/// Makes `stream` send each write at once, and end once its peer has been silent too long.
///
/// Silent means not answering even the kernel's probes for `KEEPALIVE_IDLE` +
/// `KEEPALIVE_INTERVAL` × `KEEPALIVE_PROBES`. While data awaits acknowledgement, the kernel
/// retransmits it instead, as long as its own limit allows: a caller bounds that by its deadline.
///
/// # Errors
///
/// Fails if the kernel refuses an option.
pub fn set_options(stream: &TcpStream) -> io::Result<()> {
    // Every write is a whole message, and a message is often one of a few small ones in a row,
    // which Nagle's algorithm would hold back while an earlier one awaits an acknowledgement that
    // the peer may delay.
    stream.set_nodelay(true)?;
    // No user timeout (`TCP_USER_TIMEOUT`) bounds data in flight: nothing resumes a session
    // once its connection ends, so the connection must not end while a transfer is merely slow
    // or an outage heals, and every operation on a session waits only until its caller's
    // deadline anyway.
    sockopt::set_socket_keepalive(stream, true)?;
    sockopt::set_tcp_keepidle(stream, KEEPALIVE_IDLE)?;
    sockopt::set_tcp_keepintvl(stream, KEEPALIVE_INTERVAL)?;
    sockopt::set_tcp_keepcnt(stream, KEEPALIVE_PROBES)?;
    Ok(())
}
