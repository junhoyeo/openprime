# Revival lane evidence

Status: **integrated; independently CLEAR-FOR-INTEGRATION (scope-limited); final combined acceptance pending**.

- Lane branch: `origin/verify/revival-context` at `532f11c9ae80f80175880b40095b8e116521dca0` (base `11a949bb0`), owner `verify-revival-opus`.
- Review: `lifecycle-review-opus`, verdict CLEAR-FOR-INTEGRATION (scope-limited) for that exact SHA. Full section in `/tmp/openprime-rust-lifecycle-review.md`.
- Round 1 integrated on `feat/rust-migration` as `e31116ee4` (passivated launch context), `1de572af0` (switched-in transcript cwd on respawn), `68ca60950` (parity tests).
- Rounds 2-5 (`a7a1820c2`..`8541f81a7`, each CLEAR-FOR-INTEGRATION from `lifecycle-review-opus` with its own red control) are integrated as one batch: `c6e7ce813`..`ea903681c` plus `14ef3e499`. Round 2 (`ae935ab8e`) was CLEAR but **held back**: its durable passivated-launch record persisted the CLI apiKey and the creator's client env (a new secret-at-rest location), and nothing removed it on delete or archive. The parent raised that to must-fix. Round 3 (`fd38f632b`) keeps only an allowlist of non-secret launch keys and forgets the record on delete, a failed stop, and any launch. Round 4 applies the allowlist on read too, and keeps an archived session's record for its user-driven revival (TS parity). Round 5 is a test-only fix for a helper flake (see below).

## Product gaps closed

1. Idle passivation (on by default) deleted the worker descriptor. The wake then rebuilt the session from the file header. Fallback-cwd sessions revived into a missing directory, a telemetry opt-out was lost, and the opened-with config was dropped. Fix: a durable passivated launch record (beside the descriptors) that both wake paths replay. The record is written before the idle stop, forgotten if the stop fails, and cleared on any launch over the file, on delete, and on a failed stop.
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

## Rounds 2-5 summary

- R1 closed: the passivated launch record is durable beside the descriptors, so it survives a supervisor restart (`a_passivated_launch_context_survives_a_supervisor_restart`).
- R2 closed: the identity-pending boot repair carries the moved-to cwd.
- R3 closed: update restore nests flat durable keys under `config`, as TS `createCommand` does. R3b closed: the roster `runtime_config` gains an optional `telemetryDisabled`, so the opt-out survives an update restore.
- R4 classified parity-equivalent: TS eviction requires zero attached clients, and an ACP binding is client-owned, so passivation refuses it.
- Reviewer P2 (env replay) closed: a wake sends no creator client env; the waking attach adopts its own pane.
- Secret at rest closed: `a_passivated_launch_record_holds_no_secrets_and_dies_with_its_session` asserts on the file bytes with a sentinel key. `an_old_format_launch_record_never_replays_a_credential` covers records from the unreleased round-2 format.
- Round 5 (test-only): under load, `passivate()` flaked at `revival_context_e2e:351`. Cause: the supervisor eviction fence defers `worker_idle_passivation` while any route holds an in-flight permit, and the deferral answers success, the same as an executed stop. The helper now re-asks until that exact session leaves the live list, with a fail-closed 30 s bound. `an_idle_passivation_ask_defers_while_a_route_is_in_flight` reproduces the deferral deterministically. Possible follow-up, not adopted (wire-shape change outside parity scope): a `{"deferred": true}` reply field.
- Accepted P3: a plain kill of a depth>0 RLM child marked for ledger deletion also leaves a non-secret record on disk. The ledger and the revival gate still decide revival.

## Scope limits and open items

- `843615757` and `837be6824` are proven at the daemon boundary only (create shape). The agents-view client path is not separately proven. The ledger records this limit on those rows.
- `6470a60e7` holds because admission prompts never revive. R4 is classified parity-equivalent (see Rounds 2-5 summary); it is not an open item.
- Round 1 left R1-R4 and an env-replay P2 open. Rounds 2-5 closed or classified them (above).
