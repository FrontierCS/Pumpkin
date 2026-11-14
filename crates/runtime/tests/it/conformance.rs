//! The simulated provider ([`Sim`]) against the suite every provider passes
//! ([`test_support::suite`]): as it is, and as a provider that frees an ended sandbox itself, whose
//! outcomes are the same, as `Sweep::list` allows it.

use std::path::{Path, PathBuf};

use provider::{Provider, RunTag, SandboxId};
use test_support::suite::{self, Locals, Subject};

use crate::sim::{Sim, VANISHING};

#[test]
fn suite() {
    suite::check(&[("sim", &Sim::new()), ("freeing", &Freeing(Sim::new()))]);
}

/// The simulated provider, whose mute `sandboxd` is [`VANISHING`]'s, so that it frees the
/// suite's ended sandbox itself.
struct Freeing(Sim);

impl Subject for Freeing {
    fn connect(&self) -> Result<Box<dyn Provider + '_>, String> {
        self.0.connect()
    }

    fn locals(&self, dir: &Path) -> Locals {
        Locals {
            mute: PathBuf::from(VANISHING),
            ..self.0.locals(dir)
        }
    }

    fn stall(&self, id: &SandboxId) {
        self.0.stall(id);
    }

    fn held(&self, run: &RunTag) -> Vec<SandboxId> {
        self.0.held(run)
    }
}
