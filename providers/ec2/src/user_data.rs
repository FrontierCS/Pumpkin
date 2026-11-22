//! The user data of a sandbox's instance: the [`Start`], which `ec2-boot` reads, and a
//! cloud-config with no shell program, which holds no secret. Its files are two units and root's
//! Docker configuration; its commands start the boot's unit, which copies `ec2-boot` out of the
//! image of `sandboxd` and runs it.
//!
//! cloud-init writes the files in its init stage, then runs the commands in its config stage,
//! once per instance, as AL2023's `/etc/cloud/cloud.cfg` (L34-L81 in
//! `cloud-init-22.2.2-1.amzn2023.1.15`) orders its modules, as cloud-init's own template does:
//! <https://github.com/canonical/cloud-init/blob/905d10891051496bd196798b2a8e0d77f161db74/config/cloud.cfg.tmpl#L82-L153>.

use std::time::Duration;

use schema::ec2_boot::{self, ATTESTATION_DIR, BOOT_IN_IMAGE, CONSOLE_PREFIX, Start};
use serde_json::{Value, json};

const BOOT: &str = include_str!("pumpkin-ec2-boot.service");

const BOOT_UNIT: &str = "pumpkin-ec2-boot.service";

const BOOTS_ONCE: &str = include_str!("pumpkin-boots-once.service");

const BOOTS_ONCE_UNIT: &str = "pumpkin-boots-once.service";

// How long the boot's commands get to end once their unit is stopped, as the poweroff of a
// deadline stops it, before they are killed: as long as `docker stop` gives a container, as each is
// a client of Docker's, or `ec2-boot`, which ends on the signal.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

// The constants that the units hold as they are: a systemd command line reads none of their
// bytes as syntax.
const _: () = assert!(
    is_literal(BOOT_IN_IMAGE)
        && is_literal(CONSOLE_PREFIX)
        && is_literal(ATTESTATION_DIR)
        && is_literal(BOOTS_ONCE_UNIT)
);

/// The user data that boots `start`'s sandbox, whose images are in `registry`.
///
/// # Errors
///
/// The image of `sandboxd` is empty, or holds a byte that a systemd command line reads as syntax,
/// or a part of the user data holds its boundary.
pub(crate) fn user_data(start: &Start, registry: &str) -> Result<String, String> {
    let sandboxd = exec_word(&start.sandboxd_image).map_err(|why| {
        format!(
            "the image of sandboxd `{}` {why}: expected a reference of letters, digits and \
             `-_./:@+=,%` alone",
            start.sandboxd_image.escape_debug(),
        )
    })?;
    // Whole microseconds, as logind takes the time, of whole seconds, so the deadline is the
    // start's to the second.
    let pulls_by_micros = u128::from(start.pulls_by_unix_seconds) * 1_000_000;
    let filled = [
        ("@PREFIX@", CONSOLE_PREFIX),
        ("@PULLS_BY_MICROS@", &pulls_by_micros.to_string()),
        ("@STOP_SECONDS@", &STOP_TIMEOUT.as_secs().to_string()),
        ("@BOOTS_ONCE@", BOOTS_ONCE_UNIT),
        ("@SANDBOXD_IMAGE@", &sandboxd),
        ("@BOOT_IN_IMAGE@", BOOT_IN_IMAGE),
    ];
    let boot = (filled.iter()).fold(BOOT.to_owned(), |unit, (placeholder, value)| {
        unit.replace(placeholder, value)
    });
    let boots_once = BOOTS_ONCE.replace("@ATTESTATION_DIR@", ATTESTATION_DIR);
    // Docker asks ECR's credential helper for the registry's token on each pull, by the
    // instance's role. The AMI only installs the helper, so the boot names it for the registry.
    let docker = json!({ "credHelpers": { registry: "ecr-login" } }).to_string();
    let config = json!({
        "write_files": [
            // Kept for a later boot, which it ends.
            file(&format!("/etc/systemd/system/{BOOTS_ONCE_UNIT}"), &boots_once, None),
            // In memory, so a later boot has none.
            file(&format!("/run/systemd/system/{BOOT_UNIT}"), &boot, None),
            file("/root/.docker/config.json", &docker, Some("0600")),
        ],
        // cloud-init quotes each word of a list, so no shell reads one:
        // https://github.com/canonical/cloud-init/blob/905d10891051496bd196798b2a8e0d77f161db74/cloudinit/util.py#L2180-L2203
        "runcmd": [
            ["systemctl", "daemon-reload"],
            ["systemctl", "start", "--no-block", BOOT_UNIT],
        ],
    });
    // JSON, which YAML reads as it is.
    let config = format!("#cloud-config\n{config:#}\n");
    ec2_boot::user_data(&config, start)
}

fn file(path: &str, content: &str, permissions: Option<&str>) -> Value {
    let mut file = json!({ "path": path, "content": content });
    if let Some(permissions) = permissions {
        file["permissions"] = permissions.into();
    }
    file
}

// `word` as a word of a systemd command line, each `%`, which starts a specifier, doubled; refused
// if it is empty or holds a byte that the line reads as syntax, as quotes, `$`, `\` and
// whitespace are:
// https://github.com/systemd/systemd-stable/blob/0f3ad617e60c1d037025ff29a45a6422630ad9e8/man/systemd.service.xml#L1157-L1236
// https://github.com/systemd/systemd-stable/blob/0f3ad617e60c1d037025ff29a45a6422630ad9e8/man/standard-specifiers.xml#L85-L89
fn exec_word(word: &str) -> Result<String, String> {
    if let Some(byte) = (word.bytes()).find(|&byte| byte != b'%' && !is_literal_byte(byte)) {
        return Err(format!(
            "holds `{}`, which a systemd command line reads as syntax",
            byte.escape_ascii()
        ));
    }
    if word.is_empty() {
        return Err("is empty, which a systemd command line drops".to_owned());
    }
    Ok(word.replace('%', "%%"))
}

const fn is_literal(word: &str) -> bool {
    let bytes = word.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if !is_literal_byte(bytes[index]) {
            return false;
        }
        index += 1;
    }
    !bytes.is_empty()
}

const fn is_literal_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'_' | b'.' | b'/' | b':' | b'@' | b'+' | b'=' | b','
        )
}
