"""Two Codex agents write C++ hypergraph solvers and cross-check a case on the shared board."""

load("@pumpkin//harness/codex/codex.star", "codex")
load("//team/workflow.star", "PARTICIPANTS", "team")

MODEL = arg("model", str)

PROMPT = """Write a standalone C++17 program in solution.cpp to compute hypergraph core numbers.
Read one graph from stdin: n m, then m lines, each containing the edge size followed by distinct,
zero-based vertex IDs. Print exactly n integers in vertex order. The graphs are small; the score
counts correct vectors, with no reward for speed.

A vertex's core number is the largest k for which it belongs to a set whose members all have
degree at least k. An edge contributes one to each member's degree only when all its members
are in the set. Identical edges count separately. For example, with four vertices and a single
edge {0,1,2}, all three members are in a 1-core together and vertex 3 is isolated. The output
must be 1 1 1 0. Check this case with your executable.

Compile with g++ --std=c++17 solution.cpp --output solution. Save the first working source with
/.pumpkin/bin/sandboxctl call select_ROLE solution.cpp (replace ROLE with alice or bob). This
keeps you working. Exchange your test on the board before asking the evaluator to score a draft.
You can call the board with python3 /opt/team/client.py followed by one JSON request as an
argument, or in Python: import sys, json; sys.path.insert(0, '/opt/team'); from client import board.

Run /.pumpkin/bin/sandboxctl call eval solution.cpp and poll /.pumpkin/bin/sandboxctl status JOB
until completed. Two is the full score. You have at most two evaluations and fifteen minutes in
all. When you have a full score, or after your second evaluation at any score, finalize your best
evaluated source immediately with your role's final command. A partial score counts.
"""

ASSIGNMENTS = {
    "alice": """You are Alice. Write a simple implementation based on the definition. Design a small
test with a repeated hyperedge or an isolated vertex. Run it and publish its graph and actual
output early: board(op='post', text=json.dumps({'vertices': n, 'edges': edges, 'cores': cores})).
Edges is a list of vertex lists and cores is the integer output in vertex order. Check for a
message ID. Evaluate your solution and finalize it with
/.pumpkin/bin/sandboxctl call select_alice solution.cpp --arg finalize=true.
""",
    "bob": """You are Bob. Implement an incidence-list peeling algorithm. Compile, test and save it.
Read Alice's case with board(op='read', stream='board', after=0). If empty, poll in the shell
until she posts. Parse the JSON in her message's text, serialize the graph to the stdin format,
and run your compiled program on it. Compare its output to her vector. Send her the observed
integers even if they differ: board(op='send', to='alice', text=json.dumps({'checked': message_id,
'cores': cores})). Take the numeric message_id from the board message and cores from your own
program. Check for the returned ID. Now evaluate your solution and finalize it with
/.pumpkin/bin/sandboxctl call select_bob solution.cpp --arg finalize=true.
""",
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
