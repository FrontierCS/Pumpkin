use std::fmt::{self, Write as _};
use std::fs::File;
use std::io::{BufRead, BufReader, Read as _, Seek, SeekFrom};
use std::ops::Range;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use stdx::Context;

use super::logged::{self, Line};
use super::progress::{Reading, Readout, Roles, Unread, json_file, readout};

/// Takes each line of the run log of the record in `dir` whose frame reads, as a [`Line`] of
/// the parts of it that read, in turn, by `take`, and returns the parts that don't; `None` if
/// there is no log. A last line without its newline is left out ([`take_lines`]).
///
/// # Errors
///
/// Fails if the log can't be read.
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

/// The run log at `path`, open to read; `None` if there is none.
fn open(path: &Path) -> Result<Option<File>, String> {
    match File::open(path) {
        Ok(log) => Ok(Some(log)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context(path.display()),
    }
}

/// How far a read of a run log went: up to the end of the last whole line it took.
#[derive(Clone, Copy, Default)]
struct Cursor {
    /// The bytes before that end.
    size: u64,
    /// The lines before it, so the number of the last.
    lines: u64,
}

/// Takes each whole line of `log`, the run log at `path`, past `cursor`, in turn, by `take`, with
/// the byte it starts at and its bytes, and, if its frame reads, as a [`Line`] of the parts of it
/// that read; adds the parts that don't to `unread`, and moves `cursor` past each line before it
/// takes it, so that a failed read leaves it past the lines taken before. A last line without its
/// newline, one being written or that a crash cut short, is left to a later read.
fn take_lines(
    log: File,
    path: &Path,
    cursor: &mut Cursor,
    unread: &mut Vec<Unread>,
    mut take: impl FnMut(TakenLine<'_>, Option<Line>),
) -> Result<(), String> {
    let mut log = BufReader::new(log);
    log.seek(SeekFrom::Start(cursor.size))
        .context(path.display())?;
    let mut line = Vec::new();
    loop {
        line.clear();
        log.read_until(b'\n', &mut line).context(path.display())?;
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

/// The final judge's job: the one that the run log of the record in `dir` names in its
/// `start_judge` without a grant.
///
/// # Errors
///
/// Fails if the log can't be read, or doesn't name such a job.
pub(crate) fn final_judge(dir: &Path) -> Result<u64, String> {
    let mut job = None;
    let unread = records(dir, |line| {
        let started = line.actions.iter().find_map(logged::Action::score_job);
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
                "{} names no final judge: expected a `start_judge` without a grant",
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

/// The lines of the run log of the record in `dir`, as
/// [`Record::log_lines`](crate::Record::log_lines) gives them.
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

/// Reads what the record in `dir` says of its run so far ([`Readout`]).
///
/// # Errors
///
/// Fails if a file of the record can't be read, or, once the log exists, its `ir.json` or
/// `provider.json` isn't JSON.
pub(crate) fn progress(dir: &Path) -> Result<Readout, String> {
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
/// Each [`take`](Self::take) reads only what the log gained, and a [`readout`](Self::readout)
/// says what all it took says, as a read of those bytes whole would.
///
/// Modelled on sapling's indexedlog `FoldState`, which resumes a fold over an append-only log
/// from the offset it reached, and starts over when `self.offset > log.disk_buf.len() as u64 ||
/// self.epoch != log.meta.epoch`, a log's epoch here being its file's device and inode:
/// <https://github.com/meta/sapling/blob/8a502b24d6777264898ad36acad97209e9ab1978/eden/scm/lib/indexedlog/src/log/fold.rs#L158-L216>.
pub struct LogFold {
    dir: PathBuf,
    /// What `ir.json` and `provider.json` say, read as the log is first found: a run writes
    /// each of them whole, once, before it creates its log.
    roles: Option<Roles>,
    reading: Reading,
    unread: Vec<Unread>,
    /// The device and inode of the log file taken, once it is found.
    file: Option<(u64, u64)>,
    taken: Cursor,
}

/// A whole line of a run's log that a [`LogFold`] took: the byte it starts at, and its bytes,
/// without the newline that ends it.
#[derive(Clone, Copy, Debug)]
pub struct TakenLine<'l> {
    pub byte: u64,
    pub bytes: &'l [u8],
}

/// Why a [`LogFold`] started over: its log is no longer the one it took some of.
///
/// It found no log at the log's path, a log shorter than it had taken, or another file, by its
/// device and inode, none of which a run's log, that only grows, ever is.
#[derive(Debug)]
pub struct StartedOver {
    /// The bytes it had taken.
    taken: u64,
    found: Found,
}

#[derive(Debug)]
enum Found {
    Gone,
    /// A log of this many bytes.
    Shorter(u64),
    Other,
}

/// `its log is 120 bytes, shorter than the 4096 bytes read of it before: reading it again from
/// its start`.
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
    /// The log of the record in `dir`, none of it taken.
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

    /// Takes each whole line that the log gained since the last take; a last line without its
    /// newline waits for a later one. A failed take keeps the lines it took, and the next goes
    /// on after them. Once some of the log is taken, if it is gone, shorter, or another file,
    /// the fold starts over, takes the log from its start, and says so.
    ///
    /// # Errors
    ///
    /// Fails if a file of the record can't be read, or, once the log exists, its `ir.json` or
    /// `provider.json` isn't JSON.
    pub fn take(&mut self) -> Result<Option<StartedOver>, String> {
        self.take_each(|_| {})
    }

    /// Takes what the log gained as [`take`](Self::take) does, and hands `each` every whole line
    /// it takes, in the log's order, whether it reads or not; after a [`StartedOver`], those of
    /// the log from its start.
    ///
    /// # Errors
    ///
    /// As [`take`](Self::take).
    pub fn take_each(
        &mut self,
        mut each: impl FnMut(TakenLine<'_>),
    ) -> Result<Option<StartedOver>, String> {
        let path = self.dir.join("log.jsonl");
        let log = open(&path)?;
        let found = match &log {
            Some(log) => {
                let meta = log.metadata().context(path.display())?;
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
        let roles = if let Some(roles) = self.roles.take() {
            roles
        } else {
            let ir = json_file(&self.dir.join("ir.json"))?;
            let provided = json_file(&self.dir.join("provider.json"))?;
            Roles::of(&ir, &provided)
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

    /// The byte past the last whole line taken, which the next take reads on from: 0 before any.
    pub fn taken(&self) -> u64 {
        self.taken.size
    }

    /// What the lines taken say of the run ([`Readout`]), with what the record says now of
    /// each sandbox that they name. More can be taken after.
    ///
    /// # Errors
    ///
    /// Fails if a sandbox's `sandbox.json` in the record can't be read, or isn't JSON.
    pub fn readout(&self) -> Result<Readout, String> {
        readout(
            &self.dir,
            self.roles.as_ref(),
            self.reading.clone(),
            self.unread.clone(),
        )
    }
}
