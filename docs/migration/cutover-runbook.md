# Rust cutover: preflight, rehearsal, and rollback

**Status: activation pending.** This procedure does not authorize a live switch.
It addresses preparation for [#40](https://github.com/junhoyeo/openprime/issues/40).
Do not close #40 merely because a dry-run or rehearsal passes.

The writable repository is `https://github.com/junhoyeo/openprime`.
`PrimeIntellect-ai/prime-agent` is a read-only reference, not an install source.
Preserve the old fork TypeScript install and the final `develop` commit
`703367badf5c84e48e8c8cd986fbcda882b6378c` throughout this procedure.

## Safety contract

- `scripts/cutover_preflight.py` is read-only by default. It has no install,
  update, symlink replacement, live stop, trash, or cleanup command.
- It reads only explicit artifact inputs. It does not discover auth, settings,
  models, harness data, transcripts, or installed runtime directories.
- An imported process inventory is **unverified**. A PID, executable name,
  `kill(pid, 0)` result, stale receipt, or equal-looking unsupported start token
  does not authorize a signal. Unknown identity means retain and refuse.
- The dry-run verifies local archive and staged bytes. It does **not** prove
  that a URL served those bytes, that a binary came from the declared commit,
  that a kernel imports ABI2, or that the installed runtime changed.
- `--simulate` accepts no artifact or process inputs. It creates fresh synthetic
  child processes under its own short `/private/tmp` root, stops them through
  their own input pipes, and reaps them. Its failure cleanup targets only its
  unreaped direct children. It never starts a packaged product binary.
- Never run an unqualified `prime-agent update`. The built-in default channel
  can install an upstream artifact.

## 1. Resolve acceptance gates

Before requesting activation, record the exact source tip, independent review,
red/green regressions, and per-job CI results for every accepted fix. At minimum:

1. #35 prompt-family and agents-view wake opt-out; #36 created-root opt-out.
2. #37 process identity across adoption, lease, boot reap, legacy admission and
   update exit waits. Trace the final TS semantics, not an intermediate patch.
3. #38 unverifiable-worker retention and fresh identity checks before each signal,
   including persistent-shell groups. A vanished group leader and an earlier
   authorized TERM do not authorize a later signal to a possibly reused PGID.
4. #41 path/cron/ledger/trash correctness, platform assumptions and faux runtime
   lifetime; #42 deleted-child record/wake and real gateway/DNS fencing.
5. #44 explicit environment updates preserve any leased old environment.
6. Focused UI compatibility (#39), native isolated trials, exact-tip workspace
   gates, package validation, and fresh independent review.

Existing #47 already fixes the direct-attach test driver-starvation flake at
`dcc82a3d82a1938601331015b6afe9aa18369d3c`. Do not reopen it without a new reproducer.
A missing benchmark secret or an unrun platform job is not a green gate.
The owner may explicitly accept a remaining risk; record that acceptance, do not
infer it from a merge or a passing unrelated test.

A new parser does not repair old peers. Legacy `ps:` records have no reliable
UTC/locale provenance; legacy `win:` records can use different epochs. A
versioned writer/parser fix must retain ambiguity rather than infer death or
signal authority. Even after that fix ships, keep the controlled legacy shutdown
before sharing real state unless mixed-version safety has separate proof.
Portable process-start tokens also have platform limits (for example second
precision in `ps` output); do not describe them as cryptographic process identity
or claim that a probe and a later macOS signal are one atomic operation.

## 2. Identify and verify the fork artifact

Use a reviewed fork release asset. Retain all of the following together:

- the served metadata and artifact URLs, final URL after redirects, fetch time,
  response status, and SHA-256 of the exact downloaded bytes;
- the source commit, build job/run and job results, target triple, release
  version, and independently verified association between commit and artifact;
- archive SHA-256, executable SHA-256, and complete staged file hashes;
- packaged runtime source, skill manifests/packages, catalog files and their
  validated schema/content receipts;
- the isolated native ABI2 probe receipt: actual `sys.executable`, `sys.prefix`,
  `rlm.__file__`, ABI value, runtime interface tests and bundled skill imports;
- isolated synthetic state/family/resume receipts for the exact packaged binary.

An unsigned manifest's `commit` field is a declaration, not build provenance.
The binary currently has no authenticated commit-attestation interface. Verify
its build-to-artifact association from the reviewed fork CI run separately.
The preflight deliberately reports `build_commit_verified: false` and
`served_url_verified: false`; those are outside its offline authority.

Prepare a JSON manifest with this shape (all hashes below are placeholders):

```json
{
  "repository": "https://github.com/junhoyeo/openprime",
  "artifact_url": "https://github.com/junhoyeo/openprime/releases/download/REVIEWED_TAG/REVIEWED_ASSET.tar.gz",
  "commit": "FULL_40_CHARACTER_REVIEWED_SOURCE_SHA",
  "version": "EXACT_PACKAGE_VERSION",
  "archive_sha256": "FULL_64_CHARACTER_ARCHIVE_SHA256",
  "legacy_commit": "703367badf5c84e48e8c8cd986fbcda882b6378c",
  "process_inventory": [
    {"role": "old-daemon", "pid": 123, "start_id": "operator-recorded-token"},
    {"role": "new-trial-worker", "pid": 456, "start_id": "operator-recorded-token"}
  ]
}
```

The helper accepts only HTTPS GitHub release paths in `junhoyeo/openprime`.
Tag and asset names must be plain canonical components; encoded or
path-normalizing forms are refused. It does not fetch or extract the archive. Use an already staged, separately
reviewed package layout. It compares every regular archive file with that layout,
rejects links, special files, traversal, duplicates, missing required assets and
unreviewed staged files. Catalog JSON parsing is not full catalog validation;
run the existing release catalog and package validators as separate gates.

On a machine with a live install, use the reviewed hermetic wrapper and its
approved private target/runtime profile for **all** commands below:

```text
BASE_PYTHON scripts/hermetic_test.py --kernel-python TEST_RT --   TEST_RT scripts/cutover_preflight.py --manifest OWNED_MANIFEST.json   --archive OWNED_ARTIFACT.tar.gz --staged OWNED_STAGED_DIRECTORY
```

`BASE_PYTHON`, `TEST_RT`, target and artifact paths above are placeholders supplied
by the verification coordinator. Never substitute the live managed interpreter.
Retain stdout and wrapper ownership receipts. A zero exit verifies only the
listed local checks; `activation` remains `pending`.

## 3. Rehearse rollback without a live product

Run the helper with `--simulate` through the same wrapper. This creates only
synthetic old/new/rollback worker fixtures. The receipt lists each readiness PID,
parent identity, role and successful reap. The order is old start/stop, new
start/stop, old rollback start/stop. It cannot consume an arbitrary executable,
manifest PID, real agent root or existing install symlink.

This is a procedure/lifecycle rehearsal, not a product-native trial. Separately
run the real release package against fresh synthetic agent and kernel roots,
with mock providers, no copied OAuth, resolved native socket/path checks, and
before/after ownership receipts. Readiness must include actual kernel imports,
not only `--version`, marker files or a helper's own success return.

## 4. Owner-controlled live activation (not automated here)

Only after steps 1–3 are accepted, the owner decides whether to activate. Prepare
exact installation and rollback commands for the **verified artifact and known
install layout** at that time. Do not pre-fill a generic destructive command.

1. Preserve the exact old TS executable/install and pinned develop rollback
   input. Do not overwrite or rebuild its environment. Record their identities.
2. Inventory every old daemon, worker and kernel with trustworthy current process
   start identity and ownership. Unsupported/unknown identity blocks action.
3. Stop only proven old processes by the reviewed product lifecycle procedure.
   Revalidate ownership immediately before every signal, including escalation.
   Confirm exit from fresh observation. Do not use name matching or killall.
4. Install the reviewed fork artifact at the approved destination. Select the
   new runtime only after its path and bytes are checked. Do not modify auth or
   rewrite persisted data as an implicit install side effect.
5. Start the selected runtime. Verify the **served** executable path/hash/build,
   daemon/worker identities and kernel ABI/path against the artifact receipt.
   A release build, ready artifact or green CI does not prove the runtime served.
6. The owner separately decides the repository default branch. This helper never
   changes it, publishes a release, or merges main.

## 5. Rollback decision and execution

If new runtime verification fails, stop only the newly proven-owned processes.
Keep the failed artifact, roots, logs and receipt for diagnosis. Restore selection
of the preserved old TS install, then start it through its original entrypoint.
Verify the served old executable and process identities. Do not run a generic
updater to recreate the old version. Do not infer that Rust-written state is
backward-compatible; preserve state and use an explicitly reviewed compatibility
or restoration plan before an old runtime opens changed state.

The old develop branch and install remain untouched until the owner explicitly
ends the rollback window. A successful synthetic rollback does not authorize
stopping a live TS session or modifying any real state.

## Retained isolation-incident generation

The stray `generation-55e2b7c0...` described in `upstream-rust.md` remains evidence.
This procedure does not remove it. Before activation the owner must explicitly
resolve its cleanup/retention disposition and verify no live lease uses it.
Never delete it based only on its name, age, absence from a scan, or an unverified
process identity. Preserve the incident logs and evidence even after that decision.
