//! The image of a static `sandboxd`, with the static programs a provider runs beside it, for a
//! provider whose sandboxes pull what they run from a registry, which [`Engine::load_sandboxd`]
//! makes byte for byte, as an OCI image layout, and loads into the engine, from which
//! [`Pusher::push`](crate::Pusher::push) pushes it.

use std::ffi::OsStr;
use std::fs;
use std::path::Path;

use schema::ir::{Image, Sha256Digest};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use stdx::Context as _;

use crate::build::in_scratch;
use crate::{Engine, Platform, docker, fix_reported};

/// The media types of the image's manifest, its configuration, its one layer, an uncompressed
/// archive, and the index of the layout that holds it.
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const CONFIG_TYPE: &str = "application/vnd.oci.image.config.v1+json";
const LAYER_TYPE: &str = "application/vnd.oci.image.layer.v1.tar";
const INDEX_TYPE: &str = "application/vnd.oci.image.index.v1+json";

/// The size of a block of a ustar archive, in bytes, of which a header is one.
const BLOCK: usize = 512;

/// The most bytes a file of a ustar archive holds: what its header's 11 octal digits count.
const MOST_BYTES: u64 = 0o77_777_777_777;

impl Engine {
    /// Loads into the engine, and returns the ID of, the image for [`Platform::sandboxd`] that
    /// holds copies of the static programs `binaries` alone, each by its path in the image and
    /// the file it copies, owned by root, mode 0755: one layer, no time or command, so the same
    /// files give the same image wherever it is loaded.
    ///
    /// # Errors
    ///
    /// Fails if a file can't be read or holds 8 GiB or more, or if the archive or `docker load`
    /// fails.
    pub fn load_sandboxd(&self, binaries: &[(&str, &Path)]) -> Result<Image, String> {
        let mut read = Vec::new();
        for &(in_image, path) in binaries {
            read.push((in_image, fs::read(path).context(path.display())?));
        }
        let (archive, image) = archive(&read, &Platform::sandboxd())?;
        let context = &self.report.context;
        in_scratch(&self.dir, "load", |scratch| {
            let file = scratch.join("image.tar");
            fs::write(&file, &archive).context(file.display())?;
            let load = ["load", "--quiet", "--input"].map(OsStr::new);
            let load = load.into_iter().chain([file.as_os_str()]);
            docker(context, load).map_err(|error| fix_reported(&error))?;
            Ok(image)
        })
    }
}

/// The archive of the OCI image layout whose one image, for `platform`, holds each of
/// `binaries`, at its absolute path, and that image's manifest's digest.
fn archive(binaries: &[(&str, Vec<u8>)], platform: &Platform) -> Result<(Vec<u8>, Image), String> {
    let Platform { os, architecture } = platform;
    let files: Vec<(&str, u32, &[u8])> = (binaries.iter())
        .map(|(path, bytes)| (path.trim_start_matches('/'), 0o755, bytes.as_slice()))
        .collect();
    let layer = ustar(&files)?;
    let config = json!({
        "architecture": architecture,
        "os": os,
        "config": {},
        "rootfs": {"type": "layers", "diff_ids": [digest(&layer).as_str()]},
    });
    let config = config.to_string().into_bytes();
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": MANIFEST_TYPE,
        "config": descriptor(CONFIG_TYPE, &config),
        "layers": [descriptor(LAYER_TYPE, &layer)],
    });
    let manifest = manifest.to_string().into_bytes();
    let index = json!({
        "schemaVersion": 2,
        "mediaType": INDEX_TYPE,
        "manifests": [descriptor(MANIFEST_TYPE, &manifest)],
    });
    let index = index.to_string().into_bytes();
    let layout = json!({"imageLayoutVersion": "1.0.0"})
        .to_string()
        .into_bytes();
    let blobs = [&config, &layer, &manifest].map(|blob| {
        let digest = digest(blob);
        let hex = digest.hex().to_owned();
        (format!("blobs/sha256/{hex}"), blob)
    });
    let mut files = vec![
        ("oci-layout", 0o644, layout.as_slice()),
        ("index.json", 0o644, index.as_slice()),
    ];
    files.extend((blobs.iter()).map(|(name, blob)| (name.as_str(), 0o644, blob.as_slice())));
    let archive = ustar(&files)?;
    // A manifest's digest is what an image's ID is in the containerd image store, which
    // `Engine::connect` requires.
    Ok((archive, Image::try_from(digest(&manifest).to_string())?))
}

/// The OCI descriptor of `blob`, of media type `media_type`.
fn descriptor(media_type: &str, blob: &[u8]) -> Value {
    json!({"mediaType": media_type, "digest": digest(blob).as_str(), "size": blob.len()})
}

/// The SHA-256 digest of `bytes`.
fn digest(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest::from(<[u8; 32]>::from(Sha256::digest(bytes)))
}

/// A ustar archive of `files`, each a regular file by its name, of at most 100 bytes, its mode
/// and its bytes, owned by root, with no time.
fn ustar(files: &[(&str, u32, &[u8])]) -> Result<Vec<u8>, String> {
    let mut archive = Vec::new();
    for &(name, mode, bytes) in files {
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if size > MOST_BYTES {
            return Err(format!(
                "{name} holds {size} bytes: expected at most {MOST_BYTES}, as an image's layer here \
                 holds"
            ));
        }
        let mut header = [0_u8; BLOCK];
        header[..name.len()].copy_from_slice(name.as_bytes());
        octal(&mut header[100..108], mode.into());
        // The owner's and the group's IDs, 0, root's, and the time, 0, the epoch's.
        for field in [108..116, 116..124, 136..148] {
            octal(&mut header[field], 0);
        }
        octal(&mut header[124..136], size);
        header[156] = b'0';
        header[257..265].copy_from_slice(b"ustar\x0000");
        // The checksum counts its own field as spaces, and is 6 octal digits, a NUL and a space.
        header[148..156].fill(b' ');
        let sum = header.iter().map(|&byte| u64::from(byte)).sum();
        octal(&mut header[148..155], sum);
        archive.extend_from_slice(&header);
        archive.extend_from_slice(bytes);
        archive.resize(archive.len().next_multiple_of(BLOCK), 0);
    }
    // Two blocks of zeros end the archive.
    archive.resize(archive.len() + 2 * BLOCK, 0);
    Ok(archive)
}

/// Writes `value` in `field`, a header's, as octal digits, as many as fill it but its last byte,
/// which is a NUL. The caller keeps `value` to what they count.
fn octal(field: &mut [u8], value: u64) {
    let digits = format!("{value:0width$o}", width = field.len() - 1);
    field[..digits.len()].copy_from_slice(digits.as_bytes());
    field[digits.len()] = 0;
}
