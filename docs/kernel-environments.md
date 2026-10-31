# Python kernel environments

`PRIME_AGENT_KERNEL_PYTHON` selects an exact, externally managed interpreter.
Prime Agent validates the supported runtime ABI and required imports. It does not
install into, enrol, relocate, or delete this interpreter. Missing optional skills
warn without making an otherwise valid runtime unusable.

`PRIME_AGENT_KERNEL_VENV` selects a bootstrap root. A new missing path is built
at that exact path. After validation, Prime Agent records its ownership in the
sibling `<root>.prime-agent-managed.json`. This record binds the canonical path,
base and parent filesystem identities, a unique token, and the original bootstrap
manifest hash. A version file or `pyvenv.cfg` alone is not ownership evidence.

When a verified managed root needs a different runtime or Python skill set,
Prime Agent builds and validates an immutable sibling under `<root>.generations/`.
It returns that generation's interpreter, not necessarily `<root>/bin/python`.
The original environment and other generations remain available to their existing
kernels. This makes the installed environment payload immutable, not editable
skill source: skills retain their existing `--editable` installation semantics.
A fresh import or reload can see edits in the configured skill source directory,
even from an older generation. Unchanged requests reuse a published matching generation. Optional skill
installation failures are recorded as outcomes; an unchanged failed optional
request does not trigger an endless sequence of new generations.

The installed skill set is resolved before host-capability visibility filtering.
Print and daemon embeddings can therefore share an install while preimporting
different visible skills. Bootstrap prepares bundled and enabled user skills;
project-local skills are resolved when a session opens the project. Adding,
removing, disabling, or changing a configured skill can select another generation.

## Existing and external environments

Existing compatible explicit environments remain exact and read-only. They are
not silently enrolled. A non-current, unenrolled environment is refused. Choose a
new missing `PRIME_AGENT_KERNEL_VENV` path to opt into managed generation updates,
or set `PRIME_AGENT_KERNEL_PYTHON` to an externally maintained compatible interpreter.
Do not copy or edit ownership markers to enrol an old environment.

New managed explicit bootstrap currently requires Unix owner/permission checks.
On platforms where existing-record ownership cannot be verified, a non-current
explicit target is refused before creating its parent directories, environment,
or lock metadata. On every platform, an explicit path's missing parents are
created only once the request has passed its refusal checks.
Existing compatible explicit environments and `PRIME_AGENT_KERNEL_PYTHON` remain
usable read-only. Windows ACL verification and hard-link filesystem coverage are
not established by the Unix tests; inherited ACLs are not treated as proof.

Malformed ownership, a replaced directory, changed base manifest, or an unowned
sibling generation root blocks managed updates without repair. A compatible exact
interpreter can still be used read-only; unknown ownership never authorizes lease
or enrolment writes. A leaf symlink cannot become
a managed root; a compatible legacy symlink remains read-only. Parent-directory
aliases such as `/tmp` and `/private/tmp` resolve to the same canonical namespace.
Copied environments do not inherit ownership because their filesystem identities
differ. Unknown process identities do not authorize abandoned-build reclamation.

## Retention and rollback

Published environments are never updated in place or recursively garbage-collected.
Interrupted explicit builds with proven dead ownership may be moved aside and
retained. Unknown, live, malformed, and replaced paths remain untouched. This
trades disk space for preserving active kernels. No automatic cleanup is promised.

Recovery is conservative, not seamless in every crash window:

- A private, identity-bound version-3 build record can finish first publication
  after a proven-dead builder only when the actual interpreter still validates
  against the unchanged request. Older or untrusted build records are retained
  without enrolment. A pending-record conflict can refuse even a current base.
- If the request changed after validation but before enrolment, the old base and
  build record remain unmodified. Choose a new missing configured root.
- If a crash creates the sibling root before its ownership marker, the next run
  refuses that unmarked directory rather than adopting it. Choose a new missing
  configured root; the previous base remains available through its exact Python.
- If the owner marker is published before the build record is removed, the valid
  owner allows reuse but the leftover build record remains and forces locking.

Record publication is atomic create-if-absent at the final leaf. File contents are
synced, but the containing directory is not synced. This is process-crash recovery,
not a power-loss durability guarantee. Parent checks remain pathname-based; this
is not a zero-race namespace protocol. Ownership records establish local management
provenance, not a cryptographic integrity seal over every installed file.

Read-only update/rollback tooling can inspect the configured root ownership
sidecar, `<root>.generations/.prime-agent-root.json`, each generation's
`.generation-owner` and `.generation-published`, and its `.bootstrap-version`.
The returned interpreter path identifies the selected artifact. The markers are
local identity records, not portable enrolment files or an instruction to repair
an environment. Never delete old environments as part of activation or rollback.
