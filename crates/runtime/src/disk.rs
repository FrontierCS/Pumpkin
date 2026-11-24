//! What makes the record last, on the driver's loop: each sync of the run log, and each file of
//! the record replaced whole, each told to its owner by its token once done. The jobs on one file
//! or directory go one after another, in the order asked for. Jobs on different ones don't wait
//! on each other, as a sandbox's record and the log's next line say nothing of each other.
//!
//! A write goes to the file as it is asked for, a page-cache write that the loop doesn't wait
//! for; a sync waits on the disk, and so is an operation of the loop's.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use stdx::Context as _;
use stdx::io::{Durable, Io, Loop};

/// What owns a [`Disk`], and hears of each job's end.
pub(crate) trait Owner: Loop {
    fn disk(&mut self) -> &mut Disk;

    /// Takes the end of the job that was given `token`: done, or why it failed.
    fn written(&mut self, io: &mut Io<Self>, token: u64, written: Result<(), String>);
}

#[derive(Default)]
pub(crate) struct Disk {
    // By the file that a sync is of, or the directory that a file is replaced in.
    lanes: BTreeMap<PathBuf, Lane>,
}

#[derive(Default)]
struct Lane {
    jobs: VecDeque<(Job, u64)>,
    // The job under way, with its steps left.
    current: Option<(VecDeque<Step>, u64)>,
}

pub(crate) enum Job {
    /// Syncs `file`, a handle of `path`'s, as far as `durable` says.
    Sync(File, PathBuf, Durable),
    /// Makes `dir/name` hold the bytes, as it is or as it was, whenever the machine fails: written
    /// and synced aside, then renamed over it, then the directory synced.
    Replace {
        dir: PathBuf,
        name: String,
        bytes: Vec<u8>,
    },
}

enum Step {
    Sync(File, PathBuf, Durable),
    // A sync in flight, on the loop's handle of the file at the path.
    Syncing(stdx::io::File, PathBuf),
    // Renames the first over the second, removing the first if that fails.
    Rename(PathBuf, PathBuf),
}

impl Disk {
    /// Queues `job`, whose end is told with `token`.
    pub(crate) fn push<C: Owner>(owner: &mut C, io: &mut Io<C>, job: Job, token: u64) {
        let lane = match &job {
            Job::Sync(_, path, _) => path.clone(),
            Job::Replace { dir, .. } => dir.clone(),
        };
        let lanes = &mut owner.disk().lanes;
        lanes
            .entry(lane.clone())
            .or_default()
            .jobs
            .push_back((job, token));
        Self::next(owner, io, &lane);
    }

    /// Whether a job is queued or under way.
    pub(crate) fn busy(&self) -> bool {
        !self.lanes.is_empty()
    }

    // Takes the next step of the lane's job under way, or starts its next job.
    fn next<C: Owner>(owner: &mut C, io: &mut Io<C>, lane: &Path) {
        loop {
            let lanes = &mut owner.disk().lanes;
            let Some(disk) = lanes.get_mut(lane) else {
                return;
            };
            if let Some((steps, _)) = &disk.current
                && let Some(Step::Syncing(..)) = steps.front()
            {
                return;
            }
            if disk.current.is_none() {
                let Some((job, token)) = disk.jobs.pop_front() else {
                    // A lane with nothing left goes, so the disk holds only what is under way.
                    lanes.remove(lane);
                    return;
                };
                match steps(job) {
                    Ok(steps) => disk.current = Some((steps, token)),
                    Err(error) => {
                        owner.written(io, token, Err(error));
                        continue;
                    }
                }
            }
            let Some(disk) = owner.disk().lanes.get_mut(lane) else {
                unreachable!("the lane of a job under way is held")
            };
            let Some((steps, token)) = &mut disk.current else {
                unreachable!("a job is under way, as one was just started")
            };
            let token = *token;
            match steps.pop_front() {
                None => {
                    disk.current = None;
                    owner.written(io, token, Ok(()));
                }
                Some(Step::Sync(file, path, durable)) => {
                    let handle = io.file(file);
                    io.fsync(&handle, durable, token, synced::<C>);
                    steps.push_front(Step::Syncing(handle, path));
                    return;
                }
                Some(Step::Syncing(..)) => {
                    unreachable!("a sync in flight was taken as the next step")
                }
                Some(Step::Rename(from, to)) => {
                    if let Err(error) = rename(Ok(()), &from, &to) {
                        disk.current = None;
                        owner.written(io, token, Err(error));
                    }
                }
            }
        }
    }
}

// The steps of `job`, after the write that it starts with.
fn steps(job: Job) -> Result<VecDeque<Step>, String> {
    match job {
        Job::Sync(file, path, durable) => Ok([Step::Sync(file, path, durable)].into()),
        Job::Replace { dir, name, bytes } => {
            let temporary = dir.join(format!(".{name}.partial"));
            let path = dir.join(&name);
            let written = File::create(&temporary).and_then(|mut file| {
                file.write_all(&bytes)?;
                Ok(file)
            });
            let file = written.context(temporary.display())?;
            let dir_handle = File::open(&dir).context(dir.display())?;
            Ok([
                Step::Sync(file, temporary.clone(), Durable::All),
                Step::Rename(temporary, path),
                Step::Sync(dir_handle, dir, Durable::All),
            ]
            .into())
        }
    }
}

/// Renames `from`, a file of the record written aside, over `to` once `written` says it was
/// written, or else removes it.
pub(crate) fn rename(written: Result<(), String>, from: &Path, to: &Path) -> Result<(), String> {
    let Err(error) = written.and_then(|()| fs::rename(from, to).context(to.display())) else {
        return Ok(());
    };
    // A temporary file that wasn't renamed holds nothing the record needs.
    match fs::remove_file(from) {
        Ok(()) => Err(error),
        Err(removing) if removing.kind() == io::ErrorKind::NotFound => Err(error),
        Err(removing) => Err(format!("{error}; {} is left: {removing}", from.display())),
    }
}

fn synced<C: Owner>(owner: &mut C, io: &mut Io<C>, token: u64, synced: io::Result<()>) {
    let lanes = &owner.disk().lanes;
    let lane = (lanes.iter())
        .find(|(_, lane)| {
            lane.current
                .as_ref()
                .is_some_and(|(_, current)| *current == token)
        })
        .map(|(path, _)| path.clone());
    let Some(lane) = lane else {
        unreachable!(
            "a sync completed with no job of its under way, though a job waits for its sync"
        )
    };
    let Some(disk) = owner.disk().lanes.get_mut(&lane) else {
        unreachable!("the lane of a sync under way is held")
    };
    let Some((steps, _)) = &mut disk.current else {
        unreachable!("the sync's job is under way")
    };
    let Some(Step::Syncing(handle, path)) = steps.pop_front() else {
        unreachable!("a sync's step stays first until the sync completes")
    };
    io.close_file(handle);
    if let Err(error) = synced {
        disk.current = None;
        owner.written(io, token, Err(format!("{}: {error}", path.display())));
    }
    Disk::next(owner, io, &lane);
}
