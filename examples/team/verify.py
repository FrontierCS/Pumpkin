"""Check the selections, the cross-agent message and sandbox cleanup in a team example record."""

import argparse
import hashlib
import json
import re
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
    return raw


def final_grade(record, lease, digest):
    logs = record / "leases" / str(lease) / "log"
    checksum = (logs / "1" / "command.stdout").read_bytes()
    if checksum != (digest.removeprefix("sha256:") + "  src/solution.cpp\n").encode():
        raise ValueError(f"compiler lease {lease} received different C++ source")
    # The duplicate triple and the single-agent example’s public 12-vertex graph.
    vectors = [[2, 2, 2, 0], [3, 3, 3, 3, 2, 2, 1, 1, 1, 1, 1, 0]]
    points = 0
    for command, expected in enumerate(vectors, start=2):
        path = logs / str(command) / "command.stdout"
        if not path.is_file():
            return "invalid", 0
        tokens = path.read_bytes().split()
        if len(tokens) != len(expected) or any(
            not re.fullmatch(rb"[+-]?[0-9]+", t) for t in tokens
        ):
            return "invalid", 0
        points += list(map(int, tokens)) == expected
    return ("scored" if points == len(vectors) else "wrong"), points


def check_scores(record, agents, result, selected, final_leases, codex, omit):
    roles = ["alice", "bob"]
    if [m["role"] for m in agents] != roles:
        raise ValueError("expected Alice and Bob in declaration order")
    reference = Path(__file__).parent / "solver"
    if len(final_leases) != len(selected):
        raise ValueError("expected a fresh final compiler for each selected source")
    compilers = iter(final_leases)
    rows = []
    for member, part, reference_grade in zip(
        agents, ["degrees.cpp", "peel.cpp"], [("wrong", 1), ("scored", 2)]
    ):
        role, submission = member["role"], member["submission"]
        if omit in {role, "all"}:
            if (
                role in selected
                or submission is not None
                or member["work_end"]
                != {
                    "kind": "agent_exited",
                    "status": {"code": 0},
                }
            ):
                raise ValueError(f"{role} did not exit cleanly without selecting")
            digest, status, points = None, "missing", None
        else:
            if (
                not submission
                or not submission["finalized"]
                or selected.get(role) != submission["payload"]
            ):
                raise ValueError(
                    f"{role} did not finalize its C++ source ({member['work_end']})"
                )
            source = payload(record, submission["payload"])
            digest = submission["payload"]["digest"]
            status, points = final_grade(record, next(compilers), digest)
            if status == "invalid":
                raise ValueError(
                    f"{role} selected invalid C++ or produced malformed output"
                )
            if not codex:
                expected = (reference / "main.cpp").read_bytes() + (
                    reference / part
                ).read_bytes()
                if source != expected:
                    raise ValueError(f"unexpected reference selection for {role}")
                if (status, points) != reference_grade:
                    raise ValueError(f"unexpected reference grade for {role}")
        rows.append({"role": role, "digest": digest, "status": status, "score": points})
    best = max((r["score"] for r in rows if r["score"] is not None), default=None)
    if result != {"score": best, "roles": rows}:
        raise ValueError(f"unexpected judge result {result}")
    for row in rows:
        print(f"{row['role']}: {row['status']}, score {row['score']}")
    return best


def message_data(text):
    try:
        value = json.loads(text)
    except ValueError:
        return {}
    return value if isinstance(value, dict) else {}


def check_graph(graph):
    n, edges, cores = (graph.get(k) for k in ("vertices", "edges", "cores"))
    if type(n) is not int or n <= 0 or not isinstance(edges, list):
        return False
    if (
        not isinstance(cores, list)
        or len(cores) != n
        or any(type(c) is not int for c in cores)
    ):
        return False
    return all(
        isinstance(edge, list)
        and edge
        and all(type(v) is int and 0 <= v < n for v in edge)
        and len(set(edge)) == len(edge)
        for edge in edges
    )


def verify(record, omit):
    ir, outcome = read(record / "ir.json"), read(record / "outcome.json")
    if (
        ir["name"] not in {"examples/team", "examples/team/codex"}
        or outcome["state"] != "finished"
    ):
        raise ValueError(
            f"expected a finished team example, got {ir['name']} ({outcome['state']})"
        )
    codex = ir["name"] == "examples/team/codex"
    if codex and omit != "none":
        raise ValueError("--omit applies to the scripted reference")
    verdict = outcome["verdict"]
    if verdict["kind"] != "returned":
        raise ValueError(f"judge ended with {verdict['kind']}")
    result = {f["name"]: f["value"] for f in verdict["result"]["values"]}
    callers = {f"agent-{i}": r["name"] for i, r in enumerate(ir["roles"])}
    calls, posted = {}, {}
    peer_read = alice_finished = None
    finals = []
    read_ids = {}
    deleted, selected = {}, {}
    eval_jobs, evaluated, final_leases = {}, set(), []
    refusals = set()
    responses = {"alice": 0, "bob": 0}

    with (record / "log.jsonl").open() as log:
        for line in map(json.loads, log):
            event = line.get("event", {})
            if (
                event.get("kind") == "model_answered"
                and event["answer"]["kind"] == "forwarded"
                and event.get("api") == "responses"
                and 200 <= event.get("upstream_status", 0) < 300
                and "failed" not in event
                and event.get("usage", {}).get("output_tokens", 0) > 0
            ):
                responses[callers[event["caller"]]] += 1
            request = event["request"] if event.get("kind") == "op_arrived" else {}
            grant = request.get("grant")
            args = request.get("arguments", {})
            for action in line.get("actions", []):
                if action["kind"] == "start_final_call":
                    finals.append(action["job"])
                    entries = [
                        {"role": role, "selection": selected.get(role)}
                        for role in responses
                    ]
                    if action["selections"] != entries:
                        raise ValueError(
                            "final call received different sources than the agents finalized"
                        )
                if (
                    action["kind"] == "start_lease"
                    and finals
                    and action["holder"] == f"job-{finals[0]}"
                ):
                    final_leases.append(action["lease"])
                if action["kind"] == "start_call":
                    role = callers[action["caller"]]
                    if (
                        action["grant"] != grant
                        or event["caller"] != action["caller"]
                        or request["payload"] != action["payload"]
                    ):
                        raise ValueError(
                            "job differs from the authenticated caller's request"
                        )
                    if grant == "eval":
                        eval_jobs[action["job"]] = role
                    else:
                        calls[action["job"]] = (
                            role,
                            grant,
                            args,
                            payload(record, action["payload"]),
                        )
                if action["kind"] == "reply" and action["reply"]["kind"] == "refused":
                    message = action["reply"]["message"]
                    if (
                        grant == "post"
                        and args.get("from") == "bob"
                        and "`--arg from`" in message
                    ):
                        refusals.add((callers[event["caller"]], "post"))
                    if grant == "submit" and "`--arg role`" in message:
                        refusals.add((callers[event["caller"]], "submit"))
                if (
                    action["kind"] == "reply"
                    and action["reply"].get("returned", {}).get("state") == "accepted"
                ):
                    role = callers[event["caller"]]
                    if grant != "submit":
                        raise ValueError(f"{role} finalized with the wrong grant")
                    selected[role] = request["payload"]
                    if role == "alice":
                        alice_finished = line["seq"]
            if event.get("kind") == "sandbox_deleted":
                label = event["sandbox"]
                if label in deleted:
                    raise ValueError(f"duplicate deletion for {label}")
                deleted[label] = event["provider"], event["id"]
            if event.get("kind") != "call_returned":
                continue
            reply = {f["name"]: f["value"] for f in event["returned"]}
            if event["job"] in eval_jobs:
                evaluated.add(eval_jobs[event["job"]])
            if event["job"] not in calls:
                continue
            role, grant, args, raw = calls[event["job"]]
            if (
                role == "alice"
                and grant == "post"
                and not args
                and type(reply.get("id")) is int
            ):
                posted[reply["id"]] = raw.decode("utf-8")
            if (
                role == "bob"
                and grant == "read"
                and args.get("stream", "board") == "board"
            ):
                for msg in reply.get("messages", []):
                    if (
                        msg["from"] == "alice"
                        and msg["id"] > int(args.get("after", 0))
                        and posted.get(msg["id"]) == msg["text"]
                    ):
                        graph = message_data(msg["text"])
                        if not check_graph(graph):
                            continue
                        if not codex and graph != {
                            "vertices": 4,
                            "edges": [[0, 1, 2], [0, 1, 2]],
                            "cores": [2, 2, 2, 0],
                        }:
                            raise ValueError(
                                "Alice posted an unexpected graph or answer"
                            )
                        read_ids[msg["id"]] = line["seq"], graph["cores"]
            if (
                role == "bob"
                and grant == "send"
                and args == {"to": "alice"}
                and type(reply.get("id")) is int
            ):
                ack = message_data(raw)
                ident, observed = ack.get("checked"), ack.get("cores")
                if (
                    type(ident) is int
                    and ident in read_ids
                    and isinstance(observed, list)
                ):
                    seq, expected = read_ids[ident]
                    if (
                        len(observed) == len(expected)
                        and all(type(c) is int for c in observed)
                        and (codex or observed == expected)
                    ):
                        peer_read = seq
                        if codex:
                            print(
                                f"Message {ident}: Alice posted {expected}, Bob observed {observed}"
                            )
    if peer_read is None or len(finals) != 1:
        raise ValueError(
            "expected Bob to read and acknowledge Alice's message, and one final call"
        )
    if codex:
        if not all(responses.values()):
            raise ValueError(
                f"expected successful Responses output from both members: {responses}"
            )
        if evaluated != responses.keys():
            raise ValueError("expected a draft evaluation from each agent")
        print(f"Successful model responses: {responses}")
    elif refusals != {("alice", "post"), ("alice", "submit"), ("bob", "submit")}:
        raise ValueError(
            "expected refusals of Alice's forged post and both submission role overrides"
        )
    best = check_scores(
        record, outcome["agents"], result, selected, final_leases, codex, omit
    )
    (final,) = finals
    paths = {
        "agent-0": "agents/alice",
        "agent-1": "agents/bob",
        "shared-2": "shared/board",
        f"job-{final}": f"jobs/{final}",
    }
    for job in eval_jobs:
        paths[f"job-{job}"] = f"jobs/{job}"
    for index in range(len(eval_jobs) + len(selected)):
        paths[f"lease-{index}"] = f"leases/{index}"
    if deleted.keys() != paths.keys() or len(set(deleted.values())) != len(paths):
        raise ValueError(
            "expected separate deletion events for both agents, the board, the final job and every runner"
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
            f"Alice locked at event {alice_finished}, Bob read her graph at event {peer_read}"
        )
    print(f"{record.name}: best {best}, {len(paths)} sandboxes deleted")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("record", type=Path)
    parser.add_argument("--omit", choices=["none", "bob", "all"], default="none")
    args = parser.parse_args()
    try:
        verify(args.record, args.omit)
    except (ValueError, KeyError, OSError) as error:
        parser.exit(1, f"{args.record}: {error}\n")
