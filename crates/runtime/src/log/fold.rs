//! Readers of the run log that work on a file the run is still appending to, so a last line without
//! its newline is a line not yet written, and is left until it is whole.
//!
//! A [`LogFold`] follows one log: each take reads the lines the log gained since the last, so a
//! poll costs what the log gained, and the fold's [`Readout`] covers every line taken. The other
//! readers read the log of a record's directory whole, once.

use std::fmt::{self, Write as _};
use std::fs::File;
use std::io::{BufRead, BufReader, Read as _, Seek, SeekFrom};
use std::ops::Range;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use stdx::Context;

use super::logged::{self, Line};
use super::progress::{Reading, Readout, Unread, json_file, readout};
use super::roles::Roles;
use crate::read_error::{ReadError, failed};

fn records(dir: &Path, mut take: impl FnMut(Line)) -> Result<Option<Vec<Unread>>, String> {
    let path = dir.join("log.jsonl");
    let Some(log) = open(&path)? else {
        return Ok(None);
    };
    let mut unread = Vec::new();
    take_lines(
        log,
        &path,
        &mut Cursor::default(),
        &mut unread,
        |_, record| {
            if let Some(record) = record {
                take(record);
            }
        },
    )?;
    Ok(Some(unread))
}

/// The number of the final call's job, from the log in `dir`. Fails if the log is missing, or
/// holds no start of the final call, naming the parts it couldn't read.
pub(crate) fn final_call(dir: &Path) -> Result<u64, String> {
    let mut job = None;
    let unread = records(dir, |line| {
        let started = line.actions.iter().find_map(logged::Action::final_call);
        job = job.or(started);
    })?;
    let path = dir.join("log.jsonl");
    match (unread, job) {
        (_, Some(job)) => Ok(job),
        (None, None) => Err(format!(
            "{} doesn't exist: expected a run's log",
            path.display()
        )),
        (Some(unread), None) => {
            let mut error = format!(
                "{} names no final call: expected a `start_final_call`",
                path.display()
            );
            if let Some(first) = unread.first() {
                let _ = write!(
                    error,
                    ", among the parts that this pumpkin can read; {} of its parts can't be read, \
                     the first at {first}",
                    unread.len(),
                );
            }
            Err(error)
        }
    }
}

fn open(path: &Path) -> Result<Option<File>, ReadError> {
    match File::open(path) {
        Ok(log) => Ok(Some(log)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(failed(path.display())(error)),
    }
}

#[derive(Clone, Copy, Default)]
struct Cursor {
    size: u64,
    lines: u64,
}

fn take_lines(
    log: File,
    path: &Path,
    cursor: &mut Cursor,
    unread: &mut Vec<Unread>,
    mut take: impl FnMut(TakenLine<'_>, Option<Line>),
) -> Result<(), ReadError> {
    let mut log = BufReader::new(log);
    log.seek(SeekFrom::Start(cursor.size))
        .map_err(failed(path.display()))?;
    let mut line = Vec::new();
    loop {
        line.clear();
        log.read_until(b'\n', &mut line)
            .map_err(failed(path.display()))?;
        // A last line without its newline is one being written, or that a crash cut short: it is
        // left to a later read.
        let Some(whole) = line.strip_suffix(b"\n") else {
            return Ok(());
        };
        let byte = cursor.size;
        cursor.size += line.len() as u64;
        cursor.lines += 1;
        let record = Line::read(whole, cursor.lines, unread);
        take(TakenLine { byte, bytes: whole }, record);
    }
}

/// The lines of the log in `dir` within `bytes`, each without its newline and with the byte it
/// starts at. The range must begin and end at lines' ends, as `TakenLine::byte` and the length of
/// what a fold took give them; a last line without a newline is returned as it is.
pub(crate) fn lines(dir: &Path, bytes: Range<u64>) -> Result<Vec<(u64, Vec<u8>)>, String> {
    let path = dir.join("log.jsonl");
    let size = usize::try_from(bytes.end.saturating_sub(bytes.start)).map_err(|_| {
        format!(
            "{}: {bytes:?} is past what this machine can hold",
            path.display()
        )
    })?;
    let mut read = vec![0; size];
    let mut log = File::open(&path).context(path.display())?;
    log.seek(SeekFrom::Start(bytes.start))
        .and_then(|_| log.read_exact(&mut read))
        .context(path.display())?;
    let mut byte = bytes.start;
    let lines = read.split_inclusive(|&byte| byte == b'\n').map(|line| {
        let start = byte;
        byte += line.len() as u64;
        (start, line.strip_suffix(b"\n").unwrap_or(line).to_vec())
    });
    Ok(lines.collect())
}

/// What the whole log in `dir` says now. Fails if a file of the record can't be read, or, once the
/// log exists, its `ir.json` or `provider.json` isn't JSON.
pub(crate) fn progress(dir: &Path) -> Result<Readout, ReadError> {
    let mut fold = LogFold::new(dir);
    let None = fold.take()? else {
        unreachable!("a fold that has taken nothing never starts over");
    };
    let LogFold {
        dir,
        roles,
        reading,
        unread,
        file: _,
        taken: _,
    } = fold;
    readout(&dir, roles.as_ref(), reading, unread)
}

/// A run's log, as far as it was read, kept to be read on as the run appends to it.
///
/// Each [`take`](Self::take) reads only what the log gained, and a [`readout`](Self::readout) says
/// what all it took says, as a read of those bytes whole would.
///
/// Modelled on sapling's indexedlog `FoldState`, which resumes a fold over an append-only log from
/// the offset it reached, and starts over when `self.offset > log.disk_buf.len() as u64 ||
/// self.epoch != log.meta.epoch`, a log's epoch here being its file's device and inode:
/// <https://github.com/meta/sapling/blob/8a502b24d6777264898ad36acad97209e9ab1978/eden/scm/lib/indexedlog/src/log/fold.rs#L158-L216>.
pub struct LogFold {
    dir: PathBuf,
    roles: Option<Roles>,
    reading: Reading,
    unread: Vec<Unread>,
    file: Option<(u64, u64)>,
    taken: Cursor,
}

/// A whole line of the log, as a fold took it.
#[derive(Clone, Copy, Debug)]
pub struct TakenLine<'l> {
    /// The byte of the log where the line starts.
    pub byte: u64,
    /// The line, without its newline, though it may not be JSON or not a line's shape.
    pub bytes: &'l [u8],
}

/// That a fold found the log no longer the one it had read, and read the log it found from its
/// start.
///
/// It found no log at the log's path, a log shorter than it had taken, or another file, by
/// its device and inode, none of which a run's log, which only grows, ever is. Its `Display` says
/// which, for the person following the run.
#[derive(Debug)]
pub struct StartedOver {
    taken: u64,
    found: Found,
}

#[derive(Debug)]
enum Found {
    Gone,
    Shorter(u64),
    Other,
}

impl fmt::Display for StartedOver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { taken, found } = self;
        match found {
            Found::Gone => write!(
                f,
                "its log is gone, after {taken} bytes were read of it: reading the next log \
                 there from its start"
            ),
            Found::Shorter(size) => write!(
                f,
                "its log is {size} bytes, shorter than the {taken} bytes read of it before: \
                 reading it again from its start"
            ),
            Found::Other => write!(
                f,
                "its log is another file than the one {taken} bytes were read of before: \
                 reading it from its start"
            ),
        }
    }
}

impl LogFold {
    pub(crate) fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_owned(),
            roles: None,
            reading: Reading::default(),
            unread: Vec::new(),
            file: None,
            taken: Cursor::default(),
        }
    }

    /// Reads the lines that the log gained since the last take, and adds what they say to the
    /// readout. A log that isn't there yet is no error, and adds nothing. A failed take keeps the
    /// lines it took, and the next goes on after them.
    ///
    /// If the log is gone, shorter than what was read, or another file, the fold forgets everything
    /// it held and reads the log from its start, and says so. Fails if a file of the record can't
    /// be read, or, once the log exists, its `ir.json` or `provider.json` isn't JSON.
    pub fn take(&mut self) -> Result<Option<StartedOver>, ReadError> {
        self.take_each(|_| {})
    }

    /// [`take`](LogFold::take), calling `each` with every line it takes, whether or not the line
    /// could be read. After a start over, the lines are the new log's from its first, and what the
    /// caller derived of the earlier ones is its to discard.
    pub fn take_each(
        &mut self,
        mut each: impl FnMut(TakenLine<'_>),
    ) -> Result<Option<StartedOver>, ReadError> {
        let path = self.dir.join("log.jsonl");
        let log = open(&path)?;
        let found = match &log {
            Some(log) => {
                let meta = log.metadata().map_err(failed(path.display()))?;
                Some(((meta.dev(), meta.ino()), meta.len()))
            }
            None => None,
        };
        let taken = self.taken.size;
        let started_over = if taken == 0 {
            None
        } else {
            match found {
                None => Some(Found::Gone),
                Some((file, _)) if self.file != Some(file) => Some(Found::Other),
                Some((_, size)) => (size < taken).then_some(Found::Shorter(size)),
            }
        }
        .map(|found| StartedOver { taken, found });
        if started_over.is_some() {
            *self = Self::new(&self.dir);
        }
        let (Some(log), Some((file, _))) = (log, found) else {
            return Ok(started_over);
        };
        self.file = Some(file);
        // Each of `ir.json` and `provider.json` is written whole, once, before the run creates its
        // log, so they are read as the log is first found.
        let roles = if let Some(roles) = self.roles.take() {
            roles
        } else {
            let ir = super::roles::ir_in(&self.dir)?;
            let provided = json_file(&self.dir.join("provider.json"))?;
            Roles::of(ir.as_ref(), &provided)
        };
        let roles = self.roles.insert(roles);
        let reading = &mut self.reading;
        take_lines(
            log,
            &path,
            &mut self.taken,
            &mut self.unread,
            |taken, line| {
                if let Some(line) = line {
                    reading.take(&line, roles);
                }
                each(taken);
            },
        )?;
        Ok(started_over)
    }

    /// How many bytes of the log the fold has taken: the byte where the next line it takes starts.
    pub fn taken(&self) -> u64 {
        self.taken.size
    }

    /// What the lines taken say, as reading the whole log would, with what the record says now of
    /// each sandbox that they name. Fails if a sandbox's `sandbox.json` can't be read, or isn't
    /// JSON.
    pub fn readout(&self) -> Result<Readout, ReadError> {
        readout(
            &self.dir,
            self.roles.as_ref(),
            self.reading.clone(),
            self.unread.clone(),
        )
    }
}
