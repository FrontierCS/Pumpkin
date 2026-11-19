//! Following a job across links. Only whole frames advance the log offset, and milestones that
//! the next link reads again from the run's record are shown once.

use std::collections::BTreeSet;

use super::Reached;
use super::linking::Build;
use crate::link::{Ask, Followed, Link, Unanswered};

pub(super) enum Finished {
    Ran { writer: Option<String> },
    Unstarted,
    Built(Vec<u8>),
    Unbuilt,
}

pub(super) fn job(reached: &Reached, build: &Build, id: &str) -> Result<Finished, String> {
    let mut shown = Shown::default();
    let what = format!("following job {id}");
    reached
        .carrying_on(&what, |progress| {
            let mut linked = reached.linked(build)?;
            let read = linked.stream(reached, |link| shown.read(link, id, progress));
            shown.end_line();
            read
        })?
        .map_err(|message| format!("controller `{}` refused: {message}", reached.name))
}

#[derive(Default)]
struct Shown {
    log_bytes: u64,
    asked: bool,
    mid_line: bool,
    said: BTreeSet<String>,
}

impl Shown {
    fn read(
        &mut self,
        link: &mut Link,
        id: &str,
        progress: &mut dyn FnMut(),
    ) -> Result<Result<Finished, String>, Unanswered> {
        link.send(
            &Ask::Follow {
                id: id.to_owned(),
                log_from_bytes: self.log_bytes,
                after_cut: self.asked,
            },
            &[],
        )?;
        self.asked = true;
        loop {
            let (item, body) = match link.item::<Followed>()? {
                Ok(frame) => frame,
                Err(message) => return Ok(Err(message)),
            };
            if matches!(item, Followed::Logged) {
                let Some(last) = body.last() else {
                    return Err(Unanswered::Failed("a logged frame has no bytes".to_owned()));
                };
                crate::to_stderr(&body);
                self.mid_line = *last != b'\n';
                self.log_bytes += body.len() as u64;
                progress();
                continue;
            }
            self.end_line();
            if !body.is_empty() {
                return Err(Unanswered::Failed(
                    "a follow frame other than logged has a body".to_owned(),
                ));
            }
            let end = match item {
                Followed::Logged => unreachable!("a logged frame continued just above"),
                Followed::Said { body_bytes } => {
                    let bytes = link.body(body_bytes)?;
                    let message = String::from_utf8(bytes).map_err(|error| {
                        Unanswered::Failed(format!("its milestone isn't UTF-8: {error}"))
                    })?;
                    if !self.said.contains(&message) {
                        crate::say(&message);
                        self.said.insert(message);
                        progress();
                    }
                    continue;
                }
                Followed::Ran { writer } => Finished::Ran { writer },
                Followed::Unstarted => Finished::Unstarted,
                Followed::Built { body_bytes } => Finished::Built(link.body(body_bytes)?),
                Followed::Unbuilt => Finished::Unbuilt,
            };
            return Ok(Ok(end));
        }
    }

    fn end_line(&mut self) {
        if self.mid_line {
            crate::to_stderr(b"\n");
            self.mid_line = false;
        }
    }
}
