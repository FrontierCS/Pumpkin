//! The EC2 provider's table of AWS's identity certificates, generated from AWS's page of them, so
//! that no certificate is copied by hand: `cargo xtask aws-certificates` fetches the page and
//! rewrites the table, and `cargo xtask ci` only tests the table that is committed.

use std::fs;

use stdx::Context as _;
use stdx::process::{self, Environment};

use crate::repo::STEP_TIMEOUT;
use crate::{Result, read, root};

mod table;

/// Where the table is, from the repository's root.
const PATH: &str = "providers/ec2/src/identity/certificates.rs";

/// The longest that the fetch may take, which the page's size, about 150 KiB, leaves ample room
/// in.
const FETCH_SECONDS: u64 = 60;

const FETCH_MAX_BYTES: u64 = 4 << 20;

/// Rewrites the table at [`PATH`] from AWS's page.
pub(crate) fn write() -> Result<()> {
    let (source, regions_count) = table::generate(&fetch()?)?;
    fs::write(root().join(PATH), source).context(format_args!("writing {PATH} failed"))?;
    println!("xtask: wrote the certificates of {regions_count} regions to {PATH}");
    Ok(())
}

fn fetch() -> Result<String> {
    let mut curl = process::command("curl", Environment::Inherited);
    curl.args(["--location", "--silent", "--show-error", "--fail"])
        .args(["--proto", "=https", "--tlsv1.2"])
        .args(["--max-time", &FETCH_SECONDS.to_string()])
        .args(["--max-filesize", &FETCH_MAX_BYTES.to_string()])
        .arg(table::URL);
    read(&mut curl, STEP_TIMEOUT)
}
