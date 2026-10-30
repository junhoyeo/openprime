# Hermetic macOS guard (Refs #41, #42)

`scripts/hermetic_test.py` is the only local gate entry point on a machine with a
live installation. It now fails closed when the OS fence is unavailable. Setting
`DO_NOT_TRACK` does not replace filesystem or network isolation.

## Invocation and limits

- Set `CARGO_TARGET_DIR` to an existing, owner-private canonical
  `/private/tmp/pa-<unique>/target`. The target and its parent must be owned by the
  current UID and not group/world writable. There is no implicit writable target.
- Pass `--kernel-python` explicitly. The wrapper rejects lexical and resolved
  interpreters in the real owner's `.prime` and `.prime-rust`; the owner comes
  from the OS account database, not synthetic `HOME`. Keep the venv's lexical
  interpreter path so Python retains its `sys.prefix`.
- Provision/verify that runtime separately. The guard's stdlib preflight does
  **not** prove the runtime package or ABI is installed.
- The sandbox permits filesystem writes only below the fresh canonical short
  run root and the private target, plus null/TTY device I/O. Source, installed
  toolchains and live install paths stay read-only. Real `.prime`/`.prime-rust`
  reads are also denied. A synthetic protected canary uses the same rule generator.
- HOME/TMPDIR/XDG are synthetic; provider credentials and proxy variables are not
  inherited. Kernel interpreter overrides are explicit. Agent/session directories
  and `PRIME_AGENT_KERNEL_VENV` are not forced globally: tests may set their own
  HOME and exercise the native path resolver.
- External IP traffic and direct DNS port 53 are denied. Loopback mocks are
  permitted except ports 8787, 8791 and 18789 and additional `--protect-port` values. The
  mDNSResponder socket exception is removed, and its Mach service is denied.
  Unix sockets are permitted only under the owned run root.
- This is **not** a claim that every local service is inaccessible. An unrelated
  local proxy on a non-protected port could relay traffic. Do not use proxy
  fixtures to bypass the policy. Cached/system name resolution may still occur;
  name resolution is not permission to connect to a denied destination.
- `/bin/ps` is the sole `no-sandbox` executable exception: macOS otherwise refuses
  this setuid system process inspector. It cannot execute arbitrary commands.
  The wrapper itself uses native libproc, not unsandboxed arbitrary commands.
- `--timeout` must be finite and positive (default 1800 seconds). All post-launch
  identity/receipt/wait work is inside cleanup protection. Receipt errors,
  identity errors and interruption still reap the exact retained Popen child.
- The wrapper records microsecond birth identity, UID, PPID, PGID and SID from
  native libproc (SDK `proc_bsdinfo`, checked size/offsets), without private argv
  or environment reads. It inventories same-UID identities before and after
  the subject. New unexplained identities cause a nonzero UNKNOWN result,
  including external-toolchain and detached/reparented executables; they are
  never presumed owned and never signalled. Concurrent unrelated new processes
  can therefore reject a run. Inspection errors also fail closed.
- No claim of complete OS process containment is made. The census accounts for
  identities observable at its boundaries, not ownership of every descendant.
  Only the direct retained Popen child is cleanup authority. Independent slot
  verification must resolve unknown processes; absent argv markers are not proof.
  All receipts and canaries are retained.

## Mandatory actual enforcement preflight

Before the subject starts, the wrapper runs `hermetic_preflight.py` with the
**same profile**. It checks allowed writes; outside-root, symlink and protected
writes; protected reads; an owned synthetic protected port via IPv4, IPv6,
IPv4-mapped IPv6 and localhost; direct DNS/external UDP-connect denial; and
allowed dynamic loopback/owned Unix sockets. UDP-connect checks send no packets.
It never writes real owner files or contacts the real gateway. The parent also
verifies both forbidden canaries remain unchanged. Any failure prevents launch.

The successful probe emits all 14 check labels. `preflight.json` and `command.json`
remain in the printed run root. This is actual OS enforcement evidence, not a
profile-source assertion.

## Local evidence (2026-10-30)

The first stdlib-only run failed: 6/10 unit tests passed; four nested
`sandbox-exec` applications failed with `sandbox_apply: Operation not permitted`.
No enforcement claim was made from that run. macOS does not allow reapplying the
sandbox. The correction uses one fence and a mandatory startup preflight, not a
weaker profile.

The second stdlib-only run passed: 14/14 actual preflight checks, 6/6 unit tests,
exit 0, no surviving processes. Its retained root is
`/private/tmp/pa-hrncybva6`; the launch log is
`/private/tmp/pa-guardcheck-yahbygbl/second.log`. The launch interpreter was a
read-only standalone CPython 3.11.14, not a live kernel venv. No product or ABI
runtime was imported. Later verification results belong in the PR receipt.

## Lifecycle review follow-up

Independent review correctly rejected the first receipt's broad “no survivors”
claim: argv-path matching missed `/bin/sleep` and toolchain children, and receipt
I/O happened before cleanup protection. These were fixed before native admission.
The third stdlib-only run passed **14 real OS checks + 11 unit tests**, exit 0,
with **no new same-UID processes**. Its root is `/private/tmp/pa-hero4g9ib`; the log
is `/private/tmp/pa-guardcheck-yahbygbl/third.log`. Tests inject receipt failure,
identity failure and interruption around real owned `/bin/sleep` children and
verify they are reaped. A real external `/bin/sleep` is reported UNKNOWN, and
remains alive until the test's own Popen handle cleans it up. Invalid nonfinite
deadlines and missing/unsafe target paths are refused. This remains guard-only
proof, not a runtime ABI or product-test gate.

## Final outcome and controller-metadata proof

Every launched subject now has an exclusive-create `outcome.json` written after
bounded cleanup. It records the actual child return code (negative values mean a
POSIX signal), timeout state, raw wrapper return code, normalized 8-bit CLI exit
code, decision, failure/cleanup types, direct-child reap result and full unknown
process evidence. Consumers must combine a fresh valid receipt with the observed
outer process status; a planned outcome or fallback cannot certify success.
Receipt creation errors stay nonzero and emit a distinctly failed stderr fallback.

The controller's six reserved metadata paths are write-denied to the subject.
All JSON receipts use exclusive creation, so a subject-created symlink cannot
redirect an unsandboxed controller write. The tests use only an owned forbidden
canary; no live owner file or service is probed.

The fourth run completed 14 controls and 13 tests with subject exit 0 but outer
exit 3 because of one new external process. Its raw evidence remains retained at
`/private/tmp/pa-hhdbgy3d2` and `guardcheck/fourth.log`; this was not rewritten as
an unqualified pass. A further concrete correction fixed receipt-error fallback
exit reporting and the metadata symlink seam; it was not a census retry.

Final tested source: **16/16 actual OS controls + 14/14 tests**, subject exit 0,
outer wrapper exit 0, no new same-UID identities. Root:
`/private/tmp/pa-h8lhgnjxh`; log:
`/private/tmp/pa-guardcheck-yahbygbl/fifth.log`. The subject 0/3/7 plus unknown-census
cases preserve both facts. Timeout, identity, interruption and receipt failure
cases reap the exact owned child. Metadata create/overwrite denial runs under the
real sandbox, and the exclusive writer rejects an actual owned symlink canary.

These remain guard-only gates. PR CI and Rust fixture verification are separate;
no product-test or full-CI success follows from this result.
