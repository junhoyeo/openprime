# Review: origin/fix/kernel-request-regression @ 9da267261 (base feat/rust-migration 473294ae5, upstream c4c238ca0)

Reviewer: kernel-review-opus (read-only). Scratch worktree ~/prime-agent-worktrees/review-kernel-fix (detached; restored clean to 9da267261 after the work).
Private target ~/prime-agent-worktrees/targets/review-kernel-fix (APFS clone of targets/kernel-fix). Logs: /tmp/review-kernel-fix/*.log.
Every cargo command ran through:
`/usr/bin/python3 /tmp/mig-lead-opus/hermetic_test.py --kernel-python /private/tmp/openprime-kernel-final-env-v5/abi2-venv/bin/python -- timeout 900 <cmd>`
with CARGO_TARGET_DIR=<private>, CARGO_BUILD_JOBS=2, CARGO_INCREMENTAL=0, RUST_TEST_THREADS=4. Disk: 47GiB free at start, 43GiB at end (floor 32GiB).
The wrapper reported "no surviving processes" after every run. A manual ps check also found no survivors. I killed no PIDs.

## VERDICT: CLEAR for 9da267261, with P2 follow-ups

No P0 or P1 issue. The fix removes the root cause and all gates pass. Both mutations are caught. One race (P2-a) is widened by the new await and gets a 3-line fix. I recommend that fix before merge, but it does not block.

## 1. Root cause: confirmed (red on 473294ae5)

- `cargo test -p pa-core --lib cancelled_request_write_retires_the_kernel` (timeout 240) -> "has been running for over 60 seconds", EXIT=124 (hang). Log: base-red1.log.
- `cargo test -p pa-core --lib nonreading_kernel_fails_bootstrap_without_parking_shutdown` -> FAILED at provisioner.rs:1909 "timed out bootstrap must not park during failed-boot cleanup: Elapsed(())". It took **90.82s** against its 9s `tokio::time::timeout`. 90.8s = 3 sync probes x the fake's 30s `time.sleep`. The timer could not fire while the sync `std::process::Command::status()` probes blocked the runtime. That matches the claimed mechanism exactly. Log: base-red2.log.
- At 9da267261, both tests pass (lib.log).

## 2. Fix correctness

- **Fail-closed bound** (probe.rs:57-65): a timeout returns `Ok(false)`, a wait error returns false, and a non-zero exit returns false. Only a launch failure returns `Err`, which maps to the spawn-failure message. Correct.
- **Kill and reap**: on timeout, `child.kill().await` sends SIGKILL and waits, so the child is reaped. On drop, `kill_on_drop(true)` sends SIGKILL and tokio's orphan reaper collects it. The new test `wedged_spawn_probe_fails_closed_within_its_bound` checks that `ps` finds no such pid (no zombie). Limit: only the direct child is killed, not a process group. A wrapper interpreter that does not `exec` (a sh script that runs python as a child) leaves its grandchild running until that grandchild exits. Real `python -c` probes and pyenv shims exec, so the impact is low (P2-d).
- **Post-await recheck** (startup.rs:202-207): the success path is correct. `start_stale(generation)` returns "Kernel start superseded", and the Shutdown check returns "Kernel was disposed during startup". Both are FATAL markers in provisioner.rs:914-924, so neither is retried. **The failure path is not guarded. See P2-a.**
- **Fork guarantees intact**: the probe still uses `env_clear().envs(env)` with the same `kernel_spawn_env` map, and the same cwd, as the real spawn. Results at 9da267261:
  - kernel_bootstrap_generations (16/16 ok), including:
    - `direct_manager_python_is_abi_checked_before_spawn` ok
    - `direct_manager_probe_uses_the_exact_kernel_cwd` ok
    - `direct_manager_probe_uses_the_exact_kernel_environment` ok
    - `explicit_external_runtime_requires_the_subagent_row_abi_before_start` ok. This test loops over absent / int:1 / int:3 / str:2 / float:2.0 / bool:true / malformed, and every case must fail closed. It uses the `ensure_kernel_python` path, which this branch does not change.
  - `kernel::bootstrap::venv::probe::runtime_abi_tests::runtime_probe_requires_the_exact_integer_abi_two` ok.
  - `runtime_abi_tests::configured_packaged_runtime_satisfies_the_full_probe` (ignored live test, run with `--include-ignored` against the abi2 venv) ok. Log: live.log.
- **Cancellation safety at the await**: `do_start` runs inside the `tokio::spawn` in `ReplKernelManager::start` (mod.rs:540). Callers that abort only drop their wait, not the start. The task is dropped only at runtime shutdown. Then the probe is killed by kill_on_drop, and the state stays Starting in a dying runtime, as with the existing `resolve_python` await. No new hazard.

### P2-a: a stale start's probe failure clobbers a newer start (startup.rs:178-185)
The probe-failure branch runs `live_kernels::remove(self)` and sets `state = Idle` (if not Shutdown) without the `start_stale` guard that the `resolve_python` error branch has (startup.rs:146-150).
Sequence:
1. Start A is in its probe.
2. `restart()` runs shutdown. That bumps the generation, clears start_memo, and sets Idle.
3. Start B is Starting.
4. A's probe fails.
5. A resets the state to **Idle** and removes the shared Inner from live_kernels. B continues and spawns a kernel that is no longer in the live-kernel registry, so `shutdown_all_live_kernels` misses it at host exit.

Reproduced with a review-only test in the scratch tree (not committed; reverted):
`review_stale_probe_failure_clobbers_newer_start` -> `left: Idle, right: Starting` ("stale A must not reset newer B's state"). Log: demo.log.
Severity is P2: in this workspace, `ReplKernelManager::restart()` has no non-test caller, and the provisioner uses fresh managers. The race also existed at 473294ae5 on a multi-thread runtime (the sync probe ran between the generation check and this branch). The new await widens it to every runtime.
Fix: at the top of the `!matches!(probe, Ok(true))` branch, add `if self.start_stale(generation) { return Err(anyhow!("Kernel start superseded")); }`.

### P2-b: probes are not killed on kill()/dispose/supersession (startup.rs:170-176)
Parent condition (4) is only partly met. Nothing cancels the probe when the generation is bumped. After `kill()`, a wedged probe lives until its 30s bound. A superseded start also keeps running its remaining probes (up to 3 x 30s if each one succeeds slowly) before the post-probe recheck stops it.
Review-only test `review_probe_survives_kill` printed: `alive_after_kill=true start_task_finished=false alive_after_abort=true` (demo.log).
**There is no leak**: the child is bounded and reaped, and runtime drop kills it (the wrapper found no survivors).
Fix options:
- Check `start_stale(generation)` between the 3 probes.
- Or select the probe against a cancellation token that `cleanup_resources` fires.

### P2-c: a timed-out probe reports the ABI verdict
Timeout returns `Ok(false)`, so the error says "must provide exactly prime-agent-runtime ABI 2, dill, and required default Python imports". A slow cold start would then tell the user that the runtime is wrong. A distinct message is better, for example "kernel python did not answer the runtime probe within 30000ms". Both messages are retryable, so behaviour is unchanged.

### Bound vs a slow legitimate cold start (measured)
READY_TIMEOUT_MS = 30_000 (manager/mod.rs:42). This is the existing kernel ready-handshake budget (startup.rs:605). It is reused per probe; there is no new constant. The provisioner hard budget is DEFAULT_STARTUP_BUDGET_MS = 90_000 with DEFAULT_STARTUP_RETRIES = 1 (provisioner.rs:885-886). That budget is checked only between attempts.

Measured on an APFS clone of the abi2 venv with every `__pycache__` purged. The original venv was not touched. Machine: this Mac, while cargo clippy ran with 2 jobs. Script: /tmp/review-kernel-fix/time_probe.py; times are in seconds for the ready / dill / 12-module import probes.

| run | ready | dill | imports (pandas, numpy, scipy, ...) |
|---|---|---|---|
| copy, no pyc, PYTHONDONTWRITEBYTECODE=1, cold files (1st read) | 0.22 | 0.08 | **12.96** |
| copy, no pyc, nowrite, warm page cache | 0.16 | 0.07 | 1.31 |
| copy, first run that writes pyc | 0.13 | 0.07 | 1.55 |
| copy, warm pyc | 0.07 | 0.04 | 0.38 |
| original venv, nowrite | 0.09 | 0.07 | 1.56 |

The worst observed case is 13.3s total, which leaves 2.3x margin under the 30s per-probe bound. If a much slower disk did cross 30s, the error is retryable. The provisioner retries once after 250ms, and the retry runs with a warm cache, so it takes about 1.5s.
Risk of a false permanent failure: low.

### Shared deadline across the 3 probes (parent question)
Not needed; document it instead.
- A *hung* interpreter fails at the first probe after 30s, not 90s, because the loop stops at the first false.
- The 90s case needs probes 1 and 2 to each succeed in just under 30s and then probe 3 to hang. That is not a realistic interpreter.
- What the user sees for a hung custom interpreter:
  - Probe path: 30s, then 250ms backoff, then 30s again, so about 60s to the error.
  - c4c for a never-ready interpreter: 30s ready timeout x 2 attempts, also about 60s.
  - The two paths match.

A shared `timeout_at(deadline)` is a cheap nice-to-have, not a requirement.

## 3. Fixture commits 8c78e83de and 9da267261: legitimate, no assertion weakened
- 8c78e83de changes exactly three inline fake scripts:
  1. requests.rs "ready-but-not-reading": `bounded_execute_settles_when_kernel_stops_reading_stdin`
  2. requests.rs gated stalled-write fake: `cancelled_request_write_retires_the_kernel`
  3. provisioner.rs "ready-but-not-reading": `nonreading_kernel_fails_bootstrap_without_parking_shutdown`
- 9da267261 changes the fourth fake, `SILENT_BOOTSTRAP_RUNTIME` (`silent_bootstrap_fails_bounded_and_leaves_no_kernel`).
- Every change adds `if sys.argv[1:2] == ['-c']: sys.exit(0)` plus comments. There is no shared helper; each fake is still inline. No assert line changed (checked in `git diff 473294ae5 9da267261`).
- c4c semantics: c4c never launches the interpreter with `-c` from the manager, so answering `-c` cannot change any c4c-era behaviour. The 9da change returns the start count to c4c's 1 per boot (it was 4 = 3 probes + 1 at base).
- Fakes that must fail the probe (the direct_manager_* tests above) still fail closed.

## 4. 27f4362f4 message parity: exact
- c4c startup.rs spawn-error branch: `anyhow!("failed to spawn kernel python {}: {error}", python.display())`, the diagnostic `spawn error: {error}`, and `invalidate_runtime_probe_cache()`.
- 9da267261 startup.rs:187-193 has the identical format string, the identical diagnostic, and the identical cache invalidation. The ENOENT/EACCES io::Error comes from the same `Command` spawn, with the same cwd and env.
- `prewarm_swallows_failure_and_keeps_lazy_fallback`, `startup_failure_surfaces_cause_and_duration`, and `retryable_failure_classification` all pass.
- TS: there is no TS kernel manager source in this repo (rust-only tree), so I could not compare against TS.
- State divergence (P2-e, informational): c4c's spawn error sets `Shutdown` and runs `cleanup_resources`. The probe-launch error sets `Idle` (start can be retried). Provisioner behaviour is the same (retryable, 2 boots), and the tests pass. It is arguably better, but it is not strict parity.

## 5. Author gates at 9da267261
- `cargo fmt --all -- --check` -> FMT=0
- `cargo clippy -p pa-core --all-targets -- -D warnings` -> CLIPPY=0
- `cargo test -p pa-core --lib kernel:: -- --skip lock_timeout_prevents_venv_rebuild` -> 122 passed, 0 failed, 4 ignored
- `env -u PRIME_AGENT_KERNEL_VENV cargo test -p pa-core --lib lock_timeout_prevents_venv_rebuild` (inside the wrapper) -> 1 passed
- `cargo test -p pa-core --no-fail-fast --test kernel_*` (15 binaries):
  - All pass except the following, which are **pre-existing and not caused by this branch**:
    - `kernel_prewarm::prewarmed_kernel_lands_compaction_notice_without_tool_use` FAILED ("compact: Turn prefix summarization failed: No more faux responses queued", kernel_prewarm.rs:193). It fails the same way at base 473294ae5 (base-prewarm.log) and in Linux CI 410b shard 6 (ci-410b-failed.log:9694). It passed in the c4c macOS suite. So this is a fork regression outside this branch and needs its own owner.
    - `kernel_startup_memo::dispose_after_a_kill_waits_the_doomed_boot` FAILED 3/3. It also fails at base (base-memo.log) and at c4c on macOS (suite-c4c-v2/workspace-minus-pa-cli.log), so it is a macOS baseline failure.
    - `kernel_teardown::{engine_dispose_kernel_kills_the_prewarmed_kernel_process, session_engine_drop_kills_the_prewarmed_kernel_process}`: "read /proc: No such file or directory". These tests are Linux-only; this is a macOS environment issue.

### Mutation controls
- M1, probe bound reverted only (probe.rs: `tokio::time::timeout(bound, child.wait())` -> `Ok(child.wait().await)`):
  - `wedged_spawn_probe_fails_closed_within_its_bound` FAILED: "the bound must settle the probe: Elapsed(())".
  - `wedged_pre_spawn_probe_does_not_block_the_runtime` still ok, as expected (it pins non-blocking, not the bound). Log: mut1.log.
- M2, sync blocking probe restored (`command.as_std_mut().status()` inside the async fn):
  - `wedged_pre_spawn_probe_does_not_block_the_runtime` hung ("running for over 60 seconds"), EXIT=124 under `timeout 240`. Log: mut2.log.
- The tree was restored after each mutation. Final `git status` is clean at 9da26726178b.

## Other notes (informational)
- The pre-spawn probe launches 3 interpreters on every kernel start, including a pandas/scipy import. That adds about 0.4s warm and about 1.5s without pyc per start. This is fork design from 8556b9c32, not this branch.


---

# Round 2: re-review of 9da267261..60d5f6d51 (whole branch 473294ae5..60d5f6d51)

**VERDICT: CLEAR for 60d5f6d510a2322209384c0d175ab5f3246ab96b.** No P0 or P1 issue. Two P2 items remain (below); neither blocks.
Logs: /tmp/review-kernel-fix/r2/*.log.
All 7 commits in 473294ae5..60d5f6d51 are authored and committed as Junho Yeo <i@junho.io>. The whole-branch diff touches only the 7 kernel files listed by `git diff --stat`.

## Gates at 60d5f6d51 (hermetic wrapper, same environment as round 1)
- `cargo fmt --all -- --check` -> FMT=0
- `cargo clippy -p pa-core --all-targets -- -D warnings` -> CLIPPY=0
- `--lib kernel:: --skip lock_timeout_prevents_venv_rebuild` -> 125 passed, 0 failed, 4 ignored. The 3 new tests are included:
  - `superseded_probe_failure_leaves_the_newer_start_alone`
  - `killed_start_launches_no_further_probes`
  - `unwanted_spawn_probe_launches_no_further_interpreter`
- `env -u PRIME_AGENT_KERNEL_VENV ... lock_timeout_prevents_venv_rebuild` -> 1 passed
- `kernel_*` integration tests (15 binaries, --no-fail-fast): all pass, **including kernel_prewarm 3/3**. The only failures are the macOS baseline:
  - `kernel_startup_memo::dispose_after_a_kill_waits_the_doomed_boot` (also fails at c4c on macOS)
  - 2 `kernel_teardown` tests that read `/proc` (Linux-only)
- The new manager tests plus the `wedged_*` tests: 3 repeated runs, 5/5 passed each time (flaky0-2.log).
- The wrapper reported no surviving processes after every run. I killed no PIDs.

## Mutation controls (product code reverted only, tests unchanged)
- **M-A**: remove the `if !wanted() { return SpawnProbe::Abandoned; }` check in probe.rs.
  - `unwanted_spawn_probe_launches_no_further_interpreter` FAILED.
  - `killed_start_launches_no_further_probes` FAILED.
  - Log: mutA.log.
- **M-B**: remove the post-probe `start_stale`/Shutdown checks in startup.rs.
  - `superseded_probe_failure_leaves_the_newer_start_alone` FAILED.
  - Log: mutB.log.
- **M-C**: on `LaunchFailed`, revert to Idle and skip `cleanup_resources` (the 9da267261 behaviour).
  - **Nothing fails**: lib kernel:: shows only the known wrapper-env failure `lock_timeout_prevents_venv_rebuild`, and all kernel_* integration tests stay at the baseline.
  - So P2-e is aligned with c4c but **not pinned by any test** (P2-f below). Logs: mutC.log, mutC-int.log.
- The tree was restored after each mutation. `git status` is clean at 60d5f6d51.

## Status of round-1 findings
- **P2-a: resolved.** `start_stale` and Shutdown are checked after the probe await, before any verdict. A stale start now returns "Kernel start superseded" or "Kernel was disposed during startup". Both are FATAL markers, so the provisioner does not retry them. The test pins the state and `live_kernels::is_tracked`. The mutation shows the test is red without the fix.
- **P2-b: mostly resolved.** `wanted` is checked before every probe launch, so a superseded or killed start launches no further probe (the test and M-A prove this). Residual: a probe that is *already running* is not signalled on `kill()` or supersession. It runs until it exits, at most the 30s bound, and is then reaped. No leak; acceptable.
- **P2-c: resolved.** The new message is "kernel python <path> did not answer the runtime probe within 30000ms". It is not a FATAL marker, so it stays retryable, as intended for a slow cold start. The unit test pins `SpawnProbe::TimedOut`.
- **P2-d:** documented in the doc comment ("Only the direct child is signalled"). Acceptable.
- **P2-e: aligned with c4c.** On `LaunchFailed`, the code:
  1. invalidates the runtime probe cache,
  2. writes the `spawn error:` diagnostic,
  3. sets the state to Shutdown,
  4. removes the kernel from `live_kernels`,
  5. runs `cleanup_resources(Term)`,
  6. returns "failed to spawn kernel python {}: {error}".

  This matches the c4c spawn-error branch. The only c4c step without an equivalent is `ready_tx.send`, and no ready_tx exists yet at this point. `cleanup_resources` also clears start_memo and bumps the generation, as in c4c. `Rejected` and `TimedOut` stay Idle, and the probe-failure path is retryable.
- **P2-f (new, test gap):** no test pins the defunct state after a launch failure (M-C survives). Suggested fix: one small manager test with a `/nonexistent` python. Assert the state is Shutdown and the kernel is not tracked by `live_kernels::is_tracked`. Not blocking; c4c did not pin this either, as far as I saw.
- Minor: the `SpawnProbe::Abandoned` arm is effectively unreachable, because `wanted` turns false only with a stale generation or Shutdown, and both return first. It maps to "Kernel start superseded" (FATAL), so it fails closed. Fine.

## 60d5f6d51 kernel_prewarm fixture: the author's claim is VERIFIED
`prewarmed_kernel_lands_compaction_notice_without_tool_use`, one run per cell (logs pw-*.log):

| tree | fixture | kernel interpreter | result |
|---|---|---|---|
| fork 60d5 with the fixture line removed (= 88264d0db test) | old | abi2-venv (no bundled skills) | **FAILED**: "Turn prefix summarization failed: No more faux responses queued". The warning shows the 7 skills as unavailable. |
| same | old | fork skillful runtime (/tmp/pa-g1e64828/..., ABI 2, has all bundled skills) | ok |
| fork 60d5f6d51 | new | fork skillful | ok |
| fork 60d5f6d51 | new | abi2-venv | ok |
| **c4c238ca0** | c4c | abi2-venv | **FAILED** with the same message. The warning shows 5 skills unavailable (attach-image, compact, edit, goal, refine). |
| c4c238ca0 | c4c | c4c skillful runtime (/private/tmp/pa-gc6d8874e/...) | ok |
| c4c238ca0 | c4c | fork skillful | ok |

Conclusion: this is not a fork product regression. The failure depends only on whether the interpreter carries the bundled Python skills. Linux CI 410b shard 6 shows the same warning, because the fork's ci.yml exports the product-bootstrapped interpreter as an explicit `PRIME_AGENT_KERNEL_PYTHON`. Skills are not installed into an explicit interpreter by design, so the session adds the `python_skills_unavailable` notice. That notice turns the compaction cut into a split turn that needs a second summary, and the faux queue has only one.

**Legitimacy: legitimate.**
- The test's stated subject (file doc: "a main session with the config flag boots its kernel at creation so a compaction with no ipython use still lands the ipython_state notice") does not depend on the skill set.
- `enableBuiltinSkills: false` is a per-test settings file, not a product change.
- The core assertion is unchanged, and the test still passes with a skillful runtime.
- The alternative, "fix the CI runtime", would keep the test coupled to whatever interpreter the environment supplies. That coupling is a fixture fragility, not coverage.
- Open question for the parent: if production users are expected to have the bundled skills in the bootstrapped venv, it may be worth checking separately that the fork's `--prime-agent-bootstrap` installs them as c4c does. The CI warning only shows that the explicit-interpreter path lacks them. This fixture does not decide that question.

## Process note
During this round I ran `git checkout --detach` (no target) in ~/prime-agent-worktrees/rust-migration by mistake. It detached HEAD at its current commit 31e618be2 and changed no files. I immediately ran `git checkout feat/rust-migration`. The branch is at the same commit, up to date with origin, and the working tree is clean. Reflog entry: 2026-10-27 08:38:30 +0900.
