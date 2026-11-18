//! A job's log while it builds, then the run's milestones or the completed build's IR.

use std::fs::{self, File};
use std::io::{self, BufRead as _, BufReader, Seek as _, SeekFrom, Write};
use std::path::Path;

use provider::RunTag;
use runtime::{ReadError, Record, Writer};
use schema::frame;
use stdx::Context as _;

use super::exists;
use crate::ending::{FOLLOW_POLL, milestones};
use crate::job::{self, BUILDS, JOBS};
use crate::link::{self, Followed, Part};

pub(super) fn answer(
    output: &mut impl Write,
    home: &Path,
    id: String,
    log_from_bytes: u64,
) -> io::Result<()> {
    match follow(output, home, id, log_from_bytes) {
        Ok(()) => Ok(()),
        Err(message) => link::refuse(output, message),
    }
}

fn follow(
    output: &mut impl Write,
    home: &Path,
    id: String,
    mut log_from_bytes: u64,
) -> Result<(), String> {
    let id = RunTag::try_from(id)?;
    let id = id.as_str();
    let job = home.join(JOBS).join(id);
    let is_run = exists(&job)?;
    let dir = if is_run {
        job
    } else {
        home.join(BUILDS).join(id)
    };
    if !exists(&dir)? {
        return Err(format!("there is no job {id} here"));
    }
    let active = job::jobs()?.join(id);
    let log = dir.join("log");
    let out = dir.join("out");
    loop {
        // Check first, so the last read includes what the job wrote just before it ended.
        let ended = !exists(&active)?;
        let ready = ended || (is_run && size(&out)? > 0);
        if let Some(mut file) = open(&log)? {
            file.seek(SeekFrom::Start(log_from_bytes))
                .context(log.display())?;
            log_from_bytes += parts(output, file, &Followed::Logged, &log)?;
        }
        if ready {
            break;
        }
        std::thread::sleep(FOLLOW_POLL);
    }
    let written = "writing the job's ending failed";
    if !is_run {
        let size = size(&out)?;
        if size == 0 {
            return frame::write(output, &Followed::Unbuilt, &[]).context(written);
        }
        let file = File::open(&out).context(out.display())?;
        frame::write(output, &Followed::Built { body_bytes: size }, &[]).context(written)?;
        parts(output, file, &Part, &out)?;
        return Ok(());
    }
    let dir = home.join(job::runs()).join(id);
    if !exists(&dir)? {
        return frame::write(output, &Followed::Unstarted, &[]).context(written);
    }
    let writer = runtime::writer(&dir);
    let record = match &writer {
        Ok(Writer::This) => Some(Record::open(&dir)?),
        Ok(Writer::Older) => match Record::open(&dir) {
            Ok(record) => Some(record),
            Err(ReadError::Form(_)) => None,
            Err(error) => return Err(error.into()),
        },
        Ok(Writer::Other(_)) | Err(_) => None,
    };
    if let Some(record) = record {
        if record.ending()?.is_none() {
            let first = format!("following run {id}: ending this command does not stop it");
            say(output, &first)?;
            milestones(&record, |message| say(output, &message.to_string()))?;
        }
    } else {
        while exists(&active)? {
            std::thread::sleep(FOLLOW_POLL);
        }
    }
    // Its outcome may already be written while its driver still holds the last write lock.
    // Wait for that lock before telling the client to copy, even for a record we can't read.
    let path = dir.join("driver.pid");
    if let Some(driver) = open(&path)? {
        driver.lock_shared().context(path.display())?;
    }
    let writer = match writer {
        Ok(Writer::Other(build)) => Some(build),
        Ok(Writer::This | Writer::Older) | Err(_) => None,
    };
    frame::write(output, &Followed::Ran { writer }, &[]).context(written)
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

fn parts(
    output: &mut impl Write,
    file: File,
    head: &impl serde::Serialize,
    path: &Path,
) -> Result<u64, String> {
    let mut reader = BufReader::new(file);
    let mut total = 0;
    loop {
        let bytes = reader.fill_buf().context(path.display())?;
        if bytes.is_empty() {
            return Ok(total);
        }
        let count = bytes.len().min(frame::MAX_BODY_BYTES);
        frame::write(output, head, &bytes[..count])
            .context("writing a part of the job's output failed")?;
        total += count as u64;
        reader.consume(count);
    }
}
