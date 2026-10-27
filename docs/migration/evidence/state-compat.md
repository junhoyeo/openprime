# Legacy state compatibility evidence

Status: **integrated; independently CLEAR (round 2); final combined acceptance pending**.

- Lane: `port/rust-state-compat-v3` at `d7eaa6a423136240265eaf826c44b2bad1d898ca` (base `11a949bb0`), owner `state-compat-opus`. Round 1 (`port/rust-state-compat-v2` @ `0c6a74564`) was BLOCKED on P1-1 and is superseded, not force-pushed.
- Review: `state-compat-review` (read-only, independent). Round 1 BLOCK, round 2 CLEAR for `d7eaa6a42`. The full review is in [`state-compat-review.md`](state-compat-review.md).
- Integrated on `feat/rust-migration` as `5bfaf19b4`..`172ce1942` (10 commits, same order).
- Fixture sources: `crates/pa-core/tests/fixtures/legacy-state/README.md` and `crates/pa-daemon/tests/fixtures/legacy-state/README.md` cite the legacy TS writers (`703367bad`, `c58d5f20a^`). All values are synthetic. The reviewer confirmed the fixtures match the legacy writer shapes.

## Proven behaviors

| Area | Test | Result |
| --- | --- | --- |
| Legacy v3 session context (compaction summary, kept tail, abandoned branch, /btw pointer excluded) | `pa-core tests/legacy_state_compat.rs::a_legacy_v3_session_rebuilds_the_typescript_context` | pass |
| v1 linear migration | `a_legacy_v1_session_migrates_to_a_linear_context` | pass |
| v1 compaction row keeps its summary and kept turns (product fix `0b8b00739`, red-first) | `a_legacy_v1_compaction_keeps_its_summary_and_kept_turns` | pass; red on base |
| v2 hookMessage migration (product fix `5bfaf19b4`, red-first) | `a_legacy_v2_hook_message_migrates_to_a_custom_message` | pass; red on base |
| Legacy harness entries load, including topic-era rows, and survive a Rust save (product fix `bc94930ab`, red-first) | `a_legacy_kernel_written_harness_state_loads_every_entry`, `a_topic_era_harness_entry_loads_under_path`, `saving_after_a_legacy_load_never_erases_an_entry`, `a_typed_entry_wins_over_a_preserved_row_with_the_same_id`, `create_refuses_an_id_held_by_a_preserved_row` | pass; red on base |
| Legacy Python skills, missing package, manual-only skill | `a_legacy_python_skill_*`, `a_legacy_manual_only_skill_keeps_disable_model_invocation` | pass |
| TS-written /btw side conversation: history, parent linkage, full spend 1050/85/40/1175 tokens, $0.004425 | `pa-daemon side_question_store.rs::a_ts_written_side_conversation_opens_with_its_history_and_full_spend` | pass |
| TS /btw resume into the same transcript (legacy bytes stay a prefix; one pointer) | `a_ts_written_side_conversation_resumes_into_the_same_transcript` | pass |
| TS-archived session reopens by name | `saved_session_wake_e2e::send_to_a_ts_archived_session_reopens_it_with_its_history` | pass |
| Legacy auth.json loads losslessly and survives a Rust write of another provider | `auth::manager::tests::a_legacy_auth_document_resolves_and_survives_a_rust_write` | pass |

Live-data impact (parent, read-only scan): all 412 top-level and 3,765 child transcripts are v3. The live harness files hold 0 topic-only rows. The v1, v2, and harness fixes are defensive legacy-compatibility fixes, not live-data blockers.

## Gates at the integrated tip

Hermetic wrapper, `RUST_TEST_THREADS=4`: fmt 0; clippy `pa-core`/`pa-perf`/`pa-daemon` `-D warnings` 0; `legacy_state_compat` 11/11; `pa-core --lib refinement:: session:: auth::` 202/202; `pa-daemon --lib side_question` 14/14; `saved_session_wake_e2e` 3/3; `pa-perf --lib corpus` 13/13.

## Disclosed, not fixed

- P2-1 rest: a refine update or delete of a preserved-only harness row answers "entry not found". TS acts on the row.
- P2-2: non-object harness records are kept; untyped `refinements[]` events and unknown fields on typed rows are dropped on save.
- P2-4: the kernel Python harness writer (`harness.py`, the same as legacy) still drops preserved rows and `topic` on its own writes. Only the Rust writer is lossless.
- P2-5 (upstream `pa-types`): `FileEntry::Unknown` writes a duplicate `type` key on rewrite.
- Open gaps: `pa-daemon` `SessionFile::open` does not migrate v1/v2 itself (proven through `pa-core` `SessionManager::open`). An OAuth refresh of the same provider may drop unknown extra fields. A `session_state` "crash" reopen is untested. Agents-view rendering is not diffed against the TS binary.
- `6470a60e7` is evidenced by the revival lane, not here. `8b5ec62f8` is proven for load/write identity only.
