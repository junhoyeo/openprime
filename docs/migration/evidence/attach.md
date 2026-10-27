# Attach supersession evidence

Status: **integrated; independently CLEAR-FOR-INTEGRATION (scope-limited); final combined acceptance pending**.

- Lane: `origin/verify/attach-window-int` at `4a740ed50691d7785229b320b871887de3f503fe` (base `be9e2b9b5`), owner `verify-attach-opus`. The earlier `verify/attach-supersession` (`1b0bd3709`) is evidence only.
- Review: `lifecycle-review-opus` CLEAR-FOR-INTEGRATION (scope-limited) for that exact SHA. The reviewer's combined mutation (all fixes reverted) failed exactly the six targeted tests and nothing else.
- Integrated on `feat/rust-migration` as 6 cherry-picks ending at `9307b29e6`.

## Product gaps closed (each red-first)

1. F1: a routed attach subscribes before its snapshot is cut. Events the worker flushed ahead of the attach response were published before the supervisor registered the client (T1).
2. F2: a direct client's stream opens before its attach snapshot (T4).
3. F3: the TUI discards live events that an attach snapshot already covers. Before, a covered prompt rendered twice (H1).
4. F4: a refused prompt replays only on its own transcript, never into a moved one (H4; legacy `5a19fe961` class).
5. F5: a saved session wakes under the waking client's telemetry opt-out (both opted-out revival tests).

Tests: `supervisor::attach_window_tests::*` (T1-T3), `worker::attach_window_tests::*` (T4, T5), `pa-tui tests/attach_event_window_headless.rs` (H1-H5), `revival_context_e2e` opted-out wake tests.

## Ledger mapping

- Ported: `da68b1903`, `b66ce86d3`, `0d2e4ed73`, `df0cf0752` (F1-F3); `ae2db8c32`, `5a19fe961` (F4); `966c11aa7` (F5 plus the revival lane).
- Verified-upstream-equivalent: `ba1ad540d`, `8109307ac` (structural: the Rust TUI never requests `chunked_snapshot`, and the loss class is closed by T1/T4); `50b835d49`, `a59f857cd`, `3201db643`, `4d987e700`, `324f9b637` (H2/H5 and the single UI loop attach); `37d7a7d7e`, `60afe1a78`, `ec5e6766e` (per-connection subscriptions; T2/T3); `38b69af3a`; `d3f48ca26` (create_reuse/create_join).
- `c5e9a2a5f` splits: its ownership-capability half is truly obsolete, and its cwd half is the revival lane. The `wasAttached` field is truly obsolete.
- Scope limit: the structural rows are scoped to the Rust TUI client, which never requests `chunked_snapshot`.

## Disclosed P3 notes (non-blocking)

- A typed attach success with `data: None` keeps the early subscription. That shape does not occur.
- Two concurrent attaches on one connection could cross rollbacks. The TUI serializes attaches.
- A mid-stream reattach can blank the streaming partial until the next delta. Base behaved the same.
- **Pre-cutover blocker S1 (privacy):** prompt-family wakes do not carry the waking client's telemetry opt-out. `messaging.rs` `of_command` returns `OptedOut` only for Attach/Reattach. Prompt, PromptAndWait, Steer, FollowUp, Rename and WaitForIdle wake with `Inherit` (`supervisor/routing.rs`), and `send_message` always inherits. The TUI usually revives through attach, but not always: the agents-view reply sends `Prompt` directly to a row's `activeSessionId` (`pa-tui/src/agents_view/reply.rs`), which can be a passivated session. With a launch record, the worker uses the creator's stance, not the requester's. Without one (pre-upgrade passivation, a failed record write, or a session this daemon never hosted), it uses the daemon environment. Fix before cutover: carry the stance on prompt-family commands (or per connection), fold it into every wake, and add an opted-out prompt-wake e2e test. See `upstream-rust.md`, Pre-cutover blockers.
- Test-seam incident: a first version of the direct-attach test held a std `MutexGuard` across awaits and hung under the full suite. It was replaced by a dedicated holder thread with signal-driven bounded steps.
