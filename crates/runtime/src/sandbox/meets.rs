//! How each container of a new sandbox meets the gateway, as its provider's `Meeting` says.

use std::io;

use gateway::{Dialer, Listener, Sockets};
use provider::{Admits, Create, Created, Creator, Dials};
use schema::ir::Name;
use schema::sandbox::{Endpoint, Listening, PrivateDial, Relayed, SocketDial};
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
    Dials(
        &'p dyn Creator<Listening, (Created, Dials<Endpoint>), Relayed>,
        &'p Dialer,
    ),
}

impl Meets<'_> {
    /// Issues what each container of the new sandbox dials, creates it, and returns the
    /// registrations of the sessions of its own container and of each of its companions, in their
    /// order, which await them by `by`.
    pub(super) fn create(
        self,
        sockets: &Sockets,
        create: &Create,
        by: Deadline,
    ) -> Result<(Created, Registrations), String> {
        let issued = |error: io::Error| error.to_string();
        match self {
            Self::Sockets(creator) => {
                let (main, registration) = sockets.issue(&create.name).map_err(issued)?;
                let (mut dials, mut companions) = (Vec::new(), Vec::new());
                for index in 0..create.companions.len() {
                    let name = companion_socket(&create.name, index);
                    let (dial, registration) = sockets.issue(&name).map_err(issued)?;
                    dials.push(dial);
                    companions.push(registration);
                }
                let dials = Dials {
                    main,
                    companions: dials,
                };
                let created = creator.create(create, dials)?;
                Ok((created, (registration, companions)))
            }
            Self::Private(creator, listener) => {
                assert!(
                    create.companions.is_empty(),
                    "a private provider was asked for a sandbox that holds companions, though none \
                     offers them (`Offers::companions`)"
                );
                let (dial, unattested) = listener.issue(by);
                let (created, admits) = creator.create(create, alone(dial))?;
                Ok((created, (unattested.admitting(admits), Vec::new())))
            }
            Self::Dials(creator, dialer) => {
                let (main, unreached) = dialer.issue().map_err(issued)?;
                let (mut relayed, mut unrelayed) = (Vec::new(), Vec::new());
                for companion in &create.companions {
                    let (dial, unreached) = dialer.relayed(&companion.name).map_err(issued)?;
                    relayed.push(dial);
                    unrelayed.push(unreached);
                }
                let dials = Dials {
                    main,
                    companions: relayed,
                };
                let (created, endpoints) = creator.create(create, dials)?;
                assert_eq!(
                    endpoints.companions.len(),
                    unrelayed.len(),
                    "a provider whose sandboxes listen gave other than one endpoint for each \
                     companion of a sandbox it created"
                );
                let companions = (unrelayed.into_iter().zip(endpoints.companions))
                    .map(|(unreached, endpoint)| unreached.at(endpoint));
                let registrations = (unreached.at(endpoints.main), companions.collect());
                Ok((created, registrations))
            }
        }
    }
}

/// The registrations of a sandbox's sessions: its own container's, and each of its companions', in
/// their order.
pub(super) type Registrations = (gateway::Registration, Vec<gateway::Registration>);

/// The dials of a sandbox that holds no companions.
fn alone<D>(main: D) -> Dials<D> {
    Dials {
        main,
        companions: Vec::new(),
    }
}

/// The name of the socket of the `index`th companion of sandbox `name`, which no sandbox's name
/// is, as none ends in `-c` and digits ([`Which::name`](super::Which::name)).
fn companion_socket(name: &Name, index: usize) -> Name {
    let Ok(socket) = Name::try_from(format!("{name}-c{index}")) else {
        unreachable!(
            "a companion's socket's name is no name, though it is a sandbox's name, at most 26 \
             characters, `-c` and decimal digits"
        )
    };
    socket
}
