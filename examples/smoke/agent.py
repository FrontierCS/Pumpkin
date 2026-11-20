"""The smoke's agent: runs the probe here and in a runner of each session it holds, then submits
every line the probes printed, as its final submission; or, given `hold`, holds a lease of each
session until the run ends."""

import json
import subprocess
import sys
import time

CTL = "/.pumpkin/bin/sandboxctl"
PROBE = ["python3", "-u", "/opt/smoke/probe.py"]
# How long a lease's runner may take to be ready, and its probe to run: the provider's startup
# bounds the first, and the probe's own timeouts, a few seconds each, the second.
READY_SECONDS = 1800
RUN_SECONDS = 120


def ctl(*args):
    done = subprocess.run([CTL, *args], check=False, capture_output=True, text=True)
    try:
        return json.loads(done.stdout)
    except ValueError:
        return {
            "kind": "error",
            "exit": done.returncode,
            "stdout": done.stdout,
            "stderr": done.stderr,
        }


def waited(grant, lease, states, seconds):
    """The lease's status once its state is one of `states`, or the last seen after `seconds`."""
    by = time.monotonic() + seconds
    while True:
        status = ctl("call", grant + ".status", str(lease))
        if (
            status.get("state") in states
            or status.get("kind") != "lease_status"
            or time.monotonic() > by
        ):
            return status
        time.sleep(1)


def ran_out(grant, lease, seconds):
    """The lease's status once its last command has ended, or its lease has, or after `seconds`."""
    by = time.monotonic() + seconds
    while True:
        status = ctl("call", grant + ".status", str(lease))
        ended = (status.get("last") or {}).get("end") is not None
        gone = status.get("state") in {"closed", "expired", "failed"}
        if (
            ended
            or gone
            or status.get("kind") != "lease_status"
            or time.monotonic() > by
        ):
            return status
        time.sleep(1)


def probed(grant):
    """The lines that the probe printed in a runner of session `grant`, or a line that says why there
    are none."""
    opened = ctl("call", grant + ".open")
    if opened.get("kind") != "lease":
        return [f"{grant}_runner lease: FAILED (open: {json.dumps(opened)})"]
    lease = opened["lease"]
    ready = waited(
        grant, lease, {"ready", "closed", "expired", "failed"}, READY_SECONDS
    )
    if ready.get("state") != "ready":
        return [f"{grant}_runner lease: FAILED (not ready: {json.dumps(ready)})"]
    ran = ctl(
        "call",
        grant + ".run",
        str(lease),
        "--timeout",
        str(RUN_SECONDS),
        "--",
        *PROBE,
        grant + "_runner",
    )
    if ran.get("kind") != "lease":
        return [f"{grant}_runner lease: FAILED (run: {json.dumps(ran)})"]
    ended = ran_out(grant, lease, RUN_SECONDS + 60)
    ctl("call", grant + ".close", str(lease))
    last = ended.get("last") or {}
    lines = (last.get("stdout") or "").splitlines()
    return lines or [f"{grant}_runner lease: FAILED (no output: {json.dumps(ended)})"]


def held(grants):
    # Each opened first, so that their runners start at once, and one slow to start takes none of
    # the others' time.
    opened = [(grant, ctl("call", grant + ".open")) for grant in grants]
    for grant, lease in opened:
        print(
            "{}: {}".format(
                grant,
                json.dumps(
                    waited(
                        grant, lease.get("lease"), {"ready", "failed"}, READY_SECONDS
                    )
                ),
            ),
            flush=True,
        )
    while True:
        time.sleep(60)


def main():
    grants = [
        grant["name"]
        for grant in ctl("ops").get("grants", [])
        if grant["kind"] == "session"
    ]
    if sys.argv[1:] == ["hold"]:
        held(grants)
    lines = subprocess.run(
        [*PROBE, "agent"], check=False, capture_output=True, text=True
    ).stdout.splitlines()
    for grant in grants:
        lines += probed(grant)
    for each in lines:
        print(each, flush=True)
    with open("probes", "w") as file:
        file.write("\n".join(lines) + "\n")
    print(
        json.dumps(ctl("call", "final", "probes", "--arg", "finalize=true")), flush=True
    )


if __name__ == "__main__":
    main()
