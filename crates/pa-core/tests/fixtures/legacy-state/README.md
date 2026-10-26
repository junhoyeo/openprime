# Legacy state fixtures

Synthetic or copied fixtures for `tests/legacy_state_compat.rs`. Every shape comes from a legacy
TypeScript writer or test at legacy develop `703367badf5c84e48e8c8cd986fbcda882b6378c`
(read with `git show 703367bad:<path>`). No file is copied from a real user directory.
Paths below are relative to the legacy tree.

| Fixture | Source of the shape |
| --- | --- |
| `sessions/v2-hook-message.jsonl` | v2 header (`version: 2`) with a `hookMessage` role row. Migration: `migrateV2ToV3` (`session-manager.ts:369`) renames the role to `custom`. |

## Red-first evidence

Each fixed incompatibility was proven red before its fix, with
`cargo test -p pa-core --test legacy_state_compat` run under an isolated HOME:

| Test | Failed on base | Fixed by |
| --- | --- | --- |
| `a_legacy_v2_hook_message_migrates_to_a_custom_message` | `04f9fe117` and `11a949bb0` | `fix(session): migrate v2 hookMessage rows to custom messages` |
