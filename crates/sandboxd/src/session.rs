//! Serving a session: the gateway's requests, handled in the order they arrive.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader, Read};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};

use pumpkin_schema::ir::AbsolutePath;
use pumpkin_schema::sandbox::tree::{self, Limits};
use pumpkin_schema::sandbox::{FromSandbox, ToSandbox, frame};
use stdx::Context;

use crate::collect;
use crate::commands::Commands;

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
        puts: HashMap::new(),
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
        #[expect(
            clippy::unreachable,
            reason = "only a thread that panics while holding the lock poisons it, and sandboxd \
                      panics only through a bug"
        )]
        let Ok(mut stream) = self.0.lock() else {
            unreachable!("a thread panicked while sending to the gateway")
        };
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
    /// The trees being put, by stream: `None` once one has failed, until its end arrives.
    puts: HashMap<u32, Option<tree::Writer>>,
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
            ToSandbox::Put { stream, dir } => self.begin_put(stream, &dir),
            ToSandbox::Entry { stream, entry } => self.put(stream, |tree| tree.entry(&entry)),
            ToSandbox::Data { stream } => self.put(stream, |tree| tree.data(body)),
            ToSandbox::End { stream } => self.end_put(stream),
            ToSandbox::Start { stream, launch } => self.commands.start(stream, &launch),
            ToSandbox::Freeze { stream } => self.sender.reply(stream, self.commands.freeze()),
            ToSandbox::Collect { stream, dir } => {
                collect::send(&self.sender, stream, Path::new(dir.as_str()))
            }
        }
    }

    fn begin_put(&mut self, stream: u32, dir: &AbsolutePath) -> io::Result<()> {
        if self.puts.contains_key(&stream) {
            return Err(protocol_error(stream, "is already putting a tree"));
        }
        match fs::create_dir_all(dir.as_str()).context(dir) {
            Ok(()) => {
                let tree = tree::Writer::new(dir.as_str().into(), Limits::NONE);
                self.puts.insert(stream, Some(tree));
                Ok(())
            }
            Err(message) => {
                self.puts.insert(stream, None);
                self.sender.reply(stream, Err(message))
            }
        }
    }

    /// Writes a part of the tree being put on `stream`, unless an earlier part failed.
    fn put(
        &mut self,
        stream: u32,
        part: impl FnOnce(&mut tree::Writer) -> io::Result<()>,
    ) -> io::Result<()> {
        let Some(slot) = self.puts.get_mut(&stream) else {
            return Err(protocol_error(stream, "is putting no tree"));
        };
        let Some(tree) = slot else {
            return Ok(());
        };
        match part(tree) {
            Ok(()) => Ok(()),
            Err(error) => {
                *slot = None;
                self.sender.reply(stream, Err(error.to_string()))
            }
        }
    }

    fn end_put(&mut self, stream: u32) -> io::Result<()> {
        match self.puts.remove(&stream) {
            None => Err(protocol_error(stream, "is putting no tree")),
            Some(None) => Ok(()),
            Some(Some(tree)) => {
                let outcome = tree.finish().map_err(|error| error.to_string());
                self.sender.reply(stream, outcome)
            }
        }
    }
}

fn protocol_error(stream: u32, what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("the gateway broke the protocol: stream {stream} {what}"),
    )
}
