"""The assets example's final call: it leases the runner twice, and checks its asset in each."""

# Prints the SHA-256 and the path of each file that an argument after the first names, in the
# directory that the first names, as `sha256sum` does.
SUMS = 'cd "$1" && shift && sha256sum "$@"'

# Each write that a read-only mount refuses, by its name, as a command on `$0`, a file of the asset
# in the current directory.
WRITES = [
    ("create", "echo x > new"),
    ("append", "echo x >> \"$0\""),
    ("rdwr", "exec 3<> \"$0\""),
    ("truncate", "truncate -s 0 \"$0\""),
    ("unlink", "rm \"$0\""),
    ("rename", "mv \"$0\" moved"),
    ("utime", "touch \"$0\""),
    ("mkdir", "mkdir made"),
    ("symlink", "ln -s \"$0\" linked"),
    ("link", "ln \"$0\" hard"),
    ("setxattr", "setfattr -n user.pumpkin -v 1 \"$0\""),
]

# Runs each of `WRITES` in the directory `$1` on its file `$2`, and prints each one's name and how
# it ended: `done`, or the text of its error, which for `EROFS` is `Read-only file system`.
TRY = """
cd "$1" || exit 1
file=$2
try() {
  said=$(sh -c "$2" "$file" 2>&1) && echo "$1: done" || echo "$1: ${said##*: }"
}
""" + "".join(["try {} '{}'\n".format(name, command) for name, command in WRITES])

EROFS = "Read-only file system"

# The SHA-256s of `names`, the asset's files at `path`, and, if `writes`, how each of `WRITES` ended.
def _probe(runner, path, names, writes):
    sums = {"sums": runner.run(["sh", "-c", SUMS, "sh", path] + names, timeout_seconds = 30)["stdout_tail"]}
    if not writes:
        return sums
    return sums | {"writes": runner.run(["sh", "-c", TRY, "sh", path, names[0]], timeout_seconds = 30)["stdout_tail"]}

def final_call(target: Target, _submissions, *, path, files):
    """Checks the runner's asset in two leases: its files' SHA-256s, and that each write fails.

    Args:
      target: the call's target, which holds the runner's session.
      _submissions: the agent's submission, which it never makes.
      path: where the runner holds the asset.
      files: the asset's files, as the IR gives them.

    Returns:
      The count of the facts that hold, `score`, of `of`, then each fact. `pinned`: each file has
      its SHA-256 in the first lease. `read_only`: each write there failed with `EROFS`.
      `unchanged`: each file has its SHA-256 in the second lease too. `unexpected` holds how each
      write that departed from that ended, by its name.
    """
    names = [file["path"] for file in files]
    sums = "".join(["{}  {}\n".format(file["sha256"].replace("sha256:", ""), file["path"]) for file in files])
    first = target.with_runner("sandbox", _probe, path, names, True)
    second = target.with_runner("sandbox", _probe, path, names, False)
    lines = [line for line in first["writes"].split("\n") if line != ""]
    ended = {line.split(": ")[0]: line.replace(line.split(": ")[0] + ": ", "") for line in lines}
    unexpected = {name: how for name, how in ended.items() if how != EROFS}
    facts = {
        "pinned": first["sums"] == sums,
        "read_only": sorted(ended.keys()) == sorted([name for name, _ in WRITES]) and len(unexpected) == 0,
        "unchanged": second["sums"] == sums,
    }
    return {"score": len([fact for fact in facts.values() if fact]), "of": len(facts)} | facts | {"unexpected": unexpected}
