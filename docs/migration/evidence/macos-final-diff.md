# macOS full-suite diff: fork `a9ae2e3de` vs upstream `c4c238ca0`

Parent-accepted on 2026-10-27. Logs and `results.json` are in `/tmp/mig-lead-opus/suite-fork-a9ae` and `/tmp/mig-lead-opus/suite-c4c-v2` (local evidence, not committed).

Both runs used the same harness: `/tmp/mig-lead-opus/macos_suite.py` inside `hermetic_test.py` (env -i, synthetic HOME, egress sandbox), with RUST_TEST_THREADS=4 and CARGO_BUILD_JOBS=2. Each pa-cli target had an 1800 s timeout in the fork run; the c4c run got that timeout only after it resumed. The fork used kernel runtime TEST_RT and c4c used C4C_RT.

| | fork a9ae2e3de | c4c238ca0 |
| --- | --- | --- |
| tests parsed | 5837 | 5617 |
| failed | 75 | 77 |
| ignored | 22 | 20 |
| pa-cli targets that do not compile on macOS | 0 | 4 (interactive_daemon_e2e, terminal_state_differential_e2e, termios_process_exit_e2e, windows_runtime_triage_e2e) |
| targets timed out or hung | exit_backfill_pty_e2e, kitty_verdict_time_e2e (TIMEOUT); interactive_fork_child_mode child killed at 32 min | kitty_verdict_time_e2e (4 PTY tests, killed after 2.5 h); interactive_fork_child_mode child killed after 2.5 h; exit_backfill: see below |

Load average was about 35-41 at the start, 14-19 in the middle and about 8-11 at the end; the samples are in `suite-fork-a9ae-load.txt`. Load came from programs outside our control (VMs).

## Fork-only failures: 12 raw, 0 fork-introduced product failures

1. **Failed in both trees (5).** The results parser keyed these lines differently in the two trees; a grep of the logs shows them FAILED in both: bash_bang_headless a_bang_during_a_streaming_turn_holds_then_flushes and bang_runs_the_command_and_mounts_the_bash_card, clipboard_auth_commands_e2e tui_prime_login_renders_the_inline_team_picker and tui_traces_without_a_credential_runs_the_login_flow, daemon_rejection_headless refused_saved_session_create_names_the_holder_and_next_steps.
2. **Artifact of the harness's target-dir name (2):** continue_guard_e2e print_resume_refuses_a_foreign_lease_holder and hold_refusal_e2e a_foreign_lease_holder_rejects_the_create_with_the_hold_refusal. Both failed 3 of 3 times on the fork and passed 3 of 3 at c4c.
   - Cause: `hold_refusal::classify_from` names a holder as "another Rust build" only when its exe path has a `target/<profile>` component. The fork run's CARGO_TARGET_DIR is `~/prime-agent-worktrees/targets/rust-migration`, which has no `target` component, so the holder is classified AnotherProcess. The c4c target `/tmp/pa-rust-study/target` and Linux CI's `<repo>/target` both have one.
   - Proof: the same fork test binaries copied to `/private/tmp/mig-lead-opus/tproof/target/debug/deps/` classify the holder as "another Rust build" (log `target-path-proof*.log`). Both tests pass on Linux CI at a9ae (run 40424205627).
3. **Harness env artifact (1):** pa-core lock_timeout_prevents_venv_rebuild. It fails with the wrapper's PRIME_AGENT_KERNEL_VENV export and passes with `env -u PRIME_AGENT_KERNEL_VENV` (`fork-reruns.log`). Only the fork honors that override (c4c fails venv_dir_honors_override), so only the fork sees the wrapper's value. The test passes on Linux CI.
4. **No macOS baseline (4).** These targets do not compile at c4c on macOS. Each failure reproduced on rerun, and all four pass on Linux CI at a9ae. They are tracked as macOS follow-ups (#41):
   - interactive_daemon_e2e ensure_daemon_running_spawns_supervisor_and_tui_attaches ("the spawned supervisor removed its socket file"). This is the known upstream non-Linux socket-unlink difference (upstream-rust.md, Known upstream macOS issues, item 4).
   - interactive_daemon_e2e tui_close_reason_rows_survive_late_turn_status (the settle did not complete within 60 s).
   - interactive_daemon_e2e tui_renders_and_fires_user_keybindings_from_settings (the prompt-context hint does not render the override).
   - terminal_state_differential_e2e the_agents_view_roster_failure_behind_a_handoff_restores_every_mode (the mock frame write hits BrokenPipe).

## c4c-only failures: 14

These failed at c4c and passed (or are absent) on the fork. They include the fork's portable idle_passivation probe (747571c49), venv_dir_honors_override, daemon_client socket_close_reports_daemon_closing_reason, kernel_startup_memo doomed_settle_keeps_the_newer_boot_listener_state, quiet_tick_busy_stream_headless, and rpc_mode_e2e rpc_compact_flush_is_bounded_against_a_stalled_reader. Five are headless tests whose names differ only by parser key.

## exit_backfill_pty_e2e::the_top_key_loads_the_session_start_as_the_last_input

In the c4c baseline it failed fast with `spawn pty child: NotFound`. That was an artifact: my reclaim task deleted test binaries during the run. A rerun at c4c with the binary intact hung until the 1800 s timeout (exit 124, `rerun-c4c-exit_backfill.log`), the same as the fork. So it is an upstream PTY hang in both trees.

## Verdict

There are 0 fork-introduced failures that can be attributed to fork product code. Every fork-only failure is one of three things: a harness artifact with a proof, a test in a target that does not compile at c4c (all pass on Linux CI), or a parser-key duplicate.

## Other final checks at the merged tip

- Release binary: built from `f0c09f100` with `cargo build --release -p pa-cli --bin prime-agent` (sha256 `9a50804faa84b5b8a454f05a2cd24602b2e927102c1400402252a65c0af4d1b6`, arm64).
- Native final cases on that binary, using the frozen checker (`run_native.py` sha256 `c879e342…f5f5f`) with `DO_NOT_TRACK=1` and the egress sandbox. Family: P1 spawn/collect/reply and P2 cancel. Resume: a second daemon on the same root. Both passed, with no surviving processes.
- Python suites at `97caca200` (after PR #43) under the egress sandbox: factory 309/309, the runtime full suite OK, bundled skill suites 29/29, agent-sessions 25/25, aside 4/4, and the fork release-target policy check.
- `packaged_layout_e2e -- --ignored` at `a9ae2e3de` **failed** (`bootstrap_kernel_venv_from_packaged_sidecar`). It is a real regression, fixed by the bootstrap hotfix [#45](https://github.com/junhoyeo/openprime/pull/45).
