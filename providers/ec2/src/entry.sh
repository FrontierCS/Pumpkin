#!/bin/bash
# The entry of a Pumpkin sandbox's instance, its user data once the provider fills in its values:
# it runs `ec2-boot` with the start that follows, which `ec2-boot` documents. Only the lines that
# begin @PREFIX@ reach the serial console; nothing here prints the start, which holds the grant.
set -o errexit -o nounset -o pipefail
# What the commands print goes nowhere: cloud-init would copy it to the serial console.
exec >/dev/null 2>&1
step='arming the hard end'
# Whatever ends this, a step that fails or `ec2-boot`'s end, however it ends, logs out of the
# registry and powers the instance off, which the launch makes terminate it.
trap 'echo "@PREFIX@ the entry ended with status $? in its step: $step" >/dev/console || true
timeout --signal=KILL 60 docker logout @REGISTRY@ || true
shutdown --poweroff now' EXIT
# The hard end, armed first: `date` truncates to the second and `shutdown` counts whole minutes,
# so the time left loses a second and is rounded down to the minute, and the instance starts
# powering off by the hard end.
hard_end=@HARD_END@
left=$(( hard_end - $(date +%s) - 1 ))
shutdown --poweroff "+$(( left > 0 ? left / 60 : 0 ))"
echo "@PREFIX@ the boot began" >/dev/console
step='shutting down a later boot'
# The instance boots once. A later boot, as auto-recovery or a scheduled event gives, would have
# lost the hard end and start no container, so it shuts down, which terminates it. The unit runs
# in no boot that has @GRANT_DIR@, in memory, which this boot makes, and enabling it starts nothing.
install --directory --mode=0700 @GRANT_DIR@
cat >/etc/systemd/system/pumpkin-boots-once.service <<'UNIT'
[Unit]
Description=Shut down a Pumpkin sandbox instance that boots again, which terminates it
ConditionPathExists=!@GRANT_DIR@

[Service]
Type=oneshot
ExecStart=/usr/bin/systemctl --no-block poweroff

[Install]
WantedBy=multi-user.target
UNIT
systemctl enable --quiet pumpkin-boots-once.service
# Each step until `ec2-boot` runs is killed by the start's deadline, and none runs past it.
bounded() {
    local pulls_by=@PULLS_BY@
    local left=$(( pulls_by - $(date +%s) ))
    (( left > 0 )) && timeout --signal=KILL "$left" "$@"
}
step='starting Docker'
bounded systemctl start docker
step='logging in to the registry'
# The login, by the instance profile's credentials, which `ec2-boot` logs out of once it has
# pulled, and the end of this, if it hasn't.
bounded aws ecr get-login-password --region @REGION@ |
    bounded docker login --username AWS --password-stdin @REGISTRY@
step="copying ec2-boot out of sandboxd's image"
copied=$(bounded docker create --entrypoint=/ @SANDBOXD_IMAGE@)
bounded docker cp "$copied:@BOOT_IN_IMAGE@" /usr/local/sbin/ec2-boot
bounded docker rm "$copied"
cd /
step='running ec2-boot'
/usr/local/sbin/ec2-boot start >/dev/console <<'START'
@START@
START
