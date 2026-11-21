"""Extract exactly the five submitted archives from a complete run without executing any archive."""

import hashlib
import json
import re
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent
ROLES = ["alice", "bob", "carol", "dan", "eve"]


def main(record):
    dest = ROOT.parents[1] / "benchmark-replay"
    outcome = json.loads((record / "outcome.json").read_bytes())
    values = {r["name"]: r["value"] for r in outcome["verdict"]["result"]["values"]}
    if outcome["state"] != "finished" or not values["complete"]:
        raise ValueError("original run lacks a completed final")
    if [r["role"] for r in values["roles"]] != ROLES or [
        r["role"] for r in outcome["agents"]
    ] != ROLES:
        raise ValueError("agent role names differ")
    manifest = []
    archives = {}
    for row, member in zip(values["roles"], outcome["agents"]):
        name = row["role"]
        selected = member["submission"]
        item = {
            "role": name,
            "status": row["status"],
            "original_score": row["score"],
            "sha256": None,
        }
        if selected is None:
            if row["selection"] is not None or row["status"] != "missing":
                raise ValueError(
                    f"{name}: missing selection differs from the original verdict"
                )
        else:
            ref = selected["payload"]
            checksum = row["selection"]["digest"]
            if (
                not re.fullmatch(r"sha256:[0-9a-f]{64}", checksum)
                or checksum != ref["digest"]
            ):
                raise ValueError(f"{name}: invalid archive digest")
            with (record / "payloads" / checksum.removeprefix("sha256:")).open(
                "rb"
            ) as stream:
                data = stream.read(256 * 1024 + 1)
            if (
                len(data) > 256 * 1024
                or checksum != "sha256:" + hashlib.sha256(data).hexdigest()
                or len(data) != ref["size"]
                or len(data) != row["selection"]["bytes"]
            ):
                raise ValueError(f"{name}: selected bytes disagree")
            archives[name] = data
            item["sha256"] = checksum
        manifest.append(item)
    archives["manifest.json"] = (
        json.dumps({"original_run": record.name, "entries": manifest}, indent=2) + "\n"
    ).encode()
    if dest.exists():
        if (
            not dest.is_symlink()
            and dest.is_dir()
            and {p.name for p in dest.iterdir()} == archives.keys()
            and all(
                not (dest / name).is_symlink()
                and (dest / name).is_file()
                and (dest / name).stat().st_size == len(data)
                and (dest / name).read_bytes() == data
                for name, data in archives.items()
            )
        ):
            return
        raise ValueError(
            f"{dest} already exists with different selections; use a fresh task checkout"
        )
    with tempfile.TemporaryDirectory(dir=dest.parent) as temp:
        staged = Path(temp) / "selections"
        staged.mkdir()
        for name, data in archives.items():
            (staged / name).write_bytes(data)
        staged.rename(dest)


if __name__ == "__main__":
    main(Path(sys.argv[1]).resolve())
