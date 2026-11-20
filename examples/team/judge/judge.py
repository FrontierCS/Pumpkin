"""Score predictions of (row · [2, 3] + error) modulo 13 for two public rows."""

import hashlib
import json
import sys
from pathlib import Path

ROWS = {"amber": ([3, 4], -1), "blue": ([2, 5], 1)}


def score(rows):
    results = []
    for entry in rows:
        role, selection = entry["role"], entry["selection"]
        if selection is None:
            results.append({"role": role, "status": "missing", "score": None})
            continue
        raw = Path(selection["path"]).read_bytes()
        if (
            len(raw) != selection["bytes"]
            or "sha256:" + hashlib.sha256(raw).hexdigest() != selection["digest"]
        ):
            raise ValueError(f"staged selection does not match the manifest for {role}")
        try:
            claims = json.loads(raw)
            if not isinstance(claims, dict) or not claims.keys() <= ROWS.keys():
                raise ValueError("expected a mapping from row names to predictions")
            if any(type(value) is not int for value in claims.values()):
                raise ValueError("predictions must be integers")
            points = sum(
                claims.get(name) == (2 * row[0] + 3 * row[1] + error) % 13
                for name, (row, error) in ROWS.items()
            )
            results.append({"role": role, "status": "scored", "score": points})
        except (ValueError, RecursionError):
            results.append({"role": role, "status": "invalid", "score": 0})
    return {
        "score": max(
            (r["score"] for r in results if r["score"] is not None), default=None
        ),
        "roles": results,
    }


if __name__ == "__main__":
    manifest, result = map(Path, sys.argv[1:])
    result.write_text(json.dumps(score(json.loads(manifest.read_bytes()))))
