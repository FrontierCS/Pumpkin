//! A session's hello and its answer, after which the session is open. The session reads the hello
//! on its socket, or takes the one that a meeting read on another thread. A session that fails
//! before it opens fails its accept.

use std::io;
use std::time::Duration;

use schema::sandbox::{HelloReply, RegistrationGrant};
use stdx::io::{self as sio, Io};

use super::{Phase, Session, Then};
use crate::{Answer, By, Owner, Told, frames};

impl Session {
    pub(crate) fn fail_accept<C: Owner>(
        &mut self,
        io: &mut Io<C>,
        error: io::Error,
        told: &mut Told<'_>,
    ) {
        let answer = match &mut self.phase {
            Phase::Accepting { answer, .. } | Phase::Welcoming { answer, .. } => answer.take(),
            Phase::Open => None,
        };
        if let Some(answer) = answer {
            told.answer(answer, Answer::Accepted(Err(error)));
        }
        // Nothing waits on a session that never opened.
        self.dropped = true;
        self.heard = false;
        self.end(io, "the session never opened".to_owned(), told);
    }

    pub(crate) fn accepted<C: Owner>(&mut self, io: &mut Io<C>, stream: sio::Stream) {
        self.close_listener(io);
        self.stream = Some(stream);
        self.receive(io);
    }

    pub(super) fn read_hello<C: Owner>(&mut self, io: &mut Io<C>, told: &mut Told<'_>) {
        let caps = (
            schema::sandbox::MAX_HELLO_BYTES,
            schema::sandbox::MAX_ATTESTATION_BYTES,
        );
        let hello = self
            .inbound
            .take(caps, |rest| schema::sandbox::read_hello(rest));
        let refused = |message: &str| io::Error::new(io::ErrorKind::InvalidData, message);
        let error = match hello {
            Ok(None) => return,
            Ok(Some((hello, None))) => {
                self.gpus = hello.gpus;
                return self.welcome(io, None, told);
            }
            Ok(Some((_, Some(_)))) => refused(
                "the hello presents an attestation on a socket of the sandbox's own, which \
                 identifies it: expected none there",
            ),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => error,
            Err(error) => {
                let error = crate::context("the sandbox's hello can't be read", &error);
                return self.fail_accept(io, error, told);
            }
        };
        let message = error.to_string();
        let reason = format!("the gateway refused the session: {message}");
        let bytes = frames::hello_reply(&HelloReply::Refused { message });
        let by = self.phase_by();
        let refusal = io::Error::new(io::ErrorKind::InvalidData, reason);
        self.push(io, bytes, by, Then::Refused(refusal));
    }

    fn phase_by(&self) -> By {
        let at = match &self.phase {
            Phase::Accepting { by, .. } | Phase::Welcoming { by, .. } => *by,
            Phase::Open => Duration::MAX,
        };
        By::request(at)
    }

    /// Answers the hello, presenting `grant`, after which the session is open.
    pub(crate) fn welcome<C: Owner>(
        &mut self,
        io: &mut Io<C>,
        grant: Option<RegistrationGrant>,
        told: &mut Told<'_>,
    ) {
        let by = self.phase_by();
        let answer = match &mut self.phase {
            Phase::Accepting { answer, .. } | Phase::Welcoming { answer, .. } => answer.take(),
            Phase::Open => None,
        };
        self.phase = Phase::Welcoming { by: by.at, answer };
        if self.now(io) >= by.at {
            return self.tick(io, self.now(io), told);
        }
        let bytes = frames::hello_reply(&HelloReply::Accepted { grant });
        self.push(io, bytes, by, Then::Welcomed);
        self.receive(io);
    }
}
