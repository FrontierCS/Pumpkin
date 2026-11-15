//! The channel to `pumpkin controller view` on the controller: one `ssh` over a session of a
//! shared master, asked for the runs' data on its standard input and read from its standard
//! output, one request at a time ([`Viewing`]). What it asks, and in what frames, is the view's
//! ([`crate::view`]).
//!
//! The `pumpkin` that answers there must be of this build's commit, which a job that pins the
//! commit builds in the controller's workspace, as the page and the JSON it reads must be one
//! build's. So VS Code's server refuses a client of another commit
//! (<https://github.com/microsoft/vscode/blob/69176e5c7703e4ddf6b91a8ad5daf4597c9bea29/src/vs/server/node/remoteExtensionHostAgentServer.ts#L351-L357>),
//! and keeps a server for each commit
//! (<https://github.com/microsoft/vscode/blob/69176e5c7703e4ddf6b91a8ad5daf4597c9bea29/cli/src/tunnels/code_server.rs#L457-L470>).

use std::io::{self, BufReader};
use std::process::{Child, ChildStdin, ChildStdout, ExitStatus, Stdio};

use stdx::Context as _;

use super::{Name, Reached, SHORT_TIMEOUT, Session, Slot};
use crate::{config, next, workspace};

/// The channel to the view on a controller, opened when a request needs it.
pub(crate) struct Viewing {
    reached: Reached,
    /// The `pumpkin` of this build's commit, below the controller user's home.
    exe: String,
    channel: Option<Channel>,
}

/// An `ssh` that runs `pumpkin controller view` on the controller, with its standard input and
/// output, and the session of the master that it goes over, held until it is dropped.
struct Channel {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    slot: Slot,
}

impl Viewing {
    /// Reaches the controller that `client` names ([`Reached::reach`]), and checks that it has
    /// the `pumpkin` of this build's commit.
    ///
    /// # Errors
    ///
    /// Fails if this build has no commit, or the controller has no `pumpkin` of it, or can't be
    /// reached; each refusal ends with the command that views the copies of its runs here.
    pub(crate) fn reach(client: &config::Client) -> Result<Self, String> {
        let runs = crate::default_runs()?;
        let here = format!(
            "or view the copies of its runs here: pumpkin view --runs {}",
            next::quoted(&runs)
        );
        let name = &client.controller.name;
        let Some(commit) = runtime::BUILD.commit_hash() else {
            return Err(format!(
                "this pumpkin, {}, was built from no commit of Pumpkin's, so controller `{name}` \
                 has no build of it to read its runs: run the `pumpkin` that a repository's \
                 `./pumpkin` builds; {here}",
                runtime::BUILD
            ));
        };
        let reached = Reached::reach(&client.controller)?;
        let exe = format!("{}/{commit}/bin/pumpkin", workspace::DIR);
        let script = format!("cd\nif [ -x {exe} ]; then echo built; fi");
        let built = reached.call(&mut reached.ssh(&script)?, None, "ssh", SHORT_TIMEOUT)?;
        if built != b"built\n" {
            return Err(format!(
                "controller `{name}` has no build of this pumpkin's commit, {commit}, to read \
                 its runs: build it there from a task whose .pumpkinversion pins that commit, \
                 with pumpkin build <the task's file>; {here}"
            ));
        }
        Ok(Self {
            reached,
            exe,
            channel: None,
        })
    }

    pub(crate) fn name(&self) -> &Name {
        &self.reached.name
    }

    /// What `exchange` gives, run on the channel's standard input and output, which it opens if
    /// need be; and, once, on a channel opened anew, if the channel ended before `exchange` was
    /// done, as an idle one does.
    ///
    /// # Errors
    ///
    /// Fails if a channel can't be opened, `exchange` fails but by the channel's end, or the
    /// channel opened anew ends too: first with the command that reaches the controller again if
    /// it has stopped, then the error, which says where its master's log is if that has ended.
    pub(crate) fn ask<T>(
        &mut self,
        mut exchange: impl FnMut(&mut ChildStdin, &mut BufReader<ChildStdout>) -> io::Result<T>,
    ) -> Result<T, String> {
        // What `exchange` asks for only reads, so it is safe to ask again.
        let asked = match self.once(&mut exchange) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => self.once(&mut exchange).and_then(|again| again),
            Err(error) => Err(error),
        };
        asked.map_err(|error| {
            format!(
                "if controller `{}` has stopped, stop this view and start it again, which starts \
                 it: pumpkin view. What failed: {error}",
                self.reached.name
            )
        })
    }

    /// What `exchange` gives over the channel, as [`ask`](Self::ask) says, but never again:
    /// `Ok(Err(error))` if the channel ended before `exchange` was done.
    fn once<T>(
        &mut self,
        exchange: &mut impl FnMut(&mut ChildStdin, &mut BufReader<ChildStdout>) -> io::Result<T>,
    ) -> Result<Result<T, String>, String> {
        let mut channel = match self.channel.take() {
            Some(channel) => channel,
            None => self.open()?,
        };
        let error = match exchange(&mut channel.stdin, &mut channel.stdout) {
            Ok(value) => {
                self.channel = Some(channel);
                return Ok(Ok(value));
            }
            Err(error) => error,
        };
        let master = channel.slot.master;
        // What is left of the exchange would be read as the next one's.
        let status = channel.end()?;
        let failed = format!(
            "asking the view on controller `{}` failed: {error}; `ssh` ended with {status}",
            self.reached.name
        );
        let failed = self.reached.lost(master, failed);
        match error.kind() {
            io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe => Ok(Err(failed)),
            _ => Err(failed),
        }
    }

    /// A channel to `pumpkin controller view` there, over a session of a master that answers.
    fn open(&self) -> Result<Channel, String> {
        let script = format!("cd\nexec {} controller view", self.exe);
        // Over the masters alone, never reaching the controller as `Reached::reach` does: a page
        // left open asks again every few seconds, which would start a stopped controller again and
        // again, and pay for it, with no one reading.
        let Session { mut command, slot } = self.reached.ssh(&script)?;
        let child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn();
        let mut child = child.context("running `ssh` failed")?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            unreachable!(
                "the child's standard input or output is missing, though both are piped just above"
            )
        };
        Ok(Channel {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            slot,
        })
    }
}

impl Channel {
    /// Ends the channel: kills its `ssh`, if it hasn't ended, and gives how it ended.
    fn end(self) -> Result<ExitStatus, String> {
        let Self {
            mut child, stdin, ..
        } = self;
        drop(stdin);
        child.kill().context("stopping `ssh` failed")?;
        child.wait().context("waiting for `ssh` failed")
    }
}
