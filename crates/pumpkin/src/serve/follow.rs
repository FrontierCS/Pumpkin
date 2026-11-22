//! A job's log while it builds, then the run's milestones.

use std::fs::{self, File, TryLockError};
use std::io::{self, BufRead as _, BufReader, Seek as _, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

use provider::RunTag;
use runtime::{ReadError, Record, Writer};
use rustix::event::PollFlags;
use schema::frame;
use stdx::Context as _;

use super::exists;
use crate::ending::{Polls, milestones};
use crate::job::{self, JOBS};
use crate::link::{self, Followed};

pub(super) fn answer(
    output: &mut impl Write,
    home: &Path,
    id: String,
    log_from_bytes: u64,
    after_cut: bool,
) -> io::Result<()> {
    match follow(output, home, id, log_from_bytes, after_cut) {
        Ok(()) => Ok(()),
        Err(message) => link::refuse(output, message),
    }
}

fn follow(
    output: &mut impl Write,
    home: &Path,
    id: String,
    mut log_from_bytes: u64,
    after_cut: bool,
) -> Result<(), String> {
    let id = RunTag::try_from(id)?;
    let id = id.as_str();
    let dir = home.join(JOBS).join(id);
    if !exists(&dir)? {
        return Err(format!("there is no job {id} here"));
    }
    let active = job::jobs()?.join(id);
    let log = dir.join("log");
    let out = dir.join("out");
    let mut polls = Polls::new();
    loop {
        // Check first, so the last read includes what the job wrote just before it ended.
        let ended = !exists(&active)?;
        let ready = ended || size(&out)? > 0;
        if let Some(mut file) = open(&log)? {
            file.seek(SeekFrom::Start(log_from_bytes))
                .context(log.display())?;
            let read_bytes = logged(output, file, &log)?;
            if read_bytes > 0 {
                polls.restart();
            }
            log_from_bytes += read_bytes;
        }
        if ready {
            break;
        }
        wait(polls.pause())?;
    }
    ran(
        output,
        &home.join(job::runs()).join(id),
        id,
        &active,
        after_cut,
    )
}

fn ran(
    output: &mut impl Write,
    dir: &Path,
    id: &str,
    active: &Path,
    after_cut: bool,
) -> Result<(), String> {
    let written = "writing the run's ending failed";
    if !exists(dir)? {
        return frame::write(output, &Followed::Unstarted, &[]).context(written);
    }
    let writer = runtime::writer(dir);
    let record = match &writer {
        Ok(Writer::This) => Some(Record::open(dir)?),
        Ok(Writer::Older) => match Record::open(dir) {
            Ok(record) => Some(record),
            Err(ReadError::Form(_)) => None,
            Err(error) => return Err(error.into()),
        },
        Ok(Writer::Other(_)) | Err(_) => None,
    };
    if let Some(record) = record {
        let live = record.ending()?.is_none();
        if live {
            let first = format!("following run {id}: ending this command does not stop it");
            say(output, &first)?;
        }
        // A cut may have cost the client a milestone just before the run ended.
        if live || after_cut {
            milestones(&record, |message| say(output, &message.to_string()), wait)?;
        }
    } else {
        let mut polls = Polls::new();
        while exists(active)? {
            wait(polls.pause())?;
        }
    }
    // Its outcome may already be written while its driver still holds the last write lock.
    // Wait for that lock before telling the client to copy, even for a record we can't read.
    let path = dir.join("driver.pid");
    if let Some(driver) = open(&path)? {
        let mut polls = Polls::new();
        loop {
            match driver.try_lock_shared() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) => wait(polls.pause())?,
                Err(TryLockError::Error(error)) => return Err(error).context(path.display()),
            }
        }
    }
    let writer = match writer {
        Ok(Writer::Other(build)) => Some(build),
        Ok(Writer::This | Writer::Older) | Err(_) => None,
    };
    frame::write(output, &Followed::Ran { writer }, &[]).context(written)
}

// A quiet job can outlive its follower. Watch the reader of stdout as journalctl --follow does.
// <https://github.com/systemd/systemd/blob/ca78951bd398e191ebc4550710e33ef34190514e/src/journal/journalctl-show.c#L468-L489>
fn wait(pause: Duration) -> Result<(), String> {
    let closed = super::ready(&io::stdout(), PollFlags::ERR | PollFlags::HUP, pause)
        .context("polling the standard output failed")?;
    if closed {
        return Err("the client closed its output".to_owned());
    }
    Ok(())
}

fn say(output: &mut impl Write, message: &str) -> Result<(), String> {
    let head = Followed::Said {
        body_bytes: message.len() as u64,
    };
    frame::write(output, &head, &[])
        .and_then(|()| link::write_parts(output, message.as_bytes()))
        .context("writing a milestone failed")
}

fn open(path: &Path) -> Result<Option<File>, String> {
    match File::open(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        result => result.map(Some).context(path.display()),
    }
}

fn size(path: &Path) -> Result<u64, String> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error).context(path.display()),
    }
}

fn logged(output: &mut impl Write, file: File, path: &Path) -> Result<u64, String> {
    let mut reader = BufReader::new(file);
    let mut total = 0;
    loop {
        let bytes = reader.fill_buf().context(path.display())?;
        if bytes.is_empty() {
            return Ok(total);
        }
        let count = bytes.len().min(frame::MAX_BODY_BYTES);
        frame::write(output, &Followed::Logged, &bytes[..count])
            .context("writing a part of the job's output failed")?;
        total += count as u64;
        reader.consume(count);
    }
}
