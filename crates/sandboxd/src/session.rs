//! Serving a session: the gateway's requests, handled in the order they arrive, each by
//! [`files`] or [`commands`](crate::commands).

use std::io::{self, BufReader, Read};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};

use pumpkin_schema::sandbox::{FromSandbox, ToSandbox, frame};
use stdx::Context;

use crate::commands::Commands;
use crate::files::{self, Puts};
use crate::unpoisoned;

/// Connects to the gateway at `socket` and serves the session until the gateway ends it.
pub(crate) fn serve(socket: &Path) -> Result<(), String> {
    let stream = UnixStream::connect(socket).context(format_args!(
        "connecting to the gateway at {} failed",
        socket.display()
    ))?;
    let sender = stream
        .try_clone()
        .context("cloning the connection to the gateway failed")?;
    let sender = Sender(Arc::new(Mutex::new(sender)));
    let commands = Commands::start_reaping(sender.clone()).context("starting the reaper failed")?;
    let mut session = Session {
        sender,
        commands,
        puts: Puts::default(),
    };
    let served = session.serve(&mut BufReader::new(stream));
    session.commands.kill_all();
    served.context("the session with the gateway failed")
}

/// Sends frames to the gateway, from the session and from the thread that reports exits.
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
            ToSandbox::Entry { stream, entry } => self.puts.entry(&self.sender, stream, &entry),
            ToSandbox::Data { stream } => self.puts.data(&self.sender, stream, body),
            ToSandbox::End { stream } => self.puts.end(&self.sender, stream),
            ToSandbox::Start { stream, launch } => self.commands.start(stream, &launch),
            ToSandbox::Freeze { stream } => self.sender.reply(stream, self.commands.freeze()),
            ToSandbox::Collect { stream, dir } => {
                files::collect(&self.sender, stream, Path::new(dir.as_str()))
            }
        }
    }
}
