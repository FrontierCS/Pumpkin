//! Serving a session, once the gateway accepts its hello: reading what the gateway sends as it
//! arrives, its requests, which [`files`] and [`commands`](crate::commands) serve, the replies to
//! [ops](crate::ops), and the responses to [model requests](crate::model). A collect and a
//! freeze, which may last, are served on threads of their own where one can start, so that the
//! session reads on meanwhile; any other request is served before the next frame is read.

use std::io::{self, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::{fmt, thread};

use schema::sandbox::{FromSandbox, HELLO_TIMEOUT, Hello, HelloReply, ToSandbox, frame};
use stdx::{Context, unpoisoned};

use crate::commands::Commands;
use crate::files::{self, Puts};
use crate::model::{Model, Part};
use crate::ops::Ops;

/// Where `sandboxd` meets its gateway.
pub(crate) enum Gateway {
    /// At a Unix socket of the sandbox's own, which identifies the sandbox.
    Socket(PathBuf),
}

impl fmt::Display for Gateway {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Socket(socket) => socket.display().fmt(f),
        }
    }
}

/// The reading end of a session's connection.
type Reader = Box<dyn Read>;

/// The writing end of a session's connection.
type Writer = Box<dyn Write + Send>;

/// Connects to `gateway`, opens the session, and serves it until the gateway ends it. With
/// `serve_ops`, it also serves ops, and with `serve_model`, the model endpoint, both from before
/// the first request. Fails, with the gateway's reason, if the gateway refuses the session,
/// having started nothing.
pub(crate) fn serve(gateway: &Gateway, serve_ops: bool, serve_model: bool) -> Result<(), String> {
    let (reader, writer) = meet(gateway)?;
    let sender = Sender(Arc::new(Mutex::new(writer)));
    let ops = Ops::default();
    if serve_ops {
        ops.serve(sender.clone()).context("serving ops failed")?;
    }
    let model = Model::default();
    if serve_model {
        let served = model.serve(sender.clone());
        served.context("serving the model endpoint failed")?;
    }
    let commands = Commands::start_reaping(sender.clone()).context("starting the reaper failed")?;
    let mut session = Session {
        sender,
        commands,
        ops,
        model,
        puts: Puts::default(),
    };
    let served = session.serve(&mut BufReader::new(reader));
    session.commands.kill_all();
    // The op in flight, the callers waiting their turns and the model requests in flight end as
    // `sandboxd` exits, which closes their connections.
    served.context("the session with the gateway failed")
}

/// Meets `gateway`, once it connects, or connecting to it, once, and returns the connection's two
/// ends, the session open.
fn meet(gateway: &Gateway) -> Result<(Reader, Writer), String> {
    let connecting = format_args!("connecting to the gateway at {gateway} failed");
    let opening = format_args!("opening the session with the gateway at {gateway} failed");
    match gateway {
        Gateway::Socket(socket) => {
            let stream = UnixStream::connect(socket).context(connecting)?;
            open(&mut &stream, &Hello { grant: None }).context(opening)?;
            halves(stream, UnixStream::try_clone)
        }
    }
}

/// Says `hello` to `peer`, and returns once its answer accepts the session; or why not, saying
/// what `it`, the peer, did.
fn open(peer: &mut (impl Read + Write), hello: &Hello) -> Result<(), String> {
    let late = |error: io::Error| match error.kind() {
        io::ErrorKind::TimedOut => format!(
            "it didn't take the hello and answer it within {} s, or by the sandbox's deadline if \
             sooner: expected the gateway, which answers at once",
            HELLO_TIMEOUT.as_secs()
        ),
        _ => error.to_string(),
    };
    frame::write(peer, hello, &[])
        .map_err(late)
        .context("sending the hello failed")?;
    // Unbuffered, as a frame is read to its end and no further, so that the session's reads,
    // through a buffer of their own, start at the gateway's first request.
    let answer = frame::read_hello_reply(peer)
        .map_err(late)
        .context("reading its answer failed")?;
    match answer {
        Some(HelloReply::Accepted) => Ok(()),
        Some(HelloReply::Refused { message }) => Err(format!("it refused the session: {message}")),
        None => Err(
            "it closed the connection before it answered the hello: expected it to accept or \
             refuse the session"
                .to_owned(),
        ),
    }
}

/// The two ends of `stream`, a connection to the gateway, the writing one cloned with `clone`.
fn halves<S: Read + Write + Send + 'static>(
    stream: S,
    clone: impl FnOnce(&S) -> io::Result<S>,
) -> Result<(Reader, Writer), String> {
    let writer = clone(&stream).context("cloning the connection to the gateway failed")?;
    Ok((Box::new(stream), Box::new(writer)))
}

/// Sends frames to the gateway, from the session, from the thread that reports exits, from the
/// threads that collect and freeze, and from the threads that carry ops and model requests.
#[derive(Clone)]
pub(crate) struct Sender(Arc<Mutex<Writer>>);

impl Sender {
    pub(crate) fn send(&self, message: &FromSandbox, body: &[u8]) -> io::Result<()> {
        let mut writer = unpoisoned(self.0.lock());
        frame::write(&mut *writer, message, body)
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
    model: Model,
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

            ToSandbox::Freeze { stream } => {
                let commands = self.commands.clone();
                self.aside("freeze", move |sender| {
                    sender.reply(stream, commands.freeze())
                })
            }
            ToSandbox::Collect { stream, dir } => self.aside("collect", move |sender| {
                files::collect(sender, stream, Path::new(dir.as_str()))
            }),
            ToSandbox::OpReply { op, reply } => self.ops.reply(op, reply),
            ToSandbox::ModelResponse {
                request,
                status,
                headers,
            } => self.model.part(request, Part::Head { status, headers }),
            ToSandbox::ModelData { request } => self.model.part(request, Part::Data(body.to_vec())),
            ToSandbox::ModelEnd { request, cut } => self.model.part(request, Part::End { cut }),
        }
    }

    /// Serves a request with `serve`, which replies to it, on a thread named `name`, or on this
    /// thread if that one can't start.
    fn aside(
        &self,
        name: &str,
        serve: impl FnOnce(&Sender) -> io::Result<()> + Clone + Send + 'static,
    ) -> io::Result<()> {
        let (sender, aside) = (self.sender.clone(), serve.clone());
        // The gateway writes some frames from threads of its own, beside its requests, each by
        // its deadline, and a frame this session doesn't read waits in the connection's buffers,
        // then in the gateway's write: so the session reads on while a tree goes out at the
        // link's pace, or while freezing waits for the killed processes to go, rather than make
        // the gateway's other writers wait that long, past their deadlines.
        let spawned = thread::Builder::new().name(name.to_owned()).spawn(move || {
            // A write that fails has failed the connection, which the session's reads then see.
            let _ = aside(&sender);
        });
        match spawned {
            Ok(_) => Ok(()),
            // No thread starts while the sandbox's processes fill its limit, as a fork bomb's do,
            // yet freezing is what kills them, and collecting what keeps the run's record: so the
            // request is served here, holding the session's reads up meanwhile.
            Err(_) => serve(&self.sender),
        }
    }
}
