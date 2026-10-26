#!/usr/bin/env python3
"""Fail if a write-capable release job can run outside this fork."""

from __future__ import annotations

from pathlib import Path

REPOSITORY = "junhoyeo/openprime"
ROOT = Path(__file__).resolve().parents[2]
GUARDED_JOBS = {
    ".github/workflows/nightly.yml": ("cut",),
    ".github/workflows/release-prepare.yml": ("prepare", "tag"),
    ".github/workflows/release.yml": ("promote", "nightly-refresh"),
}


def job_block(text: str, job: str) -> str:
    lines = text.splitlines()
    marker = f"  {job}:"
    try:
        start = lines.index(marker)
    except ValueError as error:
        raise AssertionError(f"missing job {job!r}") from error

    end = len(lines)
    for index in range(start + 1, len(lines)):
        line = lines[index]
        if line.startswith("  ") and not line.startswith("    ") and line.endswith(":"):
            end = index
            break
    return "\n".join(lines[start:end])


def job_condition(block: str, job: str) -> str:
    lines = block.splitlines()
    for index, line in enumerate(lines):
        if line.startswith("    if:"):
            condition = [line.removeprefix("    if:").strip()]
            for continuation in lines[index + 1 :]:
                if not continuation.startswith("      "):
                    break
                condition.append(continuation.strip())
            return " ".join(condition)
    raise AssertionError(f"write-capable job {job!r} has no repository guard")


def main() -> None:
    expected = f"github.repository == '{REPOSITORY}'"
    checked = 0
    for relative, jobs in GUARDED_JOBS.items():
        text = (ROOT / relative).read_text(encoding="utf-8")
        for job in jobs:
            condition = job_condition(job_block(text, job), job)
            if expected not in condition:
                raise AssertionError(
                    f"{relative} job {job!r} must contain {expected!r} in its job-level if; "
                    f"got {condition!r}"
                )
            checked += 1
    print(f"fork release target policy passed: {checked} jobs target {REPOSITORY}")


if __name__ == "__main__":
    main()
