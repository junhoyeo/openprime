#!/usr/bin/env python3
"""Run a test or gate command in a hermetic environment.

Use this on any machine that has a live Prime Agent install. Rust kernel paths
resolve from HOME (`kernel_venv_dir`), and many e2e helpers fall back to
`$HOME/.prime/agent/kernel-venv/bin/python` when no test interpreter is given.
A plain `cargo test` with the real HOME can therefore start or rebuild the
live kernel venv and write live agent state.

This wrapper runs the command under `env -i` with a fresh short root in /tmp:
synthetic HOME/TMPDIR/XDG_DATA_HOME,
and an explicit test kernel interpreter. On macOS the command also runs under a
mandatory sandbox that denies filesystem writes outside the run root and private
Cargo target. It permits loopback mocks except protected ports (53, 8787, 8791, 18789), and
Unix sockets only under the run root. Direct external IP/DNS traffic is denied. Agent, session, and socket paths are
not forced, so tests keep their own per-test roots. After the command, it
compares precise same-UID process identities before and after the subject. Any
new unexplained process makes the run fail (unknown ownership, never a kill target). It only terminates/reaps its exact Popen-owned child on failure; it never signals
discovered PIDs and never deletes the root (it is evidence).

Usage:
    scripts/hermetic_test.py --kernel-python PATH -- cargo test -p pa-daemon
    scripts/hermetic_test.py -- make check   # uses PA_E2E_KERNEL_PYTHON
"""
import argparse
import json
import math
import pwd
import platform
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time

from hermetic_control import ControlDirectory, write_receipt
from hermetic_processes import ProcessInventory, new_processes, reap_owned

PASS_THROUGH = ("PATH", "LANG", "LC_ALL", "TZ", "TERM", "CARGO_HOME", "RUSTUP_HOME",
                "CARGO_TARGET_DIR", "CARGO_BUILD_JOBS", "CARGO_INCREMENTAL", "CARGO_NET_OFFLINE",
                "RUST_TEST_THREADS")
KERNEL_VARS = ("PRIME_AGENT_KERNEL_PYTHON", "PA_CORE_KERNEL_PYTHON",
               "PA_E2E_KERNEL_PYTHON", "PA_TEST_RUNTIME_ABI_PYTHON")


def real_home():
    return Path(pwd.getpwuid(os.getuid()).pw_dir).resolve()


def resolve_kernel_python(explicit, environ):
    raw = explicit or environ.get("PA_E2E_KERNEL_PYTHON")
    if not raw:
        raise SystemExit("hermetic_test: pass --kernel-python or set PA_E2E_KERNEL_PYTHON "
                         "(an interpreter with prime-agent-runtime installed); no fallback is used")
    path = Path(raw)
    if not path.exists():
        raise SystemExit(f"hermetic_test: kernel python {path} does not exist")
    for live in (real_home() / ".prime", real_home() / ".prime-rust"):
        if path.resolve().is_relative_to(live.resolve()) or path.absolute().is_relative_to(live):
            raise SystemExit(f"hermetic_test: refusing the live install interpreter {path}; "
                             "use a separate test runtime")
    # Keep the lexical venv path: resolving a venv symlink here would lose sys.prefix.
    return str(path.absolute())


def build_env(root, kernel_python, environ):
    env = {key: environ[key] for key in PASS_THROUGH if environ.get(key)}
    home = real_home()
    env.setdefault("CARGO_HOME", str(home / ".cargo"))
    env.setdefault("RUSTUP_HOME", str(home / ".rustup"))
    env.setdefault("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
    env.update({
        "HOME": str(root / "h"), "TMPDIR": str(root / "t"), "XDG_DATA_HOME": str(root / "x"),
        "XDG_CONFIG_HOME": str(root / "c"), "XDG_CACHE_HOME": str(root / "cache"),
        "PYTHONNOUSERSITE": "1", "PYTHONDONTWRITEBYTECODE": "1",
        "PYTHONPYCACHEPREFIX": str(root / "pycache"),
        "NO_COLOR": "1",
    })
    env.setdefault("TERM", "xterm-256color")
    for key in KERNEL_VARS:
        env[key] = kernel_python
    return env


# ps is a fixed, OS-supplied, read-only process inspector. macOS refuses to exec
# this setuid binary in a sandbox. This is the ONLY no-sandbox executable; it
# cannot launch user commands. Product identity probes depend on its start time.
# This fence blocks external IPs and protected ports, NOT every local service:
# dynamic loopback mocks are supported, so an unknown local proxy remains outside
# this guarantee. Proxy credentials/settings never enter build_env.
def sandbox_profile(root, *, writable=(), protected=(), protected_ports=(8787, 8791, 18789)):
    quote = lambda value: json.dumps(str(value))
    paths = [Path(root).resolve(), *(Path(p).resolve() for p in writable)]
    private = [real_home() / ".prime", real_home() / ".prime-rust", *map(Path, protected)]
    lines = ["(version 1)", "(allow default)", "(deny file-write*)"]
    for path in paths:
        lines.append(f"(allow file-write* (subpath {quote(path)}))")
    # PTYs and null are device I/O, not a writable filesystem escape.
    lines.extend(['(allow file-write* (literal "/dev/null") (literal "/dev/tty") '
                  '(regex #"^/dev/ttys[0-9]+$"))'])
    for path in private:
        for spelling in {path.absolute(), path.resolve()}:
            lines.append(f"(deny file-read* file-write* (subpath {quote(spelling)}))")
    lines.extend([
        "(deny network-outbound)",
        '(allow network-outbound (remote ip "localhost:*"))',
        # Restrict Unix sockets to the owned run root (not mDNSResponder, Docker,
        # the live daemon, or an arbitrary pre-existing local service).
        f"(allow network-outbound (remote unix-socket (subpath {quote(paths[0])})))",
        '(deny mach-lookup (global-name "com.apple.mDNSResponder"))',
    ])
    for port in sorted({53, 8787, 8791, 18789, *protected_ports}):
        if not 1 <= port <= 65535:
            raise ValueError(f"invalid protected port: {port}")
        lines.append(f'(deny network-outbound (remote ip "*:{port}"))')
    lines.append('(allow process-exec (literal "/bin/ps") (with no-sandbox))')
    return "\n".join(lines) + "\n"


def sandboxed(command, profile):
    if platform.system() != "Darwin" or not Path("/usr/bin/sandbox-exec").is_file():
        raise SystemExit("hermetic_test: enforced filesystem/network sandbox unavailable; refusing to run")
    return ["/usr/bin/sandbox-exec", "-f", str(profile), *command]


def make_root():
    # Short path: AF_UNIX socket paths under TMPDIR must stay below the OS limit.
    # Canonical root (/private/tmp on macOS): a symlinked TMPDIR makes tests that
    # compare canonicalized paths with raw tempdir paths fail for harness reasons.
    root = Path(os.path.realpath(tempfile.mkdtemp(prefix="pa-h", dir="/tmp")))
    for name in ("h", "t", "x", "c", "cache", "pycache"):
        (root / name).mkdir()
    return root




def private_target(environ):
    raw = environ.get("CARGO_TARGET_DIR")
    if not raw:
        raise SystemExit("hermetic_test: CARGO_TARGET_DIR must name an existing owned /private/tmp/pa-*/target")
    path = Path(raw)
    temporary = Path("/tmp").resolve()
    if (not path.is_absolute() or path != path.resolve() or not path.is_dir()
            or path.name != "target" or path.parent.parent != temporary
            or not path.parent.name.startswith("pa-")):
        raise SystemExit(f"hermetic_test: target must be canonical {temporary}/pa-*/target: {path}")
    for directory in (path.parent, path):
        stat = directory.stat()
        if stat.st_uid != os.getuid() or stat.st_mode & 0o022:
            raise SystemExit(f"hermetic_test: target directory is not private/owned: {directory}")
    return path




def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--kernel-python")
    parser.add_argument("--timeout", type=float, default=1800,
                        help="command deadline in seconds (default: 1800)")
    parser.add_argument("--protect-port", type=int, action="append", default=[],
                        help="deny an additional local service port (8787, 8791, 18789 and DNS are always denied)")
    parser.add_argument("--do-not-track", action="store_true",
                        help="set DO_NOT_TRACK=1 for the command (some telemetry tests then fail by design)")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("missing command")
    # DO_NOT_TRACK is not a security boundary and cannot replace the OS fence.
    if platform.system() != "Darwin":
        parser.error("enforced filesystem/network sandbox unavailable; refusing to run")
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("--timeout must be finite and positive")
    target = private_target(os.environ)
    kernel_python = resolve_kernel_python(args.kernel_python, os.environ)
    root = make_root()
    env = build_env(root, kernel_python, os.environ)
    if args.do_not_track:
        env["DO_NOT_TRACK"] = "1"
    executable = shutil.which(command[0], path=env["PATH"]) or command[0]
    print(f"hermetic_test: root {root}", file=sys.stderr)
    argv = [executable, *command[1:]]
    env["CARGO_TARGET_DIR"] = str(target)
    # A separate fresh sibling is NEVER in the subject's writable allowlist.
    # Keep its directory fd open so even ancestor replacement cannot redirect a
    # controller receipt. No descriptor for this directory reaches the subject.
    control_path = Path(tempfile.mkdtemp(prefix="pa-control-", dir="/tmp")).resolve()
    print(f"hermetic_test: control {control_path}", file=sys.stderr)
    with ControlDirectory(control_path) as control:
        forbidden_fd, forbidden_name = tempfile.mkstemp(prefix="pa-forbidden-", dir="/tmp")
        with os.fdopen(forbidden_fd, "w") as stream:
            stream.write("preserve")
        forbidden = Path(forbidden_name).resolve()
        private = root / "private-canary"
        private.write_text("synthetic secret")
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen()
            port = listener.getsockname()[1]
            profile = sandbox_profile(root, writable=(target,), protected=(private,),
                                      protected_ports=[*args.protect_port, port])
            control.write_text("isolation.sb", profile)
            profile_path = control.path / "isolation.sb"
            preflight = [kernel_python, str(Path(__file__).with_name("hermetic_preflight.py")),
                         str(root), str(forbidden), str(private), str(port), str(control.path)]
            result = subprocess.run(sandboxed(preflight, profile_path), env=env,
                                    text=True, capture_output=True, timeout=20, check=False)
            write_receipt(control, "preflight.json", {"exit": result.returncode,
                "stdout": result.stdout, "stderr": result.stderr, "forbidden": str(forbidden),
                "control_identity": control.identity, "root": str(root)})
            if result.returncode or forbidden.read_text() != "preserve" or private.read_text() != "synthetic secret":
                print(f"hermetic_test: OS-fence preflight failed: {result.stderr}", file=sys.stderr)
                return result.returncode or 4
            print(f"hermetic_test: OS-fence preflight {result.stdout.strip()}", file=sys.stderr)
            argv = sandboxed(argv, profile_path)
        return run_subject(argv, env, root, target, args.timeout, control=control)


def run_subject(argv, env, root, target, timeout, *, control, inventory=None):
    """Execute and account for one subject. Tests inject only the inventory seam."""
    child = None
    output = None
    before = None
    survivors = None
    timed_out = False
    failure = None
    cleanup_failure = None
    try:
        inventory = inventory or ProcessInventory()
        before = inventory.snapshot()
        write_receipt(control, "process-baseline.json", list(before.values()))
        # Open the owned log before launch; stdout/stderr never name a path the
        # subject can replace. Directory/receipt descriptors remain CLOEXEC.
        output = control.create_file("subject.log")
        child = subprocess.Popen(argv, env=env, start_new_session=True,
                                 stdout=output, stderr=subprocess.STDOUT)
        identity = inventory.identity(child.pid)
        if identity is None and child.poll() is None:
            raise RuntimeError("child identity disappeared while the child is still live")
        receipt = {"pid": child.pid, "identity": identity, "command": argv,
                   "root": str(root), "target": str(target), "started_monotonic": time.monotonic(),
                   "control": str(control.path), "control_identity": control.identity}
        write_receipt(control, "command.json", receipt)
        try:
            child.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            print("hermetic_test: command deadline exceeded", file=sys.stderr)
    except BaseException as error:
        failure = error
    finally:
        try:
            if child is not None:
                reap_owned(child)
        except BaseException as error:
            cleanup_failure = error
        try:
            if before is not None:
                after = inventory.snapshot()
                survivors = new_processes(before, after)
                write_receipt(control, "process-after.json", list(after.values()))
        except BaseException as error:
            if failure is None:
                failure = error
        subject_exit = child.returncode if child is not None else None
        if failure is not None or cleanup_failure is not None:
            decision = "error"
            code = 130 if isinstance(failure or cleanup_failure, KeyboardInterrupt) else 1
        elif timed_out:
            decision, code = "timeout", 124
        elif subject_exit is None or survivors is None:
            decision, code = "unresolved", 4
        elif survivors:
            decision, code = "unknown_processes", subject_exit or 3
        else:
            decision, code = "subject_exit", subject_exit
        outcome = {"subject_exit_code": subject_exit, "timed_out": timed_out,
                   "wrapper_decision": decision, "wrapper_exit_code": code & 255,
                   "wrapper_return_code": code,
                   "failure_type": type(failure).__name__ if failure is not None else None,
                   "cleanup_failure_type": type(cleanup_failure).__name__ if cleanup_failure is not None else None,
                   "direct_child_reaped": child is not None and child.returncode is not None,
                   "survivors": survivors, "control_identity": control.identity}
        try:
            # One immutable final receipt per run; never overwrite earlier evidence.
            write_receipt(control, "outcome.json", outcome)
        except BaseException as receipt_error:
            outcome["planned_wrapper_decision"] = outcome["wrapper_decision"]
            outcome["wrapper_decision"] = "outcome_receipt_error"
            outcome["wrapper_exit_code"] = 130 if isinstance(receipt_error, KeyboardInterrupt) else 1
            outcome["wrapper_return_code"] = outcome["wrapper_exit_code"]
            outcome["outcome_receipt_failure_type"] = type(receipt_error).__name__
            print("hermetic_test: outcome receipt write failed; fallback " + json.dumps(outcome), file=sys.stderr)
            raise
        finally:
            if output is not None:
                try:
                    output.flush()
                    output.seek(0, os.SEEK_END)
                    # Keep the full log; mirror a bounded tail after the subject.
                    output.seek(max(0, output.tell() - 128 * 1024))
                    sys.stdout.write(output.read().decode("utf-8", errors="replace"))
                    sys.stdout.flush()
                finally:
                    output.close()
    print(f"hermetic_test: subject exit {subject_exit}; wrapper decision {decision} ({code})", file=sys.stderr)
    if failure is not None:
        raise failure
    if cleanup_failure is not None:
        raise cleanup_failure
    if survivors:
        print("hermetic_test: new same-UID processes remain (ownership UNKNOWN; not killed):", file=sys.stderr)
        for identity in survivors:
            print("  " + json.dumps(identity, sort_keys=True), file=sys.stderr)
        return code
    print(f"hermetic_test: exit {code}; no new same-UID processes; root kept at {root}", file=sys.stderr)
    return code


if __name__ == "__main__":
    sys.exit(main())
