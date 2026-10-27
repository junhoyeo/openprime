# Skills lane evidence snapshot

> Integrated source tip: `e02201fee` on `feat/rust-migration`, cherry-picked from reviewed `origin/port/rust-skills@e2ad4ac1b214d60de69fb6abf33c59827d7811db`.
> The **Final focused verification**, **Disclosed non-green evidence**, and **User-visible prompt correction** sections are authoritative where earlier chronological notes describe intermediate states. Final package/native checks must be repeated after kernel integration.

# Openprime Rust skills migration evidence

Target: `port/rust-skills` from pinned upstream Rust `c4c238ca025e382d96fb5f37e6328ca77fb82ef3`.
Source: old fork `develop` at `703367badf5c84e48e8c8cd986fbcda882b6378c` plus PR #33 fixes through `e4f7718aefdf0dd9334e8d744f58966f8b3d6565`.

## Fork commit ledger

| Fork commit | Behavior carried into Rust canonical root skills |
|---|---|
| `6c4da7150f7cc484966bdf1be673df66920867b8` | Base `agent-sessions` Python skill: Codex, Claude Code, Kimi/Kimi Code, Kiro, and Prime transcript discovery/parsing/rendering. Seven current tracked files are now under `skills/agent-sessions/`; TS builtin registration maps to the Rust native loader guard. |
| `ee7c91d058b23d289212a70953028cbd7972cc1a` | Senpi transcript glob and Prime-shape parsing, including filename/session metadata behavior. |
| `8453b5e46d927dbf1837f8cb55416a287d9fda51` | Mandatory-literal grep prefilter, complete match counts, Kiro inline-byte-array elision, and regression tests. |
| `3307acb13ea32f64b8c65a5c37892793830c5337` | Grok Build `updates.jsonl` parser, summary metadata, tool duration/error semantics, documentation, and `test_grok.py`. |
| `4af3c6d897fcbcbcddd74866433d91c78927ae4a` | Aside chats, text artifacts, read-only `state.db` metadata, artifact boundary/symlink/binary guards, scoped grep, documentation, and synthetic fixtures. |
| `5d4d1eb26f0e63ed8946bad39746412d00f6acd0` | Unicode smart-case fix: case-insensitive non-ASCII patterns bypass the ASCII-only byte prefilter; regressions cover JSON-escaped chat and raw artifact `École` matched by lowercase `école`, including fixed mode. PR #33 review thread was replied to with this SHA and resolved; PR remains open. |
| `d2b5c9569dac87ed79e9a0c0cf30df1e91e4f6e0` | Base `aside` Python skill, deterministic REPL/read/screenshot/tab helpers, bounded agent runner, health/config helpers, and reference docs under `skills/aside/`. No console script is created, avoiding collision with the real Aside CLI. |
| `3a537a23f2750ad221c21b136eab8455db4b3705` | Selected-account `models.json` resolution (`u1`/`1`), provider/model helpers, docs, and isolated config regression. |
| `2151c02717ea54bbdd085e7abf85d7b45ef2220b` | Runtime CI discovers both bundled skill unittest suites; path adapted from TS `packages/coding-agent/skills` to Rust root `skills`. |

## Canonical loading and packaging

- Exactly one product source path per skill: `skills/agent-sessions` and `skills/aside`.
- No global/user skill or symlink was created, modified, or removed. The legacy live symlink remains untouched.
- Rust `load_skills_from_dir` inventory is pinned to the openprime set. It asserts each skill appears once, has the expected import (`agent_sessions`, `aside`), and resolves package/pyproject paths under the canonical root.
- Static core prompt bullets enumerate the callable modules and every public helper detected by the strict prompt/tool-surface guard; the detailed SKILL.md remains on-demand.
- Release packaging already copies the root `skills/` tree and excludes `.venv`, bytecode, and caches. Focused packaged-layout evidence is recorded below.

## Verification

Completed:

- `uv run --no-project python -m unittest discover -s skills/agent-sessions/tests` — 25 passed, exit 0.
- `uv run --no-project python -m unittest discover -s skills/aside/tests` — 1 passed, exit 0.
- `cd prime-agent-runtime && uv run --locked python -m unittest test.test_bundled_skill_suites` — 26 passed, exit 0 (Python 3.14).
- `cd prime-agent-runtime && env -u PRIME_AGENT_BASH_SHELL uv run --python 3.11 --locked python -m unittest discover -s test` — 771 passed, exit 0. The first run inherited the agent harness's `PRIME_AGENT_BASH_SHELL=/bin/bash` and therefore failed only the test that requires that variable absent (770 passed, 1 failed); the isolated single test and full clean-env rerun passed.
- `uv build skills/agent-sessions --out-dir target/skill-wheels/agent-sessions` — sdist + wheel, exit 0.
- `uv build skills/aside --out-dir target/skill-wheels/aside` — sdist + wheel, exit 0.
- Both wheels installed into an isolated Python 3.11 venv and imported from site-packages, exit 0. An initial ad-hoc assertion incorrectly expected the module name `aside` inside `aside.__all__` and exited 1; the corrected public-API assertion (`run`) passed.
- Installed-wheel smoke over synthetic `~/.aside/u/0/sessions/...`: JSON-escaped chat `École` and raw artifact `École` both matched lowercase `école`; selected-account Aside config returned only synthetic `u1` model/provider, exit 0. No real chat/auth/config data was read.
- `cargo fmt --all --check` — exit 0 after applying rustfmt's one-line array shape.
- `CARGO_BUILD_JOBS=2 cargo test -p pa-core --test prompt_guards bundled_skills_match_the_openprime_ts_packaged_set -- --exact` — 1 passed, exit 0.
- `CARGO_BUILD_JOBS=2 cargo clippy -p pa-core --test prompt_guards -- -D warnings` — exit 0.
- `CARGO_BUILD_JOBS=2 cargo test -p pa-core --test prompt_guards` — 7 passed, exit 0 after adding the required core prompt surface; a deliberate pre-doc run failed on missing `agent_sessions`, proving the strict guard detects stale docs.

Additional packaging evidence:

- `python3 scripts/package_release.py --root . --binary target/debug/prime-agent ...` produced `target/skill-port-package/prime-agent-0.10.1-darwin-arm64` plus tarball/manifest/checksums, exit 0. All 12 ported skill source/reference/test files were byte-identical in stage and tar; no `.venv`, `__pycache__`, or `uv.lock`; `SHA256SUMS` passed.
- Unchanged ignored verifier `CARGO_BUILD_JOBS=2 cargo test -p pa-cli --test packaged_layout_e2e bootstrap_kernel_venv_from_packaged_sidecar -- --ignored --exact` — 1 passed with a real receipt, exit 0.
- The nonignored `packaged_session_boots_kernel_without_pi_package_dir` remains red locally and is disclosed, not counted green. It reproduces on a pristine archive of pinned `c4c238ca0` with the port absent. Structured frames prove tool dispatch starts but kernel validation rejects the selected `PRIME_AGENT_KERNEL_PYTHON` before Python execution: the isolated runtime-only venv lacks default packages; the live legacy venv lacks current Rust runtime callables. Logs are preserved under `target/package-cell-failing-*` and the pristine full log at `target/pinned-c4c238ca0-package-smoke.log`. The integration lead owns supplying a valid current interpreter and CI execution for this fixture.
- A real cell executed from the actual `package_release.py` stage after isolated bootstrap with explicit faux provider/model and owned socket. `target/package-cell-smoke/tool-receipt-lead-shape.txt` contains `tool-ok`; the redacted terminal frame is `status=ok`, `isError=false`.
- Two more real staged cells proved both imports resolve from staged `skills/*/src` and then searched only synthetic Aside chat/artifact `École` with lowercase `école` plus synthetic selected-account config. Receipts/frames are under `target/package-cell-smoke/`.
- Generic loader precedence tests each executed one real test and passed: `user_skill_entries_dedupe_when_agent_skills_symlinks_agents_skills` and `loads_user_and_project_skills_with_collision`.

Implementation commits (all pushed to `origin/port/rust-skills`):

- `b704c720d` — `feat(skills): port agent-sessions and Aside`
- `5ac85be96` — `docs(skills): clarify synchronous helper calls`
- `de0de6fe9` — `fix(skills): harden dynamic contracts and local probes`
- `75f27db46` — `fix(skills): close Unicode and health probe gaps`
- `f48f3fbd8` — `fix(aside): reject ambiguous health origins`
- `4e386afdb` — `fix(aside): report TLS setup failures`
- `e2ad4ac1b` — `fix(aside): require custom provider origins`

Final tested/pushed tip: `e2ad4ac1b214d60de69fb6abf33c59827d7811db`.

## Final focused verification

- `PYTHONWARNINGS=error uv run --no-project python -m unittest discover -s skills/agent-sessions/tests` — 25 passed, exit 0.
- `PYTHONWARNINGS=error uv run --no-project python -m unittest discover -s skills/aside/tests` — 4 passed, exit 0.
- `cd prime-agent-runtime && env -u PRIME_AGENT_BASH_SHELL PYTHONWARNINGS=error uv run --python 3.11 --locked python -m unittest test.test_bundled_skill_suites` — 29 passed, exit 0.
- Same two skill suites from the final staged package source — 25 + 4 passed, exit 0.
- `ruff check skills/agent-sessions skills/aside prime-agent-runtime/test/test_bundled_skill_suites.py` — exit 0.
- `cargo fmt --all --check` — exit 0.
- `CARGO_BUILD_JOBS=2 cargo test -p pa-core --test prompt_guards` — 10 passed, exit 0. Includes default bundle, disabled/force-excluded, markdown winner, different-Python-import winner, no static API leakage, and exact dynamic winner/import once.
- `CARGO_BUILD_JOBS=2 cargo clippy -p pa-core --test prompt_guards -- -D warnings` — exit 0.
- `PA_CORE_KERNEL_PYTHON=<final isolated package venv> CARGO_BUILD_JOBS=2 cargo test -p pa-core --test kernel_python_skills_unavailable` — 2 passed, exit 0; unavailable imports stay honest.
- Final wheels: both `uv build` commands produced sdist + wheel, exit 0.
- `scripts/package_release.py` final stage: `/Users/junhoyeo/prime-agent-worktrees/rust-port-skills/target/skill-port-package-e2ad4/prime-agent-0.10.1-darwin-arm64`; all 12 ported source/reference/test files byte-identical, tar contains each, no `.venv`/`__pycache__`/`uv.lock`, SHA256SUMS passes.
- Final staged artifact bootstrapped its own current isolated kernel venv and executed a real faux-provider `python_repl` cell. Receipt `/Users/junhoyeo/prime-agent-worktrees/rust-port-skills/target/package-cell-e2ad4/receipt.json` resolves both imports from the staged skill paths and reports `ok: true`; redacted structured frames are `/Users/junhoyeo/prime-agent-worktrees/rust-port-skills/target/package-cell-e2ad4/logs/skill-frames-redacted.jsonl` with `tool_execution_end status=ok/isError=false`. The cell proves lowercase/uppercase/fixed Unicode matches including K/ſ/İ/ı, rejects case-sensitive false matches, honors selected-account config, drops missing-endpoint models, and rejects ambiguous origins.
- Terra final read-only verdict for the full range: **CLEAR — no remaining ship blocker or P2 found**.

## Disclosed non-green evidence

- The unchanged nonignored `packaged_session_boots_kernel_without_pi_package_dir` test remains red in this local harness because its selected `PRIME_AGENT_KERNEL_PYTHON` is invalid. It reproduces on a pristine `git archive` of pinned `c4c238ca0` with these skills absent. Redacted frames prove dispatch occurs but kernel validation rejects the interpreter before Python execution. The integration lead owns providing a fully valid current interpreter and making CI execute the receipt gate. This failure is not counted green.
- A full runtime suite passed 771/771 once under isolated environment. A later concurrent exact-tip attempt failed two unrelated runtime tests (lock-directory subprocess race and 565 ms vs 500 ms startup threshold). That attempt is disclosed and not called green; the owned 29-test bundled-skill suite is green at final tip.
- An attempted `verify_release.py` call against `package_release.py` output failed because that verifier expects the separate assembled-release `install.sh` shape. `package_release.py` itself completed and its tar/checksum/layout were verified; the mismatched verifier invocation is not counted green.
- Early ad-hoc verification assertions used a wrong `aside.__all__` expectation and a wrong tar root prefix; corrected checks passed. These harness mistakes did not indicate product failures.

## User-visible prompt correction

The cache-stable prompt no longer hard-codes conditional `agent_sessions`/`aside` APIs. It now says bundled, user, and project skills resolve by precedence; the dynamic tail's single winner and winning `SKILL.md` are authoritative; only a successfully prepared Python winner is pre-imported; unavailable imports are reported; and shell commands exist only when the winning skill documents one. Static host/API coverage remains strict, while dynamic-winner regressions prevent disabled or overridden built-ins from leaking bundled docs.

```diff
diff --git a/crates/pa-core/src/prompts/layers/core.md b/crates/pa-core/src/prompts/layers/core.md
index 68ac9d2a6..4e9665d57 100644
--- a/crates/pa-core/src/prompts/layers/core.md
+++ b/crates/pa-core/src/prompts/layers/core.md
@@ -172,6 +172,8 @@ prime-agent has support for programmatic tools that are defined in the MCP forma

 ## Skills

-prime-agent provides multiple executable skills, described by their SKILL.md and executable code. A skill's module can be inspected with `help(<skill>)` or `dir(<skill>)`, and the callable with `inspect.signature(<skill>.<function>)`, if information is missing. The executable code is always pre-imported in the REPL as a programmatic tool, and a callable skill module can be called directly (`await <skill>(...)`).
+prime-agent resolves bundled, user, and project skills by precedence and lists the winning skill for each name in the dynamic tail of this prompt. That winning `SKILL.md` is the API and safety contract; never infer an API from a shadowed built-in with the same name.

-All programmatic tools described above except for `bash`, `rlm.*`, and `mcp.*` are implemented as executable skills and will be listed again in the dynamic tail of this prompt. Additional executable and non-executable skills may exist as well; non-executable (markdown) skills are documentation read from disk, and each skill is also available as a shell command by the same name.
+When a winning skill is Python-backed and its runtime is available, its listed Python import is pre-imported in the REPL as a programmatic tool. Inspect it with `help(<skill>)`, `dir(<skill>)`, or `inspect.signature(<skill>.<function>)`; a callable module can be invoked directly (`await <skill>(...)`). If preparation fails, the session reports the unavailable import instead of claiming it works. Markdown skills provide documentation only. A Python skill has a shell command only when its winning `SKILL.md` documents one.
+
+All genuine static programmatic tools described above except for `bash`, `rlm.*`, and `mcp.*` are implemented as executable skills and appear again when their matching winner is resolved in the dynamic tail. Additional executable and markdown skills may exist only in that resolved inventory.
```

## Review mapping

- PR #33 Unicode thread: replied with `5d4d1eb26` and resolved; PR remains open for migration finality.
- Terra finding 1 (false static API): fixed by `de0de6fe9` with red-first disabled/override tests.
- Terra finding 2 (Unicode prefilter): fixed by `de0de6fe9` + `75f27db46`, including raw/escaped uppercase, regex escapes, fixed mode, exact-case negatives, and Python I/K/S Unicode folds.
- Terra finding 3 (config errors): fixed by `de0de6fe9`, `75f27db46`, and `e2ad4ac1b` with bounded regular reads, missing/unreadable/FIFO/oversize/deep/malformed shape handling, exact required origins, and honest health status.
- Terra finding 4 (health probes): fixed by `de0de6fe9`, `75f27db46`, `f48f3fbd8`, and `4e386afdb`: exact loopback origin validation, no proxy/redirect layer, bounded manual HTTP parsing, capped concurrent cancellable probes under one deadline, generic secret-safe errors, and TLS setup regression.


## Privacy scrub (final review F7)

`final-review-opus` found owner-specific examples in the bundled `agent-sessions` skill: a real Aside session id, a personal search term, an artifact directory name in `SKILL.md`, and an owner path in a `sessions.py` docstring. `c3c74ae53` replaces them with synthetic placeholders. It is a text-only change: the only code-file edit is one docstring line. `skills/agent-sessions/tests` ran 25 tests, OK, under the egress sandbox.

The original strings remain in git history in `4af3c6d89` (`feat/agent-sessions-aside`, the legacy PR #33 branch), `b704c720d` (`port/rust-skills`), and `d46fe9be5` (`feat/rust-migration` and its lane branches). They are not in the trees at `703367bad`, `e260085dd`, or the pre-merge `main`. History is not rewritten. The owner decides on branch deletion after the merge.
