"""Observational subject ownership; never authority to signal a discovered PID.

Snapshots are non-atomic and checkpoints can miss rapid double forks. Unattributed
host arrivals are warnings, not proof of independence. Positive subject evidence
or failed required tracking blocks the run. No names are ownership rules.
"""
import ctypes
import errno
from pathlib import Path

from hermetic_processes import key, new_processes


class VinfoStat(ctypes.Structure):
    _fields_ = [("dev", ctypes.c_uint32), ("mode", ctypes.c_uint16), ("nlink", ctypes.c_uint16),
                ("ino", ctypes.c_uint64), ("uid", ctypes.c_uint32), ("gid", ctypes.c_uint32)] + [
                (name, ctypes.c_int64) for name in ("atime", "atimensec", "mtime", "mtimensec",
                "ctime", "ctimensec", "birthtime", "birthtimensec", "size", "blocks")] + [
                ("blksize", ctypes.c_int32), ("flags", ctypes.c_uint32), ("gen", ctypes.c_uint32),
                ("rdev", ctypes.c_uint32), ("spare", ctypes.c_int64 * 2)]


class VnodeInfo(ctypes.Structure):
    _fields_ = [("stat", VinfoStat), ("type", ctypes.c_int32), ("pad", ctypes.c_int32),
                ("fsid", ctypes.c_int32 * 2)]


class VnodePath(ctypes.Structure):
    _fields_ = [("info", VnodeInfo), ("path", ctypes.c_char * 1024)]


class ProcessVnodePaths(ctypes.Structure):
    _fields_ = [("cwd", VnodePath), ("root", VnodePath)]


class FileInfo(ctypes.Structure):
    _fields_ = [("flags", ctypes.c_uint32), ("status", ctypes.c_uint32),
                ("offset", ctypes.c_int64), ("type", ctypes.c_int32), ("guards", ctypes.c_uint32)]


class FdPath(ctypes.Structure):
    _fields_ = [("file", FileInfo), ("vnode", VnodePath)]


class FdInfo(ctypes.Structure):
    _fields_ = [("fd", ctypes.c_int32), ("type", ctypes.c_uint32)]


class ReferenceProbe:
    """Read only native metadata; retain only references matching owned roots."""
    def __init__(self, inventory, roots, log_identity):
        self.inventory = inventory
        self.lib = inventory.lib
        self.roots = tuple(Path(root) for root in roots)
        self.log_identity = log_identity
        # SDK sys/proc_info.h: fixed-width 64-bit Darwin layouts.
        if tuple(map(ctypes.sizeof, (VinfoStat, VnodeInfo, VnodePath, ProcessVnodePaths, FdPath))) != (
                136, 152, 1176, 2352, 1200):
            raise RuntimeError("unsupported vnode metadata ABI")
        self.lib.proc_pidpath.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32]
        self.lib.proc_pidpath.restype = ctypes.c_int
        self.lib.proc_pidfdinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_int,
                                          ctypes.c_void_p, ctypes.c_int]
        self.lib.proc_pidfdinfo.restype = ctypes.c_int

    def inspect(self, row):
        pid = row["pid"]
        matches, errors = [], []
        try:
            initial = self.inventory.identity(pid)
        except (OSError, RuntimeError) as error:
            return {"matches": [], "errors": [f"identity probe unknown: {type(error).__name__}"]}
        if initial is None:
            return {"gone": True, "matches": [], "errors": []}
        if key(initial) != key(row):
            return {"matches": [], "errors": ["identity changed before reference inspection"]}

        def retain(kind, raw):
            path = Path(raw.decode("utf-8", errors="replace"))
            if path.is_absolute() and any(path == root or path.is_relative_to(root) for root in self.roots):
                matches.append({"kind": kind, "path": str(path)})

        executable = ctypes.create_string_buffer(4096)
        if self.lib.proc_pidpath(pid, executable, len(executable)) > 0:
            retain("executable", executable.value)
        else:
            errors.append("executable metadata unavailable")
        paths = ProcessVnodePaths()
        if self.lib.proc_pidinfo(pid, 9, 0, ctypes.byref(paths), ctypes.sizeof(paths)) == ctypes.sizeof(paths):
            retain("cwd", bytes(paths.cwd.path))
        else:
            errors.append("cwd metadata unavailable")
        ctypes.set_errno(0)
        required = self.lib.proc_pidinfo(pid, 1, 0, None, 0)
        if required < 0 or (required == 0 and ctypes.get_errno()) or required > 16384 * ctypes.sizeof(FdInfo):
            errors.append("fd inventory unavailable or exceeds bound")
        elif required:
            fds = (FdInfo * (required // ctypes.sizeof(FdInfo) + 64))()
            used = self.lib.proc_pidinfo(pid, 1, 0, fds, ctypes.sizeof(fds))
            if used <= 0 or used >= ctypes.sizeof(fds) or used % ctypes.sizeof(FdInfo):
                errors.append("fd inventory changed or unavailable")
            else:
                for entry in fds[:used // ctypes.sizeof(FdInfo)]:
                    if entry.type != 1:  # PROX_FDTYPE_VNODE only, never socket payloads.
                        continue
                    info = FdPath()
                    ctypes.set_errno(0)
                    size = self.lib.proc_pidfdinfo(pid, entry.fd, 2, ctypes.byref(info), ctypes.sizeof(info))
                    if size != ctypes.sizeof(info):
                        if ctypes.get_errno() not in (errno.EBADF, errno.ENOENT):
                            errors.append("vnode fd metadata unavailable")
                        continue
                    retain("open_artifact", bytes(info.vnode.path))
                    observed = (info.vnode.info.stat.dev, info.vnode.info.stat.ino)
                    if self.log_identity is not None and observed == self.log_identity:
                        matches.append({"kind": "subject_log_inode", "fd": entry.fd})
        try:
            final = self.inventory.identity(pid)
        except (OSError, RuntimeError) as error:
            final = None
            errors.append(f"final identity probe unknown: {type(error).__name__}")
        else:
            if final is None:
                return {"gone": True, "matches": matches, "errors": errors}
        if final is not None and key(final) != key(row):
            errors.append("identity changed during reference inspection")
        return {"matches": matches, "errors": sorted(set(errors))}


class OwnershipTracker:
    """Track observed birth identities, not PID/name guesses or kill authority."""
    def __init__(self, subject_pid, identity):
        self.subject_pid = subject_pid
        self.owned = {} if identity is None else {key(identity): {"identity": identity, "reason": "direct_child"}}
        self.related = {}
        self.checkpoints = 0

    def observe(self, snapshot):
        self.checkpoints += 1
        by_pid = {row["pid"]: row for row in snapshot.values()}
        changed = True
        while changed:
            changed = False
            for row in snapshot.values():
                if key(row) in self.owned:
                    continue
                parent = by_pid.get(row["ppid"])
                linked_parent = (parent is not None and key(parent) in self.owned
                                 and key(parent)[1:] <= key(row)[1:])
                if linked_parent and row.get("topology_stable", True) and parent.get("topology_stable", True):
                    self.owned[key(row)] = {"identity": row, "reason": "observed_parent_identity", "parent": parent}
                    changed = True
                else:
                    previous = row.get("previous_topology", {})
                    previous_parent = by_pid.get(previous.get("ppid"))
                    previously_linked = previous_parent is not None and key(previous_parent) in self.owned
                    group_match = any(view.get("sid") == self.subject_pid or view.get("pgid") == self.subject_pid
                                      for view in (row, previous))
                    if linked_parent or previously_linked or group_match:
                        # Unstable topology and numeric groups are candidate
                        # evidence, not an invented stable parent-edge proof.
                        self.related[key(row)] = {"identity": row, "reason": "unverified_subject_topology"}

    def classify(self, before, after, probe):
        self.observe(after)
        owned, candidates, warnings = [], [], []
        for row in new_processes(before, after):
            if row.get("status") == 5:  # Darwin SZOMB: exited, awaiting its real parent's reap.
                continue
            identity_key = key(row)
            if identity_key in self.owned:
                owned.append({"identity": row, "proof": self.owned[identity_key]})
                continue
            evidence = probe.inspect(row)
            if evidence.get("gone"):
                continue
            if identity_key in self.related or evidence.get("matches"):
                candidates.append({"identity": row, "evidence": evidence,
                                   "relationship": self.related.get(identity_key)})
            else:
                warnings.append({"identity": row, "evidence": evidence,
                                 "status": "unattributed; host-wide attribution incomplete"})
        return {"observed_owned_survivors": owned, "unverified_subject_candidates": candidates,
                "host_warnings": warnings, "checkpoints": self.checkpoints,
                "observed_owned": list(self.owned.values()), "observed_related": list(self.related.values()),
                "limits": "Non-atomic snapshots and checkpoints can miss rapid detachment; no complete containment claim."}
