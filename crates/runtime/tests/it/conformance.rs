//! The simulated provider ([`Sim`]) against [`test_support::suite`].

use test_support::suite;

use crate::sim::Sim;

#[test]
fn suite() {
    suite::check(&[("sim", &Sim::new())]);
}
