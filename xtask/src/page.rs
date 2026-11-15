//! The type check of the page that `pumpkin view` serves: TypeScript's compiler
//! ([`tools::TSC`]) checks its modules against the types that their doc comments give, as the
//! `tsconfig.json` beside them says.

use xshell::cmd;

use crate::{Cargo, Result, root, tools};

/// The page's directory, from the workspace's root.
const PAGE: &str = "crates/pumpkin/src/view/page";

/// Fails unless the page's modules pass their type check, the compiler printing each error with
/// its line and column.
pub(crate) fn check(cargo: &Cargo) -> Result<()> {
    let tsc = tools::found(cargo, tools::TSC)?;
    let config = root().join(PAGE).join("tsconfig.json");
    let checked = cmd!(cargo.sh, "{tsc} --project {config}").run();
    checked.map_err(|_| {
        "the page's modules fail their type check, at the lines that tsc printed above: fix \
         them, then run `cargo xtask ci` again"
            .into()
    })
}
