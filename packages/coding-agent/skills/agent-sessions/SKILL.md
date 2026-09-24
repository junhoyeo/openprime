---
name: agent-sessions
description: Grep and read local coding-agent session transcripts - Codex CLI (~/.codex/sessions), Claude Code (~/.claude/projects), Kimi CLI (~/.kimi/sessions), Kimi Code (~/.kimi-code/sessions), Kiro CLI (~/.kiro/sessions/cli), Senpi (~/.senpi/agent/sessions), and Prime Agent itself (~/.prime/agent/sessions and its sub-agent artifacts). Use when the user asks what a previous or concurrent agent session was doing, to resume or pick up context from another harness, to audit what Prime Agent itself ran (IPython code, tool durations), to find which session touched a file, repo, error, or topic, or to summarize a rollout/wire/context JSONL transcript.
---

# Agent Sessions

Ripgrep for the JSONL transcripts that coding agents leave on this machine.
Every harness is parsed into the same event stream, so one pattern searches user
turns, assistant replies, reasoning, tool calls, and tool output alike.
Transcripts are **read-only evidence** — never edit or delete them.

| Harness | Location | File |
|---|---|---|
| `codex` | `~/.codex/sessions/YYYY/MM/DD/` | `rollout-<ts>-<uuid>.jsonl` |
| `claude` | `~/.claude/projects/<flattened-cwd>/` | `<uuid>.jsonl` |
| `kimi` | `~/.kimi/sessions/<group>/<uuid>/` | `context.jsonl` |
| `kimi-code` | `~/.kimi-code/sessions/<workdir>/<session>/agents/<agent>/` | `wire.jsonl` |
| `kiro-cli` | `~/.kiro/sessions/cli/` | `<uuid>.jsonl` (+ `<uuid>.json` sidecar) |
| `prime` | `~/.prime/agent/sessions/` and `~/.prime/agent/session-artifacts/<parent>/sub-<child>/` | `<uuid>.jsonl` |
| `senpi` | `~/.senpi/agent/sessions/<flattened-cwd>/` | `<ISO-ts>_<uuid>.jsonl` |

Prime Agent's own transcripts are indexed too, so this skill can audit the
harness it runs in: the `tool_call` text is the exact IPython source that was
executed, and each `tool_result` carries its real wall time.

## Grep first

```python
# what was said about a thing, anywhere, recently
print(await agent_sessions("grep", "verify-rls-coverage", project="my-app", days=7))

# regex, smart-case (a lowercase pattern is case-insensitive, like rg -S)
print(await agent_sessions("grep", r"min-height:\s*40px", project="my-app"))

# scope to one transcript and one event kind
print(await agent_sessions("grep", "bundle budget", session="019abc12", kinds="assistant"))

# rg -l / rg -c style, and neighbouring events for context
print(await agent_sessions("grep", "TypeError", files_only=True))
print(await agent_sessions("grep", "TypeError", counts=True))
print(await agent_sessions("grep", "TypeError", context=1))
```

Output is `path` then `index:KIND  …match window…`. The index is an event
number you can jump to:

```python
print(await agent_sessions("show", session="019abc12", around=1362, context=3))
```

## List and read

```python
print(await agent_sessions("list", project="my-app", days=7))       # newest first
print(await agent_sessions("show", session="019abc12"))                # tail of the chat
print(await agent_sessions("show", session="019abc12", kinds="user", limit=50))
print(await agent_sessions("show", session="019abc12", head=True, limit=10))
print(await agent_sessions("show", session="019abc12", kinds="tools", event_chars=300))
```

Shell form (flags only — the CLI takes no positional arguments):
`agent_sessions --action grep --pattern "regex" --project my-app --days 7`.

`session` accepts a full path, a session id, or an id prefix (a transcript whose
own filename carries the id wins over one that only mentions it in a parent
directory). `harness` accepts `all` or a comma list of `codex, claude, kimi,
kimi-code, kiro-cli, prime, senpi`. `kinds` is one of `all,
chat, user, assistant, reasoning, tools, tool_call, system` (grep defaults to
`all`, show to `chat`).

## Auditing Prime Agent itself

```python
# every guessed sleep this harness ever ran, with the code that ran it
print(await agent_sessions("grep", r"time\.sleep\(", harness="prime", kinds="tool_call"))

# slowest cells: tool_result events expose the recorded wall time
from agent_sessions import load_session
session = load_session("019def34")                     # root thread
slow = sorted(
    (event for event in session.events if event.duration_ms),
    key=lambda event: event.duration_ms,
    reverse=True,
)[:5]
print([(event.index, event.name, event.duration_ms) for event in slow])
```

Rendered tool results show the duration in the tag: `TOOL_RESULT(ipython, 30009ms)`,
and a failed cell is tagged `TOOL_RESULT(ipython:error, 1811ms)`.

## Structured access

Rendered strings are for reading; use the helpers when the kernel should compute
on the data:

```python
from agent_sessions import find_sessions, load_session, grep_sessions, filter_events

rows = find_sessions(harness="codex", project="my-app", days=3, limit=5)
session = load_session(rows[0]["path"])              # memoized per path+mtime
user_turns = [event.text for event in filter_events(session, "user")]
hits = grep_sessions("playwright", project="my-app", days=2)
```

`load_session` normalizes every harness into `Session.events`, a list of
`Event(index, kind, text, name, timestamp, duration_ms)` with `kind` in
`user, assistant, reasoning, tool_call, tool_result, system`. `duration_ms` is
`None` unless the harness records tool wall time (Prime does).

## Notes

- Senpi is built from the same coding-agent package as Prime, so its records (`session`,
  `session_info`, `message`, `custom`) parse with the Prime reader unchanged. Two differences are
  handled: its filename is `<ISO-ts>_<uuid>` so the id comes from the `session` record, and it
  names a session only *after* the first exchange — `session_info` sits at record ~20-150, so the
  metadata probe reads 400 records for `senpi` where 12 suffice for `prime`. The glob is
  deliberately `sessions/*/*.jsonl`, which excludes the sibling
  `<cwd>/extensions/goal/*.history.jsonl` extension-state files.

- grep skips whole transcripts with a raw-byte prefilter before parsing, and
  probes both the literal and its JSON-escaped form, so non-ASCII patterns
  (Korean, emoji) still match escaped `\uXXXX` payloads.
- Sessions can exceed 10 MB. Prefer `grep` to locate an event, then `show` with
  `around=` rather than dumping a whole transcript into context.
- Codex sub-agent rollouts are separate files whose `session_id` is their own
  thread and whose root thread is reported as `parent_id`; Kimi Code sub-agents
  live in sibling `agents/<name>/wire.jsonl` files; Prime RLM sub-agents live in
  `session-artifacts/<parent>/sub-<child>/<uuid>.jsonl`, report the spawn name as
  the session title and the parent thread as `parent_id`. All show up in grep.
- Prime tool output keeps its raw ANSI colour codes, exactly as the kernel
  emitted them; match on the payload, not on colour-broken words.
- Kimi directories only encode a workdir slug (`wd_<name>_<hash>`), so `project`
  matching there is by name, not by absolute path.
- Kiro keeps no `cwd`, title or tool wall time in the transcript; they come from
  the `<uuid>.json` sidecar, read from a bounded head because its `session_state`
  blob reaches megabytes. `duration_ms` is therefore always `None` for `kiro-cli`,
  and only `Prompt` rows carry a timestamp, which the rest of the turn inherits.
- Kiro inlines pasted images as raw JSON integer arrays, so single lines reach
  16 MB. The reader elides those arrays before parsing (text is unaffected;
  images render as `[image png]`), which keeps a 142 MB transcript under a
  second. `session_created_reason` shows up as the session source, e.g.
  `[subagent]`.
- See [references/formats.md](references/formats.md) for the raw record shapes.
