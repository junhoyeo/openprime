"""Hermetic Trash guard: profile rules, the owned fake `trash`, and its inventory.

Guarantee wording: the hermetic sandbox denies known trash executables and Apple
Event file operations, and routes known `trash` CLI calls to an owned fake. It
does not prevent all deletion, and it does not detect every trash attempt.
Containment comes from the write fence, the real-home `.Trash` deny and the
service denies. The exec name rules are detection plus compatibility only:
Seatbelt matches the resolved target, so a copy or hardlink under another name
still runs (covered by the service denies and the write fence).
"""
import json
import os
from pathlib import Path
import pwd
import stat

KNOWN_NAMES = ("trash-put", "trash-rm", "trash-empty", "gtrash", "rmtrash", "trash-cli")
FIXED_REAL_PATHS = ("/usr/bin/trash", "/opt/homebrew/bin/trash", "/usr/local/bin/trash",
                    "/opt/local/bin/trash")
# Present on this host per launchd plists and discovery run4 (live lookup 1100).
DENIED_MACH_SERVICES = (
    ("com.apple.coreservices.appleevents", "Apple Event routing and permission checks"),
    ("com.apple.coreservices.appleevents.aeserver", "remote Apple Events"),
    ("com.apple.DesktopServicesHelper", "privileged Finder copy/move/delete helper"),
    ("com.apple.FileProvider", "fileproviderd provider-domain file operations"),
)
DENIED_MACH_REGEXES = (
    (r"^com\.apple\.[Ff]ile[Pp]rovider\.", "FileProvider sub-services"),
)
# Concrete sub-service names from this host's FileProvider launchd plist; each
# must be denied by the regex above (preflight row), proving it is not only syntax.
MACH_REGEX_PROBES = ("com.apple.FileProvider.cache-delete", "com.apple.fileprovider.pushkit")
NOT_PRESENT_SERVICES = ("com.apple.desktopservicesd", "com.apple.finder*")
TRASH_EXEC_REGEX = r"/[Tt][Rr][Aa][Ss][Hh]$"
FAKE_STORE = "fake-trash"


def owner_trash():
    """The real user Trash, from the account database, never the synthetic HOME."""
    path = Path(pwd.getpwuid(os.getuid()).pw_dir) / ".Trash"
    return sorted({str(path), os.path.realpath(path)})


def discover_real_trash(search_path):
    """Fixed install locations plus every PATH hit for trash names, with realpaths."""
    found = {}
    for candidate in FIXED_REAL_PATHS:
        if os.path.lexists(candidate):
            found[candidate] = os.path.realpath(candidate)
    for directory in (search_path or "").split(os.pathsep):
        if not directory:
            continue
        for name in ("trash", *KNOWN_NAMES):
            candidate = os.path.join(directory, name)
            if os.path.lexists(candidate):
                found[candidate] = os.path.realpath(candidate)
    return found


def profile_rules(control, nonce, real_paths, quote):
    """SBPL appended after the existing fence. Last match wins: the CONTROL literal allow is last."""
    tag = f' (with message "{nonce}")'
    lines = [f"(deny file-read* file-write* (subpath {quote(spelling)}))" for spelling in owner_trash()]
    lines.append("(deny appleevent-send)")
    lines += [f"(deny mach-lookup (global-name {quote(name)}))" for name, _ in DENIED_MACH_SERVICES]
    lines += [f'(deny mach-lookup (global-name-regex #"{regex}"))' for regex, _ in DENIED_MACH_REGEXES]
    lines.append(f'(deny process-exec*{tag} (regex #"{TRASH_EXEC_REGEX}"))')
    lines += [f'(deny process-exec*{tag} (regex #"/{name}$"))' for name in KNOWN_NAMES]
    for path in sorted(set(real_paths) | set(real_paths.values())):
        lines.append(f"(deny process-exec*{tag} (literal {quote(path)}))")
    lines.append(f"(allow process-exec* (literal {quote(str(Path(control).resolve() / 'bin' / 'trash'))}))")
    return lines


def policy_receipt(real_paths, nonce):
    return {"guarantee": "denies known trash executables and Apple Event file operations; "
                         "routes known trash CLI calls to an owned fake",
            "not_claimed": ["prevents all deletion", "detects every trash attempt"],
            "real_trash_paths": real_paths, "owner_trash": owner_trash(), "nonce": nonce,
            "denied_mach_services": [{"name": name, "reason": reason} for name, reason in DENIED_MACH_SERVICES],
            "denied_mach_regexes": [{"regex": regex, "reason": reason} for regex, reason in DENIED_MACH_REGEXES],
            "mach_regex_probes": list(MACH_REGEX_PROBES),
            "recorded_not_present_not_denied": list(NOT_PRESENT_SERVICES),
            "not_denied": ["com.apple.lsd.*", "com.apple.coreservices.launchservicesd"],
            "exec_regex": TRASH_EXEC_REGEX, "known_names": list(KNOWN_NAMES)}


def fake_attestation(root, target):
    roots = [str(Path(root).resolve()), str(Path(target).resolve())]
    return {"roots": roots, "store": str(Path(root).resolve() / FAKE_STORE)}


def install_fake(control, interpreter, root, target):
    """Write CONTROL/bin/trash: absolute attested interpreter and attested roots embedded at install."""
    source = Path(__file__).with_name("hermetic_fake_trash.py").read_text()
    interpreter = Path(interpreter)
    if not interpreter.is_absolute():
        raise ValueError("fake trash interpreter must be absolute")
    marker = "ATTESTED = None\n"
    if source.count(marker) != 1:
        raise ValueError("fake trash source has no single attestation slot")
    body = source.split("\n", 1)[1] if source.startswith("#!") else source
    body = body.replace(marker, f"ATTESTED = {json.dumps(fake_attestation(root, target))}\n")
    return control.write_executable("bin/trash", f"#!{interpreter}\n{body}")


MAX_INVENTORY_ENTRIES = 2000
MAX_INVENTORY_DEPTH = 8
MAX_INVENTORY_ERRORS = 100


def store_inventory(root, *, max_entries=MAX_INVENTORY_ENTRIES, max_depth=MAX_INVENTORY_DEPTH):
    """Bounded, fd-relative listing of the subject-writable store: names, types, sizes only.

    It runs unsandboxed after the subject, so it never follows a symlink, never
    reads contents, stops at fixed bounds, and returns errors as data instead of
    raising. Worst case is about 2000 entries of at most ~2 KiB each, well under
    the 8 MiB receipt bound. The wrapper decision never depends on it.
    """
    store = Path(root).resolve() / FAKE_STORE
    result = {"store": str(store), "entries": [], "errors": [], "truncated": False,
              "bounds": {"entries": max_entries, "depth": max_depth, "errors": MAX_INVENTORY_ERRORS}}

    def error(text):
        if len(result["errors"]) < MAX_INVENTORY_ERRORS:
            result["errors"].append(text)
        else:
            result["truncated"] = True

    try:
        info = os.lstat(store)
    except FileNotFoundError:
        return {**result, "present": False}
    except OSError as failure:
        error(f"lstat store: {failure}")
        return {**result, "present": "unreadable"}
    if not stat.S_ISDIR(info.st_mode):
        return {**result, "present": "not-a-directory",
                "kind": "symlink" if stat.S_ISLNK(info.st_mode) else "other"}
    result["present"] = True
    try:
        top = os.open(store, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    except OSError as failure:
        error(f"open store: {failure}")
        return result
    pending = [(top, "", 0)]
    try:
        while pending and not (result["truncated"] and len(result["entries"]) >= max_entries):
            directory_fd, prefix, depth = pending.pop()
            try:
                try:
                    names = sorted(os.listdir(directory_fd))
                except OSError as failure:
                    error(f"list {prefix or '.'}: {failure}")
                    continue
                for name in names:
                    if len(result["entries"]) >= max_entries:
                        result["truncated"] = True
                        break
                    relative = f"{prefix}{name}"
                    try:
                        entry = os.lstat(name, dir_fd=directory_fd)
                    except OSError as failure:
                        error(f"lstat {relative}: {failure}")
                        continue
                    kind = ("symlink" if stat.S_ISLNK(entry.st_mode) else "dir" if stat.S_ISDIR(entry.st_mode)
                            else "file" if stat.S_ISREG(entry.st_mode) else "other")
                    result["entries"].append({"path": relative, "kind": kind, "size": entry.st_size})
                    if kind != "dir":
                        continue
                    if depth + 1 >= max_depth:
                        result["truncated"] = True
                        continue
                    try:
                        child = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                                        dir_fd=directory_fd)
                    except OSError as failure:
                        error(f"open {relative}: {failure}")
                        continue
                    pending.append((child, relative + "/", depth + 1))
            finally:
                if directory_fd != top:
                    os.close(directory_fd)
    except Exception as failure:  # never raise into the wrapper's finally
        error(f"inventory failed: {type(failure).__name__}: {failure}")
    finally:
        for directory_fd, _, _ in pending:
            if directory_fd != top:
                os.close(directory_fd)
        os.close(top)
    return result
