# pa-agent

The agent loop.

## Scope
Turn loop: drives a provider stream, dispatches tool calls, enforces max-turn and sequencing semantics, handles partial streams, retries on stream failure, propagates aborts.

## Non-goals
No tool implementations (pa-core), no providers (pa-ai), no session persistence, no UI. The loop is tool-agnostic: it receives a `ToolDispatcher` boundary.

## Public API
`run_loop` (session inputs -> stream of loop events), `ToolDispatcher` trait, and `Agent` (including cloneable `convert_to_llm_fn` / `transform_context_fn` / `get_api_key_fn` host hooks for cache-compatible auxiliary loops). Loop internals `pub(crate)`.

## Depends on
No workspace dependencies.
