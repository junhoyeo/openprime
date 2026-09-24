"""Cross-harness agent session transcript reader (Codex, Claude Code, Kimi, Kiro, Prime)."""

from __future__ import annotations

import json
import os
import re
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Callable, Iterable, Iterator, Sequence

HOME = Path.home()

HARNESS_GLOBS: dict[str, list[str]] = {
    # Codex CLI: ~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl
    "codex": [".codex/sessions/*/*/*/rollout-*.jsonl"],
    # Claude Code: ~/.claude/projects/<flattened-cwd>/<uuid>.jsonl
    "claude": [".claude/projects/*/*.jsonl"],
    # Kimi CLI: ~/.kimi/sessions/<group>/<uuid>/context.jsonl
    "kimi": [".kimi/sessions/*/*/context.jsonl"],
    # Kimi Code: ~/.kimi-code/sessions/<workdir>/<session>/agents/<agent>/wire.jsonl
    "kimi-code": [".kimi-code/sessions/*/*/agents/*/wire.jsonl"],
    # Kiro CLI: ~/.kiro/sessions/cli/<uuid>.jsonl, beside a <uuid>.json metadata
    # sidecar. The sibling .history/.lock/.json files are not transcripts, and
    # the glob already excludes them.
    "kiro-cli": [".kiro/sessions/cli/*.jsonl"],
    # Prime Agent: ~/.prime/agent/sessions/<uuid>.jsonl (root threads) and
    # ~/.prime/agent/session-artifacts/<parent>/sub-<child>/<uuid>.jsonl (RLM
    # sub-agents; `**` also covers deeper nesting).
    "prime": [
        ".prime/agent/sessions/*.jsonl",
        ".prime/agent/session-artifacts/**/sub-*/*.jsonl",
    ],
    # Senpi: ~/.senpi/agent/sessions/<flattened-cwd>/<ISO-ts>_<uuid>.jsonl. Senpi is built from
    # the same coding-agent package as Prime, so the record shape is identical (`session`,
    # `session_info`, `message`, `custom`) and `_parse_prime` reads it unchanged. The tight
    # `*/*.jsonl` matters: it excludes the sibling `<cwd>/extensions/goal/*.history.jsonl`
    # files, which are extension state and not transcripts.
    "senpi": [".senpi/agent/sessions/*/*.jsonl"],
}

HARNESSES = tuple(HARNESS_GLOBS)

# The home directory owning each harness, derived from its glob so a name that
# differs from its directory (`kiro-cli` -> `.kiro`) still resolves.
_HARNESS_DIRS: dict[str, str] = {
    name: patterns[0].split("/", 1)[0] for name, patterns in HARNESS_GLOBS.items()
}

# Bookkeeping files that sit next to Prime transcripts but are not transcripts.
_NON_TRANSCRIPTS = {"rlm-subagents.jsonl"}


class SessionError(RuntimeError):
    """Raised when a session cannot be located or parsed."""


@dataclass
class Event:
    index: int
    kind: str  # user | assistant | reasoning | tool_call | tool_result | system | meta
    text: str = ""
    name: str = ""
    timestamp: str = ""
    duration_ms: int | None = None  # wall time of a tool call, when recorded

    def as_dict(self) -> dict[str, Any]:
        data = {
            "index": self.index,
            "kind": self.kind,
            "name": self.name,
            "timestamp": self.timestamp,
            "text": self.text,
        }
        if self.duration_ms is not None:
            data["duration_ms"] = self.duration_ms
        return data


@dataclass
class Session:
    harness: str
    path: Path
    session_id: str = ""
    parent_id: str = ""
    source: str = ""
    cwd: str = ""
    title: str = ""
    started: str = ""
    modified: str = ""
    size: int = 0
    events: list[Event] = field(default_factory=list)

    def as_dict(self, with_events: bool = False) -> dict[str, Any]:
        data = {
            "harness": self.harness,
            "path": str(self.path),
            "session_id": self.session_id,
            "parent_id": self.parent_id,
            "source": self.source,
            "cwd": self.cwd,
            "title": self.title,
            "started": self.started,
            "modified": self.modified,
            "size": self.size,
        }
        if with_events:
            data["events"] = [event.as_dict() for event in self.events]
        return data


# --------------------------------------------------------------------------
# generic helpers
# --------------------------------------------------------------------------


_KIMI_SLUG = re.compile(r"^wd_(?P<name>.+)_[0-9a-f]{12}$")


def _kimi_project(slug: str) -> str:
    """`wd_my-app_0f1e2d3c4b5a` -> `my-app` (the path itself is not stored)."""
    match = _KIMI_SLUG.match(slug)
    return match.group("name") if match else slug


def _glob_regex(pattern: str) -> re.Pattern[str]:
    """Translate a home-relative glob (with `**`) into a full-match regex."""
    parts: list[str] = []
    for segment in pattern.split("/"):
        if segment == "**":
            parts.append("(?:[^/]+/)*")
            continue
        parts.append("".join("[^/]*" if char == "*" else re.escape(char) for char in segment) + "/")
    return re.compile("".join(parts).rstrip("/") + r"\Z")


_GLOB_REGEXES: dict[str, list[re.Pattern[str]]] = {
    name: [_glob_regex(pattern) for pattern in patterns] for name, patterns in HARNESS_GLOBS.items()
}


def _is_transcript(path: Path) -> bool:
    return path.name not in _NON_TRANSCRIPTS


def _iter_harness_paths(base: Path, harness: str) -> Iterator[Path]:
    """Every transcript file of one harness under `base`, bookkeeping excluded."""
    seen: set[Path] = set()
    for pattern in HARNESS_GLOBS[harness]:
        for path in base.glob(pattern):
            if path in seen or not _is_transcript(path):
                continue
            seen.add(path)
            yield path


def _harness_for_path(path: Path) -> str | None:
    """Which harness owns an absolute transcript path, by glob shape."""
    try:
        relative = path.resolve().relative_to(HOME.resolve()).as_posix()
    except (OSError, ValueError):
        return None
    for name, regexes in _GLOB_REGEXES.items():
        if any(regex.match(relative) for regex in regexes) and _is_transcript(path):
            return name
    return None


def _iso(ts: float) -> str:
    return datetime.fromtimestamp(ts, tz=timezone.utc).astimezone().strftime("%Y-%m-%d %H:%M")


def _read_jsonl(path: Path, limit: int | None = None) -> Iterator[dict[str, Any]]:
    with path.open("r", encoding="utf-8", errors="replace") as handle:
        for count, line in enumerate(handle):
            if limit is not None and count >= limit:
                return
            line = line.strip()
            if not line:
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(record, dict):
                yield record


def _flatten_text(content: Any) -> str:
    """Best-effort text extraction from provider content blocks."""
    if content is None:
        return ""
    if isinstance(content, str):
        return content
    if isinstance(content, dict):
        for key in ("text", "think", "thinking", "content", "message", "summary", "output"):
            if key in content:
                value = content[key]
                if isinstance(value, (str, list, dict)):
                    inner = _flatten_text(value)
                    if inner:
                        return inner
        if content.get("type") == "image":
            return "[image]"
        return ""
    if isinstance(content, (list, tuple)):
        parts = [_flatten_text(item) for item in content]
        return "\n".join(part for part in parts if part)
    return str(content)


def _tag(kind: str, name: str = "", duration_ms: int | None = None) -> str:
    """`TOOL_RESULT(ipython, 493ms)` — the duration is shown when recorded."""
    detail = ", ".join(part for part in (name, f"{duration_ms}ms" if duration_ms is not None else "") if part)
    return kind.upper() + (f"({detail})" if detail else "")


def _squeeze(text: str) -> str:
    return re.sub(r"\n{3,}", "\n\n", (text or "").strip())


def _clip(text: str, limit: int) -> str:
    text = text or ""
    if limit <= 0 or len(text) <= limit:
        return text
    return text[:limit].rstrip() + f"… [+{len(text) - limit} chars]"


# --------------------------------------------------------------------------
# per-harness parsers
# --------------------------------------------------------------------------


def _parse_codex(path: Path, records: Iterable[dict[str, Any]], session: Session) -> None:
    index = 0
    for record in records:
        rtype = record.get("type")
        payload = record.get("payload") or {}
        stamp = record.get("timestamp", "")
        if rtype == "session_meta":
            # A rollout's own thread id is `id`; `session_id` points at the root
            # thread, which differs for sub-agent rollouts.
            own = payload.get("id") or payload.get("session_id") or ""
            root = payload.get("session_id") or ""
            session.session_id = own
            session.parent_id = root if root and root != own else ""
            session.source = payload.get("thread_source", "")
            session.cwd = payload.get("cwd", "")
            session.started = payload.get("timestamp") or stamp
            continue
        if rtype == "event_msg" and payload.get("type") == "user_message":
            index += 1
            session.events.append(
                Event(index, "user", _squeeze(_flatten_text(payload.get("message"))), timestamp=stamp)
            )
            continue
        if rtype != "response_item":
            continue
        ptype = payload.get("type")
        if ptype == "message":
            role = payload.get("role", "")
            text = _squeeze(_flatten_text(payload.get("content")))
            if not text:
                continue
            if role == "assistant":
                index += 1
                session.events.append(Event(index, "assistant", text, timestamp=stamp))
            elif role in {"developer", "system"}:
                index += 1
                session.events.append(Event(index, "system", text, name=role, timestamp=stamp))
            # codex duplicates user turns in event_msg; skip role == "user" here
        elif ptype == "reasoning":
            text = _squeeze(_flatten_text(payload.get("summary")))
            if text:
                index += 1
                session.events.append(Event(index, "reasoning", text, timestamp=stamp))
        elif ptype in {"function_call", "custom_tool_call", "local_shell_call"}:
            index += 1
            args = payload.get("arguments")
            if args is None:
                args = payload.get("input")
            session.events.append(
                Event(
                    index,
                    "tool_call",
                    _squeeze(_flatten_text(args) if not isinstance(args, str) else args),
                    name=payload.get("name") or ptype,
                    timestamp=stamp,
                )
            )
        elif ptype in {"function_call_output", "custom_tool_call_output", "local_shell_call_output"}:
            index += 1
            session.events.append(
                Event(index, "tool_result", _squeeze(_flatten_text(payload.get("output"))), timestamp=stamp)
            )
        elif ptype == "agent_message":
            index += 1
            session.events.append(
                Event(index, "assistant", _squeeze(_flatten_text(payload)), name="agent_message", timestamp=stamp)
            )


def _parse_claude(path: Path, records: Iterable[dict[str, Any]], session: Session) -> None:
    index = 0
    session.session_id = path.stem
    for record in records:
        rtype = record.get("type")
        stamp = record.get("timestamp", "")
        if record.get("cwd") and not session.cwd:
            session.cwd = record["cwd"]
        if record.get("sessionId"):
            session.session_id = record["sessionId"]
        if rtype == "ai-title" and record.get("aiTitle"):
            session.title = record["aiTitle"]
            continue
        if rtype == "summary" and record.get("summary"):
            session.title = session.title or record["summary"]
            continue
        if rtype not in {"user", "assistant", "system"}:
            continue
        message = record.get("message") or {}
        role = message.get("role") or rtype
        content = message.get("content", record.get("content"))
        if isinstance(content, list):
            for block in content:
                if not isinstance(block, dict):
                    continue
                btype = block.get("type")
                if btype == "text":
                    text = _squeeze(block.get("text", ""))
                    if text:
                        index += 1
                        session.events.append(Event(index, role, text, timestamp=stamp))
                elif btype == "thinking":
                    text = _squeeze(block.get("thinking", ""))
                    if text:
                        index += 1
                        session.events.append(Event(index, "reasoning", text, timestamp=stamp))
                elif btype == "tool_use":
                    index += 1
                    session.events.append(
                        Event(
                            index,
                            "tool_call",
                            _squeeze(json.dumps(block.get("input", {}), ensure_ascii=False)),
                            name=block.get("name", ""),
                            timestamp=stamp,
                        )
                    )
                elif btype == "tool_result":
                    index += 1
                    session.events.append(
                        Event(index, "tool_result", _squeeze(_flatten_text(block.get("content"))), timestamp=stamp)
                    )
        else:
            text = _squeeze(_flatten_text(content))
            if text:
                index += 1
                session.events.append(Event(index, role, text, timestamp=stamp))
    if not session.cwd:
        session.cwd = path.parent.name


def _parse_kimi(path: Path, records: Iterable[dict[str, Any]], session: Session) -> None:
    """Kimi Code wire.jsonl and Kimi CLI context.jsonl."""
    index = 0
    if path.name == "wire.jsonl":
        # ~/.kimi-code/sessions/<workdir>/<session>/agents/<agent>/wire.jsonl
        session.session_id = path.parents[2].name
    else:
        session.session_id = path.parent.name
    for record in records:
        rtype = record.get("type")
        stamp = record.get("time") or record.get("created_at") or ""
        if isinstance(stamp, (int, float)):
            stamp = datetime.fromtimestamp(stamp / 1000, tz=timezone.utc).astimezone().isoformat(timespec="seconds")
        if rtype == "metadata" and not session.started:
            session.started = str(stamp)
            continue
        if rtype == "turn.prompt":
            text = _squeeze(_flatten_text(record.get("input")))
            if text:
                index += 1
                session.events.append(Event(index, "user", text, timestamp=str(stamp)))
            continue
        if rtype == "turn.steer":
            text = _squeeze(_flatten_text(record.get("input") or record.get("message")))
            if text:
                index += 1
                session.events.append(Event(index, "user", text, name="steer", timestamp=str(stamp)))
            continue
        if rtype == "context.append_message":
            message = record.get("message") or {}
            if message.get("role") == "user" and message.get("origin", {}).get("kind") == "user":
                continue  # already captured by turn.prompt
            continue
        if rtype != "context.append_loop_event":
            continue
        event = record.get("event") or {}
        etype = event.get("type")
        if etype == "content.part":
            part = event.get("part") or {}
            ptype = part.get("type")
            text = _squeeze(_flatten_text(part))
            if not text:
                continue
            index += 1
            kind = "reasoning" if ptype in {"think", "thinking"} else "assistant"
            session.events.append(Event(index, kind, text, timestamp=str(stamp)))
        elif etype == "tool.call":
            index += 1
            session.events.append(
                Event(
                    index,
                    "tool_call",
                    _squeeze(json.dumps(event.get("args", {}), ensure_ascii=False)),
                    name=event.get("name", ""),
                    timestamp=str(stamp),
                )
            )
        elif etype == "tool.result":
            result = event.get("result") or {}
            index += 1
            session.events.append(
                Event(index, "tool_result", _squeeze(_flatten_text(result)), timestamp=str(stamp))
            )
    if not session.cwd and path.name == "wire.jsonl":
        session.cwd = _kimi_project(path.parents[3].name)
        if path.parents[0].name != "main":
            session.source = f"sub-agent {path.parents[0].name}"


# Kiro inlines image bytes and redacted-thinking payloads as raw JSON integer
# arrays, which pushes single lines past 16 MB. Eliding them before json.loads
# keeps a transcript costing its text rather than its pixels.
# `data` carries inline image bytes; `redactedContent` carries encrypted thinking.
_KIRO_BYTE_ARRAY = re.compile(r'"(data|redactedContent)":\s*\[[\d,\s]{256,}\]')
_KIRO_WIDE_LINE = 1 << 18

# The sidecar's scalars are followed by a `session_state` blob that reaches
# megabytes, so they are read out of a bounded head, never a full parse.
_KIRO_SIDECAR_HEAD = 8192
_KIRO_FIELD = r'"{field}"\s*:\s*("(?:[^"\\]|\\.)*"|null)'


def _kiro_records(path: Path) -> Iterator[dict[str, Any]]:
    """Like `_read_jsonl`, minus the inline binary payloads."""
    with path.open("r", encoding="utf-8", errors="replace") as handle:
        for line in handle:
            if len(line) > _KIRO_WIDE_LINE:
                line = _KIRO_BYTE_ARRAY.sub(r'"\1":[]', line)
            line = line.strip()
            if not line:
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(record, dict):
                yield record


def _kiro_sidecar(path: Path) -> dict[str, str]:
    """`<uuid>.json` beside the transcript: the only source of cwd and title."""
    fields: dict[str, str] = {}
    try:
        with path.with_suffix(".json").open("r", encoding="utf-8", errors="replace") as handle:
            head = handle.read(_KIRO_SIDECAR_HEAD)
    except OSError:
        return fields
    for field_name in ("session_id", "cwd", "title", "created_at", "session_created_reason"):
        match = re.search(_KIRO_FIELD.format(field=field_name), head)
        if not match:
            continue
        try:
            value = json.loads(match.group(1))  # `title` is null on unnamed sessions
        except json.JSONDecodeError:
            continue
        if isinstance(value, str) and value:
            fields[field_name] = value
    return fields


def _kiro_block_text(block: dict[str, Any]) -> str:
    """One content block; image payloads are named, never stringified."""
    kind = block.get("kind")
    data = block.get("data")
    if kind == "image":
        fmt = data.get("format", "") if isinstance(data, dict) else ""
        return f"[image {fmt}]".replace(" ]", "]")
    if isinstance(data, str):
        return data
    if kind == "json" and isinstance(data, dict):
        if "stdout" in data or "stderr" in data:
            # shell output: the streams are the payload, exit status only a tag
            parts = [str(data.get(key) or "").strip() for key in ("stdout", "stderr")]
            status = str(data.get("exit_status") or "").strip()
            if status and not status.endswith(" 0"):
                parts.append(f"[{status}]")
            return "\n".join(part for part in parts if part)
        return json.dumps(data, ensure_ascii=False)
    return _flatten_text(data)


def _kiro_content_text(content: Any) -> str:
    if not isinstance(content, list):
        return _squeeze(_flatten_text(content))
    parts = [_kiro_block_text(block) for block in content if isinstance(block, dict)]
    return _squeeze("\n".join(part for part in parts if part))


def _kiro_tool_call_text(data: dict[str, Any]) -> str:
    """The command first, so `grep(kinds="tool_call")` hits real shell lines."""
    args = data.get("input")
    if not isinstance(args, dict):
        return _squeeze(args if isinstance(args, str) else _flatten_text(args))
    purpose = args.get("__tool_use_purpose")
    rest = {key: value for key, value in args.items() if key != "__tool_use_purpose"}
    command = rest.get("command")
    if data.get("name") == "shell" and isinstance(command, str):
        rest.pop("command")
        text = command.strip()
        if rest:
            text += "\n\n[tool args] " + json.dumps(rest, ensure_ascii=False)
    else:
        text = json.dumps(rest, ensure_ascii=False)
    if isinstance(purpose, str) and purpose:
        text += f"\n\n[purpose] {purpose}"
    return _squeeze(text)


def _parse_kiro(path: Path, records: Iterable[dict[str, Any]], session: Session) -> None:
    """Kiro CLI: ~/.kiro/sessions/cli/<uuid>.jsonl.

    `records` is deliberately unused - Kiro needs `_kiro_records` to strip inline
    image bytes, and the transcript carries no cwd, title or wall time, so those
    come from the `<uuid>.json` sidecar instead.
    """
    index = 0
    meta = _kiro_sidecar(path)
    session.session_id = meta.get("session_id") or path.stem
    session.cwd = meta.get("cwd", "")
    session.title = meta.get("title", "")
    session.started = meta.get("created_at", "")
    session.source = meta.get("session_created_reason", "")

    stamp = ""
    tool_names: dict[str, str] = {}
    for record in _kiro_records(path):
        kind = record.get("kind")
        data = record.get("data") or {}
        if kind == "Prompt":
            # the only timestamp Kiro records; later events inherit their turn's
            epoch = (data.get("meta") or {}).get("timestamp")
            if isinstance(epoch, (int, float)):
                stamp = (
                    datetime.fromtimestamp(epoch, tz=timezone.utc)
                    .astimezone()
                    .isoformat(timespec="seconds")
                )
            text = _kiro_content_text(data.get("content"))
            if text:
                index += 1
                session.events.append(Event(index, "user", text, timestamp=stamp))
            continue
        if kind == "AssistantMessage":
            for block in data.get("content") or []:
                if not isinstance(block, dict):
                    continue
                bkind, bdata = block.get("kind"), block.get("data")
                if bkind == "text":
                    text = _squeeze(bdata if isinstance(bdata, str) else _flatten_text(bdata))
                    if text:
                        index += 1
                        session.events.append(Event(index, "assistant", text, timestamp=stamp))
                elif bkind == "thinking":
                    # normally empty, carrying only encrypted `redactedContent`
                    text = _squeeze(bdata.get("text", "") if isinstance(bdata, dict) else "")
                    if text:
                        index += 1
                        session.events.append(Event(index, "reasoning", text, timestamp=stamp))
                elif bkind == "toolUse" and isinstance(bdata, dict):
                    name = bdata.get("name", "")
                    if bdata.get("toolUseId"):
                        # results name their tool by id only
                        tool_names[bdata["toolUseId"]] = name
                    index += 1
                    session.events.append(
                        Event(
                            index,
                            "tool_call",
                            _kiro_tool_call_text(bdata),
                            name=name,
                            timestamp=stamp,
                        )
                    )
            continue
        if kind != "ToolResults":
            continue
        for block in data.get("content") or []:
            if not isinstance(block, dict) or block.get("kind") != "toolResult":
                continue
            bdata = block.get("data") or {}
            name = tool_names.get(bdata.get("toolUseId", ""), "")
            status = bdata.get("status")
            if status and status != "success":
                name = f"{name}:{status}"
            index += 1
            session.events.append(
                Event(
                    index,
                    "tool_result",
                    _kiro_content_text(bdata.get("content")),
                    name=name,
                    timestamp=stamp,
                )
            )


def _prime_timestamp(record: dict[str, Any], message: dict[str, Any] | None = None) -> str:
    """Prefer the record's ISO timestamp; fall back to the message epoch (ms)."""
    stamp = record.get("timestamp")
    if isinstance(stamp, str) and stamp:
        return stamp
    raw = (message or {}).get("timestamp") or stamp
    if isinstance(raw, (int, float)):
        return datetime.fromtimestamp(raw / 1000, tz=timezone.utc).astimezone().isoformat(timespec="seconds")
    return str(raw or "")


def _prime_content_text(content: Any) -> str:
    """Prime content blocks: {type: text|thinking|toolCall|image, ...}."""
    if not isinstance(content, list):
        return _squeeze(_flatten_text(content))
    parts: list[str] = []
    for block in content:
        if not isinstance(block, dict):
            continue
        btype = block.get("type")
        if btype == "text":
            parts.append(block.get("text", ""))
        elif btype == "image":
            parts.append(f"[image {block.get('mimeType', '')}]".strip())
    return _squeeze("\n".join(part for part in parts if part))


def _prime_tool_call_text(block: dict[str, Any]) -> str:
    """The executed source first, so `grep(kinds="tool_call")` hits real code."""
    args = block.get("arguments")
    if not isinstance(args, dict):
        return _squeeze(args if isinstance(args, str) else _flatten_text(args))
    code = args.get("code")
    if not isinstance(code, str):
        return _squeeze(json.dumps(args, ensure_ascii=False))
    extra = {key: value for key, value in args.items() if key != "code"}
    text = code.strip()
    if extra:
        text += "\n\n[tool args] " + json.dumps(extra, ensure_ascii=False)
    return text


def _parse_prime(path: Path, records: Iterable[dict[str, Any]], session: Session) -> None:
    """Prime Agent: ~/.prime/agent/sessions/<uuid>.jsonl and sub-agent artifacts."""
    index = 0
    session.session_id = path.stem
    for record in records:
        rtype = record.get("type")
        stamp = _prime_timestamp(record)
        if rtype == "session":
            session.session_id = record.get("id") or path.stem
            session.cwd = record.get("cwd", "") or session.cwd
            session.started = stamp
            parent = record.get("parentSession")
            if parent:
                session.parent_id = Path(str(parent)).stem
            depth = record.get("rlmDepth") or 0
            if depth:
                session.source = f"rlm sub-agent (depth {depth})"
            continue
        if rtype == "session_info":
            # sub-agents carry the name they were spawned with
            if record.get("name"):
                session.title = record["name"]
            continue
        if rtype == "compaction":
            text = _squeeze(record.get("summary", ""))
            if text:
                index += 1
                session.events.append(Event(index, "system", text, name="compaction", timestamp=stamp))
            continue
        if rtype == "custom":
            if record.get("customType") == "prime-agent.refinement":
                data = record.get("data") or {}
                text = _squeeze(
                    "\n\n".join(
                        part
                        for part in (
                            data.get("summary", ""),
                            data.get("rationale", ""),
                            data.get("expectedOutcome", ""),
                        )
                        if part
                    )
                )
                if text:
                    index += 1
                    session.events.append(
                        Event(index, "system", text, name="refinement", timestamp=stamp)
                    )
            continue
        if rtype == "custom_message":
            ctype = record.get("customType", "")
            details = record.get("details") or {}
            text = _squeeze(record.get("content") or details.get("message") or "")
            if not text:
                continue
            if ctype == "agent_message":
                # inbound delivery: the spawn prompt, or a parent/child/sibling message
                name = "agent_message"
                relationship = details.get("fromRelationship")
                if relationship:
                    name = f"agent_message:{relationship}"
                kind = "user"
            elif ctype == "session_slash_command":
                kind, name = "user", "slash_command"
            elif ctype == "session_slash_command_result":
                kind, name = "system", "slash_result"
            else:
                kind, name = "system", ctype or "custom_message"
            index += 1
            session.events.append(Event(index, kind, text, name=name, timestamp=stamp))
            continue
        if rtype != "message":
            # agent_status, child_usage_attributed, git_state, model_change,
            # thinking_level_change, service_tier_change, session_state: bookkeeping
            continue
        message = record.get("message") or {}
        role = message.get("role", "")
        stamp = _prime_timestamp(record, message)
        content = message.get("content")
        if role == "user":
            text = _prime_content_text(content)
            if text:
                index += 1
                session.events.append(Event(index, "user", text, timestamp=stamp))
            continue
        if role == "toolResult":
            details = message.get("details") or {}
            text = _prime_content_text(content)
            if not text and message.get("errorMessage"):
                text = _squeeze(str(message["errorMessage"]))
            name = message.get("toolName", "")
            status = details.get("status")
            if message.get("isError") or (status and status != "ok"):
                name = f"{name}:{status or 'error'}"
            duration = details.get("durationMs")
            index += 1
            session.events.append(
                Event(
                    index,
                    "tool_result",
                    text,
                    name=name,
                    timestamp=stamp,
                    duration_ms=int(duration) if isinstance(duration, (int, float)) else None,
                )
            )
            continue
        if role != "assistant":
            continue
        for block in content if isinstance(content, list) else []:
            if not isinstance(block, dict):
                continue
            btype = block.get("type")
            if btype == "text":
                text = _squeeze(block.get("text", ""))
                if text:
                    index += 1
                    session.events.append(Event(index, "assistant", text, timestamp=stamp))
            elif btype == "thinking":
                # `thinking` is often empty with only an encrypted signature
                text = _squeeze(block.get("thinking", ""))
                if text:
                    index += 1
                    session.events.append(Event(index, "reasoning", text, timestamp=stamp))
            elif btype == "toolCall":
                index += 1
                session.events.append(
                    Event(
                        index,
                        "tool_call",
                        _prime_tool_call_text(block),
                        name=block.get("name", ""),
                        timestamp=stamp,
                    )
                )


PARSERS: dict[str, Callable[[Path, Iterable[dict[str, Any]], Session], None]] = {
    "codex": _parse_codex,
    "claude": _parse_claude,
    "kimi": _parse_kimi,
    "kimi-code": _parse_kimi,
    "kiro-cli": _parse_kiro,
    "prime": _parse_prime,
    "senpi": _parse_prime,
}


# --------------------------------------------------------------------------
# discovery
# --------------------------------------------------------------------------


def _harness_names(harness: str) -> list[str]:
    if harness in {"all", "", None}:
        return list(HARNESSES)
    names = [part.strip() for part in str(harness).split(",") if part.strip()]
    unknown = [name for name in names if name not in HARNESS_GLOBS]
    if unknown:
        raise SessionError(f"unknown harness {unknown}; known: {', '.join(HARNESSES)}")
    return names


def _cheap_metadata(harness: str, path: Path) -> tuple[str, str, str]:
    """(session_id, cwd, title) without parsing the whole file."""
    session_id = cwd = title = ""
    try:
        if harness == "codex":
            for record in _read_jsonl(path, limit=1):
                payload = record.get("payload") or {}
                session_id = payload.get("id") or payload.get("session_id") or ""
                cwd = payload.get("cwd", "")
                if payload.get("thread_source") == "subagent":
                    title = "sub-agent rollout"
        elif harness == "claude":
            session_id = path.stem
            for record in _read_jsonl(path, limit=400):
                if record.get("cwd") and not cwd:
                    cwd = record["cwd"]
                if record.get("aiTitle"):
                    title = record["aiTitle"]
                if cwd and title:
                    break
            cwd = cwd or path.parent.name
        elif harness in ("prime", "senpi"):
            # Senpi's filename is `<ISO-ts>_<uuid>`, so the stem is only a fallback; the
            # `session` record below carries the real id.
            session_id = path.stem.split("_", 1)[-1] if harness == "senpi" else path.stem
            # Prime writes `session_info` up front; Senpi names the session only after the
            # first exchange, so its title sits at record ~20-150 and a 12-record probe
            # returned an empty title for every session.
            for record in _read_jsonl(path, limit=12 if harness == "prime" else 400):
                rtype = record.get("type")
                if rtype == "session":
                    session_id = record.get("id") or session_id
                    cwd = record.get("cwd", "")
                    if record.get("rlmDepth"):
                        title = f"sub-agent (depth {record['rlmDepth']})"
                elif rtype == "session_info" and record.get("name"):
                    title = record["name"]
                    break
        elif harness == "kiro-cli":
            meta = _kiro_sidecar(path)
            session_id = meta.get("session_id") or path.stem
            cwd = meta.get("cwd", "")
            title = meta.get("title", "")
        elif harness == "kimi-code":
            session_id = path.parents[2].name.replace("session_", "")
            cwd = _kimi_project(path.parents[3].name)
            if path.parents[0].name != "main":
                title = f"sub-agent {path.parents[0].name}"
        else:
            session_id = path.parent.name
            cwd = path.parents[1].name
    except OSError:
        pass
    return session_id, cwd, title


def find_sessions(
    harness: str = "all",
    project: str | None = None,
    days: float | None = None,
    limit: int = 20,
    root: str | Path | None = None,
) -> list[dict[str, Any]]:
    """Discover recent agent session transcripts, newest first.

    harness: "all" or a comma list of codex, claude, kimi, kimi-code, kiro-cli,
             prime.
    project: substring matched against the session cwd / stored project path.
    days:    only sessions modified within this many days.
    """
    base = Path(root).expanduser() if root else HOME
    cutoff = None
    if days:
        cutoff = (datetime.now(tz=timezone.utc) - timedelta(days=float(days))).timestamp()
    needle = None
    if project:
        needle = str(project).replace(os.sep, "-").strip("-").lower()

    found: list[dict[str, Any]] = []
    for name in _harness_names(harness):
        for path in _iter_harness_paths(base, name):
            try:
                stat = path.stat()
            except OSError:
                continue
            if cutoff and stat.st_mtime < cutoff:
                continue
            found.append(
                {
                    "harness": name,
                    "path": path,
                    "mtime": stat.st_mtime,
                    "size": stat.st_size,
                }
            )
    found.sort(key=lambda item: item["mtime"], reverse=True)

    results: list[dict[str, Any]] = []
    for item in found:
        session_id, cwd, title = _cheap_metadata(item["harness"], item["path"])
        if needle:
            haystack = f"{cwd}|{item['path']}".replace(os.sep, "-").lower()
            if needle not in haystack:
                continue
        results.append(
            {
                "harness": item["harness"],
                "path": str(item["path"]),
                "session_id": session_id,
                "cwd": cwd,
                "title": title,
                "modified": _iso(item["mtime"]),
                "size": item["size"],
            }
        )
        if limit and len(results) >= int(limit):
            break
    return results


def resolve_session(target: str | Path, harness: str = "all") -> tuple[str, Path]:
    """Resolve a path, session id, or id prefix to (harness, path)."""
    candidate = Path(str(target)).expanduser()
    if candidate.is_file():
        # match on the full home-relative shape so ~/.kimi-code is not mistaken
        # for ~/.kimi
        name = _harness_for_path(candidate)
        if name:
            return name, candidate
        resolved = candidate.resolve()
        for name, directory in _HARNESS_DIRS.items():
            if f"/{directory}/" in str(resolved):
                return name, candidate
        return "codex", candidate

    token = str(target).strip().lower()
    matches: list[tuple[int, float, str, Path]] = []
    for name in _harness_names(harness):
        for path in _iter_harness_paths(HOME, name):
            lowered = str(path).lower()
            if token not in lowered:
                continue
            # a transcript whose own filename carries the id beats one that only
            # mentions it in a parent directory (Prime sub-agents live under
            # session-artifacts/<parent-id>/)
            stem = path.stem.lower()
            rank = 0 if stem == token else (1 if stem.startswith(token) else 2)
            try:
                matches.append((rank, path.stat().st_mtime, name, path))
            except OSError:
                continue
    if not matches:
        raise SessionError(f"no session transcript matches {target!r}")
    matches.sort(key=lambda item: (item[0], -item[1]))
    return matches[0][2], matches[0][3]


_CACHE: dict[tuple[str, float, int], Session] = {}
_CACHE_LIMIT = 12


def load_session(target: str | Path, harness: str = "all", use_cache: bool = True) -> Session:
    """Parse a transcript into a normalized Session with events (memoized)."""
    name, path = resolve_session(target, harness=harness)
    try:
        stat = path.stat()
    except OSError as error:
        raise SessionError(str(error)) from error
    key = (str(path), stat.st_mtime, stat.st_size)
    if use_cache and key in _CACHE:
        return _CACHE[key]
    session = Session(harness=name, path=path, size=stat.st_size, modified=_iso(stat.st_mtime))
    PARSERS[name](path, _read_jsonl(path), session)
    if not session.session_id:
        session.session_id = path.stem
    if use_cache:
        if len(_CACHE) >= _CACHE_LIMIT:
            _CACHE.pop(next(iter(_CACHE)))
        _CACHE[key] = session
    return session


KIND_ALIASES = {
    "all": None,
    "chat": {"user", "assistant"},
    "user": {"user"},
    "assistant": {"assistant"},
    "reasoning": {"reasoning"},
    "tools": {"tool_call", "tool_result"},
    "tool_call": {"tool_call"},
    "system": {"system"},
}


def filter_events(session: Session, kinds: str = "chat") -> list[Event]:
    wanted = KIND_ALIASES.get(kinds, None if kinds in {"all", ""} else {part.strip() for part in kinds.split(",")})
    if wanted is None:
        return list(session.events)
    return [event for event in session.events if event.kind in wanted]


def search_sessions(
    query: str,
    harness: str = "all",
    project: str | None = None,
    days: float | None = None,
    limit: int = 10,
    max_files: int = 400,
    context: int = 160,
) -> list[dict[str, Any]]:
    """Case-insensitive raw substring search across transcripts."""
    needle = query.lower()
    hits: list[dict[str, Any]] = []
    for meta in find_sessions(harness=harness, project=project, days=days, limit=max_files):
        path = Path(meta["path"])
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        lowered = text.lower()
        count = lowered.count(needle)
        if not count:
            continue
        position = lowered.find(needle)
        snippet = text[max(0, position - context) : position + context].replace("\n", " ")
        hits.append({**meta, "matches": count, "snippet": snippet})
        if limit and len(hits) >= int(limit):
            break
    return hits


# --------------------------------------------------------------------------
# rendering
# --------------------------------------------------------------------------


def _render_list(rows: Sequence[dict[str, Any]]) -> str:
    if not rows:
        return "no sessions found"
    lines = [f"{len(rows)} session(s), newest first:"]
    for row in rows:
        label = row["title"] or ""
        lines.append(
            f"- [{row['harness']}] {row['modified']}  {row['session_id'][:8] or '?'}  "
            f"{row['cwd'] or '?'}  {row['size'] / 1e6:.1f}MB" + (f"  — {label}" if label else "")
        )
        lines.append(f"    {row['path']}")
    return "\n".join(lines)


def _render_session(
    session: Session,
    kinds: str,
    limit: int,
    event_chars: int,
    max_chars: int,
    from_end: bool,
    around: int | None = None,
    context: int = 6,
    pattern: str | None = None,
) -> str:
    if around is not None:
        # `around` refers to an event index reported by grep/show, so window over
        # the full event list and ignore the kind filter.
        events = [
            event
            for event in session.events
            if around - context <= event.index <= around + context
        ]
        kinds = f"around {around} ±{context}"
    else:
        events = filter_events(session, kinds)
    if pattern:
        regex = compile_pattern(pattern)
        events = [event for event in events if regex.search(event.text)]
        kinds = f"{kinds} matching {pattern!r}"
    total = len(events)
    selected = events[-limit:] if (limit and from_end) else (events[:limit] if limit else events)
    header = [
        f"harness: {session.harness}",
        f"path: {session.path}",
        f"session_id: {session.session_id}"
        + (f"  (root thread {session.parent_id})" if session.parent_id else "")
        + (f"  [{session.source}]" if session.source and session.source != "user" else ""),
        f"cwd: {session.cwd}",
        f"modified: {session.modified}  size: {session.size / 1e6:.1f}MB",
        f"events: {len(session.events)} total, {total} matching kinds={kinds}, showing {len(selected)}"
        + (" (tail)" if from_end else ""),
    ]
    if session.title:
        header.insert(3, f"title: {session.title}")
    body: list[str] = []
    for event in selected:
        tag = _tag(event.kind, event.name, event.duration_ms)
        body.append(f"[{event.index}] {tag}: {_clip(event.text, event_chars)}")
    rendered = "\n".join(header) + "\n\n" + "\n\n".join(body)
    return _clip(rendered, max_chars)


# --------------------------------------------------------------------------
# grep
# --------------------------------------------------------------------------

# Characters that end a literal run: regex metacharacters and the escape prefix.
_RUN_BREAKERS = "\\^$.|?*+()[]{}"

# `{n}` / `{n,}` / `{n,m}`; anything else after `{` is a literal brace in Python.
_BRACE_QUANTIFIER = re.compile(r"\{(\d*)(,(\d*))?\}")


def _skip_group(pattern: str, start: int) -> int:
    """Index just past the `(`-group or `[`-class opened at `start`."""
    closers = {"(": ")", "[": "]"}
    opener = pattern[start]
    closer = closers[opener]
    depth = 0
    index = start
    in_class = opener == "["
    if in_class:
        # `[]a]` and `[^]a]` open with a literal `]`, which does not close them.
        index += 1
        if index < len(pattern) and pattern[index] == "^":
            index += 1
        if index < len(pattern) and pattern[index] == "]":
            index += 1
    while index < len(pattern):
        char = pattern[index]
        if char == "\\":
            index += 2
            continue
        if in_class:
            if char == "]":
                return index + 1
            index += 1
            continue
        if char == "[":
            index = _skip_group(pattern, index)
            continue
        if char == "(":
            depth += 1
        elif char == closer:
            depth -= 1
            if depth == 0:
                return index + 1
        index += 1
    return len(pattern)


def _mandatory_literals(pattern: str) -> list[str]:
    """Literal runs that must appear verbatim in every match of `pattern`.

    A literal-run prefilter is only sound for runs no quantifier can erase:
    `errors?` must probe `error`, never `errors`, or a transcript holding only
    `error` is skipped and the match is silently lost. Groups, classes, escapes
    and alternations end the current run, so what survives is a conservative
    subset — which is exactly what a skip-the-file probe needs.
    """
    runs: list[str] = []
    current: list[str] = []

    def flush() -> None:
        if current:
            runs.append("".join(current))
            current.clear()

    index = 0
    length = len(pattern)
    while index < length:
        char = pattern[index]
        if char in "([":
            flush()
            index = _skip_group(pattern, index)
            continue
        if char == "\\":
            # \d, \b, \n ... — never probed as a literal.
            flush()
            index += 2
            continue
        if char in "?*":
            # Erases the character it follows, so drop it and end the run.
            if current:
                current.pop()
            flush()
            index += 1
            continue
        if char == "+":
            # One occurrence is still guaranteed; only the run's contiguity ends.
            flush()
            index += 1
            continue
        if char == "{":
            match = _BRACE_QUANTIFIER.match(pattern, index)
            if match is None:  # a literal brace
                current.append(char)
                index += 1
                continue
            low = match.group(1)
            optional = low in {"", "0"}
            if optional and current:
                current.pop()
            flush()
            index = match.end()
            continue
        if char in _RUN_BREAKERS:  # . ^ $ | ) ] }
            flush()
            index += 1
            continue
        current.append(char)
        index += 1
    flush()
    return runs


def compile_pattern(pattern: str, fixed: bool = False, ignore_case: bool | None = None) -> re.Pattern[str]:
    """Compile a grep pattern. Smart-case by default: any uppercase makes it
    case-sensitive, like ripgrep's -S."""
    if ignore_case is None:
        ignore_case = pattern.lower() == pattern
    flags = re.IGNORECASE if ignore_case else 0
    if fixed:
        return re.compile(re.escape(pattern), flags)
    try:
        return re.compile(pattern, flags)
    except re.error:
        return re.compile(re.escape(pattern), flags)


def _split_alternation(pattern: str) -> list[str]:
    """Top-level `a|b|c` branches; nested groups are left intact."""
    branches: list[str] = []
    depth = 0
    current: list[str] = []
    escaped = False
    for char in pattern:
        if escaped:
            current.append(char)
            escaped = False
            continue
        if char == "\\":
            current.append(char)
            escaped = True
            continue
        if char in "([":
            depth += 1
        elif char in ")]":
            depth = max(0, depth - 1)
        if char == "|" and depth == 0:
            branches.append("".join(current))
            current = []
            continue
        current.append(char)
    branches.append("".join(current))
    return branches


def _probe_terms(literal: str) -> list[str]:
    literal = literal.strip()
    if len(literal) < 3:
        return []
    # JSONL stores text escaped (\n, \uXXXX for non-ASCII), so probe the raw
    # literal and its JSON-escaped form.
    return [literal.lower(), json.dumps(literal, ensure_ascii=True)[1:-1].lower()]


def _prefilter_terms(pattern: str, fixed: bool) -> list[str]:
    """Cheap literal probes used to skip whole transcripts before parsing.

    Returns an empty list when the pattern cannot be probed safely, which means
    "do not skip any file".
    """
    if fixed:
        return _probe_terms(pattern)
    terms: list[str] = []
    for branch in _split_alternation(pattern):
        runs = _mandatory_literals(branch)
        if not runs:
            return []  # one un-probeable branch makes the whole filter unsafe
        probes = _probe_terms(max(runs, key=len))
        if not probes:
            return []
        terms.extend(probes)
    return terms


_CHUNK = 4 << 20


def _file_may_match(path: Path, terms: Sequence[str]) -> bool:
    """Byte-level, early-exit literal probe; transcripts reach tens of MB."""
    if not terms:
        return True
    probes = [term.encode("utf-8", "replace") for term in dict.fromkeys(terms)]
    overlap = max(len(probe) for probe in probes) - 1
    try:
        with path.open("rb") as handle:
            tail = b""
            while True:
                chunk = handle.read(_CHUNK)
                if not chunk:
                    return False
                window = (tail + chunk).lower()
                if any(probe in window for probe in probes):
                    return True
                tail = window[-overlap:] if overlap else b""
    except OSError:
        return False


def _match_window(text: str, match: re.Match[str], width: int) -> str:
    start = max(0, match.start() - width // 2)
    end = min(len(text), match.end() + width // 2)
    window = text[start:end].replace("\n", " ⏎ ")
    return ("…" if start else "") + window.strip() + ("…" if end < len(text) else "")


def grep_sessions(
    pattern: str,
    harness: str = "all",
    project: str | None = None,
    session: str | Path | None = None,
    kinds: str = "all",
    days: float | None = None,
    fixed: bool = False,
    ignore_case: bool | None = None,
    max_sessions: int = 200,
    max_hits: int = 40,
    max_reported: int = 10,
    max_per_session: int = 5,
    context: int = 0,
    width: int = 220,
) -> list[dict[str, Any]]:
    """Grep parsed events across transcripts. Returns one dict per session with
    a ``hits`` list of {index, kind, name, text, context}."""
    regex = compile_pattern(pattern, fixed=fixed, ignore_case=ignore_case)
    terms = _prefilter_terms(pattern, fixed)

    if session is not None:
        candidates = [{"path": str(resolve_session(session, harness=harness)[1])}]
    else:
        candidates = find_sessions(harness=harness, project=project, days=days, limit=max_sessions)

    results: list[dict[str, Any]] = []
    hit_budget = max_hits
    for meta in candidates:
        path = Path(meta["path"])
        if not _file_may_match(path, terms):
            continue
        try:
            parsed = load_session(path, harness=harness)
        except SessionError:
            continue
        events = filter_events(parsed, kinds)
        hits: list[dict[str, Any]] = []
        matched_events = 0
        for position, event in enumerate(events):
            match = regex.search(event.text)
            if not match:
                continue
            matched_events += 1
            if max_per_session and len(hits) >= max_per_session:
                # Keep counting (`matched_events` is the `rg -c` figure) but stop
                # rendering hits once the per-session window is full.
                continue
            hit = {
                "index": event.index,
                "kind": event.kind,
                "name": event.name,
                "duration_ms": event.duration_ms,
                "text": _match_window(event.text, match, width),
            }
            if context:
                window = events[max(0, position - context) : position + context + 1]
                hit["context"] = [
                    {
                        "index": item.index,
                        "kind": item.kind,
                        "name": item.name,
                        "duration_ms": item.duration_ms,
                        "text": _clip(item.text, width),
                    }
                    for item in window
                    if item is not event
                ]
            hits.append(hit)
        if not hits:
            continue
        results.append({**parsed.as_dict(), "matched_events": matched_events, "hits": hits})
        hit_budget -= len(hits)
        if hit_budget <= 0 or (max_reported and len(results) >= max_reported):
            break
    return results


def _render_grep(results: Sequence[dict[str, Any]], pattern: str, files_only: bool = False, counts: bool = False) -> str:
    if not results:
        return f"no event matches {pattern!r}"
    lines: list[str] = []
    for result in results:
        header = (
            f"{result['path']}\n  [{result['harness']}] {result['modified']}"
            f"  id={(result['session_id'] or '?')[:8]}  cwd={result['cwd'] or '?'}"
            + (f"  ({result['source']})" if result.get("source") not in {None, "", "user"} else "")
            + (f"  — {result['title']}" if result.get("title") else "")
        )
        if files_only:
            lines.append(result["path"])
            continue
        if counts:
            lines.append(f"{result['path']}: {result['matched_events']}")
            continue
        lines.append(header)
        for hit in result["hits"]:
            tag = _tag(hit["kind"], hit["name"], hit.get("duration_ms"))
            lines.append(f"    {hit['index']}:{tag}  {hit['text']}")
            for item in hit.get("context", []):
                ctag = _tag(item["kind"], item["name"], item.get("duration_ms"))
                lines.append(f"      · {item['index']}:{ctag}  {item['text']}")
        lines.append("")
    total = sum(result["matched_events"] for result in results)
    lines.append(f"{total} matching event(s) in {len(results)} session(s) for {pattern!r}")
    return "\n".join(lines)
