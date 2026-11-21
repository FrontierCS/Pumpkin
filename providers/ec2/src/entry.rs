//! Cloud-init starts the instance's host tools until the pinned `ec2-boot` is available.
//! It gives the boot its private input in memory. Only the boot's diagnostics reach the console.

use schema::ec2_boot::{BOOT_IN_IMAGE, CONSOLE_PREFIX, GRANT_DIR, Start};
use serde_json::{Value, json};

const REBOOT_UNIT: &str = "pumpkin-boots-once.service";
const START_FILE: &str = "/run/pumpkin-start.json";

pub(super) fn user_data(start: &Start, region: &str) -> String {
    // The hard end and the marker in /run are lost across a reboot. Enabling the unit without
    // starting it leaves this boot alone and powers off a later one, terminating the instance.
    let unit = format!(
        "[Unit]\n\
         Description=Shut down a Pumpkin sandbox instance that boots again\n\
         ConditionPathExists=!{GRANT_DIR}\n\n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart=/usr/bin/systemctl --no-block poweroff\n\n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    );
    let config = json!({
        "write_files": [
            { "path": format!("/etc/systemd/system/{REBOOT_UNIT}"), "content": unit },
            { "path": START_FILE, "owner": "root:root", "permissions": "0600",
              "content": json!(start).to_string() }
        ],
        "runcmd": commands(start, region)
    });
    format!("#cloud-config\n{config}\n")
}

fn commands(start: &Start, region: &str) -> Value {
    // Cloud-init quotes each word of a command list. All output is suppressed except the lines
    // sent explicitly to the console. The registry password travels only through the pipe.
    // https://docs.cloud-init.io/en/25.3/reference/yaml_examples/boot_cmds.html
    let finish = format!(
        "trap 'echo \"{CONSOLE_PREFIX} the entry ended with status $? in its step: $step\" \
         >/dev/console || true; rm --force {START_FILE} || true; \
         timeout --signal=KILL 60 docker logout \"$registry\" || true; \
         shutdown --poweroff now' EXIT"
    );
    // date truncates to the second and shutdown counts whole minutes. Round down so the
    // instance starts powering off by its hard end, even if fetching ec2-boot hangs.
    let arm = format!(
        "left=$(( {} - $(date +%s) - 1 )); shutdown --poweroff \"+$(( left > 0 ? left / 60 : 0 ))\"",
        start.hard_end_unix_seconds
    );
    let bounded = format!(
        "bounded() {{ step=$1; shift; left=$(( {} - $(date +%s) )); \
         test \"$left\" -gt 0 && timeout --signal=KILL \"$left\" \"$@\"; }}",
        start.pulls_by_unix_seconds
    );
    let pipe = r#"aws ecr get-login-password --region "$1" | docker login --username AWS --password-stdin "$2""#;
    let login = [
        "bounded",
        "logging in to the registry",
        "bash",
        "-o",
        "pipefail",
        "-c",
        pipe,
        "bash",
        region,
        &start.registry,
    ];
    let copy = "pumpkin-boot-image";
    let create = [
        "bounded",
        "pulling the boot image",
        "docker",
        "create",
        "--name",
        copy,
        "--entrypoint=/",
        &start.sandboxd_image,
    ];
    let binary = "/usr/local/sbin/ec2-boot";
    let from = format!("{copy}:{BOOT_IN_IMAGE}");
    let extract = ["bounded", "copying ec2-boot", "docker", "cp", &from, binary];
    json!([
        "set -o errexit -o nounset",
        "exec >/dev/null 2>&1",
        ["set", "--", start.registry],
        "registry=$1; step='arming the hard end'",
        finish,
        arm,
        format!("echo '{CONSOLE_PREFIX} the boot began' >/dev/console"),
        "step='shutting down a later boot'",
        ["install", "--directory", "--mode=0700", GRANT_DIR],
        ["systemctl", "enable", "--quiet", REBOOT_UNIT],
        ["test", "-r", START_FILE],
        bounded,
        ["bounded", "starting Docker", "systemctl", "start", "docker"],
        "# shellcheck disable=SC2016 # The inner bash expands its positional arguments.",
        login,
        create,
        extract,
        [
            "bounded",
            "removing the copy container",
            "docker",
            "rm",
            copy
        ],
        ["cd", "/"],
        "step='running ec2-boot'",
        "# shellcheck disable=SC2094 # The shell opens stdin before rm unlinks the file.",
        format!("(rm -- {START_FILE} && exec {binary} start) <{START_FILE} >/dev/console")
    ])
}
