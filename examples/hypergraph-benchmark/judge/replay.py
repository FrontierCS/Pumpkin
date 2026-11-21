"""Invoke the pinned native verifier independently and recompute each timed replay score."""

import hashlib
import json
import tarfile
from pathlib import Path

import zstandard
from judge import DATASETS, official, verdict
from receipts import check_receipt, check_timing

HOME = Path("/opt/replay")
OUT = Path("/pumpkin/out")
SELECTOR = DATASETS / "eval.json"


def digest(path):
    return "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()


def evaluate(row):
    role = row["role"]
    if row["sha256"] is None:
        return {"role": role, "status": "missing", "score": None}
    archive = HOME / "input" / role
    if digest(archive) != row["sha256"]:
        raise ValueError(f"{role}: replay archive differs")
    target = OUT / role
    native = official("eval", archive, target)
    decision = verdict(native)
    if native["status"] == "failed":
        if decision["status"] == "invalid":
            return {"role": role, **decision}
        raise ValueError(f"{role}: official evaluation failed")
    receipt = target / "evidence.tar.zst"
    with (
        receipt.open("rb") as source,
        zstandard.ZstdDecompressor().stream_reader(source) as decompressed,
        tarfile.open(fileobj=decompressed, mode="r|") as tar,
    ):
        for item in tar:
            if item.name == "metrics.json":
                observation = json.load(tar.extractfile(item))
                break
        else:
            raise ValueError("native evidence lacks metrics")
    check_receipt(observation, digest(archive), digest(SELECTOR))
    score = native["raw_score"]
    median = check_timing(observation, score)
    return {
        "role": role,
        "status": decision["status"],
        "score": score,
        "median_seconds": median,
        "archive": row["sha256"],
    }


def main():
    manifest = json.loads((HOME / "input/manifest.json").read_bytes())
    rows = []
    for original in manifest["entries"]:
        row = evaluate(original)
        row["original_status"] = original["status"]
        row["original_score"] = original["original_score"]
        rows.append(row)
    best = max((r["score"] for r in rows if r["score"] is not None), default=None)
    result = {
        "original_run": manifest["original_run"],
        "roles": rows,
        "score": best,
        "winners": [r["role"] for r in rows if best is not None and r["score"] == best],
        "outcomes_reproduced": all(
            r["status"] == r["original_status"]
            and bool(r["score"]) == bool(r["original_score"])
            for r in rows
        ),
    }
    (OUT / "result.json").write_text(json.dumps(result, allow_nan=False))


if __name__ == "__main__":
    main()
