"""Verify one submission and write its result and evidence.

    verifier.py {train|eval} PAYLOAD DATASET RESULT EVIDENCE

PAYLOAD is the submitted tar, which holds `payload/solution.patch`, and DATASET the selector of
the suite, whose digest the evidence records. The patch must stay in the edit scope, and the
evaluator's `in_scope` hook must accept the tree; otherwise the submission scores zero with one
of `edit_scope.CATEGORIES`, and the evaluator never runs. Eval runs the final suite, and train
the development suite.

RESULT is the JSON a train call returns to the agent: the score, the evaluator's message, and,
in train mode, the feedback metrics and each metric's pass counts over the workloads. The exact
per-workload decisions and the reference's measurements stay in EVIDENCE, a zstd-compressed tar
that holds `metrics.json`.
"""

from __future__ import annotations

import ctypes
import hashlib
import io
import json
import os
import shutil
import sys
import tarfile
import tempfile
import time
import traceback
from pathlib import Path, PurePosixPath

import edit_scope
import evaluator
import zstandard

ENTRYPOINT = "payload/solution.patch"
# The evaluator's metrics that a train result returns to the agent.
FEEDBACK = (
    "candidate_runtime_seconds_median",
    "candidate_sequential_runtime_seconds_median",
)
SCRATCH = "/scratch"
MAX_PAYLOAD_BYTES = 64 * 1024 * 1024
MAX_PAYLOAD_MEMBERS = 4096
MAX_MESSAGE_CHARS = 4096
MAX_PRIVATE_EXCEPTION_BYTES = 8192
PR_SET_DUMPABLE = 4


class VerifierFailure(Exception):
    """A bounded, structured reason the submission could not be scored."""

    def __init__(
        self, code: str, message: str, *, private_evidence: dict | None = None
    ):
        super().__init__(message)
        self.code = code
        self.message = message
        self.private_evidence = private_evidence


def extract_payload(payload: Path, destination: Path) -> bytes:
    """Unpack the tar, refusing a member that is no plain file or directory inside it, and
    return the patch."""
    try:
        size = payload.stat().st_size
    except OSError as error:
        raise VerifierFailure(
            "invalid_submission", "payload is not readable"
        ) from error
    if not 0 < size <= MAX_PAYLOAD_BYTES:
        raise VerifierFailure(
            "invalid_submission", f"payload must be 1..={MAX_PAYLOAD_BYTES} bytes"
        )
    destination.mkdir(parents=True, exist_ok=True)
    seen: set[str] = set()
    try:
        with tarfile.open(payload, mode="r:") as archive:
            members = []
            for member in archive:
                name = member.name
                if "\\" in name or "\x00" in name or name.startswith("/"):
                    raise VerifierFailure(
                        "invalid_submission", "payload member path is not relative"
                    )
                if any(part in ("", ".", "..") for part in PurePosixPath(name).parts):
                    raise VerifierFailure(
                        "invalid_submission", "payload member path escapes the archive"
                    )
                if not (member.isfile() or member.isdir()):
                    raise VerifierFailure(
                        "invalid_submission",
                        "payload contains a link, device or FIFO member",
                    )
                if name in seen:
                    raise VerifierFailure(
                        "invalid_submission", "payload contains duplicate member paths"
                    )
                seen.add(name)
                members.append(member)
                if len(members) > MAX_PAYLOAD_MEMBERS:
                    raise VerifierFailure(
                        "invalid_submission", "payload has too many members"
                    )
            archive.extractall(destination, members=members, filter="tar")
    except tarfile.TarError as error:
        raise VerifierFailure(
            "invalid_submission", "payload is not an uncompressed POSIX tar"
        ) from error
    patch = destination / ENTRYPOINT
    if not patch.is_file() or patch.is_symlink():
        raise VerifierFailure(
            "invalid_submission",
            f"payload does not contain a regular file at {ENTRYPOINT}",
        )
    return patch.read_bytes()


def run_evaluator(tree: Path, mode: str) -> tuple[float, float, str, dict]:
    try:
        return evaluator.evaluate(str(tree), mode == "eval")
    except Exception as error:  # the evaluator's crash is a verification failure
        # The public result keeps only the exception class; the evidence keeps its bounded
        # traceback.
        tail = bytearray()
        diagnostic = traceback.TracebackException.from_exception(error, limit=8)
        for part in diagnostic.format(chain=True):
            tail.extend(part.encode("utf-8", errors="replace"))
            del tail[:-MAX_PRIVATE_EXCEPTION_BYTES]
        private_tail = tail.decode("utf-8", errors="ignore")
        print(
            "verifier: evaluator exception tail:\n" + private_tail,
            file=sys.stderr,
            flush=True,
        )
        raise VerifierFailure(
            "evaluator_error",
            f"the task evaluator raised {type(error).__name__}",
            private_evidence={
                "evaluator_exception": {
                    "exception_type": type(error).__name__[:256],
                    "traceback_tail": private_tail,
                }
            },
        ) from error


def progress(metrics: dict) -> dict:
    """How many workloads pass each metric, without saying which."""
    rows = metrics["beat_spec_workload_passes"]
    total = len(rows)
    passed = {
        name: sum(row["metrics"][name] for row in rows) for name in rows[0]["metrics"]
    }
    return {
        name: {
            "passed": count,
            "failed": total - count,
            "total": total,
            "pass_percent": round(100.0 * count / total, 2),
            "fail_percent": round(100.0 * (total - count) / total, 2),
        }
        for name, count in passed.items()
    }


def write_json(path: Path, payload: dict) -> None:
    body = json.dumps(
        payload,
        allow_nan=False,
        ensure_ascii=True,
        separators=(",", ":"),
        sort_keys=True,
    ).encode("ascii")
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_bytes(body)
    temporary.replace(path)


def write_evidence(path: Path, metrics: dict) -> None:
    """A zstd-compressed tar of `metrics.json`, the same bytes for the same metrics."""
    data = json.dumps(metrics, allow_nan=False, indent=2, sort_keys=True).encode(
        "utf-8"
    )
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        info = tarfile.TarInfo("metrics.json")
        info.size = len(data)
        info.mtime = 0
        info.mode = 0o644
        info.uid = info.gid = 0
        info.uname = info.gname = ""
        archive.addfile(info, io.BytesIO(data))
    path.write_bytes(zstandard.ZstdCompressor(level=10).compress(buffer.getvalue()))


def sha256_file(path: Path) -> str:
    with path.open("rb") as source:
        return "sha256:" + hashlib.file_digest(source, "sha256").hexdigest()


def verify(
    mode: str, payload: Path, dataset: Path, result_path: Path, evidence_path: Path
) -> None:
    started = time.time()
    scratch = Path(tempfile.mkdtemp(prefix="verify-", dir=SCRATCH))
    # Build tools that the evaluator runs write their temporary files and caches here.
    for name, subdir in (
        ("TMPDIR", "tmp"),
        ("TMP", "tmp"),
        ("TEMP", "tmp"),
        ("HOME", "home"),
        ("XDG_CACHE_HOME", "home/.cache"),
    ):
        (scratch / subdir).mkdir(parents=True, exist_ok=True)
        os.environ[name] = str(scratch / subdir)
    receipt = {"mode": mode}

    def finish(result: dict, evidence: dict) -> None:
        write_evidence(
            evidence_path,
            receipt | evidence | {"elapsed_seconds": round(time.time() - started, 3)},
        )
        write_json(result_path, result)

    def reject(category: str, paths, metrics: dict) -> None:
        shown = (
            f" ({', '.join(paths[:5])}{', ...' if len(paths) > 5 else ''})"
            if paths
            else ""
        )
        message = edit_scope.CATEGORIES[category] + shown
        result = {
            "status": "passed",
            "raw_score": 0.0,
            "summary": message[:MAX_MESSAGE_CHARS],
            "metrics": {"score_unbounded": 0.0, "rejected": category},
        }
        if mode == "eval":
            result["private_metrics"] = {"beats_reference": 0, "rejected": category}
        finish(
            result,
            {
                "raw_score": 0.0,
                "score_unbounded": 0.0,
                "message": message,
                "rejection": {"category": category, "paths": list(paths)},
                "edit_scope": metrics,
            },
        )

    try:
        patch = extract_payload(payload, scratch / "payload")
        receipt |= {
            "payload_sha256": sha256_file(payload),
            "dataset_sha256": sha256_file(dataset),
        }
        scope = edit_scope.check(patch, evaluator.PRISTINE)
        try:
            if not scope.ok:
                return reject(scope.category, scope.paths, scope.metrics)
            if not evaluator.in_scope(scope.tree):
                return reject("out_of_scope", (evaluator.EDITABLE,), scope.metrics)
            score, unbounded, message, metrics = run_evaluator(scope.tree, mode)
            if "rejected" in metrics:
                # A rejection that only the build can find.
                paths = tuple(c.path for c in scope.changes)
                return reject(metrics["rejected"], paths, scope.metrics)
        except VerifierFailure as failure:
            write_evidence(
                evidence_path,
                receipt
                | {
                    "execution_valid": False,
                    "metrics": failure.private_evidence,
                    "elapsed_seconds": round(time.time() - started, 3),
                },
            )
            message = failure.message[:MAX_MESSAGE_CHARS]
            write_json(
                result_path,
                {
                    "status": "failed",
                    "summary": message,
                    "metrics": {},
                    "failure": {
                        "code": failure.code,
                        "message": message,
                        "retryable": False,
                    },
                },
            )
            return None
        finally:
            scope.dispose()

        visible = {"score_unbounded": unbounded}
        private = None
        if mode == "train":
            visible |= {key: metrics[key] for key in FEEDBACK if key in metrics}
            visible["beat_spec_progress"] = progress(metrics)
        else:
            private = {
                "beat_spec_progress": progress(metrics),
                "beats_reference": metrics["beats_reference"],
                "reference_score": float(metrics["reference_score"]),
                "margin": float(metrics["margin"]),
            }
        result = {
            "status": "passed",
            "raw_score": score,
            "summary": message[:MAX_MESSAGE_CHARS],
            "metrics": visible,
        }
        if private is not None:
            result["private_metrics"] = private
        finish(
            result,
            {
                "raw_score": score,
                "score_unbounded": unbounded,
                "message": message,
                "metrics": metrics,
            },
        )
        return None
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


def main(mode: str, payload: str, dataset: str, result: str, evidence: str) -> int:
    result_path = Path(result)
    try:
        # No process of the same user, such as a submitted program, may read this one's memory.
        if ctypes.CDLL(None, use_errno=True).prctl(PR_SET_DUMPABLE, 0, 0, 0, 0) != 0:
            raise VerifierFailure(
                "verifier_configuration_error",
                "the verifier could not make itself undumpable",
            )
        if mode not in ("train", "eval"):
            raise VerifierFailure(
                "verifier_configuration_error", "the mode must be train or eval"
            )
        verify(mode, Path(payload), Path(dataset), result_path, Path(evidence))
        return 0
    except VerifierFailure as failure:
        write_json(
            result_path,
            {
                "status": "failed",
                "summary": failure.message[:MAX_MESSAGE_CHARS],
                "metrics": {},
                "failure": {
                    "code": failure.code,
                    "message": failure.message[:MAX_MESSAGE_CHARS],
                    "retryable": False,
                },
            },
        )
        print(f"{failure.code}: {failure.message}", file=sys.stderr)
        return 1
    except Exception as error:  # noqa: BLE001 - never exit without a result file
        write_json(
            result_path,
            {
                "status": "failed",
                "summary": f"the verifier adapter raised {type(error).__name__}",
                "metrics": {},
                "failure": {
                    "code": "verifier_internal_error",
                    "message": f"{type(error).__name__}",
                    "retryable": False,
                },
            },
        )
        traceback.print_exc()
        return 1


if __name__ == "__main__":
    raise SystemExit(main(*sys.argv[1:]))
