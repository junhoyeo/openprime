"""Run the bundled Python skills' own unittest suites under the runtime job.

The skills under packages/coding-agent/skills ship their tests beside their
sources; nothing else in CI discovers those directories, so this module loads
them into the runtime test run. Modules are imported by path rather than via a
nested ``discover`` because unittest (3.12+) refuses a second top-level dir.
"""
from __future__ import annotations

import importlib.util
import unittest
from pathlib import Path

SKILLS = Path(__file__).parents[2] / "packages/coding-agent/skills"
SUITES = ("agent-sessions", "aside")


def load_tests(loader: unittest.TestLoader, tests: unittest.TestSuite, pattern: str | None) -> unittest.TestSuite:
    for name in SUITES:
        tests_dir = SKILLS / name / "tests"
        files = sorted(tests_dir.glob("test_*.py")) if tests_dir.is_dir() else []
        if not files:
            raise AssertionError(f"bundled skill {name!r} has no tests under {tests_dir}")
        for file in files:
            spec = importlib.util.spec_from_file_location(f"bundled_skill_{name.replace('-', '_')}_{file.stem}", file)
            assert spec and spec.loader
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
            tests.addTests(loader.loadTestsFromModule(module))
    return tests
