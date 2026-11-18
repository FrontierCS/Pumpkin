//! The view's requests for the runs' data, asked of the controller's `serve` over a link
//! ([`Viewing`]), which is opened when a request needs it and kept between requests. What it
//! asks, and how it is answered, is the view's ([`crate::view`]).
//!
//! It reaches the controller over the masters alone, as the page asks every few seconds, which
//! would otherwise start a stopped controller again and again, and pay for it, with no one reading.

use super::linking::{Build, Linked, Unlinked};
use super::{Name, Reached};
use crate::config;
use crate::link::{Answer, Ask};

/// The channel to the view on a controller, opened when a request needs it.
pub(crate) struct Viewing {
    reached: Reached,
    build: Build,
    linked: Option<Linked>,
}

/// The reply to a request, as the view's HTTP gives it.
pub(crate) struct Reply {
    /// The status line's code and reason.
    pub(crate) status: String,
    pub(crate) content_type: String,
    /// Whether `body` is deflated.
    pub(crate) deflated: bool,
    pub(crate) body: Vec<u8>,
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
    /// Fails if a link can't be opened, or the `serve` gives no reply but by the link's end, or
    /// the link opened anew ends too: with the command that reaches the controller again if it
    /// has stopped, then the error, which says where its master's log is if that has ended.
    pub(crate) fn ask(&mut self, target: &str, deflate: bool) -> Result<Reply, String> {
        let ask = Ask::View {
            target: target.to_owned(),
            deflate,
        };
        // What it asks for only reads, so it is safe to ask again.
        let asked = match self.once(&ask) {
            Err(Unlinked::Cut { .. }) => self.once(&ask),
            asked => asked,
        };
        let name = self.name();
        let failed = match asked {
            Ok(reply) => return Ok(reply),
            Err(Unlinked::Cut { master, said }) => {
                let failed =
                    format!("asking the view on controller `{name}` failed, as `ssh` {said}");
                self.reached.lost(master, failed)
            }
            Err(Unlinked::Failed(error)) => error,
        };
        Err(format!(
            "if controller `{name}` has stopped, stop this view and start it again, which starts \
             it: pumpkin view. What failed: {failed}"
        ))
    }

    /// The reply over the link, but never again over another.
    fn once(&mut self, ask: &Ask) -> Result<Reply, Unlinked> {
        let mut linked = match self.linked.take() {
            Some(linked) => linked,
            None => Linked::open(&self.reached, &self.build)?,
        };
        // What is left of the exchange would be read as the next one's, so a link that fails is
        // dropped, which ends its `ssh`.
        let answer = linked.ask(ask)?;
        let name = self.name();
        match answer {
            Answer::Viewed {
                status,
                content_type,
                deflated,
                body,
            } => {
                self.linked = Some(linked);
                Ok(Reply {
                    status,
                    content_type,
                    deflated,
                    body,
                })
            }
            Answer::Refused { message } => Err(Unlinked::Failed(format!(
                "controller `{name}` refused: {message}"
            ))),
            Answer::Health(_) | Answer::Listed(_) => Err(Unlinked::Failed(format!(
                "controller `{name}` answered a request for the view with the answer to another: \
                 expected its own, as this pumpkin's `serve` gives it"
            ))),
        }
    }
}
