#!/usr/bin/env python3
"""Stdlib-only OS-fence canary, invoked by hermetic_test.py before the subject.

Only synthetic files/listeners are touched. No real protected service is contacted.
UDP connect checks do not send a DNS query or external packet.
"""
import errno
import json
from pathlib import Path
import socket
import sys


def denied(action, label):
    try:
        action()
    except OSError as error:
        if error.errno not in (errno.EPERM, errno.EACCES):
            raise AssertionError(f"{label}: expected policy denial, got {error}") from error
    else:
        raise AssertionError(f"{label}: policy did not deny operation")


def main():
    root, forbidden, private = map(Path, sys.argv[1:4])
    port = int(sys.argv[4])
    allowed = root / "canary-allowed"
    allowed.write_text("allowed")
    assert allowed.read_text() == "allowed"
    denied(lambda: forbidden.write_text("corrupted"), "outside-root write")
    alias = root / "canary-alias"
    alias.symlink_to(forbidden)
    denied(lambda: alias.write_text("corrupted"), "symlink write")
    denied(lambda: private.write_text("corrupted"), "protected write")
    denied(private.read_text, "protected read")
    checks = ["owned-write", "outside-write-denied", "symlink-write-denied",
              "protected-write-denied", "protected-read-denied"]
    # Parent holds an actual listener at this dynamically allocated protected port.
    # Check every IP spelling against that port, NEVER the real gateway.
    for host in ("127.0.0.1", "::1", "::ffff:127.0.0.1", "localhost"):
        denied(lambda: socket.create_connection((host, port), timeout=2).close(),
               f"protected port {host}")
        checks.append(f"protected-port-{host}")
    # UDP connect performs policy enforcement without sending a packet.
    for host, target_port in (("127.0.0.1", 53), ("192.0.2.1", 53), ("192.0.2.1", 443)):
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
            denied(lambda: sock.connect((host, target_port)), f"outbound {host}:{target_port}")
        checks.append(f"udp-denied-{host}:{target_port}")
    with socket.socket() as server:
        server.bind(("127.0.0.1", 0))
        server.listen()
        with socket.create_connection(server.getsockname(), timeout=2):
            connection, _ = server.accept()
            connection.close()
    checks.append("dynamic-loopback-mock")
    with socket.socket(socket.AF_UNIX) as server:
        server.bind(str(root / "canary.sock"))
        server.listen()
        with socket.socket(socket.AF_UNIX) as client:
            client.connect(str(root / "canary.sock"))
            connection, _ = server.accept()
            connection.close()
    checks.append("owned-unix-mock")
    print(json.dumps({"checks": checks, "count": len(checks)}))


if __name__ == "__main__":
    main()
