//! Serving a session: the gateway's requests, handled in the order they arrive by [`files`]
//! or [`commands`](crate::commands), and the replies to [ops](crate::ops).

use std::io::{self, BufReader, Read};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};

use pumpkin_schema::sandbox::{FromSandbox, ToSandbox, frame};
use stdx::{Context, unpoisoned};

use crate::commands::Commands;
use crate::files::{self, Puts};
use crate::ops::Ops;

/// Connects to the gateway at `socket` and serves the session until the gateway ends it. With
/// `serve_ops`, it also serves ops, from before the first request.
pub(crate) fn serve(socket: &Path, serve_ops: bool) -> Result<(), String> {
    let stream = UnixStream::connect(socket).context(format_args!(
        "connecting to the gateway at {} failed",
        socket.display()
    ))?;
    let sender = stream
        .try_clone()
        .context("cloning the connection to the gateway failed")?;
    let sender = Sender(Arc::new(Mutex::new(sender)));
    let ops = Ops::default();
    if serve_ops {
        ops.serve(sender.clone()).context("serving ops failed")?;
    }
    let commands = Commands::start_reaping(sender.clone()).context("starting the reaper failed")?;
    let mut session = Session {
        sender,
        commands,
        ops,
        puts: Puts::default(),
    };
    let served = session.serve(&mut BufReader::new(stream));
    session.commands.kill_all();
    // The op in flight, and the callers waiting their turns, end as `sandboxd` exits, which
    // closes their connections.
    served.context("the session with the gateway failed")
}

/// Sends frames to the gateway, from the session, from the thread that reports exits, and from
/// the threads that carry ops.
#[derive(Clone)]
pub(crate) struct Sender(Arc<Mutex<UnixStream>>);

impl Sender {
    pub(crate) fn send(&self, message: &FromSandbox, body: &[u8]) -> io::Result<()> {
        let mut stream = unpoisoned(self.0.lock());
        frame::write(&mut *stream, message, body)
    }

    /// Replies to the request on `stream` with its outcome.
    pub(crate) fn reply(&self, stream: u32, outcome: Result<(), String>) -> io::Result<()> {
        let message = match outcome {
            Ok(()) => FromSandbox::Done { stream },
            Err(message) => FromSandbox::Failed { stream, message },
        };
        self.send(&message, &[])
    }
}

struct Session {
    sender: Sender,
    commands: Commands,
    ops: Ops,
    puts: Puts,
}

impl Session {
    fn serve(&mut self, reader: &mut impl Read) -> io::Result<()> {
        while let Some((message, body)) = frame::read(reader)? {
            self.handle(message, &body)?;
        }
        Ok(())
    }

    /// Acts on one request. A failed request is reported to the gateway; an error here means
    /// the session itself failed.
    fn handle(&mut self, message: ToSandbox, body: &[u8]) -> io::Result<()> {
        match message {
            ToSandbox::Put { stream, dir } => self.puts.begin(&self.sender, stream, &dir),
            ToSandbox::Entry {
                stream,
                entry,
                mode,
            } => self.puts.entry(&self.sender, stream, &entry, mode),
            ToSandbox::Data { stream } => self.puts.data(&self.sender, stream, body),
            ToSandbox::End { stream } => self.puts.end(&self.sender, stream),
            ToSandbox::Start { stream, launch } => self.commands.start(stream, &launch),
            ToSandbox::Freeze { stream } => self.sender.reply(stream, self.commands.freeze()),
            ToSandbox::Collect { stream, dir } => {
                files::collect(&self.sender, stream, Path::new(dir.as_str()))
            }
            ToSandbox::OpReply { op, reply } => self.ops.reply(op, reply),
        }
    }
}
