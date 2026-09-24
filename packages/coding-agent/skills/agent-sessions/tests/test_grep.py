"""Regression tests for the grep prefilter and counts mode."""
from __future__ import annotations

import json
import re
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from agent_sessions import sessions  # noqa: E402


class MandatoryLiteralsTest(unittest.TestCase):
    def check(self, pattern: str, expected: list[str]) -> None:
        self.assertEqual(sessions._mandatory_literals(pattern), expected, pattern)

    def test_plain_literal(self) -> None:
        self.check("timeout", ["timeout"])

    def test_optional_last_char_is_dropped(self) -> None:
        # `errors?` matches `error`; probing `errors` would skip such files.
        self.check("errors?", ["error"])
        self.check("errors*", ["error"])
        self.check("errors{0,2}", ["error"])
        self.check("errors{,2}", ["error"])

    def test_plus_and_counted_repeat_keep_the_char(self) -> None:
        self.check("errors+", ["errors"])
        self.check("errors{2}", ["errors"])
        self.check("errors{1,3}", ["errors"])

    def test_groups_classes_and_escapes_break_runs(self) -> None:
        self.check("time(out)?ms", ["time", "ms"])
        self.check("time[o]ut", ["time", "ut"])
        self.check(r"time\.sleep", ["time", "sleep"])
        self.check("[]a]bcd", ["bcd"])
        self.check("(a|b)+xyz", ["xyz"])

    def test_no_mandatory_literal(self) -> None:
        self.check("(foo)?", [])
        self.check(r"\d+", [])

    def test_every_probe_occurs_in_every_match(self) -> None:
        samples = {
            "errors?": ["error", "errors"],
            "colou?r": ["color", "colour"],
            "ab{0,1}cd": ["acd", "abcd"],
            "run(ning)?-fast": ["run-fast", "running-fast"],
        }
        for pattern, texts in samples.items():
            regex = re.compile(pattern)
            for text in texts:
                self.assertTrue(regex.search(text))
                for literal in sessions._mandatory_literals(pattern):
                    self.assertIn(literal, text, (pattern, text, literal))


class GrepCountsTest(unittest.TestCase):
    def test_counts_all_matches_but_bounds_rendered_hits(self) -> None:
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "sessions" / "2026" / "09" / "01"
            root.mkdir(parents=True)
            path = root / "rollout-2026-09-01T00-00-00-0000.jsonl"
            lines = [{"type": "session_meta", "payload": {"id": "abc", "cwd": tmp, "timestamp": "2026-09-01T00:00:00Z"}}]
            for i in range(12):
                lines.append({"type": "response_item", "timestamp": "2026-09-01T00:00:01Z",
                              "payload": {"type": "message", "role": "assistant",
                                          "content": [{"type": "output_text", "text": f"needle {i}"}]}})
            path.write_text("\n".join(json.dumps(line) for line in lines) + "\n")
            results = sessions.grep_sessions("needle", session=str(path), harness="codex", max_per_session=5)
            self.assertEqual(len(results), 1)
            self.assertEqual(results[0]["matched_events"], 12)
            self.assertEqual(len(results[0]["hits"]), 5)


class KiroElisionTest(unittest.TestCase):
    def test_redacted_content_arrays_are_elided(self) -> None:
        numbers = ",".join(str(i % 256) for i in range(400))
        line = '{"redactedContent":[' + numbers + '],"data":[' + numbers + '],"keep":1}'
        out = sessions._KIRO_BYTE_ARRAY.sub(r'"\1":[]', line)
        self.assertEqual(json.loads(out), {"redactedContent": [], "data": [], "keep": 1})


if __name__ == "__main__":
    unittest.main()
