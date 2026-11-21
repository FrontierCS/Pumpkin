"""Run the source manifest generator and check the bytes used by both role images."""

import hashlib
import sys
from pathlib import Path

SOURCE = Path("/source")
EXPECTED = {
    "agent/README.md": "3bc7f728f6fa48400aa342e14306e7ecc86b52dc6112c3d4d3571da5b81307c7",
    "agent/submission-policy.json": "78c11077a764750796d4b41a7856de35a647ef1f73ebb6e33f3025485f1ec39e",
    "effective-task.toml": "4d4345bed7e3db311b85dc2c2871c5e16bbabdd9f89fd0532bff3b8255a60fb4",
    "judge/config.yaml": "d195bd9fc12bea30acdd18b4fde0c7c3f195f574d725d2ff9956aa871471d525",
    "verifier/verifier.json": "bbf3eda3b4bea93284e01a110619dfb5702539e45ca59643a684f9bf9086dea9",
}

sys.path.insert(0, str(SOURCE / "scripts"))
from task_manifest import generated_files

files = generated_files(SOURCE / (SOURCE / "task-path").read_text())
outputs = {str(Path(*Path(k).parts[1:])): data for k, data in files.items()}
if {k: hashlib.sha256(v).hexdigest() for k, v in outputs.items()} != EXPECTED:
    raise ValueError("generated files differ from the native benchmark")
for name, data in outputs.items():
    path = Path("/generated", name)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
