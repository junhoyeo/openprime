# Legacy state fixtures

Synthetic or copied fixtures for `tests/legacy_state_compat.rs`. Every shape comes from a legacy
TypeScript writer or test at legacy develop `703367badf5c84e48e8c8cd986fbcda882b6378c`
(read with `git show 703367bad:<path>`). No file is copied from a real user directory.
Paths below are relative to the legacy tree.

| Fixture | Source of the shape |
| --- | --- |
| `sessions/v2-hook-message.jsonl` | v2 header (`version: 2`) with a `hookMessage` role row. Migration: `migrateV2ToV3` (`session-manager.ts:369`) renames the role to `custom`. |
| `harness_state.python-writer.json` | `prime-agent-runtime/src/rlm/harness.py`: `HarnessEntry` (:170) and `RefinementEvent` (:189) via `asdict`, saved by `HarnessState.save` (:467) with `json.dump(data, f, indent=2, ensure_ascii=False)` (:492). Four legacy kinds, no `factory`. |
| `harness_state.topic-era.json` | `topic_entry` is the exact row of `test/refinement.test.ts:739-766` ("topic-era harness state migration"), loaded by `loadHarnessState` (`src/core/refinement/refinement.ts:289`, `storedHarnessPath` :330). `hand_edited` is a hand-edited row with wrong field types; the TS loader keeps it through its object spread (:289). |

## Red-first evidence

Each fixed incompatibility was proven red before its fix, with
`cargo test -p pa-core --test legacy_state_compat` run under an isolated HOME:

| Test | Failed on base | Fixed by |
| --- | --- | --- |
| `a_legacy_v2_hook_message_migrates_to_a_custom_message` | `04f9fe117` and `11a949bb0` | `fix(session): migrate v2 hookMessage rows to custom messages` |
| `a_topic_era_harness_entry_loads_under_path` | `04f9fe117` and `11a949bb0` | `fix(refinement): keep legacy harness entries through load and save` |
| `saving_after_a_legacy_load_never_erases_an_entry` | `04f9fe117` and `11a949bb0` | `fix(refinement): keep legacy harness entries through load and save` |
