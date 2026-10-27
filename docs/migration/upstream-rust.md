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
- [`evidence/ci-410.md`](evidence/ci-410.md), the first Linux CI run on the integrated tip, with a root cause and fix per red job.
- [`evidence/attach.md`](evidence/attach.md), the attach event-window and supersession lane with its scope-limited review.
- [`evidence/refine-cancel.md`](evidence/refine-cancel.md), the refine surfacing and cancellation lane (Option A, TS parity) with its review history.
- [`evidence/revival.md`](evidence/revival.md), the integrated revival launch-context/switched-cwd lane with its scope-limited review; R1-R3 closed and R4 classified parity-equivalent.
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
3. Preserve refinement-status test fixture compatibility after queue-selection changes. Final: TypeScript-test-only, closed as `truly-obsolete` (see the ledger row).

## Work ownership

The first-wave lane owners below have retired. They are kept for history, because ledger rows still name them as the original owner.

| Lane | First-wave owner (retired) | Owned surface |
| --- | --- | --- |
| Integration | `rust-migration-lead` | this documentation, fork CI/package safety, tested commit integration, combined gates |
| Inventory | `upstream-rust-scout` | all 99 legacy commits, upstream survey, source mappings |
| Kernel | `openprime-pr32` | bootstrap, snapshots, kernel restoration, PR #32, Python MCP2 schema compatibility |
| Skills | `openprime-pr33` | bundled `agent-sessions` and Aside parity, PR #33, focused prompt/package registration |
| Providers | `rust-port-providers` | `pa-ai`, `pa-models`, auth/gateway/retry and model-selection deltas |
| Lifecycle | `rust-port-sessions` | daemon/RLM revival, process identity, refinement feedback, persisted `/btw` behavior |

The final integration lead was `mig-lead-opus`. The later lanes (state-compat, revival rounds 2-5, attach, refine-cancel, CI fixes, kernel pre-spawn probe fix) each had an implementer and a separate read-only reviewer; their evidence files name both.

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

## Live kernel-venv incident (2026-10-27, real `~/.prime`)

What happened. Between about 03:28 and 03:41 KST on 2026-10-27, before the hermetic rule existed, lane workers ran Rust `cargo test` with the real `HOME`. Rust resolves `kernel_venv_dir` from `HOME` (`pa-core/src/kernel/bootstrap/venv/layout.rs`), not from the test's temporary agent dir, so these runs reached the owner's live kernel state:

1. 03:28-03:33: `attach_window_e2e` runs from the attach verification lane created a Rust generation `~/.prime/agent/kernel-venv.generations/generation-55e2b7c09bd909dfe3c4-00000001793039330332-fd82f3d8-9f8c-4b93-8be6-abeb7e568f0e`. Its bootstrap skill paths point into the `verify-attach-supersession` worktree. The runs also left four orphaned test workers and kernels (ppid 1). The parent stopped them by exact PID.
2. 03:36:46: a sessions-lane run (`cargo test -p pa-daemon --lib -- abort_boundary_tests ...`) rebuilt the live legacy `~/.prime/agent/kernel-venv` in place. The `.bootstrap-version` runtime became `sha256:53379d4f...` (Rust branch), with `prime_agent_runtime` installed from `file:///.../rust-port-sessions/prime-agent-runtime` and editable skills pointing into that worktree. About 35 running TypeScript kernels use that path. Modules they had already loaded were unaffected, but a first-time import would have resolved against the replaced files.

Repair. At 03:41:35-43 the parent started one TypeScript kernel, and the TypeScript bootstrap self-heal rebuilt the venv. After the repair, `.bootstrap-version` runtime is `sha256:1f23ea168d212a49e27ee65a30a971731a297897808f17f3a00971945ebb242b` (TS), `prime_agent_runtime` comes from `file:///Users/<owner>/prime-agent/packages/coding-agent/dist/prime-agent-runtime`, skills come from `~/.prime/agent/skills`, and no Rust worktree references remain. No session, auth, settings or harness file was changed.

Retained. The stray `generation-55e2b7c0...` directory is still in `~/.prime/agent/kernel-venv.generations/`. It is kept as evidence; TypeScript ignores it. Removing it is a pre-cutover step that needs owner approval (see Pre-cutover blockers).

Guard. Every cargo/uv/test command now runs through `scripts/hermetic_test.py` (`env -i`, synthetic `HOME`/`TMPDIR`/XDG, `PRIME_AGENT_KERNEL_VENV` under the root, egress sandbox), as `AGENTS.md` requires.

## Telemetry incident (possible pseudonymous usage events)

The product sends pseudonymous usage and performance metrics unless `DO_NOT_TRACK=1`, telemetry is disabled, or it runs offline. Its notice states the limits: no prompts, responses, tool content, file paths, or repository data. During the migration, these runs started product workers or kernels **without** `DO_NOT_TRACK` and **without** an egress block. It is **unknown whether any event was delivered.**

1. 2026-10-27 03:21-03:42 KST, before the hermetic rule: real-`HOME` cargo test runs by the verification and sessions lanes started real `pa-daemon` workers and kernels. Any events would carry the owner's real pseudonymous install identity from `~/.prime`.
2. The mig-lead window between dropping `DO_NOT_TRACK` from the harness (to diagnose telemetry-test failures) and the sandboxed wrapper. All runs used synthetic `HOME`s and faux or mock providers: `pa-daemon` lib reruns (`/tmp/pa-v45c31ca` and siblings, `/tmp/pa-g1e64828`), class-C integration reruns (`/tmp/pa-v82d4083`), product-built kernel runtimes (`/tmp/pa-rtdfab866`, `/tmp/pa-bc0aa232e`, `/tmp/pa-gc6d8874e`), native harness validation runs 1-3 (`/tmp/pa-n99e3bdd`, `/tmp/pa-n2e73042`, `/tmp/pa-n3300788`), and one CI-runtime print-mode probe.

Guards added afterwards: `scripts/hermetic_test.py` refuses to start unless the macOS egress sandbox is active or `--do-not-track` sets `DO_NOT_TRACK=1`. The out-of-repo native harness (`/tmp/mig-lead-opus/gate_harness.py` `native_launch`, not shipped in this repository) refuses any product launch without both `DO_NOT_TRACK=1` and the sandbox. The CI test-runtime step sets `DO_NOT_TRACK=1`.

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

Upstream CI runs every Rust test job on `ubuntu-24.04` only. On macOS, a static triage ([`evidence/macos-triage.md`](evidence/macos-triage.md)) found the issues below. They are in upstream code with no fork commits, so they are outside the 99-commit migration scope. They are not migration blockers: the owner's default `~/.prime` is not a symlink. They are tracked in [#41](https://github.com/junhoyeo/openprime/issues/41) on `junhoyeo/openprime`, never upstream.

1. **Stop/archive does not cancel descendant scheduled jobs under a symlinked sessions path.** `stop_cleanup::cancel_scheduled_jobs_for_tree` (`crates/pa-daemon/src/stop_cleanup.rs:98-147`) passes the canonical member path as the cron cancel filter. Jobs store the worker's raw `store.path` (`scheduled_jobs.rs:352`), and the cron store matches lexically (`crates/pa-core/src/cron/store/jobs.rs:119/135`). Impact: when the agent or sessions directory path contains a symlink (`/tmp`, `/var/folders`, a symlinked `~/.prime`, an override path), root stop/archive and passivated-child delete leave heartbeat/cron jobs active, and they fire again on resume or revive. Test: `stop_cleanup::tests::finalize_cancels_the_tree_and_archives_the_root`. Fix idea: key the cancel by session id, or canonicalize both sides in the matcher.
2. **Saved-session delete reports `method: trash` for a missing file.** `saved_session_commands.rs:113-135` computes `removed_by_trash = success || !path.exists()`, and macOS 15+ ships `/usr/bin/trash`. The response is `{ok: true, method: "trash"}` for a missing file, and the tombstone hook still runs. Unverified by design (a real `trash` probe would touch the owner's live Trash): if macOS `trash` rejects `--`, real deletes always fall back to a permanent unlink. Test: `protocol_breadth_b10_b11::wave_b11_saved_sessions_wire_shapes`. Fix idea: check the trash exit status separately from file absence, and probe the argument form.
3. **RLM ledger path identity split.** `rlm_ledger_path` hashes a sessions directory that is canonicalized when it exists and raw otherwise. That is the same split class the fork fixed for session leases in `be7a229d3`. Ledger rows publish a canonical `parentSessionPath` while scan/stats publish raw paths, and `pa-tui` file identity is lexical, so file-keyed lineage can miss under a symlinked directory. Not test-backed. Fix idea: canonicalize the nearest existing ancestor, as the lease fix does.
4. **Intentional platform differences (not defects).** `unix_listener_definitely_closed` returns `false` on non-Linux (`crates/pa-types/src/platform/transport.rs:181-186`), so supervisor and worker exits leave their socket files on macOS. The next boot or stale-bind probe unlinks them. TS unlinked by identity. Tests that assert Linux behavior: `socket_identity_guard_e2e`, `worker_orphan_exit_e2e`, and the `supervisor_e2e` SIGTERM drain.
5. **`rlm.create_session` depth-0 roots drop the parent's telemetry opt-out (pre-cutover blocker S2, [#36](https://github.com/junhoyeo/openprime/issues/36); privacy; all platforms).** `crates/pa-daemon/src/rlm_children/lifecycle.rs:146-161` always sends `telemetry_disabled: None`. Its comment says depth 0 installs nothing, but `create_session` launches depth 0. `rlm_children/host.rs:57-78` builds a launch env with only `PATH`, one provider key and `PRIME_TEAM_ID`. Legacy TypeScript forwarded `runtimeConfig.telemetryDisabled` (`703367bad:packages/coding-agent/src/modes/daemon/daemon-mode.ts:2877`, from upstream TS `b9cf467ed`, #2068). So this is an upstream-Rust parity regression outside the 99-commit set. Unlike items 1-4, it **blocks cutover**: see Pre-cutover blockers. Fix idea: thread the parent's opt-out into `create_child`, with a test.

Test-only macOS assumptions in upstream tests (`/proc` reads, `proc:` start-id prefixes, raw-vs-canonical tempdir path equality, Linux AF_UNIX half-close) are listed in [`evidence/macos-triage.md`](evidence/macos-triage.md). They are not rewritten in this migration. The fork made one fork-touched test portable (`idle_passivation_e2e`, `747571c49`).

## Rollback and cutover plan

Rollback evidence is the untouched `origin/develop` tip `703367badf5c84e48e8c8cd986fbcda882b6378c` and the currently installed TypeScript runtime. The migration does not rewrite or delete either.

After every blocker is closed, the integration lead will provide the tested Rust artifact identity, package checksums, fixture hashes, exact cutover commands, and rollback commands. The parent owns merge, default-branch selection, installation, daemon restart, and live verification. Until that explicit action, this branch is not the served runtime.

## Final status

Code-freeze tip: `a9ae2e3defcbdfc4ad4d5d66c1d9b63e2d0a449d`. Commits after it change only docs, the ledger, and the `agent-sessions` skill's example text (synthetic placeholders for a real Aside id, a personal search term, and an owner path in a docstring). The original strings remain in git history in `4af3c6d89` (`feat/agent-sessions-aside`, the legacy PR #33 branch), `b704c720d` (`port/rust-skills`) and `d46fe9be5` (`feat/rust-migration` and its lane branches). They are not in the trees at `703367bad`, `e260085dd`, or the pre-merge `main`. History is not rewritten. The owner decides on branch deletion after the merge.

- **Linux CI at `a9ae2e3de`**: run `40424205627` is green on every job. That is 241/241 job units across 8 test shards, with 6056 tests ok, 0 failed, 22 ignored. The runtime suite (Python 3.11) ran 777 tests, OK. fmt, check + clippy (workspace), the Windows cross-check + clippy, deny, packaging, and the lifecycle checks all pass. Every shard also ran the ignored ABI2 live probe explicitly, and it passed. The benchmarks run `40424203003` fails because the fork has no benchmark secrets, so nothing was measured; it is **not** counted as green. CI on the final docs SHA is recorded in the PR.
- **Ledger**: every row has a final state with exact proof; see [`legacy-commit-map.md`](legacy-commit-map.md). Exactly 8 rows stay `BLOCKED`. They are the pre-cutover blockers below (items 3-5), each with its missing proof named. Five accepted rows carry a parent-approved divergence: kernel GC and retention are retain-only (`2f2cdfb1e`, `632dd62f6`), and the `/fast` unavailable hint keeps upstream wording (`41f5a313d`, `0e87bcc3e`, `6865437be`; the 2-line hint fix goes in the follow-up PR).
- **Independent review**: `final-review-opus` gave CLEAR-FOR-MERGE (scope-limited, P0/P1 = 0) at `a9ae2e3de`, plus a re-key on the final SHA. Its P2 findings are fixed in docs (incident, stale text, ledger) or listed below.
- **macOS and native final runs**: by owner decision, the merge does not wait for them. The full fork macOS suite at `a9ae2e3de`, diffed per test against the c4c baseline under the same harness, and the native final cases (release binary, frozen checker, `DO_NOT_TRACK=1` plus the egress sandbox) are reported on PR #34 after the merge. A real failure there becomes a GitHub issue or a hotfix PR. The c4c macOS baseline (`c4c238ca0`, same harness): 5617 tests, 77 failed, and 4 pa-cli targets that do not compile on macOS.

## Pre-cutover blockers

These do **not** block merging into fork `main`. They **do** block switching the installed runtime. The installed runtime is not switched until each item is fixed, or the owner explicitly accepts it. Each item is tracked as a GitHub issue on `junhoyeo/openprime`, listed per item below. Other follow-ups: [#39](https://github.com/junhoyeo/openprime/issues/39) (the `/fast` hint text), [#41](https://github.com/junhoyeo/openprime/issues/41) (known upstream macOS issues and test-infra), and [#42](https://github.com/junhoyeo/openprime/issues/42) (review P3s: launch-record lifetime, ledger-child wake, test sandbox loopback). Until then, any Rust trial install uses its own agent dir and kernel venv, never the live `~/.prime` state.

1. **S1, prompt-family wakes ignore the waking client's telemetry opt-out** (privacy, fork delta; [#35](https://github.com/junhoyeo/openprime/issues/35)). Details in [`evidence/attach.md`](evidence/attach.md). Fix: carry the stance on prompt-family commands, plus an opted-out prompt-wake e2e test.
2. **S2, `rlm.create_session` depth-0 roots drop the parent's opt-out** (privacy, upstream parity regression vs TS `b9cf467ed`; [#36](https://github.com/junhoyeo/openprime/issues/36)). Details under Known upstream macOS issues, item 5.
3. **Process start-id format mismatch (`ps2:` vs `ps:`)** ([#37](https://github.com/junhoyeo/openprime/issues/37); ledger rows `43556ee4f`, `b24bd42f6`, `07af2c24b`, `0b5676b64` subitem (a)). On macOS, fork TypeScript writes process start ids as `ps2:...`. Rust writes `ps:...` and compares them raw in adoption, the session lease, boot reap, legacy admission, and the `update_flow` successor exit wait. So Rust would treat a live fork-TS process as recycled or exited: it could adopt it, steal its lease, or reap it. **Precondition until fixed:** stop every fork-TS daemon, worker and kernel before any Rust daemon or runtime starts against real state. Fix (first item of the pre-cutover follow-up PR): normalize `ps2:X` to `ps:X`, with tests at each call site.
4. **Missing process-identity proofs** ([#38](https://github.com/junhoyeo/openprime/issues/38)):
   - `aa3ae8fc0`: no test covers the stop path that keeps a live but unverifiable worker's descriptor.
   - `fed4fd4c7`: no test re-validates identity before each signal on the macOS `kill(2)` branch. Linux relies on the pidfd guarantee.
   Fix: add both tests in the same follow-up PR.
5. **Agents-view client path for revival** ([#35](https://github.com/junhoyeo/openprime/issues/35); ledger rows `843615757`, `837be6824`). Daemon-side launch-record replay is proven, and agents-view opens rows through attach. The agents-view chat/reply path is not proven end to end, especially for a saved session without a launch record (the same `Prompt` path as S1). Fix: an end-to-end client test in the follow-up PR, alongside S1.
6. **Stray Rust kernel generation** ([#40](https://github.com/junhoyeo/openprime/issues/40)) `generation-55e2b7c0...` in `~/.prime/agent/kernel-venv.generations/` (see Live kernel-venv incident). Remove it only with owner approval, before cutover.
7. **Cutover manifest** ([#40](https://github.com/junhoyeo/openprime/issues/40)), per Rollback and cutover plan: the tested Rust artifact identity, package checksums, fixture hashes, exact cutover and rollback commands, and the state-compatibility proofs listed under Safety boundary, run against copies of the owner's real state.
