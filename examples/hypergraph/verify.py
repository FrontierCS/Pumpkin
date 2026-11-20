"""Check a finished hypergraph run through the files in its record."""

import argparse
import hashlib
import json
import re
from pathlib import Path

CORES = [3, 3, 3, 3, 2, 2, 1, 1, 1, 1, 1, 0]


def vector(record, payload):
    match = re.fullmatch(r"sha256:([0-9a-f]{64})", payload["digest"])
    if not match:
        raise ValueError("invalid payload digest")
    data = (record / "payloads" / match[1]).read_bytes()
    if hashlib.sha256(data).hexdigest() != match[1] or len(data) != payload["size"]:
        raise ValueError("stored payload differs from its record")
    try:
        answer = json.loads(data)
    except (UnicodeError, json.JSONDecodeError) as error:
        raise ValueError("malformed submission: expected a JSON vector") from error
    if not (
        isinstance(answer, list)
        and len(answer) == len(CORES)
        and all(type(value) is int for value in answer)
    ):
        raise ValueError(f"malformed submission: expected {len(CORES)} JSON integers")
    return answer


def model_replied(event):
    return (
        event["kind"] == "model_answered"
        and event["caller"] == "agent"
        and event["answer"]["kind"] == "forwarded"
        and 200 <= event["upstream_status"] < 300
        and "failed" not in event
        and event.get("usage", {}).get("output_tokens", 0) > 0
    )


def verify(record):
    if json.loads((record / "ir.json").read_bytes())["name"] != "examples/hypergraph":
        raise ValueError("expected the hypergraph workflow")
    outcome = json.loads((record / "outcome.json").read_bytes())
    if outcome["state"] != "finished":
        raise ValueError(f"run {outcome['state']}")
    usage = outcome["usage"]
    sandboxes = [record / "agent", *record.glob("jobs/*"), *record.glob("leases/*")]
    if len(sandboxes) != sum(place["count"] for place in usage["sandboxes"]):
        raise ValueError("record is missing a sandbox")
    for sandbox in sandboxes:
        detail = json.loads((sandbox / "sandbox.json").read_bytes())
        if detail.get("delete", {}).get("state") != "deleted":
            raise ValueError(f"{sandbox.relative_to(record)} was not deleted")

    with (record / "log.jsonl").open() as log:
        events = (line["event"] for line in map(json.loads, log) if "event" in line)
        if not any(model_replied(event) for event in events):
            raise ValueError("no successful agent model response with output tokens")

    end = outcome["work_end"]
    if end not in (
        {"kind": "finalized"},
        {"kind": "agent_exited", "status": {"code": 0}},
    ):
        raise ValueError(f"agent work ended with {end}")
    verdict = outcome["verdict"]
    if verdict["kind"] != "returned":
        raise ValueError(f"judge has no result ({verdict['kind']})")
    if not verdict["submission"]["finalized"]:
        raise ValueError("agent did not finalize its submission")
    answer = vector(record, verdict["submission"]["payload"])
    score = int(answer == CORES)
    if verdict["result"]["values"] != [{"name": "score", "value": score}]:
        raise ValueError(
            f"judge score disagrees with the selected vector (expected {score})"
        )
    build = json.loads((record / "record.json").read_bytes())["pumpkin"]["commit"][
        "hash"
    ]
    places = ", ".join(
        f"{p['count']} on {p['provider']}/{p['class']}" for p in usage["sandboxes"]
    )
    print(
        f"{record.name}: score={score}, {usage['model_requests']} model requests, "
        f"sandboxes deleted ({places}), Pumpkin {build}"
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("record", type=Path)
    args = parser.parse_args()
    try:
        verify(args.record)
    except (KeyError, OSError, ValueError) as error:
        parser.exit(1, f"{args.record}: {error}\n")
