"""Regression tests for the grep prefilter and counts mode."""
from __future__ import annotations

import asyncio
import json
import os
import sqlite3
import tempfile
import re
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from agent_sessions import run, sessions  # noqa: E402


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


class AsideTest(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.home = Path(temporary.name)
        self.root = self.home / ".aside/u/0/sessions/2026-10-21_AsideSession"
        self.artifacts = self.root / "artifacts/lecture-transcripts"
        self.artifacts.mkdir(parents=True)
        self.chat = self.root / "messages.jsonl"
        records = [
            {"role": "turn-lifecycle", "event": "started", "timestamp": 1792500000000},
            {"role": "user", "content": "Find 강의 notes at École", "timestamp": 1792500000001},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "Read source", "thinkingSignature": "opaque-secret"},
                {"type": "toolCall", "name": "ipython", "arguments": {"code": "print('강의')"}},
            ]},
            {"role": "toolResult", "toolName": "ipython", "content": [
                {"type": "text", "text": "강의 output"}, {"type": "image", "data": "opaque-secret"},
            ], "details": {"elapsedMs": 12.9}},
            {"role": "toolResult", "toolName": "bash", "content": [], "isError": True,
             "errorMessage": "failed", "details": None, "durationMs": 5.9, "timing": {"durationMs": 6}},
            {"role": "toolResult", "toolName": "read", "content": "result", "timing": {"durationMs": 7.2}},
            {"role": "steering", "content": "Keep reading"},
            {"role": "system-message", "content": "Recovered", "kind": "recovery"},
            {"role": "assistant", "content": [{"type": "text", "text": "강의 summary"}]},
            {"role": "assistant", "content": "Plain reply"},
            {"role": "compaction", "summary": "old needle context"},
            {"role": "user-message-metadata", "attachments": []},
        ]
        self.chat.write_text("\n".join(json.dumps(row) for row in records) + '\n{"role":', encoding="utf-8")
        self.text = self.artifacts / "강의.txt"
        self.text.write_text("Heading École\n\n[00:01] 강의 artifact-only\nNext line\n", encoding="utf-8")
        (self.artifacts / "강의.vtt").write_text("WEBVTT\n\n00:00.000 --> 00:01.000\n강의 artifact-only\n", encoding="utf-8")
        (self.artifacts / "notes.MD").write_text("# nested notes", encoding="utf-8")
        (self.artifacts / "image.png").write_bytes(b"artifact-only")
        (self.artifacts / "binary.txt").write_bytes(b"artifact-only\0bytes")
        (self.artifacts / "linked.txt").symlink_to(self.text)
        (self.root / "tmp").mkdir()
        (self.root / "tmp/noise.txt").write_text("artifact-only")
        home_patch = patch.object(sessions, "HOME", self.home)
        home_patch.start()
        self.addCleanup(home_patch.stop)

    def test_chat_events_unicode_and_optional_fields(self) -> None:
        parsed = sessions.load_session(self.chat)
        self.assertEqual((parsed.harness, parsed.session_id, parsed.cwd), ("aside", "AsideSession", str(self.root)))
        self.assertEqual([event.kind for event in parsed.events], [
            "user", "reasoning", "tool_call", "tool_result", "tool_result", "tool_result",
            "user", "system", "assistant", "assistant", "system",
        ])
        self.assertEqual([event.duration_ms for event in parsed.events[3:6]], [12, 5, 7])
        self.assertEqual(parsed.events[4].name, "bash:error")
        self.assertEqual(parsed.events[4].text, "failed")
        self.assertTrue(parsed.events[0].timestamp.startswith("2026-"))
        self.assertEqual(parsed.events[2].text, "print('강의')")
        self.assertEqual(sessions.grep_sessions("opaque-secret", session=self.chat), [])
        self.assertEqual(sessions.grep_sessions("old needle", session=self.chat, kinds="system")[0]["hits"][0]["name"], "compaction")
        hits = sessions.grep_sessions("강의", session=self.chat)[0]["hits"]
        self.assertEqual({hit["kind"] for hit in hits}, {"user", "tool_call", "tool_result", "assistant"})
        self.assertEqual(len(sessions.grep_sessions("école", session=self.chat)), 1)
        self.assertEqual(len(sessions.grep_sessions("École", session=self.chat)), 1)

    def test_discovery_and_scoped_grep_include_artifact_only_sessions_and_profiles(self) -> None:
        orphan = self.home / ".aside/u/1/sessions/2026-10-20_Orphan/artifacts/notes.md"
        orphan.parent.mkdir(parents=True)
        orphan.write_text("artifact-only")
        rows = sessions.find_sessions(harness="aside", root=self.home, limit=0)
        self.assertEqual({row["path"] for row in rows}, {
            str(self.chat), str(self.text), str(self.artifacts / "강의.vtt"), str(self.artifacts / "notes.MD"), str(orphan),
        })
        self.assertEqual(len(sessions.grep_sessions("artifact-only", harness="all")), 3)
        for target in ("AsideSession", "AsideS", self.root, self.artifacts):
            with self.subTest(target=target):
                hits = sessions.grep_sessions("artifact-only", session=target, harness="aside")
                self.assertEqual({hit["path"] for hit in hits}, {str(self.text), str(self.artifacts / "강의.vtt")})
        self.assertEqual(len(sessions.grep_sessions("artifact-only", session="Orphan")), 1)
        self.assertEqual(sessions.resolve_session("AsideSession")[1], self.chat)
        self.assertEqual(sessions.resolve_session(self.root)[1], self.chat)
        self.assertEqual(len(sessions.grep_sessions("강의", session=self.text)), 1)
        self.assertEqual(len(sessions.grep_sessions("école", session=self.text, fixed=True)), 1)
        self.assertEqual(len(sessions.grep_sessions("École", session=self.text, fixed=True)), 1)
        self.assertEqual(len(sessions.grep_sessions(r"\u00c9cole", session=self.text)), 1)
        self.assertEqual(sessions.grep_sessions("강의", session=self.text, kinds="chat"), [])
        self.assertEqual(len(sessions.find_sessions(harness="aside", project="lecture-transcripts")), 3)
        os.utime(self.text, (0, 0))
        self.assertNotIn(str(self.text), {row["path"] for row in sessions.find_sessions(harness="aside", days=1)})
        for path in (self.artifacts / "binary.txt", self.artifacts / "linked.txt", self.root / "tmp/noise.txt"):
            with self.subTest(path=path), self.assertRaises(sessions.SessionError):
                sessions.load_session(path)
        lowercase = self.artifacts / "lowercase.txt"
        lowercase.write_text("école lowercase only", encoding="utf-8")
        for fixed in (False, True):
            with self.subTest(fixed=fixed):
                self.assertEqual(sessions.grep_sessions("École", session=lowercase, fixed=fixed), [])
                self.assertEqual(len(sessions.grep_sessions("école", session=lowercase, fixed=fixed)), 1)

    def test_artifact_names_cannot_shadow_session_ids(self) -> None:
        other = self.home / ".aside/u/1/sessions/2026-10-20_Other/artifacts"
        other.mkdir(parents=True)
        for name in ("AsideSession", "Orphan", "codex-id"):
            (other / f"{name}.txt").write_text("wrong session")
            os.utime(other / f"{name}.txt", (2000000000, 2000000000))
        orphan = self.root.with_name("2026-10-21_Orphan") / "artifacts/notes.txt"
        orphan.parent.mkdir(parents=True)
        orphan.write_text("orphan needle")
        codex = self.home / ".codex/sessions/2026/10/21/rollout-codex-id.jsonl"
        codex.parent.mkdir(parents=True)
        codex.write_text('{}\n')
        for target, expected in (("AsideSession", self.chat), ("Orphan", orphan), ("codex-id", codex)):
            with self.subTest(target=target):
                self.assertEqual(sessions.resolve_session(target)[1], expected)
        self.assertEqual(sessions.grep_sessions("needle", session="Orphan")[0]["path"], str(orphan))

    def test_relative_files_and_artifact_boundary(self) -> None:
        previous = Path.cwd()
        try:
            os.chdir(self.artifacts)
            self.assertEqual(sessions.load_session(self.text.name).path.resolve(), self.text.resolve())
            self.assertEqual(len(sessions.grep_sessions("artifact-only", session=self.text.name)), 1)
        finally:
            os.chdir(previous)
        escape = self.root / "artifacts/../tmp"
        for target in (escape, escape / "noise.txt"):
            with self.subTest(target=target), self.assertRaises(sessions.SessionError):
                sessions.grep_sessions("artifact-only", session=target)
        with self.assertRaises(sessions.SessionError):
            sessions.load_session(escape / "noise.txt")

    def test_tool_results_ignore_non_object_optional_metadata(self) -> None:
        for metadata in (None, "opaque", ["opaque"]):
            with self.subTest(metadata=metadata):
                self.chat.write_text(json.dumps({"role": "toolResult", "toolName": "read", "content": "output",
                                                "details": metadata, "timing": metadata, "durationMs": 5}))
                event = sessions.load_session(self.chat).events[0]
                self.assertEqual((event.text, event.duration_ms), ("output", 5))

    def test_artifact_lines_show_context_counts_and_cache_refresh(self) -> None:
        hits = sessions.grep_sessions("artifact-only", session=self.text, context=1)
        self.assertEqual(hits[0]["source"], "artifact")
        self.assertEqual(hits[0]["hits"][0]["index"], 3)
        self.assertEqual([event["index"] for event in hits[0]["hits"][0]["context"]], [1, 4])
        shown = asyncio.run(run("show", session=str(self.text)))
        self.assertIn("[3] ARTIFACT: [00:01] 강의 artifact-only", shown)
        self.assertEqual(asyncio.run(run("grep", "강의", session=str(self.text), files_only=True)).splitlines()[0], str(self.text))
        self.assertEqual(asyncio.run(run("grep", "강의", session=str(self.text), counts=True)).splitlines()[0], f"{self.text}: 1")
        self.text.write_text("Updated artifact-only\n", encoding="utf-8")
        self.assertEqual(sessions.load_session(self.text).events[0].text, "Updated artifact-only")

    def test_optional_read_only_database_metadata_and_fresh_updates(self) -> None:
        database = self.root.parent.parent / "state.db"
        self.assertFalse(database.exists())
        sessions.load_session(self.chat)
        self.assertFalse(database.exists())
        connection = sqlite3.connect(database)
        self.addCleanup(connection.close)
        connection.execute("PRAGMA journal_mode=WAL")
        connection.execute("CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, title TEXT, created_at REAL, parent_id TEXT)")
        connection.execute("INSERT INTO sessions VALUES (?, ?, ?, ?, ?)",
                           ("AsideSession", "/projects/tokscale", "Lecture notes", 1792500000, "parent"))
        connection.commit()
        parsed = sessions.load_session(self.chat)
        self.assertEqual((parsed.cwd, parsed.title, parsed.parent_id), ("/projects/tokscale", "Lecture notes", "parent"))
        self.assertEqual(len(sessions.find_sessions(harness="aside", project="tokscale")), 4)
        connection.execute("UPDATE sessions SET title = 'Renamed'")
        connection.commit()
        self.assertEqual(sessions.load_session(self.chat).title, "Renamed")


if __name__ == "__main__":
    unittest.main()
