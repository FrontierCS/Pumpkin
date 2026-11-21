"""Five Codex agents implement exact hypergraph core numbers and exchange results."""

load("//hypergraph-benchmark/images.star", "agent_image", "judge_image")
load("//hypergraph-benchmark/participant.star", "participant")
load("//hypergraph-benchmark/workflow.star", "ROLES", "team")

agent = agent_image()

team(
    [participant(role, agent) for role in ROLES],
    train_image = judge_image("train"),
    judge_image = judge_image("eval"),
)
