# pa-daemon

Session supervision and wire serving.

## Scope
ACP stdio transport (`acp`): the JSON-RPC serve surface for Agent
Client Protocol clients, served over a daemon session (initialize, session/new, session/prompt, session/close,
session/cancel, and the outgoing session/update notification with
namespaced `_meta` correlation), owned by this crate because
wire-protocol serving is its area; the worker turn loop owns compaction
arms, auto-refine, and goal continuation. The compact-trigger
auto-refine scheduling owns its surface here (`compact_autorefine.rs`
for the daemon worker: every compaction arm plus the manual `compact`
command arm the pa-core trigger, and the quiescent turn boundaries /
the command path consume the gated review - TS
`_scheduleAutoRefineAfterCompaction` plus the background
`_maybeAutoRefine("compact")`).
Supervisor process (one worker process per active session), restart/backoff
supervision, session registry/roster + worker self-registration (session
identity survives supervisor restarts: workers re-register with backoff and
the roster rebuilds), parent-death child cleanup (`supervisor_parent_death.rs`:
an unexpectedly exited worker's resident RLM children close with it - the
same plain-stop semantics as the worker-side #246 close, which SIGKILL
bypasses - via the durable create's `parentActiveSessionId` join), append-only session store ownership (including the
compaction-entry fold for compacted message reads), client attach/detach
(full-snapshot and chunked `session_snapshot_begin`/`chunk`/`end`
streaming), direct-attach transport (supervisor-issued single-use tickets
with a 10s TTL, worker-side peer grants burned on first use, session-plane
command gating on peer links), kernel agent_message/agent_observe bridge (`agent_messaging.rs`: the worker-side controllers over the supervisor link; the family view joins the supervisor roster with the session's own RLM children registry - the same registry `rlm.list_subagents` reads - so registry children are Child members addressable by name, RLM child id, and persisted session id, the spawning session is the Parent member for a subagent worker, and everything else stays a sibling; delivery tries the direct peer transport then the supervisor-routed `send_message`), worker-to-worker peer messaging (stage 3:
`worker`-purpose single-use grants minted by `get_worker_peer_transport`
for a source worker's kernel `agent_message.send`, direct
`worker_deliver_message` on the target worker's socket with the supervisor
routed `send_message` as the never-retried fallback),
saved-session wake
for non-resident `send_message` targets (`session_catalog.rs`: catalog
resolve by session-id prefix or exact name, cwd-scoped first; a live miss
lazily restores a matching file from `sessions-archive/` without
overwriting an existing live file; no new sessions are physically archived;
a resident worker hosting the file is reused, otherwise one spawns over it; RLM
children fall back to the spawn ledger's live child edges when the catalog
misses - children persist outside the sessions dir, under the parent's
session-artifacts tree - and wake over their own session file), wire protocol serve/negotiation (including the
`compact`/`abort_compaction`/`set_auto_compaction` commands and their
`compaction_start`/`compaction_end` events, and the live
`set_model`/`set_thinking_level` commands — `model_switch.rs`: registry
resolution, the durable `model_change`/`thinking_level_change` rows, the
settings defaults, and the engine's live agent/provider-target/level
switch), session export commands (`session_export.rs`: the worker-side
`export_html`/`export_jsonl` handlers; the HTML data carries the engine's
tools section and custom-tool pre-render through the
`SessionEngine::export_tools`/`export_rendered_tools` seams, built
lazily when an export precedes the first turn - the TS state exists from
create), the `get_model_catalog` command (the TS `refreshModelCatalog`:
registry refresh — live Prime Inference catalog fetch, disk cache at
`<agent-dir>/prime-inference-models-cache.json`, bundled fallback — then
the full catalog minus unauthorized private models plus the configured
providers; the worker create path fires the same refresh at startup), the
agent-roster arms
(`roster_subscribe`/`roster_unsubscribe` with the full snapshot, live
`roster_update` pushes keyed by the TS roster `agentId` = session id, and
authenticated `worker_roster_delta` self-reports so live status reaches
subscribers without polling) plus the live-roster ledger seed (TS
`seedRosterLedger`, `supervisor_roster.rs`: passivated ledger descendants
of resident workers seed into the live roster - roots are the resident
session files, descent is membership at any parent-walk step, and
present rows by agent id or session file are never overwritten - at
subscribe, spawn-admission, and worker-stop moments, so subscribers see
the full family, not just resident rows), cloud sandbox attach, session
leases (`core/session-lease.ts` port). Daemon-owned RLM spawn ledger (`rlm_ledger.rs`):
one append-only JSONL per sessions dir (spawn/rename/delete admissions with
the TS `rlm-ledger.ts` record grammar, bounds, stat-guarded replay, and
legacy-registry seeding) plus the per-child display files; family topology
is read from the ledger, never re-derived from session files. Passive-RLM
roster walk (`rlm_roster.rs`): `list --all` and the saved-session catalog
merge non-resident ledger children (walk roots = saved + resident session
files), so passivated children stay roster-visible (TS
`walkPassiveRlmSubagents` / `withPassiveRlmDescendantInfos`). The saved
session scan (`session_store.rs`) folds each file once into the durable
catalog row.
Supervisor-backed RLM child sessions
(`rlm_children.rs`, the daemon side of the pa-core `RlmSubagentHost` seam):
`rlm.spawn`/`rlm.create_session` create real daemon sessions through the
worker's supervisor link - one supervised worker process per child - and the
parent-side registry serves `rlm.list_subagents`/`rlm.collect`/
`rlm.delete_subagent` with TS-parity selector errors; child model resolution
and thinking-level validation live in `rlm_child_model.rs`; the create
command carries the RLM recursion identity (`rlmDepth`/`rlmMaxDepth`/
`parentSessionPath`/`thinking`) so respawned children keep their depth. Per-session model binding: the
create-config `provider`/`model`/`apiKey` are authoritative for worker model
resolution (explicit CLI flags reach the worker; env remains the no-flag
fallback). Worker session files carry the TS creation prefix
(`model_change`/`thinking_level_change`/`service_tier_change`), and queue
snapshots persist to the worker recovery journal, not the session file.
Queue-lane command surface (`queue_commands.rs`): the full TS
`DAEMON_COMMAND_TYPES` accept list with exhaustive router tables in
`protocol.rs`, plus the worker's `mutate_queued_message`/`resume_queue` arms
(`AgentSession.mutateQueuedMessage`/`resumeQueuedWork`: preview-addressed
delete/move/replace over the two lanes with the TS status vocabulary, and
the empty-queue resume refusal). The `sessionActions` queue projection
(`SessionActionSnapshot`) carries one Rust-native typed rider the TS wire
has no counterpart for: `rlmChildStatus`, the parked RLM child status
notices' lane indices, derived per item from the injected custom row (the
`rlm_child_terminal_notice`/`rlm_child_failure` kinds) at projection time —
so journal recovery re-derives it, the lane strings stay the TS
`queuedAgentMessagePreview` projection verbatim, and the condensed queue
strip folds exactly these rows (a user-typed lookalike never flags); the
rider serializes only when a notice is parked. The reserved kinds are
daemon provenance, never client data (`child_status_notices.rs`): the
prompt/steer/follow-up parse and the `restore_actions` validation answer a
caller-supplied custom row claiming one loudly — it never parks, never
folds — and the daemon's own notice injection rides the follow-up route
with a one-shot capability minted in the worker process (the wire's
optional `rlmNoticeNonce`), so the queue's classification only ever sees
daemon-authentic rows.
Prompt attachments: the `prompt`/`steer`/`follow_up` wire `images` array
(base64 payload + mime type) rides the queue item into the session engine
as multimodal user content (images on a queued prompt do not survive a
worker respawn - the recovery journal keeps the text lanes only, the TS
command-recovery shape).
Eager turn abort (TS `requestAbort`'s closing `agent.abort()`): every
worker abort surface — `abort`/`abort_and_clear_queue`, the compaction and
branch-navigation interrupt-and-settle waits, shutdown, kill, and the
`cancel_prompt_admission` cancel-owned arm — funnels through
`SessionEngine::abort_in_flight_turn` (the engine's build-time agent
mirror), so an abort landing mid-provider-wait cancels the in-flight fetch
immediately and the aborted turn settles on its zero-usage aborted message
(the usage-accounting parity of the #238 adjacent gap 3 fix; the transport
half is pa-core's provider-adapter cancellation token).
Queued-input suspension (TS `_sessionInputPumpSuspended`, the #227/#233
ruling): `abort`/`abort_and_clear_queue`/manual
`compact` suspend queued-input admission indefinitely; while suspended a
plain `prompt`/`prompt_and_wait` is rejected with the TS admission error
and the lanes park. Resume sites: a prompt carrying `streamingBehavior`,
`steer`/`follow_up`, `resume_queue` (even on the empty queue), an applied
`mutate_queued_message`, a cron/heartbeat fire, and a successful compact
with an active goal. Agent-message delivery is rejected while suspended
and idle (TS `acceptAgentMessagePrompt` runs with `resumeIfIdle: false`).
The compact-with-active-goal resume is also the TS post-compaction
continue (agent-session.ts `compact()`'s `didCompact` branch): with no
queued work parked, the worker mints the owed goal continuation through
the engine (`SessionEngine::mint_post_compaction_goal_continuation`, the
`_maybeResumeGoalContinuationAfterRlmWork` mint — the follow-up lane item
with the durable goal-context row), emits the mint's `goal_update`, then
resumes: the parked continuation crosses the suspension gate through the
resume site and the turn runner drives it (the `_schedulePostCompactionContinue`
schedule). The compact-trigger auto-refine defers behind that continuation
(TS `_compactAutoRefinePending`), servicing at its turn boundary.
Thread-goal durability (TS one-store parity): every `goal_update`
announcement mirrors the new state as a `thread_goal_state` custom row in
the worker session file (the turn emit closure; the compact mint persists
its row at the mint site, outside a turn), and the engine's session-build
adoption rehydrates the fresh goal driver from the moved branch's or the
session file's latest valid `thread_goal_state` entry
(`goal_state_persist.rs`) — status, objective, usage counters, and
continuation counts continue across a worker-recovery rebuild, and the
seeded published baseline keeps the rehydrated state from announcing
itself (TS loads at construction without emitting). Recovery branch
rebuild (TS one-store parity): the owned-session worker respawns with
`--resume <sessionFile>` (`createRpcRecoveryArgs`), so the rebuilt
session's branch is the durable history — while the Rust worker owns the
file writes and the engine keeps an in-memory manager, the engine's
session-build adoption rebuilds the branch from the worker session file
(the `pending_branch` seam's recovery twin), so a post-recovery compaction
walk (and the live loop context) reads the full durable history instead
of a fresh engine's empty branch skipping "Session is too short to
compact". Platform wall
(`platform`): per-OS endpoint naming and socket identity; the transport itself
is the shared trait in `pa_types::platform::transport`, and the private-frame
codec plus command planes are the shared wire contract in
`pa_types::daemon` (clients in pa-tui/pa-cli speak them).

Live token-stream rendering
(`streaming`): turn events forward from the hosted engine to the worker's
emit path as they arrive (one `message_update` per provider delta, the
full partial message per frame - never buffered until turn settle); the
worker coalesces them for broadcast in a single-slot coalescer with a
50ms flusher, while `message_start`/`message_end`/tool frames flush the
parked update first and go out directly, so wire order and event-sequence
order match uncoalesced streaming. The supervisor stays payload-free:
deltas ride the worker -> client session-event stream (direct-attach or
supervisor-routed).

Autonomous
continuation driving in the worker's engine: per-message usage accounting
runs in the agent-loop subscription, and the in-run continuation hook
(`autonomous_continuation.rs`) consults the pa-core `AutonomousDriver`
policy at the agent loop's natural turn end (product default: shell
quality gates in the session cwd; deterministic drivers injectable for
eval harnesses) — continuations mint as durable user rows churned
inside the one prompt wait (the TS `getContinuationMessages` shape), a
threshold compaction holds its minted continuation for the worker's
queued `followUp` admission, and gate pass/fail or limit stops surface no
row (the headless status request and the print exit contract carry
them).

Live MCP product-path verifier
(`tests/mcp_product_path_e2e.rs`): a settings-declared stdio MCP server
round-trips through a real worker session and kernel - the kernel cell's
`mcp.list_tools`/`mcp.call_tool` resolve `mcp.config` through the
session's host handlers, spawn the fixture
(`tests/fixtures/mcp_echo_server.py`), and echo back.

Update-prepare transaction (`update_prepare.rs`): the supervisor-side FSM
`Draining -> Fenced -> Snapshotted -> Prepared -> Stopping` with the
mutation-drain latch + admission gate (mutating commands refused with
"Daemon is preparing an update restart" while active; reads/attach and the
TS abort-family drain commands stay served), the hard prepare deadline
(90 s default) and the durable 45 s marker self-expiry as watchdogs — both
re-checked on a timer and on any later command — idempotent
`prepare_update_restart` on `updateId` (a different id is a typed refusal
the coordinator maps to Join), and `Aborted -> Serving` with prepared-dir
cleanup as the failure default. The prepare RPC drives accept
through the mutation drain to `Fenced`, snapshots every resident worker
(`update_snapshot`, `worker.rs`), assembles the roster (`update_roster.rs`:
sessions/workers/subagents from the ledger + displays, heartbeats as a
read-only `scheduled-jobs.json` projection), writes
`prepared/<id>/{roster,marker}.json` durably, and acks `Prepared` — all
inside the prepare budget. `commit_update_restart` consumes the
transaction (`update_stop.rs`): the acked worker `shutdown` (its handler is
the flush barrier) with the 30s + 30s budgets, abandon-on-refusal (the
supervisor resumes `Serving`, refused sessions untouched, stopped workers
relaunched — never a kill), or the update exit when all workers stopped
(descriptors survive for the new supervisor's create-or-adopt restore).
The new supervisor's boot owns the update's restore side
(`update_restore.rs`, spec §6): it consumes `PRIME_AGENT_UPDATE_ROSTER`
from its spawn env before the unconditional scratch-dir sweep (this
socket's `update-restarts/` subtree plus the TS-era legacy names — no
liveness checks, no exceptions), then restores the roster rows bottom-up
(create-or-adopt; per-session failure records, restore never fails the
boot), re-arms scheduled work (a boot scan of `scheduled-jobs.json`;
due active jobs of sessionless files are woken once, never archived), and
reports the pass over `update_restore_status` while `hello.update_resume`
carries the settle state; client attaches to a not-yet-restored roster row
queue behind the pass instead of failing.
For historical TypeScript update manifests, the restore pass recreates parents
before their children, remaps active parent IDs, and restores pending context
and action queues explicitly because those manifests predate Rust's journals.
Attaches remain gated until that replay completes.
The heartbeat-catalog change
broadcast owns its surface here too (the worker's cron-store
`on_heartbeat_change` listener emits a `heartbeats_changed` outbound frame
— TS daemon-mode's `broadcastGlobal` — and the supervisor re-broadcasts it
to every client, so any heartbeat mutation anywhere in the daemon reaches
attached UIs daemon-wide).

Side-question lifecycle (`side_question` + `side_question_store`): `/btw` runs stream through the worker, settle before close, and persist each pane as a depth-zero linked transcript under the parent artifact directory. One immutable `side_question` pointer in the parent keeps the side messages out of main context while preserving their complete paid usage. Negotiated `side_question_pane_id` groups follow-ups across reconnects; legacy pointer/transcript files resume in place.

RLM ledger namespace compatibility (#41): stable aliases converge to one
history. This includes existing symlinked sessions roots and missing macOS
`/var` roots that are later created. A validated existing legacy bucket keeps
its filename; no history is moved, copied, linked, or rehashed. Dangling or
unreadable path components refuse ledger authority. A configured sessions path
with `..` after a symlink also refuses, because its lexical and physical roots
can name different histories. Configure that path without `..` after a symlink. When an agent directory
contains RLM ledgers for multiple sessions roots, the daemon checks whether any
other ledger may contain authority for this root. A malformed, unresolved, or
over-limit other ledger does not hide the healthy selected history from display
reads, but it blocks authority reads and writes for this root. The error names
the blocking ledger. Repair it, or retire it only after verifying that it is
unrelated; the daemon never merges or deletes it automatically. Relation checks
have limits: each history read is capped at 32 MiB and 100,000 records; each
relation check is capped at 32 MiB + 400,000 work units; all checks in one
namespace operation share a cap of 4x that amount. Exceeding a cap refuses
authority and names the blocking ledger. A successful "unrelated" verdict is
cached by path, device, inode, size and modification time. No file handle is
kept between calls; each operation reopens the files and compares these values.
Writers that change a ledger within the same modification-time granularity
without changing its size are covered only for cooperating writers, which take
the namespace lock. Display reads of a healthy selected history continue while
a blocker exists. With no selected history and a blocker, display reads return
empty without an error, and authority, admission, and append refuse.

## Non-goals
No agent behavior inside workers beyond hosting a pa-core engine; no UI.

## Public API
Supervisor entrypoint, worker entrypoint, `mcp_login::{WorkerMcpLoginUi, wire_worker_mcp_login}` (the worker's browser+callback login behind `mcp.begin_login`; wired by the agent engine before sessions register host handlers), client connection API for pa-tui/pa-cli, `acp::daemon::{run_daemon_attached_acp_mode, DaemonAcpOptions}` (pa-cli dispatches `--mode acp` through it), `agent_messaging::LinkAgentMessageController` + `rlm_children::{SupervisorChildSessions, ParentIdentity, RlmChildIdentity}` (e2e verifiers construct the worker-side family controller and the children registry; the engine wires the same types), `agent_engine::AgentSessionEngine::dispose_kernel` (the session-end kernel teardown the worker invokes at kill/shutdown/orphan exit — the engine outlives the session, so the pa-core engine-drop teardown cannot run there), the worker's `get_mcp_connections` command (`mcp_connections.rs`: the `/mcp` view's roster from the session's MCP manager overlaid with the session kernel's per-server tool listing, plus the api-key credential rows the same response serves — Rust-native session-plane extension; the TS daemon has no counterpart). Supervision internals `pub(crate)`.


## Depends on
pa-types, pa-core (one-way).

The composition root initializes the packaged product version with
`protocol::configure_app_version` before entering daemon or worker mode.
`protocol::app_version` supplies that identity to both hello frames and update
checkpoints; library embedders fall back to the compiled version. Release
manifest restamping therefore remains consistent with CLI `--version`.

### Deleted-child revival boundary

An explicit RLM ledger deletion is irreversible for that child transcript path.
Launch, recovery, and adoption consult the ledger even when a stale launch record
or roster row survives. File-bound launch and deletion share a lifecycle gate.
A tombstone is keyed by `(childId, canonical child path)`. A late spawn of the
same child id at that path is refused (#42). A different child id may reuse the
path, and the same child id may start a new transcript at a new path. The
ledger itself never opens a transcript. Gates are layered: the ledger admits a
different child id at a tombstoned path, but launching or opening a worker over
any tombstoned path is still refused by the lifecycle gate, which is keyed by
the path alone. The deleted transcript is therefore not read through the reuse.
An ordinary archive keeps its non-secret launch context for user-driven revival.
Only committed deletion retires a matching launch record. Unknown or foreign
records are retained and cleanup failures are reported; no transcript is removed
by launch-record cleanup.

### Legacy registry import

Released TS v0.3.2-v0.7.2 recorded deletions only in the per-parent
`session-artifacts/<root id>/rlm-subagents.jsonl` registry. The first ledger
seed imports each deleted latest row from a claimed registry as a spawn plus a
delete record with `reason: "user"` and `source: "legacy-registry"`. The
registry has no cause field; v0.6.0-v0.7.2 cancelled releases (now `revoked`)
cannot be told apart and are approximated as `user`. Both readers treat every
reason the same for authority, and they ignore the extra field. No usage is
invented, and the deleted transcript is not opened. As in dcc and final TS, a
deleted child's own registry is not descended. Its legacy grandchildren get no
seeded edge (neither live nor tombstoned) unless another walked registry or a
sessions-root file names them; no grandchild tombstone is inferred.

A root whose first line is not JSON, or has no nonempty string `id`, claims no
registry and is skipped, as in shipped readers. A root that is not a regular
file, a root header longer than the bounded header read (shipped readers
treated it as absent; this import refuses conservatively), a path-unsafe id, a
malformed row, one child id with two paths, an I/O error or a budget overrun
refuses the seed and names the root or registry file. Until the first ledger exists, strict reads and
spawns in that namespace refuse; display reads still work through the tolerant
reader. Remedy: inspect the named registry, fix it or move it out of
`session-artifacts/`, then retry. A failed display-path seed is not memoized,
so the next read seeds after the repair. Known narrowing: dcc skipped a
malformed row and still showed that registry's other children; now display
shows no seeded legacy edges for the namespace until the registry is fixed.

The seed builds its whole payload in memory, writes a synced `seed-<pid>-<ms>`
scratch, and publishes it with a no-clobber hard link. A crash before the link
leaves only that scratch. Under the namespace lock, when no ledger exists, a
regular self-owned scratch with exactly that name comes from a dead attempt.
It is moved aside to `rlm-ledger-retained/`, never deleted, and the seed is
rebuilt. If a ledger exists and the scratch is an extra name of the same
file (a crash after publication but before cleanup), only that extra name is
unlinked; the bytes remain as the ledger. Any other entry for this ledger, a
symlink, a foreign owner, or a different file beside an existing ledger still
refuses. Scratch files of other namespaces are left for their own guard. The
retained directory is created with mode 0700 and must be owned by this user,
not a symlink, and not group- or other-writable; otherwise recovery refuses, and
retained names never overwrite each other.

### Saved-session delete

`delete_saved_session` commits the logical deletion before it removes any
file (#42). For a child session, the tombstone is appended under the
session's common fence after a strict ledger read; only an Ok append counts.
- If the commit fails, the answer is `Session delete was not committed;
  nothing was removed: <reason>` and the transcript is kept.
- If another live runtime holds the file's lease, the reason is `Another
  runtime owns this session; deletion deferred` (`Runtime ownership is
  unknown; deletion deferred` when ownership cannot be read). Earlier builds
  deleted a file that another worker still held when the request was routed
  to a different worker; this is now refused. Stop that session first. This
  differs from `Session worker is recovering; retry the delete once it is
  reachable`, which means the owning worker is temporarily unreachable.
- The file is removed after the commit, through the Trash helper: the
  installed `trash` tool, never followed by a permanent unlink if it fails;
  an explicit unlink only when no tool is installed (reported as `unlink`);
  a missing entry is never reported as trashed.
- If removal fails after the commit, the answer is the failure object plus
  `logicalDeletion: "committed"`. The child stays deleted (it cannot be
  revived) and the transcript is kept for a retry.
A top-level session has no ledger edge and keeps the plain result.
Integration tests under `crates/pa-daemon/tests/` run a production daemon, so
they must run with a fake `trash` first on PATH and the real tool denied.

A child deletion follows the same order: the tombstone commits before the
resident worker's stop is persisted. After a crash between the two, boot
adoption and registration find the tombstone and finish the stop.

### Torn ledger lines

Every ledger record is appended as one complete line, ending in a newline, and
synced while the namespace lock is held. Delete and rename callers act only
after that append returns Ok. Only Ok proves that a record committed. An error
can still follow a complete line (for example a failed sync); that errs on the
safe side, because the tombstone is present while cleanup did not run, and a
retry takes the already-deleted path. A new ledger is never created by a bare
append: its header and first record are written to a synced scratch file and
published with a no-clobber hard link, so a crash cannot leave a header-only
ledger.

A crash can leave a torn final segment after the last newline. Both readers
treat it as an unacknowledged record and skip it. Before the next append, the
writer copies those exact bytes into `rlm-ledger-retained/` (same private-
directory checks and no-clobber names as seed scratch) and syncs the copy,
then truncates the ledger to just after the last newline. If the copy fails,
the append refuses and nothing is truncated. Complete lines are never
truncated. This matches the final TS writer, so dcc, final TS and this reader
all read the result.

Authority reads apply one rule per line and keep reading, so a later line is
never hidden by an earlier bad one:
- skipped and logged with the ledger file name and line number (never the
  content): torn, non-UTF-8 or non-JSON lines, and lines with an absent
  required field;
- refused: duplicate keys, an unknown op or version, or a required field that
  is present with the wrong type or an unknown value (for example an unknown
  delete reason or an invalid usage block), because a newer binary may have
  written it.
Display reads skip all of these lines and log the file and line number.

Known limitations:
- A committed line that is corrupted later (disk damage or manual editing)
  cannot be told apart from a torn line and is also skipped.
- Ledgers written by earlier Rust builds may already contain torn fragments in
  the middle, because those builds started the next record on a new line
  instead of truncating. The skip rule still handles them, but the final TS
  reader cannot read such a ledger. That damage predates this change.
