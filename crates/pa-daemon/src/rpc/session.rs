//! The in-process RPC connection: one live engine slot, the loop-event
//! subscription that forwards raw session-event frames, and the
//! whole-session replacement drive (`new_session`/`switch_session`/`fork`).

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use pa_agent::agent::Subscription;
use pa_core::session_engine::engine::SessionEngine;
use pa_core::session_engine::provider_adapter::ProviderTarget;
use pa_core::session_engine::session_events::agent_event_json;
use pa_types::ai::Model;

use super::LineWriter;

/// One assembled engine, the live provider target its stream reads
/// (`set_model` swaps it without rebuilding), and the session lease the
/// factory acquired for the opened file (dropped on replacement).
pub struct RpcEngineHandle {
    pub engine: Arc<SessionEngine>,
    pub model: Model,
    pub api_key: Option<String>,
    pub provider_target: Arc<std::sync::RwLock<Option<ProviderTarget>>>,
    /// The built engine's background-bash wake: its completion and
    /// settle seams fire it, and the pump listener parks on it.
    pub bash_wake: Arc<tokio::sync::Notify>,
    /// The cross-process ownership lease on the opened session file
    /// (`None` only for genuinely in-memory sessions).
    pub session_lease: Option<Arc<crate::lease::SessionLease>>,
}

/// A whole-session replacement request: a fresh session (optionally under
/// a parent session) or an existing session file to open.
pub enum RpcEngineRequest {
    New {
        parent_session: Option<String>,
        /// The ACTIVE session's cwd (not the CLI startup directory); the
        /// factory falls back to the startup cwd when absent.
        cwd: Option<std::path::PathBuf>,
    },
    Open {
        session_path: PathBuf,
        /// The same-path reopen: the caller adopted the current lease, so
        /// the factory must not re-acquire (its open guard would refuse our own holder).
        reuse_lease: bool,
    },
}

/// The composition root's engine assembly; pa-cli owns the assembly
/// (cwd, model resolution, auth), not this mode.
pub type RpcEngineFactory = Arc<
    dyn Fn(
            RpcEngineRequest,
        ) -> Pin<Box<dyn Future<Output = Result<RpcEngineHandle, String>> + Send>>
        + Send
        + Sync,
>;

/// The live session state one RPC connection drives.
pub struct RpcSession {
    handle: Arc<tokio::sync::RwLock<RpcEngineHandle>>,
    writer: LineWriter,
    factory: Option<RpcEngineFactory>,
    subscription: tokio::sync::Mutex<Option<Subscription>>,
    /// Connection outputs buffered while a prompt response is pending:
    /// `Some` arms buffering, the flush emits the buffered frames in order.
    pending_outputs: Arc<tokio::sync::Mutex<Option<Vec<serde_json::Value>>>>,
    /// One replacement at a time: a fork racing a `switch_session` must not interleave.
    replacement: tokio::sync::Mutex<()>,
    mutation_admission: tokio::sync::RwLock<()>,
    /// Bumped on every whole-session replacement: pumps spawned against
    /// the replaced engine retire instead of delivering.
    pump_epoch: Arc<AtomicU64>,
    /// True for the whole span of a replacement: a pump kicked mid-swap
    /// carries a current generation and identity, so those checks cannot
    /// see it — it must not deliver onto the engine being disposed.
    replacing: Arc<AtomicBool>,
    /// Fired when a signal exit (SIGTERM/SIGHUP) begins: an in-flight
    /// replacement aborts, so the exit never queues behind a settle.
    signal_shutdown: std::sync::Arc<tokio_util::sync::CancellationToken>,
}

impl RpcSession {
    /// Adopt the first engine and subscribe its loop events.
    pub async fn adopt(
        handle: RpcEngineHandle,
        factory: Option<RpcEngineFactory>,
        writer: LineWriter,
    ) -> Self {
        let session = Self {
            handle: Arc::new(tokio::sync::RwLock::new(handle)),
            writer,
            factory,
            subscription: tokio::sync::Mutex::new(None),
            pending_outputs: Arc::new(tokio::sync::Mutex::new(None)),
            replacement: tokio::sync::Mutex::new(()),
            mutation_admission: tokio::sync::RwLock::new(()),
            pump_epoch: Arc::new(AtomicU64::new(0)),
            replacing: Arc::new(AtomicBool::new(false)),
            signal_shutdown: std::sync::Arc::new(tokio_util::sync::CancellationToken::new()),
        };
        session.resubscribe().await;
        session
    }

    pub async fn handle(&self) -> tokio::sync::RwLockReadGuard<'_, RpcEngineHandle> {
        self.handle.read().await
    }

    /// The handle for mutation (the model-selection swap: provider target,
    /// model, and api key move together under the write guard).
    pub async fn handle_mut(&self) -> tokio::sync::RwLockWriteGuard<'_, RpcEngineHandle> {
        self.handle.write().await
    }

    pub async fn set_prompt_response_pending(&self, pending: bool) {
        let mut pending_outputs = self.pending_outputs.lock().await;
        if pending && pending_outputs.is_none() {
            *pending_outputs = Some(Vec::new());
        }
    }

    /// Disarm the buffer and emit its frames in order, one step so no
    /// event slips between them; the cell stays locked until enqueued.
    pub async fn flush_connection_events(&self) {
        let mut pending_outputs = self.pending_outputs.lock().await;
        let buffered = pending_outputs.take().unwrap_or_default();
        for event in buffered {
            self.writer.write(event);
        }
    }

    /// Publish one connection output through the same buffering seam
    /// (buffers while a prompt response is pending).
    pub async fn write_connection_output(&self, frame: serde_json::Value) {
        let mut pending_outputs = self.pending_outputs.lock().await;
        if let Some(buffer) = pending_outputs.as_mut() {
            buffer.push(frame);
        } else {
            self.writer.write(frame);
        }
    }

    pub fn pump_generation(&self) -> u64 {
        self.pump_epoch.load(Ordering::SeqCst)
    }

    pub(crate) fn try_mutation_admission(
        &self,
    ) -> Result<tokio::sync::RwLockReadGuard<'_, ()>, String> {
        let guard = self
            .mutation_admission
            .try_read()
            .map_err(|_| "Session replacement is in progress; retry the command".to_string())?;
        if self.is_replacing() {
            return Err("Session replacement is in progress; retry the command".to_string());
        }
        Ok(guard)
    }

    pub fn is_replacing(&self) -> bool {
        self.replacing.load(Ordering::SeqCst)
    }

    /// Arm the replacement-in-flight gate; the returned guard clears it
    /// on drop, so every return path releases the gate exactly once.
    fn replacing_gate(&self) -> ReplacingGate<'_> {
        self.replacing.store(true, Ordering::SeqCst);
        ReplacingGate(&self.replacing)
    }

    /// Whether `engine` is the live handle's engine (pointer identity): a replace bumps the
    /// generation BEFORE swapping the handle, so the identity check closes that sampling window.
    pub async fn engine_is_live(&self, engine: &std::sync::Arc<SessionEngine>) -> bool {
        std::sync::Arc::ptr_eq(&self.handle.read().await.engine, engine)
    }

    /// Subscribe the current engine's loop events as raw session-event
    /// frames; replaces the previous subscription.
    async fn resubscribe(&self) {
        let engine = self.handle.read().await.engine.clone();
        let subscription =
            Self::engine_subscription(&engine, &self.pending_outputs, &self.writer).await;
        *self.subscription.lock().await = Some(subscription);
    }

    /// Create the loop-event subscription for one engine (the frames
    /// forward through the shared prompt-response buffer and writer).
    async fn engine_subscription(
        engine: &Arc<SessionEngine>,
        pending_outputs: &Arc<tokio::sync::Mutex<Option<Vec<serde_json::Value>>>>,
        writer: &LineWriter,
    ) -> Subscription {
        let pending_outputs = Arc::clone(pending_outputs);
        let writer = writer.clone();
        engine
            .session
            .agent()
            .subscribe(move |event, _signal| {
                let pending_outputs = Arc::clone(&pending_outputs);
                let writer = writer.clone();
                Box::pin(async move {
                    if let Some(event) = agent_event_json(&event) {
                        let mut pending_outputs = pending_outputs.lock().await;
                        if let Some(buffer) = pending_outputs.as_mut() {
                            buffer.push(event);
                        } else {
                            writer.write(event);
                        }
                    }
                    Ok(())
                })
            })
            .await
    }

    /// Retire the queued-input pumps: the signal exit paths call this
    /// the instant the abort fires, so no pump delivers before the exit.
    pub fn retire_pumps(&self) {
        self.pump_epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// Acquire the whole-session replacement lease: one replacement flow
    /// at a time; the fork path holds it across its read/branch/swap.
    pub async fn replacement_lease(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.replacement.lock().await
    }

    /// Begin the signal exit (SIGTERM/SIGHUP): in-flight replacements'
    /// settles abort and new replacements are refused.
    pub fn fire_shutdown(&self) {
        self.signal_shutdown.cancel();
    }

    pub fn shutdown_fired(&self) -> bool {
        self.signal_shutdown.is_cancelled()
    }

    /// Whole-session replacement with the lease already held by the caller.
    ///
    /// # Errors
    ///
    /// Returns the factory's assembly error when the replacement engine cannot
    /// be built (the live session stays serving).
    pub async fn replace_locked(&self, request: RpcEngineRequest) -> Result<(), String> {
        let factory = self
            .factory
            .clone()
            .ok_or_else(|| "Session switching is not wired for this RPC transport".to_string())?;
        // A signal exit has begun: never start a replacement the exit
        // would have to wait out (the 143/129 path aborts and exits).
        if self.signal_shutdown.is_cancelled() {
            return Err("A signal exit is in progress".to_string());
        }
        let mut adopted_lease = None;
        let mut request = request;
        // Retire the pumps spawned against the replaced engine BEFORE the
        // settle: a queued pump waking during the wait sees the moved epoch;
        // the in-flight gate retires a pump KICKED mid-settle.
        let _replacing = self.replacing_gate();
        // This is an operation admission gate, not the engine/registry data
        // mutex. Abort remains served while older mutations settle.
        let _mutations = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            self.mutation_admission.write(),
        )
        .await
        .map_err(|_| "Session mutations are still settling; replacement deferred".to_string())?;
        self.pump_epoch.fetch_add(1, Ordering::SeqCst);
        // Settle the running turn BEFORE the factory opens the file: the
        // turn's final rows land in the history the replacement hydrates
        // from, and its final events still reach the client.
        // Unguarded settle: an RPC `abort` arriving mid-settle must reach the agent (a guarded
        // settle would wait a provider call out).
        {
            let agent = {
                let handle = self.handle.read().await;
                std::sync::Arc::clone(handle.engine.session.agent())
            };
            let shutdown_during_settle = tokio::select! {
                () = agent.wait_for_idle() => false,
                () = self.signal_shutdown.cancelled() => true,
            };
            if shutdown_during_settle {
                return Err("A signal exit is in progress".to_string());
            }
        }
        let (old_engine, current_lease) = {
            let handle = self.handle.read().await;
            (Arc::clone(&handle.engine), handle.session_lease.clone())
        };
        if let RpcEngineRequest::Open {
            session_path,
            reuse_lease,
        } = &mut request
        {
            let canonical = crate::lease::checked_session_path(session_path)
                .map_err(|error| error.to_string())?;
            if let Some(lease) = current_lease.filter(|lease| lease.session_path == canonical) {
                // #42 fix (RECURRENCE SWEEP, site 3 of 3): this is the exact
                // scenario the lock-contract review named verbatim ("RPC
                // same-path replacement transfers the existing lease and
                // sets reuse_lease"). `revalidate()` alone never re-reads
                // the deletion ledger - a tombstone committed while this
                // session held the lease must still be caught before this
                // reuse is allowed.
                lease
                    .revalidate_and_check_admission()
                    .map_err(|error| format!("{error:#}"))?;
                adopted_lease = Some(lease);
                *reuse_lease = true;
            }
        }
        // Build the replacement after the settle: a failed assembly
        // leaves the live session serving; its lease never left the old handle.
        let mut replacement = {
            let built = tokio::select! {
                built = factory(request) => built,
                () = self.signal_shutdown.cancelled() => {
                    return Err("A signal exit is in progress".to_string());
                }
            };
            match built {
                Ok(replacement) => replacement,
                Err(error) => {
                    return Err(error);
                }
            }
        };
        // The teardown aborts only now that the replacement exists; the
        // check re-samples the LIVE streaming state, catching a turn
        // admitted after the re-acquire.
        if old_engine.session.agent().state().await.is_streaming {
            old_engine.session.agent().abort();
            old_engine.session.agent().wait_for_idle().await;
        }
        // Subscribe the replacement BEFORE publishing the handle: a prompt
        // dispatched the instant the handle lands never drops its first events.
        let subscription =
            Self::engine_subscription(&replacement.engine, &self.pending_outputs, &self.writer)
                .await;
        if let Some(subscription) = self.subscription.lock().await.take() {
            subscription.unsubscribe().await;
        }
        // The teardown races the shutdown broadcast: a signal during the
        // disposal cancels the swap; the old handle still retains its lease.
        let disposed = tokio::select! {
            () = old_engine.dispose_kernel() => true,
            () = self.signal_shutdown.cancelled() => false,
        };
        if !disposed {
            return Err("A signal exit is in progress".to_string());
        }
        // The adopted same-path lease rides the replacement; a fresh-open
        // replacement carries the lease the factory's open guard acquired.
        if adopted_lease.is_some() {
            replacement.session_lease = adopted_lease;
        }
        if let Some(lease) = replacement.session_lease.as_ref() {
            // #42 fix (RECURRENCE SWEEP, reviewer-recommended 4th fix,
            // same day/same package): closes the narrower, bounded
            // same-shape window between the reuse-lease decision and this
            // post-factory-build publication point. Uses the same shared
            // revalidate_and_check_admission() as site 3's primary fix -
            // harmless-redundant for a freshly-acquired lease (admission
            // was already checked moments earlier), closes the gap for
            // the adopted-lease case.
            lease
                .revalidate_and_check_admission()
                .map_err(|error| format!("Replacement ownership changed: {error:#}"))?;
        }
        let mut handle = self.handle.write().await;
        if !Arc::ptr_eq(&handle.engine, &old_engine) {
            return Err("Session generation changed before replacement publication".to_string());
        }
        // The background-bash listener parks on the retired handle's wake:
        // fire it so the listener re-reads the LIVE handle (a stored
        // permit, because it may not be parked yet). Its kick against the
        // retired engine is dropped by the pump's liveness checks.
        let retired_bash_wake = Arc::clone(&handle.bash_wake);
        let retired = std::mem::replace(&mut *handle, replacement);
        drop(handle);
        drop(retired);
        retired_bash_wake.notify_one();
        *self.subscription.lock().await = Some(subscription);
        Ok(())
    }

    /// Whole-session replacement: build, settle, dispose, swap, resubscribe.
    ///
    /// # Errors
    ///
    /// Returns the factory's assembly error when the replacement engine cannot be built.
    pub async fn replace(&self, request: RpcEngineRequest) -> Result<(), String> {
        let _lease = self.replacement.lock().await;
        self.replace_locked(request).await
    }

    /// The stdin-close settle: retires the queued-input pumps, waits the
    /// running turn out, unsubscribes, and disposes the kernel. The handle
    /// retains its lease: bounded kernel disposal may leave host writers
    /// alive while the process drains its output.
    pub async fn dispose(&self) {
        // Retire the detached pumps first: none may deliver queued input
        // onto the session this settle is about to dispose.
        self.pump_epoch.fetch_add(1, Ordering::SeqCst);
        let engine = self.handle.read().await.engine.clone();
        // The settle precedes the unsubscribe: the turn's trailing frames
        // and terminal `agent_end` still stream while it settles.
        engine.session.agent().wait_for_idle().await;
        if let Some(subscription) = self.subscription.lock().await.take() {
            subscription.unsubscribe().await;
        }
        engine.dispose_kernel().await;
    }
}

/// The replacement-in-flight gate's release: clears the flag on drop so
/// every return path of `replace_locked` releases the gate exactly once.
struct ReplacingGate<'a>(&'a AtomicBool);

impl Drop for ReplacingGate<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// #42 RECURRENCE SWEEP (parent/coordinator priority item, 2026-11-01): a
/// minimal harness for `replace_locked`'s `RpcEngineRequest::Open` same-
/// path-reuse branch - the exact scenario the lock-contract review named
/// verbatim ("RPC same-path replacement transfers the existing lease and
/// sets `reuse_lease`"). Uses `LineWriter::for_test()` (a new, minimal,
/// `#[cfg(test)]`-gated constructor added alongside this test, reviewed
/// together as one unit) since the production constructor writes to the
/// real stdout and is private to this crate's own module.
#[cfg(test)]
mod same_path_reuse_tests {
    use super::*;

    /// `RpcEngineHandle.model`'s type (`pa_types::ai::Model`, the catalog/wire shape).
    fn test_model() -> pa_types::ai::Model {
        pa_types::ai::Model {
            id: "test".to_string(),
            name: "test".to_string(),
            api: "openai-completions".to_string(),
            provider: "test".to_string(),
            base_url: "https://example.invalid".to_string(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![],
            cost: pa_types::ai::ModelCost {
                input: 0.0.into(),
                output: 0.0.into(),
                cache_read: 0.0.into(),
                cache_write: 0.0.into(),
            },
            context_window: 100_000,
            max_tokens: 8_000,
            max_tokens_explicit: false,
            featured: None,
            headers: None,
            compat: None,
        }
    }

    /// `SessionEngineConfig.model`'s DIFFERENT type (`pa_agent::types::Model`,
    /// the provider-runtime shape - a distinct, similarly-named type).
    fn test_engine_model() -> pa_agent::types::Model {
        pa_agent::types::Model {
            id: "m".into(),
            name: "m".into(),
            api: "test".into(),
            provider: "test".into(),
            base_url: "http://localhost".into(),
            reasoning: false,
            cost: pa_agent::types::UsageCost::default(),
            context_window: 1_000,
            max_tokens: 100,
            max_tokens_explicit: false,
        }
    }

    async fn minimal_handle(
        cwd: &std::path::Path,
        agent_dir: &std::path::Path,
        lease: Option<Arc<crate::lease::SessionLease>>,
    ) -> RpcEngineHandle {
        let engine_model = test_engine_model();
        let provider = std::sync::Arc::new(pa_agent::scripted::ScriptedProvider::new(
            engine_model.clone(),
        ));
        provider.push_text_turn("ok");
        let engine = pa_core::session_engine::engine::create_session(
            pa_core::session_engine::engine::SessionEngineConfig {
                cwd: cwd.to_path_buf(),
                agent_dir: agent_dir.to_path_buf(),
                daemonless: true,
                model: Some(engine_model),
                stream_fn: Some(provider.stream_fn()),
                ..Default::default()
            },
        )
        .await
        .expect("minimal session engine assembles");
        RpcEngineHandle {
            engine: Arc::new(engine),
            model: test_model(),
            api_key: None,
            provider_target: Arc::new(std::sync::RwLock::new(None)),
            bash_wake: Arc::new(tokio::sync::Notify::new()),
            session_lease: lease,
        }
    }

    /// RED: demonstrates the bypass under the CURRENT, unfixed code - the
    /// same gap confirmed for `switch_session` (`worker/replacement_gate_tests.rs`)
    /// and the idempotent re-create path (`worker/create_collapse_tests.rs`).
    /// `lease.revalidate()` alone proves the lease is unreleased and
    /// identity-matched; it never re-reads the deletion ledger.
    #[tokio::test]
    async fn issue_42_rpc_same_path_open_revalidates_a_committed_tombstone() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let dir = tempfile::tempdir().unwrap();
            let agent_dir = dir.path().join("agent");
            let sessions = agent_dir.join("sessions");
            std::fs::create_dir_all(&sessions).unwrap();
            let child = sessions.join("child.jsonl");
            let parent = sessions.join("parent.jsonl");
            for (path, depth) in [(&parent, 0), (&child, 1)] {
                let mut file = crate::session_store::SessionFile::create(
                    &dir.path().to_string_lossy(), None, depth,
                );
                file.set_path(path.clone());
                file.rewrite().unwrap();
            }
            let ledger = crate::rlm_ledger::RlmSpawnLedger::new(&agent_dir, &sessions, |_| {});
            ledger
                .append_spawn(&crate::rlm_ledger::RlmSpawnInput {
                    child_id: "sub-child".into(),
                    parent: parent.to_string_lossy().into_owned(),
                    child: child.to_string_lossy().into_owned(),
                    depth: 1,
                    name: "child".into(),
                })
                .unwrap();
            let authority = crate::lease::RuntimeSessionAuthority::for_agent_dir(&agent_dir).unwrap();
            let lease = Arc::new(authority.acquire(&child).unwrap());

            let handle = minimal_handle(dir.path(), &agent_dir, Some(Arc::clone(&lease))).await;
            let writer = LineWriter::for_test();
            // The factory ignores the request's specifics - `replace_locked`
            // overwrites a fresh handle's `session_lease` with the adopted
            // lease when the same-path branch sets `reuse_lease`, so the
            // factory's own returned lease never matters for this scenario.
            let factory_cwd = dir.path().to_path_buf();
            let factory_agent_dir = agent_dir.clone();
            let factory: RpcEngineFactory = Arc::new(move |_request: RpcEngineRequest| {
                let cwd = factory_cwd.clone();
                let agent_dir = factory_agent_dir.clone();
                Box::pin(async move {
                    let engine_model = test_engine_model();
                    let provider = std::sync::Arc::new(pa_agent::scripted::ScriptedProvider::new(engine_model.clone()));
                    provider.push_text_turn("ok");
                    let engine = pa_core::session_engine::engine::create_session(
                        pa_core::session_engine::engine::SessionEngineConfig {
                            cwd,
                            agent_dir,
                            daemonless: true,
                            model: Some(engine_model),
                            stream_fn: Some(provider.stream_fn()),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                    Ok(RpcEngineHandle {
                        engine: Arc::new(engine),
                        model: test_model(),
                        api_key: None,
                        provider_target: Arc::new(std::sync::RwLock::new(None)),
                        bash_wake: Arc::new(tokio::sync::Notify::new()),
                        session_lease: None,
                    })
                })
            });
            let session = RpcSession::adopt(handle, Some(factory), writer).await;

            // An interrupted prior stop can leave this exact RPC session
            // holding a lease the ledger now says is deleted - the same
            // simulated scenario the other 2 RED tests use, outside the
            // common-fence admission protocol on purpose.
            ledger
                .append_delete("sub-child", child.to_str().unwrap(), crate::rlm_ledger::RlmLedgerDeleteReason::User)
                .unwrap();
            let before = std::fs::read(&child).unwrap();

            let result = session
                .replace_locked(RpcEngineRequest::Open { session_path: child.clone(), reuse_lease: false })
                .await;
            assert!(
                result.is_err(),
                "same-path RPC open must revalidate deletion authority, not just lease release/identity: {result:?}"
            );
            assert_eq!(std::fs::read(&child).unwrap(), before);
        })
        .await
        .expect("bounded rpc same-path reuse test");
    }

    /// RED (pre-fix)/GREEN (post-fix) for the reviewer-identified second
    /// window: a narrower, bounded-to-one-`replace_locked`-invocation race
    /// between the reuse-lease decision (fixed above) and this function's
    /// POST-factory-build publication check. A tombstone that lands while
    /// the factory is building the replacement (not before `replace_locked`
    /// is even called) must still be caught here, using the same shared
    /// `revalidate_and_check_admission()`.
    #[tokio::test]
    async fn issue_42_rpc_replacement_publication_revalidates_a_tombstone_committed_during_the_build(
    ) {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let dir = tempfile::tempdir().unwrap();
            let agent_dir = dir.path().join("agent");
            let sessions = agent_dir.join("sessions");
            std::fs::create_dir_all(&sessions).unwrap();
            let child = sessions.join("child.jsonl");
            let parent = sessions.join("parent.jsonl");
            for (path, depth) in [(&parent, 0), (&child, 1)] {
                let mut file = crate::session_store::SessionFile::create(
                    &dir.path().to_string_lossy(), None, depth,
                );
                file.set_path(path.clone());
                file.rewrite().unwrap();
            }
            let ledger = std::sync::Arc::new(crate::rlm_ledger::RlmSpawnLedger::new(&agent_dir, &sessions, |_| {}));
            ledger
                .append_spawn(&crate::rlm_ledger::RlmSpawnInput {
                    child_id: "sub-child".into(),
                    parent: parent.to_string_lossy().into_owned(),
                    child: child.to_string_lossy().into_owned(),
                    depth: 1,
                    name: "child".into(),
                })
                .unwrap();
            let authority = crate::lease::RuntimeSessionAuthority::for_agent_dir(&agent_dir).unwrap();
            let lease = Arc::new(authority.acquire(&child).unwrap());

            let handle = minimal_handle(dir.path(), &agent_dir, Some(Arc::clone(&lease))).await;
            let writer = LineWriter::for_test();
            let factory_cwd = dir.path().to_path_buf();
            let factory_agent_dir = agent_dir.clone();
            // The factory itself commits the tombstone - simulating a
            // delete landing WHILE the replacement is being built, strictly
            // AFTER the reuse-lease decision already passed (the ledger was
            // still clean at that point) but BEFORE this function's later
            // publication check runs.
            let factory_ledger = std::sync::Arc::clone(&ledger);
            let factory_child = child.clone();
            let factory: RpcEngineFactory = Arc::new(move |_request: RpcEngineRequest| {
                let cwd = factory_cwd.clone();
                let agent_dir = factory_agent_dir.clone();
                let ledger = std::sync::Arc::clone(&factory_ledger);
                let child = factory_child.clone();
                Box::pin(async move {
                    ledger
                        .append_delete("sub-child", child.to_str().unwrap(), crate::rlm_ledger::RlmLedgerDeleteReason::User)
                        .map_err(|error| error.to_string())?;
                    let engine_model = test_engine_model();
                    let provider = std::sync::Arc::new(pa_agent::scripted::ScriptedProvider::new(engine_model.clone()));
                    provider.push_text_turn("ok");
                    let engine = pa_core::session_engine::engine::create_session(
                        pa_core::session_engine::engine::SessionEngineConfig {
                            cwd,
                            agent_dir,
                            daemonless: true,
                            model: Some(engine_model),
                            stream_fn: Some(provider.stream_fn()),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                    Ok(RpcEngineHandle {
                        engine: Arc::new(engine),
                        model: test_model(),
                        api_key: None,
                        provider_target: Arc::new(std::sync::RwLock::new(None)),
                        bash_wake: Arc::new(tokio::sync::Notify::new()),
                        session_lease: None,
                    })
                })
            });
            let session = RpcSession::adopt(handle, Some(factory), writer).await;
            let before = std::fs::read(&child).unwrap();

            // The ledger is still clean when this call starts - the reuse-
            // lease decision (already fixed) correctly adopts the lease;
            // the factory then commits the tombstone DURING the build.
            let result = session
                .replace_locked(RpcEngineRequest::Open { session_path: child.clone(), reuse_lease: false })
                .await;
            assert!(
                result.is_err(),
                "a tombstone committed during the replacement build must still refuse publication: {result:?}"
            );
            assert_eq!(std::fs::read(&child).unwrap(), before);
        })
        .await
        .expect("bounded rpc mid-build tombstone test");
    }
}
