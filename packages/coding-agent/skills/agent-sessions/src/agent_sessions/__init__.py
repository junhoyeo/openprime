"""Read and grep Codex CLI, Claude Code, Kimi, Kiro CLI, and Prime Agent session transcripts.

Entrypoint: ``run`` (exposed as ``await agent_sessions(...)``).
Structured helpers: ``find_sessions``, ``load_session``, ``grep_sessions``,
``filter_events``, ``resolve_session``, ``search_sessions``.
"""

from __future__ import annotations

from typing import Any

from .sessions import (  # noqa: F401
    HARNESSES,
    Event,
    Session,
    SessionError,
    compile_pattern,
    filter_events,
    find_sessions,
    grep_sessions,
    load_session,
    resolve_session,
    search_sessions,
    _render_grep,
    _render_list,
    _render_session,
)

__all__ = [
    "run",
    "find_sessions",
    "grep_sessions",
    "load_session",
    "search_sessions",
    "filter_events",
    "resolve_session",
    "compile_pattern",
    "Session",
    "Event",
    "SessionError",
    "HARNESSES",
]


async def run(
    action: str = "list",
    pattern: str | None = None,
    session: str | None = None,
    harness: str = "all",
    project: str | None = None,
    kinds: str = "",
    limit: int = 0,
    days: float | None = None,
    context: int = 0,
    around: int | None = None,
    fixed: bool = False,
    ignore_case: bool | None = None,
    max_per_session: int = 5,
    event_chars: int = 1200,
    max_chars: int = 20000,
    width: int = 220,
    head: bool = False,
    files_only: bool = False,
    counts: bool = False,
) -> str:
    """Grep and read coding-agent session transcripts stored on this machine.

    Actions
      grep  - regex search over parsed events (default action when a pattern is
              given); ripgrep-style output of `path` then `index:KIND  match`.
      list  - recent sessions, newest first.
      show  - render one session's events (tail by default).

    Args
      pattern:  regex (smart-case: lowercase pattern => case-insensitive).
                Use fixed=True for a literal search.
      session:  path, session id, or id prefix. Scopes grep to one transcript,
                and selects which transcript `show` renders.
      harness:  "all" or comma list of codex, claude, kimi, kimi-code, kiro-cli,
                prime.
      project:  substring filter on the session cwd / project path.
      kinds:    all, chat, user, assistant, reasoning, tools, tool_call, system.
                Defaults to "all" for grep and "chat" for show.
      limit:    sessions for list/grep, events for show (0 = sensible default).
      days:     only consider sessions modified within this many days.
      context:  neighbouring events to include around each grep hit, or the
                half-window used with `around`.
      around:   with show, render the events surrounding this event index
                (the index grep prints), ignoring `kinds`.
      max_per_session: cap on grep hits reported per transcript.
      event_chars/width/max_chars: truncation of events, match windows, output.
      head:     with show, take the first events instead of the last.
      files_only/counts: grep in `rg -l` / `rg -c` style.
    """
    verb = (action or "").strip().lower()
    if not verb:
        verb = "grep" if pattern else "list"
    if verb in {"grep", "search", "rg"} and not pattern:
        raise SessionError("grep needs a pattern: run('grep', 'regex')")

    if verb in {"list", "ls"}:
        rows = find_sessions(harness=harness, project=project, days=days, limit=limit or 20)
        return _render_list(rows)

    if verb in {"grep", "search", "rg"}:
        results = grep_sessions(
            pattern,
            harness=harness,
            project=project,
            session=session,
            kinds=kinds or "all",
            days=days,
            fixed=fixed,
            ignore_case=ignore_case,
            max_sessions=200,
            max_reported=limit or 10,
            max_per_session=max_per_session,
            context=context,
            width=width,
        )
        return _clip(_render_grep(results, pattern, files_only=files_only, counts=counts), max_chars)

    if verb in {"show", "read", "tail"}:
        target = session or pattern
        if not target:
            rows = find_sessions(harness=harness, project=project, days=days, limit=1)
            if not rows:
                raise SessionError("no session found to show")
            target = rows[0]["path"]
        parsed = load_session(target, harness=harness)
        return _render_session(
            parsed,
            kinds=kinds or "chat",
            limit=limit or (0 if around is not None else 40),
            event_chars=event_chars,
            max_chars=max_chars,
            from_end=not head,
            around=around,
            context=context or 6,
            pattern=pattern if (session and pattern) else None,
        )

    raise SessionError(f"unknown action {action!r}; use grep, list, or show")


def _clip(text: str, limit: int) -> str:
    if limit <= 0 or len(text) <= limit:
        return text
    return text[:limit].rstrip() + f"… [+{len(text) - limit} chars truncated]"
