//! A blocking function call uses the same admission and status ops as a caller that polls.

use std::os::unix::net::UnixStream;
use std::time::Duration;
use std::{io, thread};

use rustix::net::{self, AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
use schema::sandbox::CONTROL_SOCKET;
use schema::sandbox::op::{CallReply, JobState, OpCall, OpReply, StatusReply};
use stdx::Deadline;

use super::exchange;
use crate::until::Until;

// A short call gets its first check promptly. Doubling up to two seconds limits repeated
// payload transfers while a function stays busy and keeps the result within a short poll.
const FIRST_POLL: Duration = Duration::from_millis(100);
const LONGEST_POLL: Duration = Duration::from_secs(2);

pub(super) fn call(call: &OpCall, body: &[u8], by: Deadline) -> Result<OpReply, (u8, String)> {
    let mut delay = FIRST_POLL;
    let job = loop {
        let reply = request(call, body, by)?;
        match reply {
            OpReply::Call(CallReply::Job(admitted)) => break admitted.job,
            OpReply::Refused { busy: true, .. } => {
                if !pause(by, &mut delay) {
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
    let mut delay = FIRST_POLL;
    loop {
        let reply = request(&OpCall::Status { job }, &[], by).map_err(|(code, message)| {
            if code == 124 {
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
                if !pause(by, &mut delay) {
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

fn pause(by: Deadline, delay: &mut Duration) -> bool {
    if let Some(left) = by.remaining() {
        thread::sleep(left.min(*delay));
        *delay = (*delay * 2).min(LONGEST_POLL);
    }
    by.remaining().is_some()
}

fn request(call: &OpCall, body: &[u8], by: Deadline) -> Result<OpReply, (u8, String)> {
    let stream = connect(by).map_err(|error| match error.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => (
            124,
            "wait expired connecting to sandboxd: the request was not sent".to_owned(),
        ),
        _ => (
            1,
            format!("sandboxd at {CONTROL_SOCKET} can't be reached: {error}"),
        ),
    })?;
    let answer = exchange(Until::new(stream, by), call, body).map_err(|message| {
        let (code, message) = if by.remaining().is_none() {
            (124, "wait expired before the reply".to_owned())
        } else {
            (1, message)
        };
        if matches!(call, OpCall::Call { .. }) {
            (code, format!("{message}: the call may have started"))
        } else {
            (code, message)
        }
    })?;
    answer.map_err(|message| (1, message))
}

fn connect(by: Deadline) -> io::Result<UnixStream> {
    let socket = net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )?;
    let stream = UnixStream::from(socket);
    // SO_SNDTIMEO bounds a blocking connect when the Unix listener's backlog is full.
    stream.set_write_timeout(Some(by.remaining().ok_or(io::ErrorKind::TimedOut)?))?;
    net::connect(&stream, &SocketAddrUnix::new(CONTROL_SOCKET)?)?;
    Ok(stream)
}
