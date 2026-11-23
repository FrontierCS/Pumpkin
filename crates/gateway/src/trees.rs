//! The session's transfers of trees, each on a stream of its own: a put sends a tree to the
//! sandbox, and a collect receives one from it.

use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use schema::frame;
use schema::ir::{AbsolutePath, Mode};
use schema::sandbox::tree::{self, Limits, Modes};
use schema::sandbox::{Entry, RelativePath, Root, ToSandbox};

use crate::{Bound, Part, Put, Reply, Route, Shared, Trees, broke, context, out_of_turn};

impl Trees {
    /// As [`crate::Session::put`], with `root` saying how `dir` is made. [`Root::New`] requires
    /// a new directory.
    pub fn put<'a>(
        &self,
        dir: &AbsolutePath,
        root: Root,
        entries: impl IntoIterator<Item = Put<'a>>,
        left: impl Fn() -> Option<Duration>,
    ) -> io::Result<()> {
        self.shared.put(dir, root, entries, Bound { left: &left })
    }

    /// Collects as [`crate::Session::collect`] does. Once `stopped` returns true before a part
    /// of the tree, returns [`io::ErrorKind::Interrupted`] and keeps the session open.
    /// Returns the tree's entries, each with its mode as the sandbox sent it, which the host's
    /// umask may have narrowed in `into`.
    pub fn collect(
        &self,
        path: &AbsolutePath,
        into: &Path,
        limits: Limits,
        left: impl Fn() -> Option<Duration>,
        stopped: &mut dyn FnMut() -> bool,
    ) -> io::Result<Vec<Entry>> {
        self.shared
            .collect(path, into, limits, Bound { left: &left }, stopped)
    }
}

impl Shared {
    pub(crate) fn put<'a>(
        &self,
        dir: &AbsolutePath,
        root: Root,
        entries: impl IntoIterator<Item = Put<'a>>,
        by: Bound<'_>,
    ) -> io::Result<()> {
        let (stream, replies) = self.open(Route::Reply, by)?;
        let put = self.send_tree(stream, dir, root, entries, &replies, by);
        self.close(stream);
        put
    }

    pub(crate) fn collect(
        &self,
        path: &AbsolutePath,
        into: &Path,
        limits: Limits,
        by: Bound<'_>,
        stopped: &mut dyn FnMut() -> bool,
    ) -> io::Result<Vec<Entry>> {
        let (stream, parts) = self.open(Route::Tree, by)?;
        let created = fs::create_dir(into).map_err(|error| context(into.display(), &error));
        let collected = created.and_then(|()| {
            let tree = tree::Writer::new(into.to_owned(), limits, Modes::Umasked);
            let path = path.clone();
            self.send(&ToSandbox::Collect { stream, path }, &[], by)?;
            self.receive_tree(stream, &parts, tree, by, stopped)
        });
        // What `sandboxd` still sends of a tree cut short arrives on a closed stream, which the
        // reader drops.
        self.close(stream);
        collected
    }

    fn send_tree<'a>(
        &self,
        stream: u32,
        dir: &AbsolutePath,
        root: Root,
        entries: impl IntoIterator<Item = Put<'a>>,
        replies: &Receiver<Result<(), String>>,
        by: Bound<'_>,
    ) -> io::Result<()> {
        let dir = dir.clone();
        self.send(&ToSandbox::Put { stream, dir, root }, &[], by)?;
        for entry in entries {
            // `sandboxd` replies before the end only to fail the tree, which then takes nothing
            // more. A `done` is out of turn: this entry and the tree's end are still to send.
            match replies.try_recv() {
                Ok(Ok(())) => {
                    let expected = "`failed`, or nothing before the tree's end";
                    self.end(out_of_turn(stream, &Reply::Done, expected));
                    return Err(self.ended());
                }
                Ok(Err(message)) => {
                    self.send(&ToSandbox::End { stream }, &[], by)?;
                    return Err(io::Error::other(message));
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
            }
            match entry {
                Put::Dir(path, mode) => {
                    let entry = Entry::Dir { path, mode };
                    self.send(&ToSandbox::Entry { stream, entry }, &[], by)?;
                }
                Put::File(path, mode, bytes) => {
                    let size = bytes.len() as u64;
                    let entry = Entry::File { path, size, mode };
                    self.send(&ToSandbox::Entry { stream, entry }, &[], by)?;
                    for chunk in bytes.chunks(frame::MAX_BODY_BYTES) {
                        self.send(&ToSandbox::Data { stream }, chunk, by)?;
                    }
                }
                Put::Read(path, mode, file) => {
                    // The tree ends with the file short, which `sandboxd` fails; the file's
                    // error is the one to report.
                    if let Err(error) = self.send_file(stream, path, mode, file, by)? {
                        self.send(&ToSandbox::End { stream }, &[], by)?;
                        let _ = self.receive(replies, by)?;
                        return Err(error);
                    }
                }
            }
        }
        self.send(&ToSandbox::End { stream }, &[], by)?;
        self.done(replies, by)
    }

    // The outer error is the session's; the inner one is the file's, read short or failing to be
    // read, after which the tree is to end at once.
    fn send_file(
        &self,
        stream: u32,
        path: RelativePath,
        mode: Mode,
        file: fs::File,
        by: Bound<'_>,
    ) -> io::Result<io::Result<()>> {
        let size = match file.metadata() {
            Ok(metadata) => metadata.len(),
            Err(error) => return Ok(Err(context(&path, &error))),
        };
        let shown = path.clone();
        let entry = Entry::File { path, size, mode };
        self.send(&ToSandbox::Entry { stream, entry }, &[], by)?;
        let (mut file, mut left) = (file.take(size), size);
        let mut buffer =
            vec![0; frame::MAX_BODY_BYTES.min(usize::try_from(size).unwrap_or(usize::MAX))];
        while left > 0 {
            let read = match file.read(&mut buffer) {
                Ok(0) => {
                    let error = io::Error::other(format!("{left} of its bytes are missing"));
                    return Ok(Err(context(&shown, &error)));
                }
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Ok(Err(context(&shown, &error))),
            };
            self.send(&ToSandbox::Data { stream }, &buffer[..read], by)?;
            left -= read as u64;
        }
        Ok(Ok(()))
    }

    fn receive_tree(
        &self,
        stream: u32,
        parts: &Receiver<Result<Part, String>>,
        mut tree: tree::Writer,
        by: Bound<'_>,
        stopped: &mut dyn FnMut() -> bool,
    ) -> io::Result<Vec<Entry>> {
        let mut entries = Vec::new();
        loop {
            if stopped() {
                let cut = "stopped before the tree was whole";
                return Err(io::Error::new(io::ErrorKind::Interrupted, cut));
            }
            match self.receive(parts, by)? {
                Ok(Part::Entry(entry)) => {
                    self.created(stream, tree.entry(&entry))?;
                    entries.push(entry);
                }
                Ok(Part::Data(bytes)) => self.written(stream, tree.data(&bytes))?,
                Ok(Part::End) => return self.written(stream, tree.finish()).map(|()| entries),
                Err(message) => return Err(io::Error::other(message)),
            }
        }
    }

    // A malformed tree breaks the protocol. The writer fails with `InvalidData` where the parts
    // don't fit together, as file data past its file's size, and no system error has that kind.
    // Any other error fails this collect alone: a limit passed, `FileTooLarge`, is the request's
    // own bound, not a break, and the host failing to write is no fault of the sandbox's.
    fn written(&self, stream: u32, written: io::Result<()>) -> io::Result<()> {
        match written {
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                Err(self.malformed(stream, &error))
            }
            written => written,
        }
    }

    // Creating an entry fails with `NotFound`, `NotADirectory` or `AlreadyExists` where its parent
    // is missing or a file, or its path is taken. In `into`, new and left to this collect, only the
    // tree's own earlier entries can cause that, given that `into` is on a filesystem that neither
    // folds case nor normalizes names: on one that does, a correct `sandboxd` that sends `A` and
    // `a` would meet `AlreadyExists`, and end the session. Writing a file's bytes goes to a file
    // already open, so a system error there is the host's.
    fn created(&self, stream: u32, created: io::Result<()>) -> io::Result<()> {
        match created {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound
                        | io::ErrorKind::NotADirectory
                        | io::ErrorKind::AlreadyExists
                ) =>
            {
                Err(self.malformed(stream, &error))
            }
            created => self.written(stream, created),
        }
    }

    fn malformed(&self, stream: u32, error: &io::Error) -> io::Error {
        let reason = broke(&format!(
            "the tree it sent on stream {stream} is malformed: {error}"
        ));
        self.end(reason);
        self.ended()
    }
}
