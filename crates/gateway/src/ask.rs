//! What the loop's owner asks of a session ([`Sessions::ask`]), and the one dispatch that asks it
//! on the loop.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use schema::ir::AbsolutePath;
use schema::sandbox::op::OpReply;
use schema::sandbox::tree::Limits;
use schema::sandbox::{Launch, Root, ToSandbox, dir_mode, pumpkin_dir, pumpkin_subdirs};
use stdx::io::Io;

use crate::session::Session;
use crate::{
    Answer, Answerer, IncomingOp, Owner, Process, Put, RefusedOp, SessionId, Sessions, Told,
};

/// What the loop's owner asks of a session ([`Sessions::ask`]).
#[derive(Debug)]
pub enum Ask {
    /// Writes the entries below the directory in the sandbox, made as the root says, and its
    /// missing parents. Each entry's parent must be the directory or come earlier among the
    /// entries, unless it exists already. It ends at the first failure, and whatever was written
    /// before it stays.
    Put(AbsolutePath, Root, Vec<Put>),
    /// Kills the sandbox's processes that [`ToSandbox::Freeze`] names, so that nothing collected
    /// afterwards changes, and is answered once they are gone.
    Freeze,
    /// Copies the tree at the path, a directory's or a file's, into the directory, a new one,
    /// within the limits. It fails if the directory exists or the tree passes a limit; after any
    /// failure, the directory holds whatever arrived before it. Its answer is
    /// [`Answer::Collected`].
    Collect(AbsolutePath, PathBuf, Limits),
    /// Starts a command, whose [`Answer::Started`] names the process that its
    /// [`crate::Arrival::Exited`] names.
    Start(Launch),
    /// Kills the process's group ([`ToSandbox::Kill`]), whose exit then arrives as any other's.
    Kill(Process),
    /// Answers an op, promptly: the sandbox opens no other until it is. A reply too long for a
    /// frame refuses the op instead, and fails.
    Reply(IncomingOp, OpReply),
    /// Answers an op whose payload was over the limit with its refusal.
    Refuse(RefusedOp),
    /// Sends a frame of the response to the session's model request of that number, by the
    /// response's deadline: a response still open at its deadline ends the session.
    Respond(u32, Response),
}

/// A frame of a model request's response, which ends with [`Response::End`], once.
#[derive(Debug)]
pub enum Response {
    /// The response's status and headers, first. One too long for a frame fails, and the owner
    /// then ends the response cut short.
    Head {
        status: u16,
        headers: Vec<(String, String)>,
    },
    /// The next bytes of the response's body, at most a frame's
    /// ([`MAX_BODY_BYTES`](schema::frame::MAX_BODY_BYTES)).
    Data(Vec<u8>),
    /// The response's end: whole, or cut short, which its client must not take for a whole one.
    End { cut: bool },
}

impl Sessions {
    /// Asks the session for what `ask` says, by `by`. The owner hears the answer with `token` once
    /// this call has returned, never in it, and of a session that has ended, that it has. Returns,
    /// for a start, the process that the command's exit will name, before the start is answered,
    /// as the sandbox may send an op first; none if the start failed as it was asked, which the
    /// answer says.
    ///
    /// # Panics
    ///
    /// Panics if the op or the process that `ask` names is another session's.
    pub fn ask<C: Owner>(
        owner: &mut C,
        io: &mut Io<C>,
        id: SessionId,
        ask: Ask,
        (by, token): (Duration, u64),
    ) -> Option<Process> {
        ask.check_session(id);
        let process = Self::dispatch_to(owner, io, id, (ask, by), Answerer::Owner(token));
        owner.sessions().flush_later(io);
        process
    }

    /// Puts what every sandbox needs before its first command, and answers as [`Sessions::ask`]
    /// does. A failure's error says what failed.
    pub fn set_up<C: Owner>(
        owner: &mut C,
        io: &mut Io<C>,
        id: SessionId,
        (by, token): (Duration, u64),
    ) {
        let (root, puts) = set_up_puts();
        let ask = Ask::Put(root, Root::Kept, puts);
        Self::dispatch_to(owner, io, id, (ask, by), Answerer::SetUp(token));
        owner.sessions().flush_later(io);
    }

    // Asks the session `id` for what `ask` says, whose answer `answerer` hears: for a start, the
    // process that its exit will name.
    fn dispatch_to<C: Owner>(
        owner: &mut C,
        io: &mut Io<C>,
        id: SessionId,
        (ask, by): (Ask, Duration),
        answerer: Answerer,
    ) -> Option<Process> {
        let (session, mut told) = owner.sessions().held(id);
        let process = if let Some(session) = session {
            dispatch(session, io, (ask, by), answerer, &mut told)
        } else {
            // A session already forgotten, as its owner dropped it, answers nothing more but that
            // it has ended.
            told.answer(answerer, ask.gone());
            None
        };
        owner.sessions().reap(io, id);
        process
    }
}

// Asks `session` for what `ask` says, whose answer `answerer` hears: for a start, the process that
// its exit will name.
fn dispatch<C: Owner>(
    session: &mut Session,
    io: &mut Io<C>,
    (ask, by): (Ask, Duration),
    answerer: Answerer,
    told: &mut Told<'_>,
) -> Option<Process> {
    match ask {
        Ask::Put(dir, root, entries) => session.put(io, (dir, root, entries), (answerer, by), told),
        Ask::Freeze => {
            let freeze = |stream| ToSandbox::Freeze { stream };
            session.request(io, freeze, (answerer, by), told);
        }
        Ask::Collect(path, into, limits) => {
            session.collect(io, (path, into, limits), (answerer, by), told);
        }
        Ask::Start(launch) => return session.start(io, launch, (answerer, by), told),
        Ask::Kill(process) => session.kill(io, process, (answerer, by), told),
        Ask::Reply(op, reply) => session.reply(io, op, reply, (answerer, by), told),
        Ask::Refuse(refused) => session.refuse(io, &refused, (answerer, by), told),
        Ask::Respond(request, response) => {
            let message = match response {
                Response::Head { status, headers } => ToSandbox::ModelResponse {
                    request,
                    status,
                    headers,
                },
                Response::Data(bytes) => {
                    let data = ToSandbox::ModelData { request };
                    session.respond(io, (data, bytes), (answerer, by), told);
                    return None;
                }
                Response::End { cut } => ToSandbox::ModelEnd { request, cut },
            };
            session.respond(io, (message, Vec::new()), (answerer, by), told);
        }
    }
    None
}

impl Ask {
    // Panics if the op that it answers, or the command that it kills, is another session's than
    // `session`.
    fn check_session(&self, session: SessionId) {
        let of = match self {
            Self::Kill(process) => process.session,
            Self::Reply(op, ..) => op.session,
            Self::Refuse(refused) => refused.session,
            Self::Put(..)
            | Self::Freeze
            | Self::Collect(..)
            | Self::Start(_)
            | Self::Respond(..) => return,
        };
        assert!(
            of == session,
            "an op or a command of session {of:?} was answered or killed on session {session:?}: \
             expected the session that handed it out"
        );
    }

    // What it is answered once its session is forgotten: that the session has ended.
    fn gone(&self) -> Answer {
        let gone = || io::Error::new(io::ErrorKind::ConnectionAborted, "the session has ended");
        match self {
            Self::Collect(..) => Answer::Collected(Err(gone())),
            Self::Start(_) => Answer::Started(Err(gone())),
            Self::Put(..)
            | Self::Freeze
            | Self::Kill(_)
            | Self::Reply(..)
            | Self::Refuse(_)
            | Self::Respond(..) => Answer::Done(Err(gone())),
        }
    }
}

// The directory under which every sandbox needs `pumpkin_subdirs()` before its first command, and
// their puts.
fn set_up_puts() -> (AbsolutePath, Vec<Put>) {
    let puts = (pumpkin_subdirs().into_iter()).map(|dir| Put::Dir(dir, dir_mode()));
    (pumpkin_dir(), puts.collect())
}

// What a failed put of the directories under `root` that every sandbox needs says.
pub(crate) fn set_up_failed(root: &AbsolutePath, error: &io::Error) -> String {
    let failed = format!("the directories under `{root}` can't be put in the sandbox: {error}");
    // A deadline's error says what is expected.
    if error.kind() == io::ErrorKind::TimedOut {
        failed
    } else {
        format!(
            "{failed}: expected its `sandboxd` to put them, as the role's user, who owns `{root}`"
        )
    }
}
