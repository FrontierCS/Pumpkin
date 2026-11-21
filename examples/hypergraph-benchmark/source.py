"""Stage the upstream Dockerfiles, their Git inputs and the checksummed LFS downloads."""

import json
import subprocess
from pathlib import Path

CERTIFICATES = "python:3.12-slim-bookworm@sha256:935c3c5b12357a7e10caa9c1e6ca5692344d97cf229832ef584be2cec9fc03df"
WHEEL = "zstandard-0.25.0-cp312-cp312-manylinux2014_x86_64.manylinux_2_17_x86_64.whl"
WHEEL_URL = (
    "https://files.pythonhosted.org/packages/53/60/7be26e610767316c028a2cbedb9a3beabdbe33e2182c373f71a1c0b88f36/"
    + WHEEL
)
WHEEL_SHA256 = "5a56ba0db2d244117ed744dfa8f6f5b366e14148e00de44723413b2f3938a902"


def write(path, data, mode=0o644):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data if isinstance(data, bytes) else data.encode())
    path.chmod(mode)


class Source:
    def __init__(self, repository, revision):
        self.repository = repository
        self.revision = revision

    def git(self, *args):
        return subprocess.check_output(
            ["git", "--no-replace-objects", "-C", str(self.repository), *args]
        )

    def read(self, path):
        return self.git("show", f"{self.revision}:{path}")

    def files(self, *paths):
        for record in (
            self.git("ls-tree", "-r", self.revision, "--", *paths).decode().splitlines()
        ):
            metadata, name = record.split("\t")
            mode, kind, _ = metadata.split()
            if kind != "blob" or mode not in ("100644", "100755"):
                raise ValueError(f"expected a regular source file at {name}")
            yield Path(name), self.read(name), int(mode, 8) & 0o777

    def copy(self, root, prefix, paths):
        for name, data, mode in self.files(*(str(Path(prefix, p)) for p in paths)):
            write(root / name.relative_to(prefix), data, mode)


def shared_recipe(dockerfile, role):
    # Bootstrap HTTPS from a pinned certificate bundle so all packages come from the dated snapshot.
    before, install = dockerfile.split("RUN apt-get update", 1)
    _, snapshot = install.split("    && printf 'Types: deb", 1)
    before = before.replace(
        "ARG DEBIAN_FRONTEND",
        "COPY --from=certificates /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt\nARG DEBIAN_FRONTEND",
    )
    recipe = (
        f"FROM {CERTIFICATES} AS certificates\n"
        + before
        + "RUN printf 'Types: deb"
        + snapshot
    )
    recipe = recipe.replace(
        "--no-install-recommends git python3",
        "--no-install-recommends ca-certificates git python3",
    )
    if role == "verifier":
        recipe = (
            f"FROM scratch AS wheels\nADD --checksum=sha256:{WHEEL_SHA256} {WHEEL_URL} /{WHEEL}\n"
            + recipe.replace(
                "RUN printf 'Types:",
                "RUN --mount=type=bind,from=wheels,target=/wheels printf 'Types:",
            ).replace(
                "pip3 install --no-cache-dir zstandard==0.25.0",
                f"pip3 install --no-index --no-deps /wheels/{WHEEL}",
            )
        )
    return recipe


def build_inputs(binding, repository, root):
    source = binding["source"]
    git = Source(repository, source["revision"])
    task = source["task"]
    shared = "tasks/shared"
    generator = root / "generation"
    git.copy(
        generator,
        ".",
        [
            "scripts/task_manifest.py",
            f"{shared}/lib/edit_scope.py",
            f"{shared}/task-defaults.toml",
            f"{shared}/agent/README.platform.md",
            f"{task}/README.md",
            f"{task}/task.toml",
            f"{task}/datasets/train.json",
            f"{task}/datasets/eval.json",
        ],
    )
    write(generator / "task-path", task)

    for role in ("agent", "verifier"):
        base = root / f"{role}-base"
        paths = (
            ["agent/bin", "lib/edit_scope.py"]
            if role == "agent"
            else [
                "verifier",
                "lib/edit_scope.py",
                "lib/isolate.py",
                source["verifier_python_path"],
            ]
        )
        git.copy(base, shared, paths)
        dockerfile = git.read(f"{shared}/images/{role}-base.Dockerfile").decode()
        write(base / "Dockerfile", shared_recipe(dockerfile, role))

        native = root / f"native-{role}"
        git.copy(native, task, ["repo"])
        recipe = git.read(f"{task}/docker/{role}.Dockerfile").decode()
        arg = role.upper() + "_BASE"
        recipe = recipe.replace("ARG " + arg + "\nFROM ${" + arg + "}", "FROM shared")
        recipe = recipe.replace(
            "COPY " + source["generated"] + "/",
            "COPY --chown=0:0 --from=generated /generated/",
        )
        if role == "verifier":
            downloads = []
            for name, data, mode in git.files(f"{task}/judge"):
                if name.parent == Path(task, "judge/workloads"):
                    version, oid, _ = data.decode().splitlines()
                    if version != "version https://git-lfs.github.com/spec/v1":
                        raise ValueError(f"expected a Git LFS pointer at {name}")
                    digest = oid.removeprefix("oid ")
                    downloads.append(
                        f"ADD --checksum={digest} {source['media']}/{name} /{name.name}"
                    )
                else:
                    write(native / name.relative_to(task), data, mode)
            prefix = "FROM scratch AS workloads\n" + "\n".join(downloads) + "\n"
            copy = next(
                line for line in recipe.splitlines() if line.startswith("COPY judge ")
            )
            recipe = prefix + recipe.replace(
                copy,
                copy + "\nCOPY --from=workloads / " + copy.split()[-1] + "/workloads/",
            )
        write(native / "Dockerfile", recipe)

    for role, interface in binding["interface"].items():
        folder = root / role
        write(
            folder / "interface.json",
            json.dumps(interface, sort_keys=True, indent=2) + "\n",
        )
        write(folder / "ATTRIBUTION", binding["attribution"] + "\n")
        write(folder / "LICENSE", git.read("LICENSE"))
    for name in ("train.json", "eval.json"):
        write(root / "judge" / name, git.read(f"{task}/datasets/{name}"))
