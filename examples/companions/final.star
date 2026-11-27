"""The companions example's final call: it leases the runner, and probes each of its containers."""

# Prints what each argument, a container's name, serves at `/who`, or `unreached`.
FETCH = 'for peer in "$@"; do wget -q -O - -T 5 "http://$peer:8000/who" || echo unreached; done'

# Serves the first argument, the container's own name, at `/who` on port 8000, and echoes each
# line on port 9000, at each of the container's addresses. Prints the user that it runs as, then
# fetches from the other arguments.
SERVE = """
mkdir /tmp/www && echo "$1" > /tmp/www/who && httpd -p 8000 -h /tmp/www || exit 1
nc -lk -p 9000 -e cat </dev/null >/dev/null 2>&1 &
echo "$(id -u):$(id -g)"
shift
""" + FETCH

# For each argument, sends one line to its port 9000 for 2 s over one connection, each once the
# last came back. Prints the median and the slowest of those exchanges in milliseconds, and their
# count. `ts -i` stamps each line with the time since the last, and the first with the connect's.
ROUND_TRIPS = """
for peer in "$@"; do
  rm -f /tmp/loop && mkfifo /tmp/loop || exit 1
  { echo x; cat /tmp/loop; } | timeout 2 nc "$peer" 9000 2>/dev/null | tee /tmp/loop |
    ts -i %.S | sed 1d | awk '{print $1 * 1000}' | sort -n |
    awk '{ms[NR] = $1} END {printf "%.3f %.3f %d\\n", ms[int((NR + 1) / 2)], ms[NR], NR}'
done
"""

# Prints whether a public name resolves, then what a TCP connection to a public address says.
EGRESS = """
nslookup example.com >/dev/null 2>&1 && echo "DNS answers" || echo "DNS fails"
nc -vv -w 5 1.1.1.1 443 </dev/null 2>&1 && echo connected
"""

def _probe(runner, names, before, hold_seconds):
    said = {"main": runner.run(["sh", "-c", SERVE, "sh", "main"], timeout_seconds = 30)}
    for_each = {
        name: runner.run(["sh", "-c", SERVE, "sh", name] + _peers(name, before), timeout_seconds = 30, container = name)
        for name in names
    }
    said = said | for_each | {
        "main reaches": runner.run(["sh", "-c", FETCH, "sh"] + names, timeout_seconds = 60),
        "round trips": runner.run(["sh", "-c", ROUND_TRIPS, "sh", names[0], "127.0.0.1"], timeout_seconds = 30),
        "egress": runner.run(["sh", "-c", EGRESS], timeout_seconds = 30, container = names[0]),
    }
    if hold_seconds > 0:
        runner.run(["sleep", str(hold_seconds)], timeout_seconds = hold_seconds + 30)

        # A session that the hold ended fails the lease, which names its container.
        held = [runner.run(["true"], timeout_seconds = 30, container = name) for name in names]
        said = said | {"after the hold": {"stdout_tail": "".join([output["stdout_tail"] for output in held])}}
    return {where: output["stdout_tail"] for where, output in said.items()}

# What companion `name` fetches from: `main`, and the companion declared before it, if any.
def _peers(name, before):
    return ["main"] + ([before[name]] if name in before else [])

def _text(lines):
    return "".join([line + "\n" for line in lines])

def _none_false(values):
    return len([value for value in values if not value]) == 0

def _round_trip(line):
    parts = line.split(" ")
    if len(parts) != 3:
        return {"said": line}
    return {"median": float(parts[0]), "slowest": float(parts[1]), "exchanges": int(parts[2])}

def final_call(target: Target, _submissions, *, user, companions, hold_seconds):
    """Checks what holds of the runner's containers, and times the round trips between them.

    Args:
      target: the call's target, which holds the runner's session.
      _submissions: the agent's submission, which it never makes.
      user: `main`'s user.
      companions: each companion in the order declared: its `name`, its `user`, and `before`,
        the name of the one declared before it, if any.
      hold_seconds: how long the lease is held, idle, once its probes are done, before a command
        in each companion.

    Returns:
      The count of the facts that hold, `score`, of `of`, then each fact. `users`: each
      container's command ran as its user. `main_reaches`: `main` reached each companion by its
      name. `companions_reach`: each companion reached `main` and the one before it by their
      names. `internet_unreached`: the first companion's TCP to a public address failed. Then
      `dns`, whether a public name resolves there, and `bridge_ms` and `loopback_ms`, the
      exchanges of `main` with the first companion and with itself. `unexpected` holds what each
      command said that departed from these facts, by where it ran.
    """
    names = [companion["name"] for companion in companions]
    users = {"main": user} | {companion["name"]: companion["user"] for companion in companions}
    before = {companion["name"]: companion["before"] for companion in companions if "before" in companion}
    said = target.with_runner("sandbox", _probe, names, before, hold_seconds)
    first = {where: said[where].split("\n")[0] for where in users.keys()}
    ran_as = {where: first[where] == "{}:{}".format(users[where]["uid"], users[where]["gid"]) for where in users.keys()}
    reached = {name: said[name] == first[name] + "\n" + _text(_peers(name, before)) for name in names}
    egress = said["egress"].split("\n")
    facts = {
        "users": _none_false(ran_as.values()),
        "main_reaches": said["main reaches"] == _text(names),
        "companions_reach": _none_false(reached.values()),
        "internet_unreached": len(egress) > 1 and egress[1].startswith("nc: 1.1.1.1 (1.1.1.1:443): "),
    }
    departed = [where for where in users.keys() if not ran_as[where] or not reached.get(where, True)]
    departed = departed + [where for where, holds in [("main reaches", facts["main_reaches"]), ("egress", facts["internet_unreached"])] if not holds]
    trips = said["round trips"].split("\n")
    return {"score": len([fact for fact in facts.values() if fact]), "of": len(facts)} | facts | {
        "dns": egress[0],
        "bridge_ms": _round_trip(trips[0]),
        "loopback_ms": _round_trip(trips[1]) if len(trips) > 1 else {"said": ""},
        "unexpected": {where: said[where] for where in departed},
    }
