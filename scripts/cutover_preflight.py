#!/usr/bin/env python3
"""Read-only fork cutover preflight. No live activation or arbitrary-process stop API.

The default mode validates a locally staged artifact and prints unresolved gates.
--simulate rehearses only protocol-controlled child fixtures created by this tool.
It never executes an artifact, updates an install, changes a symlink, or reads auth.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import selectors
import subprocess
import sys
import tarfile
import tempfile
import time
from urllib.parse import urlparse

FORK = "https://github.com/junhoyeo/openprime"
LEGACY_COMMIT = "703367badf5c84e48e8c8cd986fbcda882b6378c"
REQUIRED = (
    "prime-agent", "package.json", "models.bundled.json", "mcp-services.bundled.json",
    "prime-agent-runtime/pyproject.toml", "prime-agent-runtime/src/rlm/__init__.py",
    "prime-agent-runtime/src/rlm/repl.py",
)
PENDING = (
    "owner accepts exact reviewed fixes and all verification evidence",
    "served fork artifact provenance and build commit independently verified",
    "isolated native trial verifies ABI2, catalog, skills, state and family behavior",
    "owner inventories and stops every old daemon, worker and kernel by proven identity",
    "owner retains old TS install and pinned develop rollback input",
    "owner decides live activation, default branch and retained incident cleanup",
)


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def regular_file(path):
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"not a regular non-symlink file: {path}")
    return path


def validate_manifest(manifest, archive, staged):
    """Verify local bytes, not publication, execution, or process-stop authority."""
    if manifest.get("repository") != FORK:
        raise ValueError("artifact repository must be junhoyeo/openprime")
    url = urlparse(manifest.get("artifact_url", ""))
    if (url.scheme != "https" or url.netloc != "github.com"
            or not re.fullmatch(
                r"/junhoyeo/openprime/releases/download/[A-Za-z0-9][A-Za-z0-9._-]*/[A-Za-z0-9][A-Za-z0-9._-]*",
                url.path,
            )
            or url.query or url.fragment):
        raise ValueError("artifact URL must name a fork GitHub release asset")
    commit = manifest.get("commit", "")
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("commit must be the full reviewed source SHA")
    if manifest.get("legacy_commit") != LEGACY_COMMIT:
        raise ValueError("preserve the pinned legacy develop rollback commit")
    expected = manifest.get("archive_sha256", "")
    if not re.fullmatch(r"[0-9a-f]{64}", expected):
        raise ValueError("archive_sha256 must be a full SHA-256")
    actual = digest(regular_file(archive))
    if actual != expected:
        raise ValueError("archive SHA-256 mismatch")
    if staged.is_symlink() or not staged.is_dir():
        raise ValueError("staged artifact must be a non-symlink directory")
    # Never extract an untrusted archive. Compare every regular member to the
    # existing staging tree, rejecting links, special files and traversal.
    files = {}
    with tarfile.open(archive, "r:gz") as bundle:
        names = set()
        for member in bundle:
            name = PurePosixPath(member.name)
            if (name.is_absolute() or ".." in name.parts or str(name) in names
                    or str(name) != member.name.rstrip("/")):
                raise ValueError(f"unsafe or duplicate archive member: {member.name}")
            names.add(str(name))
            if not (member.isfile() or member.isdir()):
                raise ValueError(f"unsupported archive member: {member.name}")
            path = staged.joinpath(*name.parts)
            if any(parent.is_symlink() for parent in (path, *path.parents)):
                raise ValueError(f"symlink in staged path: {path}")
            if member.isdir():
                if not path.is_dir():
                    raise ValueError(f"missing staged directory: {path}")
                continue
            regular_file(path)
            stream = bundle.extractfile(member)
            value = hashlib.sha256()
            for block in iter(lambda: stream.read(1024 * 1024), b""):
                value.update(block)
            archive_hash = value.hexdigest()
            if digest(path) != archive_hash:
                raise ValueError(f"staged bytes differ from archive: {member.name}")
            files[member.name] = archive_hash
    staged_entries = list(staged.rglob("*"))
    if any(path.is_symlink() or not (path.is_file() or path.is_dir()) for path in staged_entries):
        raise ValueError("staged tree contains a link or special file")
    staged_files = {str(path.relative_to(staged)) for path in staged_entries if path.is_file()}
    if staged_files != set(files):
        raise ValueError("staged tree contains files absent from archive")
    for required in REQUIRED:
        if required not in files:
            raise ValueError(f"missing artifact layout entry: {required}")
    skills = sorted(name for name in files if name.startswith("skills/") and name.endswith("/SKILL.md"))
    if not skills:
        raise ValueError("artifact has no bundled skill manifests")
    package = json.loads((staged / "package.json").read_text())
    if package.get("version") != manifest.get("version"):
        raise ValueError("package version mismatch")
    for name in ("models.bundled.json", "mcp-services.bundled.json"):
        json.loads((staged / name).read_text())
    observations = []
    for process in manifest.get("process_inventory", []):
        # An imported inventory is evidence to review, NOT proof that a PID is
        # still owned, dead, or safe to signal. Unknown identity always retains.
        observations.append({
            "role": process.get("role", "unknown"), "pid": process.get("pid"),
            "start_id": process.get("start_id"),
            "ownership": "unverified; retain; no signal authorized",
        })
    return {
        "mode": "dry-run", "activation": "pending", "repository": FORK,
        "declared_commit": commit, "build_commit_verified": False,
        "served_url_verified": False, "artifact_url": manifest["artifact_url"],
        "archive_sha256": actual, "staged_file_sha256": files,
        "version": package["version"], "skill_manifests": skills,
        "runtime_abi": "unverified; execute isolated packaged ABI2 gate",
        "catalog": "JSON syntax only; execute package catalog validation gate",
        "process_inventory": observations, "pending": list(PENDING),
    }


FIXTURE = """import hashlib, json, os, sys
from pathlib import Path
artifact = Path(sys.argv[2])
print(json.dumps({"pid": os.getpid(), "parent": os.getppid(), "role": sys.argv[1],
                  "artifact_sha256": hashlib.sha256(artifact.read_bytes()).hexdigest()}), flush=True)
for line in sys.stdin:
    if line.strip() == "stop":
        raise SystemExit(0)
raise SystemExit(2)
"""


def simulate():
    """Rehearse stop/start/rollback only with newly owned synthetic children."""
    root = Path(tempfile.mkdtemp(prefix="pa-cut-", dir=os.environ.get("TMPDIR", "/tmp"))).resolve()
    (root / "h").mkdir()
    (root / "t").mkdir()
    fixture = root / "synthetic_worker.py"
    fixture.write_text(FIXTURE)
    receipt = {"mode": "synthetic-simulation", "root": str(root), "steps": [],
               "activation": "pending", "live_processes_touched": False}
    artifacts = root / "artifacts"
    artifacts.mkdir()
    old = artifacts / "old-ts.json"
    new = artifacts / "new-rust.json"
    old.write_text(json.dumps({"runtime": "synthetic-ts", "commit": LEGACY_COMMIT}))
    new.write_text(json.dumps({"runtime": "synthetic-rust", "commit": "fixture-only"}))
    old_hash = digest(old)
    selection = root / "synthetic-install-selection.json"
    children = []
    try:
        for role, artifact in (("old-ts-fixture", old), ("new-rust-fixture", new),
                               ("rollback-ts-fixture", old)):
            selection.write_text(json.dumps({"artifact": str(artifact), "sha256": digest(artifact)}))
            child = subprocess.Popen(
                [sys.executable, "-I", str(fixture), role, str(artifact)], stdin=subprocess.PIPE,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                env={"HOME": str(root / "h"), "TMPDIR": str(root / "t"),
                     "PATH": "/usr/bin:/bin", "DO_NOT_TRACK": "1"},
            )
            children.append(child)
            with selectors.DefaultSelector() as selector:
                selector.register(child.stdout, selectors.EVENT_READ)
                deadline = time.monotonic() + 5
                line = b""
                while b"\n" not in line:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0 or not selector.select(timeout=remaining):
                        raise TimeoutError(f"owned fixture {child.pid} did not become ready")
                    block = os.read(child.stdout.fileno(), 4096)
                    if not block or len(line) + len(block) > 65536:
                        raise ValueError("invalid fixture readiness frame")
                    line += block
                ready = json.loads(line)
            if ready != {"pid": child.pid, "parent": os.getpid(), "role": role,
                         "artifact_sha256": digest(artifact)}:
                raise ValueError("owned fixture readiness identity mismatch")
            receipt["steps"].append({"action": "ready", **ready,
                                     "ownership": "direct Popen child; unreaped"})
            # Stop cooperatively through our own stdin; never signal a PID from
            # an imported inventory, executable-name match, or stale receipt.
            child.stdin.write(b"stop\n")
            child.stdin.flush()
            code = child.wait(timeout=5)
            receipt["steps"].append({"action": "reaped", "pid": child.pid, "exit_code": code})
            if code != 0:
                raise RuntimeError(f"fixture {role} exited {code}")
        if digest(old) != old_hash:
            raise ValueError("old synthetic rollback artifact changed")
        receipt["old_artifact_sha256"] = old_hash
        receipt["final_selection"] = json.loads(selection.read_text())
    finally:
        # Cleanup only children created in this invocation. A still-unreaped
        # direct child cannot have its PID reused. No killall/pkill/discovery.
        for child in children:
            if child.poll() is None:
                child.kill()
                child.wait(timeout=5)
            for stream in (child.stdin, child.stdout, child.stderr):
                stream.close()
        (root / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    return receipt


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--archive", type=Path)
    parser.add_argument("--staged", type=Path)
    parser.add_argument("--simulate", action="store_true",
                        help="run owned synthetic fixtures, never an artifact or live daemon")
    args = parser.parse_args(argv)
    try:
        if args.simulate:
            if any((args.manifest, args.archive, args.staged)):
                parser.error("simulation cannot accept live/artifact inputs")
            result = simulate()
        else:
            if not all((args.manifest, args.archive, args.staged)):
                parser.error("dry-run requires --manifest, --archive and --staged")
            result = validate_manifest(json.loads(args.manifest.read_text()), args.archive, args.staged)
        print(json.dumps(result, indent=2))
    except (OSError, ValueError, RuntimeError, TimeoutError, tarfile.TarError) as error:
        print(f"cutover preflight refused: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
