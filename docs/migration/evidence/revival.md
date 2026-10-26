# Revival lane evidence

Status: **integrated; independently CLEAR-FOR-INTEGRATION (scope-limited); final combined acceptance pending**.

- Lane branch: `origin/verify/revival-context` at `532f11c9ae80f80175880b40095b8e116521dca0` (base `11a949bb0`), owner `verify-revival-opus`.
- Review: `lifecycle-review-opus`, verdict CLEAR-FOR-INTEGRATION (scope-limited) for that exact SHA. Full section in `/tmp/openprime-rust-lifecycle-review.md`.
- Integrated on `feat/rust-migration` as `e31116ee4` (passivated launch context), `1de572af0` (switched-in transcript cwd on respawn), `68ca60950` (parity tests).

## Product gaps closed

1. Idle passivation (on by default) deleted the worker descriptor. The wake then rebuilt the session from the file header. Fallback-cwd sessions revived into a missing directory, a telemetry opt-out was lost, and the opened-with config was dropped. Fix: an in-memory passivated launch record that both wake paths replay. The record is written only after a successful idle stop and cleared on any launch over the file.
2. A crash respawn after `switch_session` replayed the launch cwd for the switched-in transcript. Fix: the root-identity follow carries the live cwd into the durable create.

## Verification

- Reviewer red control (product source reverted to base, tests kept): `revival_context_e2e` 3 failed / 2 passed. The failures were the missing-cwd bash refusal, the telemetry attach refusal, and the respawn in the launch directory.
- Integrated tip `68ca60950`, hermetic wrapper (`env -i`, canonical `/private/tmp` root, macOS loopback-only sandbox, product-built test kernel runtime with bundled skills):
  - `cargo fmt --all --check`: exit 0
  - `cargo clippy -p pa-daemon -p pa-tui --all-targets -- -D warnings`: exit 0
  - `cargo test -p pa-daemon --test revival_context_e2e`: 5 passed
  - `cargo test -p pa-tui --test prompt_revival_rebind_headless`: 1 passed
  - `cargo test -p pa-daemon --lib -- passivat root_identity`: 27 passed
  - `cargo test -p pa-daemon --test idle_passivation_e2e -- --test-threads=1`: 3 passed (portable liveness probe `747571c49`)

## Scope limits and open items

- `843615757` and `837be6824` are proven at the daemon boundary only (create shape). The agents-view client path is not separately proven.
- `6470a60e7` holds because admission prompts never revive. R4 stays open.
- Open residuals: R1, the passivation record is lost on supervisor restart (falls back to the header); R2, `apply_identity_pending` carries no cwd; R3, `update_restore::restore_session` builds Create from flat durable keys (needs a test before any claim); R4, ACP admission prompts do not wake passivated sessions.
- Reviewer P2 (follow-up owed): the wake replays the original creator's client env (`HERDR_PANE_ID`). TS revive used the waking client's env (`daemon-agent-connection.ts:1893`). A successor delta fixes this after its own review.
