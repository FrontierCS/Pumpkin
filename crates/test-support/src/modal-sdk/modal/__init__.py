"""A stand-in for Modal's SDK, for the tests of the Modal provider and of `pumpkin`, which find it
first on the helper's path, as the stand-in `uv` puts it there: it reaches nothing, and records
each call to it in `modal-calls`, in the directory that `FAKE_MODAL_DIR` names, a line each, its name and its
arguments in JSON, but the grant's digits.

As the SDK does, it takes the profile from `MODAL_PROFILE`: a profile reaches the workspace
`<profile>-workspace`, but `unknown`, which Modal's configuration lacks, or, if
`MODAL_CONFIG_PATH` names a file, one whose `[<profile>]` table that file lacks. An image it
imports gets an id made of its reference's digest, unless that directory holds `import-fails`,
whose text it fails with. If that directory holds `clock`, the helper reads the time from it, in
seconds since the Unix epoch.

A sandbox it creates is a process of this machine's, `../sandbox.py`, which runs the real
`sandboxd` that the image attaches, in a container of its image if it imported that, behind a
stand-in for Modal's TLS edge: it keeps its files in
`sandboxes/<id>` there, `sb-1` on, and outlives the call, until it is terminated, or its
`sandboxd` exits, as `returncode` there then says. Its tunnel's host is `localhost`, which the
stand-in edge's certificate is for, as `edge.pem` and `edge.key` in that directory hold it.
"""

import hashlib
import json
import os
import signal
import subprocess
import sys
import time
from pathlib import Path

from modal import exception

__version__ = "1.6.3"

DIR = Path(os.environ["FAKE_MODAL_DIR"])

# The clock a test sets, in seconds since the Unix epoch, which the helper then reads in place of
# this machine's, so that a timeout it takes from it is exact.
if (DIR / "clock").exists():
    time.time = lambda: float((DIR / "clock").read_text())
SANDBOXES = DIR / "sandboxes"
IMAGES = DIR / "images"

# How long a sandbox whose runner still runs takes at most to say where its tunnel is: only a test
# that fails waits this long.
STARTED_SECONDS = 30


def called(call: str, **arguments: object) -> None:
    with (DIR / "modal-calls").open("a") as calls:
        calls.write(f"modal: {call} {json.dumps(arguments, sort_keys=True)}\n")


class Workspace:
    def __init__(self) -> None:
        self.name: str | None = None

    @staticmethod
    def from_context() -> "Workspace":
        return Workspace()

    def hydrate(self) -> "Workspace":
        profile = os.environ["MODAL_PROFILE"]
        called("Workspace.hydrate", profile=profile)
        config = os.environ.get("MODAL_CONFIG_PATH")
        known = profile != "unknown"
        if config is not None:
            try:
                with open(config) as file:
                    known = f"[{profile}]" in file.read()
            except FileNotFoundError:
                known = False
        if not known:
            raise exception.AuthError(
                f"Modal profile '{profile}' was not found in {config or '~/.modal.toml'}. Run "
                "`modal profile list` to see available profiles"
            )
        self.name = f"{profile}-workspace"
        return self


class App:
    def __init__(self, name: str) -> None:
        self.name = self.app_id = name

    @staticmethod
    def lookup(name: str, *, create_if_missing: bool = False) -> "App":
        called("App.lookup", name=name, create_if_missing=create_if_missing)
        apps = DIR / "apps"
        apps.mkdir(exist_ok=True)
        if not (apps / name).exists():
            if not create_if_missing:
                raise exception.NotFoundError(f"App '{name}' not found")
            (apps / name).write_text("")
        return App(name)


class Secret:
    def __init__(self, values: dict[str, str]) -> None:
        self.values = values

    @staticmethod
    def from_dict(values: dict[str, str]) -> "Secret":
        return Secret(values)


class Image:
    def __init__(self, layers: list[dict[str, object]]) -> None:
        self.layers = layers
        self.object_id: str | None = None

    @staticmethod
    def from_registry(reference: str, *, secret: Secret) -> "Image":
        image = Image([{"from_registry": reference, "secret": secret.values}])
        return image

    @staticmethod
    def from_id(image_id: str) -> "Image":
        return Image([{"from_id": image_id}])

    def add_local_file(self, local: str, remote: str, *, copy: bool) -> "Image":
        return Image([*self.layers, {"add_local_file": local, "remote": remote, "copy": copy}])

    def build(self, app: App) -> "Image":
        [layer] = self.layers
        called("Image.build", app=app.name, reference=layer["from_registry"], secret=layer["secret"])
        fails = DIR / "import-fails"
        if fails.exists():
            raise exception.InvalidError(fails.read_text().strip())
        digest = hashlib.sha256(layer["from_registry"].encode()).hexdigest()
        self.object_id = f"im-{digest[:22]}"
        IMAGES.mkdir(exist_ok=True)
        (IMAGES / self.object_id).write_text(layer["from_registry"].split("@")[-1])
        return self


class Tunnel:
    def __init__(self, host: str, port: int) -> None:
        self.host, self.port = host, port


class Entry:
    def __init__(self, message: str) -> None:
        self.message = message


class Logs:
    def __init__(self, dir: Path) -> None:
        self.dir = dir

    def tail(self, entries: int) -> list[Entry]:
        lines = (self.dir / "console").read_text(errors="replace").splitlines(keepends=True)
        return [Entry(line) for line in lines[-entries:]]


class Sandbox:
    def __init__(self, id: str) -> None:
        self.object_id = id
        self.dir = SANDBOXES / id

    @property
    def returncode(self) -> int | None:
        code = self.dir / "returncode"
        return int(code.read_text()) if code.exists() else None

    @property
    def logs(self) -> Logs:
        return Logs(self.dir)

    @staticmethod
    def create(*args: str, app: App, tags: dict[str, str], image: Image, env: dict[str, str],
               **options: object) -> "Sandbox":
        shown = {key: "<grant>" if key == "SANDBOXD_GRANT" else value for key, value in env.items()}
        called("Sandbox.create", args=args, app=app.name, tags=tags, image=image.layers,
               env=shown, **options)
        SANDBOXES.mkdir(exist_ok=True)
        sandbox = Sandbox(f"sb-{len(list(SANDBOXES.iterdir())) + 1}")
        sandbox.dir.mkdir()
        (sandbox.dir / "app").write_text(app.name)
        (sandbox.dir / "tags.json").write_text(json.dumps(tags))
        local = next(layer["add_local_file"] for layer in image.layers
                     if layer.get("remote") == args[0])
        imported = IMAGES / image.layers[0]["from_id"]
        digest = imported.read_text() if imported.exists() else None
        request = {"sandboxd": local, "args": list(args), "grant": env["SANDBOXD_GRANT"],
                   "digest": digest}
        name = hashlib.sha256(str(sandbox.dir).encode()).hexdigest()[:16]
        (sandbox.dir / "container").write_text(f"modal-stand-in-{name}")
        (sandbox.dir / "request.json").write_text(json.dumps(request))
        runner = Path(__file__).resolve().parent.parent / "sandbox.py"
        with open(sandbox.dir / "runner.log", "wb") as log:
            process = subprocess.Popen(
                [sys.executable, "-I", "-B", str(runner), str(sandbox.dir)],
                stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True,
            )
        (sandbox.dir / "pid").write_text(str(process.pid))
        return sandbox

    def tunnels(self) -> dict[int, Tunnel]:
        called("Sandbox.tunnels", id=self.object_id)
        deadline = time.monotonic() + STARTED_SECONDS
        pid = int((self.dir / "pid").read_text())
        while True:
            # Before the port is looked for, so that a runner that wrote it, then ended, is seen
            # to have opened its tunnel.
            ended = not alive(pid)
            if (self.dir / "port").exists():
                return {7434: Tunnel("localhost", int((self.dir / "port").read_text()))}
            if ended or time.monotonic() > deadline:
                raise exception.Error(f"sandbox {self.object_id} opened no tunnel")
            time.sleep(0.05)

    def poll(self) -> int | None:
        return self.returncode

    @staticmethod
    def list(*, app_id: str, tags: dict[str, str] | None = None) -> list["Sandbox"]:
        called("Sandbox.list", app_id=app_id, tags=tags)
        if not SANDBOXES.exists():
            return []
        held = [Sandbox(dir.name) for dir in sorted(SANDBOXES.iterdir())]
        return [sandbox for sandbox in held if sandbox.returncode is None
                and (sandbox.dir / "app").read_text() == app_id
                and (tags or {}).items() <= sandbox.get_tags().items()]

    @staticmethod
    def from_id(id: str) -> "Sandbox":
        called("Sandbox.from_id", id=id)
        if not id.startswith("sb-"):
            raise exception.InvalidError(f'"{id}" is not a valid sandbox ID')
        if not (SANDBOXES / id).exists():
            raise exception.NotFoundError(f"Sandbox with id {id} not found")
        return Sandbox(id)

    def get_tags(self) -> dict[str, str]:
        return json.loads((self.dir / "tags.json").read_text())

    def terminate(self, *, wait: bool = False) -> int | None:
        called("Sandbox.terminate", id=self.object_id, wait=wait)
        pid = int((self.dir / "pid").read_text())
        try:
            os.killpg(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        if json.loads((self.dir / "request.json").read_text())["digest"] is not None:
            container = (self.dir / "container").read_text()
            subprocess.run(["docker", "rm", "--force", container], capture_output=True)
        if self.returncode is None:
            (self.dir / "returncode").write_text(str(-signal.SIGKILL))
        deadline = time.monotonic() + STARTED_SECONDS
        while wait and alive(pid) and time.monotonic() < deadline:
            time.sleep(0.02)
        return self.returncode if wait else None


def alive(pid: int) -> bool:
    """Whether process `pid` runs: it exists, and isn't a zombie, which its parent has yet to
    reap."""
    try:
        stat = Path(f"/proc/{pid}/stat").read_text()
    except FileNotFoundError:
        return False
    return stat.rsplit(")", 1)[1].split()[0] != "Z"
