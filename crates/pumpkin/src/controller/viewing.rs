//! The view's requests for the runs' data, asked of the controller's `serve` over a link
//! ([`Viewing`]), which is opened when a request needs it and kept between requests. What it
//! asks, and how it is answered, is the view's ([`crate::view`]).
//!
//! It never starts a stopped controller on its own, as the page asks every few seconds, which
//! would otherwise start it again and again, and pay for it, with no one reading: once a link
//! can't be opened, it asks EC2 whether the controller runs, and while it doesn't, or its `sshd`
//! doesn't answer, each request asks EC2 and that `sshd` again in place of opening a link, and
//! the controller starts only when [`Viewing::start`] is called.

use ::aws::Cli;

use super::aws::{self, answers};
use super::linking::{Build, Linked, Unlinked};
use super::{Name, Reached};
use crate::config;
use crate::link::Viewed;

/// Why the view on the controller gave no reply.
pub(crate) enum Unviewed {
    /// The controller can't answer now, as EC2 or a probe of its `sshd` says, as `message` says,
    /// and is `stopped`, so that a start would end that.
    Down { message: String, stopped: bool },
    /// Why, with what was being done.
    Failed(String),
}

/// The channel to the view on a controller, opened when a request needs it.
pub(crate) struct Viewing {
    reached: Reached,
    build: Build,
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
        Ok(Self {
            reached,
            build,
            linked: None,
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
    /// [`Unviewed::Down`] while the controller doesn't run or its `sshd` doesn't answer, else
    /// [`Unviewed::Failed`] if a link can't be opened, or the `serve` gives no reply but by the
    /// link's end, or the link opened anew ends too.
    pub(crate) fn ask(&mut self, target: &str, deflate: bool) -> Result<Viewed, Unviewed> {
        if self.down {
            self.answerable()?;
            self.down = false;
        }
        // What it asks for only reads, so it is safe to ask again.
        let asked = match self.once(target, deflate) {
            Ok(Err(Unlinked::Cut { .. })) => self.once(target, deflate),
            asked @ (Ok(Ok(_) | Err(Unlinked::Failed(_))) | Err(_)) => asked,
        };
        let failed = match asked {
            Ok(Ok(viewed)) => return Ok(viewed),
            Ok(Err(unlinked)) => self.failed(unlinked),
            Err(unopened) => {
                let failed = self.failed(unopened);
                return Err(match self.answerable() {
                    Ok(()) => Unviewed::Failed(failed),
                    Err(Unviewed::Failed(error)) => Unviewed::Failed(format!(
                        "{failed}; asking EC2 whether it runs failed too: {error}"
                    )),
                    Err(down @ Unviewed::Down { .. }) => {
                        self.down = true;
                        down
                    }
                });
            }
        };
        Err(Unviewed::Failed(failed))
    }

    /// Starts the controller's instance, which is stopped; a request then finds when it answers.
    ///
    /// # Errors
    ///
    /// Fails if EC2 refuses, as when the instance is stopping.
    pub(crate) fn start(&mut self) -> Result<(), String> {
        let Self { reached, cli, .. } = self;
        aws::start(
            connected(cli, reached)?,
            &reached.name,
            &reached.located.instance,
        )
    }

    /// Whether the controller runs, as EC2 says, and its `sshd` answers where it was found.
    fn answerable(&mut self) -> Result<(), Unviewed> {
        let Self { reached, cli, .. } = self;
        let Reached { name, located, .. } = &*reached;
        let state =
            connected(cli, reached).and_then(|cli| aws::state(cli, name, &located.instance));
        let message = match state.map_err(Unviewed::Failed)?.as_str() {
            "running" => match answers(located.address) {
                Ok(()) => return Ok(()),
                Err(error) => format!(
                    "controller `{name}` runs, but its `sshd` doesn't answer at {}: {error}: \
                     expected it to answer shortly after it starts",
                    located.address
                ),
            },
            "stopped" => {
                return Err(Unviewed::Down {
                    message: format!(
                        "controller `{name}` is stopped: start it, as the page's button does"
                    ),
                    stopped: true,
                });
            }
            state => format!("controller `{name}` is {state}, as EC2 says: expected it to run"),
        };
        Err(Unviewed::Down {
            message,
            stopped: false,
        })
    }

    /// The reply over the link, but never again over another; the outer error is why no link
    /// could be opened.
    fn once(&mut self, target: &str, deflate: bool) -> Result<Result<Viewed, Unlinked>, Unlinked> {
        let mut linked = match self.linked.take() {
            Some(linked) => linked,
            None => Linked::open(&self.reached, &self.build)?,
        };
        // What is left of the exchange would be read as the next one's, so a link that fails is
        // dropped, which ends its `ssh`.
        let viewed = match linked.view(&self.reached, target, deflate) {
            Ok(viewed) => viewed,
            Err(unlinked) => return Ok(Err(unlinked)),
        };
        Ok(match viewed {
            Ok(viewed) => {
                self.linked = Some(linked);
                Ok(viewed)
            }
            Err(message) => Err(Unlinked::Failed(format!(
                "controller `{}` refused: {message}",
                self.name()
            ))),
        })
    }

    /// What failed, as `unlinked` says, with where the log of its master is if that has ended.
    fn failed(&self, unlinked: Unlinked) -> String {
        match unlinked {
            Unlinked::Cut { master, said } => {
                let failed = format!(
                    "asking the view on controller `{}` failed, as `ssh` {said}",
                    self.name()
                );
                self.reached.lost(master, failed)
            }
            Unlinked::Failed(error) => error,
        }
    }
}

/// The connection to EC2 in the controller's region, made at its first use.
fn connected<'a>(cli: &'a mut Option<Cli>, reached: &Reached) -> Result<&'a Cli, String> {
    let made = match cli.take() {
        Some(made) => made,
        None => Cli::connect(reached.region.clone())?,
    };
    Ok(cli.insert(made))
}
