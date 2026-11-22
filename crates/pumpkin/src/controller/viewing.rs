//! The view's requests for the runs' data, asked of the controller's `serve` over a link
//! ([`Viewing`]), which is opened when a request needs it and kept between requests. What it
//! asks, and how it is answered, is the view's ([`crate::view`]).
//!
//! It never starts a stopped controller on its own, as the page asks every few seconds, which
//! would otherwise start it again and again, and pay for it, with no one reading: once a link
//! can't be opened, it asks EC2 whether the controller runs, and while it is stopped or on its
//! way to running or to being stopped, or its `sshd` doesn't answer at its private address on a
//! direct connection, each request asks EC2 and that `sshd` again in place of opening a link; the
//! controller starts only when
//! [`Viewing::start`] is called. A request opens a link with one attempt, as the page asks again
//! after a pause of its own, and the view answers no other request while one waits.

use ::aws::Cli;

use super::aws::{self, Located, answers, unanswered};
use super::linking::{Build, Linked, Unlinked};
use super::{Name, Reached};
use crate::config::Connection;
use crate::link::{ViewRequest, Viewed};
use crate::{config, say};

/// Why the view on the controller gave no reply.
pub(crate) enum Unviewed {
    /// The controller is stopped, as EC2 says, so that a start would end that.
    Stopped,
    /// The controller can't answer now, as EC2 or a probe of its `sshd` says: why.
    Down(String),
    /// Why, with what was being done.
    Failed(String),
}

/// What one request over one link came to.
enum Over {
    Viewed(Viewed),
    /// The link gave no reply.
    Unanswered(Unlinked),
    /// No link could be opened.
    Unopened(Unlinked),
}

/// The channel to the view on a controller, opened when a request needs it.
pub(crate) struct Viewing {
    reached: Reached,
    build: Build,
    located: Located,
    linked: Option<Linked>,
    /// Whether the controller couldn't answer at the last request, so that the next asks EC2 and
    /// its `sshd` first rather than wait out a connection to an instance that doesn't run.
    down: bool,
    /// Connected at the first call to EC2.
    cli: Option<Cli>,
}

impl Viewing {
    /// Reaches the controller that `client` names ([`Reached::reach`]), which starts it if it is
    /// stopped.
    ///
    /// # Errors
    ///
    /// Fails if this executable can't be read, or the controller can't be reached.
    pub(crate) fn reach(client: &config::Client) -> Result<Self, String> {
        let expected = "expected to send it to the controller, to read the runs there";
        let build = Build::of_this(expected)?;
        let reached = Reached::reach(&client.controller)?;
        let (linked, found) = reached.opening(&build)?;
        Ok(Self {
            reached,
            build,
            located: found.at,
            linked: Some(linked),
            down: false,
            cli: None,
        })
    }

    pub(crate) fn name(&self) -> &Name {
        &self.reached.name
    }

    /// The reply to a `GET` of `target`, deflated if `deflate` and that is shorter, over the link,
    /// which is opened if need be, and once more over another if the link ended before the reply
    /// was whole, as an idle one does.
    ///
    /// # Errors
    ///
    /// [`Unviewed::Stopped`] or [`Unviewed::Down`] while the controller can't answer, as they
    /// say; else [`Unviewed::Failed`] if a link can't be opened, or the `serve` gives no reply but
    /// by the link's end, or the link opened anew ends too.
    pub(crate) fn ask(&mut self, target: &str, deflate: bool) -> Result<Viewed, Unviewed> {
        if self.down {
            // An EC2 that can't be asked says nothing of the controller, which a link may reach.
            match self.answerable() {
                Err(down @ (Unviewed::Stopped | Unviewed::Down(_))) => return Err(down),
                Ok(()) | Err(Unviewed::Failed(_)) => self.down = false,
            }
        }
        // What it asks for only reads, so it is safe to ask again.
        let over = match self.once(target, deflate) {
            Over::Unanswered(Unlinked::Cut { .. }) => self.once(target, deflate),
            over @ (Over::Viewed(_)
            | Over::Unanswered(Unlinked::HostKey(_) | Unlinked::Failed(_))
            | Over::Unopened(_)) => over,
        };
        let unopened = match over {
            Over::Viewed(viewed) => return Ok(viewed),
            Over::Unanswered(unlinked) => return Err(Unviewed::Failed(Self::failed(unlinked))),
            Over::Unopened(unlinked) => Self::failed(unlinked),
        };
        Err(match self.answerable() {
            Ok(()) => Unviewed::Failed(unopened),
            Err(Unviewed::Failed(error)) => Unviewed::Failed(format!(
                "{unopened}; asking EC2 whether it runs failed too: {error}"
            )),
            Err(down @ (Unviewed::Stopped | Unviewed::Down(_))) => {
                self.down = true;
                down
            }
        })
    }

    /// Starts the controller's instance, as `start-instances` does, and says so; a request then
    /// finds when it answers.
    ///
    /// # Errors
    ///
    /// Fails if EC2 refuses, as when the instance is stopping.
    pub(crate) fn start(&mut self) -> Result<(), String> {
        let Self { reached, cli, .. } = self;
        let id = &self.located.instance;
        say(format_args!(
            "starting controller `{}`, {id}, as the page asked",
            reached.name
        ));
        aws::start(connected(cli, reached)?, id)
    }

    /// Whether the controller runs, as EC2 says, and, on a direct connection, its `sshd` answers
    /// where it was found. Through the endpoint, only a link says whether `sshd` answers.
    fn answerable(&mut self) -> Result<(), Unviewed> {
        let Self { reached, cli, .. } = self;
        let name = &reached.name;
        let located = &self.located;
        let state =
            connected(cli, reached).and_then(|cli| aws::state(cli, name, &located.instance));
        let address = located.address;
        let down = match state.map_err(Unviewed::Failed)?.as_str() {
            "running" if matches!(reached.connection, Connection::Endpoint) => return Ok(()),
            "running" => match answers(address) {
                Ok(()) => return Ok(()),
                Err(error) => format!(
                    "controller `{name}` runs, but its `sshd` doesn't answer at {address}: \
                     {error}: one just started answers within a minute; past that, {}",
                    unanswered(&error)
                ),
            },
            "stopped" => return Err(Unviewed::Stopped),
            "pending" => format!("controller `{name}` is starting"),
            "stopping" => format!("controller `{name}` is stopping: start it once it has stopped"),
            // As when it was terminated, and another made in its place, which `reach` finds.
            state => {
                return Err(Unviewed::Failed(format!(
                    "controller `{name}`, {}, is {state}: stop this view and start it again, \
                     which finds the controller anew",
                    located.instance
                )));
            }
        };
        Err(Unviewed::Down(down))
    }

    /// The reply over the link, opened if need be, but never again over another.
    fn once(&mut self, target: &str, deflate: bool) -> Over {
        let mut linked = match self.linked.take() {
            Some(linked) => linked,
            // Once, as the page asks again after a pause, which lets the view answer others.
            None => match Linked::open(&self.reached, &self.build, &self.located) {
                Ok(linked) => linked,
                Err(unlinked) => return Over::Unopened(unlinked),
            },
        };
        // What is left of the exchange would be read as the next one's, so a link that fails is
        // dropped, which ends its `ssh`.
        let target = target.to_owned();
        match linked.ask(&self.reached, ViewRequest { target, deflate }) {
            Ok(Ok(viewed)) => {
                self.linked = Some(linked);
                Over::Viewed(viewed)
            }
            Ok(Err(message)) => Over::Unanswered(Unlinked::Failed(format!(
                "controller `{}` refused: {message}",
                self.name()
            ))),
            Err(unlinked) => Over::Unanswered(unlinked),
        }
    }

    fn failed(unlinked: Unlinked) -> String {
        match unlinked {
            Unlinked::Cut { said, .. } | Unlinked::HostKey(said) | Unlinked::Failed(said) => said,
        }
    }
}

/// The connection to EC2 in the controller's region, made at its first use.
fn connected<'a>(cli: &'a mut Option<Cli>, reached: &Reached) -> Result<&'a Cli, String> {
    match cli {
        Some(made) => Ok(made),
        None => Ok(cli.insert(Cli::connect(reached.region.clone())?)),
    }
}
