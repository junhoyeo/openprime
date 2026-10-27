# state-compat-review — origin/port/rust-state-compat-v2 @ 0c6a74564f7e2876c42274351afe2cf3a9682ccb

Reviewer: state-compat-review (read-only; child of mig-lead-opus). No edits, commits, or pushes to any branch.
Scratch: detached worktree ~/prime-agent-worktrees/review-state-compat (clean at 0c6a745 after review),
private target ~/prime-agent-worktrees/targets/review-state-compat (APFS clone of targets/rust-state-compat).
Every cargo command ran through /tmp/mig-lead-opus/hermetic_test.py (env -i, synthetic HOME, no-egress sandbox)
with CARGO_TARGET_DIR=<private>, CARGO_BUILD_JOBS=2, CARGO_INCREMENTAL=0. Logs: /tmp/mig-lead-opus/review-sc/.
Disk before compiling: 54 GiB free; lowest seen 42 GiB (stayed above 35 GiB).

## VERDICT: BLOCK (one P1, in the branch's own claim area). No regression found. All author gates are green.

The two product fixes are correct and red-first. The tests pass and prove most claims. The block is P1-1 below.
The branch claims that legacy v1 sessions load (`a_legacy_v1_session_migrates_to_a_linear_context`). But a v1 file
with a `compaction` row (a real v1 writer shape) still loses its summary and kept history. The migration rewrite
then makes the loss permanent. This is the same bug class as the hookMessage fix (an Unknown row that the
migration skips), in the function next to it. If the lead scopes v1 compaction out of this PR, downgrade to
CLEAR with P1-1 as a follow-up. In that case, the ledger must not record "v1 sessions load" as closed.

## Gates I ran (hermetic, exact SHA 0c6a745)

| Gate | Command | Result |
| --- | --- | --- |
| fmt | `cargo fmt --all -- --check` | EXIT 0 |
| legacy | `cargo test -p pa-core --test legacy_state_compat` | 9 passed, 0 failed |
| auth | `cargo test -p pa-core --lib auth::manager::tests` | 16 passed (incl. `a_legacy_auth_document_resolves_and_survives_a_rust_write`) |
| side_question | `cargo test -p pa-daemon --lib side_question` | 14 passed (incl. both `a_ts_written_side_conversation_*`) |
| wake e2e | `cargo test -p pa-daemon --test saved_session_wake_e2e` | 3 passed (incl. `send_to_a_ts_archived_session_reopens_it_with_its_history`), 14.4 s |
| pa-perf | `cargo test -p pa-perf corpus` | 13 passed |
| clippy | `cargo clippy -p pa-core -p pa-perf -p pa-daemon --all-targets -- -D warnings` | EXIT 0, 0 warnings |

Wrapper survivor reports (exit 3) were false positives: my own `bash` parent (its command line holds the kernel-python
marker), another session's pa-tui/pa-daemon run (HOME=/tmp/pa-so5fd578), and an `rlm.repl` PID 4957 with
HOME=/tmp/pa-so23126f/h. None was under my run roots (/private/tmp/pa-h5rgji79_ and others). I killed nothing.
Isolation: after all gates, the synthetic HOME/XDG dirs of the gate root (/private/tmp/pa-h5rgji79_/{h,x,c}) were
empty. No test resolved a home.

## Red and mutation controls (scratch only, all reverted; worktree clean)

1. Revert both product files to 11a949bb0 (`session/mod.rs`, `refinement/mod.rs`): `legacy_state_compat` gives
   6 passed, 3 FAILED (`a_legacy_v2_hook_message_migrates_to_a_custom_message`, `a_topic_era_harness_entry_loads_under_path`,
   `saving_after_a_legacy_load_never_erases_an_entry`). This matches the README red-first table.
2. Mutation: drop only the topic->path insert: 2 FAILED (topic test and save test). Covered.
3. Mutation: in `save_harness_state`, let a preserved row overwrite a typed row with the same id (remove the
   `!records.contains_key(id)` guard): 9 passed. **The guard has no test** (see P2-3).
4. Probe: copy each session fixture, open it (this triggers the migration rewrite), and reopen it. v1 and v2 are
   rewritten, with no `"type":"unknown"` and no `hookMessage` left. The context is identical before and after the
   reopen. v3 is not rewritten.
5. RED `red_v1_compaction_row_keeps_ts_context` (P1-1): got `[user q3, assistant a3]`; TS gives
   `[compactionSummary, q2, a2, q3, a3]`.
6. RED `red_create_over_preserved_row_is_refused_like_ts` (P2-1): Rust applies the create and replaces the row.
7. RED `red_delete_of_preserved_row_works_like_ts` (P2-1): Rust returns "entry not found" and keeps the row.
8. Probe (P2-2): a non-object record (`"junk": "not an object"`) is kept on save. TS drops it. A refinement event that
   lacks `evidence/outcome/created_at` is dropped on save. TS keeps it verbatim, and Python keeps it with defaults.

## Findings

### P1-1 — v1 `compaction` rows break the chain, and the rewrite makes the loss permanent (pre-existing, inside the claim)
- The legacy v1 writer (`git show c58d5f20a^:packages/coding-agent/src/core/session-manager.ts:42-48`) writes
  `{"type":"compaction","timestamp","summary","firstKeptEntryIndex","tokensBefore"}`. That row has no `firstKeptEntryId`.
- TS `migrateV1ToV2` (703367bad `session-manager.ts:342-367`) gives every row an id and a parentId. It then resolves
  the **top-level** `firstKeptEntryIndex`.
- Rust `crates/pa-core/src/session/mod.rs:178-222`:
  (a) `CompactionEntry.first_kept_entry_id` is required (`pa-types/src/session.rs:192`), so the row parses as
  `FileEntry::Unknown`. `entry_base_mut` returns None for Unknown (`:226`), so the row gets no id, but
  `prev_id` still moves forward (`:195`). The next row's `parentId` then points at an id that does not exist.
  (b) The index is read from `details.firstKeptEntryIndex` (`:199-203`), not from the top-level field.
- Result: the context is only the turns after the compaction. `open()` then rewrites the file (`manager/lifecycle.rs:335-346`).
  The rewrite writes the dangling parentId and the row as `{"type":"unknown","type":"compaction",...}` (a duplicate key,
  see P2-5). The file now says version 3, so the migration never runs again.
- `sessions/v1-linear.jsonl` has no compaction row, so the test does not see this. The fixture was not bent to fit
  Rust. It is incomplete.
- Fix: do the same as cb7235de9 does for v2. Migrate the raw JSON of Unknown rows (insert id/parentId into `rest`,
  move the top-level `firstKeptEntryIndex` to `firstKeptEntryId`), parse the row again, and add a v1 compaction
  row to the fixture.

### P2-1 — Rust cannot update or delete preserved harness rows, and `create` overwrites them (differs from TS)
- `load_harness_state` puts rows that do not type into `preserved_entries` (`refinement/mod.rs:207-216`). The
  planner reads only `state.entries` (`planner.rs:354-355`). Results:
  - delete/update of such an id gives "entry not found" (`planner.rs:390-396,412-416`). TS deletes or updates the row
    (refinement.ts kept it through the spread at :363-370).
  - create on that id is applied (TS: "entry already exists", refinement.ts:1111). On save, the typed row wins
    (`mod.rs:338`), so the hand-edited row is **lost**. A later rollback of that create deletes the new row and
    cannot bring the old row back.
  - create+delete in the same proposal puts the old row back (probe confirmed). The net disk result is
    unchanged, so this is acceptable.
- Duplicates: none. The typed row always wins on save, and each update loads from disk again under the
  lock (`try_update_harness_state` :372-384), so stale preserved rows do not carry over between updates.
  Reorder: with preserve_order, preserved rows are written after the sorted typed rows. The order is stable on
  later saves but can differ from the original file order (cosmetic). Resurrection after a delete: only in the
  same-proposal case above.
- Suggested fix: let create/update/delete see `preserved_entries`. At minimum, refuse create on a preserved id with
  "entry already exists". Add a test.

### P2-2 — the preservation is lossless only for rows that are objects, and not for refinement events
- A non-object record is kept forever (`normalize_stored_entry` returns it unchanged at `:242-244`, the parse fails, and the row is preserved).
  TS drops it (`refinement.ts:357-358`). This is harmless but differs from TS.
- `refinements[]` items that do not type are still dropped on save (`mod.rs:226-234`, filter_map). TS keeps the array
  verbatim (:374-376). Python keeps such events with default values (`harness.py:598-615`). So a Rust save can still
  erase legacy refinement events that lack `evidence`, `outcome`, or `created_at`.
- Typed rows have no `#[serde(flatten)] rest`, so unknown extra fields on an otherwise valid row are dropped on save.
  TS keeps them through the spread.

### P2-3 — the precedence guard has no test
Mutation 3 survived: no test covers a typed row and a preserved row with the same id.

### P2-4 — Python kernel writers still erase what the Rust loader keeps (legacy parity, not a regression)
`prime-agent-runtime/src/rlm/harness.py` is byte-identical to 703367bad (`git diff --stat 703367bad HEAD` is empty).
Its `load` (:558-595) skips rows whose `title`/`content` are not strings, and drops `topic`. A missing `path`
becomes `"general"`. Its `save` (:627-655) writes only typed rows. So any kernel `rlm.harness.create_*` erases
`hand_edited` and makes `topic_entry` lose its grouping (it becomes "general") if the kernel writes before Rust does.
Both writers use the same `harness_state.json.lock`, so the race is safe, but the README claim "a load-save cycle
never erases an entry" is true only for the Rust writer. Document this, or port the same preservation to harness.py.

### P2-5 — `FileEntry::Unknown` serializes with a duplicate `type` key (pre-existing, pa-types)
`FileEntry` is internally tagged and `Unknown { #[serde(flatten)] rest }` (`pa-types/src/session.rs:344-450`). So
`serialize_entry` (`session/manager/repair.rs:6-8`) writes `{"type":"unknown","type":"<original>",...}`. JSON.parse,
serde_json Value, and Python all keep the last value, so a re-read works. But every rewrite of a file that holds an
Unknown row produces non-canonical JSON. Seen in the P1-1 probe. Worth a separate fix, for example a
custom Serialize that writes `rest` verbatim.

### P2-6 — fixture fidelity notes (none bent)
- Matches the legacy writers: header fields (session-manager.ts:1863-1872, minus `git`), `custom` pointer key order and
  fields (:2159-2167; side-question-store.ts:46-57,139-151), `session_info`/`session_state` (:820-838),
  `btw-<id>` dir (:41,:128), auxiliary completions before the kept answer (:182-185), failed turn plus status
  (side-question.ts:626-630), hookMessage shape (`git show c6fc08453^:packages/coding-agent/src/core/messages.ts:46-53`),
  archived row (daemon-mode.ts:6998-7000), topic-era row (refinement.test.ts:739-766).
- Gaps: no fixture header has `git` (real repo sessions have it). The `v3` custom row puts `id,parentId,timestamp`
  first (cosmetic). The auth fixture's `google-gemini-cli` OAuth provider does not exist in 703367bad `packages/ai/src`
  (no grep hits), so that row is synthetic. It is valid under the open index signature, but it is not "a shape the
  legacy writer produces". The v1 header copies the older `large-session.jsonl:1` variant (with provider/modelId/thinkingLevel). The
  writer just before v2 (c58d5f20a^ :262-267) wrote only `type,id,timestamp,cwd`. Both are valid v1 shapes.
- hookMessage cannot appear in v1 files: hooks arrived on 2025-12-27 (75a9c3c71), after v2 (c58d5f20a, 2025-12-25).
  So the v1 `Unknown` path is not a hookMessage risk. The P1-1 compaction row is the risk.

### P2-7 — AGENTS.md style
- `normalize_stored_entry` (`refinement/mod.rs:241`) has one call site. AGENTS.md says "No single-use helper methods".
  The test helper `entry_ids` (legacy_state_compat.rs) is also single-use.
- The new `pub preserved_entries` adds to the public API. It is pub only because pa-perf builds `HarnessState`
  with a struct literal (`pa-perf/src/corpus/artifacts.rs:115`). `#[serde(skip)]` keeps it out of the wire and the
  fingerprint. This is acceptable, but `..empty_harness_state()` in pa-perf would avoid the cross-crate field.
- Crate boundaries, dependency direction, file sizes (all < 1,000 lines), fmt, and clippy are fine.

## Product fix 1 — hookMessage migration (cb7235de9): correct
`migrate_v2_to_v3` (`session/mod.rs:248-274`) matches TS `migrateV2ToV3` (:369-383). It sets the header version to 3
and renames `message.role` from `hookMessage` to `custom` on `type:"message"` rows. Rust has to do this on the raw
`Unknown` JSON and parse the row again, which is correct. `CustomMessage.rest` (flatten) keeps extra fields. One
small difference: if the parse fails after the rename, Rust keeps the row Unknown with `hookMessage`, while TS
renames it anyway. This cannot happen for the HookMessage shape (customType/content/display/details?/timestamp).
Proof: red-first (control 1), and the rewritten file reopens with the same context (probe 4).

## Product fix 2 — harness preservation (4c7aac5f9): correct for its stated scope, not fully lossless
Normalization matches TS `loadHarnessState` (:351-373): topic->path only when `path` is not a string, `topic` dropped,
non-object reference/arguments/metadata become `{}`, an invalid scope falls back to the load scope. The new
`#[serde(default)]` on source/created_at/updated_at matches TS, which kept such rows. Concurrent Rust/Python writers
serialize on `harness_state.json.lock`, and the Rust update loads again under the lock, so the preserved set is never
stale. Limits: P2-1 to P2-4. There is no upstream invariant conflict: merge/render/fingerprint/baseline read only
`entries`.

## Tests vs claims
- Full spend: proven for reading. `session_stats` over the TS side transcript gives input 1050 / output 85 / cacheRead 40 /
  total 1175 / cost 0.004425. I recomputed this by hand from the fixture. It includes the auxiliary summary, the
  overflow answer, and the failed billed turn (6567e228f). The resume test adds 17 tokens (total 1192).
- Parent linkage: proven. `parentSession` equals the parent path and `rlmDepth` is 0. Resume uses the legacy pointer (parent bytes
  unchanged). The side file only grows (prefix check). `resume_linked_transcript` (`side_question_store.rs:67-128`)
  also checks the root, the canonical path, the parent, and the depth.
- Visible history: proven at the store level (`SessionFile::messages`). The `/btw` pointer stays out of the parent
  context. Note: TS never showed a closed pane again in the UI either, so the store level is the correct bar.
- Archived reopen: proven end to end (daemon supervisor, send by name, worker wakes, TS-era turn shown, legacy bytes
  stay a prefix of the file).
- Auth value identity: proven by `Value` equality after load and after a Rust write to another provider (the order is not
  compared, and the order is not required). See the gemini note in P2-6.

## saved_session_wake_e2e `PRIME_AGENT_SESSION_DIR` pin: harness fix, not product weakening
`pa-daemon/src/paths.rs:55-66` `sessions_dir` returns the override when it is set, else `<agent-dir>/sessions`. The
test pins the env to exactly `<agent-dir>/sessions` (the product default). An ambient override from the shell that
runs the tests (Prime sessions export it, see PID 89404's env) can no longer send the catalog scan to a different directory. The
value equals what the product computes with no override, so this hides no product path.

## Per-ledger-row notes
- **a4ca8cbd8** (btw linked sub-sessions): the TS-written parent pointer and side transcript open, link, and resume into the same
  transcript with one pointer. Covered by `a_ts_written_side_conversation_{opens_with_its_history_and_full_spend,resumes_into_the_same_transcript}`
  and the existing `legacy_pointer_and_transcript_resume_without_a_duplicate_parent_link`. OK.
- **6567e228f** (pointer + full spend): auxiliary completions and the failed billed turn are counted on read (exact numbers above). The pointer
  durability (flushNow) is a TS writer concern. The Rust `persist_entry` writes before it indexes (`side_question_store.rs:230-232`). OK.
- **1bfc4904b** (capability-gate pane ids on the wire): this branch does not touch it. The existing
  `protocol::tests::side_question_pane_identity_is_optional_and_capability_advertised` passed in my side_question run.
  It does not need legacy state. OK as covered elsewhere (not re-diffed against TS wire bytes here).
- **a4599defb** (settle btw panes reliably): not exercised by this branch beyond the existing `side_question::tests::*`
  settle tests, which passed (detach_abort_settles…, shutdown_settle…, same_id_restart…). It has no legacy-state aspect.
- **502e7a496** (revive archived on prompt): proven for a TS-archived root by the new e2e. OK.
- **6470a60e7** (do not revive for a cancelled prompt): **not evidenced by this branch.** The TS fix is a client-side
  abort race (`daemon-agent-connection.ts`). This branch adds no Rust test for "aborted send does not create or revive". It still
  needs its own spot-check. `revival_gate.rs` tests cover archived and lease vetoes, not prompt cancellation.
- **8b5ec62f8** (preserve credentials after gateway auth failures): the new auth test proves legacy auth.json
  load and write identity only. It does not exercise a 401. Separate evidence: `mark_auth_stale` has **no production caller**
  (grep: only `auth/manager/tests.rs`, `models/registry/tests.rs`, and a test in `pa-daemon/src/model_switch.rs:500`), so
  Rust never marks stored credentials stale after an auth failure. The invariant holds by absence, but no test guards
  it. Suggest a regression test: a 401 turn leaves `auth.json` and `get_api_key` unchanged.
- **Harness topic migration**: correct and red-first (controls 1 and 2). Limits: P2-1 (refine cannot act on preserved
  rows, and create overwrites them), P2-2 (non-object rows kept, untyped refinement events and extra fields dropped),
  P2-3 (precedence guard has no test), and P2-4 (Python kernel writer still erases preserved rows and topic).

## Exact scratch artifacts
- /tmp/mig-lead-opus/review-sc/review_scratch.rs (red tests 5-8 source), scratch.log, scratch2.log
- mutation-revert-both.log, mutation-preserved-overrides.log, mutation-no-topic.log
- gates.txt, {fmt,legacy,auth,sideq,wake,perf,clippy}.log, build.log, gates-wrapper.log


---

# Re-review: origin/port/rust-state-compat-v3 @ d7eaa6a423136240265eaf826c44b2bad1d898ca

Delta 0c6a74564..d7eaa6a42: 9f733e83c (v1 compaction migration), 580997790 (create refusal over a preserved row),
4b4fadfcf (precedence test), f56020d1a (inline single-use helpers), d7eaa6a42 (docs on the limits).
Same rules: hermetic wrapper, scratch worktree ~/prime-agent-worktrees/review-state-compat (detached at d7eaa6a, clean after the review),
private target, CARGO_BUILD_JOBS=2, CARGO_INCREMENTAL=0, RUST_TEST_THREADS=4. Disk: 34 GiB free at start and end (floor 32).
Logs: /tmp/mig-lead-opus/review-sc/v3/.

## VERDICT: CLEAR for d7eaa6a423136240265eaf826c44b2bad1d898ca

## Tip gates (one hermetic run; wrapper: "exit 0; no surviving processes", root /private/tmp/pa-hcp9a6shn)
| Gate | Result |
| --- | --- |
| cargo fmt --all -- --check | EXIT 0 |
| cargo test -p pa-core --test legacy_state_compat | 11 passed (new: a_legacy_v1_compaction_keeps_its_summary_and_kept_turns, a_typed_entry_wins_over_a_preserved_row_with_the_same_id) |
| cargo test -p pa-core --lib auth::manager::tests | 16 passed |
| cargo test -p pa-daemon --lib side_question | 14 passed |
| cargo test -p pa-daemon --test saved_session_wake_e2e | 3 passed (13.2 s) |
| cargo test -p pa-perf corpus | 13 passed |
| cargo clippy -p pa-core -p pa-perf -p pa-daemon --all-targets -- -D warnings | EXIT 0, 0 warnings |
| cargo test -p pa-core --lib refinement::planner::tests::create_refuses_an_id_held_by_a_preserved_row | passed |

## Red and mutation controls (scratch only, all reverted; `git status` clean at d7eaa6a)
- P1-1: with session/mod.rs put back to 0c6a74564, `a_legacy_v1_compaction_keeps_its_summary_and_kept_turns` FAILS
  (the other 10 tests pass). At the tip, my own independent scratch red test from round 1, `red_v1_compaction_row_keeps_ts_context`, PASSES.
- P2-1: with the `|| preserved` guard removed in planner.rs, `create_refuses_an_id_held_by_a_preserved_row` FAILS. At the tip,
  my scratch `red_create_over_preserved_row_is_refused_like_ts` PASSES.
- P2-3: with the `!records.contains_key(id)` guard removed in save_harness_state, `a_typed_entry_wins_over_a_preserved_row_with_the_same_id`
  FAILS. Round 1 had no test that caught this mutation. Now one does.
- Expected and disclosed: my scratch `red_delete_of_preserved_row_works_like_ts` still fails at the tip. The README "Known limits" section documents
  this (update and delete on a preserved row answer "entry not found").

## Code review of the delta
- **9f733e83c v1 migration**: this matches TS migrateV1ToV2 (703367bad session-manager.ts:342-367). Every non-header row gets an id and a
  parentId, including `Unknown` rows, which now get `id`/`parentId` in their raw `rest`. The chain is unbroken. A top-level
  `firstKeptEntryIndex` on a compaction row resolves against the same entries vector (header included, malformed lines skipped in both). A header target
  is refused (TS `targetEntry.type !== "session"`). The index is removed and the row is parsed again into a typed Compaction. The wrong `details.firstKeptEntryIndex` read is gone.
  The test also reopens the rewritten file and gets the same context, so the permanent-loss path is covered.
  The fixture matches the writer at c58d5f20a^ (header :262-267, compaction :42-48).
  P3 (corrupt input only, not a writer shape): if the index is out of range, points at the header, or is not a u64, TS keeps a compaction
  with `firstKeptEntryId` undefined and deletes the index. Rust keeps the row `Unknown` with id and parentId (the chain stays intact, and the summary is not
  in context). Also, TS resolves the index inside the loop, so a forward index gets undefined, while Rust resolves it after the loop and gets the later id.
  Neither case comes from a real v1 writer.
- **580997790 create refusal**: `create` now answers "entry already exists" when the id is held by a typed entry OR a preserved row
  (TS refinement.ts:1111). `before` stays None, the preserved row is untouched, and the unit test checks whole-value equality of the row.
- **4b4fadfcf**: pins the precedence on save. It is mutation-verified (above).
- **f56020d1a**: inlines `normalize_stored_entry` (now at the load site, behavior unchanged; the topic tests still pass) and `entry_ids`. P2-7 is resolved.
- **d7eaa6a42 docs**: the HarnessState field doc and the README "Known limits" section now state P2-2 (non-object rows kept, untyped
  refinements and extra fields dropped), the rest of P2-1 (update and delete answer "entry not found"), and P2-4 (the Python writer is not lossless).
  This matches what I observed.

## Status of round-1 findings
- P1-1: FIXED (red-verified).
- P2-1: create part FIXED (red-verified). The update/delete part is DISCLOSED as a known limit.
- P2-2: DISCLOSED.
- P2-3: FIXED (mutation-verified).
- P2-4: DISCLOSED.
- P2-5 (`Unknown` writes a duplicate `type` key on rewrite): still open. It existed before the branch and is not part of this branch. It no longer hits v1 compaction rows, because they now parse to a typed row.
- P2-6: the notes stand (fixtures not bent; no `git` header; synthetic gemini row).
- P2-7: FIXED.
- Ledger notes from round 1 are unchanged: 6470a60e7 is still not evidenced by this branch, and 8b5ec62f8 still holds by absence (no production caller of `mark_auth_stale`) with no 401 regression test.
