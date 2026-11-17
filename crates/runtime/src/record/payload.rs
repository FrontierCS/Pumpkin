//! A payload that a record stores, read back by its digest.

use std::fs;

use decision::PayloadRef;
use schema::ir::Sha256Digest;
use sha2::{Digest, Sha256};
use stdx::Context;

use super::Record;

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
