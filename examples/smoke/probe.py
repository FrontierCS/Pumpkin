"""Prints, for the sandbox it runs in, one line for each way out that its confinement must close
or keep open, `<sandbox> <check>: ok|FAILED (<what it saw>)`, where `ok` says the sandbox is as
it should be: its role's user isn't root; the instance metadata address and a public address
can't be reached; DNS can't be either, but where `sandboxd` listens, as on Modal, whose
sandboxes reach Modal's resolver and nothing else, so a name resolves there; and the gateway
can, as `sandboxd` holds the session that this probe runs over."""

import os
import socket
import sys

METADATA = ("169.254.169.254", 80)
PUBLIC = ("1.1.1.1", 443)
NAME = "example.com"
TIMEOUT_SECONDS = 5


def line(sandbox, check, ok, seen):
    print("{} {}: {} ({})".format(sandbox, check, "ok" if ok else "FAILED", seen), flush=True)


def connects(address):
    """What a TCP connection to `address` came to: `None` once it opened, else the error."""
    try:
        with socket.create_connection(address, timeout=TIMEOUT_SECONDS):
            return None
    except OSError as error:
        return error


def sandboxd():
    """The arguments of `sandboxd`, the sandbox's supervisor, as its process shows them."""
    for pid in filter(str.isdigit, os.listdir("/proc")):
        try:
            with open("/proc/{}/cmdline".format(pid), "rb") as file:
                words = file.read().decode(errors="replace").split("\0")
        except OSError:
            continue
        if words and words[0].endswith("/sandboxd"):
            return words[1:]
    return None


def gateway(sandbox, words):
    if words is None:
        line(sandbox, "gateway", False, "no sandboxd process found")
    elif "--listen" in words:
        line(sandbox, "gateway", True, "sandboxd listens at {}, and the gateway dialed it".format(words[0]))
    elif words[0].startswith("/"):
        # A socket that the gateway removes once a sandbox has dialed it.
        line(sandbox, "gateway", True, "sandboxd dialed the gateway's socket {}".format(words[0]))
    else:
        host, _, port = words[0].rpartition(":")
        error = connects((host, int(port)))
        line(sandbox, "gateway", error is None, "connect {}: {}".format(words[0], error or "opened"))


def main():
    sandbox = sys.argv[1]
    words = sandboxd()
    listens = words is not None and "--listen" in words
    uid = os.getuid()
    line(sandbox, "user", uid != 0, "uid {}".format(uid))
    for check, address in (("metadata", METADATA), ("public", PUBLIC)):
        error = connects(address)
        seen = "connect {}:{}: {}".format(*address, error or "opened")
        line(sandbox, check, error is not None, seen)
    try:
        found = socket.getaddrinfo(NAME, 443)
        line(sandbox, "dns", listens, "{} resolved to {}".format(NAME, found[0][4][0]))
    except OSError as error:
        line(sandbox, "dns", not listens, "{}: {}".format(NAME, error))
    gateway(sandbox, words)


if __name__ == "__main__":
    main()
