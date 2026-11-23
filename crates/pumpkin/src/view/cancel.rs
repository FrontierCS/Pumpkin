//! The page's cancel of a run: `pumpkin cancel` of it, run as a child of the server, which
//! answers at once and reaps the child as the page next asks what it says, so that the page shows
//! the command's own words.
//!
//! A child, as a cancel waits until the run has ended and its sandboxes are deleted, minutes on a
//! provider whose sandboxes are instances, while the view answers one request at a time.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read as _;
use std::process::{Child, ExitStatus, Stdio};

use provider::RunTag;
use serde_json::json;
use stdx::Context as _;
use stdx::process::Environment;
use tempfile::TempDir;

use super::http::Response;
use crate::{next, workspace};

/// The most bytes of a cancel's output that the page reads: a cancel says a few lines.
const SAID_MAX_BYTES: u64 = 64 * 1024;

/// The cancels started so far, the latest of each run.
#[derive(Default)]
pub(super) struct Cancels {
    by_run: BTreeMap<String, Cancel>,
}

struct Cancel {
    child: Child,
    ended: Option<ExitStatus>,
    said: Said,
}

/// A cancel's output: the file the process writes, in a directory of its own, until the process
/// has ended and the output is read for the last time.
enum Said {
    Saying(TempDir),
    Said(String),
}

const SAID: &str = "said";

impl Cancels {
    /// Starts `pumpkin cancel` of run `id`, where `at` says, and answers with what it has said.
    ///
    /// Refuses with `409` if a cancel of the run is still going.
    pub(super) fn start(&mut self, id: &RunTag, at: &next::At) -> Response {
        let under_way = match self.by_run.get_mut(id.as_str()) {
            Some(cancel) => cancel.reaped().map(|ended| ended.is_none()),
            None => Ok(false),
        };
        match under_way {
            Ok(true) => {
                return Response::refused(
                    "409 Conflict",
                    &format!(
                        "the page's cancel of run {0} goes on: read what it says: GET \
                         /api/runs/{0}/cancelling",
                        id.as_str()
                    ),
                );
            }
            Ok(false) => {}
            Err(error) => return Response::refused("500 Internal Server Error", &error),
        }
        match self.started(id, at) {
            Ok(()) => self.said(id).with_status("202 Accepted"),
            Err(error) => Response::refused("500 Internal Server Error", &error),
        }
    }

    /// What the latest cancel of run `id` has said, whether it ended and its exit code; refused if
    /// none was started.
    pub(super) fn said(&mut self, id: &RunTag) -> Response {
        let Some(cancel) = self.by_run.get_mut(id.as_str()) else {
            return Response::refused(
                "404 Not Found",
                &format!(
                    "the page started no cancel of run {0}: cancel it from the page, by POST \
                     /api/runs/{0}/cancel",
                    id.as_str()
                ),
            );
        };
        let said = cancel.reaped().and_then(|ended| {
            Ok(json!({
                "said": cancel.said(ended.is_some())?,
                "ended": ended.is_some(),
                "exit": ended.and_then(|status| status.code()),
            }))
        });
        match said {
            Ok(said) => Response::json(&said),
            Err(error) => Response::refused("500 Internal Server Error", &error),
        }
    }

    fn started(&mut self, id: &RunTag, at: &next::At) -> Result<(), String> {
        let scratch = crate::controller::scratch()?;
        let path = scratch.path().join(SAID);
        let said = File::create(&path).context(path.display())?;
        let also = said.try_clone().context(path.display())?;
        let exe = workspace::exe("expected it to run `pumpkin cancel`")?;
        let mut command = stdx::process::command(exe, Environment::Inherited);
        command.arg("cancel");
        // Named always, as without `--runs` a machine whose runs go to a controller cancels the
        // run there, and without `--controller` one with several controllers refuses.
        if let Some(flag) = at.flag() {
            command.args(flag);
        }
        command.arg(id.as_str());
        let child = stdx::process::spawn(&mut command, Stdio::null(), said.into(), also.into())?;
        let cancel = Cancel {
            child,
            ended: None,
            said: Said::Saying(scratch),
        };
        // The last, if any, has ended, and was reaped.
        self.by_run.insert(id.as_str().to_owned(), cancel);
        Ok(())
    }
}

impl Cancel {
    fn reaped(&mut self) -> Result<Option<ExitStatus>, String> {
        if self.ended.is_none() {
            let waited = self.child.try_wait();
            self.ended = waited.context("waiting for `pumpkin cancel` failed")?;
        }
        Ok(self.ended)
    }

    fn said(&mut self, ended: bool) -> Result<String, String> {
        let scratch = match &self.said {
            Said::Said(said) => return Ok(said.clone()),
            Said::Saying(scratch) => scratch,
        };
        let path = scratch.path().join(SAID);
        let file = File::open(&path).context(path.display())?;
        let mut said = Vec::new();
        (file.take(SAID_MAX_BYTES))
            .read_to_end(&mut said)
            .context(path.display())?;
        let said = String::from_utf8_lossy(&said).into_owned();
        if ended
            && let Said::Saying(scratch) =
                std::mem::replace(&mut self.said, Said::Said(said.clone()))
        {
            let dir = scratch.path().display().to_string();
            scratch.close().context(dir)?;
        }
        Ok(said)
    }
}
