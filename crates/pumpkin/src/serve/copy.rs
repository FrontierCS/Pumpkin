//! An ended record can be copied even when this build cannot read it. Its driver's lock is what
//! says that it ended. The copy's last file is the one by which a reader recognizes a record.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read as _, Write};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use provider::RunTag;
use schema::frame;
use schema::ir::Mode;
use stdx::Context as _;

use crate::link;
use crate::link::copy::{CopyRequest, Entries, Item, Stamp};

pub(super) fn answer(
    output: &mut impl Write,
    home: &Path,
    request: CopyRequest,
    body: &[u8],
) -> io::Result<()> {
    match send(output, home, request, body) {
        Ok(()) => Ok(()),
        Err(message) => link::refuse(output, message),
    }
}

fn send(
    output: &mut impl Write,
    home: &Path,
    request: CopyRequest,
    body: &[u8],
) -> Result<(), String> {
    let id = RunTag::try_from(request.id)?;
    let root = home.join(crate::job::runs()).join(id.as_str());
    let pid = root.join("driver.pid");
    let driver = File::open(&pid).context(pid.display())?;
    match driver.try_lock_shared() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err(format!(
                "run {} is still writing its record: wait until it ends and copy it again",
                id.as_str()
            ));
        }
        Err(TryLockError::Error(error)) => return Err(error).context(pid.display()),
    }
    let held = serde_json::Deserializer::from_slice(body)
        .into_iter::<(OsString, Stamp)>()
        .map(|entry| entry.map(|(path, stamp)| (PathBuf::from(path), stamp)))
        .collect::<Result<BTreeMap<_, _>, _>>()
        .context("reading the list of files the copy holds failed")?;
    let Entries { dirs, mut files } = Entries::at(&root)?;
    let record = PathBuf::from("record.json");
    let last = files.remove_entry(&record).ok_or_else(|| {
        format!(
            "{} has no record.json to copy: expected a run's record",
            root.display()
        )
    })?;
    for path in dirs {
        wrote(
            output,
            &Item::Directory {
                path: path.into_os_string(),
            },
            &[],
        )?;
    }
    let mut unread = None;
    let mut buffer = vec![0; frame::MAX_BODY_BYTES];
    for (name, stamp) in files.into_iter().chain([last]) {
        if held.get(&name) == Some(&stamp) || name == record && unread.is_some() {
            continue;
        }
        let path = root.join(&name);
        match open(&path, stamp) {
            Ok((file, mode)) => send_file(output, &root, name, stamp, mode, file, &mut buffer)?,
            Err(error) => {
                unread.get_or_insert(error);
            }
        }
    }
    let writer = match runtime::writer(&root) {
        Ok(runtime::Writer::Other(build)) => Some(build),
        Ok(runtime::Writer::This | runtime::Writer::Older) | Err(_) => None,
    };
    wrote(output, &Item::Copied { unread, writer }, &[])
}

fn open(path: &Path, stamp: Stamp) -> Result<(File, Mode), String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .context(path.display())?;
    let meta = file.metadata().context(path.display())?;
    if !meta.is_file() || Stamp::of(&meta) != stamp {
        return Err(format!("{} changed while it was copied", path.display()));
    }
    Ok((file, schema::sandbox::tree_mode(meta.mode())))
}

fn send_file(
    output: &mut impl Write,
    root: &Path,
    name: PathBuf,
    stamp: Stamp,
    mode: Mode,
    mut file: File,
    buffer: &mut [u8],
) -> Result<(), String> {
    let path = root.join(&name);
    wrote(
        output,
        &Item::File {
            path: name.into_os_string(),
            stamp,
            mode,
        },
        &[],
    )?;
    let mut left = stamp.size_bytes;
    while left > 0 {
        let count = usize::try_from(left)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        file.read_exact(&mut buffer[..count])
            .context(path.display())?;
        left -= count as u64;
        if left == 0 && Stamp::of(&file.metadata().context(path.display())?) != stamp {
            return Err(format!(
                "{} changed while it was copied: copy the record again",
                path.display()
            ));
        }
        wrote(output, &Item::Data, &buffer[..count])?;
    }
    Ok(())
}

fn wrote(output: &mut impl Write, head: &Item, body: &[u8]) -> Result<(), String> {
    frame::write(output, head, body).context("writing the record to the client failed")
}
