# /// script
# requires-python = ">=3.12"
# dependencies = ["modal==1.6.3"]
# ///
"""The Modal provider's helper, which Pumpkin's `modal` crate runs once for each call on Modal,
as its `helper` module says.

It reads one request on standard input and prints one answer on standard output, as the crate's
`helper::Request` says: `{"ok": …}`, what the call returns, or `{"error": "…"}`, what Modal, or
the way to it, reports. Anything else, such as a bug's traceback, it prints on standard error, and
exits with a status other than 0.

Every sandbox carries its run's tag and its role's (`RUN_TAG`, `ROLE_TAG`): a run's list finds its
sandboxes by its tag, and a call on a sandbox that a request names refuses one of another run's.
"""

import hashlib
import json
import math
import sys
import time

# The App that holds the images Pumpkin imports, which Modal builds once for each definition, and
# every run's sandboxes. Nothing stops it: an App that holds no sandbox costs nothing.
APP = "pumpkin-images"

# The tags of a sandbox: its run's tag, by which its run's list finds it, and its role's name.
RUN_TAG = "pumpkin-run"
ROLE_TAG = "pumpkin-role"

# Where a sandbox has `sandboxd`, and `sandboxctl` for a role that makes ops
# (`schema::sandbox::SANDBOXD_PATH`, `SANDBOXCTL_PATH`), which Modal attaches to it from this
# host as it starts it.
SANDBOXD_PATH = "/.pumpkin/sandboxd"
SANDBOXCTL_PATH = "/.pumpkin/bin/sandboxctl"

# The port `sandboxd` listens at in its sandbox, which Modal's tunnel exposes behind its TLS.
PORT = 7434

# How often a create lists its run until the list finds its sandbox: as often as Modal's SDK asks
# about a sandbox it has just created.
LISTED_EVERY = 0.5

# How many of a sandbox's last log entries its console holds.
CONSOLE_ENTRIES = 20

# What begins the line of a sandbox's console that this helper adds.
CONSOLE_PREFIX = "pumpkin-modal:"


class Refused(Exception):
    """What a call can't do, as the answer's error: what is wrong, then what is expected."""


def main() -> None:
    request = json.load(sys.stdin)
    # Imported only now, so that a request that isn't JSON fails before Modal's SDK is loaded.
    import modal

    calls = {
        "ready": ready,
        "import": imported,
        "create": create,
        "list": listed,
        "delete": delete,
        "console": console,
    }
    try:
        answer = {"ok": calls[request["call"]](modal, request)}
    except modal.exception.Error as error:
        answer = {"error": f"{type(error).__name__}: {error}"}
    except Refused as error:
        answer = {"error": str(error)}
    json.dump(answer, sys.stdout)


def ready(modal, request: dict) -> dict:
    workspace = modal.Workspace.from_context()
    workspace.hydrate()
    return {}


def imported(modal, request: dict) -> str:
    secret = modal.Secret.from_dict(
        {"AWS_ROLE_ARN": request["role_arn"], "AWS_REGION": request["region"]}
    )
    # Modal runs an image's ENTRYPOINT with a sandbox's command as its arguments, and no role's
    # sandbox runs its image's own (`schema::ir`), so the import leaves the image none.
    image = modal.Image.from_registry(
        request["reference"], secret=secret, setup_dockerfile_commands=["ENTRYPOINT []"]
    )
    image.build(modal.App.lookup(APP, create_if_missing=True))
    return image.object_id


def create(modal, request: dict) -> dict:
    """Creates the run's sandbox of the request's role, and returns its ID and its tunnel's host
    and port.

    Modal ends it at the run's `hard_end`, in seconds since the Unix epoch, if nothing ends it
    first. Its `sandboxd` ends it too if no gateway has claimed it a minute after its start's
    deadline, `by`, as when Modal makes it after the create was given up."""
    # Modal counts the timeout from its create, which this is just before.
    timeout = request["hard_end"] - int(time.time())
    if timeout <= 0:
        raise Refused("the create's hard end has passed: expected one to come")
    sandboxd = request["sandboxd"]
    with open(sandboxd["path"], "rb") as file:
        digest = hashlib.sha256(file.read()).hexdigest()
    if digest != sandboxd["sha256"]:
        raise Refused(
            f"the sandboxd at {sandboxd['path']} has changed since it was published: expected "
            f"the file whose SHA-256 is {sandboxd['sha256']}"
        )
    run = request["run"]
    app = modal.App.lookup(APP, create_if_missing=True)
    image = modal.Image.from_id(request["image"])
    image = image.add_local_file(sandboxd["path"], SANDBOXD_PATH, copy=False)
    args = [SANDBOXD_PATH, f"0.0.0.0:{PORT}", "--listen", "--by", str(request["by"])]
    args += ["--user", request["user"]]
    if request["ops"]:
        image = image.add_local_file(sandboxd["path"], SANDBOXCTL_PATH, copy=False)
        args.append("--ops")
    if request["model"]:
        args.append("--model")
    cpu, memory = cores(request["cpu_millis"]), request["memory_mib"]
    sandbox = modal.Sandbox.create(
        *args,
        app=app,
        tags={RUN_TAG: run, ROLE_TAG: request["role"]},
        image=image,
        env={"SANDBOXD_GRANT": request["grant"]},
        timeout=timeout,
        gpu=request["gpu"],
        cpu=(cpu, cpu),
        memory=(memory, memory),
        runtime="gvisor",
        outbound_cidr_allowlist=[],
        inbound_cidr_allowlist=[request["source"]],
        encrypted_ports=[PORT],
    )
    tunnel = sandbox.tunnels()[PORT]
    while sandbox.object_id not in ids(modal, app, run) and sandbox.poll() is None:
        time.sleep(LISTED_EVERY)
    return {"id": sandbox.object_id, "host": tunnel.host, "port": tunnel.port}


def cores(millis: int) -> float:
    """`millis` thousandths of a CPU as the cores the SDK takes, which it turns back into the same
    thousandths (`int(1000 * cpu)`), as `millis / 1000` alone doesn't for some, such as 1001."""
    cpu = millis / 1000
    return cpu if int(1000 * cpu) == millis else math.nextafter(cpu, math.inf)


def ids(modal, app, run: str) -> list[str]:
    return [box.object_id for box in modal.Sandbox.list(app_id=app.app_id, tags={RUN_TAG: run})]


def listed(modal, request: dict) -> list[dict]:
    """The run's sandboxes that Modal lists, which are those that haven't finished: it frees a
    finished one itself."""
    run = request["run"]
    try:
        app = modal.App.lookup(APP)
    except modal.exception.NotFoundError:
        # A create or an import makes Pumpkin's App, so before one there are no sandboxes.
        return []
    sandboxes = modal.Sandbox.list(app_id=app.app_id, tags={RUN_TAG: run})
    return [{"id": box.object_id, "ended": box.returncode is not None} for box in sandboxes]


def owned(modal, run: str, id: str):
    """Sandbox `id`, if Modal holds it, once its tag says it is `run`'s."""
    try:
        sandbox = modal.Sandbox.from_id(id)
        tags = sandbox.get_tags()
    except modal.exception.NotFoundError:
        return None
    if tags.get(RUN_TAG) != run:
        raise Refused(f"sandbox {id} is not run {run}'s: expected one of its own")
    return sandbox


def delete(modal, request: dict) -> dict:
    failed = {}
    for id in request["ids"]:
        try:
            sandbox = owned(modal, request["run"], id)
            if sandbox is not None:
                sandbox.terminate(wait=True)
        except modal.exception.SandboxTimeoutError:
            # The wait's report of a sandbox that Modal ended at its timeout, which has finished
            # as any other.
            pass
        except modal.exception.Error as error:
            failed[id] = f"{type(error).__name__}: {error}"
        except Refused as error:
            failed[id] = str(error)
    return {"failed": failed}


def console(modal, request: dict) -> str:
    """The last `CONSOLE_ENTRIES` entries of the sandbox's log, then, if Modal reports it ended, a
    line `pumpkin-modal: sandbox <id> exited with status <N>`."""
    sandbox = owned(modal, request["run"], request["id"])
    if sandbox is None:
        raise Refused(f"Modal holds no sandbox {request['id']}: expected one of the run's")
    text = "".join(entry.message for entry in sandbox.logs.tail(CONSOLE_ENTRIES))
    # A sandbox that the kernel's OOM killer ends prints nothing of why: only its status, 137,
    # says how it ended.
    status = sandbox.poll()
    if status is None:
        return text
    if text and not text.endswith("\n"):
        text += "\n"
    return f"{text}{CONSOLE_PREFIX} sandbox {sandbox.object_id} exited with status {status}\n"


if __name__ == "__main__":
    main()
