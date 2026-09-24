# Transcript formats

Reference for maintaining the parsers in `src/agent_sessions/sessions.py`.

## Codex CLI — `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`

Every line is `{timestamp, type, payload}`.

- `session_meta` (first line): `payload.session_id`, `payload.cwd`,
  `payload.cli_version`, `payload.originator`, base instructions.
- `event_msg` with `payload.type == "user_message"`: the clean user turn.
- `response_item` with `payload.type`:
  - `message` + `role` in `assistant | user | developer` — `content` is a list of
    `{type: input_text|output_text, text}`. The `user` role duplicates
    `event_msg/user_message` (plus injected context), so the parser skips it.
  - `reasoning` — `summary: [{text}]`.
  - `function_call` / `custom_tool_call` — `name`, `arguments` or `input`.
  - `*_call_output` — `output` (string or content-block list).
  - `agent_message` — sub-agent messaging.
- Other line types seen: `turn_context`, `world_state`, `compacted`,
  `inter_agent_communication_metadata`, `event_msg/token_count`.

## Claude Code — `~/.claude/projects/<flattened-cwd>/<uuid>.jsonl`

Directory name is the cwd with `/` and `.` replaced by `-` (not reversible), but
records also carry `cwd`, `gitBranch`, `sessionId`, `timestamp`.

- `user` / `assistant`: `message.role` and `message.content`, either a plain
  string or blocks `{type: text|thinking|tool_use|tool_result}`.
- `ai-title` (`aiTitle`) and `summary` give a human label for the session.
- `last-prompt`, `attachment`, `file-history-delta`, `mode`, `permission-mode`
  are UI bookkeeping and are ignored.
- Curated per-project notes may live in
  `~/.claude/projects/<flattened-cwd>/memory/MEMORY.md`.

## Kimi Code — `~/.kimi-code/sessions/wd_<name>_<hash>/session_<uuid>/agents/<agent>/wire.jsonl`

- `metadata` (protocol version, `created_at` ms epoch).
- `config.update`, `permission.set_mode`, `tools.set_active_tools` — setup.
- `turn.prompt` / `turn.steer`: `input` is a list of `{type: text, text}`.
- `context.append_message`: full provider messages, including system reminders
  and notifications; user-origin ones duplicate `turn.prompt`.
- `context.append_loop_event` with `event.type`:
  - `content.part` — `part.type` is `text` or `think`.
  - `tool.call` — `name`, `args`, `display`.
  - `tool.result` — `result.output`.
  - `step.begin` / `step.end` (`usage`, `finishReason`).
- Sub-agents write their own `agents/<agent>/wire.jsonl` under the same session.

## Kimi CLI — `~/.kimi/sessions/<group>/<uuid>/context.jsonl`

Same event vocabulary as Kimi Code; files may be empty for aborted sessions.

## Kiro CLI — `~/.kiro/sessions/cli/<uuid>.jsonl`

Four files share the `<uuid>` stem; only the `.jsonl` is a transcript. The
`.json` **sidecar** holds the metadata the transcript lacks, `.history` is the
raw prompt history, and `.lock` is `{pid, started_at}` for a live session.

Sidecar keys, in file order: `session_id`, `cwd`, `created_at`, `updated_at`,
`title` (may be `null`), `session_created_reason` (`subagent` on every local
session), then `session_state` — a replay blob that reaches megabytes and
duplicates the conversation. The parser reads only a bounded head of the file
and regexes out the five scalars, so `session_state` is never parsed.

Transcript lines are `{version, kind, data}` with three `kind`s:

- `Prompt` — `data.content` blocks, `{kind: text, data: "<string>"}` or
  `{kind: image, data: {format, source: {kind: bytes, data: [int, …]}}}`.
  `data.meta.timestamp` is **epoch seconds** and is the only timestamp Kiro
  writes; the rest of the turn inherits it.
- `AssistantMessage` — `data.content` blocks:
  - `{kind: thinking, data: {text, signature, redactedContent: [int, …], modelId}}`
    — `text` is normally `""` with only encrypted `redactedContent`.
  - `{kind: text, data: "<string>"}`.
  - `{kind: toolUse, data: {toolUseId, name, input}}`. `input` always carries
    `__tool_use_purpose`; the rest is tool-shaped (`shell`: `command`,
    `working_dir`; `write`: `path`, `oldStr`, `newStr`; `read`: `operations`;
    `grep`: `pattern`, `output_mode`). Tools seen: `shell`, `read`, `write`,
    `grep`, `glob`, `summary`, `todo_list`, `code`, `subagent`, `web_search`,
    `web_fetch`, `introspect`, `goal`.
- `ToolResults` — `data.content` blocks `{kind: toolResult, data: {toolUseId,
  status: success|error, content}}`, whose inner blocks are `{kind: text}`,
  `{kind: json, data: {exit_status, stdout, stderr}}`, or `{kind: image}`.
  `data.results` repeats the same outcome keyed by `toolUseId`, with the tool's
  typed form (`{BuiltIn: {ExecuteCmd: {command, working_dir}}}`); the parser
  uses `content` and ignores this duplicate. **No wall time is recorded.**

Only the `toolUse` block names a tool, so the parser keeps a `toolUseId -> name`
map to label results. Empty transcripts (0 lines beside a valid sidecar) are
normal for aborted sessions.

Inline image and `redactedContent` byte arrays push single lines past 16 MB, so
`_kiro_records` rewrites `"data":[<digits>]` to `"data":[]` on lines over 256 KB
before `json.loads`. Verified lossless: for all 29 oversized lines of a 142 MB
transcript, every non-byte field is identical between the full and elided parse.

Mapping to normalized events:

| record | event |
|---|---|
| sidecar `cwd` / `title` / `created_at` / `session_created_reason` | session `cwd`, `title`, `started`, `source` |
| `Prompt` | `user`, timestamp from `data.meta.timestamp` |
| `AssistantMessage`, `text` block | `assistant` |
| `AssistantMessage`, non-empty `thinking` | `reasoning` |
| `AssistantMessage`, `toolUse` block | `tool_call`, name = tool name, text = `input.command` for `shell` (else the JSON args), with `[purpose]` appended |
| `ToolResults`, `toolResult` block | `tool_result`, name = the calling tool (`<tool>:error` when `status != success`) |

Not mapped: `data.results` (duplicates `content`), `redactedContent`, and image
bytes (rendered as `[image <format>]`).

## Prime Agent — `~/.prime/agent/sessions/<uuid>.jsonl`

Root threads live in `sessions/`; RLM sub-agents write their own transcript to
`~/.prime/agent/session-artifacts/<parent-uuid>/sub-<child-id>/<uuid>.jsonl`.
Everything else in `session-artifacts/` is state, not transcript, and is skipped:
`rlm-subagents.jsonl` (a spawn registry: `childId`, `sessionName`, `sessionFile`,
`spawnCode`, `model`, `status`), `kernel-state.json` / `kernel-state.dill`,
`harness/harness_state.json`, and `session-artifacts/<uuid>/` kernel snapshots.

Line types observed (counts from 71 local transcripts, 58 MB):

- `session` (first line): `version`, `id`, `timestamp`, `cwd`, `rlmDepth`,
  `git: {repoUrl, commit, branch}`, plus `parentSession` (an absolute path to the
  parent transcript) on sub-agents. This is the only source of `cwd`.
- `session_info`: `name` — the name the sub-agent was spawned with; used as the
  session title.
- `message`: `{id, parentId, timestamp (ISO), message}`. The **role lives on the
  inner `message`**, not on the row, and is one of:
  - `user` — `content: [{type: "text", text}]`.
  - `assistant` — `content` blocks `{type: "thinking", thinking, thinkingSignature}`
    (`thinking` is frequently `""` with only an encrypted signature),
    `{type: "text", text}`, and `{type: "toolCall", id, name, arguments}`. Also
    carries `api`, `provider`, `model`, `usage` (tokens + cost), `stopReason`,
    `responseId`, and an epoch-ms `timestamp`. One row can hold several
    `toolCall` blocks.
  - `toolResult` — `toolCallId`, `toolName`, `content` blocks
    (`{type: "text", text}`, sometimes `{type: "image", data, mimeType}`),
    `isError`, `errorMessage`, and `details`:
    `durationMs`, `status` (`ok` | `error`), `stdout`, `stderr`,
    `kernelRestarted`, and optionally `result`, `diffs: [{path, oldStr, newStr,
    startLine}]`, `errorEname`, `error.traceback`, `attachments`.
    The rendered `content` already includes stdout/stderr/traceback, so the
    parser uses it and keeps `durationMs` as `Event.duration_ms`.
  Effectively every tool call is `ipython` with `arguments.code`; other
  `arguments` keys seen are `timeout`, `yield_time_ms`, `max_output_chars`.
- `custom_message` with `customType`:
  - `agent_message` — inbound delivery only (the spawn prompt
    `[task from parent]`, or a message from parent/child/sibling). `content` is
    the provenance header plus the body; `details` has `message`, `from`,
    `fromRelationship`, `target`, `id`.
  - `session_slash_command` / `session_slash_command_result` — e.g. `/refine`.
  - `ipython_state` — kernel-revival notice after compaction.
- `custom` with `customType: "prime-agent.refinement"`: `data.summary`,
  `data.rationale`, `data.expectedOutcome`, `data.appliedEdits`, `data.scope`.
- `compaction`: `summary` — the rolled-up transcript summary.
- Bookkeeping rows, all skipped: `agent_status` (`status.taskState`, by far the
  noisiest type), `child_usage_attributed` (per-child token/cost attribution),
  `git_state`, `model_change`, `thinking_level_change`, `service_tier_change`,
  `session_state`.

Mapping to normalized events:

| record | event |
|---|---|
| `session` | session metadata (`cwd`, `started`, `parent_id`, `source`) |
| `session_info.name` | session title |
| `message` role `user` | `user` |
| `message` role `assistant`, `text` block | `assistant` |
| `message` role `assistant`, non-empty `thinking` | `reasoning` |
| `message` role `assistant`, `toolCall` block | `tool_call`, name = tool name, text = `arguments.code` (extra args appended as a `[tool args]` line) |
| `message` role `toolResult` | `tool_result`, name = `toolName` (`toolName:error` when failed), `duration_ms` = `details.durationMs` |
| `custom_message` `agent_message` | `user`, name `agent_message:<relationship>` |
| `custom_message` `session_slash_command` | `user`, name `slash_command` |
| `custom_message` `session_slash_command_result` | `system`, name `slash_result` |
| `custom` refinement | `system`, name `refinement` |
| `compaction` | `system`, name `compaction` |

Not mapped: token usage/cost (`message.usage`, `child_usage_attributed`),
`agent_status` task-state transitions, `details.diffs`, and image payloads
(rendered as `[image <mime>]`). Prime tool output retains raw ANSI escapes.
