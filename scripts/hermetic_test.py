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
Cargo target. It permits loopback mocks except protected ports (8787, 53), and
Unix sockets only under the run root. Direct external IP/DNS traffic is denied. Agent, session, and socket paths are
not forced, so tests keep their own per-test roots. After the command, it
reports any surviving process that references the root or the Cargo target
directory, and exits non-zero if one exists. It never kills processes and
never deletes the root (it is evidence).

Usage:
    scripts/hermetic_test.py --kernel-python PATH -- cargo test -p pa-daemon
    scripts/hermetic_test.py -- make check   # uses PA_E2E_KERNEL_PYTHON
"""
import argparse
import json
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

GRACE_SECONDS = 60
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
def sandbox_profile(root, *, writable=(), protected=(), protected_ports=(8787,)):
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
    for port in sorted({53, 8787, *protected_ports}):
        if not 1 <= port <= 65535:
            raise ValueError(f"invalid protected port: {port}")
        lines.append(f'(deny network-outbound (remote ip "*:{port}"))')
    lines.append('(allow process-exec (literal "/bin/ps") (with no-sandbox))')
    return "\n".join(lines) + "\n"


def sandboxed(command, root, *, writable=(), protected=(), protected_ports=(8787,)):
    if platform.system() != "Darwin" or not Path("/usr/bin/sandbox-exec").is_file():
        raise SystemExit("hermetic_test: enforced filesystem/network sandbox unavailable; refusing to run")
    profile = root / "isolation.sb"
    profile.write_text(sandbox_profile(root, writable=writable, protected=protected,
                                       protected_ports=protected_ports))
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
    result = subprocess.run(["/bin/ps", "-axo", "pid=,ppid=,command="], capture_output=True,
                            text=True, timeout=30, check=False)
    if result.returncode:
        raise RuntimeError(f"process inspection failed ({result.returncode}): {result.stderr}")
    out = result.stdout
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
                        help="deny an additional local service port (8787 and DNS are always denied)")
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
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
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
    # The same profile fences the canary and the actual command. Never test this
    # by attempting writes to real owner files or connections to a real gateway.
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
        policy = dict(writable=(target,), protected=(private,), protected_ports=[*args.protect_port, port])
        preflight = [kernel_python, str(Path(__file__).with_name("hermetic_preflight.py")),
                     str(root), str(forbidden), str(private), str(port)]
        result = subprocess.run(sandboxed(preflight, root, **policy), env=env,
                                text=True, capture_output=True, timeout=20, check=False)
        (root / "preflight.json").write_text(json.dumps({"exit": result.returncode,
            "stdout": result.stdout, "stderr": result.stderr, "forbidden": str(forbidden)}, indent=2))
        if result.returncode or forbidden.read_text() != "preserve" or private.read_text() != "synthetic secret":
            print(f"hermetic_test: OS-fence preflight failed: {result.stderr}", file=sys.stderr)
            return result.returncode or 4
        print(f"hermetic_test: OS-fence preflight {result.stdout.strip()}", file=sys.stderr)
        argv = sandboxed(argv, root, **policy)
    child = subprocess.Popen(argv, env=env, start_new_session=True)
    identity = subprocess.run(["/bin/ps", "-p", str(child.pid), "-o", "pid=,lstart=,command="],
                              capture_output=True, text=True, timeout=10, check=False)
    receipt = {"pid": child.pid, "identity": identity.stdout.strip(), "command": argv,
               "root": str(root), "target": str(target), "started_monotonic": time.monotonic()}
    (root / "command.json").write_text(json.dumps(receipt, indent=2) + "\n")
    try:
        code = child.wait(timeout=args.timeout)
    except subprocess.TimeoutExpired:
        # Popen retains this exact unreaped child; never signal a discovered PID.
        child.terminate()
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=5)
        code = 124
        print(f"hermetic_test: command timed out; owned child {child.pid} reaped", file=sys.stderr)
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
