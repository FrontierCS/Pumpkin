"""The assets example's final call: it leases the runner twice, and checks its asset in each."""

SUMS = 'cd "$1" && shift && sha256sum "$@"'

# The texts of the errors that refuse a write: the mount's, and the file's mode's.
EROFS = "Read-only file system"
EACCES = "Permission denied"

# Each write, by its name, as a command on `$0`, a file of the asset in the current directory, with
# the error that refuses it. The kernel looks at a mount's flag before a file's mode, but for an
# open of a file to write. There the file's mode refuses it first, as the role's user isn't root.
WRITES = [
    ("create", "echo x > new", EROFS),
    ("append", "echo x >> \"$0\"", EACCES),
    ("rdwr", "exec 3<> \"$0\"", EACCES),
    ("truncate", "truncate -s 0 \"$0\"", EACCES),
    ("unlink", "rm \"$0\"", EROFS),
    ("rename", "mv \"$0\" moved", EROFS),
    ("utime", "touch \"$0\"", EROFS),
    ("mkdir", "mkdir made", EROFS),
    ("symlink", "ln -s \"$0\" linked", EROFS),
    ("link", "ln \"$0\" hard", EROFS),
    ("setxattr", "setfattr -n user.pumpkin -v 1 \"$0\"", EROFS),
    ("chmod", "chmod 0666 \"$0\"", EROFS),
    ("chown", "chown 1000:1000 \"$0\"", EROFS),
]

# Runs each of `WRITES` in the directory `$1` on its file `$2`, and prints each one's name and how
# it ended: `done`, or the text of its error. Then it prints the options of the mount at `$1`.
TRY = """
cd "$1" || exit 1
file=$2
try() {
  said=$(sh -c "$2" "$file" 2>&1) && echo "$1: done" || echo "$1: ${said##*: }"
}
""" + "".join(["try {} '{}'\n".format(name, command) for name, command, _ in WRITES]) + """
awk -v at="$1" '$5 == at {print "mounted: " $6}' /proc/self/mountinfo
"""

# The SHA-256s of `names`, the asset's files at `path`, and, if `writes`, how each of `WRITES` ended.
def _probe(runner, path, names, writes):
    sums = {"sums": runner.run(["sh", "-c", SUMS, "sh", path] + names, timeout_seconds = 30)["stdout_tail"]}
    if not writes:
        return sums
    return sums | {"writes": runner.run(["sh", "-c", TRY, "sh", path, names[0]], timeout_seconds = 30)["stdout_tail"]}

def final_call(target: Target, _submissions, *, path, files):
    """Checks the runner's asset in two leases: its files' SHA-256s, its mount, and each write.

    Args:
      target: the call's target, which holds the runner's session.
      _submissions: the agent's submission, which it never makes.
      path: where the runner holds the asset.
      files: the asset's files, as the IR gives them.

    Returns:
      The count of the facts that hold, `score`, of `of`, then each fact. `pinned`: each file has
      its SHA-256 in the first lease. `read_only`: the asset is mounted there with `ro`.
      `refused`: each write there failed with its error. `unchanged`: each file has its SHA-256 in
      the second lease too. `unexpected` holds how each write that departed from that ended, by its
      name, and the mount's options, if it departed.
    """
    names = [file["path"] for file in files]
    sums = "".join(["{}  {}\n".format(file["sha256"].replace("sha256:", ""), file["path"]) for file in files])
    first = target.with_runner("sandbox", _probe, path, names, True)
    second = target.with_runner("sandbox", _probe, path, names, False)
    lines = [line for line in first["writes"].split("\n") if line != ""]
    ended = {line.split(": ")[0]: line.replace(line.split(": ")[0] + ": ", "") for line in lines}
    expected = {name: error for name, _, error in WRITES}
    unexpected = {name: how for name, how in ended.items() if name != "mounted" and how != expected.get(name)}
    mounted = ended.get("mounted", "")
    facts = {
        "pinned": first["sums"] == sums,
        "read_only": mounted.split(",")[0] == "ro",
        "refused": sorted([name for name in ended.keys() if name != "mounted"]) == sorted(expected.keys()) and len(unexpected) == 0,
        "unchanged": second["sums"] == sums,
    }
    departed = {} if facts["read_only"] else {"mounted": mounted}
    return {"score": len([fact for fact in facts.values() if fact]), "of": len(facts)} | facts | {"unexpected": unexpected | departed}
