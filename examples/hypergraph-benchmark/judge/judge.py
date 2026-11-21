"""Run the pinned verifier once per selection and retain its native result and evidence."""

import hashlib
import json
import math
import os
import signal
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

from receipts import check_receipt

INTERFACE = Path("/opt/team/interface.json")
DATASETS = Path("/opt/team/data")
OUT = Path("/pumpkin/out")


def digest(path):
    with path.open("rb") as stream:
        return "sha256:" + hashlib.file_digest(stream, "sha256").hexdigest()


def official(mode, archive, out):
    out.mkdir(exist_ok=False, parents=True)
    result = out / "official.json"
    evidence = out / "evidence.tar.zst"
    dataset = DATASETS / (mode + ".json")
    interface = json.loads(INTERFACE.read_bytes())
    names = interface["env"]
    with tempfile.TemporaryDirectory(dir=interface["home"]) as scratch:
        env = os.environ | {
            "HOME": interface["home"],
            names["mode"]: mode,
            names["scratch"]: scratch,
            names["payload"]: str(archive),
            names["dataset"]: str(dataset),
            names["result"]: str(result),
            names["evidence"]: str(evidence),
        }
        with subprocess.Popen(
            [interface["command"]], cwd=scratch, env=env, start_new_session=True
        ) as process:
            try:
                code = process.wait(timeout=1810)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
                raise
    raw = json.loads(result.read_bytes())
    if code and raw.get("failure", {}).get("code") != "invalid_submission":
        raise RuntimeError(f"verifier exited {code}: {raw.get('failure')}")
    return raw


def verdict(raw):
    if raw.get("status") == "failed":
        code = raw["failure"]["code"]
        return {
            "status": "invalid" if code == "invalid_submission" else "error",
            "score": None,
            "reason": code,
        }
    score = raw["raw_score"]
    if (
        raw["status"] != "passed"
        or type(score) not in (int, float)
        or not math.isfinite(score)
        or not 0 <= score <= 100
    ):
        raise ValueError("invalid official result or missing score")
    reason = raw["metrics"].get("rejected")
    if raw["summary"].startswith("submission_invalid code="):
        reason = raw["summary"].removeprefix("submission_invalid code=")
    if reason:
        return {"status": "invalid", "score": score, "reason": reason}
    return {"status": "scored", "score": score}


def evidence_metrics(out, checksum):
    import zstandard  # Provided by the pinned verifier image.

    evidence = out / "evidence.tar.zst"
    try:
        with (
            evidence.open("rb") as compressed,
            zstandard.ZstdDecompressor().stream_reader(compressed) as stream,
            tarfile.open(fileobj=stream, mode="r|") as archive,
        ):
            for member in archive:
                if (
                    member.name == "metrics.json"
                    and member.isfile()
                    and member.size <= 2 * 1024**2
                ):
                    data = json.load(archive.extractfile(member))
                    check_receipt(data, checksum, digest(DATASETS / "eval.json"))
                    metrics = data.get("metrics", {})
                    return {
                        key: metrics[key]
                        for key in (
                            "candidate_runtime_seconds_median",
                            "candidate_sequential_runtime_seconds_median",
                            "reference_score",
                            "beats_reference",
                            "workload_sha256",
                        )
                        if key in metrics
                    }
    except zstandard.ZstdError as error:
        raise ValueError("invalid native evidence compression") from error
    raise ValueError("native evaluation evidence lacks metrics.json")


def judge(role, path, out):
    row = {"role": role, "selection": None}
    if path is None:
        return row | {"status": "missing", "score": None}
    try:
        archive = Path(path)
        checksum = digest(archive)
        row["selection"] = {
            "path": str(archive),
            "bytes": archive.stat().st_size,
            "digest": checksum,
        }
        raw = official("eval", archive, out / role)
        row.update(verdict(raw))
        if raw.get("status") == "passed":
            row.update(evidence_metrics(out / role, checksum))
        row["official"] = str((out / role / "official.json").relative_to(out.parent))
        evidence = out / role / "evidence.tar.zst"
        if evidence.exists():
            row["evidence"] = str(evidence.relative_to(out.parent))
        return row
    except (
        OSError,
        RuntimeError,
        ValueError,
        KeyError,
        TypeError,
        ImportError,
        EOFError,
        subprocess.SubprocessError,
        tarfile.TarError,
    ) as error:
        return row | {"status": "error", "score": None, "reason": str(error)}


def best(rows):
    complete = all(row["status"] != "error" for row in rows)
    scores = [row["score"] for row in rows if row["score"] is not None]
    score = max(scores) if complete and scores else None
    return {
        "score": score,
        "complete": complete,
        "winners": [
            r["role"] for r in rows if score is not None and r["score"] == score
        ],
        "roles": rows,
    }


def main(mode, source, destination):
    if mode == "train":
        raw = official("train", Path(source), destination.parent / "train")
        result = {
            k: v
            for k, v in raw.items()
            if k in {"status", "raw_score", "summary", "metrics", "failure"}
        }
    elif mode == "eval":
        paths = json.loads(source)
        result = best(
            [
                judge(role, path, destination.parent / "roles")
                for role, path in paths.items()
            ]
        )
    else:
        raise ValueError(f"unknown mode {mode}")
    destination.write_text(json.dumps(result, allow_nan=False))


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2], Path(sys.argv[3]))
