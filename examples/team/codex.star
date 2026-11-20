"""Two Codex agents compute public modular predictions and exchange them on a shared board."""

load("@pumpkin//harness/codex/codex.star", "codex")
load("//team/workflow.star", "PARTICIPANTS", "team")

MODEL = arg("model", str)

PROMPT = """For each assigned row [a,b] and public offset e, predict (2*a + 3*b + e) modulo 13.
Use your shell. Call the shared board using python3 /opt/team/client.py followed by one JSON
request as an argument. It prints the board's JSON reply. Write your own answer as a JSON object
mapping row names to integer predictions in answer.json. Finalize it by running
/.pumpkin/bin/sandboxctl call select_ROLE answer.json --arg finalize=true (replace ROLE with
your lowercase role name).
"""

ASSIGNMENTS = {
    "alice": """You are alice. Compute amber from [3,4], offset -1. Post your prediction to
the board before you finalize using {"op":"post","text":"..."} with your row and value in text.
Check that the board returns an id. The required answer.json template is {"amber": A}.
Replace A with your computed integer, keeping the literal key "amber". Finalize this file with
select_alice. Bob can read your post after you finish.
""",
    "bob": """You are bob. Read Alice's amber prediction with {"op":"read","stream":"board","after":0}.
If messages is empty, wait two seconds and read again. Check her arithmetic: amber has row [3,4]
and offset -1. Acknowledge the message you read with {"op":"send","to":"alice","text":"Checked message N"}
where N is its numeric id. Compute blue from [2,5], offset 1. The required answer.json template
is {"amber": A, "blue": B}. Replace A with your checked amber integer and B with your blue
integer, keeping both literal keys. Finalize this file with select_bob.
""",
}

agent = codex(
    image("Dockerfile", target = "client"),
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
        "stdin": ASSIGNMENTS[role] + "\n" + PROMPT,
        "egress": {"model": routes},
    } for role in PARTICIPANTS},
    needs = {"cpu_millis": 1000, "memory_bytes": 1 * GiB},
    deadline_seconds = 300,
)
