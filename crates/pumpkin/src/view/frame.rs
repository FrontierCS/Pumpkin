//! The frames in which one view asks another for the runs' data, over the standard input and
//! output of the `pumpkin controller view` that an `ssh` runs, one request at a time: a request is
//! a line of JSON, an [`Asking`]; its reply a line of JSON, a [`Head`], then the body, of as many
//! bytes as the head says, so that a body needs no escaping. Git talks to the program that an
//! `ssh` child runs in the same way, over the child's standard input and output
//! (<https://github.com/git/git/blob/fb255012acbcbe81189cb8014da0280b09c66ecc/connect.c#L1531-L1537>),
//! in frames that each start with their length
//! (<https://github.com/git/git/blob/fb255012acbcbe81189cb8014da0280b09c66ecc/pkt-line.c#L134-L160>).
//!
//! What a request may ask for, and the reply's status and body, are those of the view's HTTP
//! ([`super::http`]): a `GET` of the runs' data, answered as [`super::api`] answers it.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read as _, Write};
use std::os::fd::AsFd as _;
use std::time::Duration;

use rustix::event::{self, PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use stdx::Context as _;

use super::View;
use super::http::{self, Request, Response, Sent};

/// How long to wait for a request before ending: a page that is open polls every few seconds, so
/// a minute with none is a closed page or a lost connection, and ending lets the `ssh` session,
/// and the master it goes over, end too.
pub(super) const IDLE: Duration = Duration::from_mins(1);

// Well past the largest reply: a body that a record keeps, at most the 64 MiB that the gateway
// takes of a request, or JSON of a few MiB.
const BODY_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// A request: its path and query, as a request's line gives them, and whether the reply may be
/// deflated.
#[derive(Serialize, Deserialize)]
struct Asking {
    target: String,
    deflate: bool,
}

/// The head of a reply, which a line of JSON gives ahead of its body.
#[derive(Serialize, Deserialize)]
struct Head {
    /// The status line's code and reason.
    status: String,
    content_type: String,
    deflated: bool,
    body_bytes: u64,
}

/// Answers each request that comes on the standard input, of `view`, on the standard output,
/// until the standard input ends, or brings no request for [`IDLE`].
///
/// # Errors
///
/// Fails if either fails, or a request's line is longer than [`http::HEAD_MAX_BYTES`], or cut
/// short.
pub(super) fn answer(view: &mut View) -> Result<(), String> {
    let stdin = io::stdin().as_fd().try_clone_to_owned();
    let mut input = BufReader::new(File::from(stdin.context("the standard input")?));
    let mut output = io::stdout().lock();
    while arrives(&input)? {
        let mut line = Vec::new();
        let mut read = (&mut input).take(http::HEAD_MAX_BYTES);
        let read = read.read_until(b'\n', &mut line);
        read.context("reading the standard input failed")?;
        if line.is_empty() {
            break;
        }
        if !line.ends_with(b"\n") {
            return Err(format!(
                "a request's line is cut short, or longer than {} KiB: expected a line of JSON, \
                 as `pumpkin view` sends",
                http::HEAD_MAX_BYTES / 1024
            ));
        }
        let (response, deflate) = match serde_json::from_slice::<Asking>(&line) {
            Ok(Asking { target, deflate }) => match Request::get(&target, deflate) {
                Ok(request) => (super::api(&request, view), deflate),
                Err(refused) => (refused, false),
            },
            Err(error) => {
                let message = format!(
                    "the request is no JSON `{{\"target\": <path>, \"deflate\": <bool>}}`: \
                     {error}"
                );
                (Response::refused("400 Bad Request", &message), false)
            }
        };
        reply(&mut output, response, deflate).context("writing to the standard output failed")?;
    }
    Ok(())
}

/// Whether `input` brings a request, or its end, within [`IDLE`].
fn arrives(input: &BufReader<File>) -> Result<bool, String> {
    if !input.buffer().is_empty() {
        return Ok(true);
    }
    let Ok(timeout) = Timespec::try_from(IDLE) else {
        unreachable!("{IDLE:?} fits a `timespec`")
    };
    loop {
        let mut polled = [PollFd::new(input.get_ref(), PollFlags::IN)];
        match event::poll(&mut polled, Some(&timeout)) {
            Ok(0) => return Ok(false),
            Ok(_) => return Ok(true),
            Err(Errno::INTR) => {}
            Err(error) => return Err(format!("polling the standard input failed: {error}")),
        }
    }
}

/// Writes `response` on `output` as a reply, deflated if `deflate`, as the request accepts it.
fn reply(output: &mut impl Write, response: Response, deflate: bool) -> io::Result<()> {
    // A page's file, the one answer a browser may keep, is never asked for here.
    let Sent {
        status,
        content_type,
        body,
        deflated,
        immutable: _,
    } = response.sent(deflate);
    let head = Head {
        status: status.into_owned(),
        content_type: content_type.into_owned(),
        deflated,
        body_bytes: body.len() as u64,
    };
    let mut line = serde_json::to_vec(&head)?;
    line.push(b'\n');
    output.write_all(&line)?;
    output.write_all(&body)?;
    output.flush()
}

/// The reply to `request`, a `GET` of the runs' data, asked on `to`, the standard input of
/// `pumpkin controller view`, and read from `from`, its standard output.
///
/// # Errors
///
/// Fails as [`io::ErrorKind::UnexpectedEof`] or [`io::ErrorKind::BrokenPipe`] if the view there
/// ended before it replied in full, and else if the reply is not one that it writes.
pub(super) fn ask(
    request: &Request,
    to: &mut impl Write,
    from: &mut impl BufRead,
) -> io::Result<Response> {
    let asking = Asking {
        target: request.target.clone(),
        deflate: request.deflate,
    };
    let mut line = serde_json::to_vec(&asking)?;
    line.push(b'\n');
    to.write_all(&line)?;
    to.flush()?;
    let ended = || {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "it ended before it replied in full",
        )
    };
    let mut line = Vec::new();
    from.take(http::HEAD_MAX_BYTES)
        .read_until(b'\n', &mut line)?;
    if !line.ends_with(b"\n") {
        if line.len() as u64 == http::HEAD_MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "its reply's head is longer than {} KiB: expected a line of JSON",
                    http::HEAD_MAX_BYTES / 1024
                ),
            ));
        }
        return Err(ended());
    }
    let Head {
        status,
        content_type,
        deflated,
        body_bytes,
    } = serde_json::from_slice(&line).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("its reply's head is no JSON head of a reply: {error}"),
        )
    })?;
    if body_bytes > BODY_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "its reply's body is of {body_bytes} bytes: expected {} MiB at most",
                BODY_MAX_BYTES / 1024 / 1024
            ),
        ));
    }
    let mut body = Vec::new();
    from.take(body_bytes).read_to_end(&mut body)?;
    if body.len() as u64 != body_bytes {
        return Err(ended());
    }
    Ok(Response::relayed(status, content_type, body, deflated))
}
