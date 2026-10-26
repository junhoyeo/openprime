"""Grok Build (`~/.grok/sessions/.../updates.jsonl`) parsing, on a synthetic fixture."""
from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from agent_sessions import sessions  # noqa: E402


def _envelope(update: dict, *, at_ms: int, timestamp: int = 1_758_000_000) -> dict:
    """One ACP `session/update` line: `_meta` sits beside `update`, not inside it."""
    return {
        "timestamp": timestamp,
        "method": "session/update",
        "params": {
            "sessionId": "01a0af03-6a1f-7f63-a272-abd11322be58",
            "update": update,
            "_meta": {"eventId": "evt-1", "agentTimestampMs": at_ms, "promptId": "p-1"},
        },
    }


UPDATES = [
    _envelope({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "list the skills"}}, at_ms=1_000),
    _envelope({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "Need to read the directory."}}, at_ms=1_500),
    _envelope(
        {
            "sessionUpdate": "tool_call",
            "toolCallId": "call-read",
            "title": "Read SKILL.md",
            "rawInput": {"target_file": "SKILL.md"},
            "_meta": {"x.ai/tool": {"name": "read_file"}},
        },
        at_ms=2_000,
    ),
    # progress label: status is null and it must not become the tool_result
    _envelope({"sessionUpdate": "tool_call_update", "toolCallId": "call-read", "status": None, "title": "Reading SKILL.md"}, at_ms=2_100),
    _envelope(
        {
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-read",
            "status": "completed",
            "content": [{"type": "content", "content": {"type": "text", "text": "name: demo-skill"}}],
        },
        at_ms=2_750,
    ),
    _envelope({"sessionUpdate": "tool_call", "toolCallId": "call-miss", "rawInput": {"target_file": "nope.md"}, "_meta": {"x.ai/tool": {"name": "read_file"}}}, at_ms=3_000),
    _envelope(
        {
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-miss",
            "status": "failed",
            "content": [{"type": "content", "content": {"type": "text", "text": "File not found: nope.md"}}],
        },
        at_ms=3_400,
    ),
    # a call whose only name source is the prose `title` (no `_meta["x.ai/tool"]`)
    _envelope({"sessionUpdate": "tool_call", "toolCallId": "call-search", "title": "Web search:", "rawInput": {"query": "acp spec"}}, at_ms=3_500),
    _envelope(
        {
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-search",
            "status": "completed",
            "content": [{"type": "content", "content": {"type": "text", "text": "one result"}}],
        },
        at_ms=3_600,
    ),
    _envelope({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "One skill: demo-skill."}}, at_ms=4_000),
    # bookkeeping, skipped like Prime's agent_status
    _envelope({"sessionUpdate": "plan", "entries": []}, at_ms=4_100),
    _envelope({"sessionUpdate": "turn_completed", "usage": {"totalTokens": 12}}, at_ms=4_200),
    _envelope({"sessionUpdate": "background_tasks", "tasks": []}, at_ms=4_300),
]

SUMMARY = {
    "info": {"id": "01a0af03-6a1f-7f63-a272-abd11322be58", "cwd": "/Users/dev/demo-repo"},
    "generated_title": "Audit the bundled skills",
    "session_summary": "fallback title",
    "created_at": "2026-09-16T04:00:00.000000Z",
}


class GrokFixture:
    """A `~/.grok/sessions/<url-encoded-cwd>/<uuid>/` directory under a temp HOME."""

    def __init__(self, tmp: Path, *, summary: dict | None = SUMMARY, cwd_dir: str = "%2FUsers%2Fdev%2Fdemo-repo") -> None:
        self.home = tmp
        self.dir = tmp / ".grok" / "sessions" / cwd_dir / "01a0af03-6a1f-7f63-a272-abd11322be58"
        self.dir.mkdir(parents=True)
        self.path = self.dir / "updates.jsonl"
        self.path.write_text("".join(json.dumps(line) + "\n" for line in UPDATES))
        # state files that share the directory and must not be picked up
        (self.dir / "chat_history.jsonl").write_text('{"role":"user"}\n')
        (self.dir / "events.jsonl").write_text('{"kind":"noise"}\n')
        (self.dir / "usage.json").write_text("{}")
        if summary is not None:
            (self.dir / "summary.json").write_text(json.dumps(summary))

    def load(self) -> sessions.Session:
        return sessions.load_session(self.path, harness="grok", use_cache=False)


class GrokParseTest(unittest.TestCase):
    def test_events_and_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            session = GrokFixture(Path(tmp)).load()
        self.assertEqual(session.harness, "grok")
        self.assertEqual(session.session_id, "01a0af03-6a1f-7f63-a272-abd11322be58")
        self.assertEqual(session.cwd, "/Users/dev/demo-repo")
        self.assertEqual(session.title, "Audit the bundled skills")
        self.assertEqual(session.started, "2026-09-16T04:00:00.000000Z")
        self.assertEqual(
            [(event.kind, event.name) for event in session.events],
            [
                ("user", ""),
                ("reasoning", ""),
                ("tool_call", "read_file"),
                ("tool_result", "read_file"),
                ("tool_call", "read_file"),
                ("tool_result", "read_file:error"),
                ("tool_call", "Web search:"),
                ("tool_result", "Web search:"),
                ("assistant", ""),
            ],
        )
        self.assertEqual([event.index for event in session.events], [1, 2, 3, 4, 5, 6, 7, 8, 9])
        self.assertEqual(session.events[0].text, "list the skills")
        self.assertEqual(session.events[1].text, "Need to read the directory.")
        self.assertEqual(json.loads(session.events[2].text), {"target_file": "SKILL.md"})
        self.assertEqual(session.events[-1].text, "One skill: demo-skill.")

    def test_tool_results_carry_output_and_paired_duration(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            session = GrokFixture(Path(tmp)).load()
        results = [event for event in session.events if event.kind == "tool_result"]
        self.assertEqual(
            [event.text for event in results],
            ["name: demo-skill", "File not found: nope.md", "one result"],
        )
        # terminal update minus the initiating tool_call, both from agentTimestampMs
        self.assertEqual([event.duration_ms for event in results], [750, 400, 100])

    def test_tool_name_prefers_the_x_ai_meta_over_the_prose_title(self) -> None:
        """`_meta["x.ai/tool"].name` is the canonical id; `title` is only the fallback."""
        with tempfile.TemporaryDirectory() as tmp:
            session = GrokFixture(Path(tmp)).load()
        self.assertEqual(session.events[2].name, "read_file")  # title "Read SKILL.md" loses
        self.assertEqual(session.events[4].name, "read_file")  # no title, meta name used
        self.assertEqual(session.events[6].name, "Web search:")  # no meta, prose title used

    def test_timestamps_prefer_agent_timestamp_ms(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            session = GrokFixture(Path(tmp)).load()
        expected = sessions.datetime.fromtimestamp(1.0, tz=sessions.timezone.utc).astimezone().isoformat(timespec="seconds")
        self.assertEqual(session.events[0].timestamp, expected)

    def test_missing_summary_falls_back_to_the_directory_names(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            session = GrokFixture(Path(tmp), summary=None).load()
        self.assertEqual(session.session_id, "01a0af03-6a1f-7f63-a272-abd11322be58")
        self.assertEqual(session.cwd, "/Users/dev/demo-repo")  # percent-decoded directory name
        self.assertEqual(session.title, "")
        self.assertEqual(session.started, "")

    def test_session_summary_is_the_title_fallback(self) -> None:
        summary = {**SUMMARY}
        del summary["generated_title"]
        with tempfile.TemporaryDirectory() as tmp:
            session = GrokFixture(Path(tmp), summary=summary).load()
        self.assertEqual(session.title, "fallback title")


class GrokDiscoveryTest(unittest.TestCase):
    def test_find_sessions_lists_only_updates_jsonl(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            fixture = GrokFixture(Path(tmp))
            rows = sessions.find_sessions(harness="grok", root=tmp)
        self.assertEqual([row["path"] for row in rows], [str(fixture.path)])
        self.assertEqual(rows[0]["harness"], "grok")
        self.assertEqual(rows[0]["session_id"], "01a0af03-6a1f-7f63-a272-abd11322be58")
        self.assertEqual(rows[0]["cwd"], "/Users/dev/demo-repo")
        self.assertEqual(rows[0]["title"], "Audit the bundled skills")

    def test_project_filter_matches_the_decoded_cwd(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            GrokFixture(Path(tmp))
            hits = sessions.find_sessions(harness="grok", project="demo-repo", root=tmp)
            misses = sessions.find_sessions(harness="grok", project="other-repo", root=tmp)
        self.assertEqual(len(hits), 1)
        self.assertEqual(misses, [])

    def test_grok_is_a_known_harness(self) -> None:
        self.assertIn("grok", sessions.HARNESSES)
        self.assertIn("grok", sessions.PARSERS)
        self.assertEqual(sessions._harness_names("grok"), ["grok"])

    def test_grep_finds_a_grok_event(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            fixture = GrokFixture(Path(tmp))
            results = sessions.grep_sessions("demo-skill", session=str(fixture.path), harness="grok")
        self.assertEqual(len(results), 1)
        kinds = {hit["kind"] for hit in results[0]["hits"]}
        self.assertEqual(kinds, {"tool_result", "assistant"})


if __name__ == "__main__":
    unittest.main()
