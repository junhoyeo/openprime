# Refine-cancel evidence

Status: **integrated; independently CLEAR-FOR-INTEGRATION (scope-limited); final combined acceptance pending**.

- Lane: `origin/port/rust-refine-cancel-v2` at `8d76aea05cc2a235bb1db6cfab54ab3dc3de0342` (base `11a949bb0`), owner `sessions-opus`. The lane twin `a498d6d20` is patch-id identical. The superseded `port/rust-refine-cancel` (`ef86090fb`) and the BLOCKED `e8c605e0` / `e8742507b` deltas are not integrated.
- Review: `lifecycle-review-opus`. `e8742507b` BLOCK (3 P1s). `e37800462` CLEAR (scope-limited, P2-a/P2-b open). `8d76aea05` CLEAR (P2-a/P2-b closed; the reviewer's mutation that removes `arm()` fails the gated test). Full sections in `/tmp/openprime-rust-lifecycle-review.md`.
- Integrated on `feat/rust-migration` as 11 cherry-picks ending at `53b5d0ef6`.

## Decision and behavior

The parent chose Option A, fork-tip TS parity. At `703367bad`, `interactive-mode.ts:7554-7561` cancels a running refine with a plain `abort()`. Queued prompts stay queued, and nothing is restored to the editor. The earlier journal-backed queue-restore transaction was dropped.

- The TUI surfaces a running `/refine` (typed `session_command`, the Refining loader with the configured cancel key, attach and live decode). Ctrl+C checks `refine_active` before `turn_active` and sends the plain abort off-loop.
- The daemon projects a directly admitted `/refine` as the active session command. There is no queue clear, restore, new capability, or new command.
- Cancellation (an upstream parity gap closed by the fork): pinned upstream `c4c238ca0` has no abort path into `execute_refine`, and TS aborts `_refineAbortController`. The fork adds an engine-owned `Arc<RefineAbortSlot>` that abort reaches with a short mutex (P2-a). A delivery `arm()` makes an abort that lands before `register()` refuse the refine before planning (P2-b). A plan that is cancelled before its commit point applies nothing, and its outcome rows pass the abort gate.

## Ledger mapping

- `2ac5eed42` (show progress while refine runs): ported.
- `0401ba71d` (reconcile the refine loader on attach): ported.
- `9aca6ba2a` and `6f0b233dd` (restore queued prompts after cancel): superseded within the fork's own tip. Upstream `71ca6cfd1` (#838) and `e2fb7bfa1` (#2426) removed `restoreQueuedMessagesToEditor` before `703367bad`.
- `3f747ebbe`: docs note only.
- Refine-cancel parity: an upstream parity gap closed by the fork, not a migration regression.

## Disclosures

- Accepted P3 (TS-equivalent): the abort slot is engine-wide. An auto-refine that registers while a `/refine` delivery is armed adopts the pending controller.
- Upstream test-design residual: the `abort_boundary_tests` faux tests share the process-global faux provider registry, and each engine runtime outlives its `#[tokio::test]`. When one test's request-start bound expires under load, its leaked request can consume a later test's scripted response. `sessions-opus` reproduced this deterministically (scratch harness). In this migration only the bounds were raised (120 s, event-driven). Per-test provider ids or runtime shutdown are possible follow-ups.
