"""The edit scope of a submission, which the agent's packager and the judge both check.

A submission is a unified diff of the starter tree. Whether it stays inside the editable surface
is decided from the tree the patch produces, never from the patch text: the patch is applied,
strictly and once, to a scratch Git copy of the starter, and the changed files are read back
from Git. The judge then measures a fresh copy of the starter with only the changed files
overlaid, so nothing outside the surface can reach the measurement even if a check here were
wrong. Every rejection is a scored zero with one of CATEGORIES and its fixed sentence.
"""

from __future__ import annotations

import hashlib
import os
import re
import shutil
import stat
import subprocess
import tempfile
from dataclasses import dataclass, field
from pathlib import Path

# The public rejection categories, and the sentence the agent sees for each.
CATEGORIES = {
    "empty": "The patch changes nothing.",
    "too_large": "The patch is larger than the task allows.",
    "malformed": "The patch does not apply to the starter tree.",
    "forbidden_file_type": "The patch adds or changes a file of a kind the task does not allow "
                           "(binary, symlink, executable, submodule, or a mode change).",
    "out_of_scope": "The patch changes something outside the task's editable surface: a file "
                    "it may not change, add or delete, or a part of an editable file that must "
                    "stay as it is.",
    "too_many_files": "The patch changes more files than the task allows.",
    "content_policy": "The patch uses content the task forbids in the editable files.",
}

EDITABLE = "apps/hyper/HyperKCore-Efficient.C"
# New files may go below this directory; no file may be deleted.
SOLUTION = "apps/hyper/solution/"
MAX_CHANGED_FILES = 20
MAX_PATCH_BYTES = 256 * 1024
GIT_TIMEOUT_SECONDS = 300


@dataclass(frozen=True)
class Change:
    status: str  # A, M or D
    path: str


@dataclass
class Result:
    ok: bool
    category: str = ""
    message: str = ""
    paths: tuple[str, ...] = ()  # the offending paths of a rejection
    changes: tuple[Change, ...] = ()
    tree: Path | None = None     # the starter with the changed files: what the judge measures
    metrics: dict = field(default_factory=dict)
    _scratch: Path | None = None

    def dispose(self) -> None:
        if self._scratch is not None:
            _remove(self._scratch)
            self._scratch = None


def editable(path: str) -> bool:
    return path == EDITABLE or re.fullmatch(re.escape(SOLUTION) + ".*", path) is not None


def check(patch: bytes, pristine: str | Path, *, build_tree: bool = True) -> Result:
    """Apply `patch` to a scratch copy of `pristine` and decide whether it stays in scope.

    On success `result.tree` (when `build_tree`) is a fresh copy of `pristine` with only the
    changed files overlaid; the caller measures it and calls `result.dispose()`.
    """
    metrics = {"patch_bytes": len(patch), "patch_sha256": hashlib.sha256(patch).hexdigest(),
               "changed_files": 0}

    def reject(category: str, paths: list[str] | tuple[str, ...] = ()) -> Result:
        metrics["edit_scope"] = category
        shown = f" ({', '.join(paths[:5])}{', ...' if len(paths) > 5 else ''})" if paths else ""
        return Result(False, category, CATEGORIES[category] + shown, tuple(paths), metrics=metrics)

    pristine = Path(pristine)
    if not pristine.is_dir():
        raise FileNotFoundError(f"no pristine tree at {pristine}")
    if not patch:
        return reject("empty")
    if len(patch) > MAX_PATCH_BYTES:
        return reject("too_large")
    if not patch.strip():
        return reject("malformed")
    if b"\x00" in patch or b"GIT binary patch" in patch or b"\nBinary files " in patch:
        return reject("forbidden_file_type")

    scratch = Path(tempfile.mkdtemp(prefix="edit-scope-"))
    try:
        base = scratch / "applied"
        shutil.copytree(pristine, base, symlinks=True)
        _make_owner_writable(base)
        env = _git_env(scratch)
        _git(base, env, "init", "--quiet")
        _git(base, env, "add", "--all", "--force")
        _git(base, env, "commit", "--quiet", "--allow-empty", "--no-verify", "-m", "pristine")
        staged = scratch / "submission.patch"
        staged.write_bytes(patch)
        # One strict application: no fuzz, no 3-way, no reject files, no fallbacks.
        if _git(base, env, "apply", "--index", "--whitespace=nowarn", str(staged),
                check=False).returncode != 0:
            _remove(scratch)
            return reject("malformed")

        changes, odd, new_executables = _changes(base, env)
        metrics["changed_files"] = len(changes)
        if odd:
            _remove(scratch)
            return reject("forbidden_file_type", odd)
        if not changes:
            _remove(scratch)
            return reject("empty")
        outside = [c.path for c in changes if c.status == "D" or not editable(c.path)]
        if outside:
            _remove(scratch)
            return reject("out_of_scope", outside)
        if new_executables:
            _remove(scratch)
            return reject("forbidden_file_type", new_executables)
        if len(changes) > MAX_CHANGED_FILES:
            _remove(scratch)
            return reject("too_many_files")

        tree = None
        if build_tree:
            tree = scratch / "tree"
            shutil.copytree(pristine, tree, symlinks=True)
            _make_owner_writable(tree)
            for change in changes:
                target = tree / change.path
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(base / change.path, target, follow_symlinks=False)
        return Result(True, changes=tuple(changes), tree=tree, metrics=metrics, _scratch=scratch)
    except BaseException:
        _remove(scratch)
        raise


def _changes(base: Path, env: dict) -> tuple[list[Change], list[str], list[str]]:
    """Every file the applied patch changed; the paths whose change is not a plain regular-file
    edit; and new executable files."""
    raw = _git(base, env, "diff", "--cached", "--raw", "--no-renames", "-z", "HEAD").stdout
    fields = raw.split("\0")
    changes, odd, new_executables = [], [], []
    for meta, path in zip(fields[0::2], fields[1::2]):
        old_mode, new_mode, _, _, status = meta[1:].split()
        status = status[0]
        changes.append(Change(status, path))
        modes = {m for m in (old_mode, new_mode) if m != "000000"}
        if not modes <= {"100644", "100755"} or (status == "M" and old_mode != new_mode):
            odd.append(path)
        elif status == "A" and new_mode == "100755":
            new_executables.append(path)
    # `git apply --index` stages what it writes; anything else it left behind is suspect.
    odd.extend(p for p in _git(base, env, "ls-files", "--others", "-z").stdout.split("\0") if p)
    return changes, odd, new_executables


def _git_env(scratch: Path) -> dict:
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    env.update(
        # Never discover an enclosing repository (a TMPDIR below a checkout), never read the
        # user's or system's git config, and commit with a fixed identity.
        GIT_CEILING_DIRECTORIES=str(scratch.resolve()), GIT_CONFIG_NOSYSTEM="1",
        GIT_CONFIG_GLOBAL=os.devnull, HOME=str(scratch),
        GIT_AUTHOR_NAME="edit-scope", GIT_AUTHOR_EMAIL="edit-scope@invalid",
        GIT_COMMITTER_NAME="edit-scope", GIT_COMMITTER_EMAIL="edit-scope@invalid",
        GIT_AUTHOR_DATE="1970-01-01T00:00:00Z", GIT_COMMITTER_DATE="1970-01-01T00:00:00Z",
    )
    return env


def _git(cwd: Path, env: dict, *args: str, check: bool = True) -> subprocess.CompletedProcess:
    command = ["git", "-c", "core.fileMode=true", "-c", "core.symlinks=true",
               "-c", "core.autocrlf=false", "-c", "core.quotePath=false", *args]
    result = subprocess.run(command, cwd=cwd, env=env, capture_output=True, text=True,
                            errors="surrogateescape", timeout=GIT_TIMEOUT_SECONDS)
    if check and result.returncode != 0:
        raise RuntimeError(f"git {args[0]} failed: {(result.stderr or result.stdout).strip()[:300]}")
    return result


def _make_owner_writable(root: Path) -> None:
    """The starter is read-only in the image; only this disposable copy becomes writable."""
    for directory, names, files in os.walk(root, followlinks=False):
        for path in [Path(directory), *(Path(directory) / name for name in names + files)]:
            if path.is_symlink():
                continue
            mode = path.lstat().st_mode
            if stat.S_ISDIR(mode) or stat.S_ISREG(mode):
                os.chmod(path, stat.S_IMODE(mode) | stat.S_IWUSR, follow_symlinks=False)


def _remove(root: Path) -> None:
    try:
        _make_owner_writable(root)
    except OSError:
        pass
    shutil.rmtree(root, ignore_errors=True)
