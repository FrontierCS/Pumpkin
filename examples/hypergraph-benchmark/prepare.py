"""Prepare image contexts from the official binding and its public Git source repository."""

import argparse
import errno
import hashlib
import json
import stat
import subprocess
import tempfile
from pathlib import Path

from source import build_inputs

DESTINATION = Path(__file__).resolve().parents[1] / "benchmark-input"
BINDING_SHA256 = "fc95851d530ef9dd72acff38207b6ffac4602c7ee5ac6148622dd0d5c1de7828"


def contents(root):
    if root.is_symlink() or not root.is_dir():
        raise ValueError(f"{root} must be a directory")
    entries = {}
    for path in root.rglob("*"):
        mode = path.lstat().st_mode
        if stat.S_ISDIR(mode):
            entries[path.relative_to(root)] = None
        elif stat.S_ISREG(mode):
            with path.open("rb") as stream:
                entries[path.relative_to(root)] = (
                    stat.S_IMODE(mode),
                    hashlib.file_digest(stream, "sha256").digest(),
                )
        else:
            raise ValueError(f"unexpected file type at {path}")
    return entries


def prepare(binding_file, repository, destination=DESTINATION):
    binding = json.loads(binding_file.read_bytes())
    data = json.dumps(binding, sort_keys=True, separators=(",", ":")).encode()
    if hashlib.sha256(data).hexdigest() != BINDING_SHA256:
        raise ValueError("binding differs from the official workload recipe")
    with tempfile.TemporaryDirectory(dir=destination.parent) as scratch:
        root = Path(scratch) / destination.name
        root.mkdir()
        build_inputs(binding, repository, root)
        try:
            root.rename(destination)
        except OSError as error:
            if error.errno not in (errno.EEXIST, errno.ENOTEMPTY):
                raise
            if contents(destination) != contents(root):
                raise ValueError(
                    f"{destination} differs from the pinned source"
                ) from error


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binding", type=Path)
    parser.add_argument(
        "repository", type=Path, help="Git repository with the pinned public commit"
    )
    args = parser.parse_args()
    try:
        prepare(args.binding, args.repository)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"prepare: {error}\n")
