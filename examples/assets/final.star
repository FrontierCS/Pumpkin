"""The assets example's final call: it leases the runner twice, and checks its asset in each."""

SUMS = 'cd "$1" && shift && sha256sum "$@"'

# The texts of the errors that refuse a write: a read-only mount's, a file's mode's, and that of a
# change that only a file's owner may make.
REFUSALS = ["Read-only file system", "Permission denied", "Operation not permitted"]

# Each write, by its name, as a command on `$0`, a file of the asset in the current directory.
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
    ("chmod", "chmod 0666 \"$0\""),
    ("chown", "chown 1000:1000 \"$0\""),
]

# Runs each of `WRITES` in the directory `$1` on its file `$2`, and prints each one's name and how
# it ended: `done`, or the text of its error. Then it prints the options of the mount at `$1`.
TRY = """
cd "$1" || exit 1
file=$2
try() {
  said=$(sh -c "$2" "$file" 2>&1) && echo "$1: done" || echo "$1: ${said##*: }"
}
""" + "".join(["try {} '{}'\n".format(name, command) for name, command in WRITES]) + """
awk -v at="$1" '$5 == at {print "mounted: " $6}' /proc/self/mountinfo
"""

# The SHA-256s of `names`, the asset's files at `path`, and, if `writes`, how each of `WRITES` ended.
def _probe(runner, path, names, writes):
    sums = {"sums": runner.run(["sh", "-c", SUMS, "sh", path] + names, timeout_seconds = 30)["stdout_tail"]}
    if not writes:
        return sums
    return sums | {"writes": runner.run(["sh", "-c", TRY, "sh", path, names[0]], timeout_seconds = 30)["stdout_tail"]}

def final_call(target: Target, _submissions, *, path, files):
    """Checks the runner's asset in two leases: its files' SHA-256s, and each write.

    Args:
      target: the call's target, which holds the runner's session.
      _submissions: the agent's submission, which it never makes.
      path: where the runner holds the asset.
      files: the asset's files, as the IR gives them.

    Returns:
      The count of the facts that hold, `score`, of `of`, then each fact. `pinned`: each file has
      its SHA-256 in the first lease. `refused`: each write there failed with one of `REFUSALS`.
      `unchanged`: each file has its SHA-256 in the second lease too. Then `mounted`, the first
      option of the asset's mount, `ro` or `rw`, which is no fact, as a provider may keep the files
      read-only by their modes alone. `unexpected` holds how each write that wasn't refused ended,
      by its name.
    """
    names = [file["path"] for file in files]
    sums = "".join(["{}  {}\n".format(file["sha256"].replace("sha256:", ""), file["path"]) for file in files])
    first = target.with_runner("sandbox", _probe, path, names, True)
    second = target.with_runner("sandbox", _probe, path, names, False)
    lines = [line for line in first["writes"].split("\n") if line != ""]
    ended = {line.split(": ")[0]: line.replace(line.split(": ")[0] + ": ", "") for line in lines}
    writes = {name: how for name, how in ended.items() if name != "mounted"}
    unexpected = {name: how for name, how in writes.items() if how not in REFUSALS}
    facts = {
        "pinned": first["sums"] == sums,
        "refused": sorted(writes.keys()) == sorted([name for name, _ in WRITES]) and len(unexpected) == 0,
        "unchanged": second["sums"] == sums,
    }
    shown = {"mounted": ended.get("mounted", "").split(",")[0], "unexpected": unexpected}
    return {"score": len([fact for fact in facts.values() if fact]), "of": len(facts)} | facts | shown
