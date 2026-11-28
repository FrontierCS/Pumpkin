#!/usr/bin/python3
"""Package the current checkout and print the path of the new submission archive."""

import argparse
import io
import os
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

import edit_scope

STARTER = Path("/opt/starter")
SOURCE = Path("/workspace/repo")
WORKSPACE = Path("/workspace")
BASELINE_REF = "refs/starter/baseline"
MAX_ARCHIVE_BYTES = 256 * 1024


class Refused(Exception):
    pass


def make_patch(scratch):
    """The patch of the checkout against the starter commit, checked against the edit scope.

    It holds every change to a starter file, committed or not, so that an edit out of scope is
    reported rather than dropped, and new files below the editable paths alone, so that build
    outputs elsewhere stay out.
    """
    env = {
        key: value for key, value in os.environ.items() if not key.startswith("GIT_")
    }
    env.update(
        GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull, GIT_LITERAL_PATHSPECS="1"
    )
    env["GIT_INDEX_FILE"] = str(scratch / "index")
    git = [
        "git",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.quotePath=false",
        "-c",
        "core.autocrlf=false",
        "-C",
        str(SOURCE),
    ]

    def run(*args):
        return subprocess.run(
            [*git, *args], env=env, capture_output=True, check=True, timeout=60
        ).stdout

    try:
        base = (
            run("rev-parse", "--verify", "--quiet", BASELINE_REF + "^{commit}")
            .decode()
            .strip()
        )
    except subprocess.CalledProcessError:
        raise Refused(f"{BASELINE_REF} is missing from {SOURCE}") from None
    run("read-tree", base)
    run("add", "--update")
    new = [
        name
        for name in run("ls-files", "--others", "--exclude-standard", "-z")
        .decode("utf-8", "surrogateescape")
        .split("\0")
        if name and edit_scope.editable(name)
    ]
    if new:
        listing = scratch / "new"
        listing.write_bytes("\0".join(new).encode("utf-8", "surrogateescape") + b"\0")
        run("add", "--pathspec-from-file=" + str(listing), "--pathspec-file-nul")
    patch = run(
        "diff",
        "--cached",
        "--binary",
        "--no-renames",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        base,
    )
    result = edit_scope.check(patch, STARTER, build_tree=False)
    if not result.ok:
        raise Refused(result.message)
    return patch


def package(output):
    with tempfile.TemporaryDirectory(prefix="submission-", dir=WORKSPACE) as scratch:
        patch = make_patch(Path(scratch))
    info = tarfile.TarInfo("payload/solution.patch")
    info.size = len(patch)
    info.mode = 0o600
    with tarfile.open(fileobj=output, mode="w", format=tarfile.USTAR_FORMAT) as bundle:
        bundle.addfile(info, io.BytesIO(patch))
    if output.tell() > MAX_ARCHIVE_BYTES:
        raise ValueError("the submission tar exceeds the 256 KiB limit")


def main():
    argparse.ArgumentParser(description=__doc__).parse_args()
    with tempfile.NamedTemporaryFile(
        dir=WORKSPACE, prefix="submission-", suffix=".tar", delete=False
    ) as output:
        path = Path(output.name)
        try:
            package(output)
        except BaseException as error:
            path.unlink(missing_ok=True)
            if isinstance(error, Refused):
                sys.exit(f"submission refused: {error}")
            raise
    print(path)


if __name__ == "__main__":
    main()
