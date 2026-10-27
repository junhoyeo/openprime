//! The heartbeat-catalog fetch for the agents-view rows' `◷ N` badges.
use super::{mpsc, DaemonClient, DaemonCommand, UiInput};

/// The heartbeat-catalog fetch (TS `refreshHeartbeats` over
/// `listDaemonHeartbeats`): the selector-less `heartbeats_list` the
/// activity dock reads. A landed catalog re-enters the loop tagged with
/// its generation; a failed fetch keeps the last catalog (the dock's
/// stale-while-revalidate) and the next `heartbeats_changed` retries.
pub(super) fn spawn_heartbeat_catalog_fetch(
    client: &DaemonClient,
    ui_tx: mpsc::UnboundedSender<UiInput>,
    generation: u64,
) {
    let client = client.clone();
    tokio::spawn(async move {
        let accepts_partial = client.supports_server_capability("heartbeat_partial_errors");
        let mut rest = serde_json::Map::default();
        if accepts_partial {
            rest.insert(
                "acceptPartialErrors".to_string(),
                serde_json::Value::Bool(true),
            );
        }
        let request = DaemonCommand::HeartbeatsList {
            id: None,
            active_session_id: None,
            rest,
        };
        let update = match client.request_ok(request).await {
            Ok(data) => UiInput::HeartbeatsLoaded {
                generation,
                heartbeats: Some(crate::heartbeats_picker::parse_heartbeats(&data)),
                error: crate::heartbeats_picker::heartbeat_worker_error(&data),
            },
            Err(error) => UiInput::HeartbeatsLoaded {
                generation,
                heartbeats: None,
                error: Some(format!(
                    "Heartbeat catalog refresh failed; showing stale badges: {error:#}"
                )),
            },
        };
        let _ = ui_tx.send(update);
    });
}
