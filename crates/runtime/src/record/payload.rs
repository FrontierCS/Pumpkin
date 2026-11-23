//! A payload that a record stores, read back by its digest.

use std::path::{Path, PathBuf};
use std::{fs, io};

use decision::PayloadRef;
use schema::ir::Sha256Digest;
use sha2::{Digest, Sha256};
use stdx::Context;

use super::{Record, sync_dir};
use crate::Host;
use crate::disk::Job;

impl Record {
    /// The bytes of the payload that `digest` names, as the record stores it in `payloads/`.
    ///
    /// # Errors
    ///
    /// Fails if the record stores no such payload, it can't be read, or its bytes are of another
    /// digest.
    pub fn payload(&self, digest: &Sha256Digest) -> Result<Vec<u8>, String> {
        let path = self.dir.join("payloads").join(digest.hex());
        let bytes = fs::read(&path).context(path.display())?;
        let read = reference(&bytes).digest;
        if read != *digest {
            return Err(format!(
                "{} holds bytes of digest {}: expected {}",
                path.display(),
                read.as_str(),
                digest.as_str()
            ));
        }
        Ok(bytes)
    }
}

pub(crate) fn reference(bytes: &[u8]) -> PayloadRef {
    let sha256: [u8; 32] = Sha256::digest(bytes).into();
    PayloadRef {
        size: bytes.len() as u64,
        digest: Sha256Digest::from(sha256),
    }
}

/// The record's `payloads/`, where each accepted payload is kept under the digest of its bytes.
pub(crate) struct Payloads {
    dir: PathBuf,
}

impl Payloads {
    pub(crate) fn create(host: &impl Host, record: &Path) -> Result<Self, String> {
        let dir = record.join("payloads");
        fs::create_dir(&dir).context(dir.display())?;
        sync_dir(host, record)?;
        Ok(Self { dir })
    }

    /// The job that keeps `bytes`, whose reference is `payload`, or none if a payload of that
    /// digest is kept already. A file has a digest's name only once it holds all of its bytes.
    pub(crate) fn store(&self, bytes: &[u8], payload: &PayloadRef) -> Result<Option<Job>, String> {
        let path = self.path(&payload.digest);
        match fs::symlink_metadata(&path) {
            Ok(_) => Ok(None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Some(Job::Replace {
                dir: self.dir.clone(),
                name: payload.digest.hex().to_owned(),
                bytes: bytes.to_vec(),
            })),
            Err(error) => Err(error).context(path.display()),
        }
    }

    pub(crate) fn stored(&self, payload: &PayloadRef) -> Stored {
        Stored {
            path: self.path(&payload.digest),
            reference: payload.clone(),
        }
    }

    fn path(&self, digest: &Sha256Digest) -> PathBuf {
        self.dir.join(digest.hex())
    }
}

pub(crate) struct Stored {
    path: PathBuf,
    pub(crate) reference: PayloadRef,
}

impl Stored {
    /// The payload's bytes. Fails if the file's size or digest isn't the reference's.
    pub(crate) fn read(&self) -> Result<Vec<u8>, String> {
        let bytes = fs::read(&self.path).context(self.path.display())?;
        let read = reference(&bytes);
        if read != self.reference {
            let (PayloadRef { size, digest }, expected) = (read, &self.reference);
            return Err(format!(
                "{} holds {size} bytes of digest {}: expected the payload's {} bytes of digest {}",
                self.path.display(),
                digest.as_str(),
                expected.size,
                expected.digest.as_str()
            ));
        }
        Ok(bytes)
    }
}
