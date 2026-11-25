//! How a new sandbox meets the gateway, as its provider's [`Meeting`](provider::Meeting) says.

use std::io;

use gateway::{Dialer, Listener, Sockets};
use provider::{Admits, Create, Created, Creator, ListenDials, Listened};
use schema::sandbox::{PrivateDial, SocketDial};
use stdx::Deadline;

/// How a provider's sandboxes and the gateway meet ([`Meeting`](provider::Meeting)), with what the
/// run supplies for it.
#[derive(Clone, Copy)]
pub(super) enum Meets<'p> {
    Sockets(&'p dyn Creator<SocketDial>),
    Private(
        &'p dyn Creator<PrivateDial, (Created, Admits)>,
        &'p Listener,
    ),
    Dials(&'p dyn Creator<ListenDials, Listened>, &'p Dialer),
}

impl Meets<'_> {
    /// Issues what the new sandbox dials, creates it, and returns the registration of its session,
    /// which awaits it by `by`.
    pub(super) fn create(
        self,
        sockets: &Sockets,
        create: &Create,
        by: Deadline,
    ) -> Result<(Created, gateway::Registration), String> {
        let issued = |error: io::Error| error.to_string();
        match self {
            Self::Sockets(creator) => {
                let (dial, registration) = sockets.issue(&create.name).map_err(issued)?;
                Ok((creator.create(create, dial)?, registration))
            }
            Self::Private(creator, listener) => {
                let (dial, unattested) = listener.issue(by);
                let (created, admits) = creator.create(create, dial)?;
                Ok((created, unattested.admitting(admits)))
            }
            Self::Dials(creator, dialer) => {
                let (main, unreached) = dialer.issue().map_err(issued)?;
                let dials = ListenDials {
                    main,
                    companions: Vec::new(),
                };
                // Given no companions, the create gives no relay.
                let Listened {
                    created,
                    main,
                    relay: _,
                } = creator.create(create, dials)?;
                Ok((created, unreached.at(main)))
            }
        }
    }
}
