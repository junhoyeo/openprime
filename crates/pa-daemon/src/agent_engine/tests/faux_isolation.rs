//! A late request from a failed test must not consume another engine's script.
use super::*;
use crate::engine::{BranchSummaryOutcome, BranchSummaryRequest};

fn summarize(engine: &AgentSessionEngine) -> String {
    let request = BranchSummaryRequest {
        entries: vec![pa_types::session::FileEntry::Message {
            message: pa_types::session::AgentMessage::User(pa_types::ai::UserMessage {
                content: pa_types::ai::UserContent::Text(
                    "summarize this owned fixture".to_string(),
                ),
                timestamp: 0,
                rest: Map::default(),
            }),
            base: pa_types::session::EntryBase {
                id: Some("fixture-message".to_string()),
                parent_id: None,
                timestamp: Some("2024-01-01T00:00:00.000Z".to_string()),
                rest: Map::default(),
            },
        }],
        custom_instructions: None,
        replace_instructions: false,
        session_id: "owned-faux-fixture".to_string(),
    };
    match engine.run_branch_summary(request, &pa_agent::abort::AbortController::new().signal()) {
        BranchSummaryOutcome::Complete { run } => run.summary,
        outcome => panic!("the scripted provider request failed: {outcome:?}"),
    }
}

#[test]
fn a_late_request_from_an_abandoned_script_cannot_drain_the_next_engine() {
    // Serialization alone did not help: an old engine/runtime can survive the
    // failing test and issue its request after the next script is registered.
    let _faux = FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (abandoned, _old_dir) = faux_engine_with_settings(
        &json!({"responses": ["the abandoned engine reply"]}),
        20_000,
    );
    abandoned.resolve_model().expect("the old script registers");
    let (next, _new_dir) =
        faux_engine_with_settings(&json!({"responses": ["the next engine reply"]}), 20_000);
    next.resolve_model().expect("the next script registers");

    // These are actual SessionEngine -> branch summarizer -> provider calls,
    // not equality checks on names or a test helper returning its input. The
    // ordering reproduces the failed-test cascade deterministically, even with
    // one test thread, without a timeout or a leaked background task.
    let old_reply = summarize(&abandoned);
    assert_eq!(
        old_reply,
        "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\nthe abandoned engine reply"
    );
    assert_eq!(
        summarize(&next),
        "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\nthe next engine reply"
    );
}

#[test]
fn concurrent_model_resolution_keeps_one_script_queue_per_engine() {
    let _faux = FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (engine, _dir) = faux_engine_with_settings(
        &json!({"responses": ["first scoped reply", "second scoped reply"]}),
        20_000,
    );
    let apis = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| engine.resolve_model().expect("script model").api))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert!(apis.iter().all(|api| api == &apis[0]));
    assert_eq!(
        summarize(&engine),
        "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\nfirst scoped reply"
    );
    assert_eq!(
        summarize(&engine),
        "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\nsecond scoped reply"
    );
}

#[test]
fn dropping_one_script_engine_removes_only_its_private_registration() {
    let _faux = FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (first, _first_dir) = faux_engine_with_settings(&json!({"responses": ["first"]}), 20_000);
    let (second, _second_dir) =
        faux_engine_with_settings(&json!({"responses": ["surviving reply"]}), 20_000);
    let first_api = first.resolve_model().unwrap().api;
    let second_api = second.resolve_model().unwrap().api;
    assert_ne!(first_api, second_api);
    assert!(pa_ai::registry::get_api_provider(&first_api).is_some());
    drop(first);
    assert!(pa_ai::registry::get_api_provider(&first_api).is_none());
    assert!(pa_ai::registry::get_api_provider(&second_api).is_some());
    assert_eq!(
        summarize(&second),
        "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\nsurviving reply"
    );
    drop(second);
    assert!(pa_ai::registry::get_api_provider(&second_api).is_none());
}

#[test]
fn a_scripted_faux_catalog_alias_keeps_the_engine_private_queue() {
    let _faux = FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (engine, dir) = faux_engine_with_settings(&json!({"responses": ["alias reply"]}), 20_000);
    std::fs::write(
        dir.path().join("agent/models.json"),
        json!({"providers": {"faux": {
            "api": "faux", "baseUrl": "http://localhost:0", "apiKey": "synthetic-faux-key",
            "models": [{"id": "faux-alias", "name": "Faux alias", "contextWindow": 128_000, "maxTokens": 16_384}]
        }}}).to_string(),
    ).unwrap();
    let script = engine.resolve_model().unwrap();
    engine.configure_model(EngineModelSelection {
        provider: Some("faux".to_string()),
        model: Some("faux-alias".to_string()),
        api_key: None,
        thinking: None,
    });
    let alias = engine.resolve_model().unwrap();
    assert_eq!(
        (
            alias.provider.as_str(),
            alias.id.as_str(),
            alias.api.as_str()
        ),
        ("faux", "faux-alias", script.api.as_str())
    );
    assert_eq!(
        summarize(&engine),
        "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\nalias reply"
    );
}
