# Kernel migration evidence

Pinned migration base: `c4c238ca025e382d96fb5f37e6328ca77fb82ef3`.
Legacy fork reference: `703367badf5c84e48e8c8cd986fbcda882b6378c`.

## Snapshot restore safety (#32)

Do not port `21316aaa4`'s `/dev/null` reviver. Upstream commit
`63bdb5ad0366d5addc15662c2cf912d10ddf4590` (#3749) is already an ancestor
of the pinned Rust base. It is stronger: path handles revive as inert closed
`io.IOBase` stubs, raw file descriptors fail that record, and neither saved
paths nor live standard streams are opened. Upstream PR #3749 was approved,
had 35/35 checks green, and has no unresolved review thread.

Focused verifier:

```text
cd prime-agent-runtime
uv run --locked python -m unittest   test.test_repl.ReplTest.test_restore_never_reopens_pickled_file_handles   test.test_repl.ReplTest.test_restore_refuses_to_reopen_pickled_raw_fds
# 2 passed; exit 0
```

An additional direct probe restored closed `w`, `a`, and `x` handles and
verified the `w` target retained `draft`, deleted `a`/`x` targets stayed absent,
and all restored stubs preserved `name`, `mode`, and `closed=True` (exit 0).

## MCP SDK 2 schema compatibility

Legacy `934108148442f27dc764f4b3b219fe65ff6bd802` is already equivalent.
Current `prime-agent-runtime/src/rlm/mcp_base.py` and `mcp.py` read SDK 2's
`input_schema`, fall back to legacy `inputSchema`, and keep the public key
`inputSchema`. Upstream `844e85545af6858dcb3d6cfe42bbfcf2ca0be4e5`
introduced the snake-case regression coverage and is an ancestor of the pin.
No production port was needed.

## Bootstrap safety ledger

| Legacy commit | Migration verdict |
| --- | --- |
| `6da4f31a6` | Ported private generation isolation and continuously drained, bounded, credential-redacted child stderr. |
| `632dd62f6` | Ported exact override safety, compatible legacy reuse, root-wide serialization, and private publication. Deliberate safety divergence: inactive generations are retained on disk rather than recursively deleted; no environment toggle was added. |
| `2f2cdfb1e` | Ported PID + process-start identity leases and conservative liveness classification. Automatic recursive GC is intentionally omitted; inactive, unknown, malformed, unreadable, and symlinked generations are retained. |
| `9e06acc69` | Ported lease-before-marker atomic publication; unpublished builds are never selectable. |
| `b3e91c3d6` | Ported OAuth access/refresh/id token, client-secret, URL-userinfo, header, assignment, and CLI credential redaction. |
| `cc4f35b8d` | Ported resolver-to-child handoff protection, child lease lifetime through observed process exit, and same-line token redaction. Rust has direct children only, so no TS forkserver path was copied. |
| `8a681bfd1` | Superseded. `--clear` is incompatible with uv 0.12's non-venv rule and is not used. |
| `d3572635d` | Ported final uv invariant: the collision-free generation target is atomically reserved as an empty directory before `uv venv`; neither `--clear` nor `--force` is used. Real macOS commands passed with uv 0.9.5 and 0.12.5. Windows `Scripts/python.exe`, `PYTHONUTF8=1`, and no-window spawn remain intact. |

Path contract:

- A fully current legacy default environment is reused read-only.
- New default environments live under `kernel-venv.generations/`. Their
  collision-free paths are atomically created empty, carry platform directory
  identity through build/owner/publication/lease records, and stay ignored
  until validation, resolver lease, and atomic publication marker complete.
- `PRIME_AGENT_KERNEL_VENV` stays exact. A current path (including a current
  symlink) is reused read-only. A stale or unknown existing path errors without
  mutation. A missing exact path is built directly there under its lock.
- Automatic recursive generation GC is disabled. Inactive and abandoned
  generations are classified and reported but retained on disk; logical
  selection ignores them. A future explicit descriptor-relative doctor path
  may reclaim disk safely. External/custom interpreters are never touched.

Optional Python skill failures remain warn-and-continue. The immutable
bootstrap marker records the exact requested fingerprint and a strict
`requested = installed ∪ unavailable` partition with nonempty bounded reasons.
An identical failed request reuses that generation read-only; a changed
`pyproject.toml` fingerprint selects a new generation. Runtime ABI and promised
default imports remain fatal before publication.

## Runtime ABI boundary

Session/runtime producer commit `a032278ba` owns public integer
`rlm.RLM_RUNTIME_ABI = 2`.
The kernel pre-start probe accepts exactly integer ABI 2. Absent, ABI 1,
unknown newer ABI 3, string, float, bool, and malformed markers fail before
host subagent payloads; external overrides are never modified. This guards the
closed `queued` state and optional error-row receiver contract. A running
manager probes before its first child spawn and then reuses only that already
validated interpreter; update restore creates a new worker/manager, so the new
host cannot send the payload to an unprobed old kernel.

## Focused gates

The first native test attempt accidentally selected the real managed
interpreter because `PA_CORE_KERNEL_PYTHON` was unset; incident limits and
current file hash are recorded at `/tmp/openprime-kernel-isolation-incident.md`.
No bootstrap/uv/generation code ran on that path, but absence of mutation cannot
be proved without before-state. Final named tests used reviewed env-i v5 state,
synthetic config/auth absence, owned writable paths, and a manifest-matched
read-only base/runtime.

Red-first evidence is saved outside the repository:

- `/tmp/rust-kernel-red-diagnostics.log`: missing bounded sanitizer helpers,
  exit 101.
- `/tmp/rust-kernel-red-runner.log`: inherited raw stderr/arguments leaked and
  the behavioral test failed, exit 101.
- `/tmp/rust-kernel-red-explicit-override.log`: the old path deleted/rebuilt a
  stale explicit environment, exit 101.

Final reviewed v5 evidence: source `cargo fmt --check`, focused `cargo check`,
and focused clippy `-D warnings` all exit 0; `kernel_bootstrap_generations`
16/16 pass (after one preserved 15/1 macOS `/tmp` alias fixture failure and
one authorized corrected rerun); five kernel lifecycle/capture/restore/resume/
stop binaries 31/31 pass; and the isolated real `a032278ba` full ABI probe 1/1
passes. Python framed/legacy/raw-fd/closed-file/MCP schema regressions passed
5/5 before the isolation hold; runtime source is unchanged here. Earlier
intermediate bootstrap-unit evidence was 61 pass/3 ignored, but is not claimed
as a final-tip gate. The stable Win32 directory-identity replacement passed a
private `x86_64-pc-windows-gnu` cargo check before final retain-only edits.
Exact commands, exit codes, and final SHA are recorded in the handoff after the
final combined-tip gates. `Cargo.toml` and `Cargo.lock` are unchanged.
