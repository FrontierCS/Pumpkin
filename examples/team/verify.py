"""Check the selections, the cross-agent message and sandbox cleanup in a team example record."""

import argparse
import hashlib
import json
from pathlib import Path


def read(path):
    return json.loads(path.read_bytes())


def payload(record, ref):
    algorithm, digest = ref["digest"].split(":")
    if (
        algorithm != "sha256"
        or len(digest) != 64
        or any(c not in "0123456789abcdef" for c in digest)
    ):
        raise ValueError("invalid payload digest")
    raw = (record / "payloads" / digest).read_bytes()
    if hashlib.sha256(raw).hexdigest() != digest or len(raw) != ref["size"]:
        raise ValueError("stored payload differs from its record")
    return json.loads(raw)


def verify(record, omit):
    ir, outcome = read(record / "ir.json"), read(record / "outcome.json")
    if ir["name"] != "examples/team" or outcome["state"] != "finished":
        raise ValueError("expected a finished team example")
    members = outcome["members"]
    roles = ["alice", "bob"]
    if [m["role"] for m in members] != roles:
        raise ValueError("expected Alice and Bob in declaration order")
    expected = [{"amber": 4}, {"amber": 4, "blue": 7}]
    if omit in {"bob", "all"}:
        expected[1] = None
    if omit == "all":
        expected[0] = None
    scores = []
    for member, wanted in zip(members, expected):
        submission = member["submission"]
        if wanted is None and member["work_end"] != {
            "kind": "agent_exited",
            "status": {"code": 0},
        }:
            raise ValueError(
                f"{member['role']} failed to exit cleanly without selecting"
            )
        if (submission is None) != (wanted is None):
            raise ValueError(f"unexpected missing selection for {member['role']}")
        if submission and (
            not submission["finalized"]
            or payload(record, submission["payload"]) != wanted
        ):
            raise ValueError(f"unexpected selection for {member['role']}")
        scores.append(len(wanted) if wanted is not None else None)
    result = {
        field["name"]: field["value"]
        for field in outcome["verdict"]["result"]["values"]
    }
    rows = [
        {
            "role": role,
            "status": "missing" if score is None else "scored",
            "score": score,
        }
        for role, score in zip(roles, scores)
    ]
    best = max((s for s in scores if s is not None), default=None)
    if result != {"score": best, "roles": rows}:
        raise ValueError(f"unexpected judge result {result}")

    callers = {f"agent-{i}": r["name"] for i, r in enumerate(ir["roles"])}
    calls, posted = {}, {}
    peer_read = alice_finished = None
    finals = []
    read_ids = set()
    acknowledged = False
    deleted = {}
    with (record / "log.jsonl").open() as log:
        for line in map(json.loads, log):
            event = line.get("event", {})
            for action in line.get("actions", []):
                if action["kind"] == "start_final_call":
                    finals.append(action["job"])
                if action["kind"] == "start_call" and action["grant"] == "board":
                    calls[action["job"]] = (
                        callers[action["caller"]],
                        payload(record, action["payload"]),
                    )
                request = event.get("request", {})
                if (
                    action["kind"] == "reply"
                    and action["reply"].get("returned", {}).get("state") == "accepted"
                    and request.get("grant") == "select_alice"
                ):
                    alice_finished = line["seq"]
            if event.get("kind") == "sandbox_deleted":
                label = event["sandbox"]
                if label in deleted:
                    raise ValueError(f"duplicate deletion for {label}")
                deleted[label] = event["provider"], event["id"]
            if event.get("kind") != "call_returned" or event["job"] not in calls:
                continue
            role, request = calls[event["job"]]
            reply = {f["name"]: f["value"] for f in event["returned"]}
            if role == "alice" and request.get("op") == "post" and "id" in reply:
                posted[reply["id"]] = request["text"]
            if role == "bob" and request.get("op") == "read":
                for msg in reply.get("messages", []):
                    if msg["from"] == "alice" and posted.get(msg["id"]) == msg["text"]:
                        peer_read = line["seq"]
                        read_ids.add(msg["id"])
            if (
                role == "bob"
                and request.get("op") == "send"
                and request.get("to") == "alice"
                and request.get("text")
                in {f"Checked message {ident}" for ident in read_ids}
                and type(reply.get("id")) is int
            ):
                acknowledged = True
    if peer_read is None or not acknowledged or len(finals) != 1:
        raise ValueError(
            "expected Bob to read and acknowledge Alice's message, and one final call"
        )
    (final,) = finals
    paths = {
        "agent-0": "agents/alice",
        "agent-1": "agents/bob",
        "shared-2": "shared/board",
        f"job-{final}": f"jobs/{final}",
    }
    if deleted.keys() != paths.keys() or len(set(deleted.values())) != 4:
        raise ValueError(
            "expected separate deletion events for both agents, the board and the final job"
        )
    for label, path in paths.items():
        receipt = read(record / path / "sandbox.json")
        if (receipt["provider"], receipt["id"]) != deleted[label] or receipt.get(
            "delete", {}
        ).get("state") != "deleted":
            raise ValueError(
                f"unexpected physical sandbox deletion receipt for {label}"
            )
    if alice_finished is not None:
        print(
            f"Alice locked at event {alice_finished}, Bob read her prediction at event {peer_read}"
        )
    print(
        f"{record.name}: individual scores {scores}, best {best}, four sandboxes deleted"
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("record", type=Path)
    parser.add_argument("--omit", choices=["none", "bob", "all"], default="none")
    args = parser.parse_args()
    try:
        verify(args.record, args.omit)
    except (ValueError, KeyError, OSError) as error:
        parser.exit(1, f"{args.record}: {error}\n")
