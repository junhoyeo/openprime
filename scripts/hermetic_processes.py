"""High-resolution macOS observations. Never signal discovered PIDs."""
import ctypes
from dataclasses import dataclass, field
import errno
import os
import subprocess
import time

MAX_UNKNOWN_ROWS = 4096
OBSERVATION_SCHEMA = "hermetic.process-observation.v1"


@dataclass
class UnknownObservation:
    """Per-PID uncertainty, never an invented birth identity or absence proof."""
    pid: int
    stage: str
    size: int | None
    error_number: int
    reason: str
    partial: list = field(default_factory=list)

    def receipt(self):
        return {"kind": "unknown", "pid": self.pid, "stage": self.stage,
                "size": self.size, "errno": self.error_number, "reason": self.reason,
                "partial": self.partial}


class ProcessSnapshot(dict):
    """Known birth-keyed rows plus complete per-PID observation provenance."""
    def __init__(self, *, sequence=0, listed=(), observed_at=None):
        super().__init__()
        self.sequence = sequence
        self.listed = list(listed)
        self.observed_at = time.monotonic() if observed_at is None else observed_at
        self.unknown = {}
        self.gone = set()
        self.gone_observations = {}
        self.complete = False

    def receipt(self):
        return {"schema": OBSERVATION_SCHEMA, "known": list(self.values()), "unknown": [row.receipt() for row in self.unknown.values()],
                "gone_pids": sorted(self.gone), "gone_observations": list(self.gone_observations.values()), "listed_pids": self.listed,
                "sequence": self.sequence, "observed_monotonic": self.observed_at,
                "enumeration_complete": self.complete}


def snapshot_receipt(snapshot):
    return snapshot.receipt() if isinstance(snapshot, ProcessSnapshot) else {"schema": OBSERVATION_SCHEMA, "known": list(snapshot.values()), "injected_legacy_map": True}


def absence_receipt(inventory, pid):
    records = getattr(inventory, "absence_observations", {})
    if isinstance(records, dict) and pid in records:
        return records[pid]
    return {"kind": "gone", "pid": pid, "stage": "injected_identity_contract"}


class BsdInfo(ctypes.Structure):
    _fields_ = [(name, ctypes.c_uint32) for name in (
        "flags", "status", "xstatus", "pid", "ppid", "uid", "gid", "ruid",
        "rgid", "svuid", "svgid", "reserved")] + [
        ("comm", ctypes.c_char * 16), ("name", ctypes.c_char * 32),
        ("nfiles", ctypes.c_uint32), ("pgid", ctypes.c_uint32),
        ("pjobc", ctypes.c_uint32), ("tdev", ctypes.c_uint32),
        ("tpgid", ctypes.c_uint32), ("nice", ctypes.c_int32),
        ("start_seconds", ctypes.c_uint64), ("start_microseconds", ctypes.c_uint64)]


class ProcessInventory:
    def __init__(self):
        if (ctypes.sizeof(BsdInfo), BsdInfo.start_seconds.offset,
                BsdInfo.start_microseconds.offset) != (136, 120, 128):
            raise RuntimeError("unsupported proc_bsdinfo ABI")
        self.sequence = 0
        self.absence_observations = {}
        self.lib = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
        self.lib.proc_listpids.argtypes = [ctypes.c_uint32, ctypes.c_uint32, ctypes.c_void_p, ctypes.c_int]
        self.lib.proc_listpids.restype = ctypes.c_int
        self.lib.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
        self.lib.proc_pidinfo.restype = ctypes.c_int

    def gone(self, pid, stage, size, error, partial):
        if not hasattr(self, "absence_observations"):
            self.absence_observations = {}
        self.absence_observations[pid] = {"kind": "gone", "pid": pid, "stage": stage,
                                          "size": size, "errno": error, "partial": partial}
        return None

    def identity(self, pid):
        if hasattr(self, "absence_observations"):
            self.absence_observations.pop(pid, None)
        partial = []
        for attempt in range(2):
            stage = f"bsd_{attempt + 1}"
            info = BsdInfo()
            ctypes.set_errno(0)
            size = self.lib.proc_pidinfo(pid, 3, 0, ctypes.byref(info), ctypes.sizeof(info))
            error = ctypes.get_errno()
            if size == 0 and error == errno.ESRCH:
                return self.gone(pid, stage, size, error, partial)
            if size == 0:
                return UnknownObservation(pid, stage, size, error, "BSD identity unavailable", partial)
            if size != ctypes.sizeof(info) or info.pid != pid:
                raise RuntimeError(f"invalid BSD observation shape: pid={pid}, size={size}, returned_pid={info.pid}")
            sample = {"pid": pid, "ppid": info.ppid, "pgid": info.pgid, "uid": info.uid,
                      "start_seconds": info.start_seconds, "start_microseconds": info.start_microseconds,
                      "status": info.status,
                      "name": bytes(info.name or info.comm).decode("utf-8", errors="replace")}
            partial.append(sample)
            if not info.start_seconds or info.start_microseconds >= 1_000_000:
                return UnknownObservation(pid, stage, size, error, "invalid or missing birth timestamp", partial)
            try:
                sample["sid"] = os.getsid(pid)
            except ProcessLookupError as absent:
                return self.gone(pid, f"sid_{attempt + 1}", None, absent.errno, partial)
            except PermissionError as denied:
                return UnknownObservation(pid, f"sid_{attempt + 1}", None, denied.errno,
                                          "session identity unavailable", partial)
        first, last = partial
        if key(first) != key(last):
            return UnknownObservation(pid, "birth_revalidation", ctypes.sizeof(info), 0,
                                      "birth identity changed during observation", partial)
        previous = {name: first[name] for name in ("ppid", "pgid", "sid", "uid")}
        current = {name: last[name] for name in previous}
        return {**last, "previous_topology": previous, "topology_stable": previous == current}

    def snapshot(self):
        required = self.lib.proc_listpids(4, os.getuid(), None, 0)
        if required <= 0 or required % ctypes.sizeof(ctypes.c_int):
            raise RuntimeError("cannot enumerate same-UID processes")
        for _ in range(3):
            pids = (ctypes.c_int * (required // ctypes.sizeof(ctypes.c_int) + 128))()
            used = self.lib.proc_listpids(4, os.getuid(), pids, ctypes.sizeof(pids))
            if used <= 0 or used % ctypes.sizeof(ctypes.c_int) or used > ctypes.sizeof(pids):
                raise RuntimeError("invalid process inventory result")
            if used < ctypes.sizeof(pids):
                listed = [pid for pid in pids[:used // ctypes.sizeof(ctypes.c_int)] if pid]
                if any(pid < 0 for pid in listed) or len(set(listed)) != len(listed):
                    raise RuntimeError("malformed PID inventory")
                self.sequence += 1
                result = ProcessSnapshot(sequence=self.sequence, listed=listed)
                for pid in listed:
                    identity = self.identity(pid)
                    if isinstance(identity, UnknownObservation):
                        result.unknown[pid] = identity
                        if len(result.unknown) > MAX_UNKNOWN_ROWS:
                            raise RuntimeError("per-PID unknown observations exceed bound")
                    elif identity is None:
                        result.gone.add(pid)
                        result.gone_observations[pid] = absence_receipt(self, pid)
                    else:
                        result[key(identity)] = identity
                result.complete = True
                return result
            required = used * 2
        raise RuntimeError("process inventory changed too quickly to inspect safely")


def key(identity):
    return (identity["pid"], identity["start_seconds"], identity["start_microseconds"])


def new_processes(before, after):
    return [identity for identity_key, identity in after.items() if identity_key not in before]


def reap_owned(child):
    """Terminate/reap only this exact unreaped Popen child."""
    if child.poll() is not None:
        return
    child.terminate()
    try:
        child.wait(timeout=5)
    except subprocess.TimeoutExpired:
        child.kill()
        child.wait(timeout=5)
