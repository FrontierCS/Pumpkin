//! What a run used, as its `outcome.json` keeps it.

use super::Record;
use super::ending::read_outcome;
use crate::log::{self, Used};
use crate::read_error::{ReadError, form};

impl Record {
    /// What the run used, as its `outcome.json` records it. `None` while it has none, or if it has
    /// one without what the run used, as a reap writes of a lost run.
    pub fn used(&self) -> Result<Option<Used>, ReadError> {
        let path = self.dir.join("outcome.json");
        let Some(outcome) = read_outcome(&path)? else {
            return Ok(None);
        };
        match outcome.get("usage") {
            None => Ok(None),
            Some(usage) => log::used(usage).map(Some).map_err(form(path.display())),
        }
    }
}
