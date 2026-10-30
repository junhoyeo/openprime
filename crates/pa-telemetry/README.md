# pa-telemetry

Modular telemetry: the event schema, queueing/batching, sinks, and the
pseudonymous installation identity for Prime Agent product analytics.

## Scope

- `Properties`: primitive-only property maps (the privacy boundary — strings,
  numbers, booleans, null; structured values are rejected at insertion).
- `TelemetryEvent`: `{id, name, timestamp, properties}` records.
- `TelemetryClient`: non-blocking `track(event, properties)`, `flush`,
  `shutdown`. One background worker, capped queue (drop-oldest), batched
  flush (size + interval + explicit), fan-out to every sink. Best-effort by
  contract: never blocks, never panics, never fails the agent.
- `TelemetrySink` trait + shipped sinks: `AnalyticsSink` (the one product
  destination: the TS endpoint `ANALYTICS_ENDPOINT` and wire format
  `{installation_id, events: [{id, name, timestamp, properties}]}`, no
  credentials, 1.5s timeout; the platform backend forwards to PostHog),
  `FileSink` (local JSONL transparency mirror at `<agentDir>/telemetry.jsonl`),
  `NoopSink` (opt-out fast path), `MockSink` (tests, also re-exported for
  downstream crate tests).
- `install_id`: `<agentDir>/telemetry.json` `{version, installationId}`,
  exclusive 0600 create, validated on load (TS parity).
- `rename_onto`: the shared rename-onto-destination primitive for durable
  persist writes (TS `renameOntoSync`: bounded win32 destination-busy
  retry, `10ms * attempt` backoff, every failure immediate off Windows).
  Lives in this crate because every persist owner (pa-core's settings and
  session writes, pa-daemon's descriptors/journals/session store, this
  crate's install id) already depends on it while it depends on no other
  workspace crate; pa-core re-exports it as `platform::rename_onto` so its
  platform wall stays the engine's single platform entry.
- The TS boolean env parsing (`parse_bool_override`); the opt-out
  precedence (`PI_OFFLINE`, `DO_NOT_TRACK`, `PRIME_AGENT_TELEMETRY`,
  settings) lives in pa-core's `telemetry_switch`.

## Non-goals

- No settings reading: the enabled/disclosure flow is owned by pa-core
  settings + pa-cli wiring (this crate only answers "does the environment
  override?").
- No event emission from this crate: seams live in pa-core/pa-daemon/pa-cli,
  which call `track` with schema-documented events.
- No prompt/session content, tool arguments, file paths, or user identity —
  by construction (primitives-only properties).
- No delivery or exactly-once guarantee: failed batches use bounded best-effort
  retries. An uncertain or partially accepted failure can produce duplicate
  receipts; exhausted attempts are dropped.

## Public API

- `TelemetryClient` / `TelemetryClientConfig` (`track`, `flush`, `shutdown`,
  `dropped_count`, `install_id`, `spawn`)
- `RestrictionToken` (`unrestricted`, `restricted`, `restrict`, `is_restricted`)
- `TelemetryClient::track_scoped(name, properties, &restriction)`
- `TelemetrySink` trait, `SinkOutcome`
- `Properties`, `TelemetryEvent`
- `install_id(agent_dir)`
- `rename_onto(from, to)`
- `parse_bool_override(value)`, `existing_install_id(agent_dir)`
- Sinks: `AnalyticsSink` (+ `ANALYTICS_ENDPOINT`), `FileSink`, `NoopSink`, `MockSink`
  (+ `RecordedBatch`)
- `AnalyticsSink::new(endpoint)` and `with_timeout(endpoint, duration)` retain
  their default HTTP clients. `with_http_builder(endpoint, builder)` lets the
  caller supply its trust configuration while retaining the product timeout;
  pa-core composition supplies the shared extra-CA builder without adding a
  workspace dependency to this leaf crate.

## Placement

Depends on no workspace crate (sits beside pa-types at the bottom of the
dependency graph). Consumers: pa-core, pa-daemon, pa-cli.

## Admission-scoped delivery restrictions

Attributed work uses `track_scoped` with its admission's `RestrictionToken`.
The immutable event properties and a clone of that exact token travel together
through the worker command, each sink queue, and retries. Clones share one atomic
restriction; there is no reset. New unrestricted work requires a distinct token.
Unscoped `track` remains available for genuinely global events. Callers own the
attribution audit; adding this API alone does not make those callers private.

`restrict()` is a short, nonblocking, sequentially consistent atomic store. Reads
are sequentially consistent too. A restriction before delivery begins suppresses
the event, including on explicit flush and shutdown. Delivery begins at the final
restriction check when the worker selects that event for a sink batch. There is no
await between selection and the sink invocation. The check is repeated for every
sink and retry; it does not look up a mutable current-session token. Events whose
delivery has already begun cannot be recalled, including a sink's in-flight await.
No mutex is held over a sink await, and callers never await the worker to restrict.

The global `enabled` callback remains an additional gate; an unrestricted token
cannot enable telemetry. Suppressed items increment the existing dropped counter,
produce no telemetry of their own, and never cause empty sink calls. Existing
best-effort flush/retry behavior and sink timeout responsibility are unchanged.
