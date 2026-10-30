"""High-resolution macOS process receipts. Never signal discovered PIDs."""
import ctypes
import errno
import os
import subprocess


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
        # SDK sys/proc_info.h: proc_bsdinfo on 64-bit macOS.
        if (ctypes.sizeof(BsdInfo), BsdInfo.start_seconds.offset,
                BsdInfo.start_microseconds.offset) != (136, 120, 128):
            raise RuntimeError("unsupported proc_bsdinfo ABI")
        self.lib = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
        self.lib.proc_listpids.argtypes = [ctypes.c_uint32, ctypes.c_uint32, ctypes.c_void_p, ctypes.c_int]
        self.lib.proc_listpids.restype = ctypes.c_int
        self.lib.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
        self.lib.proc_pidinfo.restype = ctypes.c_int

    def identity(self, pid):
        info = BsdInfo()
        ctypes.set_errno(0)
        size = self.lib.proc_pidinfo(pid, 3, 0, ctypes.byref(info), ctypes.sizeof(info))
        if size == 0 and ctypes.get_errno() == errno.ESRCH:
            return None
        if size != ctypes.sizeof(info) or info.pid != pid or not info.start_seconds:
            raise RuntimeError(f"cannot prove process identity for pid {pid}: size={size}, errno={ctypes.get_errno()}")
        try:
            session = os.getsid(pid)
        except ProcessLookupError:
            return None
        # Re-read after getsid: do not combine metadata from a recycled PID.
        again = BsdInfo()
        ctypes.set_errno(0)
        size = self.lib.proc_pidinfo(pid, 3, 0, ctypes.byref(again), ctypes.sizeof(again))
        if size == 0 and ctypes.get_errno() == errno.ESRCH:
            return None
        if size != ctypes.sizeof(again) or (info.start_seconds, info.start_microseconds) != (
                again.start_seconds, again.start_microseconds):
            raise RuntimeError(f"process identity changed during inspection: {pid}")
        try:
            session_again = os.getsid(pid)
        except ProcessLookupError:
            return None
        previous = {"ppid": info.ppid, "pgid": info.pgid, "sid": session, "uid": info.uid}
        current = {"ppid": again.ppid, "pgid": again.pgid, "sid": session_again, "uid": again.uid}
        return {"pid": pid, **current, "previous_topology": previous,
                "topology_stable": previous == current, "status": again.status,
                "start_seconds": again.start_seconds, "start_microseconds": again.start_microseconds,
                "name": bytes(again.name or again.comm).decode("utf-8", errors="replace")}

    def snapshot(self):
        # PROC_UID_ONLY: no private argv/env or other users' process details.
        required = self.lib.proc_listpids(4, os.getuid(), None, 0)
        if required <= 0:
            raise RuntimeError("cannot enumerate same-UID processes")
        for _ in range(3):
            pids = (ctypes.c_int * (required // ctypes.sizeof(ctypes.c_int) + 128))()
            used = self.lib.proc_listpids(4, os.getuid(), pids, ctypes.sizeof(pids))
            if used <= 0 or used % ctypes.sizeof(ctypes.c_int):
                raise RuntimeError("invalid process inventory result")
            if used < ctypes.sizeof(pids):
                result = {}
                for pid in pids[:used // ctypes.sizeof(ctypes.c_int)]:
                    if not pid:
                        continue
                    identity = self.identity(pid)
                    if identity is not None:
                        result[key(identity)] = identity
                return result
            required = used * 2
        raise RuntimeError("process inventory changed too quickly to inspect safely")


def key(identity):
    return (identity["pid"], identity["start_seconds"], identity["start_microseconds"])


def new_processes(before, after):
    # Detached/reparented descendants and external executables are not hidden by
    # argv[0]. Concurrent unrelated new processes stay UNKNOWN, never kill targets.
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
