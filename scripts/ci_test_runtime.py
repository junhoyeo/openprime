#!/usr/bin/env python3
"""Print the CI test kernel runtime: the newest product-built kernel generation
under $HOME/.prime/agent/kernel-venv.generations whose bootstrap marker lists the
bundled Python skills.

`--prime-agent-bootstrap` never syncs skills (TS parity: bootstrap-cli/postinstall
callers skip the skill sync), and generations are immutable, so CI first runs one
faux print-mode turn that makes the product publish a skill-bearing generation,
the way a first session does. Tests then pin that interpreter instead of the
skill-less bootstrap one.
"""
import json
import os
from pathlib import Path
import sys


def find(home):
    root = Path(home) / ".prime" / "agent" / "kernel-venv.generations"
    candidates = []
    for generation in root.iterdir() if root.is_dir() else []:
        marker = generation / ".bootstrap-version"
        python = generation / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
        try:
            skills = json.loads(marker.read_text()).get("python_skills") or []
        except (OSError, ValueError):
            continue
        if skills and python.exists():
            candidates.append((marker.stat().st_mtime, python, [s.get("import_name") for s in skills]))
    if not candidates:
        raise SystemExit(f"no skill-bearing kernel generation under {root}")
    _, python, skills = max(candidates)
    return python, skills


if __name__ == "__main__":
    python, skills = find(os.environ["HOME"])
    print(f"test runtime skills: {','.join(skills)}", file=sys.stderr)
    print(python)
