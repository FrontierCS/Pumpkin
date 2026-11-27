"""Prints, for the sandbox it runs in, one line for each way out that its confinement must close
or keep open, `<sandbox> <check>: ok|FAILED (<what it saw>)`, where `ok` says the sandbox is as
it should be: its role's user isn't root; the instance metadata address and a public address
can't be reached; DNS can't be either, but where `sandboxd` listens, as on Modal, whose
sandboxes reach Modal's resolver and nothing else, so a name resolves there; the gateway
can, as `sandboxd` holds the session that this probe runs over; and where the gateway's address
is one that several sandboxes dial, as on EC2, the gateway refuses a hello that presents an
attestation of no sandbox's, as this sandbox's code can make."""

import json
import os
import socket
import struct
import sys

METADATA = ("169.254.169.254", 80)
PUBLIC = ("1.1.1.1", 443)
NAME = "example.com"
TIMEOUT_SECONDS = 5
# The sandbox protocol's version, `schema::sandbox::PROTOCOL_VERSION`, so that the gateway reads the
# attestation rather than refuse the hello for its version.
PROTOCOL_VERSION = 4


def line(sandbox, check, ok, seen):
    print(
        "{} {}: {} ({})".format(sandbox, check, "ok" if ok else "FAILED", seen),
        flush=True,
    )


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
            with open(f"/proc/{pid}/cmdline", "rb") as file:
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
        line(
            sandbox,
            "gateway",
            True,
            f"sandboxd listens at {words[0]}, and the gateway dialed it",
        )
    elif words[0].startswith("/"):
        # A socket that the gateway removes once a sandbox has dialed it.
        line(
            sandbox, "gateway", True, f"sandboxd dialed the gateway's socket {words[0]}"
        )
    else:
        host, _, port = words[0].rpartition(":")
        error = connects((host, int(port)))
        line(
            sandbox,
            "gateway",
            error is None,
            "connect {}: {}".format(words[0], error or "opened"),
        )


def frame(message, body=b""):
    """A frame of the sandbox protocol: a message's length and its JSON, then a body's."""
    encoded = json.dumps(message).encode()
    return struct.pack(">I", len(encoded)) + encoded + struct.pack(">I", len(body)) + body


def exactly(connection, size):
    read = b""
    while len(read) < size:
        more = connection.recv(size - len(read))
        if not more:
            raise OSError("the gateway closed the connection")
        read += more
    return read


def answer(address, attestation):
    """The gateway's answer to a hello that presents `attestation`, or the error that ended it."""
    try:
        with socket.create_connection(address, timeout=TIMEOUT_SECONDS) as connection:
            connection.sendall(frame({"version": PROTOCOL_VERSION}, attestation))
            (size,) = struct.unpack(">I", exactly(connection, 4))
            return json.loads(exactly(connection, size))
    except (OSError, ValueError) as error:
        return error


def forged(sandbox, words):
    check = "forged hello"
    if words is None:
        line(sandbox, check, False, "no sandboxd process found")
    elif "--listen" in words or words[0].startswith("/"):
        line(sandbox, check, True, "no address that several sandboxes share to say it at")
    else:
        host, _, port = words[0].rpartition(":")
        # An identity document that AWS never signed.
        document = json.dumps({"instanceId": "i-0123456789abcdef0"})
        attestation = json.dumps({"document": document, "signature": "AAAA"}).encode()
        answered = answer((host, int(port)), attestation)
        # Refused for its attestation, not for its version or its form, which would leave the
        # check of the attestation unprobed.
        refused = (
            isinstance(answered, dict)
            and answered.get("type") == "refused"
            and "attestation" in answered.get("message", "")
        )
        # The refusal's reason alone, which names no address of the instance's.
        seen = answered.get("message") if isinstance(answered, dict) else answered
        line(sandbox, check, refused, f"the gateway answered: {seen}")


def main():
    sandbox = sys.argv[1]
    words = sandboxd()
    listens = words is not None and "--listen" in words
    uid = os.getuid()
    line(sandbox, "user", uid != 0, f"uid {uid}")
    for check, address in (("metadata", METADATA), ("public", PUBLIC)):
        error = connects(address)
        seen = "connect {}:{}: {}".format(*address, error or "opened")
        line(sandbox, check, error is not None, seen)
    try:
        found = socket.getaddrinfo(NAME, 443)
        line(sandbox, "dns", listens, f"{NAME} resolved to {found[0][4][0]}")
    except OSError as error:
        line(sandbox, "dns", not listens, f"{NAME}: {error}")
    gateway(sandbox, words)
    forged(sandbox, words)


if __name__ == "__main__":
    main()
