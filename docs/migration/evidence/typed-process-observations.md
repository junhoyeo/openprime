# Typed process-observation receipts for local test gates

Refs #41, #42. This is developer tooling, not a product process-identity format.

## Version and receiving consumers

The receipt schema is `hermetic.process-observation.v1`. A run manifest must bind
that schema, the exact committed guard source hashes, its unique frozen bundle,
and the explicit runtime provenance. The validated source candidate was named
`typed-unknown-v4`; it is **not** the previously deployed `guard-v4` bundle.
Coordinator activation requires a uniquely named next bundle and applies only at
future job boundaries. Never switch an in-flight profile.

`process-baseline.json` and `process-after.json` are now objects with:

- `schema`: the exact version above.
- `known`: complete birth-identified process rows.
- `unknown`: requested PID, probe stage, native size, errno, reason and safe partial
  metadata. These rows do not have fabricated birth keys.
- `gone_pids` and `gone_observations`: explicit absence observations and native
  phase/size/errno provenance. EPERM/EACCES never mean Gone.
- `listed_pids`, `sequence`, `observed_monotonic`, `enumeration_complete`.

The in-memory known-row map can be empty while Unknown observations remain. A
consumer must parse the complete envelope; it must not equate falsey/empty known
maps with a complete empty census. Invalid envelopes are errors, not empty arrays.

Updated consumers are `hermetic_test.py`, `hermetic_ownership.py` and the focused
`test_hermetic_test.py`, `test_hermetic_ownership.py`, `test_hermetic_unknown.py`
fixtures. The old one-off `/private/tmp/pa-faux-yct60aja/reconcile-owned-v1.py`
remains pinned to the old `guard-v4` API and old artifacts. Do not pass a typed
bundle to it. Prior ad-hoc array readers and frozen bundles remain old-protocol
consumers. Future reconciliation needs a separately reviewed, schema-bound reader.
This inventory is bounded to these known consumers, not a machine-wide audit.

## Observation and ownership rules

Expected per-PID observation denial/races produce typed Unknown, preserving earlier
rows and continuing to later PIDs. Global enumeration failure/truncation, invalid
native layout/result shapes, unexpected observer exceptions, and evidence bounds
remain hard tracking failures. Direct live-child Unknown is a required failure.

Historical birth/parent proof stays retained. Active proof, unresolved gaps and
explicit resolutions are separate. A current gap binds only its own complete
proofs and partial positive relationship; stale resolved PID history cannot erase
new evidence. A missing birth does not prove reuse. Missing historically relevant
identities receive one bounded final reconciliation; an already-required Unknown
is emitted as blocking without extra reference identity retries.

Explicit parent absence resolves the parent's current presence, not every earlier
descendant relationship. Such PPID/previous-PPID evidence can create a candidate,
never restored parent ownership. A complete different parent birth retires that
numeric inference. Known-row and Unknown-row reference failures both preserve
positive partial topology and private-root/log-inode evidence.

Unattributed host observations remain warnings, not declarations of independence
or safety. They do not mask subject exits 0/3/7 or signals. Required tracking gaps,
observed owned survivors and related candidates remain blocking. No scan-derived
PID is signalled. Only the retained exact Popen child is cleanup authority.

Snapshots/checkpoints are non-atomic and may miss rapid detachment. Final
classification has no separate wall-clock deadline. The subject timeout is not a
whole-wrapper deadline or a complete-containment guarantee. Shared TEST_RT paths
alone are not ownership evidence; profiles need unique roots, private targets and
dedicated owned working directories. Filesystem/network rules, controller dirfd
receipts, bounded positional log reads and signal authority are unchanged.

## Retained red/green evidence

The original faux attempt was interrupted during compilation by a per-PID EPERM
in the old guard. It executed **zero tests**; subject -15 / outer 1 remains an
infrastructure failure, not faux RED. CONTROL:
`/private/tmp/pa-control-rajbey5g`. The coordinator later observed the recorded
owned rustc identity absent without signals; that does not alter the original run.

Typed candidate v3 executed **42 tests: 40 passed, 2 failed**, with 19 OS controls
passing, subject/outer 1. CONTROL: `/private/tmp/pa-control-i247yt_4`. Its two failed
assertions exposed the explicit-parent-absence versus proven-reuse distinction.
They were preserved, not weakened.

The corrected typed candidate v4 executed **43 tests, all passed**, with **19 actual
OS controls passed**, subject exit 0 and observed outer exit 0. CONTROL:
`/private/tmp/pa-control-jjwpvcfo`; full log is `CONTROL/subject.log`. The typed
schema and complete enumeration were verified. No observed owned survivor,
blocking candidate or host warning remained; cleanup/tracking fields were null.
Tests include deterministic EPERM, prior-owned/related Unknown, later-row traversal,
partial topology, repeated reuse, missing-final reconciliation, global failures,
receipt/cleanup paths, and the real cooperative orphan and owned cwd/log-inode
probes. Native assertions exercise those fields, not every possible ABI or race.

This evidence does not certify whole-workspace CI or a native product gate. No
main merge, live runtime cutover, Trash use or automatic profile activation follows
from this tooling change. The unrelated PR #48 TMPDIR fixture thread requires its
own actual native verification.
