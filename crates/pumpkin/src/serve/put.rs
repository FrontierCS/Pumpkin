//! An upload watches admission as well as its input: a peer can finish it between any two bytes.

use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use rustix::event::{self, PollFd, PollFlags};
use rustix::fs::inotify::{self, CreateFlags, WatchFlags};
use rustix::io::Errno;
use schema::frame;

use crate::link;
use crate::link::store::{Parts, Put};
use crate::manifest::store::{self, PutAnswer};

/// Returns whether the client can issue another request on this link.
pub(super) fn receive(
    home: &Path,
    put: &Put,
    input: &mut BufReader<File>,
    output: &mut impl Write,
) -> io::Result<bool> {
    let store = home.join(crate::job::STORE);
    let bytes = put.body_bytes()?;
    std::fs::create_dir_all(store.join(crate::manifest::INCOMING))?;
    // Watch before checking the store so admission cannot fall between the check and the wait.
    let events = inotify::init(CreateFlags::CLOEXEC)?;
    inotify::add_watch(&events, &store, WatchFlags::MOVED_TO | WatchFlags::CREATE)?;
    let mut incoming = Incoming {
        input,
        events,
        blob: store.join(put.digest.hex()),
        size: put.size_bytes,
        admitted: false,
    };
    let mut parts = Parts::new(&mut incoming, bytes);
    let stored = store::put(
        &store,
        &put.upload,
        &put.digest,
        put.from,
        put.size_bytes,
        &mut parts,
    );
    let early = matches!(stored, Ok(PutAnswer::Stored { .. })) && parts.remaining() != 0;
    let drained = if early { Ok(()) } else { parts.finish() };
    if early || incoming.admitted {
        frame::write(
            output,
            &PutAnswer::StoredEarly {
                committed_size: put.size_bytes,
            },
            &[],
        )?;
        return Ok(false);
    }
    drained?;
    match stored {
        Ok(answer) => frame::write(output, &answer, &[])?,
        Err(error) => link::refuse(output, error)?,
    }
    Ok(true)
}

struct Incoming<'a> {
    input: &'a mut BufReader<File>,
    events: OwnedFd,
    blob: PathBuf,
    size: u64,
    admitted: bool,
}

impl Read for Incoming<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        while self.input.buffer().is_empty() && !bytes.is_empty() {
            let mut fds = [
                PollFd::new(self.input.get_ref(), PollFlags::IN),
                PollFd::new(&self.events, PollFlags::IN),
            ];
            match event::poll(&mut fds, None) {
                Err(Errno::INTR) => continue,
                result => {
                    result?;
                }
            }
            if !fds[1].revents().is_empty() {
                // Any rename, including a coalesced or overflowed batch, calls for the same check.
                rustix::io::read(&self.events, &mut [0_u8; 4096])?;
                if store::stored_size(&self.blob, self.size)? {
                    self.admitted = true;
                    return Ok(0);
                }
            }
            if !fds[0].revents().is_empty() {
                break;
            }
        }
        self.input.read(bytes)
    }
}
