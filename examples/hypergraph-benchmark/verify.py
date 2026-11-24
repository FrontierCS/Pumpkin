"""Check that a run of the benchmark worked, and say what each role scored."""

import argparse
import collections
import hashlib
import io
import json
import math
import subprocess
import sys
import tarfile
from pathlib import Path

# So that importing the judge's module writes no bytecode into the judge image's context.
sys.dont_write_bytecode = True
from judge.receipts import check_receipt, check_timing  # noqa: E402

ROOT = Path(__file__).resolve().parent
SELECTOR = ROOT / "judge/eval.json"
ROLES = ["alice", "bob", "carol", "dan", "eve"]


def sha(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def read(path):
    return json.loads(path.read_bytes())


def require(condition, message):
    if not condition:
        raise ValueError(message)


def values(entries):
    return {e["name"]: e["value"] for e in entries}


def evidence(path):
    raw = subprocess.check_output(["zstd", "--decompress", "--stdout", str(path)])
    with tarfile.open(fileobj=io.BytesIO(raw), mode="r:") as archive:
        return json.load(archive.extractfile("metrics.json"))


def check_native(row, out):
    name = row["role"]
    native = read(out / row["official"])
    if row["score"] is None:
        require(
            row["status"] == "invalid"
            and native.get("failure", {}).get("code") == "invalid_submission",
            f"{name} invalid archive",
        )
        return
    reason = native["metrics"].get("rejected")
    if native["summary"].startswith("submission_invalid code="):
        reason = native["summary"].removeprefix("submission_invalid code=")
    require(
        native["status"] == "passed"
        and row["status"] == ("invalid" if reason else "scored"),
        f"{name} native verdict",
    )
    receipt = evidence(out / row["evidence"])
    check_receipt(receipt, row["selection"]["digest"], sha(SELECTOR.read_bytes()))
    require(native["raw_score"] == row["score"], f"{name} native score")
    median = check_timing(receipt, row["score"])
    if median is not None:
        require(
            math.isclose(median, row["candidate_runtime_seconds_median"]),
            "recorded median",
        )


def check_run(path):
    outcome = read(path / "outcome.json")
    require(outcome["state"] == "finished", "run did not finish")
    result = values(outcome["verdict"]["result"]["values"])
    (output,) = [
        p.parent for p in path.glob("jobs/*/out/result.json") if read(p) == result
    ]
    rows = result["roles"]
    require(result["complete"] and [r["role"] for r in rows] == ROLES, "team result")
    require([m["role"] for m in outcome["agents"]] == ROLES, "member identities")
    for member, row in zip(outcome["agents"], rows):
        ref = member["submission"]
        if ref is None:
            require(
                row["selection"] is None
                and row["status"] == "missing"
                and row["score"] is None,
                "missing role",
            )
            continue
        selected = ref["payload"]
        payload = (
            path / "payloads" / selected["digest"].removeprefix("sha256:")
        ).read_bytes()
        require(
            sha(payload) == selected["digest"] == row["selection"]["digest"],
            "driver archive digest",
        )
        require(
            len(payload) == selected["size"] == row["selection"]["bytes"],
            "driver archive size",
        )
        check_native(row, output)
    best = max((r["score"] for r in rows if r["score"] is not None), default=None)
    require(
        result["score"] == best
        and result["winners"]
        == [r["role"] for r in rows if best is not None and r["score"] == best],
        "best individual score",
    )
    ir = read(path / "ir.json")
    require(
        ir["name"] == "examples/hypergraph-benchmark"
        and ir["deadline_seconds"] == 5400,
        "team workflow clock",
    )
    responses, counts = collections.Counter(), collections.Counter()
    sessions = {role: set() for role in ROLES}
    sandbox_ids = set()
    disk = {}
    for r in ir["roles"]:
        if "agent" not in r["runtime"]:
            continue
        role, runtime = r["name"], r["runtime"]["agent"]
        require(
            r["needs"]["cpu_millis"] == 8000
            and r["needs"]["memory_bytes"] == 32 * 1024**3,
            "participant CPU and RAM",
        )
        require(
            any(a.endswith("/bin/codex") for a in runtime["command"]["argv"]),
            "real Codex command",
        )
        (route,) = runtime["egress"]["model"]
        require(route["api"] == "responses", "role model route")
        grants = {g["name"]: g for g in runtime["grants"]}
        require(
            runtime["prepend_grant_help"]
            and set(grants) == {"train", "select", "finish", "post", "send", "read"}
            and all(
                grants[g]["job"]["target"] == "board"
                and grants[g]["job"]["calls"] == limit
                for g, limit in (("post", 50), ("send", 50), ("read", 100))
            )
            and grants["train"]["job"]["target"] == "train_judge",
            "role grants",
        )
        disk[role] = read(path / "agents" / role / "out/workspace.json")[
            "available_bytes"
        ]
        require(disk[role] >= 2 * 1024**3, "native workspace capacity")
        receipt = read(path / "agents" / role / "sandbox.json")
        sandbox_ids.add((receipt["provider"], receipt["id"]))
    require(len(sandbox_ids) == 5, "five private sandboxes")
    trained, board_calls, jobs, posts, peer_reads = set(), {}, {}, {}, set()
    agents = {
        f"agent-{i}": r["name"]
        for i, r in enumerate(ir["roles"])
        if "agent" in r["runtime"]
    }
    counts_by_role = {role: collections.Counter() for role in ROLES}
    saved = dict.fromkeys(ROLES)
    final_calls = []
    for line in map(json.loads, (path / "log.jsonl").open()):
        event = line.get("event", {})
        actions = line.get("actions", [])
        request = event["request"] if event.get("kind") == "op_arrived" else {}
        effects = request.get("evaluated", {}).get("effects", [])
        if (
            request.get("kind") == "now"
            and "set_submission" in effects
            and any(
                a["kind"] == "reply" and a["reply"]["kind"] == "call" for a in actions
            )
        ):
            saved[agents[event["caller"]]] = request["payload"]
        for a in actions:
            if a["kind"] == "start_final_call":
                final_calls.append(a["selections"])
            if a["kind"] != "start_call" or a["grant"] not in (
                "train",
                "post",
                "send",
                "read",
            ):
                continue
            role = agents[a["caller"]]
            jobs[a["job"]] = (role, a["grant"])
            if a["grant"] != "train":
                counts_by_role[role][a["grant"]] += 1
                board_calls[a["job"]] = {
                    "op": a["grant"],
                    "args": request.get("arguments", {}),
                    "payload": a["payload"],
                }
        if event.get("kind") == "target_run" and event["job"] in board_calls:
            call = board_calls[event["job"]]
            role, op = jobs[event["job"]]
            args = call["args"]
            expected = ["python3", "/opt/team/board.py", op, role]
            if op == "read":
                expected.extend(
                    [args.get("stream", "board"), str(args.get("after", 0))]
                )
            elif op == "send":
                expected.append(args["to"])
            require(
                event["argv"] == expected and event["stdin"] == call["payload"],
                "authenticated board invocation",
            )
            call["command"] = event["command"]
        if (
            event.get("kind") == "model_answered"
            and not event.get("failed")
            and event["answer"]["kind"] == "forwarded"
            and event.get("upstream_status") == 200
        ):
            role = agents[event["caller"]]
            usage = event.get("usage", {})
            counts[role] += usage.get("output_tokens", 0)
            responses[role] += 1
            request_path = path / "model" / f"{event['request']}.request"
            if request_path.exists():
                key = read(request_path).get("prompt_cache_key")
                if key:
                    sessions[role].add(key)
        if event.get("kind") != "call_returned" or event["job"] not in jobs:
            continue
        role, grant = jobs[event["job"]]
        reply = values(event["returned"])
        if grant == "train":
            if reply.get("status") == "passed":
                trained.add(role)
            continue
        call = board_calls[event["job"]]
        stdout = (
            path
            / "jobs"
            / str(event["job"])
            / "log"
            / str(call["command"])
            / "command.stdout"
        )
        require(read(stdout) == reply, "worker stdout and board result")
        if call["op"] in ("post", "send") and "id" in reply:
            ref = call["payload"]
            raw = (
                path / "payloads" / ref["digest"].removeprefix("sha256:")
            ).read_bytes()
            require(
                sha(raw) == ref["digest"] and len(raw) == ref["size"],
                "board input bytes",
            )
            posts[reply["id"]] = (role, raw.decode("utf-8"), call["args"].get("to"))
        elif call["op"] == "read":
            for m in reply.get("messages", []):
                if (
                    posts.get(m["id"]) == (m["from"], m["text"], m.get("to"))
                    and m["from"] != role
                    and m.get("to") in (None, role)
                ):
                    peer_reads.add((m["id"], role))
    expected = [
        {
            "role": member["role"],
            "selection": member["submission"]["payload"]
            if member["submission"]
            else None,
        }
        for member in outcome["agents"]
    ]
    require(final_calls == [expected], "role-keyed inputs to the single final call")
    require(
        all(saved[e["role"]] == e["selection"] for e in expected),
        "accepted set_submission effects",
    )
    receipts = [read(r) for r in path.rglob("sandbox.json")]
    deleted = len(
        {
            (r["provider"], r["id"])
            for r in receipts
            if r.get("delete", {}).get("state") == "deleted"
        }
    )
    require(
        deleted == sum(b["count"] for b in outcome["usage"]["sandboxes"]),
        "all sandbox deletion receipts",
    )
    return {
        "run": path.name,
        "all_five_models_answered": all(counts[r] > 0 for r in ROLES),
        "distinct_model_sessions": len(set.union(*sessions.values())),
        "successful_responses": dict(responses),
        "worker_usd": sum(r.get("usd", 0) for r in outcome["usage"]["sandboxes"]),
        "total_metered_usd": outcome["usage"].get("usd"),
        "sandboxes_deleted": deleted,
        "workspace_free_bytes": disk,
        "best": best,
        "winners": result["winners"],
        "roles": [
            {
                k: r.get(k)
                for k in ("role", "score", "status", "candidate_runtime_seconds_median")
            }
            for r in rows
        ],
        "output_tokens": dict(counts),
        "roles_with_native_train": sorted(trained),
        "board_calls_by_role": counts_by_role,
        "authenticated_peer_reads": len(peer_reads),
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("record", type=Path)
    print(json.dumps(check_run(parser.parse_args().record), indent=2))
