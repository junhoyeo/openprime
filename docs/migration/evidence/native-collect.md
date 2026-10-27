# Combined native collect evidence

Status: **core behavior passed; harness exited 1 after collection; parent acceptance pending**.

- Source: `56946f04a5b81683232d680f6feb2d9b5905f2d2`.
- Release binary SHA-256: `36a931c7495e4644833fd6da216f27ee35150491235bcdadd9d4ce8ba845f505`.
- Fresh owned root: `/tmp/pa-0b5d31e1`; read-only preflight evidence SHA-256 `8c594eb3585db1d9986a9ac389d2e664d690d721ef7e219d7035a0bc87b9a5dc`.
- Every resolved write path was under the owned root. The longest worker socket was 72 bytes and passed the native AF_UNIX check. Configs were synthetic empty JSON. The v5 ABI2 runtime was the only read-only exception.
- Faux-only parent/child. No provider credentials, endpoints, proxies, updater, or external model call.

## Behavior result

Inside the parent Python tool turn:

- `rlm.spawn(...)`: 0.382 s.
- `rlm.collect(..., timeout_ms=10000)`: 0.782 s.
- Result: `status=done`, `settled=true`, full answer `native child terminal answer`.
- The parent then completed its normal assistant boundary with `parent boundary complete`.
- Preserved child JSONL contains exactly one `spawn:sub-69a33f92` task row, proving no duplicate kickoff after the boundary.

## Harness failure and cleanup

The standalone verifier exited 1 after all behavior above because it assumed the child JSONL would live in the user sessions root. The product correctly stored it under the child `session_dir` artifact. Per the one-run authorization, the case was not rerun. Post-run inspection used only the preserved owned root.

The full session is **not green** for a second independent harness defect: after `parent boundary complete`, the no-reply child terminal notice intentionally triggered another model turn, but the parent faux script had no response reserved for that follow-up. The raw final assistant row therefore has `stopReason=error` and `errorMessage="No more faux responses queued"`. This does not invalidate the already-observed in-turn collect/one-kickoff behavior, which parent accepted narrowly, but it prevents any whole-session success claim. A corrected later harness must address the child terminal notice explicitly and assert every intended follow-up is error-free.

Graceful protocol shutdown succeeded. Exact owned PIDs (daemon `8138`, parent worker `8639`, kernel `8974`, child worker `8982`) were all gone; root-scoped `lsof` and `ps` checks were empty. No global cleanup or live product state was touched.

Full preserved summary: `/tmp/pa-0b5d31e1/case-summary-posthoc.json`, SHA-256 `2805917c40aa4e5a90baadb0b0addf9fddb47f156387f2aa03e10f57e5fafaf5`. The isolation incident records for earlier non-accepted worker attempts remain separate and are not superseded by this run.
