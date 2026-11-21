#!/usr/bin/python3
"""Package the current checkout and print the path of the new submission archive."""

import argparse
import fcntl
import json
import subprocess
import tarfile
import tempfile
from pathlib import Path

WORKSPACE = Path("/workspace")


def package(output):
    # All invocations of the native packager replace the same workspace patch.
    with (WORKSPACE / ".team-pack.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        subprocess.run(
            [json.loads(Path("/opt/team/interface.json").read_bytes())["package"]],
            check=True,
            stdout=subprocess.DEVNULL,
        )
        with (WORKSPACE / "payload/solution.patch").open("rb") as patch:
            info = tarfile.TarInfo("payload/solution.patch")
            info.size = patch.seek(0, 2)
            info.mode = 0o600
            patch.seek(0)
            with tarfile.open(
                fileobj=output, mode="w", format=tarfile.USTAR_FORMAT
            ) as bundle:
                bundle.addfile(info, patch)
    if output.tell() > 262144:
        raise ValueError("the submission tar exceeds the 256 KiB limit")


def main():
    argparse.ArgumentParser(description=__doc__).parse_args()
    with tempfile.NamedTemporaryFile(
        dir=WORKSPACE, prefix="submission-", suffix=".tar", delete=False
    ) as output:
        path = Path(output.name)
        try:
            package(output)
        except BaseException:
            path.unlink(missing_ok=True)
            raise
    print(path)


if __name__ == "__main__":
    main()
