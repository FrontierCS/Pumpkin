//! The process control `std` lacks: commands whose caller chooses what of the environment
//! they get, a process group's kill and a bounded run ([`output`]).

use std::time::Duration;

mod children;
mod command;
mod run;

pub use children::{kill_group, wait_any};
pub use command::{Environment, cargo_package_vars, command};
pub use run::{fed, output};

const FIRST_PAUSE: Duration = Duration::from_millis(1);

// The longest pause between checks that a command has ended. A command that starts a process of
// its own takes milliseconds at least, so ending up to 10 ms after it is noise beside it, and the
// checks wake this thread at most 100 times a second.
const MAX_PAUSE: Duration = Duration::from_millis(10);
