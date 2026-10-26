# Legacy state fixtures (daemon)

Synthetic fixtures for the `/btw` legacy-transcript tests in `src/side_question_store.rs`.
Every shape comes from a legacy TypeScript writer at legacy develop `703367badf5c84e48e8c8cd986fbcda882b6378c`
(read with `git show 703367bad:<path>`). No file is copied from a real user directory.
Placeholders (`{{PARENT_FILE}}`, `{{SIDE_FILE}}`) are filled with temp paths by the tests.
Paths below are relative to `packages/coding-agent/` in the legacy tree.

| Fixture | Source of the shape |
| --- | --- |
| `btw/parent.jsonl` | Parent session rows from `src/core/session-manager.ts` (header :75/:1863, messages :101). The last row is the immutable pointer: `SideQuestionPointer` (`src/core/side-question-store.ts:46-57`) appended by `appendCustomEntryWithRollback` (:151), whose row layout is `appendCustomEntry` (`session-manager.ts:2159`). |
| `btw/side.jsonl` | Side transcript under `session-artifacts/<parent>/btw-<paneId>/` (`side-question-store.ts:41`, :128). Header `newSession({ parentSession, rlmDepth: 0 })` (:137), `appendModelChange` (:138), the user row (:173), auxiliary completions written before the kept answer (:183, from `src/core/side-question.ts` compaction summary and replaced overflow answer, commit 6567e228f), the kept answer (:185), and a failed turn that still billed followed by its `side_question_status` row (:198, `side-question.ts:627-630`). |
