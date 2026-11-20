"""Prepare each agent's own workspace before handing its stdin to Codex."""

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

free = shutil.disk_usage("/workspace").free
Path("/pumpkin/out/workspace.json").write_text(json.dumps({"available_bytes": free}))
if free < 2 * 1024**3:
    raise SystemExit(f"/workspace has {free} bytes free; the workload requires 2 GiB")
subprocess.run(
    [json.loads(Path("/opt/team/interface.json").read_bytes())["prepare"]], check=True
)
os.execv(sys.argv[1], sys.argv[1:])
