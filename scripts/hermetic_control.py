"""Parent-controlled, descriptor-relative guard evidence (stdlib only)."""
import json
import os
from pathlib import Path

MAX_RECEIPT_BYTES = 8 * 1024 * 1024


class ControlDirectory:
    """Hold the original directory inode, independent of later path replacement."""
    def __init__(self, path):
        self.path = Path(path)
        self.fd = os.open(self.path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        os.set_inheritable(self.fd, False)
        info = os.fstat(self.fd)
        if info.st_uid != os.getuid() or info.st_mode & 0o022:
            os.close(self.fd)
            raise RuntimeError("control directory must be private and owned")
        self.identity = {"device": info.st_dev, "inode": info.st_ino, "uid": info.st_uid}

    def create_file(self, name):
        if not isinstance(name, str) or name in ("", ".", "..") or "/" in name or "\\" in name:
            raise ValueError("controller file names must be single fixed components")
        fd = os.open(name, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                     0o600, dir_fd=self.fd)
        os.set_inheritable(fd, False)
        return os.fdopen(fd, "w+b")

    def write_text(self, name, text):
        data = text.encode("utf-8")
        if len(data) > MAX_RECEIPT_BYTES:
            raise ValueError("controller receipt exceeds its bound")
        with self.create_file(name) as stream:
            stream.write(data)

    def close(self):
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


def write_receipt(control, name, value):
    """Publish once beneath the held directory fd, never a subject-supplied path."""
    control.write_text(name, json.dumps(value, indent=2) + "\n")
