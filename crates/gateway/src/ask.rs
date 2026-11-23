//! What a session is asked, by its loop's owner ([`Sessions::ask`]) or by a thread's handle
//! ([`crate::Remote`]), and the one dispatch that asks it on the loop.

use std::io;
use std::path::PathBuf;
use std::sync::mpsc::SyncSender;
use std::time::Duration;

use schema::ir::AbsolutePath;
use schema::sandbox::op::OpReply;
use schema::sandbox::tree::Limits;
use schema::sandbox::{Launch, Root, ToSandbox, dir_mode, pumpkin_dir, pumpkin_subdirs};
use stdx::io::Io;

use crate::remote::ToThread;
use crate::session::Session;
use crate::{
    Answer, Answerer, IncomingOp, Owner, Process, Put, RefusedOp, SessionId, Sessions, Told,
};

/// What the loop's owner asks of a session ([`Sessions::ask`]), each as the method of
/// [`crate::Remote`]'s of the same name does.
#[derive(Debug)]
pub enum Ask {
    /// Puts the entries below the directory, made as the root says.
    Put(AbsolutePath, Root, Vec<Put>),
    Freeze,
    /// Collects the tree at the path into the directory, within the limits.
    Collect(AbsolutePath, PathBuf, Limits),
    Start(Launch),
    Reply(IncomingOp, OpReply),
    Refuse(RefusedOp),
}

/// What a session is asked, by its owner or by a thread's handle, each with its deadline but for
/// what answers nothing.
pub(crate) enum Asked {
    Put {
        dir: AbsolutePath,
        root: Root,
        entries: Vec<Put>,
        // Where the put asks for its next entries, if its caller holds more.
        more: Option<SyncSender<ToThread>>,
        by: Duration,
    },
    // The next entries of the put on the stream, of which its caller holds more if the flag says
    // so.
    PutMore(u32, Vec<Put>, bool),
    Freeze(Duration),
    Collect {
        path: AbsolutePath,
        into: std::path::PathBuf,
        limits: Limits,
        by: Duration,
    },
    Start(Launch, Duration),
    Kill(Process, Duration),
    Reply(IncomingOp, OpReply, Duration),
    Refuse(RefusedOp, Duration),
    Respond(ToSandbox, Vec<u8>, Duration),
    Ended,
    // Gives up each collect of the group's, of a handle in none if none, on the session, which
    // then fails as interrupted.
    Cancel(Option<u64>),
    Drop,
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
    /// Panics if the op that `ask` replies to or refuses arrived on another session.
    pub fn ask<C: Owner>(
        owner: &mut C,
        io: &mut Io<C>,
        id: SessionId,
        ask: Ask,
        (by, token): (Duration, u64),
    ) -> Option<Process> {
        let asked = ask.by(by);
        asked.check_session(id);
        let process = Self::dispatch_to(owner, io, id, asked, Answerer::Owner(token));
        owner.sessions().flush_later(io);
        process
    }

    /// Puts what every sandbox needs before its first command, as [`crate::Remote::set_up`]
    /// does, and answers as [`Sessions::ask`] does. A failure's error says what failed.
    pub fn set_up<C: Owner>(
        owner: &mut C,
        io: &mut Io<C>,
        id: SessionId,
        (by, token): (Duration, u64),
    ) {
        let (root, puts) = set_up_puts();
        let asked = Ask::Put(root, Root::Kept, puts).by(by);
        Self::dispatch_to(owner, io, id, asked, Answerer::SetUp(token));
        owner.sessions().flush_later(io);
    }

    // Asks the session `id` for what `asked` says, whose answer `answerer` hears: for a start,
    // the process that its exit will name.
    pub(crate) fn dispatch_to<C: Owner>(
        owner: &mut C,
        io: &mut Io<C>,
        id: SessionId,
        asked: Asked,
        answerer: Answerer,
    ) -> Option<Process> {
        let (session, mut told) = owner.sessions().held(id);
        let process = if let Some(session) = session {
            dispatch(session, io, asked, answerer, &mut told)
        } else {
            // A session already forgotten, as its owner dropped it, answers nothing more but that
            // it has ended, as to a response's or a tree's handle, which outlive the session's.
            if let Some(gone) = asked.gone() {
                told.answer(answerer, gone);
            }
            None
        };
        owner.sessions().reap(io, id);
        process
    }
}

// Asks `session` for what `asked` says, whose answer `answerer` hears: for a start, the process
// that its exit will name.
fn dispatch<C: Owner>(
    session: &mut Session,
    io: &mut Io<C>,
    asked: Asked,
    answerer: Answerer,
    told: &mut Told<'_>,
) -> Option<Process> {
    match asked {
        Asked::Put {
            dir,
            root,
            entries,
            more,
            by,
        } => session.put(io, (dir, root, entries, more), (answerer, by), told),
        Asked::PutMore(stream, entries, more) => {
            session.put_more(io, stream, (entries, more), told);
        }
        Asked::Freeze(by) => {
            let freeze = |stream| ToSandbox::Freeze { stream };
            session.request(io, freeze, (answerer, by), told);
        }
        Asked::Collect {
            path,
            into,
            limits,
            by,
        } => session.collect(io, (path, into, limits), (answerer, by), told),
        Asked::Start(launch, by) => return session.start(io, launch, (answerer, by), told),
        Asked::Kill(process, by) => session.kill(io, process, (answerer, by), told),
        Asked::Reply(op, reply, by) => session.reply(io, op, reply, (answerer, by), told),
        Asked::Refuse(refused, by) => session.refuse(io, &refused, (answerer, by), told),
        Asked::Respond(message, body, by) => {
            session.respond(io, (message, body), (answerer, by), told);
        }
        Asked::Ended => {
            let ended = session.ended().map_or(Ok(()), |reason| {
                Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    reason.to_owned(),
                ))
            });
            told.answer(answerer, Answer::Done(ended));
        }
        Asked::Cancel(group) => session.cancel_group(group, told),
        Asked::Drop => session.drop_by_owner(io, told),
    }
    None
}

impl Ask {
    // The same, by `by`, as the dispatch takes it.
    pub(crate) fn by(self, by: Duration) -> Asked {
        match self {
            Self::Put(dir, root, entries) => Asked::Put {
                dir,
                root,
                entries,
                more: None,
                by,
            },
            Self::Freeze => Asked::Freeze(by),
            Self::Collect(path, into, limits) => Asked::Collect {
                path,
                into,
                limits,
                by,
            },
            Self::Start(launch) => Asked::Start(launch, by),
            Self::Reply(op, reply) => Asked::Reply(op, reply, by),
            Self::Refuse(refused) => Asked::Refuse(refused, by),
        }
    }
}

impl Asked {
    // Panics if the op that it answers, or the command that it kills, is another session's than
    // `session`.
    pub(crate) fn check_session(&self, session: SessionId) {
        let of = match self {
            Self::Kill(process, _) => process.session,
            Self::Reply(op, ..) => op.session,
            Self::Refuse(refused, _) => refused.session,
            Self::Put { .. }
            | Self::PutMore(..)
            | Self::Freeze(_)
            | Self::Collect { .. }
            | Self::Start(..)
            | Self::Respond(..)
            | Self::Ended
            | Self::Cancel(_)
            | Self::Drop => return,
        };
        assert!(
            of == session,
            "an op or a command of session {of:?} was answered or killed on session {session:?}: \
             expected the session that handed it out"
        );
    }

    // What it is answered once its session is forgotten: that the session has ended, unless
    // nothing answers it, as a put's next entries, which come after the put's answer.
    fn gone(&self) -> Option<Answer> {
        match self {
            Self::Collect { .. } => Some(Answer::Collected(Err(gone()))),
            Self::Start(..) => Some(Answer::Started(Err(gone()))),
            Self::Put { .. }
            | Self::Freeze(_)
            | Self::Kill(..)
            | Self::Reply(..)
            | Self::Refuse(..)
            | Self::Respond(..)
            | Self::Ended => Some(Answer::Done(Err(gone()))),
            Self::PutMore(..) | Self::Cancel(_) | Self::Drop => None,
        }
    }
}

// The directory under which every sandbox needs `pumpkin_subdirs()` before its first command, and
// their puts.
pub(crate) fn set_up_puts() -> (AbsolutePath, Vec<Put>) {
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

// Why a request of a session already forgotten fails.
fn gone() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, "the session has ended")
}
