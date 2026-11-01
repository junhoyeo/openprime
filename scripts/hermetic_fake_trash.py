#!/usr/bin/env python3
"""Owned recoverable stand-in for a `trash` CLI inside hermetic test runs.

The wrapper installs this file at CONTROL/bin/trash with an absolute attested
interpreter in the shebang, and puts CONTROL/bin first on the subject PATH. It
never contacts the system Trash and never deletes. Each accepted operand is moved
with renamex_np(RENAME_EXCL) into a fresh per-call directory under the attested
store. Exit 0 only after every operand is gone from its original path.

Accepted forms: `trash -- /abs/path` (old base) and `trash /abs/path [...]`
(record v13). Every flag, relative path, and `.`/`..`/empty component is refused.
The attested roots and store are embedded at install time; no environment
variable can change them.
"""
import ctypes
import errno
import json
import os
from pathlib import Path
import sys
import time

# The wrapper replaces this single line at install time with the attested roots
# and store. The subject environment can never redirect them.
ATTESTED = None
RENAME_EXCL = 0x00000004  # SDK sys/stdio.h


class Refusal(Exception):
    """A refused request: nothing is moved, exit 2."""


def parse(argv):
    operands = list(argv)
    if operands[:1] == ["--"]:
        operands = operands[1:]
    if not operands:
        raise Refusal("usage: trash [--] ABSOLUTE_PATH [...]")
    for operand in operands:
        if operand.startswith("-"):
            raise Refusal(f"refusing flag or dash operand: {operand!r}")
        if not operand.startswith("/"):
            raise Refusal(f"refusing relative path: {operand!r}")
        parts = operand.split("/")[1:]
        if any(part in ("", ".", "..") for part in parts):
            raise Refusal(f"refusing non-normalized path: {operand!r}")
    return [Path(operand) for operand in operands]


def attested(attestation):
    if not isinstance(attestation, dict):
        raise Refusal("guard did not attest owned roots")
    roots = [Path(item) for item in attestation.get("roots") or []]
    store = attestation.get("store")
    if not roots or not store or not all(root.is_absolute() for root in roots) or not Path(store).is_absolute():
        raise Refusal("guard did not attest owned roots")
    roots = [root.resolve() for root in roots]
    store = Path(store).resolve()
    if not any(store.is_relative_to(root) for root in roots):
        raise Refusal("attested store is outside the attested roots")
    return roots, store


def validate(path, roots, store):
    parent = path.parent.resolve()
    leaf = parent / path.name
    if not any(parent == root or parent.is_relative_to(root) for root in roots):
        raise Refusal(f"refusing a path outside the attested roots: {path}")
    protected = [*roots, store]
    if any(leaf == item or item.is_relative_to(leaf) for item in protected):
        raise Refusal(f"refusing a protected path or ancestor: {path}")
    if leaf.is_relative_to(store):
        raise Refusal(f"refusing a path inside the fake store: {path}")
    try:
        leaf.lstat()
    except FileNotFoundError:
        raise Refusal(f"no such file: {path}") from None
    return leaf


def rename_exclusive(source, destination, renamex=None):
    if renamex is None:
        lib = ctypes.CDLL(None, use_errno=True)
        renamex = lib.renamex_np
        renamex.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint]
        renamex.restype = ctypes.c_int
    ctypes.set_errno(0)
    if renamex(os.fsencode(source), os.fsencode(destination), RENAME_EXCL) != 0:
        code = ctypes.get_errno() or errno.EIO
        raise OSError(code, os.strerror(code), str(source))


def main(argv=None, attestation=None, renamex=None):
    argv = sys.argv[1:] if argv is None else argv
    attestation = ATTESTED if attestation is None else attestation
    try:
        paths = parse(argv)
        roots, store = attested(attestation)
        leaves = [validate(path, roots, store) for path in paths]
    except Refusal as refusal:
        print(f"hermetic fake trash: {refusal}", file=sys.stderr)
        return 2
    moved = []
    try:
        store.mkdir(mode=0o700, exist_ok=True)
        slot = store / f"{time.time_ns()}-{os.getpid()}"
        slot.mkdir(mode=0o700)
        for index, leaf in enumerate(leaves):
            entry = slot / str(index)
            entry.mkdir(mode=0o700)
            destination = entry / leaf.name
            rename_exclusive(leaf, destination, renamex)
            moved.append({"source": str(leaf), "destination": str(destination)})
    except OSError as error:
        print(f"hermetic fake trash: move failed ({error}); moved so far: {json.dumps(moved)}",
              file=sys.stderr)
        return 1
    if any(os.path.lexists(leaf) for leaf in leaves):
        print(f"hermetic fake trash: operand still present after move: {json.dumps(moved)}",
              file=sys.stderr)
        return 1
    # Best effort only: the verified move already succeeded, so a receipt failure
    # is reported on stderr but never turns a recoverable move into a failure.
    try:
        with (store / "receipts.jsonl").open("a", encoding="utf-8") as stream:
            stream.write(json.dumps({"moved": moved}) + "\n")
    except OSError as error:
        print(f"hermetic fake trash: receipt not recorded ({error}); moved: {json.dumps(moved)}",
              file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
