# Upstream Rust migration

Status: **in progress; not approved for cutover or release**

This migration adopts the upstream Rust implementation and re-ports only fork behavior that is still needed. It does not merge the legacy TypeScript `develop` branch into Rust. The legacy branch remains the rollback implementation and the evidence source.

## Source pins and repository boundary

| Role | Immutable source |
| --- | --- |
| Target repository | `https://github.com/junhoyeo/openprime` (`origin`) |
| Rust base | `c4c238ca025e382d96fb5f37e6328ca77fb82ef3` |
| Legacy fork `develop` | `703367badf5c84e48e8c8cd986fbcda882b6378c` |
| Legacy fork `main` | `e260085dd8f742e0def3d871860c9a888b114851` |
| Integration branch | `feat/rust-migration` |
| Intended PR base | `origin/main` |

`origin/main` is an ancestor of the pinned Rust base. The upstream history is retained with its original authors. New fork commits use the ambient repository identity. This work must never push, open a pull request, tag, or publish against `PrimeIntellect-ai/prime-agent`.

## Inventory and proof rule

The complete 99-commit, non-merge legacy inventory is in:

- [`legacy-commit-map.json`](legacy-commit-map.json), the authoritative machine-readable ledger.
- [`legacy-commit-map.md`](legacy-commit-map.md), its review view.
- [`evidence/skills.md`](evidence/skills.md), the integrated #33 source/package/review record with disclosed non-green attempts.
- [`evidence/providers.md`](evidence/providers.md), the final provider review/provenance record plus combined safe-metadata seam evidence.
- [`evidence/kernel.md`](evidence/kernel.md), the retain-only immutable-generation/diagnostics review and hermetic ABI evidence, including native holds.
- [`evidence/lifecycle.md`](evidence/lifecycle.md), the final RLM/`/btw`/heartbeat review and combined hermetic evidence, with both isolation incidents and native hold.
- [`evidence/state-compat.md`](evidence/state-compat.md), legacy session/harness/skill/auth/`/btw`/archive state compatibility from synthetic TS-writer fixtures, with its independent review.
- [`evidence/attach.md`](evidence/attach.md), the attach event-window and supersession lane with its scope-limited review.
- [`evidence/refine-cancel.md`](evidence/refine-cancel.md), the refine surfacing and cancellation lane (Option A, TS parity) with its review history.
- [`evidence/revival.md`](evidence/revival.md), the integrated revival launch-context/switched-cwd lane with its scope-limited review and open residuals R1-R4.
- [`evidence/native-collect.md`](evidence/native-collect.md), the one approved combined faux collect run, including its successful core behavior, post-collection harness failure, and exact cleanup.

Each commit and PR #32/#33 must end in exactly one state:

- `verified-upstream-equivalent`: source-level mapping plus a relevant verifier proves equivalent behavior at the integrated Rust tip.
- `ported`: the integrated Rust commit and its passing regression/parity verifier are recorded.
- `truly-obsolete`: a language/runtime-specific behavior no longer exists, with a concrete rationale. This state cannot be inferred from a subject line.
- `BLOCKED`: evidence is missing or a gap remains. A blocker cannot be used to approve cutover.

### Special audit: `0b5676b`

Legacy commit `0b5676b6498cf0ecb89dbffc530b2f67545b8f18` restored three behaviors after an old upstream merge. They must be audited independently rather than classifying the commit by its broad subject:

1. Normalize process start IDs before deciding a descriptor belongs to a replaced process; unverifiable identity remains unknown and is not reaped as stale.
2. Preserve the intentional per-worker heartbeat failure isolation so one poisoned worker cannot fail the entire global listing. An explicit error/stale row is acceptable; upstream's fail-whole/last-good snapshot alone is not accepted without a parent decision.
3. Preserve refinement-status test fixture compatibility after queue-selection changes. This third item may be TypeScript-test-only, but it stays blocked until the Rust test surface is checked.

## Work ownership

| Lane | Owner | Owned surface |
| --- | --- | --- |
| Integration | `rust-migration-lead` | this documentation, fork CI/package safety, tested commit integration, combined gates |
| Inventory | `upstream-rust-scout` | all 99 legacy commits, upstream survey, source mappings |
| Kernel | `openprime-pr32` | bootstrap, snapshots, kernel restoration, PR #32, Python MCP2 schema compatibility |
| Skills | `openprime-pr33` | bundled `agent-sessions` and Aside parity, PR #33, focused prompt/package registration |
| Providers | `rust-port-providers` | `pa-ai`, `pa-models`, auth/gateway/retry and model-selection deltas |
| Lifecycle | `rust-port-sessions` | daemon/RLM revival, process identity, refinement feedback, persisted `/btw` behavior |

Shared `pa-core` and CLI seams require owner coordination. Duplicate or stub implementations are rejected when upstream already provides the behavior.

## Safety boundary

Runtime and state checks must use a resolved-path preflight, short owned root, explicit agent/session/kernel/socket paths, copied or synthetic fixtures, private build output, and mock providers. Tests must never auto-discover or stop the installed daemon. Real OAuth values are never copied into another runtime or recorded in evidence.

Two non-accepted worker attempts violated isolation and remain preserved as incident evidence rather than being erased: a lifecycle attempt omitted the agent-dir override and wrote one synthetic child artifact under the real `~/.prime` artifact tree; a kernel test selected the real managed interpreter when its test-specific Python override was omitted. The lifecycle artifact was not deleted, the exact owned test processes were stopped, and neither attempt counts as acceptance. Records: `/tmp/openprime-native-isolation-incident-audit.md` and `/tmp/openprime-kernel-isolation-incident.md`. Later hermetic/native work uses pre-write resolver guards; absence of intentional writes is never presented as proof of no mutation when no before-state exists.

State-compatibility acceptance is not satisfied by `--version`. Before cutover, the integrated binary must prove all of the following while original evidence remains byte-identical:

- sanitized TypeScript session JSONL, settings, models, harness entries, and skill metadata load from copied fixtures;
- legacy persisted `/btw` linked sessions load and resume with parent linkage, full spend, and visible historical conversation; byte preservation alone is insufficient;
- archived sessions reopen;
- restored kernel snapshots never truncate or recreate referenced files;
- auth schema compatibility is tested with synthetic values only;
- daemon sessions, tool calls, spawn/message/observe/collect, cancellation, and resume work over isolated sockets;
- the packaged install layout contains the Python runtime and Python-backed skill resources expected by the kernel contract.

## Baseline evidence

The pinned base was checked from a detached, clean worktree at `/private/tmp/pa-rust-study`.

| Check | Result |
| --- | --- |
| Source `HEAD` | `c4c238ca025e382d96fb5f37e6328ca77fb82ef3` |
| Release binary SHA-256 | `5e1b004ba97be729c3f3fcb05187e1b7a5759c9a455a9e2cccfa9cefa0cedd54` |
| `prime-agent --version` in isolated `HOME` | pass, `0.10.1` |
| `prime-agent --help` in isolated `HOME` | pass; expected CLI command surface printed |
| `prime-agent prompt --cwd <rust-worktree> --json` in isolated `HOME` | pass; 8 layers and a 53,520-byte JSON result |
| Explicit-socket daemon `status --json` and `list --all --json` | pass in `/tmp/pab-7d67ab19`; protocol 7/schema 30 and zero cross-home sessions |
| Synthetic-provider print session | pass; persisted JSONL and returned the scripted reply |
| Python kernel tool call | pass with isolated venv and pinned source package; receipt contained `tool-ok` |
| Persisted-session resume | pass; a second synthetic turn appended and returned the scripted resume reply |
| Spawn/list/observe/message | pass; the recorded child appeared in both rosters and `agent_message.send` reported delivered |
| Child cancellation | pass; deletion returned `cancelled`, and a zero-time collect returned `settled=true` with the cancellation reason |
| Positive-timeout collect immediately after spawn | **BLOCKED**; returned `running, settled=false` in about 1 ms for a 500 ms delayed child instead of waiting |
| Packaged-layout real kernel receipt | pass after the test supplies an isolated product-current interpreter; 1 passed, 0 failed, 0 ignored; an incompatible interpreter fails at explicit staged-product validation |

All of these native binary checks used synthetic data under `/tmp/pab-7d67ab19`; product shutdown stopped the isolated daemon afterward. The positive-timeout collect result is a pinned-base admission/observability race assigned to the session-lifecycle lane. The earlier package-fixture failure was kernel startup rejection of an incompatible interpreter, not dispatch or receipt execution; `bd192f4c4` makes the test and CI supply/validate isolated current state. Final staged-package receipts must still be repeated after all ports integrate. These baseline results do **not** prove final integrated state, package, or migration parity.

## Acceptance gates

All commands use `CARGO_BUILD_JOBS=2` and a worktree-private `CARGO_TARGET_DIR`. Focused lane checks run before integration. The exact integrated tip must then pass:

- `make check` (format, clippy with `-D warnings`, workspace tests, release workspace build);
- `make deny` when dependency files change;
- release/package assembly and install-layout smoke tests;
- required focused Python suites for bundled runtime and skills;
- state-safety tests over copied/synthetic fixtures;
- isolated daemon and family behavior checks;
- user-visible and protocol differential checks against the frozen TypeScript reference for changed surfaces;
- cross-platform CI inspection, including per-job results and unresolved review threads;
- independent adversarial review of `c4c238ca0..HEAD`, followed by fixes and exact-tip reruns.

A synthetic provider is the CI/default smoke path. A paid gateway smoke is optional and may run only after config/auth wire compatibility is verified; it must use a small prompt, disclose cost, and never print token values.

## Packaging and release boundary

During migration, the branch and draft PR may build verification artifacts only. No release tag, default-branch switch, installer update, publish job, or live runtime restart is authorized. Fork workflow changes must make `junhoyeo/openprime` the explicit target and prevent a migration PR or an upstream-shaped workflow from publishing accidentally.

Never use an unqualified `prime-agent update` for cutover. The updater resolves channel metadata from `PRIME_AGENT_DOWNLOAD_BASE_URL` or its default upstream bucket and can install an upstream artifact. Cutover must use an isolated manual install or a reviewed fork artifact channel only after the served URL, artifact identity, checksum, and rollback input are verified.

Fork target audit:

- `AGENTS.md`, the pull-request template, CI badge, and contribution links target only `junhoyeo/openprime:main`; `PrimeIntellect-ai/prime-agent` is read-only.
- `nightly.yml` is manual-dispatch-only and all write-capable release jobs are hard-gated to `github.repository == 'junhoyeo/openprime'`.
- The built-in updater still defaults to the upstream R2 channel and the official installer constant still names `app.primeintellect.ai`; neither is an authorized fork cutover source. README installation commands no longer point fork users at that upstream installer.
- A final cutover manifest must name the fork-served metadata URL, artifact commit/build identity and SHA-256, verify those served bytes, and provide an isolated manual fallback. Repository targeting and release approval remain separate controls.

## Known upstream macOS issues

Upstream CI runs every Rust test job on `ubuntu-24.04` only. On macOS, a static triage ([`evidence/macos-triage.md`](evidence/macos-triage.md)) found the issues below. They are in upstream code with no fork commits, so they are outside the 99-commit migration scope. They are not migration blockers: the owner's default `~/.prime` is not a symlink. After the merge, they get tracking issues on `junhoyeo/openprime`, never upstream.

1. **Stop/archive does not cancel descendant scheduled jobs under a symlinked sessions path.** `stop_cleanup::cancel_scheduled_jobs_for_tree` (`crates/pa-daemon/src/stop_cleanup.rs:98-147`) passes the canonical member path as the cron cancel filter. Jobs store the worker's raw `store.path` (`scheduled_jobs.rs:352`), and the cron store matches lexically (`crates/pa-core/src/cron/store/jobs.rs:119/135`). Impact: when the agent or sessions directory path contains a symlink (`/tmp`, `/var/folders`, a symlinked `~/.prime`, an override path), root stop/archive and passivated-child delete leave heartbeat/cron jobs active, and they fire again on resume or revive. Test: `stop_cleanup::tests::finalize_cancels_the_tree_and_archives_the_root`. Fix idea: key the cancel by session id, or canonicalize both sides in the matcher.
2. **Saved-session delete reports `method: trash` for a missing file.** `saved_session_commands.rs:113-135` computes `removed_by_trash = success || !path.exists()`, and macOS 15+ ships `/usr/bin/trash`. The response is `{ok: true, method: "trash"}` for a missing file, and the tombstone hook still runs. Unverified by design (a real `trash` probe would touch the owner's live Trash): if macOS `trash` rejects `--`, real deletes always fall back to a permanent unlink. Test: `protocol_breadth_b10_b11::wave_b11_saved_sessions_wire_shapes`. Fix idea: check the trash exit status separately from file absence, and probe the argument form.
3. **RLM ledger path identity split.** `rlm_ledger_path` hashes a sessions directory that is canonicalized when it exists and raw otherwise. That is the same split class the fork fixed for session leases in `be7a229d3`. Ledger rows publish a canonical `parentSessionPath` while scan/stats publish raw paths, and `pa-tui` file identity is lexical, so file-keyed lineage can miss under a symlinked directory. Not test-backed. Fix idea: canonicalize the nearest existing ancestor, as the lease fix does.
4. **Intentional platform differences (not defects).** `unix_listener_definitely_closed` returns `false` on non-Linux (`crates/pa-types/src/platform/transport.rs:181-186`), so supervisor and worker exits leave their socket files on macOS. The next boot or stale-bind probe unlinks them. TS unlinked by identity. Tests that assert Linux behavior: `socket_identity_guard_e2e`, `worker_orphan_exit_e2e`, and the `supervisor_e2e` SIGTERM drain.

Test-only macOS assumptions in upstream tests (`/proc` reads, `proc:` start-id prefixes, raw-vs-canonical tempdir path equality, Linux AF_UNIX half-close) are listed in [`evidence/macos-triage.md`](evidence/macos-triage.md). They are not rewritten in this migration. The fork made one fork-touched test portable (`idle_passivation_e2e`, `747571c49`).

## Rollback and cutover plan

Rollback evidence is the untouched `origin/develop` tip `703367badf5c84e48e8c8cd986fbcda882b6378c` and the currently installed TypeScript runtime. The migration does not rewrite or delete either.

After every blocker is closed, the integration lead will provide the tested Rust artifact identity, package checksums, fixture hashes, exact cutover commands, and rollback commands. The parent owns merge, default-branch selection, installation, daemon restart, and live verification. Until that explicit action, this branch is not the served runtime.

## Current blockers

The scout inventory is fully recorded, and inventory disposition remains separate from verification outcome. The independently reviewed provider lane plus combined safe metadata seam closes 10 rows as `ported`, 4 as `verified-upstream-equivalent`, and 2 as `truly-obsolete`; 83 remain `BLOCKED`. Skills, kernel, and lifecycle source are now integrated and independently CLEAR, but their rows stay blocked until combined package/native/state acceptance closes. Multiple legacy commits naming one invariant may share one reviewed regression proof. PR #32 and PR #33 remain open through final migration landing.

The spawn-to-positive-collect source race is fixed. One approved combined native case returned the expected terminal child inside the parent turn in 0.782 seconds and later recorded exactly one spawn row; parent accepts that narrow observation. The harness/full session is not green: its post-collection path lookup was wrong, and its unhandled child terminal notice exhausted the parent faux response queue, producing a final assistant error. No unchanged rerun occurred. Remaining blockers are copied legacy state/`/btw` semantic fixtures, final packaged ABI2/skills self-bootstrap, exact-tip workspace gates, substantive (non-skipped) Rust CI, changed-surface parity, and independent combined review.
