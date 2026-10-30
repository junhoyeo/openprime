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
  permitted except port 8787 and additional `--protect-port` values. The
  mDNSResponder socket exception is removed, and its Mach service is denied.
  Unix sockets are permitted only under the owned run root.
- This is **not** a claim that every local service is inaccessible. An unrelated
  local proxy on a non-protected port could relay traffic. Do not use proxy
  fixtures to bypass the policy. Cached/system name resolution may still occur;
  name resolution is not permission to connect to a denied destination.
- `/bin/ps` is the sole `no-sandbox` executable exception: macOS otherwise refuses
  this setuid system process inspector. It cannot execute arbitrary commands.
  Process inspection errors fail closed instead of reporting zero survivors.
- `--timeout` bounds the command (default 1800 seconds). The wrapper records its
  child PID/start evidence, reaps that exact child on timeout, and reports other
  survivors. It never signals discovered PIDs or deletes evidence. The caller
  must inspect and clean up only proven-owned descendants.

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
