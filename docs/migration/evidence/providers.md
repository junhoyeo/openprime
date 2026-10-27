# Provider lane evidence snapshot

> Reviewed source: `origin/port/rust-providers@89d9310592c640f6cdb5ef666122c2f12eef7931`.
> Integrated correction tip includes provider tree through `b77676cda` plus safe metadata producer net `454e79ff2` on `feat/rust-migration`.
> Combined hermetic evidence: metadata secret/capability projection 1/1, Fast consumer 2/2, production four-builder public eligibility 1/1, raw-epoch transform 18/18. Provider-owned paths byte-match reviewed source. Full workspace/native acceptance remains separate.

# OpenPrime Rust Provider/Model/Auth Port — Adversarial Review

**Review target:** `origin/port/rust-providers` `befb4c26a4c9bfe0d7b3aa86dc1bb18224108b9c`
**Base:** `c4c238ca025e382d96fb5f37e6328ca77fb82ef3`
**Verdict: BLOCK**

## Scope and method

- Created a fresh detached review worktree at `/Users/junhoyeo/prime-agent-worktrees/review-rust-providers`; source tree was clean apart from pre-existing unrelated untracked `.omc/` content in the shared checkout.
- Read root `AGENTS.md`, changed-crate READMEs (`pa-ai`, `pa-core`, `pa-daemon`, `pa-types`), all 26 changed paths/diffs, and relevant producer seam at existing session worktree commit `b4445ed1788374a091870198c3cc5d49fbe0572b`.
- No product source, auth/settings file, daemon, skill, gateway, remote branch, or commit was mutated. All provider evidence uses synthetic/loopback test code only.
- Native focused test started with `CARGO_BUILD_JOBS=2` and private `CARGO_TARGET_DIR=/Users/junhoyeo/prime-agent-worktrees/review-rust-providers/.review-target`; result recorded below when terminal.

## Ship blockers

### P1 — interrupted cross-turn repeated tool-call IDs corrupt tool-result ownership

**Location:** `crates/pa-ai/src/providers/transform_messages.rs`, `append_real_tool_result`, and the main transform loop's user lookahead/ToolResult handling (new code around 125 and 260–350).

**Reproduction:**

1. assistant turn A emits tool call `{ id: "X", name: "a" }`.
2. A user interruption/notice is persisted.
3. assistant turn B emits tool call `{ id: "X", name: "b" }`.
4. A delayed result for A `{ tool_call_id: "X", tool_name: "a" }` arrives.
5. B's real result `{ tool_call_id: "X", tool_name: "b" }` arrives.

The state is keyed only by `tool_call_id`. Once B becomes pending, step 4 passes the ID-only check and becomes B's result. Its ID enters `existing_tool_result_ids`, so step 5 is discarded as a duplicate. The model receives A's result attributed to B. The existing assistant-boundary test uses distinct IDs, so it cannot detect this.

**Minimal remedy:** own results by an assistant/call epoch rather than a global ID; user-boundary lookahead must not claim through a later assistant epoch. Reject mismatched stable tool names. For same-id/same-name ambiguity, implement/document a fail-closed policy or carry reliable provenance; do not silently select one. Add red cases for repeated IDs, exact retransmission vs conflicting duplicate, late orphans, missing synthetic placeholder, sequential reuse, and idempotence.

### P1 — Anthropic schema normalization is not semantics-preserving for valid JSON Schema property names

**Location:** `crates/pa-ai/src/providers/anthropic/convert.rs`, `normalize_anthropic_tool_schema`.

When a schema has a sole catch-all `patternProperties["^.*$"]`, the function replaces it with `additionalProperties`. This is not equivalent in JSON Schema: `.` does not match a newline, while `additionalProperties` applies to unmatched newline-containing property names. The implementation skips conversion where `properties` is nonempty, but the conversion remains unsound for allowed root/nested record schemas.

**Validated reproduction:** installed `jsonschema` `Draft202012Validator` evaluated:

```json
{
  "type": "object",
  "patternProperties": {"^.*$": {"type": "string"}},
  "additionalProperties": {"maxLength": 2}
}
```

For a normal property `{ "x": "long" }`, both source and emitted schema allow it. (The earlier review claim that matching `patternProperties` and `additionalProperties` conjoin was wrong and is retracted.) But for newline-bearing names:

- `{ "a\nb": "long" }`: source **rejects** (`additionalProperties.maxLength`), emitted schema **accepts** (string).
- `{ "a\nb": 7 }`: source **accepts** (`maxLength` does not constrain a number), emitted schema **rejects** (string required).

Without an explicit `additionalProperties`, source default semantics accept any newline-key value, while the emitted schema requires a string. Parent independently obtained the same results with an isolated `jsonschema` Draft 2020-12 probe (exit 0). That probe also demonstrates the general named-property distinction (`properties.x: number` plus `patternProperties ^.*$: string`, `x=1`: source invalid, normalized valid); current code's nonempty-properties guard happens to retain that particular shape. The new test does not cover valid-key divergence.

**Minimal remedy:** preserve native `input_schema` unless Anthropic demonstrably rejects `patternProperties`; document that requirement if it exists. Do not add `allOf` based on the retracted conjunction premise. If normalization remains necessary, establish an actually supported equivalent for all JSON property names and add validator-backed edge cases.

### P1 — public-key selection trusts mutable custom model data and can send `public` to an arbitrary endpoint

**Location:** `crates/pa-ai/src/models.rs`, `is_opencode_public_model`, and its registry/transport callers.

The predicate trusts only `provider == "opencode" | "opencode-go"` and `model.cost.input == 0.0`. A user-controlled `models.json` can define a custom model under provider `opencode`, omit `apiKey` (cost defaults to zero), and select an arbitrary `baseUrl`. It becomes explicitly selectable and fully-qualified RLM-resolvable despite no auth, then every transport sends the literal public fallback value (`Bearer public` / equivalent header) to the configured URL. `public` is not a secret and this is **not** a credential-exfiltration claim; owners intentionally configure custom endpoints. The proven issue is policy/provenance: mutable custom/unknown catalog data may self-declare eligibility for automatic keyless public fallback, contrary to *verified public Zen only*.

Separately, a model with input `0` but output/cache pricing nonzero is still called zero cost. `resolve_set_model_selection`, `can_use_model`, and `get_api_key_and_headers` all rely on this predicate. The current paid regression changes only input cost and misses output/cache-only charging and custom-source provenance.

**Minimal remedy:** define a narrow trusted eligibility contract, such as catalog provenance plus an authoritative public/free marker and known/validated zero for every billed dimension. Unknown/defaulted custom cost values must not prove free eligibility. Do not infer it from a mutable provider string/cost. Preserve supported custom gateway routes when they carry an explicitly configured credential; coordinate any origin/override policy with the owner rather than treating generic endpoint configuration as secret exposure. Add custom-provider unknown-provenance rejection, output/cache pricing, invalid/nonfinite/negative/missing data as applicable, no-auth RLM resolution, genuine free selection, and paid-with-configured-credential tests.

### P1 — proposed complete-model metadata seam is an unsafe outbound boundary and does not prove Fast mode end-to-end

**Producer:** `crates/pa-daemon/src/agent_engine/session_engine_impl.rs` at sibling session commit `b4445ed1788374a091870198c3cc5d49fbe0572b` returns `serde_json::to_value(self.resolve_model()?)`.
**Consumer:** provider tip `crates/pa-daemon/src/setting_switches.rs` deserializes metadata as `pa_types::ai::Model`.

That serializes entire resolved model data to connection-state metadata, including `baseUrl`, arbitrary model headers, and raw compat/config content. It is an outward wire boundary and must not publish endpoint query/userinfo, provider-config headers, authorization-like values, or unknown nested config. The supplied producer test only round-trips a safe synthetic model and uses `sendSessionIdHeader`, not `supportsFastMode`; the consumer test uses a manually constructed JSON `Value`, not the real engine→connection-state projection.

**Required remedy (owner direction):** replace the full `Model` metadata value with an explicit typed public allowlist retaining existing public identity/display fields and the sole needed optional `supportsFastMode` capability (preserve true/false/absent). Do not deserialize it as `Model`. Test secret URL/header sentinels and unknown parsed config absent from the actual serialized connection-state path, plus real engine true/false/absent Fast capability and the actual consumer clamp. This is a known integration seam, but is a blocker until combined producer+consumer evidence exists.

## Other review evidence

- Retry settings correctly map `retry.maxBackoffMs`: default 60,000 ms, `0` to `UNBOUNDED_BACKOFF_MS`, while legacy `retry.maxDelayMs` remains the server `Retry-After` cap. The delay function applies the agent cap after an allowed `Retry-After`, with saturation/timer cap. Focused settings/unit coverage is credible.
- The auth failure E2E tests use synthetic key data and show no apparent code path that calls destructive auth mutation on provider 401. They check unchanged auth bytes for disabled retry, recovery, and cancelled retry. They do **not** test concurrent external auth-file update, but no destructive provider-failure writer was found (`mark_auth_stale` is not called on this path); record as a coverage gap, not an independent defect.
- OpenCode request identity remains confined to OpenCode providers and adds `User-Agent: prime-agent`/session only when absent. Existing OAuth Anthropic header construction still selects its Claude identity in the OAuth branch. The loopback four-transport test is useful, subject to fixing the zero-cost predicate.
- Custom model `supportsFastMode` serde/registry propagation is locally covered. It is not integration proof until the safe daemon metadata producer and real consumer path are combined.

## Known integration caveat

The provider tip intentionally lacks the session-owner metadata producer. `b4445ed1788374a091870198c3cc5d49fbe0572b` exists in the existing sessions worktree (the SHA advertised with a longer suffix was not present locally). Its full-Model approach is itself unsafe as described above. No acceptance of the Fast-mode seam is possible until a new combined tip implements the narrow metadata boundary and has producer-to-consumer proof.

## Verification status

- `CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/Users/junhoyeo/prime-agent-worktrees/review-rust-providers/.review-target cargo test -p pa-ai transform_messages --lib`: **PASS** — 9 passed, 0 failed, 0 ignored (372 filtered), 1m08s including cold private-target build. This verifies existing tests only; it does not cover the repeated-ID blocker.
- Same private-target settings: `cargo test -p pa-ai --test opencode_session`: **PASS** — 4 passed, 0 failed, 0 ignored, 0.27s. This confirms existing four-transport loopback coverage only; it does not prove trusted public-model eligibility.
- Reported earlier branch gates were not accepted as integration proof, per review instruction.


## SHA correction

Parent verified canonical object IDs with `git rev-parse`: unsafe full-model producer is `b4445ed1788374a091870198c3cc5d49fbe0572b`; safe metadata follow-up is `dae6c891652bb50156a7dd8a5ab2192495fc5fd4`. Earlier handoff-only expansions ending `...d12...` / `...9eb...` do not exist and are not relied on by this review.


# Corrective-delta review (local committed, not yet remote-approved)

**Provider correction tip reviewed:** `f597f13f56eeb690c8c17aca32bfc9c5ca78ef87`
**Baseline:** `befb4c26a4c9bfe0d7b3aa86dc1bb18224108b9c`
**Safe producer paired for source inspection:** `dae6c891652bb50156a7dd8a5ab2192495fc5fd4`.

## New P1 — cross-model raw-ID normalization still aliases a late result into the next epoch

`8366a5b5f` correctly adds epoch-level ownership *after* the preliminary normalization, but `tool_call_id_map` in `transform_messages_with_normalizer` remains one global `HashMap<String, String>`.

Reproduction with the normalizer active:

1. assistant A is from a different model and has raw call id `X`, name `tool`; normalizer maps it to `A1`.
2. user interruption closes A; the synthetic A1 result leaves `(A1, tool)` unresolved.
3. assistant B (also transformed) reuses raw `X`, name `tool`; normalizer maps it to `B1`, overwriting global `X -> B1`.
4. A's late raw tool result `X/tool` arrives after B. The preliminary map changes it to `B1/tool` *before* the new epoch/ambiguity check.

The pending B ownership sees no unresolved `(B1, tool)` (only `(A1, tool)`) and may accept A's late output as B's. If B's real result then arrives, it conflicts, removes the incorrectly accepted one and leaves B synthetic; if B's result never arrives, A's output remains wrongly attributed to B. The added regressions use the same model/no normalizer and do not cover this path.

**Required minimal correction:** associate raw-result ID normalization with the producing assistant epoch (or otherwise avoid global last-write-wins mapping for results), before ownership comparison. Add a regression where the normalizer produces distinct `A1` / `B1` for repeated raw `X` across an interruption; prove the late A raw result remains orphaned and B's real result remains intact. **This keeps the corrective tip BLOCKED.**

## Corrective evidence accepted pending the P1 repair

- `0d3497a28` removes the lossy `patternProperties` rewrite and adds root/nested byte-preservation tests. This resolves the previously reported schema semantic rewrite.
- `4697a2a3a` uses canonical Zen route, reviewed IDs, and all-four-cost equality. The checked `pa-models` fixture contains exactly seven canonical all-zero entries matching the documented IDs; its parity test fails if a public-eligible parsed fixture entry is outside that set. This reconciles the runtime catalog fixture with the older smaller `pa-ai` generated fallback. Explicit configured keys still precede fallback. This remains subject to final pushed gate evidence.
- `dae6c891...` producer is a narrow metadata projection and its real connection-state test checks secret URL/header sentinels and true/false/absent `supportsFastMode`. `f597f13...` consumer no longer deserializes metadata as `Model` and reads only allowlisted identity/capability fields. This seam appears structurally correct, subject to final combined integration evidence.

- Corrective tip focused check: `CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/Users/junhoyeo/prime-agent-worktrees/review-rust-providers-corrective/.review-target cargo test -p pa-ai transform_messages --lib`: **PASS** (13 passed, 0 failed; cold private-target build). This does not cover the discovered cross-model stateful-normalizer bypass, so it is not acceptance evidence.

- Final raw-epoch successor `9a6912ded9bffc3ea7de06cfaf6a7597808d1511`: independent `CARGO_BUILD_JOBS=2` / private-target `cargo test -p pa-ai transform_messages --lib` **PASS** — 18 passed, 0 failed, 0 ignored; cold build 1m27s. The earlier global-normalizer P1 is covered by distinct/same-name, absent-current, valid noninterrupted, and collision cases. Acceptance still awaits required actual four-transport eligible-public boundary coverage, push, gates, and safe-producer combined proof.

- Boundary predecessor `0b9a7a540d71fa31db3f483fedf45cbd9582879f`: independent private-target `cargo test -p pa-ai public_zen_request_tests --lib` **PASS** — 1 passed, 0 failed, cold build 1m08s. It calls production-used resolved-header builders across all four transports. Successor `fd65596bdfa2b899c9f77caa54c72b290ea70ae5` only makes the existing untrusted-loopback assertion apply to Google too; source reviewed, final gate/push still pending.


# Final corrective review verdict

**Reviewed/pushed provider tip:** `89d9310592c640f6cdb5ef666122c2f12eef7931` (`origin/port/rust-providers`, verified locally with `git rev-parse`).
**Safe metadata producer reviewed:** `dae6c891652bb50156a7dd8a5ab2192495fc5fd4` (not unsafe `b4445ed1788374a091870198c3cc5d49fbe0572b` alone). `dae6...` is an ancestor of the currently advanced sessions ref; subsequent sessions commits are outside this provider review.

**Verdict: CLEAR-FOR-INTEGRATION, with a remaining combined metadata-seam caveat.**

The four original P1s are closed at the reviewed provider tip:

1. Raw assistant-epoch ownership now precedes result normalization. The exact cross-model raw-ID reuse, same-name ambiguity, absent-current-result, valid noninterrupted reuse, target collision, duplicates/orphans, and idempotence cases are covered. Independent focused run at the raw-epoch corrective ancestor passed 18/18.
2. Anthropic conversion preserves `patternProperties` and user schema keywords. Root/nested schema preservation replaces the invalid lossy normalization; no allOf/conjunction workaround was added.
3. Public Zen fallback requires the documented canonical route, reviewed fixture allowlist, and finite zero values for every billed dimension; untrusted/default-zero custom entries fail closed while configured custom keys prevail. The fixture parity test pins the seven reviewed entries and source/hashes. The production-used four request builders now each prove fallback, precedence, untrusted refusal, and explicit custom-key behavior. Independent boundary run passed 1/1.
4. The provider Fast-mode consumer reads only allowlisted metadata identity fields plus optional boolean `supportsFastMode`; it no longer deserializes metadata as a full `Model`. Safe producer `dae6...` emits the narrow public projection and tests no URL/header secret leakage plus true/false/absent capability across actual connection-state projection.

Final minor commits after the independently run focused tips were directly inspected: `fd65596...` expands the no-connect negative test to Google, `bfea27...` restores the lint attribute to the long function, `7d166...` improves test diagnostics, and `89d931...` reports the timeout error. They do not change behavior. The exact pushed provider ref is confirmed, and author identity through the provider correction range is Junho Yeo only.

**Caveat / integration hold outside this provider verdict:** the final integration must combine the provider consumer with safe producer `dae6...` (not b444 alone) and run the lead-owned combined producer→connection-state→consumer test and full integration gates at that combined SHA. The provider branch cannot independently prove an unmerged cross-branch product artifact. Do not represent this provider-only clearance as completion of that combined seam.
