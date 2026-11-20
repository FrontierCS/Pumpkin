//! A command's stream on a sandbox session, from reservation through its reported exit.

use std::io;
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use schema::sandbox::{Launch, ToSandbox};
use stdx::Deadline;

use super::{Route, Session, Shared};

/// A command started on a session, which names it on that session alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Process {
    pub(super) session: u64,
    pub(super) stream: u32,
}

struct ReservedStart {
    stream: u32,
    shared: Arc<Shared>,
    replies: Receiver<Result<(), String>>,
    running: bool,
}

impl ReservedStart {
    fn send(mut self, launch: Launch, by: Deadline) -> io::Result<()> {
        self.shared.send(
            &ToSandbox::Start {
                stream: self.stream,
                launch,
            },
            &[],
            by,
        )?;
        self.shared.done(&self.replies, by)?;
        // The reader removes a running command's route at its exit.
        self.running = true;
        Ok(())
    }
}

impl Drop for ReservedStart {
    fn drop(&mut self) {
        if !self.running {
            self.shared.close(self.stream);
        }
    }
}

impl Session {
    /// Starts a command, whose exit arrives as an [`Arrival::Exited`] naming the [`Process`] this
    /// returns.
    pub fn start(&mut self, launch: Launch, by: Deadline) -> io::Result<Process> {
        let (process, send) = self.prepare_start(by)?;
        send(launch, by)?;
        Ok(process)
    }

    /// Reserves a command's process handle without starting it. The sender may run on another
    /// thread while the owner handles arrivals. Its deadline bounds sending the command and
    /// awaiting confirmation. Dropping the sender releases its unused stream. Dropping the
    /// session ends the connection even if the sender still exists.
    pub fn prepare_start(
        &mut self,
        by: Deadline,
    ) -> io::Result<(
        Process,
        impl FnOnce(Launch, Deadline) -> io::Result<()> + use<>,
    )> {
        let (stream, replies) = self.shared.open(Route::Start, by)?;
        let process = Process {
            session: self.id,
            stream,
        };
        let reserved = ReservedStart {
            stream,
            shared: Arc::clone(&self.shared),
            replies,
            running: false,
        };
        Ok((process, move |launch, by| reserved.send(launch, by)))
    }

    /// Kills `process`'s group ([`ToSandbox::Kill`]), whose exit then arrives as any other's.
    ///
    /// # Panics
    ///
    /// Panics if `process` was started on another session.
    pub fn kill(&mut self, process: Process, by: Deadline) -> io::Result<()> {
        let Process { session, stream } = process;
        if session != self.id {
            unreachable!(
                "the command on stream {stream} was killed on a session other than the one it was \
                 started on, though `kill`'s contract requires that one"
            )
        }
        self.shared.request(
            |request| ToSandbox::Kill {
                stream: request,
                command: stream,
            },
            by,
        )
    }
}
