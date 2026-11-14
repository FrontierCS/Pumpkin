"""A stand-in Modal sandbox, which the stand-in for Modal's SDK starts, as
`python3 -I sandbox.py <dir>`, detached, in a session of its own, so that it outlives the helper's
call: in `<dir>`, `request.json` holds its `sandboxd`'s path and arguments, its grant, and the
digest of its image, if the stand-in imported it, and `container` the name of its container,
which carries the label `pumpkin-modal-stand-in`, of the stand-in SDK's directory, two above
`<dir>`.

It runs that `sandboxd`, listening at a port of the loopback in place of the one its arguments
name: as the first process of a container of that image on the tests' Docker engine, started as
root, on the machine's network, as `docker run` in front of it; or, if the image is none the
stand-in imported, as a process of this machine's, without `--user`, as the tests don't run as
root. Its output goes to `console`;
and it stands in front of that port as Modal's tunnel does, ending TLS with the stand-in edge's
certificate, `edge.pem` and `edge.key` two directories above `<dir>`, at a port of the loopback it writes to
`port`. Once `sandboxd` exits it writes its status to `returncode` and ends.
"""

import json
import os
import socket
import ssl
import subprocess
import sys
import threading
import time
from pathlib import Path

# Where a sandbox whose role makes ops has `sandboxctl` (`schema::sandbox::SANDBOXCTL_PATH`).
SANDBOXCTL_PATH = "/.pumpkin/bin/sandboxctl"

# How long the front tries to reach `sandboxd` for a connection it took, as Modal's edge holds a
# connection until the sandbox's server listens.
REACH_SECONDS = 30


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def pump(source: socket.socket, sink: socket.socket) -> None:
    try:
        while data := source.recv(65536):
            sink.sendall(data)
    except OSError:
        pass
    for end in (sink, source):
        try:
            end.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass


def serve(client: socket.socket, target: int) -> None:
    deadline = time.monotonic() + REACH_SECONDS
    while True:
        try:
            upstream = socket.create_connection(("127.0.0.1", target))
            break
        except OSError:
            if time.monotonic() > deadline:
                client.close()
                return
            time.sleep(0.05)
    threading.Thread(target=pump, args=(client, upstream), daemon=True).start()
    pump(upstream, client)


def main() -> None:
    dir = Path(sys.argv[1])
    request = json.loads((dir / "request.json").read_text())
    target = free_port()
    args, kept = request["args"], []
    skip = False
    container = request["digest"] is not None
    for arg in args[1:]:
        if skip:
            skip = False
        elif arg == "--user" and not container:
            skip = True
        elif arg.startswith("0.0.0.0:"):
            kept.append(f"127.0.0.1:{target}")
        else:
            kept.append(arg)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(dir.parent.parent / "edge.pem", dir.parent.parent / "edge.key")
    listener = socket.socket()
    # A small buffer, which holds back what the gateway sends once `sandboxd` stops reading, as
    # a stalled sandbox's own socket does.
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 65536)
    listener.bind(("127.0.0.1", 0))
    listener.listen()
    (dir / "port").write_text(str(listener.getsockname()[1]))
    with open(dir / "console", "wb") as console:
        env = {"SANDBOXD_GRANT": request["grant"], "PATH": os.environ["PATH"]}
        if container:
            env["HOME"] = os.environ["HOME"]
            mount = "type=bind,source={},target={},readonly"
            command = [
                "docker", "run", "--rm", f"--name={(dir / 'container').read_text()}",
                f"--label=pumpkin-modal-stand-in={dir.parent.parent}",
                "--network=host", "--user=0:0", "--env=SANDBOXD_GRANT",
                "--mount=" + mount.format(request["sandboxd"], args[0]),
            ]
            if "--ops" in args:
                command.append("--mount=" + mount.format(request["sandboxd"], SANDBOXCTL_PATH))
            command += [f"--entrypoint={args[0]}", request["digest"], *kept]
        else:
            command = [request["sandboxd"], *kept]
        try:
            child = subprocess.Popen(command, stdout=console, stderr=console, env=env)
        except OSError as error:
            console.write(f"exec failed: {error}\n".encode())
            (dir / "returncode").write_text("127")
            return

        def accept() -> None:
            while True:
                client, _ = listener.accept()
                threading.Thread(target=secure, args=(client,), daemon=True).start()

        def secure(client: socket.socket) -> None:
            try:
                client = context.wrap_socket(client, server_side=True)
            except (OSError, ssl.SSLError):
                client.close()
                return
            serve(client, target)

        threading.Thread(target=accept, daemon=True).start()
        code = child.wait()
    if container:
        # As `--rm` leaves one that never started, such as one whose `sandboxd` can't run.
        name = (dir / "container").read_text()
        subprocess.run(["docker", "rm", "--force", name], capture_output=True, env=env)
    (dir / "returncode").write_text(str(code))


if __name__ == "__main__":
    main()
