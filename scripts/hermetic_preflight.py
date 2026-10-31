#!/usr/bin/env python3
"""Stdlib-only OS-fence canary, invoked by hermetic_test.py before the subject.

Only synthetic files/listeners are touched. No real protected service is contacted.
UDP connect checks do not send a DNS query or external packet.
"""
import ctypes
import errno
import json
import os
from pathlib import Path
import socket
import subprocess
import sys


def denied(action, label):
    try:
        action()
    except OSError as error:
        if error.errno not in (errno.EPERM, errno.EACCES):
            raise AssertionError(f"{label}: expected policy denial, got {error}") from error
    else:
        raise AssertionError(f"{label}: policy did not deny operation")


def sandbox_checker():
    lib = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
    check = lib.sandbox_check
    check.restype = ctypes.c_int
    check.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int]
    no_report = ctypes.c_int.in_dll(lib, "SANDBOX_CHECK_NO_REPORT").value

    def run(operation, filter_type, argument):
        ctypes.set_errno(0)
        result = check(os.getpid(), operation.encode(), filter_type | no_report, argument.encode())
        if result not in (0, 1):
            raise AssertionError(f"sandbox_check {operation} {argument}: unusable result {result} "
                                 f"errno {ctypes.get_errno()}")
        return result
    return lib, run


def trash_checks(spec, root):
    """Fail closed unless every trash-guard rule is attributably in force."""
    lib, check = sandbox_checker()
    path, global_name, appleevent_destination = 1, 2, 4
    checks = []

    def expect(label, value, wanted):
        if value != wanted:
            raise AssertionError(f"{label}: expected {wanted}, got {value}")
        checks.append(label)

    # Validated form (discovery run3/run4): process-exec* with the path filter.
    expect("exec-check-allowed-control", check("process-exec*", path, "/bin/ls"), 0)
    # The preflight cannot write CONTROL, so its owned canary lives in ROOT.
    canary_dir = root / f"trash-canary-{spec['nonce']}"
    canary_dir.mkdir()
    canary = canary_dir / "trash"
    marker = canary_dir / "ran"
    canary.write_text(f"#!/bin/sh\nprintf ran > {marker}\n")
    canary.chmod(0o700)
    expect("exec-check-denied-control", check("process-exec*", path, str(canary)), 1)
    expect("exec-check-fake-allowed", check("process-exec*", path, spec["fake"]), 0)
    for real in spec["real_trash"]:
        expect(f"exec-check-real-denied:{real}", check("process-exec*", path, real), 1)
    try:
        subprocess.run([str(canary)], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL, timeout=5, check=False)
    except PermissionError:
        pass
    else:
        raise AssertionError("owned trash canary exec was not denied")
    if marker.exists():
        raise AssertionError("owned trash canary body executed")
    checks.append("exec-denied-owned-canary")
    # Positive control in the same form: an owned ROOT path must be allowed.
    expect("metadata-check-allowed-control", check("file-read-metadata", path, str(canary_dir)), 0)
    for spelling in spec["owner_trash"]:
        expect(f"owner-trash-metadata-denied:{spelling}",
               check("file-read-metadata", path, f"{spelling}/absent-{spec['nonce']}"), 1)
    expect("appleevent-send-finder-denied",
           check("appleevent-send", appleevent_destination, "com.apple.finder"), 1)
    lookup = lib.bootstrap_look_up
    lookup.restype = ctypes.c_int
    lookup.argtypes = [ctypes.c_uint, ctypes.c_char_p, ctypes.POINTER(ctypes.c_uint)]
    bootstrap_port = ctypes.c_uint.in_dll(lib, "bootstrap_port").value
    deallocate = lib.mach_port_deallocate
    deallocate.argtypes = [ctypes.c_uint, ctypes.c_uint]
    task_self = ctypes.c_uint.in_dll(lib, "mach_task_self_").value
    # The FileProvider regex row: a concrete sub-service name must be denied by the regex.
    for name in spec["regex_probes"]:
        expect(f"mach-regex-check:{name}", check("mach-lookup", global_name, name), 1)
    for service in (*spec["services"], "com.apple.system.logger"):
        allowed_control = service == "com.apple.system.logger"
        expect(f"mach-check:{service}", check("mach-lookup", global_name, service), 0 if allowed_control else 1)
        port = ctypes.c_uint(0)
        code = lookup(bootstrap_port, service.encode(), ctypes.byref(port))
        if port.value:
            deallocate(task_self, port.value)
        if allowed_control:
            expect("mach-live-control-allowed", (code, bool(port.value)), (0, True))
        else:
            # 1100 = BOOTSTRAP_NOT_PRIVILEGED; no message is ever sent.
            expect(f"mach-live-denied:{service}", (code, bool(port.value)), (1100, False))
    return checks


def main():
    root, forbidden, private = map(Path, sys.argv[1:4])
    port = int(sys.argv[4])
    control = Path(sys.argv[5])
    trash_spec = json.loads(sys.argv[6])
    allowed = root / "canary-allowed"
    allowed.write_text("allowed")
    assert allowed.read_text() == "allowed"
    denied(lambda: forbidden.write_text("corrupted"), "outside-root write")
    alias = root / "canary-alias"
    alias.symlink_to(forbidden)
    denied(lambda: alias.write_text("corrupted"), "symlink write")
    denied(lambda: private.write_text("corrupted"), "protected write")
    denied(private.read_text, "protected read")
    denied(lambda: (control / "command.json").write_text("forged"), "controller metadata create")
    denied(lambda: (control / "isolation.sb").write_text("forged"), "controller policy overwrite")
    denied(lambda: control.rename(root / "stolen-control"), "controller ancestor rename")
    denied(lambda: (control / "preflight.json").symlink_to(forbidden), "controller symlink create")
    denied(lambda: (control / "isolation.sb").unlink(), "controller policy unlink")
    checks = ["owned-write", "outside-write-denied", "symlink-write-denied",
              "protected-write-denied", "protected-read-denied",
              "metadata-create-denied", "metadata-overwrite-denied", "metadata-ancestor-rename-denied",
              "metadata-symlink-denied", "metadata-unlink-denied"]
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
    checks += trash_checks(trash_spec, root)
    print(json.dumps({"checks": checks, "count": len(checks)}))


if __name__ == "__main__":
    main()
