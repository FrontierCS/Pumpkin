"""The companions example's final call: it leases the runner, and probes each of its containers."""

# Prints what each argument, a container's name, serves at `/who`, or `unreached`.
FETCH = 'for peer in "$@"; do wget -q -O - -T 5 "http://$peer:8000/who" || echo unreached; done'

# Serves the first argument, the container's own name, at `/who`, at port 8000 of each of the
# container's addresses, and prints the user that it runs as. Then fetches from the other
# arguments.
SERVE = 'mkdir /tmp/www && echo "$1" > /tmp/www/who && httpd -p 8000 -h /tmp/www && echo "$(id -u):$(id -g)" && shift && ' + FETCH

def _probe(runner, names):
    main = runner.run(["sh", "-c", SERVE, "sh", "main"], timeout_seconds = 30)
    beside = {name: runner.run(["sh", "-c", SERVE, "sh", name, "main"], timeout_seconds = 30, container = name) for name in names}
    reached = runner.run(["sh", "-c", FETCH, "sh"] + names, timeout_seconds = 60)
    said = {"main": main, "main reaches": reached} | beside
    return {where: output["stdout_tail"] for where, output in said.items()}

def _written(user):
    return "{}:{}\n".format(user["uid"], user["gid"])

def final_call(target: Target, submissions, *, user, companions):
    """Counts what holds of the runner's containers, and shows what each said.

    Args:
      target: the call's target, which holds the runner's session.
      submissions: the agent's submission, which it never makes.
      user: the runner's user.
      companions: each companion's user, by the companion's name.

    Returns:
      The score, the count of the commands that said what was expected, of `of`, and what each
      said, by its container. `main reaches` is what `main` fetched from each companion.
    """
    names = companions.keys()
    said = target.with_runner("sandbox", _probe, names)
    beside = {name: _written(companions[name]) + "main\n" for name in names}
    expected = {"main": _written(user), "main reaches": "".join([name + "\n" for name in names])} | beside
    score = len([where for where in expected.keys() if said[where] == expected[where]])
    return {"score": score, "of": len(expected), "submitted": submissions["agent"] != None, "said": said}
