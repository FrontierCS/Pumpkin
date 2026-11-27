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

import contextlib
import hashlib
import json
import math
import sys
import time
from concurrent.futures import ThreadPoolExecutor

# The App that holds the images Pumpkin imports, which Modal builds once for each definition, and
# every run's sandboxes. Nothing stops it: an App that holds no sandbox costs nothing.
APP = "pumpkin-images"

# The tags of a sandbox: its run's tag, by which its run's list finds it, and its role's name.
RUN_TAG = "pumpkin-run"
ROLE_TAG = "pumpkin-role"

# How many images an import builds at once, and how many sidecars a create makes at once. Each
# waits on Modal on a thread of its own. The builds share the SDK's one HTTP/2 connection, a stream
# each. HTTP/2 asks a server to allow at least 100 streams at once, so 64 leaves room for the calls
# beside them: https://www.rfc-editor.org/rfc/rfc9113.html#section-6.5.2
# Past what Modal allows, its SDK waits rather than fails.
AT_ONCE = 64

# How often a create lists its run until the list finds its sandbox: as often as Modal's SDK asks
# about a sandbox it has just created.
LISTED_EVERY = 0.5

# How many of a sandbox's last log entries its console holds.
CONSOLE_ENTRIES = 20

# What begins the line of a sandbox's console that this helper adds.
CONSOLE_PREFIX = "pumpkin-modal:"

# Any size up to 16 MiB would do. The SDK buffers an exec's standard input up to 16 MiB, and
# refuses a write past that until a drain sends what it holds:
# https://github.com/modal-labs/modal-client/blob/e8a24c8e304766d1b3b6905f297e3f43d08e1d93/py/modal/io_streams.py#L633-L636
# https://github.com/modal-labs/modal-client/blob/e8a24c8e304766d1b3b6905f297e3f43d08e1d93/py/modal/io_streams.py#L746-L754
STDIN_CHUNK = 1 << 20


class Refused(Exception):
    """What a call can't do, as the answer's error: what is wrong, then what is expected."""


def main() -> None:
    request = json.load(sys.stdin)
    # Imported only now, so that a request that isn't JSON fails before Modal's SDK is loaded.
    import modal

    calls = {
        "ready": ready,
        "import": imported,
        "fill": fill,
        "held": held,
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
    return {"workspace": workspace.name}


def imported(modal, request: dict) -> list[str]:
    """Imports the image at each of the request's `references`, at most `AT_ONCE` at a time, and
    returns their IDs in that order. With the request's `sandboxd`, each holds that too, built in,
    as a sidecar's image must."""
    sandboxd = request["sandboxd"]
    if sandboxd is not None:
        unchanged(sandboxd)
    app = modal.App.lookup(APP, create_if_missing=True)

    def build(reference: str) -> str:
        secret = modal.Secret.from_dict(
            {"AWS_ROLE_ARN": request["role_arn"], "AWS_REGION": request["region"]}
        )
        # Modal runs an image's ENTRYPOINT with a container's command as its arguments, and no
        # role's sandbox runs its image's own (`schema::ir`), so the import leaves the image none.
        image = modal.Image.from_registry(
            reference, secret=secret, setup_dockerfile_commands=["ENTRYPOINT []"]
        )
        if sandboxd is not None:
            for at in sandboxd["at"]:
                image = image.add_local_file(sandboxd["path"], at, copy=True)
        try:
            image.build(app)
        except modal.exception.Error as error:
            raise Refused(f"image {reference}: {type(error).__name__}: {error}") from error
        return image.object_id

    with ThreadPoolExecutor(AT_ONCE) as pool:
        # The first failure in the request's order is raised here, once every build has ended.
        return list(pool.map(build, request["references"]))


def unchanged(sandboxd: dict) -> None:
    """Refuses a `sandboxd` whose file is no longer the one published."""
    with open(sandboxd["path"], "rb") as file:
        digest = hashlib.sha256(file.read()).hexdigest()
    if digest != sandboxd["sha256"]:
        raise Refused(
            f"the sandboxd at {sandboxd['path']} has changed since it was published: expected "
            f"the file whose SHA-256 is {sandboxd['sha256']}"
        )


def fill(modal, request: dict) -> dict:
    """Returns the ID of an image of the request's `files`, or fails with what its `fetch` printed.
    It ends its sandbox before it returns."""
    app = modal.App.lookup(APP, create_if_missing=True)
    cpu = cores(request["cpu_millis"])
    sandbox = modal.Sandbox.create(
        *request["args"],
        app=app,
        tags={RUN_TAG: request["run"]},
        image=modal.Image.debian_slim(python_version=request["python"]),
        cpu=cpu,
        memory=request["memory_mib"],
        timeout=request["timeout_seconds"],
    )
    try:
        sandbox.mount_image(request["at"], modal.Image.from_scratch())
        fetch = sandbox.exec(*request["fetch"])
        files = json.dumps(request["files"]).encode()
        for start in range(0, len(files), STDIN_CHUNK):
            fetch.stdin.write(files[start : start + STDIN_CHUNK])
            fetch.stdin.drain()
        fetch.stdin.write_eof()
        fetch.stdin.drain()
        status = fetch.wait()
        if status != 0:
            said = fetch.stderr.read().strip()
            raise Refused(
                f"the fill's sandbox {sandbox.object_id} fetched the asset's files with status "
                f"{status}: {said}"
            )
        image = sandbox.snapshot_directory(
            request["at"], timeout=request["snapshot_seconds"], ttl=request["ttl_seconds"]
        )
    finally:
        try:
            end(modal, sandbox)
        # Modal ends it at its timeout all the same, and the fill's own failure is the one to say.
        except modal.exception.Error:
            pass
    return {"id": image.object_id}


def held(modal, request: dict) -> bool:
    """Whether Modal holds the request's `image`: false once it is deleted."""
    app = modal.App.lookup(APP, create_if_missing=True)
    try:
        modal.Image.from_id(request["image"]).build(app)
    except modal.exception.NotFoundError:
        return False
    return True


def create(modal, request: dict) -> dict:
    """Creates the run's sandbox of the request's role, then its `sidecars`. Returns the sandbox's
    ID, a tunnel for each of the request's `ports` in their order, and the `phases` that it timed.
    The phases are `lookup`, `create`, `tunnels` and `list`, then `assets` if it mounted any, then
    `companions` if it made any.

    Each container starts with the `args` and `env` that the request gives it, which this passes
    on as they are. `main` has the request's `sandboxd` attached at each path of its `at`, and
    mounts each of its `assets`, an image by its ID, at its path.

    Modal ends the sandbox at its `hard_end`, in seconds since the Unix epoch, if nothing ends it
    first, and its sidecars with it."""
    # Modal counts the timeout from its create, which this is just before.
    timeout = request["hard_end"] - int(time.time())
    if timeout <= 0:
        raise Refused("the create's hard end has passed: expected one to come")
    sandboxd = request["sandboxd"]
    unchanged(sandboxd)
    run = request["run"]
    phases = []
    with timed(phases, "lookup"):
        app = modal.App.lookup(APP, create_if_missing=True)
    image = modal.Image.from_id(request["image"])
    for at in sandboxd["at"]:
        image = image.add_local_file(sandboxd["path"], at, copy=False)
    cpu, memory = cores(request["cpu_millis"]), request["memory_mib"]
    with timed(phases, "create"):
        sandbox = modal.Sandbox.create(
            *request["args"],
            app=app,
            tags={RUN_TAG: run, ROLE_TAG: request["role"]},
            image=image,
            env=request["env"],
            timeout=timeout,
            gpu=request["gpu"],
            cpu=(cpu, cpu),
            memory=(memory, memory),
            runtime="gvisor",
            outbound_cidr_allowlist=[],
            inbound_cidr_allowlist=[request["source"]],
            encrypted_ports=request["ports"],
        )
    with timed(phases, "tunnels"):
        tunnels = sandbox.tunnels()
    with timed(phases, "list"):
        while sandbox.object_id not in ids(modal, app, run) and sandbox.poll() is None:
            time.sleep(LISTED_EVERY)
    if request["assets"]:
        with timed(phases, "assets"):
            mount(modal, sandbox, request["assets"])
    if request["sidecars"]:
        with timed(phases, "companions"):
            accompany(modal, sandbox, request["sidecars"])
    return {
        "id": sandbox.object_id,
        "tunnels": [
            {"host": tunnels[port].host, "port": tunnels[port].port} for port in request["ports"]
        ],
        "phases": phases,
    }


def mount(modal, sandbox, assets: list[dict]) -> None:
    """Mounts each of `assets` in `sandbox`'s own container. If one isn't mounted, this ends the
    sandbox, and then raises why."""
    for asset in assets:
        try:
            sandbox.mount_image(asset["path"], modal.Image.from_id(asset["image"]))
        # Whatever fails, as the SDK lets through errors of its transport that aren't its own, and
        # the sandbox is to be ended before any is reported.
        except Exception as error:
            why = f"{type(error).__name__}: {error}"
            break
    else:
        return
    was = "is ended"
    try:
        end(modal, sandbox)
    except modal.exception.Error as error:
        was = f"wasn't ended ({type(error).__name__}: {error}), so the run's list finds it"
    raise Refused(
        f"the asset at {asset['path']} wasn't mounted in sandbox {sandbox.object_id}: {why}; the "
        f"sandbox {was}"
    )


def accompany(modal, sandbox, sidecars: list[dict]) -> None:
    """Starts each of `sidecars` beside `sandbox`'s own container, at most `AT_ONCE` at a time.

    If one isn't created, this ends the sandbox, and then raises the first failure in the order of
    `sidecars`, by its sidecar's name. A sandbox that has ended refuses every sidecar, and Modal
    may not say for seconds that it has, so the first sidecar is then named for the sandbox's own
    end."""

    def start(sidecar: dict) -> str | None:
        try:
            sandbox._experimental_sidecars.create(
                *sidecar["args"],
                name=sidecar["name"],
                image=modal.Image.from_id(sidecar["image"]),
                env=sidecar["env"],
                outbound_cidr_allowlist=[],
            )
        # Whatever fails, as the SDK lets through errors of its transport that aren't its own, and
        # the sandbox is to be ended before any is reported.
        except Exception as error:
            return f"{type(error).__name__}: {error}"
        return None

    with ThreadPoolExecutor(AT_ONCE) as pool:
        failures = list(pool.map(start, sidecars))
    failed = [(sidecar["name"], why) for sidecar, why in zip(sidecars, failures) if why]
    if not failed:
        return
    name, why = failed[0]
    was = "is ended"
    try:
        status = sandbox.poll()
        if status is not None:
            was = f"had exited with status {status}"
    except modal.exception.Error:
        pass
    try:
        end(modal, sandbox)
    except modal.exception.Error as error:
        was = f"wasn't ended ({type(error).__name__}: {error}), so the run's list finds it"
    raise Refused(
        f"sidecar `{name}` of sandbox {sandbox.object_id} wasn't created: {why}; the sandbox {was}"
    )


@contextlib.contextmanager
def timed(phases: list[dict], name: str):
    """Appends to `phases` the step that the block takes, if it returns: its `name`, when it began
    by the machine's monotonic clock, which the crate reads too, and how long it took, each in
    nanoseconds."""
    began = time.monotonic_ns()
    yield
    phases.append({"name": name, "began_ns": began, "took_ns": time.monotonic_ns() - began})


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


def end(modal, sandbox) -> None:
    """Terminates `sandbox`, and returns once it has finished, its sidecars with it."""
    try:
        sandbox.terminate(wait=True)
    except modal.exception.SandboxTimeoutError:
        # The wait's report of a sandbox that Modal ended at its timeout, which has finished as
        # any other.
        pass


def delete(modal, request: dict) -> dict:
    failed = {}
    for id in request["ids"]:
        try:
            sandbox = owned(modal, request["run"], id)
            if sandbox is not None:
                end(modal, sandbox)
        except modal.exception.Error as error:
            failed[id] = f"{type(error).__name__}: {error}"
        except Refused as error:
            failed[id] = str(error)
    return {"failed": failed}


def console(modal, request: dict) -> str:
    """The last `CONSOLE_ENTRIES` entries of the sandbox's log, which holds its sidecars' output
    too, unmarked. Then, while the sandbox runs, a line `pumpkin-modal: sidecar <name> exited with
    status <N>` for each sidecar that has ended. Once Modal reports the sandbox ended, the one
    line `pumpkin-modal: sandbox <id> exited with status <N>` instead."""
    sandbox = owned(modal, request["run"], request["id"])
    if sandbox is None:
        raise Refused(f"Modal holds no sandbox {request['id']}: expected one of the run's")
    text = "".join(entry.message for entry in sandbox.logs.tail(CONSOLE_ENTRIES))
    # A sandbox that the kernel's OOM killer ends prints nothing of why: only its status, 137,
    # says how it ended. The same holds of a sidecar.
    status = sandbox.poll()
    if status is None:
        lines = [f"sidecar {name} exited with status {code}" for name, code in exited(sandbox)]
    else:
        lines = [f"sandbox {sandbox.object_id} exited with status {status}"]
    if not lines:
        return text
    if text and not text.endswith("\n"):
        text += "\n"
    return text + "".join(f"{CONSOLE_PREFIX} {line}\n" for line in lines)


def exited(sandbox) -> list[tuple[str, int]]:
    """Each sidecar of `sandbox`, which runs, that has ended, by its name, with its exit status."""
    try:
        every = sandbox._experimental_sidecars.list(include_terminated=True)
        running = {sidecar.object_id for sidecar in sandbox._experimental_sidecars.list()}
        ended = [sidecar for sidecar in every if sidecar.object_id not in running]
        # An ended sidecar's status came with the list, so its poll asks Modal nothing.
        return [(sidecar.name, sidecar.poll()) for sidecar in ended]
    # Whatever fails, as the console is for diagnostics alone, and the log's end is still worth
    # giving. A sandbox that ends meanwhile fails these calls.
    except Exception:
        return []


if __name__ == "__main__":
    main()
