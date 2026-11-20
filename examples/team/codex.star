"""Two Codex agents write C++ hypergraph solvers and cross-check a case on the shared board."""

load("@pumpkin//harness/codex/codex.star", "codex")
load("//team/workflow.star", "PARTICIPANTS", "team")

MODEL = arg("model", str)

PROMPT = """Write a standalone C++17 program to compute hypergraph core numbers.
Read one graph from stdin: n m, then m lines, each containing the edge size followed by distinct,
zero-based vertex IDs. Print exactly n integers in vertex order. The graphs are small; the score
counts correct vectors, with no reward for speed.

A vertex's core number is the largest k for which it belongs to a set whose members all have
degree at least k. An edge contributes one to each member's degree only when all its members
are in the set. Identical edges count separately. For example, with four vertices and a single
edge {0,1,2}, all three members are in a 1-core together and vertex 3 is isolated. The output
must be 1 1 1 0. Check this case with your executable.

Save your first working source as soon as it compiles. Exchange your test before having a draft
scored. Two correct vectors earn full credit. Finalize your best evaluated source as soon as it
earns full credit or you use your last evaluation. A partial score counts.
"""

ASSIGNMENTS = {
    "alice": """You are Alice. Implement the definition directly. Design a small test with a repeated
hyperedge or an isolated vertex. Run your program and publish the graph and its actual output
early. Share a JSON object with vertices (vertex count), edges (lists of vertex IDs), and cores
(your program's integers in vertex order).""",
    "bob": """You are Bob. Implement an incidence-list peeling algorithm. Look for Alice's graph on
the shared board. If it is empty, check again until she posts. Run your compiled program on her
graph and compare its output with hers. Send Alice a private reply even if the outputs differ.
Share a JSON object with checked (her numeric message ID) and cores (the integers your program
printed in vertex order).""",
}

toolchain = image("runner/Dockerfile")
agent = codex(
    image("Dockerfile", target = "client", contexts = {"toolchain": toolchain}),
    model = MODEL,
    effort = "low",
    tools = "native",
    workspace = "/workspace",
    prompt = PROMPT,
    finalize_grace_seconds = 0,
)
runtime = agent["runtime"]["agent"]
routes = [route | {"budget": {"microusd": USD // 4}} for route in runtime["egress"]["model"]]

team(
    "examples/team/codex",
    agent["image"],
    {role: runtime | {
        "stdin": PROMPT + "\n" + ASSIGNMENTS[role],
        "egress": {"model": routes},
    } for role in PARTICIPANTS},
    toolchain = toolchain,
    evaluations = 2,
    needs = {"cpu_millis": 1000, "memory_bytes": 1 * GiB},
    deadline_seconds = 900,
)
