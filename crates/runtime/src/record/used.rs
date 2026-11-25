//! What a run used, as its `outcome.json` keeps it.

use serde_json::Value;

use super::Record;
use super::ending::read_outcome;
use crate::DriverUse;
use crate::log::{self, Used};
use crate::read_error::{ReadError, form};

impl Record {
    /// What the run used, as its `outcome.json` records it. `None` while it has none, or if it has
    /// one without what the run used, as a reap writes of a lost run.
    pub fn used(&self) -> Result<Option<Used>, ReadError> {
        self.outcome_part("usage", log::used)
    }

    /// What the run's driver used, as its `outcome.json` records it. `None` while it has none, or
    /// if it has one without what the driver used, as a reap writes of a lost run.
    pub fn driver_use(&self) -> Result<Option<DriverUse>, ReadError> {
        self.outcome_part("driver", log::driver_use)
    }

    fn outcome_part<T>(
        &self,
        name: &str,
        read: impl FnOnce(&Value) -> Result<T, String>,
    ) -> Result<Option<T>, ReadError> {
        let path = self.dir.join("outcome.json");
        let Some(outcome) = read_outcome(&path)? else {
            return Ok(None);
        };
        match outcome.get(name) {
            None => Ok(None),
            Some(part) => read(part).map(Some).map_err(form(path.display())),
        }
    }
}
