//! The replies that the sandbox sends on a request's stream, each taken by the request that opened
//! the stream.

use std::io;
use std::time::Duration;

use schema::sandbox::{Entry, ToSandbox, tree};
use stdx::io::Io;

use super::{Session, Then};
use crate::trees::{PutState, Step};
use crate::{Answer, Answerer, Arrival, By, Owner, Process, Told, broke};

// Where the frames on a stream go. Each route takes only the replies its variant is for: any other
// breaks the protocol. A `Start` becomes `Running` once its command has started, and its stream
// then carries only the command's exit.
pub(super) enum Route {
    Reply {
        answer: Answerer,
        by: Duration,
    },
    Put {
        answer: Answerer,
        by: Duration,
        put: PutState,
    },
    Start {
        answer: Answerer,
        by: Duration,
        process: Process,
    },
    Tree {
        answer: Answerer,
        by: Duration,
        tree: tree::Writer,
        entries: Vec<Entry>,
    },
    Running,
}

impl Route {
    pub(super) fn by(&self) -> Option<Duration> {
        match self {
            Self::Reply { by, .. }
            | Self::Put { by, .. }
            | Self::Start { by, .. }
            | Self::Tree { by, .. } => Some(*by),
            Self::Running => None,
        }
    }
}

pub(super) enum Reply {
    Done,
    Failed(String),
    Exited(schema::sandbox::ExitStatus),
    Entry(Entry),
    Data(Vec<u8>),
    End,
}

impl Reply {
    fn name(&self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Failed(_) => "failed",
            Self::Exited(_) => "exited",
            Self::Entry(_) => "entry",
            Self::Data(_) => "data",
            Self::End => "end",
        }
    }
}

impl Session {
    pub(super) fn route<C: Owner>(
        &mut self,
        io: &mut Io<C>,
        stream: u32,
        reply: Reply,
        told: &mut Told<'_>,
    ) -> Result<(), String> {
        let more = || {
            broke(&format!(
                "it sent more on stream {stream} than a request takes"
            ))
        };
        let Some(route) = self.routes.remove(&stream) else {
            if !(1..self.next_stream).contains(&stream) {
                let expected = "nothing on a stream no request has opened";
                return Err(out_of_turn(stream, &reply, expected));
            }
            // A request closes its stream as it ends, even if it gave up before `sandboxd` was done
            // answering, as a collect does at a limit its tree passes. `sandboxd` can't know, and
            // may still be sending what it had begun, so a reply on a closed stream is an expected
            // race, not a break.
            return Ok(());
        };
        match (route, reply) {
            (Route::Reply { answer, .. }, Reply::Done) => {
                told.answer(answer, Answer::Done(Ok(())));
            }
            (Route::Reply { answer, .. }, Reply::Failed(message)) => {
                told.answer(answer, Answer::Done(Err(io::Error::other(message))));
            }
            (
                Route::Start {
                    answer, process, ..
                },
                Reply::Done,
            ) => {
                self.routes.insert(stream, Route::Running);
                told.answer(answer, Answer::Started(Ok(process)));
            }
            (Route::Start { answer, .. }, Reply::Failed(message)) => {
                told.answer(answer, Answer::Started(Err(io::Error::other(message))));
            }
            (Route::Running, Reply::Exited(status)) => {
                let process = Process {
                    session: self.id,
                    stream,
                };
                told.arrived(&self.sink, Arrival::Exited { process, status });
            }
            (Route::Running, _) => return Err(more()),
            (Route::Put { answer, by, put }, reply @ (Reply::Done | Reply::Failed(_))) => {
                return self.put_replied(io, stream, (answer, by, put), reply, told);
            }
            (
                Route::Tree {
                    answer,
                    by,
                    tree,
                    entries,
                },
                reply @ (Reply::Entry(_) | Reply::Data(_) | Reply::End | Reply::Failed(_)),
            ) => return self.part(stream, (answer, by, tree, entries), reply, told),
            (route, reply) => {
                let expected = match route {
                    Route::Reply { .. } | Route::Start { .. } | Route::Put { .. } => {
                        "`done` or `failed`"
                    }
                    Route::Tree { .. } => "`entry`, `data`, `end` or `failed`",
                    Route::Running => "`exited`",
                };
                // Back, so that the session's end answers it.
                self.routes.insert(stream, route);
                return Err(out_of_turn(stream, &reply, expected));
            }
        }
        Ok(())
    }

    // Takes the reply to a put: a failure while its tree is sent, or its answer at the tree's end.
    fn put_replied<C: Owner>(
        &mut self,
        io: &mut Io<C>,
        stream: u32,
        (answer, by, mut put): (Answerer, Duration, PutState),
        reply: Reply,
        told: &mut Told<'_>,
    ) -> Result<(), String> {
        match put.replied() {
            // `sandboxd` replies before the end only to fail the tree, which then takes nothing
            // more; a `done` before the end is out of turn.
            Step::Sending => {
                let Reply::Failed(message) = reply else {
                    // Back, so that the session's end answers it.
                    self.routes.insert(stream, Route::Put { answer, by, put });
                    let expected = "`failed`, or nothing before the tree's end";
                    return Err(out_of_turn(stream, &Reply::Done, expected));
                };
                let by = By::request(by);
                let end = (&ToSandbox::End { stream }, &[][..]);
                // The end goes once the frames already queued have, of which none is lost.
                let _ = self.queue(io, end, by, Then::Nothing, told);
                put.stop();
                told.answer(answer, Answer::Done(Err(io::Error::other(message))));
            }
            Step::Ended(file_error) => {
                let result = match (reply, file_error) {
                    (_, Some(error)) => Err(error),
                    (Reply::Done, None) => Ok(()),
                    (Reply::Failed(message), None) => Err(io::Error::other(message)),
                    (other, None) => {
                        unreachable!("{} is no reply to a put", other.name())
                    }
                };
                told.answer(answer, Answer::Done(result));
            }
        }
        Ok(())
    }

    // Writes a part of a collected tree.
    fn part(
        &mut self,
        stream: u32,
        (answer, by, mut tree, mut entries): (Answerer, Duration, tree::Writer, Vec<Entry>),
        reply: Reply,
        told: &mut Told<'_>,
    ) -> Result<(), String> {
        let written = match reply {
            Reply::Entry(entry) => {
                let created = created(tree.entry(&entry));
                entries.push(entry);
                created
            }
            Reply::Data(bytes) => written(tree.data(&bytes)),
            Reply::End => {
                let finished = written(tree.finish());
                return match finished {
                    Ok(()) => {
                        told.answer(answer, Answer::Collected(Ok(entries)));
                        Ok(())
                    }
                    Err(Written::Malformed(error)) => Err(broken(told, answer, stream, &error)),
                    Err(Written::Failed(error)) => {
                        told.answer(answer, Answer::Collected(Err(error)));
                        Ok(())
                    }
                };
            }
            Reply::Failed(message) => {
                told.answer(answer, Answer::Collected(Err(io::Error::other(message))));
                return Ok(());
            }
            Reply::Done | Reply::Exited(_) => unreachable!("a tree's part is no `done`"),
        };
        match written {
            Ok(()) => {
                let route = Route::Tree {
                    answer,
                    by,
                    tree,
                    entries,
                };
                self.routes.insert(stream, route);
                Ok(())
            }
            Err(Written::Malformed(error)) => Err(broken(told, answer, stream, &error)),
            // The collect fails alone; what `sandboxd` still sends of it arrives on a closed
            // stream, which is dropped.
            Err(Written::Failed(error)) => {
                told.answer(answer, Answer::Collected(Err(error)));
                Ok(())
            }
        }
    }
}

fn out_of_turn(stream: u32, reply: &Reply, expected: &str) -> String {
    broke(&format!(
        "it sent `{}` on stream {stream} out of turn: expected {expected}",
        reply.name()
    ))
}

enum Written {
    // The parts don't fit together, which breaks the protocol.
    Malformed(io::Error),
    // The collect fails alone: a limit passed, or the host failing to write.
    Failed(io::Error),
}

// A malformed tree breaks the protocol. The writer fails with `InvalidData` where the parts don't
// fit together, as file data past its file's size, and no system error has that kind. Any other
// error fails this collect alone: a limit passed, `FileTooLarge`, is the request's own bound, not
// a break, and the host failing to write is no fault of the sandbox's.
fn written(written: io::Result<()>) -> Result<(), Written> {
    match written {
        Err(error) if error.kind() == io::ErrorKind::InvalidData => Err(Written::Malformed(error)),
        Err(error) => Err(Written::Failed(error)),
        Ok(()) => Ok(()),
    }
}

// Creating an entry fails with `NotFound`, `NotADirectory` or `AlreadyExists` where its parent is
// missing or a file, or its path is taken. In the collect's directory, new and left to it, only the
// tree's own earlier entries can cause that, given that it is on a filesystem that neither folds
// case nor normalizes names: on one that does, a correct `sandboxd` that sends `A` and `a` would
// meet `AlreadyExists`, and end the session.
fn created(created: io::Result<()>) -> Result<(), Written> {
    match created {
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound
                    | io::ErrorKind::NotADirectory
                    | io::ErrorKind::AlreadyExists
            ) =>
        {
            Err(Written::Malformed(error))
        }
        created => written(created),
    }
}

// Answers a collect whose tree is malformed with the break that ends its session, and returns it.
fn broken(told: &mut Told<'_>, answer: Answerer, stream: u32, error: &io::Error) -> String {
    let reason = malformed(stream, error);
    let aborted = io::Error::new(io::ErrorKind::ConnectionAborted, reason.clone());
    told.answer(answer, Answer::Collected(Err(aborted)));
    reason
}

fn malformed(stream: u32, error: &io::Error) -> String {
    broke(&format!(
        "the tree it sent on stream {stream} is malformed: {error}"
    ))
}
