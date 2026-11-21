//! The process control `std` lacks.
//!
//! A child gets only what of this process's environment its caller states ([`command`]). It
//! either runs to its end within the bound that its caller states ([`run`]), which ends what it
//! started too, or runs on its own, followed by its caller ([`spawn`]).

use std::time::Duration;

mod children;
mod command;
mod failure;
mod run;

pub use children::{kill_group, spawn, wait_any};
pub use command::{Environment, cargo_package_vars, command, shown};
pub use failure::{Failure, Missing};
pub use run::{Bound, Echo, Run, Stream, fed, output, printed, run};

const FIRST_PAUSE: Duration = Duration::from_millis(1);

// The longest pause between checks that a command has ended. A command that starts a process of
// its own takes milliseconds at least, so ending up to 10 ms after it is noise beside it, and the
// checks wake this thread at most 100 times a second.
const MAX_PAUSE: Duration = Duration::from_millis(10);
