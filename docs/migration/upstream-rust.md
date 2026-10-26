# Upstream Rust migration

Status: **in progress; not approved for cutover or release**

This migration adopts the upstream Rust implementation and re-ports only fork behavior that is still needed. It does not merge the legacy TypeScript `develop` branch into Rust. The legacy branch remains the rollback implementation and the evidence source.

## Source pins and repository boundary

| Role | Immutable source |
| --- | --- |
| Target repository | `https://github.com/junhoyeo/openprime` (`origin`) |
| Rust base | `c4c238ca025e382d96fb5f37e6328ca77fb82ef3` |
| Legacy fork `develop` | `703367badf5c84e48e8c8cd986fbcda882b6378c` |
| Legacy fork `main` | `e260085dd8f742e0def3d871860c9a888b114851` |
| Integration branch | `feat/rust-migration` |
| Intended PR base | `origin/main` |

`origin/main` is an ancestor of the pinned Rust base. The upstream history is retained with its original authors. New fork commits use the ambient repository identity. This work must never push, open a pull request, tag, or publish against `PrimeIntellect-ai/prime-agent`.

## Inventory and proof rule

The complete 99-commit, non-merge legacy inventory is in:

- [`legacy-commit-map.json`](legacy-commit-map.json), the authoritative machine-readable ledger.
- [`legacy-commit-map.md`](legacy-commit-map.md), its review view.

Each commit and PR #32/#33 must end in exactly one state:

- `verified-upstream-equivalent`: source-level mapping plus a relevant verifier proves equivalent behavior at the integrated Rust tip.
- `ported`: the integrated Rust commit and its passing regression/parity verifier are recorded.
- `truly-obsolete`: a language/runtime-specific behavior no longer exists, with a concrete rationale. This state cannot be inferred from a subject line.
- `BLOCKED`: evidence is missing or a gap remains. A blocker cannot be used to approve cutover.

### Special audit: `0b5676b`

Legacy commit `0b5676b6498cf0ecb89dbffc530b2f67545b8f18` restored three behaviors after an old upstream merge. They must be audited independently rather than classifying the commit by its broad subject:

1. Normalize process start IDs before deciding a descriptor belongs to a replaced process; unverifiable identity remains unknown and is not reaped as stale.
2. Exclude failed workers from the global heartbeat list so one poisoned worker cannot fail the entire listing.
3. Preserve refinement-status test fixture compatibility after queue-selection changes. This third item may be TypeScript-test-only, but it stays blocked until the Rust test surface is checked.

## Work ownership

| Lane | Owner | Owned surface |
| --- | --- | --- |
| Integration | `rust-migration-lead` | this documentation, fork CI/package safety, tested commit integration, combined gates |
| Inventory | `upstream-rust-scout` | all 99 legacy commits, upstream survey, source mappings |
| Kernel | `openprime-pr32` | bootstrap, snapshots, kernel restoration, PR #32, Python MCP2 schema compatibility |
| Skills | `openprime-pr33` | bundled `agent-sessions` and Aside parity, PR #33, focused prompt/package registration |
| Providers | `rust-port-providers` | `pa-ai`, `pa-models`, auth/gateway/retry and model-selection deltas |
| Lifecycle | `rust-port-sessions` | daemon/RLM revival, process identity, refinement feedback, persisted `/btw` behavior |

Shared `pa-core` and CLI seams require owner coordination. Duplicate or stub implementations are rejected when upstream already provides the behavior.

## Safety boundary

All runtime and state checks use a temporary `HOME`, explicit temporary daemon socket, isolated session directories, copied or synthetic fixtures, private build output, and mock provider endpoints. Tests never auto-discover or stop the installed daemon. Real OAuth files are neither read into tracked artifacts nor copied into another runtime. No test mutates `~/.prime`, `~/prime-agent`, the installed symlinks, gateway configuration, or existing processes.

State-compatibility acceptance is not satisfied by `--version`. Before cutover, the integrated binary must prove all of the following while original evidence remains byte-identical:

- sanitized TypeScript session JSONL, settings, models, harness entries, and skill metadata load from copied fixtures;
- archived sessions reopen;
- restored kernel snapshots never truncate or recreate referenced files;
- auth schema compatibility is tested with synthetic values only;
- daemon sessions, tool calls, spawn/message/observe/collect, cancellation, and resume work over isolated sockets;
- the packaged install layout contains the Python runtime and Python-backed skill resources expected by the kernel contract.

## Baseline evidence

The pinned base was checked from a detached, clean worktree at `/private/tmp/pa-rust-study`.

| Check | Result |
| --- | --- |
| Source `HEAD` | `c4c238ca025e382d96fb5f37e6328ca77fb82ef3` |
| Release binary SHA-256 | `5e1b004ba97be729c3f3fcb05187e1b7a5759c9a455a9e2cccfa9cefa0cedd54` |
| `prime-agent --version` in isolated `HOME` | pass, `0.10.1` |
| `prime-agent --help` in isolated `HOME` | pass; expected CLI command surface printed |
| `prime-agent prompt --cwd <rust-worktree> --json` in isolated `HOME` | pass; 8 layers and a 53,520-byte JSON result |

These checks prove only binary startup and prompt assembly. They do **not** prove daemon, state, provider, family, resume, or migration parity.

## Acceptance gates

All commands use `CARGO_BUILD_JOBS=2` and a worktree-private `CARGO_TARGET_DIR`. Focused lane checks run before integration. The exact integrated tip must then pass:

- `make check` (format, clippy with `-D warnings`, workspace tests, release workspace build);
- `make deny` when dependency files change;
- release/package assembly and install-layout smoke tests;
- required focused Python suites for bundled runtime and skills;
- state-safety tests over copied/synthetic fixtures;
- isolated daemon and family behavior checks;
- user-visible and protocol differential checks against the frozen TypeScript reference for changed surfaces;
- cross-platform CI inspection, including per-job results and unresolved review threads;
- independent adversarial review of `c4c238ca0..HEAD`, followed by fixes and exact-tip reruns.

A synthetic provider is the CI/default smoke path. A paid gateway smoke is optional and may run only after config/auth wire compatibility is verified; it must use a small prompt, disclose cost, and never print token values.

## Packaging and release boundary

During migration, the branch and draft PR may build verification artifacts only. No release tag, default-branch switch, installer update, publish job, or live runtime restart is authorized. Fork workflow changes must make `junhoyeo/openprime` the explicit target and prevent a migration PR or an upstream-shaped workflow from publishing accidentally.

## Rollback and cutover plan

Rollback evidence is the untouched `origin/develop` tip `703367badf5c84e48e8c8cd986fbcda882b6378c` and the currently installed TypeScript runtime. The migration does not rewrite or delete either.

After every blocker is closed, the integration lead will provide the tested Rust artifact identity, package checksums, fixture hashes, exact cutover commands, and rollback commands. The parent owns merge, default-branch selection, installation, daemon restart, and live verification. Until that explicit action, this branch is not the served runtime.

## Current blockers

The machine-readable ledger is intentionally all `BLOCKED` at the first checkpoint. It will be updated only from source mappings and passing verifiers. PR #32 and #33 remain blocked until their owners' exact-tip reports are integrated. Daemon, state, package, parity, combined gates, CI, and adversarial review are also pending.
