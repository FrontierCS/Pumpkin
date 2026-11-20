//! A blocking function call uses the same admission and status ops as a caller that polls.

use std::os::unix::net::UnixStream;
use std::time::Duration;
use std::{io, thread};

use rustix::net::{self, AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
use schema::sandbox::CONTROL_SOCKET;
use schema::sandbox::op::{CallReply, JobState, OpCall, OpReply, StatusReply};
use stdx::{Context, Deadline};

use super::exchange;
use crate::until::Until;

// Short board calls share a sandbox. Leave time for the next caller between attempts.
const POLL: Duration = Duration::from_millis(100);

pub(super) fn call(call: &OpCall, body: &[u8], by: Deadline) -> Result<OpReply, (u8, String)> {
    let job = loop {
        let reply = request(call, body, by).map_err(|message| {
            if by.remaining().is_none() {
                (
                    124,
                    "wait expired before the admission reply: the call may have started".to_owned(),
                )
            } else {
                (1, message)
            }
        })?;
        match reply {
            OpReply::Call(CallReply::Job(admitted)) => break admitted.job,
            OpReply::Refused { busy: true, .. } => {
                if !pause(by) {
                    return Err((
                        124,
                        "wait expired while the function was busy: the call was not charged"
                            .to_owned(),
                    ));
                }
            }
            reply => return Ok(reply),
        }
    };
    loop {
        let reply = request(&OpCall::Status { job }, &[], by).map_err(|message| {
            if by.remaining().is_none() {
                expired(job)
            } else {
                (1, format!("checking job {job} failed: {message}: inspect it with `sandboxctl status {job}`"))
            }
        })?;
        match reply {
            OpReply::Status(StatusReply {
                state: JobState::Queued | JobState::Running,
                ..
            }) => {
                if !pause(by) {
                    return Err(expired(job));
                }
            }
            reply => return Ok(reply),
        }
    }
}

fn expired(job: u64) -> (u8, String) {
    (
        124,
        format!(
            "wait expired after job {job} was admitted: inspect it with `sandboxctl status {job}`"
        ),
    )
}

fn pause(by: Deadline) -> bool {
    if let Some(left) = by.remaining() {
        thread::sleep(POLL.min(left));
    }
    by.remaining().is_some()
}

fn request(call: &OpCall, body: &[u8], by: Deadline) -> Result<OpReply, String> {
    let stream = connect().context(format_args!(
        "sandboxd at {CONTROL_SOCKET} can't be reached"
    ))?;
    exchange(Until::new(stream, by), call, body)
}

fn connect() -> io::Result<UnixStream> {
    // A blocking Unix connect can wait for space in the listener's backlog.
    let socket = net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        None,
    )?;
    net::connect(&socket, &SocketAddrUnix::new(CONTROL_SOCKET)?)?;
    let stream = UnixStream::from(socket);
    stream.set_nonblocking(false)?;
    Ok(stream)
}
