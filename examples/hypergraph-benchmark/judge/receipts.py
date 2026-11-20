"""Check the native evaluator's qualified SHA-256 digests for the selected bytes and dataset."""

import math
import re
import statistics


def check_receipt(receipt, payload_digest, dataset_digest):
    if receipt.get("mode") != "eval":
        raise ValueError("native evidence is not from eval mode")
    for field, expected in (
        ("payload_sha256", payload_digest),
        ("dataset_sha256", dataset_digest),
    ):
        observed = receipt.get(field)
        if not all(
            isinstance(value, str) and re.fullmatch(r"sha256:[0-9a-f]{64}", value)
            for value in (expected, observed)
        ):
            raise ValueError(f"invalid qualified SHA-256 for {field}")
        if expected != observed:
            raise ValueError(f"native evidence digest mismatch for {field}")


# SHA-256 of each graph in the pinned native evaluator.
GRAPHS = {
    "feedback-a": "47b51f021331da95098c8f11faa35c7c2bb617d69c838ed982b79f882e27dcdb",
    "feedback-b": "5d97c2500eac5d07f5959d8436b61db3cf420e8e8e12bb09205ca7d4f799b740",
    "graded-a": "5d90f176f2e912be3eb2836d0fd0f92b0b77bc367e2237facaa9b2dc0c82ad81",
    "graded-b": "5a684b49a471c6d37a6be35a16bdf044453fb01c81b0d9616ea41a3935478c87",
}


def check_timing(receipt, score):
    if receipt["raw_score"] != score:
        raise ValueError("native score and evidence disagree")
    if score == 0:
        return None
    metrics = receipt["metrics"]
    if metrics["workload_sha256"] != GRAPHS:
        raise ValueError("native evaluation used different graphs")
    failures = metrics["candidate_sequential_failures"]
    if failures.keys() - GRAPHS.keys() or failures and metrics["beats_reference"]:
        raise ValueError("single-thread failure and reference result disagree")
    for field in (
        "candidate_runtime_samples",
        "reference_runtime_samples",
        "candidate_sequential_runtime_samples",
        "reference_sequential_runtime_samples",
    ):
        observed = metrics[field]
        if observed.keys() != GRAPHS.keys():
            raise ValueError(f"{field}: workload coverage")
        for graph, times in observed.items():
            if not all(t > 0 and math.isfinite(t) for t in times):
                raise ValueError(f"{field}: invalid time for {graph}")
            if "sequential" in field and graph in failures:
                if len(times) > 7:
                    raise ValueError(f"{field}: too many samples for {graph}")
            elif len(times) != 7:
                raise ValueError(f"{field}: expected seven samples for {graph}")
    median = statistics.median(
        statistics.median(times)
        for times in metrics["candidate_runtime_samples"].values()
    )
    if not math.isclose(
        median, metrics["candidate_runtime_seconds_median"]
    ) or not math.isclose(score, 100 * math.exp(-median / 0.25), rel_tol=1e-12):
        raise ValueError("score differs from the native timer samples")
    if metrics["candidate_core_numbers"] != "validated on every measured execution":
        raise ValueError("incomplete core-vector validation")
    return median
