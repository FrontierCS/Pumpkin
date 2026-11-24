"""Prepare each agent's own workspace before handing its stdin to Codex."""

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

REPO = Path("/workspace/repo")

free = shutil.disk_usage("/workspace").free
Path("/pumpkin/out/workspace.json").write_text(json.dumps({"available_bytes": free}))
if free < 2 * 1024**3:
    raise SystemExit(f"/workspace has {free} bytes free; the workload requires 2 GiB")
# A Git checkout of the read-only starter, whose first commit make-team-archive diffs against,
# so that the agent's later commits still count.
shutil.copytree("/opt/starter", REPO, symlinks=True)
subprocess.run(["chmod", "-R", "u+rwX", REPO], check=True)
for args in (
    ["init", "--quiet"],
    ["config", "user.name", "starter"],
    ["config", "user.email", "starter@invalid"],
    ["add", "--all"],
    ["commit", "--quiet", "--message", "baseline"],
    ["update-ref", "refs/starter/baseline", "HEAD"],
):
    subprocess.run(["git", "-C", REPO, *args], check=True)
os.execv(sys.argv[1], sys.argv[1:])
