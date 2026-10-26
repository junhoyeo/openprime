#!/usr/bin/env python3
"""Run a test or gate command in a hermetic environment.

Use this on any machine that has a live Prime Agent install. Rust kernel paths
resolve from HOME (`kernel_venv_dir`), and many e2e helpers fall back to
`$HOME/.prime/agent/kernel-venv/bin/python` when no test interpreter is given.
A plain `cargo test` with the real HOME can therefore start or rebuild the
live kernel venv and write live agent state.

This wrapper runs the command under `env -i` with a fresh short root in /tmp:
synthetic HOME/TMPDIR/XDG_DATA_HOME, PRIME_AGENT_KERNEL_VENV inside the root,
and an explicit test kernel interpreter. On macOS the command also runs under a
sandbox profile that denies outbound network except loopback and Unix sockets. Agent, session, and socket paths are
not forced, so tests keep their own per-test roots. After the command, it
reports any surviving process that references the root or the Cargo target
directory, and exits non-zero if one exists. It never kills processes and
never deletes the root (it is evidence).

Usage:
    scripts/hermetic_test.py --kernel-python PATH -- cargo test -p pa-daemon
    scripts/hermetic_test.py -- make check   # uses PA_E2E_KERNEL_PYTHON
"""
import argparse
import platform
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

GRACE_SECONDS = 60
PASS_THROUGH = ("PATH", "LANG", "LC_ALL", "TZ", "TERM", "CARGO_HOME", "RUSTUP_HOME",
                "CARGO_TARGET_DIR", "CARGO_BUILD_JOBS", "CARGO_INCREMENTAL", "CARGO_NET_OFFLINE")
KERNEL_VARS = ("PRIME_AGENT_KERNEL_PYTHON", "PA_CORE_KERNEL_PYTHON",
               "PA_E2E_KERNEL_PYTHON", "PA_TEST_RUNTIME_ABI_PYTHON")


def real_home():
    return Path(os.environ.get("HOME") or Path.home()).resolve()


def resolve_kernel_python(explicit, environ):
    raw = explicit or environ.get("PA_E2E_KERNEL_PYTHON")
    if not raw:
        raise SystemExit("hermetic_test: pass --kernel-python or set PA_E2E_KERNEL_PYTHON "
                         "(an interpreter with prime-agent-runtime installed); no fallback is used")
    path = Path(raw)
    if not path.exists():
        raise SystemExit(f"hermetic_test: kernel python {path} does not exist")
    live = real_home() / ".prime"
    if path.resolve().is_relative_to(live.resolve()) or path.absolute().is_relative_to(live):
        raise SystemExit(f"hermetic_test: refusing the live install interpreter {path}; "
                         "use a separate test runtime")
    return str(path)


def build_env(root, kernel_python, environ):
    env = {key: environ[key] for key in PASS_THROUGH if environ.get(key)}
    home = real_home()
    env.setdefault("CARGO_HOME", str(home / ".cargo"))
    env.setdefault("RUSTUP_HOME", str(home / ".rustup"))
    env.setdefault("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
    env.update({
        "HOME": str(root / "h"), "TMPDIR": str(root / "t"), "XDG_DATA_HOME": str(root / "x"),
        "XDG_CONFIG_HOME": str(root / "c"), "XDG_CACHE_HOME": str(root / "cache"),
        "PRIME_AGENT_KERNEL_VENV": str(root / "k"),
        "PYTHONNOUSERSITE": "1", "PYTHONDONTWRITEBYTECODE": "1",
        "PYTHONPYCACHEPREFIX": str(root / "pycache"),
        "NO_COLOR": "1",
    })
    env.setdefault("TERM", "xterm-256color")
    for key in KERNEL_VARS:
        env[key] = kernel_python
    return env


# macOS: deny outbound network except loopback and Unix sockets, so tests that would
# reach a real endpoint (telemetry, catalogs) fail loudly instead of egressing.
# DNS through mDNSResponder stays allowed for localhost resolution.
EGRESS_PROFILE = """(version 1)
(allow default)
(deny network-outbound)
(allow network-outbound (remote ip "localhost:*"))
(allow network-outbound (remote unix-socket))
(allow network-outbound (literal "/private/var/run/mDNSResponder"))
"""


def sandboxed(command, root):
    if platform.system() != "Darwin":
        return command
    profile = root / "deny-egress.sb"
    profile.write_text(EGRESS_PROFILE)
    return ["/usr/bin/sandbox-exec", "-f", str(profile), *command]


def make_root():
    # Short path: AF_UNIX socket paths under TMPDIR must stay below the OS limit.
    # Canonical root (/private/tmp on macOS): a symlinked TMPDIR makes tests that
    # compare canonicalized paths with raw tempdir paths fail for harness reasons.
    root = Path(os.path.realpath(tempfile.mkdtemp(prefix="pa-h", dir="/tmp")))
    for name in ("h", "t", "x", "c", "cache", "pycache"):
        (root / name).mkdir()
    return root


def leftovers(markers):
    """Processes whose EXECUTABLE (argv[0]) lives under a marker path: the run's
    root, the Cargo target directory, or the test kernel interpreter. Matching
    argv[0] only keeps unrelated shells whose arguments merely mention a path,
    and this wrapper's own ancestors, out of the report."""
    out = subprocess.run(["ps", "-axo", "pid=,ppid=,command="], capture_output=True, text=True,
                         timeout=30, check=False).stdout
    rows, parents = [], {}
    for line in out.splitlines():
        fields = line.split(None, 2)
        if len(fields) < 3 or not fields[0].isdigit() or not fields[1].isdigit():
            continue
        pid, ppid, command = int(fields[0]), int(fields[1]), fields[2]
        parents[pid] = ppid
        rows.append((pid, command))
    ancestors, cursor = set(), os.getpid()
    while cursor and cursor not in ancestors:
        ancestors.add(cursor)
        cursor = parents.get(cursor, 0)
    found = []
    for pid, command in rows:
        executable = command.split(" ", 1)[0]
        if pid not in ancestors and any(executable == m or executable.startswith(m.rstrip("/") + "/")
                                        for m in markers if m):
            found.append(f"{pid} {command}")
    return found


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--kernel-python")
    parser.add_argument("--allow-egress", action="store_true",
                        help="do not wrap the command in the macOS loopback-only network sandbox")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("missing command")
    kernel_python = resolve_kernel_python(args.kernel_python, os.environ)
    root = make_root()
    env = build_env(root, kernel_python, os.environ)
    executable = shutil.which(command[0], path=env["PATH"]) or command[0]
    print(f"hermetic_test: root {root}", file=sys.stderr)
    argv = [executable, *command[1:]]
    if not args.allow_egress:
        argv = sandboxed(argv, root)
    code = subprocess.run(argv, env=env, check=False).returncode
    # The kernel interpreter marker catches orphaned `python -m rlm.repl` kernels, whose
    # command lines name neither the root nor the target. Another live user of the same
    # test runtime would also match; the report lists exact PIDs for review.
    markers = [str(root), os.path.realpath(root), kernel_python]
    if env.get("CARGO_TARGET_DIR"):
        markers.append(env["CARGO_TARGET_DIR"])
    else:
        markers.append(str(Path.cwd() / "target"))
    # Test workers that lose their supervisor exit on their own after a bounded
    # window (WORKER_SUPERVISOR_LOST_EXIT_MS in e2e helpers); report only what is
    # still alive once that observable condition has had time to settle.
    deadline = time.monotonic() + GRACE_SECONDS
    survivors = leftovers(markers)
    while survivors and time.monotonic() < deadline:
        time.sleep(1)
        survivors = leftovers(markers)
    if survivors:
        print("hermetic_test: processes survived the command (not killed):", file=sys.stderr)
        for row in survivors:
            print("  " + row, file=sys.stderr)
        return code or 3
    print(f"hermetic_test: exit {code}; no surviving processes; root kept at {root}", file=sys.stderr)
    return code


if __name__ == "__main__":
    sys.exit(main())
