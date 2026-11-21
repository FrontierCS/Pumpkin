"""One private Codex session and its grants per participant."""

load("@pumpkin//harness/codex/codex.star", "codex")
load("//hypergraph-benchmark/workflow.star", "GRANTS")

PROMPT = """
You are {role}, one of alice, bob, carol, dan and eve. Your team has up to 5,400 seconds to
implement and optimize exact hypergraph core numbers in C++. The team earns
the best official final score among five individual selections. Write your own candidate,
try algorithmic improvements and exchange ideas, implementation details and measurements.
Read /workspace/repo/README.md in full, including the exact result boundary, local
compiler flags, legal edit surface and performance objective. Follow that task statement.
Your private Git checkout is ready at /workspace/repo. Write working files in /workspace.
Develop and test with your own inputs. The judge runs your submitted code on eight cores.

In your checkout, edit the allowed C++ file outside its fixed Compute suffix. Add included
helper sources only under the directory allowed by the task statement. Write separate test
drivers and generated graphs under /workspace outside the Git checkout. The map you return must
own GA.nv uintE values, one exact core number per vertex. Compute the result from the input
graph without recognizing benchmark contents, sizes, or the train/final phase.

Publish your approach on the shared board, read public posts and check your private inbox.
Exchange results during development. Reply to a peer by message ID with what you actually tested
or learned. Teammates are alice, bob, carol, dan and eve.

Save your own native archive as soon as you have a working candidate and again after improvements.
Obtain official training feedback on promising implementations. Training may take minutes; you
can continue working or communicating while it runs. Use your eight-core sandbox for local
experiments as often as useful. After reaching correctness, compare genuinely different
implementations using measurements and consider what your peers have learned. Lock your best
selection when your experiments justify it or the common deadline approaches.
"""

def participant(role, base):
    """Sets up one Codex participant.

    Args:
      role: its declared name.
      base: the native agent image.

    Returns:
      The sandbox, model route and grants for this role.
    """
    harness = codex(
        base,
        model = "glm-5.3-flash",
        tools = "native",
        effort = "low",
        workspace = "/workspace",
        prompt = PROMPT.format(role = role),
        finalize_grace_seconds = 60,
    )
    runtime = harness["runtime"]["agent"]
    command = runtime["command"]
    route = runtime["egress"]["model"]
    return {
        "name": role,
        "image": harness["image"],
        "user": {"uid": 10001, "gid": 10001},
        "needs": {"cpu_millis": 8000, "memory_bytes": 32 * GiB},
        "runtime": {"agent": runtime | {
            "command": command | {"argv": ["python3", "/opt/team/start.py"] + command["argv"]},
            "egress": {"model": [entry | {"budget": {"microusd": 5 * USD}} for entry in route]},
            "grants": GRANTS,
        }},
    }
