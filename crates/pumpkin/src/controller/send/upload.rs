//! Missing blobs are independent work. Each connection sends one at a time, and the job can
//! start only when the store has acknowledged every blob.

use std::cmp::Reverse;
use std::sync::Mutex;
use std::thread;

use stdx::{Context as _, unpoisoned};

use super::super::linking::{Build, Linked, Unlinked};
use super::super::{Located, Reached};
use super::{counting, holds};
use crate::link::store::{Lacks, Put};
use crate::manifest::store::{Missing, PutAnswer};
use crate::{manifest, say};

// An extra SSH handshake pays back only over a large upload, with work for each connection.
const MIN_BATCH_BYTES: u64 = 256 << 20;
const MIN_BLOB_BYTES: u64 = 32 << 20;
const MAX_LINKS: usize = 4;

fn remaining(blob: &Missing) -> u64 {
    blob.size_bytes - blob.held_bytes
}

fn connections(missing: &[Missing]) -> usize {
    let mut count = 0;
    let mut bytes = 0;
    for left in missing
        .iter()
        .map(remaining)
        .filter(|left| *left >= MIN_BLOB_BYTES)
    {
        count += 1;
        bytes += left;
    }
    if bytes >= MIN_BATCH_BYTES {
        count.min(MAX_LINKS)
    } else {
        1
    }
}

pub(super) fn send(
    linked: &mut Linked,
    reached: &Reached,
    hashed: &manifest::Hashed,
    mut peer: Option<(&Build, &Located)>,
    previous: &mut Option<u64>,
    advanced: &mut (dyn FnMut() + Send),
) -> Result<(), Unlinked> {
    let install = peer.is_none();
    let text = hashed.manifest.text();
    let request = || Lacks {
        manifest: manifest::sha256(&text),
        body_bytes: text.len(),
        install,
    };
    'lacks: loop {
        let mut missing = linked.ask_with_body(reached, request(), &text)??;
        let bytes = missing.iter().map(remaining).sum();
        if previous
            .replace(bytes)
            .is_some_and(|earlier| bytes < earlier)
        {
            advanced();
        }
        if !install || !missing.is_empty() {
            say(holds(
                &reached.name,
                &hashed.manifest,
                &missing,
                bytes,
                install,
            ));
        }
        let mut count = counting(&reached.name, bytes);
        let width = connections(&missing);
        if let Some((build, at)) = peer
            && width > 1
        {
            // Git LFS queues large blobs first so a late one won't hold the whole upload open.
            // https://github.com/git-lfs/git-lfs/blob/33752a72ba9a30fc591a8a58ed8fc55197a36928/tq/transfer_queue.go#L455-L457
            missing.sort_unstable_by_key(|blob| Reverse(remaining(blob)));
            let progress = Mutex::new((&mut count, &mut *advanced));
            let together = together(
                linked,
                &missing,
                width,
                || Linked::open(reached, build, at),
                |link, blob| {
                    put(
                        link,
                        reached,
                        hashed,
                        blob,
                        |n| unpoisoned(progress.lock()).0(n),
                        || unpoisoned(progress.lock()).1(),
                    )
                },
            )?;
            match together {
                Batch::Stored => return Ok(()),
                Batch::OffsetsChanged => {}
                Batch::Cut(said) => {
                    say(format_args!(
                        "{said}: continuing the upload on the open connection"
                    ));
                    peer = None;
                }
            }
            continue;
        }
        for blob in &missing {
            if !put(linked, reached, hashed, blob, &mut count, &mut *advanced)? {
                continue 'lacks;
            }
        }
        return Ok(());
    }
}

enum Batch {
    Stored,
    OffsetsChanged,
    Cut(String),
}

fn together(
    linked: &mut Linked,
    missing: &[Missing],
    width: usize,
    open: impl Fn() -> Result<Linked, Unlinked> + Sync,
    put: impl Fn(&mut Linked, &Missing) -> Result<bool, Unlinked> + Sync,
) -> Result<Batch, Unlinked> {
    let (first, queued) = missing.split_at(width);
    let mut seeds = first.iter();
    let original = seeds.next();
    let queued = Mutex::new(queued.iter());
    let stop = || *unpoisoned(queued.lock()) = [].iter();
    let terminal = |error: &Unlinked| {
        if matches!(error, Unlinked::HostKey(_) | Unlinked::Failed(_)) {
            stop();
        }
    };
    let send = |link: &mut Linked, first: Option<&Missing>| {
        let blobs = first
            .into_iter()
            .chain(std::iter::from_fn(|| unpoisoned(queued.lock()).next()));
        for blob in blobs {
            if !put(link, blob)? {
                return Ok(false);
            }
        }
        Ok(true)
    };
    thread::scope(|scope| {
        let mut workers = Vec::new();
        for first in seeds {
            let worker = thread::Builder::new()
                .spawn_scoped(scope, || {
                    open()
                        .and_then(|mut link| send(&mut link, Some(first)))
                        .inspect_err(terminal)
                })
                .inspect_err(|_| stop())
                .context("starting an upload thread")?;
            workers.push(worker);
        }
        let original = send(linked, original).inspect_err(terminal);
        let mut complete = true;
        let mut cut = None;
        for worker in workers {
            match worker
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            {
                Ok(stored) => complete &= stored,
                Err(Unlinked::Cut { said, .. }) => cut = Some(said),
                Err(error) => return Err(error),
            }
        }
        let stored = original?;
        Ok(match cut {
            Some(said) => Batch::Cut(said),
            None if stored && complete => Batch::Stored,
            None => Batch::OffsetsChanged,
        })
    })
}

fn put(
    linked: &mut Linked,
    reached: &Reached,
    hashed: &manifest::Hashed,
    blob: &Missing,
    mut count: impl FnMut(u64),
    mut advanced: impl FnMut(),
) -> Result<bool, Unlinked> {
    let Missing {
        digest,
        held_bytes,
        size_bytes,
    } = blob;
    let Some(source) = hashed.sources.get(digest) else {
        return Err(format!(
            "the controller asked for an unknown blob {}: send the task again",
            digest.hex()
        )
        .into());
    };
    let input = source.at(*held_bytes, *size_bytes)?;
    let put = Put {
        digest: digest.clone(),
        from: *held_bytes,
        size_bytes: *size_bytes,
    };
    match linked.stream(reached, |link| link.put(put, input, &mut count))?? {
        PutAnswer::Stored => {
            advanced();
            Ok(true)
        }
        PutAnswer::OffsetChanged => Ok(false),
        PutAnswer::NotItsDigest => Err(format!(
            "{} changed since it was hashed: send the task again",
            hashed.shown(digest)
        )
        .into()),
    }
}
