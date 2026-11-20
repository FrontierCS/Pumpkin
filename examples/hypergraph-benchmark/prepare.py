"""Prepare the native benchmark image contexts from an operator-supplied JSON binding."""

import argparse
import errno
import hashlib
import json
import re
import tempfile
from pathlib import Path
from urllib.parse import urlsplit
from urllib.request import urlopen

DESTINATION = Path(__file__).resolve().parents[1] / "benchmark-input"
FILES = {
    "LICENSE": (
        18657,
        "9ba9550ad48438d0836ddab3da480b3b69ffa0aac7b7878b5a0039e7ab429411",
    ),
    "train.json": (
        346,
        "639b86ebca311ac505235a2727e42381b3f4e95a9863f7519b9e9aaec5eed8e7",
    ),
    "eval.json": (
        313,
        "a6ff2b61e604f152cf22ef1f7f6888e64d191029b2b40ecedfceaa3ef526210e",
    ),
}
# These are the three files that must be removed from the pinned training image.
WITHHELD_DIGEST = "d75609b9f45a7c8f5659329a33bd948225095920b3418475464e83baae7867a7"
REPOSITORY = re.compile(
    r"[a-z0-9]+(?:[.-][a-z0-9]+)*(?::[0-9]+)?(?:/[a-z0-9]+(?:[._-]+[a-z0-9]+)*)+"
)
ABSOLUTE_PATH = re.compile(r"(?:/[A-Za-z0-9_.-]+)+")


def fields(obj, expected, label):
    if not isinstance(obj, dict) or set(obj) != set(expected):
        raise ValueError(f"{label} must contain exactly {', '.join(sorted(expected))}")


def check_file(name, data):
    size, digest = FILES[name]
    if len(data) != size or hashlib.sha256(data).hexdigest() != digest:
        raise ValueError(f"{name} differs from the pinned benchmark input")


def context(binding):
    fields(
        binding,
        [
            "agent_repository",
            "verifier_repository",
            "interface",
            "files",
            "attribution",
        ],
        "binding",
    )
    fields(binding["files"], FILES, "files")
    if not all(isinstance(v, str) and v for v in binding["files"].values()):
        raise ValueError("each file needs a source path or HTTPS URL")
    images = ['"""Repositories supplying the pinned native images."""', ""]
    for key in ("agent_repository", "verifier_repository"):
        value = binding[key]
        if not isinstance(value, str) or not REPOSITORY.fullmatch(value):
            raise ValueError(f"{key} must be a repository name without a tag or digest")
        images.append(f"{key.upper()} = {json.dumps(value)}")

    interface = binding["interface"]
    fields(interface, ["agent", "judge"], "interface")
    agent, judge = interface["agent"], interface["judge"]
    fields(agent, ["prepare", "package"], "agent interface")
    fields(judge, ["command", "home", "env", "withheld"], "judge interface")
    fields(
        judge["env"],
        ["mode", "scratch", "payload", "dataset", "result", "evidence"],
        "judge env",
    )
    env = list(judge["env"].values())
    if not all(
        isinstance(v, str) and re.fullmatch(r"[A-Z_][A-Z0-9_]*", v) for v in env
    ) or len(set(env)) != len(env):
        raise ValueError("judge environment variables must be distinct uppercase names")
    withheld = judge["withheld"]
    if not isinstance(withheld, list) or len(withheld) != 3:
        raise ValueError("withheld must contain the three required absolute paths")
    for path in [*agent.values(), judge["command"], judge["home"], *withheld]:
        if (
            not isinstance(path, str)
            or not ABSOLUTE_PATH.fullmatch(path)
            or any(p in (".", "..") for p in path.split("/"))
        ):
            raise ValueError("interface paths must be absolute paths without traversal")
    path_list = json.dumps(sorted(withheld), separators=(",", ":")).encode()
    if hashlib.sha256(path_list).hexdigest() != WITHHELD_DIGEST:
        raise ValueError(
            "withheld paths differ from the pinned training image contract"
        )
    attribution = binding["attribution"]
    if not isinstance(attribution, str) or not attribution.strip():
        raise ValueError("the binding must include attribution")
    docs = {"images.star": ("\n".join(images) + "\n").encode()}
    for name, spec in interface.items():
        docs[f"{name}/interface.json"] = (
            json.dumps(spec, indent=2, sort_keys=True) + "\n"
        ).encode()
        docs[f"{name}/ATTRIBUTION"] = (attribution.rstrip() + "\n").encode()
    return docs


def read_source(binding_file, source, name):
    size, _ = FILES[name]
    url = urlsplit(source)
    if url.scheme == "https" and url.hostname and not url.username and not url.password:
        stream = urlopen(source, timeout=30)
    elif not url.scheme and not url.netloc:
        stream = (binding_file.parent / source).open("rb")
    else:
        raise ValueError(
            f"{name} needs a file path or an HTTPS URL without credentials"
        )
    with stream:
        data = stream.read(size + 1)
    check_file(name, data)
    return data


def check_existing(root, docs, assets):
    if root.is_symlink() or not root.is_dir():
        raise ValueError(f"{root} must be a directory")
    paths = list(root.rglob("*"))
    if any(path.is_symlink() for path in paths) or {
        p.relative_to(root).as_posix() for p in paths
    } != set(docs) | set(assets) | {"agent", "judge"}:
        raise ValueError(f"unexpected files in {root}")
    for name, expected in docs.items():
        path = root / name
        if (
            not path.is_file()
            or path.stat().st_size != len(expected)
            or path.read_bytes() != expected
        ):
            raise ValueError(f"existing {name} differs from the binding")
    for name, asset in assets.items():
        path = root / name
        if not path.is_file() or path.stat().st_size != FILES[asset][0]:
            raise ValueError(f"existing {name} differs from the pinned benchmark input")
        check_file(asset, path.read_bytes())


def prepare(binding_file, destination=DESTINATION):
    binding = json.loads(binding_file.read_bytes())
    docs = context(binding)
    assets = {"judge/" + name: name for name in FILES} | {"agent/LICENSE": "LICENSE"}
    if destination.exists() or destination.is_symlink():
        check_existing(destination, docs, assets)
        return
    with tempfile.TemporaryDirectory(dir=destination.parent) as scratch:
        root = Path(scratch) / destination.name
        (root / "agent").mkdir(parents=True)
        (root / "judge").mkdir()
        inputs = {
            name: read_source(binding_file, source, name)
            for name, source in binding["files"].items()
        }
        for path, data in docs.items():
            (root / path).write_bytes(data)
        for path, asset in assets.items():
            (root / path).write_bytes(inputs[asset])
        try:
            root.rename(destination)
        except OSError as error:
            if error.errno not in (errno.EEXIST, errno.ENOTEMPTY):
                raise
            check_existing(destination, docs, assets)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binding", type=Path)
    try:
        prepare(parser.parse_args().binding)
    except (OSError, ValueError) as error:
        parser.exit(1, f"prepare: {error}\n")
