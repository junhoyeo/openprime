# Lifecycle lane evidence snapshot

> Reviewed source: `origin/port/rust-session-lifecycle@2e156424002bcfd7b1576e0ec8935144ac96db3f`.
> Integrated source tip through `a02416f19`; all 40 lifecycle-owned paths byte-match the reviewed tree.
> Combined hermetic evidence: RLM watch 54/54, exact retry-boundary 1/1, engine corrupt-pointer zero-provider 1/1, agents-view partial/legacy warning 1/1, Python runtime registry 30/30. Kernel ABI2/provider safe metadata are integrated separately. Native app/daemon/collect remains paused pending the reviewed read-only path manifest.
>
> **Superseded (final tip):** native runs resumed under the reviewed preflight and the hermetic wrapper; see [`native-collect.md`](native-collect.md) and the native final cases in [`../upstream-rust.md`](../upstream-rust.md). The kernel ABI2 seam is integrated (`9bf6b6a32`, with the CI/runtime preflight `ed0949308`), and every Linux CI shard runs the ABI2 live probe. The PAUSED and "not integrated" statements below are historical review text.

# Final re-review — `2e156424002bcfd7b1576e0ec8935144ac96db3f`

**Verdict: CLEAR-FOR-INTEGRATION (lifecycle lane only; native acceptance remains HOLD).**

- **Kickoff P1 closed:** `KickoffAttemptPhase` and monotonically invalidated `kickoff_epoch` now reserve and validate each first/retry attempt under the child record lock, then release it before every RPC. The exact attempt `CancellationToken` is revoked by cancel/delete/close. Result reconciliation requires matching epoch, `Dispatched` phase, and nonterminal state; a revoked successful dispatch receives an owned abort on the record's exact active session id without setting `prompt_admitted` (`rlm_children/host.rs`, `registry.rs`, final tip). No record/registry guard spans RPC/abort/kill awaits. The new failure->retry gate test (`cancel_at_retry_dispatch_boundary_sends_no_second_prompt`) waits after first failure and proves cancellation leaves `prompt_calls == 1`.
- **Agents-view P1 closed:** actual agents-view fetch negotiates `heartbeat_partial_errors`, retains healthy rows, and sends bounded parsed partial/legacy failure warning into `StatusTone::Warning`. `agents_view/tests/notices.rs` now checks both a negotiated partial catalog and legacy failed refresh preserve heartbeats while rendering visible stale status.
- **Pointer P0 closed at prior `38e42220...`:** real engine malformed-pointer test proves terminal pre-admission error, unchanged bytes, zero provider admission via sole-response valid control. The final tree retains it.
- The final source still keeps canonical pointer containment/rejects duplicate malformed pointers, cancellation persistence/usage label, capability-gated heartbeat behavior, and the final safe metadata projection previously inspected.

**Independent validation performed:** exact origin and detached final SHA verified; `git diff --check c4..2e` clean; final phase/token/wire/test changes read. Owner reports hermetic `rlm_children::watch_tests` 54/54 plus the focused engine and agents-view tests. I did not run native daemon/app acceptance and do not treat unit evidence as it.

**Remaining non-lifecycle holds:**
1. Native app/daemon acceptance is PAUSED pending the separately reviewed resolved-path/synthetic-config preflight. No production/live state claim follows this review.
2. ABI2 kernel successor and provider-consumer corrective work remain separate integration concerns; verify the combined tree's SHA and their gates when assembled.

---

# P0 closure review — `38e42220b68fde4bb76a25b34fc2a319e337b9ab`

Verified the added `agent_engine::tests::corrupt_side_pointer_stops_before_provider_admission` test against the exact tip. It uses a real `AgentSessionEngine` with malformed immutable pointer plus the worker recorder; asserts terminal `SideQuestionOutcome::Failed` with `before admission`, byte-identical parent file, and then consumes the engine's only faux response through a valid-pane control. That valid `"valid answer"` control makes a corrupt-path provider call observable (it would exhaust/fail). This closes the required P0 boundary regression.

**Overall verdict remains BLOCK** for the two explicitly remaining P1 acceptance gaps: exact first-failure→retry-dispatch epoch/barrier proof, and real agents-view negotiated partial + legacy failed-refresh state/render regression. Native acceptance remains PAUSED.

---

# Follow-up re-review — `cee3e6ac8fb2d87b89e3bc160ddb5049eee9b564`

**Verdict: BLOCK.**

- **Pointer P0 behavior:** the typed `ensure_started` error now returns from `AgentSessionEngine::run_side_question` before `side_stream_fn`/`run_side_question` provider admission (`session_engine_impl.rs:1014-1024`), so the earlier source-level charge-without-persistence path is closed. However, the owner confirms there is no engine-level malformed-pointer zero-provider-call regression. This does **not** meet the parent-requested P0 acceptance verifier; require that fixture to prevent a future reorder from restoring the loss.
- **Kickoff remains P1:** `host.rs:298-322` merely observes nonterminal state, then dispatches retry. It does not reserve/revalidate an attempt epoch under lock immediately before the retry RPC. Cancellation can still land after `retry_allowed` is read and before `prompt_child` dispatch, so an accepted retry can start after cancellation. The new test cancels while the first RPC remains gate-blocked, i.e. before it returns failure; it does not prove the specified *first-failure -> retry-dispatch* boundary. Require an explicit boundary barrier and per-attempt epoch/reservation without an await-held mutex.
- **Agents view:** the capability negotiation and visible `StatusTone::Warning` route now exist (`agents_view/heartbeats.rs:14-46`, `agents_view.rs:1062-1080`), resolving the prior silent-consumer behavior at source level. No changed agents-view state/render regression test appears in this commit, despite the requested real partial and legacy failure test. Add it before acceptance.

Native acceptance remains PAUSED.

---

# Re-review addendum — blocker-fix tip

**Re-reviewed final tip:** `cb725539479f6fed9e349e30cfa63fd1e23ba4bf`
**Base:** `c4c238ca025e382d96fb5f37e6328ca77fb82ef3`
**Verdict:** **BLOCK** (two P1 lifecycle regressions remain; all native acceptance remains paused).

## Closed from the prior review

- The recorder is started before side work, cancellation now carries/drains the latest streamed assistant, and persisted status carries `usageKnown` (`side_question.rs:630-640,742-751`; `session_engine_impl.rs:1013-1018`; `side_question_store.rs:137-240,287-306`).
- Pointer resume now rejects duplicate/missing records and canonicalizes root/path; the new traversal and current-symlink tests keep sentinels unchanged (`side_question_store.rs:71-130,574-708`).
- New heartbeat negotiation preserves the old fail-whole response unless `acceptPartialErrors` is set; the session heartbeat manager requests it and renders a bounded picker warning (`scheduling_catalog.rs:454-581`; `session_ui/heartbeats.rs:142-277`).
- The kickoff mutex was removed. Admission is state-claimed under the record mutex and RPC calls occur unlocked (`rlm_children/host.rs:255-368`; `registry.rs:61-155`).

## Remaining blockers

1. **P0 — malformed/traversal pointer failures are only logged, but the side run continues chargeable work with persistence disabled.**
   - **Paths:** `crates/pa-daemon/src/side_question_store.rs:137-155,175-178`; `crates/pa-daemon/src/agent_engine/session_engine_impl.rs:1013-1056`.
   - **Counterexample:** A reconnect finds a malformed, duplicate, traversal, or symlink pointer. `ensure_started` catches the error, sets `state.disabled = true`, and only calls `eprintln!` (147-153); it returns `()` to the engine. `run_side_question` then continues into the model call (1025-1053), and the terminal `record_turn` is a no-op because the recorder is disabled (175-178). The UI reports a normal completed/cancelled answer while its paid completion/usage are unpersisted. The new malformed test proves preserved bytes/no duplicate but does not route through the engine or prove that a chargeable call is refused or visibly reported.
   - **Minimal remedy:** Make recorder initialization a typed, user-visible outcome. For any pre-existing pointer validation failure, stop before provider admission and surface a recoverable persistence error (or explicitly persist an equivalent safe error record without changing the corrupt pointer). Add an engine-level test proving malformed/traversal pointer makes zero provider calls and returns the clear failure while original bytes remain exact.

2. **P1 — a cancellation/delete racing a failed first kickoff still permits the unconditional retry to admit a prompt after terminal state.**
   - **Path:** `crates/pa-daemon/src/rlm_children/host.rs:287-325`.
   - **Counterexample:** A positive collect claims `kickoff_started`, the first `prompt_child` fails without a durable kickoff row, and cancellation then marks `settled_status = cancelled` / sends abort. The watcher performs the retry at 305-313 without checking `closed_by_parent`, `retiring`, or `settled_status`. That retry can admit the task after cancellation; only after it returns does the later block see terminal state and send a second abort (327-356). This violates the stated late-completion rule: no resurrection, no post-cancel task admission/spend.
   - **Minimal remedy:** Re-check a per-attempt epoch/terminal reservation immediately before *each* retry RPC; do not retry after terminal/retiring state. Add a barrier test where first prompt fails, cancellation wins before retry release, and assert retry prompt count remains zero, no worker turn starts, and the terminal result remains cancelled.

3. **P1 — the agents-view heartbeat consumer neither negotiates partial errors nor displays a bounded failure/staleness indicator.**
   - **Path:** `crates/pa-tui/src/agents_view/heartbeats.rs:9-27`.
   - **Counterexample:** This actual TUI consumer always sends `HeartbeatsList { rest: {} }` (16-20). On a new daemon with an omitted worker it gets the legacy failure response, then silently discards it through `if let Ok(data)` (21), retaining the old badges with no error row. Thus it neither shows healthy partial rows plus a warning nor tells the operator that counts are stale. The session `/heartbeats` picker is fixed, but the agents view remains contrary to the all-consumer requirement.
   - **Minimal remedy:** Capability-negotiate `acceptPartialErrors` in this request too; carry `heartbeat_worker_error` through its UI input/state and render the bounded partial/stale warning beside the affected catalog/badges. Alternatively explicitly render the legacy failed refresh state. Add a headless agents-view test covering a healthy row plus omitted worker and an old-daemon failure.

## Holds

- Native daemon/app acceptance is **PAUSED**. The prior isolation incident remains non-accepted; no rerun is authorized until the reviewed native resolved-path/synthetic-config preflight helper prevents out-of-sandbox writes.
- Kernel ABI2 successor `04be4b1c39a254b75af9bc14f91c08de7e350b95` is separate/unintegrated. Provider corrective work remains separate. A combined safe tree requires rereview.

## Evidence

- Fresh detached scratch worktree at exact `cb725539479f6fed9e349e30cfa63fd1e23ba4bf`; `git diff --check c4..cb` clean.
- A first hermetic lib-test invocation with a new synthetic HOME did not execute tests because rustup has no toolchain under that synthetic HOME (environment/toolchain setup failure, not a product failure). The retry used the existing offline/read-only Rust toolchain/cache with synthetic HOME/TMPDIR and a private target: `cargo test -p pa-daemon side_question_store::tests --lib` passed **7/7** (0 failures). This validates the direct recorder fixtures only; it does not exercise the remaining engine-level P0 provider-admission path. No daemon, provider, kernel, or native acceptance process was started by this review.

---

# OpenPrime Rust lifecycle migration — adversarial review

**Reviewed final tip:** `fbefc018bbd861b051b867a56d32b0a8574b228c`
**Frozen base:** `c4c238ca025e382d96fb5f37e6328ca77fb82ef3`
**Scope:** final `c4..fbef` tree (32 files, 1,979 insertions / 80 deletions). The unsafe intermediate `b4445ed1788374a091870198c3cc5d49fbe0572b` was not assessed as shipped; its safe metadata successor is `dae6c891652bb50156a7dd8a5ab2192495fc5fd4`.

## Verdict: BLOCK

### Blockers

1. **P0 — cancelled side questions drop streamed paid completion and may leave no durable pane at all.**
   - **Paths:** `crates/pa-core/src/session_engine/side_question.rs:574,630-633`; `crates/pa-daemon/src/agent_engine/session_engine_impl.rs:1054-1064`; `crates/pa-daemon/src/side_question_store.rs:228-230`.
   - **Counterexample:** Start the first `/btw` turn, let the provider stream an `AssistantMessage` with usage, then cancel before `wait_for_idle`. `run_attempt` records the partial in `latest_streamed_message` (lines 718-722), but the `AttemptOutcome::Aborted` arm returns `Ok(None)` (574) and `SideQuestionResult::cancelled(answer)` deliberately contains `assistant: None` (77-85, 630-633). The daemon only calls `record_turn` when `assistant` is present (1054-1055). `record_status(Cancelled)` returns without creating a transcript (228-230). Result: the visible terminal answer can be partial, but its paid assistant message/usage and even the pointer are absent from disk. The existing focused unit test proves the partial-cancel condition (`abort_mid_run_cancels_with_partial_answer` returns `Cancelled` with `partial answer`).
   - **Minimal remedy:** Carry the latest streamed assistant completion (when it is a paid/completed message) in a cancelled result and initialize/persist the pointer+transcript before writing the cancelled status. Add an end-to-end recorder test that starts with an empty pane, streams usage, cancels, and asserts exactly one pointer, the assistant usage row, and the cancelled terminal record after restart.

2. **P0 — a corrupt/malicious legacy pointer can traverse outside the parent artifact root and be appended to.**
   - **Path:** `crates/pa-daemon/src/side_question_store.rs:57-103`, especially 87-103.
   - **Counterexample:** Parent JSONL contains a matching `side_question` pointer whose `sessionFile` is `/agent/session-artifacts/<parent-id>/../../victim.jsonl`; `Path::starts_with(expected_root)` is lexical and returns true for this path (verified with native Rust: `/tmp/a/session-artifacts/parent/../../victim.jsonl: true`). If the target contains a syntactically valid `SessionFile` header with `parentSession` matching the parent and `rlmDepth: 0` (or is reached through a symlink to one), `SessionFile::open` preserves the supplied raw `path` (`session_store/read.rs:128-153`) and later `transcript.persist_entry` appends to that raw path (`session_store/write.rs:226-250`). This violates the required no-traversal/no-arbitrary-overwrite behavior for corrupt pointer records.
   - **Minimal remedy:** Reject `ParentDir` components; canonicalize both the existing candidate and root before accepting it, reject symlinks/escaping aliases, and retain/use the validated canonical path for future writes. Add traversal and symlink negative tests.

3. **P1 — malformed/missing matching pointers are silently replaced, breaking immutable one-pointer semantics and hiding recovery failure.**
   - **Path:** `crates/pa-daemon/src/side_question_store.rs:65-85, 159-188`.
   - **Counterexample:** A legacy parent row has `customType: "side_question"`, matching `btwId`, but lacks `data.sessionFile` (or has invalid JSON / a partially written record). The `filter_map` drops it, `linked.next_back()` returns `None`, and `record_turn_inner` creates and appends a new pointer. There is neither an explicit error nor a repair marker. Multiple valid matching pointers also silently select the last one. This loses a clear recovery outcome and violates the immutable parent-pointer/exactly-once contract.
   - **Minimal remedy:** Parse matching rows separately from valid paths. If any matching record is malformed, partial, missing, ambiguous, or cannot be opened safely, surface a clear durable error and do not add another pointer. Test old valid data resumes, while malformed/missing/duplicate pointers neither write nor overwrite.

4. **P1 — heartbeat worker errors are sent but discarded by every TUI consumer, so a partial/stale catalog is still shown as healthy.**
   - **Paths:** producer `crates/pa-daemon/src/scheduling_catalog.rs:515-533, 574-580`; consumers `crates/pa-tui/src/session_ui/heartbeats.rs:159,191-193`, `crates/pa-tui/src/agents_view/heartbeats.rs:21-25`, parser `crates/pa-tui/src/heartbeats_picker/data.rs:102-121`.
   - **Counterexample:** One worker has no current snapshot. The daemon returns `success: true`, healthy rows, and `workerErrors:[{stale:true, omitted:true,...}]`. `parse_heartbeats` reads only `heartbeats`; both the picker/dock and agent view install the partial result with `fetch_error: None` or no error channel. Operators therefore see an apparently successful empty/partial list rather than the required explicit failure/staleness signal.
   - **Minimal remedy:** Parse bounded worker diagnostics into a typed catalog result, preserve/render an explicit warning in the picker and agents view, and add UI integration coverage for an omitted and a stale worker slice. Do not treat this response as a clean catalog.

5. **P1 — collect/cancel/delete now hold the per-child kickoff mutex across supervisor RPC awaits (up to the 30-second prompt/kill timeout), violating the no-held-lock requirement and making cancellation nonresponsive behind a stuck kickoff.**
   - **Paths:** `crates/pa-daemon/src/rlm_children/host.rs:261-319, 516-573`; `crates/pa-daemon/src/rlm_children/registry.rs:77-112`; timeout `crates/pa-daemon/src/rlm_children.rs:199-201,69`.
   - **Counterexample:** Positive `collect` releases a queued child, and `prompt_child` hangs. The detached kickoff owns `kickoff_lock` while awaiting prompt and retry RPCs (261-308). A concurrent cancel/delete/parent close blocks at its own `kickoff_lock.lock().await` (registry 77; host 517); once acquired, cancel/delete also retain the lock while awaiting abort/kill RPCs. Thus a cancellation cannot claim terminal state until a potentially 30-second supervisor call returns, despite the stated race/latency contract. Existing race tests release the fake prompt gate and do not exercise a permanently stalled RPC.
   - **Minimal remedy:** Replace the await-held gate with a short critical-section admission state (claimed/cancelled/retiring generation) and release the mutex before every RPC. Re-check the generation after admission; if cancellation won, abort an admitted child without resurrecting it. Add a deterministic gate test proving cancel/delete returns/claims terminal before an unreleased kickoff RPC completes.

### Verified non-blocking observations

- The final safe metadata projection at `crates/pa-daemon/src/agent_engine/session_engine_impl.rs:574-591` allowlists `{id,name,api,provider,reasoning}` plus optional boolean `supportsFastMode`; the actual connection-state test at `agent_engine/tests/model_resolution.rs:112-215` checks true/false/absent and forbids base URL/header secrets. No metadata leak finding against the final successor.
- `paneId` is capability-gated in the TUI at `crates/pa-tui/src/session_ui/prompt.rs:524-542`; it is omitted for older daemons. This does not cure the persistence blockers above.
- The lifecycle code correctly added target-scoped positive collect release, a shared monotonic deadline, queued status, and terminal wait notifications in the final tree. This review does not treat the superseded unsafe metadata commit as live.

### Known unintegrated seams / acceptance holds

1. **Kernel ABI2 boundary (#32): not integrated at this reviewed tip.** Runtime exports exact `RLM_RUNTIME_ABI = 2` in `prime-agent-runtime/src/rlm/__init__.py:22-27`, but no Rust bootstrap probe exists in this final tree. The owner subsequently pushed the reviewed seam at `04be4b1c39a254b75af9bc14f91c08de7e350b95` on `origin/port/rust-kernel-safety` (reported fmt/check/clippy clean, host ABI probe and hermetic generation cases passed). It remains a separate branch; do not claim ABI2 integration until that exact successor is combined and rerun against this lifecycle lane.
2. **Provider consumer integration:** explicitly owned by the separate `rust-provider-review` corrective branch. It is not reviewed or integrated here; combined provider/lifecycle gate remains pending.
3. **Owner native in-turn collect proof:** pending. The lane owner invalidated its first receipt after discovering missing `PRIME_AGENT_CODING_AGENT_DIR` let a synthetic artifact reach the real agent state; it stopped that daemon and is rerunning with both agent-dir environment aliases isolated. This review does not count the invalid receipt.

### Evidence executed

- Created detached scratch worktree at the exact tip; `git rev-parse HEAD` and `origin/port/rust-session-lifecycle` both resolved to `fbefc018bbd861b051b867a56d32b0a8574b228c` before review.
- Read repository `AGENTS.md`, affected crate READMEs, final affected code paths, and final diffs. `git diff --check c4..fbef` was clean.
- Native Rust path semantics reproduction: `Path::starts_with` accepted `/tmp/a/session-artifacts/parent/../../victim.jsonl` under `/tmp/a/session-artifacts/parent`.
- `CARGO_BUILD_JOBS=2`, private `CARGO_TARGET_DIR=/tmp/openprime-review-rust-lifecycle-target`: `cargo test -p pa-core abort_mid_run_cancels_with_partial_answer --lib` passed (1 passed, 1269 filtered, 0.10s). It establishes the exact partial-stream cancellation condition used in blocker 1; it does not verify persistence.
- `CARGO_BUILD_JOBS=2`, same private target: `cargo test -p pa-daemon --test protocol_breadth_b10_b11 wave_b10_heartbeat_snapshot_fallback -- --exact` passed (1 passed, 6 filtered, 8.71s). It confirms the daemon producer emits `workerErrors`; it also confirms why the unmodified TUI consumer gap remains material.

# Authoritative final verdict (append-only handoff)

**CLEAR-FOR-INTEGRATION — lifecycle lane only**

- **Final reviewed SHA:** `2e156424002bcfd7b1576e0ec8935144ac96db3f`
- **Frozen base:** `c4c238ca025e382d96fb5f37e6328ca77fb82ef3`
- **Closure:** side pointer P0 is covered by actual `AgentSessionEngine` zero-provider pre-admission failure/control test (`38e42220...`); agents-view partial/legacy stale source behavior is covered by mode state/status test; the final `KickoffAttemptPhase` / epoch / per-attempt token implementation releases locks before RPC, revokes cancel/delete/close attempts, reconciles only exact matching attempts, and has a post-first-failure retry boundary one-prompt test. `record.active_session_id` is created/reseeded as record-owned value and this field has no mutation assignment in the RLM child modules; final compensation uses the attempt-captured created active-session id, not a current-session lookup.
- **Static gate:** `git diff --check c4..2e` clean.
- **Native:** app/daemon/collect acceptance is not run and remains PAUSED. This verdict makes no production/live-state claim.
- **Combined seam hold:** exact kernel ABI2 successor and provider-consumer corrective integration are separate. Verify their final combined-tree SHAs and gates when integration assembles them.

## Final reviewer-run hermetic receipts (exact `2e156424002bcfd7b1576e0ec8935144ac96db3f`)

Approved isolated product roots plus explicit read-only `CARGO_HOME`/`RUSTUP_HOME`, private `CARGO_TARGET_DIR`, `CARGO_BUILD_JOBS=2`, offline Cargo; no daemon/app/collect native invocation:

- `cargo test -p pa-daemon cancel_at_retry_dispatch_boundary_sends_no_second_prompt --lib` — **1 passed**.
- `cargo test -p pa-daemon corrupt_side_pointer_stops_before_provider_admission --lib` — **1 passed**.
- `cargo test -p pa-tui heartbeat_partial_and_legacy_failures_render_visible_stale_status --lib` — **1 passed**.

A preliminary combined Cargo filter matched zero tests and is not counted; the three individual receipts above are the actual verifier results.
