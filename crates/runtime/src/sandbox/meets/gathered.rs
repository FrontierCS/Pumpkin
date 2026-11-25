//! What the meetings of a sandbox's containers come to, once each has ended.

use std::collections::BTreeMap;

use schema::ir::Container;

use super::{Companions, Opened, Sessions};

pub(super) struct Failure {
    pub(super) reason: String,
    /// Whether it is the first failure among its sandbox's containers, which stopped the others.
    pub(super) first: bool,
}

/// The sessions of a sandbox whose containers' meetings went so, `main`'s and each companion's,
/// if each opened one. Otherwise the failure that stopped the others, with its container, and
/// every session that opened is dropped, which ends it.
pub(super) fn gathered<C>(
    main: Opened<C>,
    companions: Companions<C>,
) -> Result<Sessions<C>, (Container, Failure)> {
    let mut sessions = (main.session)
        .map(|main| Sessions {
            main,
            companions: BTreeMap::new(),
        })
        .map_err(|failure| (Container::Main, failure));
    for (name, opened) in companions {
        sessions = match (sessions, opened.session) {
            (Ok(mut sessions), Ok(session)) => {
                sessions.companions.insert(name, session);
                Ok(sessions)
            }
            (Ok(_), Err(failure)) => Err((Container::Companion(name), failure)),
            // The failure that stopped the others is the sandbox's, wherever its container is
            // declared.
            (Err(_), Err(failure)) if failure.first => Err((Container::Companion(name), failure)),
            (Err(failed), Ok(_) | Err(_)) => Err(failed),
        };
    }
    sessions
}
