"""Check the selections, the cross-agent message and sandbox cleanup in a team example record."""

import argparse
import hashlib
import json
from pathlib import Path

ROLES = ["alice", "bob"]
ANSWERS = {"amber": 4, "blue": 7}


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
    return raw


def grade(raw):
    try:
        answer = json.loads(raw)
    except (ValueError, RecursionError):
        answer = None
    if (
        not isinstance(answer, dict)
        or not answer.keys() <= ANSWERS.keys()
        or any(type(value) is not int for value in answer.values())
    ):
        return "invalid", 0
    return "scored", sum(answer.get(name) == value for name, value in ANSWERS.items())


def check_scores(record, members, verdict, selected, codex, omit):
    if [member["role"] for member in members] != ROLES:
        raise ValueError("expected Alice and Bob in declaration order")
    rows = []
    for member in members:
        role, submission = member["role"], member["submission"]
        missing = omit in {role, "all"}
        if missing:
            if (
                submission
                or role in selected
                or member["work_end"]
                != {
                    "kind": "agent_exited",
                    "status": {"code": 0},
                }
            ):
                raise ValueError(f"{role} did not exit cleanly without selecting")
            status, score = "missing", None
        else:
            if (
                not submission
                or not submission["finalized"]
                or selected.get(role) != submission["payload"]
            ):
                raise ValueError(
                    f"{role} did not finalize its own selection ({member['work_end']})"
                )
            raw = payload(record, submission["payload"])
            status, score = grade(raw)
            if not codex:
                wanted = {"amber": 4} if role == "alice" else ANSWERS
                if json.loads(raw) != wanted or status != "scored":
                    raise ValueError(f"unexpected reference selection for {role}")
        rows.append({"role": role, "status": status, "score": score})
    scores = [row["score"] for row in rows]
    best = max((s for s in scores if s is not None), default=None)
    if verdict["kind"] != "returned":
        raise ValueError(f"judge ended with {verdict['kind']}")
    result = {f["name"]: f["value"] for f in verdict["result"]["values"]}
    if result != {"score": best, "roles": rows}:
        raise ValueError(f"judge result disagrees with selected answers: {result}")
    print(
        f"{record.name}: individual scores {scores}, best {best}, four sandboxes deleted"
    )


def verify(record, omit):
    ir, outcome = read(record / "ir.json"), read(record / "outcome.json")
    if ir["name"] not in {"examples/team", "examples/team/codex"}:
        raise ValueError("expected the team example")
    codex = ir["name"] == "examples/team/codex"
    if codex and omit != "none":
        raise ValueError("--omit applies to the scripted reference")
    callers = {f"agent-{i}": r["name"] for i, r in enumerate(ir["roles"])}
    calls, posted = {}, {}
    peer_read = alice_finished = None
    finals = []
    read_ids = set()
    acknowledged = False
    deleted, selected = {}, {}
    responses = dict.fromkeys(ROLES, 0)
    last_model = {}
    with (record / "log.jsonl").open() as log:
        for line in map(json.loads, log):
            event = line.get("event", {})
            if event.get("kind") == "model_answered":
                role = callers[event["caller"]]
                last_model[role] = (
                    event["answer"]["kind"],
                    event.get("upstream_status"),
                )
                if (
                    event["answer"]["kind"] == "forwarded"
                    and event.get("api") == "responses"
                    and 200 <= event["upstream_status"] < 300
                    and "failed" not in event
                    and event.get("usage", {}).get("output_tokens", 0) > 0
                ):
                    responses[role] += 1
            for action in line.get("actions", []):
                if action["kind"] == "start_final_call":
                    finals.append(action["job"])
                if action["kind"] == "start_call" and action["grant"] == "board":
                    calls[action["job"]] = (
                        callers[action["caller"]],
                        action["payload"],
                    )
                request = event.get("request", {})
                if (
                    action["kind"] == "reply"
                    and action["reply"].get("returned", {}).get("state") == "accepted"
                ):
                    role = callers[event["caller"]]
                    if request.get("grant") != "select_" + role:
                        raise ValueError(f"{role} finalized with the wrong grant")
                    selected[role] = request["payload"]
                    if role == "alice":
                        alice_finished = line["seq"]
            if event.get("kind") == "sandbox_deleted":
                label = event["sandbox"]
                if label in deleted:
                    raise ValueError(f"duplicate deletion for {label}")
                deleted[label] = event["provider"], event["id"]
            if event.get("kind") != "call_returned" or event["job"] not in calls:
                continue
            reply = {f["name"]: f["value"] for f in event["returned"]}
            if "error" in reply:
                continue
            role, ref = calls[event["job"]]
            request = json.loads(payload(record, ref))
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
    if codex:
        for role, count in responses.items():
            if not count:
                raise ValueError(
                    f"{role}: no successful Responses output (last model reply {last_model.get(role)})"
                )
        print(f"Successful model responses: {responses}")
    if outcome["state"] != "finished":
        raise ValueError(f"run {outcome['state']}")
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
    check_scores(record, outcome["members"], outcome["verdict"], selected, codex, omit)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("record", type=Path)
    parser.add_argument("--omit", choices=["none", "bob", "all"], default="none")
    args = parser.parse_args()
    try:
        verify(args.record, args.omit)
    except (ValueError, KeyError, OSError) as error:
        parser.exit(1, f"{args.record}: {error}\n")
