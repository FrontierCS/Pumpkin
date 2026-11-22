//! The EC2 provider's table of AWS's identity certificates, generated from AWS's page of them, so
//! that no certificate is copied by hand: `cargo xtask aws-certificates` fetches the page and
//! rewrites the table, and `cargo xtask ci` only tests the table that is committed.

use xshell::{Shell, cmd};

use crate::Result;

mod table;

/// Where the table is, from the repository's root.
const PATH: &str = "providers/ec2/src/identity/certificates.rs";

/// The longest that the fetch may take, which the page's size, about 150 KiB, leaves ample room
/// in.
const FETCH_SECONDS: u64 = 60;

/// The most that the fetch may download.
const FETCH_MAX_BYTES: u64 = 4 << 20;

/// Rewrites the table at [`PATH`] below `sh`'s directory from AWS's page.
pub(crate) fn write(sh: &Shell) -> Result<()> {
    let (source, regions_count) = table::generate(&fetch(sh)?)?;
    sh.write_file(PATH, source)?;
    println!("xtask: wrote the certificates of {regions_count} regions to {PATH}");
    Ok(())
}

fn fetch(sh: &Shell) -> Result<String> {
    let seconds = FETCH_SECONDS.to_string();
    let bytes = FETCH_MAX_BYTES.to_string();
    let url = table::URL;
    let curl = cmd!(
        sh,
        "curl --location --silent --show-error --fail --proto =https --tlsv1.2 --max-time {seconds} --max-filesize {bytes} {url}"
    );
    Ok(curl.read()?)
}
