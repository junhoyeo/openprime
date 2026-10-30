//! The telemetry client: non-blocking `track`, background queue + batch flush.
//! `track()` never blocks, never panics, and never fails the agent: events
//! flow over an unbounded FIFO channel to a single background task that
//! flushes when a batch fills, on the interval, or on `flush()`/`shutdown()`.
//! Every event is normalized through [`crate::catalog::sanitize`] before any sink sees it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::catalog::sanitize;
use crate::event::TelemetryEvent;
use crate::properties::Properties;
use crate::restriction::RestrictionToken;
use crate::sink::{SinkOutcome, TelemetrySink};

pub const DEFAULT_BATCH_SIZE: usize = 20;
pub const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_secs(10);
pub const DEFAULT_QUEUE_CAPACITY: usize = 256;
/// The batch byte cap (wire-body estimate).
pub const DEFAULT_MAX_BATCH_BYTES: usize = 30_000;
/// Retention: older entries expire on the next flush.
pub const MAX_AGE: Duration = Duration::from_hours(24);

/// The bounded delivery retry policy.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total send attempts per entry before it expires (1 = no retry).
    pub max_attempts: u32,
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            max_backoff: Duration::from_secs(60),
        }
    }
}

/// One queued event per sink channel.
struct QueueEntry {
    event: TelemetryEvent,
    restriction: Option<RestrictionToken>,
    attempts: u32,
}

/// One sink's delivery channel: its own bounded queue and backoff state, so
/// a retried batch never re-delivers to a sink that already accepted it.
struct Channel {
    sink: Arc<dyn TelemetrySink>,
    queue: VecDeque<QueueEntry>,
    /// When the next retry may run (`None` = no failed batch waiting).
    next_retry_at: Option<tokio::time::Instant>,
}

/// Client configuration.
#[derive(Clone)]
pub struct TelemetryClientConfig {
    /// Pseudonymous installation id (the analytics body's
    /// `installation_id`). Load via [`crate::install_id`].
    pub install_id: String,
    /// Base properties merged under every event's own properties
    /// (version, os, execution mode...).
    pub base_properties: Properties,
    pub batch_size: usize,
    pub flush_interval: Duration,
    /// In-memory queue cap per sink; oldest events drop on overflow.
    pub queue_capacity: usize,
    /// The batch byte cap (wire-body estimate).
    pub max_batch_bytes: usize,
    /// The bounded retry policy.
    pub retry: RetryPolicy,
    /// Fan-out sinks: every sink receives every event.
    pub sinks: Vec<Arc<dyn TelemetrySink>>,
    /// The live on/off switch, asked for every tracked event and before
    /// every delivery pass: while it answers false, new and queued events
    /// are dropped unsent (a mid-session opt-out applies without a
    /// restart). `None` is always on.
    pub enabled: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

impl TelemetryClientConfig {
    pub fn new(install_id: impl Into<String>) -> Self {
        Self {
            install_id: install_id.into(),
            base_properties: Properties::new(),
            batch_size: DEFAULT_BATCH_SIZE,
            flush_interval: DEFAULT_FLUSH_INTERVAL,
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            max_batch_bytes: DEFAULT_MAX_BATCH_BYTES,
            retry: RetryPolicy::default(),
            sinks: Vec::new(),
            enabled: None,
        }
    }
}

// Sinks and RetryPolicy have no Debug surface; the config rows above are the debug surface.
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for TelemetryClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryClientConfig")
            .field("install_id", &self.install_id)
            .field("base_properties", &self.base_properties)
            .field("batch_size", &self.batch_size)
            .field("flush_interval", &self.flush_interval)
            .field("queue_capacity", &self.queue_capacity)
            .field("max_batch_bytes", &self.max_batch_bytes)
            .field("sinks", &self.sinks.len())
            .finish()
    }
}

/// The client handle. `Clone` shares one background worker.
#[derive(Clone)]
pub struct TelemetryClient {
    tx: mpsc::UnboundedSender<Cmd>,
    /// Events dropped (queue overflow, worker gone, entry expired).
    dropped: Arc<AtomicU64>,
    /// Copy of the config base properties so `track` merges lock-free.
    base_properties: Properties,
    install_id: String,
}

enum Cmd {
    Track(TelemetryEvent, Option<RestrictionToken>),
    Flush(oneshot::Sender<()>),
    Shutdown(oneshot::Sender<()>),
}

impl TelemetryClient {
    /// A client that counts every track as dropped. Fallback for
    /// environments without a tokio runtime (telemetry must never fail the
    /// caller or pretend events were sent).
    #[must_use]
    pub fn inert() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        Self {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
            base_properties: Properties::new(),
            install_id: "inert".to_string(),
        }
    }

    /// Spawn the background worker.
    ///
    /// # Errors
    ///
    /// Returns an error only when there is no tokio runtime on the current thread.
    pub fn spawn(mut config: TelemetryClientConfig) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let dropped = Arc::new(AtomicU64::new(0));
        let base_properties = config.base_properties.clone();
        let install_id = config.install_id.clone();
        let sinks = std::mem::take(&mut config.sinks);
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|error| anyhow::anyhow!("no tokio runtime on this thread: {error}"))?;
        runtime.spawn(
            Worker {
                channels: sinks
                    .into_iter()
                    .map(|sink| Channel {
                        sink,
                        queue: VecDeque::new(),
                        next_retry_at: None,
                    })
                    .collect(),
                config,
                queue_dropped: Arc::clone(&dropped),
                rx,
            }
            .run(),
        );
        Ok(Self {
            tx,
            dropped,
            base_properties,
            install_id,
        })
    }

    /// Enqueue an event. The config base properties are merged under the
    /// event properties. Never blocks; if the worker is gone the event is
    /// dropped and counted.
    // Workspace API consumed across crates (pa-cli, pa-core); the by-value signature is fleet-wide.
    #[allow(clippy::needless_pass_by_value)]
    pub fn track(&self, name: impl Into<String>, properties: Properties) {
        self.capture(name, &properties, None);
    }

    /// Enqueue immutable properties for one admission. The same token identity
    /// must follow the event through queueing, retry, and every sink delivery.
    // Keep the same owned-property API as `track`; capture merges it immediately.
    #[allow(clippy::needless_pass_by_value)]
    pub fn track_scoped(
        &self,
        name: impl Into<String>,
        properties: Properties,
        restriction: &RestrictionToken,
    ) {
        if restriction.is_restricted() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.capture(name, &properties, Some(restriction.clone()));
    }

    fn capture(
        &self,
        name: impl Into<String>,
        properties: &Properties,
        restriction: Option<RestrictionToken>,
    ) {
        let mut merged = self.base_properties.clone();
        merged.merge(properties);
        let event = TelemetryEvent::new(name, merged);
        if self.tx.send(Cmd::Track(event, restriction)).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Drain and flush everything tracked so far. Returns when the batches
    /// have been handed to every sink (or dropped by their policy).
    ///
    /// # Errors
    ///
    /// Returns an error when the worker stopped before the flush finished.
    pub async fn flush(&self) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Cmd::Flush(tx)).is_err() {
            return Ok(());
        }
        rx.await
            .map_err(|_| anyhow::anyhow!("telemetry worker stopped before flush"))
    }

    /// Flush once and stop the worker. Subsequent `track` calls are counted as
    /// dropped.
    ///
    /// # Errors
    ///
    /// Returns an error when the worker stopped before the shutdown handshake completed.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Cmd::Shutdown(tx)).is_err() {
            return Ok(());
        }
        rx.await
            .map_err(|_| anyhow::anyhow!("telemetry worker stopped before shutdown"))
    }

    /// Events dropped so far (queue overflow / worker gone / expiry).
    #[must_use]
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// The installation id this client reports as.
    #[must_use]
    pub fn install_id(&self) -> &str {
        &self.install_id
    }
}

/// Background worker: owns the per-sink channels, batching, interval, and
/// sink fan-out.
struct Worker {
    channels: Vec<Channel>,
    config: TelemetryClientConfig,
    queue_dropped: Arc<AtomicU64>,
    rx: mpsc::UnboundedReceiver<Cmd>,
}

impl Worker {
    async fn run(mut self) {
        let mut next_deadline = tokio::time::Instant::now() + self.config.flush_interval;
        loop {
            tokio::select! {
                cmd = self.rx.recv() => {
                    let Some(cmd) = cmd else {
                        // Every client handle is gone: one final best-effort drain so a one-shot
                        // client's events are not lost.
                        self.flush_final().await;
                        break;
                    };
                    match cmd {
                        Cmd::Track(event, restriction) => {
                            // An event recorded while the switch is off is
                            // dropped now, so turning telemetry back on
                            // never sends what happened while it was off.
                            if self.drop_while_disabled()
                                || restriction.as_ref().is_some_and(RestrictionToken::is_restricted)
                            {
                                self.queue_dropped.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            self.enqueue(&event, restriction.as_ref());
                            if self.channels.iter().any(|channel| {
                                channel.queue.len() >= self.config.batch_size
                                    && channel.ready_for_send(tokio::time::Instant::now())
                            }) {
                                self.flush_pass().await;
                                next_deadline = self.next_deadline();
                            }
                        }
                        Cmd::Flush(tx) => {
                            self.flush_pass().await;
                            let _ = tx.send(());
                            next_deadline = self.next_deadline();
                        }
                        Cmd::Shutdown(tx) => {
                            // The final drain never waits on a backoff and
                            // never retries: bounded shutdown, best effort.
                            self.flush_final().await;
                            let _ = tx.send(());
                            break;
                        }
                    }
                }
                () = tokio::time::sleep_until(next_deadline) => {
                    self.flush_pass().await;
                    next_deadline = self.next_deadline();
                }
            }
        }
    }

    /// The next flush deadline: the base interval, stretched by the largest
    /// pending retry backoff, so a failing channel never request-storms.
    fn next_deadline(&self) -> tokio::time::Instant {
        let now = tokio::time::Instant::now();
        let max_attempts = self
            .channels
            .iter()
            .flat_map(|channel| channel.queue.iter().map(|entry| entry.attempts))
            .max()
            .unwrap_or(0);
        let mut delay = self.config.flush_interval;
        if max_attempts > 0 {
            let backoff = self
                .config
                .flush_interval
                .saturating_mul(1 << max_attempts.min(16));
            delay = delay.max(backoff.min(self.config.retry.max_backoff));
        }
        now + delay
    }

    /// Ask the live switch; when it is off, drop every queued event
    /// (counted) and report true so the pass sends nothing.
    fn drop_while_disabled(&mut self) -> bool {
        if self.config.enabled.as_ref().is_none_or(|enabled| enabled()) {
            return false;
        }
        for channel in &mut self.channels {
            self.queue_dropped
                .fetch_add(channel.queue.len() as u64, Ordering::Relaxed);
            channel.queue.clear();
            channel.next_retry_at = None;
        }
        true
    }

    fn enqueue(&mut self, event: &TelemetryEvent, restriction: Option<&RestrictionToken>) {
        for channel in &mut self.channels {
            if channel.queue.len() >= self.config.queue_capacity {
                channel.queue.pop_front();
                self.queue_dropped.fetch_add(1, Ordering::Relaxed);
            }
            channel.queue.push_back(QueueEntry {
                event: event.clone(),
                restriction: restriction.cloned(),
                attempts: 0,
            });
        }
    }

    /// One delivery pass over every channel: expire, batch, send, retry.
    async fn flush_pass(&mut self) {
        if self.drop_while_disabled() {
            return;
        }
        let now = std::time::SystemTime::now();
        let now_epoch_ms = now
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis() as u64);
        for channel in &mut self.channels {
            channel.expire(
                now_epoch_ms,
                self.config.retry.max_attempts,
                &self.queue_dropped,
            );
            if !channel.ready_for_send(tokio::time::Instant::now()) {
                continue;
            }
            channel.next_retry_at = None;
            let mut retry_after: Option<u32> = None;
            while !channel.queue.is_empty() {
                let entries = channel.take_batch(&self.config, &self.queue_dropped);
                if entries.is_empty() {
                    continue;
                }
                let mut events: Vec<TelemetryEvent> =
                    entries.iter().map(|entry| entry.event.clone()).collect();
                for event in &mut events {
                    sanitize(&event.name, &mut event.properties);
                }
                let outcome = channel
                    .sink
                    .send_batch(&self.config.install_id, events)
                    .await;
                match outcome {
                    SinkOutcome::Sent => {}
                    SinkOutcome::Dropped => {
                        // Requeue (front, in order) with attempts+1; the
                        // attempt cap expires the entries on a later pass.
                        let max_attempts_after = entries
                            .iter()
                            .map(|entry| entry.attempts + 1)
                            .max()
                            .unwrap_or(1);
                        for entry in entries.into_iter().rev() {
                            channel.queue.push_front(QueueEntry {
                                event: entry.event,
                                restriction: entry.restriction,
                                attempts: entry.attempts + 1,
                            });
                        }
                        retry_after = Some(max_attempts_after);
                        break;
                    }
                }
            }
            if let Some(attempts) = retry_after {
                channel.next_retry_at =
                    Some(tokio::time::Instant::now() + self.config.retry_backoff(attempts));
            }
        }
    }

    /// The shutdown drain: every channel, one pass, no retry bookkeeping
    /// (bounded shutdown; a hard exit may lose reports).
    async fn flush_final(&mut self) {
        if self.drop_while_disabled() {
            return;
        }
        for channel in &mut self.channels {
            while !channel.queue.is_empty() {
                let entries = channel.take_batch(&self.config, &self.queue_dropped);
                if entries.is_empty() {
                    continue;
                }
                let mut events: Vec<TelemetryEvent> =
                    entries.into_iter().map(|entry| entry.event).collect();
                for event in &mut events {
                    sanitize(&event.name, &mut event.properties);
                }
                let _ = channel
                    .sink
                    .send_batch(&self.config.install_id, events)
                    .await;
            }
        }
    }
}

impl Channel {
    /// Start delivery for this batch. These checks are the linearization point:
    /// restriction after an entry's check cannot recall that in-flight delivery.
    /// There is no await between these checks and the sink invocation. Repeated
    /// for every sink, retry, and final drain, including after another sink awaits.
    fn take_batch(
        &mut self,
        config: &TelemetryClientConfig,
        dropped: &AtomicU64,
    ) -> Vec<QueueEntry> {
        if config.enabled.as_ref().is_some_and(|enabled| !enabled()) {
            dropped.fetch_add(self.queue.len() as u64, Ordering::Relaxed);
            self.queue.clear();
            self.next_retry_at = None;
            return Vec::new();
        }
        let mut entries = Vec::new();
        let mut bytes = 0;
        while let Some(entry) = self.queue.front() {
            if entry
                .restriction
                .as_ref()
                .is_some_and(RestrictionToken::is_restricted)
            {
                self.queue.pop_front();
                dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let size = entry.event.wire_size_estimate();
            if !entries.is_empty()
                && (entries.len() >= config.batch_size || bytes + size > config.max_batch_bytes)
            {
                break;
            }
            bytes += size;
            if let Some(entry) = self.queue.pop_front() {
                entries.push(entry);
            }
        }
        entries
    }

    /// True when the channel may send now (`next_retry_at` passed or no
    /// failed batch waiting).
    fn ready_for_send(&self, now: tokio::time::Instant) -> bool {
        self.next_retry_at.is_none_or(|at| now >= at)
    }

    /// Drop entries past the attempt cap or age; count them as dropped.
    fn expire(&mut self, now_epoch_ms: u64, max_attempts: u32, dropped: &AtomicU64) {
        let before = self.queue.len();
        self.queue.retain(|entry| {
            entry.attempts < max_attempts
                && now_epoch_ms.saturating_sub(entry.event.timestamp_ms)
                    < MAX_AGE.as_millis() as u64
        });
        let expired = before - self.queue.len();
        if expired > 0 {
            dropped.fetch_add(expired as u64, Ordering::Relaxed);
        }
    }
}

impl TelemetryClientConfig {
    /// The backoff after `attempts` failed sends:
    /// `min(max_backoff, flush_interval * 2^attempts)`.
    fn retry_backoff(&self, attempts: u32) -> Duration {
        self.flush_interval
            .saturating_mul(1 << attempts.min(16))
            .min(self.retry.max_backoff)
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::properties::Properties;
    use crate::{MockSink, RecordedBatch};

    fn client_with(
        mock: Arc<MockSink>,
        batch_size: usize,
        flush_interval: Duration,
    ) -> TelemetryClient {
        let mut config = TelemetryClientConfig::new("install-1");
        config.batch_size = batch_size;
        config.flush_interval = flush_interval;
        config.sinks = vec![mock as Arc<dyn TelemetrySink>];
        TelemetryClient::spawn(config).expect("spawn client")
    }

    #[test]
    fn defaults_match_the_2117_delivery_contract() {
        assert_eq!(DEFAULT_BATCH_SIZE, 20);
        assert_eq!(DEFAULT_QUEUE_CAPACITY, 256);
        assert_eq!(DEFAULT_MAX_BATCH_BYTES, 30_000);
        assert_eq!(MAX_AGE, Duration::from_hours(24));
        assert_eq!(RetryPolicy::default().max_attempts, 5);
        assert_eq!(RetryPolicy::default().max_backoff, Duration::from_secs(60));
    }

    #[test]
    fn retry_backoff_is_interval_scaled_and_capped() {
        let mut config = TelemetryClientConfig::new("install-1");
        config.flush_interval = Duration::from_secs(10);
        assert_eq!(config.retry_backoff(1), Duration::from_secs(20));
        assert_eq!(config.retry_backoff(2), Duration::from_secs(40));
        assert_eq!(config.retry_backoff(3), Duration::from_secs(60), "capped");
        assert_eq!(config.retry_backoff(9), Duration::from_secs(60), "capped");
    }

    #[tokio::test]
    async fn batch_size_triggers_flush() {
        let mock = Arc::new(MockSink::new());
        let client = client_with(mock.clone(), 3, Duration::from_secs(60));
        for i in 0..2 {
            client.track(format!("event {i}"), Properties::new());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(mock.batches().len(), 0, "below batch size, no flush yet");
        client.track("event 2", Properties::new());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(mock.event_names(), vec!["event 0", "event 1", "event 2"]);
        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn explicit_flush_drains_partial_batches() {
        let mock = Arc::new(MockSink::new());
        let client = client_with(mock.clone(), 10, Duration::from_secs(60));
        client.track("solo", Properties::new());
        assert_eq!(mock.batches().len(), 0);
        client.flush().await.unwrap();
        assert_eq!(mock.event_names(), vec!["solo"]);
        assert_eq!(mock.batches()[0].install_id, "install-1");
        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn interval_flushes_without_batch_fill() {
        let mock = Arc::new(MockSink::new());
        let client = client_with(mock.clone(), 100, Duration::from_millis(30));
        client.track("timer event", Properties::new());
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(mock.event_names(), vec!["timer event"]);
        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_flushes_and_stops() {
        let mock = Arc::new(MockSink::new());
        let client = client_with(mock.clone(), 100, Duration::from_secs(60));
        client.track("last", Properties::new());
        client.shutdown().await.unwrap();
        assert_eq!(mock.event_names(), vec!["last"]);
        client.track("after shutdown", Properties::new());
        assert_eq!(client.dropped_count(), 1);
        assert_eq!(mock.event_names(), vec!["last"]);
    }

    #[tokio::test]
    async fn base_properties_merge_under_event_properties() {
        let mock = Arc::new(MockSink::new());
        let mut config = TelemetryClientConfig::new("install-1");
        config.batch_size = 1;
        config.flush_interval = Duration::from_secs(60);
        config.sinks = vec![mock.clone() as Arc<dyn TelemetrySink>];
        let mut base = Properties::new();
        base.set("version", serde_json::Value::from("0.1.0"));
        base.set("shared", serde_json::Value::from("base"));
        config.base_properties = base;
        let client = TelemetryClient::spawn(config).unwrap();
        let mut properties = Properties::new();
        properties.set("shared", serde_json::Value::from("event"));
        properties.set("extra", serde_json::Value::from(1));
        client.track("merged", properties);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let events = mock.events();
        assert_eq!(
            events[0].properties.get("version"),
            Some(&serde_json::Value::from("0.1.0"))
        );
        assert_eq!(
            events[0].properties.get("shared"),
            Some(&serde_json::Value::from("event"))
        );
        assert_eq!(
            events[0].properties.get("extra"),
            Some(&serde_json::Value::from(1))
        );
        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn queue_cap_drops_oldest_per_channel() {
        let mock = Arc::new(MockSink::new());
        let mut config = TelemetryClientConfig::new("install-1");
        config.batch_size = 1000; // never self-flush; queue only
        config.flush_interval = Duration::from_secs(60);
        config.queue_capacity = 4;
        config.sinks = vec![mock.clone() as Arc<dyn TelemetrySink>];
        let client = TelemetryClient::spawn(config).unwrap();
        for i in 0..10 {
            client.track(format!("e{i}"), Properties::new());
        }
        // Let the worker drain the channel before inspecting the drop counter.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(client.dropped_count(), 6);
        client.flush().await.unwrap();
        assert_eq!(mock.event_names(), vec!["e6", "e7", "e8", "e9"]);
        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn fan_out_delivers_to_every_sink() {
        let a = Arc::new(MockSink::new());
        let b = Arc::new(MockSink::new());
        let mut config = TelemetryClientConfig::new("install-1");
        config.batch_size = 1;
        config.flush_interval = Duration::from_secs(60);
        config.sinks = vec![
            a.clone() as Arc<dyn TelemetrySink>,
            b.clone() as Arc<dyn TelemetrySink>,
        ];
        let client = TelemetryClient::spawn(config).unwrap();
        client.track("fanned", Properties::new());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(a.event_names(), vec!["fanned"]);
        assert_eq!(b.event_names(), vec!["fanned"]);
        client.shutdown().await.unwrap();
    }

    /// A sink that fails `fail_times` batches, then succeeds.
    struct FlakySink {
        shared: Arc<MockSink>,
        remaining: AtomicU32,
    }

    impl FlakySink {
        fn new(shared: Arc<MockSink>, fail_times: u32) -> Arc<Self> {
            Arc::new(Self {
                shared,
                remaining: AtomicU32::new(fail_times),
            })
        }
    }

    impl TelemetrySink for FlakySink {
        fn send_batch<'a>(
            &'a self,
            install_id: &'a str,
            events: Vec<TelemetryEvent>,
        ) -> Pin<Box<dyn Future<Output = SinkOutcome> + Send + 'a>> {
            Box::pin(async move {
                if self
                    .remaining
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                        remaining.checked_sub(1)
                    })
                    .is_ok()
                {
                    return SinkOutcome::Dropped;
                }
                self.shared.send_batch(install_id, events).await
            })
        }
    }

    #[tokio::test]
    async fn failed_batch_retries_after_backoff_and_delivers_once() {
        let delivered = Arc::new(MockSink::new());
        let mut config = TelemetryClientConfig::new("install-1");
        config.batch_size = 10;
        config.flush_interval = Duration::from_millis(10);
        config.sinks = vec![FlakySink::new(delivered.clone(), 2) as Arc<dyn TelemetrySink>];
        let client = TelemetryClient::spawn(config).unwrap();
        client.track("offline event", Properties::new());
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            delivered.event_names(),
            vec!["offline event"],
            "delivered exactly once after two failed attempts"
        );
        assert_eq!(client.dropped_count(), 0);
        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn permanently_failing_channel_expires_without_duplicating_sibling() {
        let failing = Arc::new(MockSink::failing());
        let healthy = Arc::new(MockSink::new());
        let mut config = TelemetryClientConfig::new("install-1");
        config.batch_size = 10;
        config.flush_interval = Duration::from_millis(10);
        config.sinks = vec![
            failing as Arc<dyn TelemetrySink>,
            healthy.clone() as Arc<dyn TelemetrySink>,
        ];
        let client = TelemetryClient::spawn(config).unwrap();
        client.track("survives", Properties::new());
        // The failure/retry/expiry cycle completes well inside this window (backoffs
        // 20+40+80+160ms).
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(
            healthy.event_names(),
            vec!["survives"],
            "the healthy channel saw the event exactly once"
        );
        assert!(
            client.dropped_count() >= 1,
            "the failing channel expired its entry"
        );
        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn batch_byte_cap_splits_batches() {
        let mock = Arc::new(MockSink::new());
        let mut config = TelemetryClientConfig::new("install-1");
        config.batch_size = 20;
        config.flush_interval = Duration::from_secs(60);
        config.max_batch_bytes = 1_000;
        config.sinks = vec![mock.clone() as Arc<dyn TelemetrySink>];
        let client = TelemetryClient::spawn(config).unwrap();
        for i in 0..3 {
            let mut properties = Properties::new();
            properties.set("padding", serde_json::Value::from("x".repeat(600)));
            client.track(format!("fat {i}"), properties);
        }
        client.flush().await.unwrap();
        let batches: Vec<RecordedBatch> = mock.batches();
        assert_eq!(batches.len(), 3, "each fat event ships alone");
        assert_eq!(mock.event_names(), vec!["fat 0", "fat 1", "fat 2"]);
        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn aged_entries_expire() {
        let mut channel = Channel {
            sink: Arc::new(MockSink::new()),
            queue: VecDeque::new(),
            next_retry_at: None,
        };
        let fresh = TelemetryEvent {
            id: "fresh-id".into(),
            name: "fresh".into(),
            timestamp_ms: now_epoch_ms_for_tests(),
            properties: Properties::new(),
        };
        let stale = TelemetryEvent {
            id: "stale-id".into(),
            name: "stale".into(),
            timestamp_ms: now_epoch_ms_for_tests().saturating_sub(MAX_AGE.as_millis() as u64 + 1),
            properties: Properties::new(),
        };
        channel.queue.push_back(QueueEntry {
            event: fresh,
            restriction: None,
            attempts: 0,
        });
        channel.queue.push_back(QueueEntry {
            event: stale,
            restriction: None,
            attempts: 0,
        });
        let dropped = Arc::new(AtomicU64::new(0));
        channel.expire(now_epoch_ms_for_tests(), 5, &dropped);
        assert_eq!(channel.queue.len(), 1);
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        assert_eq!(channel.queue[0].event.name, "fresh");
    }

    #[tokio::test]
    async fn capped_attempts_expire() {
        let mut channel = Channel {
            sink: Arc::new(MockSink::new()),
            queue: VecDeque::new(),
            next_retry_at: None,
        };
        channel.queue.push_back(QueueEntry {
            event: TelemetryEvent::new("exhausted", Properties::new()),
            restriction: None,
            attempts: 5,
        });
        let dropped = Arc::new(AtomicU64::new(0));
        channel.expire(now_epoch_ms_for_tests(), 5, &dropped);
        assert!(channel.queue.is_empty());
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
    }

    fn now_epoch_ms_for_tests() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn flush_sanitizes_events_against_the_catalog() {
        let mock = Arc::new(MockSink::new());
        let client = client_with(mock.clone(), 1, Duration::from_secs(60));
        let mut properties = Properties::new();
        properties.set(
            "session_id",
            serde_json::Value::from("0197d0a0-8f5c-7f2a-b0e3-2d7e0d2b3b1a"),
        );
        properties.set("trigger", serde_json::Value::from("spontaneous"));
        properties.set("secret_path", serde_json::Value::from("/home/user/project"));
        client.track("agent run completed", properties);
        client.flush().await.unwrap();
        let events = mock.events();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].properties.get("trigger"),
            Some(&serde_json::Value::from("unknown")),
            "out-of-vocabulary trigger fell back"
        );
        assert!(
            events[0].properties.get("secret_path").is_none(),
            "uncatalogued property dropped at the sink boundary"
        );
        client.shutdown().await.unwrap();
    }
    /// Records actual sink invocations and blocks only the sentinel batch.
    struct GatedSink {
        recorded: Arc<MockSink>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        gone: Arc<tokio::sync::Notify>,
        outcome: SinkOutcome,
        blocked: std::sync::atomic::AtomicBool,
    }

    impl Drop for GatedSink {
        fn drop(&mut self) {
            self.gone.notify_one();
        }
    }

    impl TelemetrySink for GatedSink {
        fn send_batch<'a>(
            &'a self,
            install_id: &'a str,
            events: Vec<TelemetryEvent>,
        ) -> Pin<Box<dyn Future<Output = SinkOutcome> + Send + 'a>> {
            Box::pin(async move {
                let sentinel = events.iter().any(|event| event.name == "sentinel");
                self.recorded.send_batch(install_id, events).await;
                if sentinel && !self.blocked.swap(true, Ordering::SeqCst) {
                    self.entered.notify_one();
                    self.release.notified().await;
                }
                self.outcome
            })
        }
    }

    #[tokio::test]
    async fn issues_35_36_restrictions_follow_capture_through_worker_and_final_drain() {
        tokio::time::timeout(Duration::from_secs(5), async {
            for finish in ["flush", "shutdown", "drop"] {
                let recorded = Arc::new(MockSink::new());
                let entered = Arc::new(tokio::sync::Notify::new());
                let release = Arc::new(tokio::sync::Notify::new());
                let gone = Arc::new(tokio::sync::Notify::new());
                let directory = tempfile::tempdir().unwrap().keep();
                let mirror = crate::FileSink::new(&directory);
                let mirror_path = mirror.path().to_path_buf();
                let mut config = TelemetryClientConfig::new("synthetic");
                config.batch_size = 1;
                config.flush_interval = Duration::from_hours(1);
                config.enabled = Some(Arc::new(|| true));
                config.sinks = vec![
                    Arc::new(GatedSink {
                        recorded: recorded.clone(),
                        entered: entered.clone(),
                        release: release.clone(),
                        gone: gone.clone(),
                        outcome: SinkOutcome::Sent,
                        blocked: std::sync::atomic::AtomicBool::new(false),
                    }),
                    Arc::new(mirror),
                ];
                let client = TelemetryClient::spawn(config).unwrap();
                client.track("sentinel", Properties::new());
                entered.notified().await;
                let private = crate::RestrictionToken::restricted();
                client.track_scoped("private at capture", Properties::new(), &private);
                let pending = crate::RestrictionToken::unrestricted();
                client.track_scoped("restricted in queue", Properties::new(), &pending);
                pending.clone().restrict();
                let fresh = crate::RestrictionToken::unrestricted();
                client.track_scoped("fresh public", Properties::new(), &fresh);
                release.notify_one();
                match finish {
                    "flush" => {
                        client.flush().await.unwrap();
                        client.shutdown().await.unwrap();
                    }
                    "shutdown" => client.shutdown().await.unwrap(),
                    "drop" => drop(client),
                    _ => unreachable!(),
                }
                gone.notified().await;
                eprintln!("synthetic telemetry receipt: {}", mirror_path.display());
                let receipt = tokio::fs::read_to_string(&mirror_path).await.unwrap();
                let rows: Vec<serde_json::Value> = receipt
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(
                    rows.iter()
                        .map(|row| row["name"].as_str().unwrap())
                        .collect::<Vec<_>>(),
                    ["sentinel", "fresh public"],
                    "actual FileSink receipt: {finish}"
                );
                assert_eq!(
                    recorded.event_names(),
                    ["sentinel", "fresh public"],
                    "{finish}"
                );
                assert_eq!(recorded.batches().len(), 2, "no empty sink calls");
            }
        })
        .await
        .expect("worker drains without waiting on suppressed items");
    }

    #[tokio::test]
    async fn issues_35_36_recheck_between_sinks_and_on_retry_or_final_drain() {
        tokio::time::timeout(Duration::from_secs(5), async {
            for finish in ["flush", "shutdown", "drop"] {
                let first = Arc::new(MockSink::new());
                let second = Arc::new(MockSink::new());
                let entered = Arc::new(tokio::sync::Notify::new());
                let release = Arc::new(tokio::sync::Notify::new());
                let gone = Arc::new(tokio::sync::Notify::new());
                let mut config = TelemetryClientConfig::new("synthetic");
                config.flush_interval = Duration::from_hours(1);
                config.sinks = vec![Arc::new(GatedSink {
                    recorded: first.clone(), entered: entered.clone(),
                    release: release.clone(), gone: gone.clone(), outcome: SinkOutcome::Dropped,
                    blocked: std::sync::atomic::AtomicBool::new(false),
                }), second.clone()];
                let client = TelemetryClient::spawn(config).unwrap();
                let token = crate::RestrictionToken::unrestricted();
                client.track_scoped("sentinel", Properties::new(), &token);
                {
                    let flushing = client.flush();
                    tokio::pin!(flushing);
                    tokio::select! {
                        () = entered.notified() => {}
                        result = &mut flushing => panic!("flush finished before release: {result:?}"),
                    }
                    token.restrict();
                    release.notify_one();
                    flushing.await.unwrap();
                }
                match finish {
                    "flush" => { client.flush().await.unwrap(); client.shutdown().await.unwrap(); }
                    "shutdown" => client.shutdown().await.unwrap(),
                    "drop" => {}
                    _ => unreachable!(),
                }
                drop(client);
                gone.notified().await;
                assert_eq!(first.event_names(), ["sentinel"], "in-flight call is not recalled");
                assert!(second.batches().is_empty(), "restriction checked after first sink awaits");
            }
        }).await.expect("restricted retries and final drain complete");
    }

    #[tokio::test]
    async fn issues_35_36_global_policy_still_restricts_fresh_tokens() {
        let recorded = Arc::new(MockSink::new());
        let mut config = TelemetryClientConfig::new("synthetic");
        config.enabled = Some(Arc::new(|| false));
        config.sinks = vec![recorded.clone()];
        let client = TelemetryClient::spawn(config).unwrap();
        client.track_scoped(
            "public token",
            Properties::new(),
            &crate::RestrictionToken::unrestricted(),
        );
        tokio::time::timeout(Duration::from_secs(5), client.shutdown())
            .await
            .unwrap()
            .unwrap();
        assert!(recorded.batches().is_empty());
    }

    struct FailOnceRecordingSink {
        recorded: Arc<MockSink>,
        failed: std::sync::atomic::AtomicBool,
        delivered: tokio::sync::Notify,
    }

    impl TelemetrySink for FailOnceRecordingSink {
        fn send_batch<'a>(
            &'a self,
            install_id: &'a str,
            events: Vec<TelemetryEvent>,
        ) -> Pin<Box<dyn Future<Output = SinkOutcome> + Send + 'a>> {
            Box::pin(async move {
                self.recorded.send_batch(install_id, events).await;
                if self.failed.swap(true, Ordering::SeqCst) {
                    self.delivered.notify_one();
                    SinkOutcome::Sent
                } else {
                    SinkOutcome::Dropped
                }
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn issues_35_36_scheduled_retry_keeps_public_event_and_drops_restricted_event() {
        tokio::time::timeout(Duration::from_secs(60), async {
            let recorded = Arc::new(MockSink::new());
            let sink = Arc::new(FailOnceRecordingSink {
                recorded: recorded.clone(),
                failed: std::sync::atomic::AtomicBool::new(false),
                delivered: tokio::sync::Notify::new(),
            });
            let mut config = TelemetryClientConfig::new("synthetic");
            config.flush_interval = Duration::from_secs(10);
            config.sinks = vec![sink.clone()];
            let client = TelemetryClient::spawn(config).unwrap();
            let private = crate::RestrictionToken::unrestricted();
            let public = crate::RestrictionToken::unrestricted();
            client.track_scoped("private retry", Properties::new(), &private);
            let mut properties = Properties::new();
            properties.set("count", serde_json::Value::from(1));
            client.track_scoped("public retry", properties.clone(), &public);
            properties.set("count", serde_json::Value::from(2));
            client.flush().await.unwrap();
            let initial = recorded.batches();
            assert_eq!(initial.len(), 1);
            assert_eq!(initial[0].events.len(), 2);
            private.restrict();
            // Only the worker's scheduled timer can deliver the retry: no
            // flush/shutdown command is sent until its successful receipt.
            tokio::time::advance(Duration::from_secs(20)).await;
            tokio::time::timeout(Duration::from_secs(1), sink.delivered.notified())
                .await
                .expect("eligible public retry delivered by scheduled timer");
            let attempts = recorded.batches();
            assert_eq!(attempts.len(), 2);
            assert_eq!(attempts[1].events, vec![initial[0].events[1].clone()]);
            assert_eq!(
                attempts[1].events[0].properties.get("count"),
                Some(&serde_json::Value::from(1))
            );
            client.shutdown().await.unwrap();
            assert_eq!(
                recorded.batches().len(),
                2,
                "accepted public retry is not replayed"
            );
        })
        .await
        .expect("scheduled retry is bounded");
    }

    #[tokio::test]
    async fn issues_35_36_global_opt_out_during_sink_await_gates_later_sink() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let first = Arc::new(MockSink::new());
            let second = Arc::new(MockSink::new());
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let gone = Arc::new(tokio::sync::Notify::new());
            let on = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let switch = on.clone();
            let mut config = TelemetryClientConfig::new("synthetic");
            config.flush_interval = Duration::from_hours(1);
            config.enabled = Some(Arc::new(move || switch.load(Ordering::SeqCst)));
            config.sinks = vec![
                Arc::new(GatedSink {
                    recorded: first.clone(),
                    entered: entered.clone(),
                    release: release.clone(),
                    gone: gone.clone(),
                    outcome: SinkOutcome::Sent,
                    blocked: std::sync::atomic::AtomicBool::new(false),
                }),
                second.clone(),
            ];
            let client = TelemetryClient::spawn(config).unwrap();
            client.track_scoped(
                "sentinel",
                Properties::new(),
                &crate::RestrictionToken::unrestricted(),
            );
            {
                let flushing = client.flush();
                tokio::pin!(flushing);
                tokio::select! {
                    () = entered.notified() => {}
                    result = &mut flushing => panic!("flush finished before release: {result:?}"),
                }
                on.store(false, Ordering::SeqCst);
                release.notify_one();
                flushing.await.unwrap();
            }
            assert_eq!(
                first.event_names(),
                ["sentinel"],
                "in-flight delivery cannot be recalled"
            );
            assert!(
                second.batches().is_empty(),
                "later sink must recheck global policy"
            );
            on.store(true, Ordering::SeqCst);
            client.track_scoped(
                "fresh public",
                Properties::new(),
                &crate::RestrictionToken::unrestricted(),
            );
            client.shutdown().await.unwrap();
            gone.notified().await;
            assert_eq!(first.event_names(), ["sentinel", "fresh public"]);
            assert_eq!(
                second.event_names(),
                ["fresh public"],
                "fresh enabled work still emits"
            );
        })
        .await
        .expect("global policy transition is bounded");
    }
}
