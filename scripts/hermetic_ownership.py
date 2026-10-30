"""Observational subject ownership; never authority to signal a discovered PID.

Snapshots are non-atomic and checkpoints can miss rapid double forks. Unattributed
host arrivals are warnings, not proof of independence. Positive subject evidence
or failed required tracking blocks the run. No names are ownership rules.
"""
import ctypes
import errno
from pathlib import Path

from hermetic_processes import UnknownObservation, absence_receipt, key, new_processes


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
        pid = row.pid if isinstance(row, UnknownObservation) else row["pid"]
        matches, errors = [], []
        initial = self.inventory.identity(pid)
        if initial is None:
            return {"gone": True, "matches": [], "errors": [], "absence": absence_receipt(self.inventory, pid)}
        expected = None if isinstance(row, UnknownObservation) else key(row)
        if isinstance(initial, UnknownObservation):
            errors.append(initial.receipt())
        elif expected is not None and key(initial) != expected:
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
        final = self.inventory.identity(pid)
        if final is None:
            return {"gone": True, "matches": matches, "errors": errors, "absence": absence_receipt(self.inventory, pid)}
        if isinstance(final, UnknownObservation):
            errors.append(final.receipt())
        elif expected is not None and key(final) != expected:
            errors.append("identity changed during reference inspection")
        return {"matches": matches, "errors": errors}



class OwnershipTracker:
    """Retain historical proof and unresolved observation gaps; never signal."""
    MAX_GAPS = 4096
    MAX_AUDIT = 16384

    def __init__(self, subject_pid, identity):
        self.subject_pid = subject_pid
        self.subject_birth = None if identity is None else key(identity)
        self.owned = {} if identity is None else {key(identity): {"identity": identity, "reason": "direct_child"}}
        self.related = {}
        self.gaps = {}
        self.resolved = {}
        self.audit = []
        self.checkpoints = 0

    def record(self, entry):
        if len(self.audit) >= self.MAX_AUDIT:
            raise RuntimeError("ownership observation audit exceeds bound")
        self.audit.append(entry)

    def historical(self, pid):
        return [proof for proof in (*self.owned.values(), *self.related.values())
                if proof["identity"]["pid"] == pid]

    def active_history(self, pid):
        return [proof for proof in self.historical(pid) if key(proof["identity"]) not in self.resolved]

    def topology_history(self, pid):
        # Parent exit does not erase an earlier descendant edge. A complete
        # different parent birth does retire that numeric inference. This is
        # candidate evidence only; never reactivate ownership of an exited parent.
        return [proof for proof in self.historical(pid)
                if self.resolved.get(key(proof["identity"]), {}).get("resolution") != "different_complete_birth"]

    def resolve_historical(self, known, gone):
        for proof in (*self.owned.values(), *self.related.values()):
            original = proof["identity"]
            birth = key(original)
            current = known.get(original["pid"])
            existing = self.resolved.get(birth)
            different = current is not None and key(current) != birth
            if (existing is None and (original["pid"] in gone or different)) or (
                    existing is not None and existing["resolution"] == "explicit_absence" and different):
                resolution = {"resolution": "different_complete_birth" if different else "explicit_absence",
                              "historical_proof": proof, "current": current}
                self.resolved[birth] = resolution
                self.record(resolution)

    def subject_group_matches(self, view):
        # A complete different leader birth retires numeric group inference.
        # Mere leader exit does not: its descendants may retain that session.
        resolution = self.resolved.get(self.subject_birth)
        if resolution is not None and resolution["resolution"] == "different_complete_birth":
            return False
        return view.get("sid") == self.subject_pid or view.get("pgid") == self.subject_pid

    def partial_relation(self, observation):
        direct_unresolved = observation.pid == self.subject_pid and (
            self.subject_birth is None or self.subject_birth not in self.resolved)
        return direct_unresolved or bool(self.active_history(observation.pid)) or self.partial_topology_relation(observation)

    def partial_topology_relation(self, observation):
        for part in observation.partial:
            for view in (part, part.get("previous_topology", {})):
                if self.subject_group_matches(view):
                    return True
                if self.topology_history(view.get("ppid")) or self.gaps.get(view.get("ppid"), {}).get("required", False):
                    return True
        return False

    def reference_relation(self, evidence):
        for error in evidence.get("errors", []):
            if isinstance(error, dict) and error.get("kind") == "unknown":
                partial = UnknownObservation(error["pid"], error["stage"], error.get("size"),
                                             error.get("errno", 0), error["reason"], error.get("partial", []))
                if self.partial_relation(partial):
                    return True
        return False

    def resolve_gaps(self, known_by_pid, gone):
        for pid in list(self.gaps):
            known = known_by_pid.get(pid)
            if known is not None or pid in gone:
                gap = self.gaps.pop(pid)
                # Only evidence captured for THIS gap can discharge it. Older
                # resolved PID history cannot erase a newly observed relation.
                proof = gap["historical_proof"]
                resolution = "known_birth" if known is not None else "explicit_absence"
                if known is not None and gap["required"]:
                    same_recorded_birth = any(key(entry["identity"]) == key(known) for entry in proof)
                    if gap["unbound_positive"] or not proof or same_recorded_birth:
                        # Partial positive topology cannot be erased by a later
                        # detached full row. Latch a candidate, not an owned edge.
                        # With no earlier complete birth, reuse is not proven.
                        if key(known) not in self.owned:
                            self.related[key(known)] = {
                                "identity": known, "reason": "latched_positive_unknown",
                                "previous_unknown": gap["observation"].receipt(),
                                "historical_proof": gap["historical_proof"],
                            }
                        resolution = "known_birth_with_latched_relationship"
                    else:
                        resolution = "different_complete_birth"
                self.record({"checkpoint": self.checkpoints, "resolved_pid": pid,
                             "resolution": resolution,
                             "known": known, "previous_unknown": gap["observation"].receipt(),
                             "historical_proof": gap["historical_proof"]})

    def observe(self, snapshot):
        self.checkpoints += 1
        by_pid = {row["pid"]: row for row in snapshot.values()}
        self.resolve_historical(by_pid, getattr(snapshot, "gone", set()))
        self.resolve_gaps(by_pid, getattr(snapshot, "gone", set()))
        changed = True
        while changed:
            changed = False
            for row in snapshot.values():
                if key(row) in self.resolved:
                    raise RuntimeError("a previously resolved birth identity reappeared")
                if key(row) in self.owned:
                    continue
                parent = by_pid.get(row["ppid"])
                historical_parent = self.topology_history(row["ppid"])
                linked = (parent is not None and key(parent) in self.owned and key(parent) not in self.resolved
                          and key(parent)[1:] <= key(row)[1:])
                if linked and row.get("topology_stable", True) and parent.get("topology_stable", True):
                    self.owned[key(row)] = {"identity": row, "reason": "observed_parent_identity", "parent": parent}
                    changed = True
                else:
                    previous = row.get("previous_topology", {})
                    previous_parent = by_pid.get(previous.get("ppid"))
                    prior_parent = self.topology_history(previous.get("ppid")) if (
                        previous_parent is None or key(previous_parent) in self.owned) else []
                    group = any(self.subject_group_matches(view) for view in (row, previous))
                    # A historical PID with missing/Unknown metadata is evidence
                    # of a candidate edge, not proof of a matching parent birth.
                    unresolved_parent = parent is None and bool(historical_parent)
                    if linked or unresolved_parent or prior_parent or group:
                        self.related[key(row)] = {"identity": row, "reason": "unverified_subject_topology",
                                                  "historical_parent_proof": historical_parent or prior_parent}
        for pid, observation in getattr(snapshot, "unknown", {}).items():
            required = self.partial_relation(observation) or self.gaps.get(pid, {}).get("required", False)
            previous = self.gaps.get(pid)
            bound = {} if previous is None else {key(proof["identity"]): proof for proof in previous["historical_proof"]}
            for proof in self.active_history(pid):
                bound.setdefault(key(proof["identity"]), proof)
            unbound_positive = (self.partial_topology_relation(observation) or
                                (required and not bound) or
                                (previous is not None and previous["unbound_positive"]))
            entry = {"observation": observation, "required": required,
                     "historical_proof": list(bound.values()), "unbound_positive": unbound_positive}
            self.gaps[pid] = entry
            if len(self.gaps) > self.MAX_GAPS:
                raise RuntimeError("ownership unknown gaps exceed bound")
            if previous is None or previous["observation"].receipt() != observation.receipt() or previous["required"] != required or previous["unbound_positive"] != unbound_positive:
                self.record({"checkpoint": self.checkpoints, "observation": observation.receipt(),
                             "required": required, "unbound_positive": unbound_positive,
                             "historical_proof": entry["historical_proof"]})
        changed = True
        while changed:
            changed = False
            for gap in self.gaps.values():
                if not gap["required"] and self.partial_relation(gap["observation"]):
                    gap["required"] = True
                    gap["unbound_positive"] = True
                    changed = True
                    self.record({"checkpoint": self.checkpoints, "propagated_required_gap": gap["observation"].receipt()})
        for row in snapshot.values():
            if key(row) not in self.owned and self.gaps.get(row["ppid"], {}).get("required", False):
                self.related[key(row)] = {"identity": row, "reason": "parent_has_required_unknown_gap",
                                          "parent_observation": self.gaps[row["ppid"]]["observation"].receipt()}


    def classify(self, before, after, probe):
        self.observe(after)
        known = {row["pid"]: row for row in after.values()}
        unknown = dict(getattr(after, "unknown", {}))
        gone = set(getattr(after, "gone", set()))
        reconciled = []
        # One explicit final observation per missing historically relevant PID.
        # Set subtraction of only known birth keys must never erase an old gap.
        required_pids = {proof["identity"]["pid"] for proof in (*self.owned.values(), *self.related.values())
                         if key(proof["identity"]) not in self.resolved}
        required_pids.update(pid for pid, gap in self.gaps.items() if gap["required"])
        for pid in sorted(required_pids - set(known) - set(unknown) - gone):
            observation = probe.inventory.identity(pid)
            if observation is None:
                gone.add(pid)
                resolution = absence_receipt(probe.inventory, pid)
            elif isinstance(observation, UnknownObservation):
                unknown[pid] = observation
                resolution = observation.receipt()
            else:
                known[pid] = observation
                resolution = {"kind": "known", "identity": observation}
            reconciled.append(resolution)
            self.record({"final_reconciliation": resolution, "historical_proof": self.historical(pid)})
        # Earlier host-only unknowns still appear in diagnostics if omitted; they
        # are not implicitly Gone. A later explicit known/ESRCH resolves them.
        for pid, gap in self.gaps.items():
            if pid not in known and pid not in gone:
                unknown.setdefault(pid, gap["observation"])
        self.resolve_historical(known, gone)
        self.resolve_gaps(known, gone)
        owned, candidates, warnings = [], [], []
        for row in known.values():
            identity_key = key(row)
            historical = self.historical(row["pid"])
            if row.get("status") == 5:
                continue
            if identity_key in self.owned and identity_key not in self.resolved:
                owned.append({"identity": row, "proof": self.owned[identity_key]})
                continue
            if identity_key in before and (identity_key not in self.related or identity_key in self.resolved):
                continue
            evidence = probe.inspect(row)
            if evidence.get("gone"):
                continue
            if (identity_key in self.related and identity_key not in self.resolved) or evidence.get("matches") or self.reference_relation(evidence):
                candidates.append({"identity": row, "evidence": evidence,
                                   "relationship": self.related.get(identity_key)})
            else:
                warnings.append({"identity": row, "evidence": evidence,
                                 "historical_pid_proof": historical,
                                 "status": "unattributed; host-wide attribution incomplete"})
        for pid, observation in unknown.items():
            required = self.partial_relation(observation) or self.gaps.get(pid, {}).get("required", False)
            if required:
                # Required Unknown already blocks. Do not turn its one explicit
                # final reconciliation into extra identity retries via references.
                evidence = {"matches": [], "errors": [observation.receipt()],
                            "inspection": "required Unknown retained without reference rechecks"}
            else:
                evidence = probe.inspect(observation)
                if evidence.get("gone") and not evidence.get("matches"):
                    self.record({"final_reference_absence": evidence.get("absence", {"pid": pid}), "previous_unknown": observation.receipt(),
                                 "historical_proof": self.historical(pid)})
                    continue
                required = self.reference_relation(evidence)
            entry = {"unknown": observation.receipt(), "evidence": evidence,
                     "historical_proof": self.historical(pid)}
            if required or evidence.get("matches"):
                candidates.append(entry)
            else:
                entry["status"] = "unattributed unknown; host-wide attribution incomplete"
                warnings.append(entry)
        return {"observed_owned_survivors": owned, "unverified_subject_candidates": candidates,
                "host_warnings": warnings, "checkpoints": self.checkpoints,
                "observed_owned": list(self.owned.values()), "observed_related": list(self.related.values()),
                "observation_audit": self.audit, "final_reconciliation": reconciled,
                "historical_resolutions": list(self.resolved.values()),
                "limits": "Non-atomic snapshots and checkpoints can miss rapid detachment; no complete containment claim."}
