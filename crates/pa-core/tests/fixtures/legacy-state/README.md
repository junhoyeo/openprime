# Legacy state fixtures

Synthetic or copied fixtures for `tests/legacy_state_compat.rs` and the auth test in
`src/auth/manager/tests.rs`. Every shape comes from a legacy TypeScript writer or test at
legacy develop `703367badf5c84e48e8c8cd986fbcda882b6378c` (read with `git show 703367bad:<path>`).
No file is copied from a real user directory. Every credential value is synthetic.
Paths below are relative to the legacy tree.

| Fixture | Source of the shape |
| --- | --- |
| `sessions/v3-compacted-branch.jsonl` | `packages/coding-agent/src/core/session-manager.ts`: header `SessionHeader` (:75) as written by `newSession` (:1863), `SessionMessageEntry` (:101), `ThinkingLevelChangeEntry` (:108), `ModelChangeEntry` (:118), `CompactionEntry` (:124), `CustomEntry` (:148), `LabelEntry` (:167), `buildSessionInfoEntry` (:820). The `custom` row is the `/btw` pointer from `src/core/side-question-store.ts:151`. Expected context follows `buildSessionContext` (:444). |
| `sessions/v1-linear.jsonl` | v1 rows have no `id`/`parentId` and no header `version`; the header carries `provider`/`modelId`/`thinkingLevel` like `test/fixtures/large-session.jsonl:1`. Migration: `migrateV1ToV2` (`session-manager.ts:342`). Text is synthetic. |
| `sessions/v2-hook-message.jsonl` | v2 header (`version: 2`) with a `hookMessage` role row. Migration: `migrateV2ToV3` (`session-manager.ts:369`) renames the role to `custom`. |
| `auth.json` | `src/core/auth-storage.ts`: `ApiKeyCredential` (:34, `primeTeam` may be `null`, :37), `OAuthCredential` (:40, with the open index signature of `packages/ai/src/utils/oauth/types.ts:3`), `McpStaticTokenCredential` (:54). Serialized like the writer, `JSON.stringify(data, null, 2)` (:313). |
| `harness_state.python-writer.json` | `prime-agent-runtime/src/rlm/harness.py`: `HarnessEntry` (:170) and `RefinementEvent` (:189) via `asdict`, saved by `HarnessState.save` (:467) with `json.dump(data, f, indent=2, ensure_ascii=False)` (:492). Four legacy kinds, no `factory`. |
| `harness_state.topic-era.json` | `topic_entry` is the exact row of `test/refinement.test.ts:739-766` ("topic-era harness state migration"), loaded by `loadHarnessState` (`src/core/refinement/refinement.ts:289`, `storedHarnessPath` :330). `hand_edited` is a hand-edited row with wrong field types; the TS loader keeps it through its object spread (:289). |
| `skills/python-skill/**`, `skills/python-package-missing/**`, `skills/disable-model-invocation/SKILL.md` | Byte copies of `packages/coding-agent/test/fixtures/skills/<same path>`, used by `test/skills.test.ts:102-133`. |
