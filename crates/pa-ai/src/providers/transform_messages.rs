//! Message normalization shared by providers.

use std::collections::{HashMap, HashSet};

use serde_json::Map;

use crate::types::{
    AssistantContent, AssistantMessage, Message, Model, ModelExt, StopReason, TextContent,
    ToolCall, ToolResultMessage, UserMessage, UserMessageContent, UserOrToolContent,
};

const NON_VISION_USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const NON_VISION_TOOL_IMAGE_PLACEHOLDER: &str =
    "(tool image omitted: model does not support images)";

fn replace_images_with_placeholder(
    content: &[UserOrToolContent],
    placeholder: &str,
) -> Vec<UserOrToolContent> {
    let mut result: Vec<UserOrToolContent> = Vec::new();
    let mut previous_was_placeholder = false;
    for block in content {
        if let UserOrToolContent::Image(_) = block {
            if !previous_was_placeholder {
                result.push(UserOrToolContent::Text(TextContent {
                    text: placeholder.to_string(),
                    text_signature: None,
                    rest: Map::default(),
                }));
            }
            previous_was_placeholder = true;
            continue;
        }
        previous_was_placeholder =
            matches!(block, UserOrToolContent::Text(text) if text.text == placeholder);
        result.push(block.clone());
    }
    result
}

fn downgrade_unsupported_images(messages: &[Message], model: &Model) -> Vec<Message> {
    if model.supports_image_input() {
        return messages.to_vec();
    }
    messages
        .iter()
        .map(|msg| match msg {
            Message::User(user) => {
                let UserMessageContent::Blocks(blocks) = &user.content else {
                    return msg.clone();
                };
                Message::User(UserMessage {
                    content: UserMessageContent::Blocks(replace_images_with_placeholder(
                        blocks,
                        NON_VISION_USER_IMAGE_PLACEHOLDER,
                    )),
                    ..user.clone()
                })
            }
            Message::ToolResult(tool_result) => Message::ToolResult(ToolResultMessage {
                content: replace_images_with_placeholder(
                    &tool_result.content,
                    NON_VISION_TOOL_IMAGE_PLACEHOLDER,
                ),
                ..tool_result.clone()
            }),
            other @ Message::Assistant(_) => other.clone(),
        })
        .collect()
}

/// A tool call without a function name: some gateways synthesize a
/// `tool_calls` entry from a model's native tool-call template and lose the
/// name (often the id too). Nothing can execute it, and strict providers
/// reject every request that replays it (`tool_calls[N] is missing a
/// function name`), which would leave the session failing on every turn.
fn is_unnamed_tool_call(tool_call: &ToolCall) -> bool {
    tool_call.name.trim().is_empty()
}

/// Name given to the calls of a turn whose every tool call came without a function name. Strict
/// providers accept it on replay, and the agent loop answers it with a `Tool unknown_tool not found`
/// error, so the model learns its call was malformed and can retry in the same turn.
pub(crate) const UNNAMED_TOOL_CALL: &str = "unknown_tool";

/// Repair the tool calls a provider streamed without a function name in a finished assistant
/// message, before it reaches the agent loop or the session file. A nameless call beside named
/// calls is dropped: the turn goes on through the named ones. When a tool-use turn has only
/// nameless calls, they are named [`UNNAMED_TOOL_CALL`] and given a stable id instead, so the turn
/// keeps going with an error result rather than ending silently.
pub(crate) fn repair_unnamed_tool_calls(output: &mut AssistantMessage) {
    let has_named_call = output.content.iter().any(
        |block| matches!(block, AssistantContent::ToolCall(tool_call) if !is_unnamed_tool_call(tool_call)),
    );
    if !has_named_call && output.stop_reason == StopReason::ToolUse {
        let timestamp = output.timestamp;
        for (index, block) in output.content.iter_mut().enumerate() {
            if let AssistantContent::ToolCall(tool_call) = block {
                tool_call.name = UNNAMED_TOOL_CALL.to_string();
                if tool_call.id.is_empty() {
                    tool_call.id = format!("call_unnamed_{timestamp}_{index}");
                }
            }
        }
        return;
    }
    output.content.retain(
        |block| !matches!(block, AssistantContent::ToolCall(tool_call) if is_unnamed_tool_call(tool_call)),
    );
}

type Normalizer<'a> = dyn Fn(&str, &Model, &AssistantMessage) -> Option<String> + 'a;

fn append_real_tool_result(
    result: &mut Vec<Message>,
    pending_tool_calls: &[ToolCall],
    accepted_tool_results: &mut HashMap<String, ToolResultMessage>,
    ambiguous_tool_calls: &mut HashSet<(String, String)>,
    message: &Message,
) -> bool {
    let Message::ToolResult(tool_result) = message else {
        return false;
    };
    let Some(tool_call) = pending_tool_calls.iter().find(|tool_call| {
        tool_call.id == tool_result.tool_call_id
            && (tool_result.tool_name.is_empty()
                || tool_call.name.is_empty()
                || tool_call.name == tool_result.tool_name)
    }) else {
        return false;
    };
    let ownership = (tool_call.id.clone(), tool_call.name.clone());
    if ambiguous_tool_calls.contains(&ownership) {
        return false;
    }
    if let Some(previous) = accepted_tool_results.get(&tool_result.tool_call_id) {
        if previous == tool_result {
            return false;
        }
        if let Some(index) = result.iter().rposition(
            |message| matches!(message, Message::ToolResult(candidate) if candidate == previous),
        ) {
            result.remove(index);
        }
        accepted_tool_results.remove(&tool_result.tool_call_id);
        ambiguous_tool_calls.insert(ownership);
        return false;
    }
    accepted_tool_results.insert(tool_result.tool_call_id.clone(), tool_result.clone());
    result.push(message.clone());
    true
}

/// Tool-call ID normalization plus synthetic tool results for unanswered calls, dropping errored
/// assistant turns (TS `transformMessages`). Unnamed tool calls already stored in a session (written
/// before the stream dropped them) are stripped here, and their results go with them as orphans, so
/// a session poisoned by one replays again.
// Long by design: mirrors the provider's stream shape.
#[allow(clippy::too_many_lines)]
pub fn transform_messages_with_normalizer(
    messages: &[Message],
    model: &Model,
    normalize_tool_call_id: &Normalizer<'_>,
) -> Vec<Message> {
    // Raw-id ownership is resolved before normalization. A positional tool result can only inherit
    // the normalized id of its owning assistant epoch; it never takes the most recent global map.
    let mut pending_raw_calls: Vec<(String, String, String)> = Vec::new();
    let mut unresolved_raw_calls: Vec<(String, String, String)> = Vec::new();
    let mut normalized_id_owners: HashMap<String, String> = HashMap::new();
    let mut colliding_normalized_ids: HashSet<String> = HashSet::new();
    let image_aware = downgrade_unsupported_images(messages, model);

    // `None` marks a dropped unnamed-only assistant turn: it still ends the previous turn's pending
    // calls, so its own results cannot be replayed against an earlier call that reused the id.
    let transformed: Vec<Option<Message>> = image_aware
        .iter()
        .map(|msg| match msg {
            Message::User(_) => Some(msg.clone()),
            Message::ToolResult(tool_result) => {
                let owns_result = |call: &(String, String, String)| {
                    call.0 == tool_result.tool_call_id
                        && (tool_result.tool_name.is_empty()
                            || call.1.is_empty()
                            || call.1 == tool_result.tool_name)
                };
                if let Some(index) = pending_raw_calls.iter().position(owns_result) {
                    if unresolved_raw_calls.iter().any(owns_result) {
                        // The same raw id/name is unresolved in an earlier assistant epoch. The
                        // wire has no provenance to distinguish the late result from the current
                        // one, so preserve it unmatched and synthesize the current result later.
                        return Some(msg.clone());
                    }
                    let (_, _, normalized) = pending_raw_calls.remove(index);
                    if normalized != tool_result.tool_call_id {
                        return Some(Message::ToolResult(ToolResultMessage {
                            tool_call_id: normalized,
                            ..tool_result.clone()
                        }));
                    }
                    return Some(msg.clone());
                }

                let candidates: HashSet<&str> = unresolved_raw_calls
                    .iter()
                    .filter(|call| owns_result(call))
                    .map(|call| call.2.as_str())
                    .collect();
                if candidates.len() == 1 {
                    let normalized = candidates.iter().next().expect("one candidate");
                    if *normalized != tool_result.tool_call_id {
                        return Some(Message::ToolResult(ToolResultMessage {
                            tool_call_id: (*normalized).to_string(),
                            ..tool_result.clone()
                        }));
                    }
                }
                Some(msg.clone())
            }
            Message::Assistant(assistant) => {
                unresolved_raw_calls.extend(pending_raw_calls.drain(..));
                let tracks_tool_calls = !matches!(
                    assistant.stop_reason,
                    StopReason::Error | StopReason::Aborted
                );
                let is_same_model = assistant.provider == model.provider
                    && assistant.api == model.api
                    && assistant.model == model.id;

                let transformed_content: Vec<AssistantContent> = assistant
                    .content
                    .iter()
                    .flat_map(|block| match block {
                        AssistantContent::Thinking(thinking) => {
                            let redacted = thinking.redacted.unwrap_or(false);
                            if redacted {
                                return if is_same_model {
                                    vec![block.clone()]
                                } else {
                                    vec![]
                                };
                            }
                            if is_same_model && thinking.thinking_signature.is_some() {
                                return vec![block.clone()];
                            }
                            if thinking.thinking.trim().is_empty() {
                                return vec![];
                            }
                            if is_same_model {
                                return vec![block.clone()];
                            }
                            vec![AssistantContent::Text(TextContent {
                                text: thinking.thinking.clone(),
                                text_signature: None,
                                rest: Map::default(),
                            })]
                        }
                        AssistantContent::Text(_) => vec![block.clone()],
                        AssistantContent::ToolCall(tool_call)
                            if is_unnamed_tool_call(tool_call) =>
                        {
                            vec![]
                        }
                        AssistantContent::ToolCall(tool_call) => {
                            let mut normalized: ToolCall = tool_call.clone();
                            if !is_same_model && tool_call.thought_signature.is_some() {
                                normalized.thought_signature = None;
                            }
                            if !is_same_model {
                                if let Some(normalizer) =
                                    normalize_tool_call_id(&tool_call.id, model, assistant)
                                {
                                    normalized.id = normalizer;
                                }
                            }
                            if tracks_tool_calls {
                                match normalized_id_owners.get(&normalized.id) {
                                    Some(owner) if owner != &tool_call.id => {
                                        colliding_normalized_ids.insert(normalized.id.clone());
                                    }
                                    None => {
                                        normalized_id_owners
                                            .insert(normalized.id.clone(), tool_call.id.clone());
                                    }
                                    Some(_) => {}
                                }
                                pending_raw_calls.push((
                                    tool_call.id.clone(),
                                    tool_call.name.clone(),
                                    normalized.id.clone(),
                                ));
                            }
                            vec![AssistantContent::ToolCall(normalized)]
                        }
                    })
                    .collect();

                // A turn that held nothing but unnamed tool calls has nothing left to replay.
                if transformed_content.is_empty()
                    && assistant.content.iter().any(|block| {
                        matches!(block, AssistantContent::ToolCall(tool_call) if is_unnamed_tool_call(tool_call))
                    })
                {
                    return None;
                }
                Some(Message::Assistant(crate::types::AssistantMessage {
                    content: transformed_content,
                    ..assistant.clone()
                }))
            }
        })
        .collect();

    // Pair every emitted tool result with a currently pending tool call. A persisted user message
    // can race an abort result; recover that result from the remainder of the same turn before
    // synthesizing anything at the user boundary.
    let mut result: Vec<Message> = Vec::new();
    let mut consumed_tool_result_indexes: HashSet<usize> = HashSet::new();
    let mut pending_tool_calls: Vec<ToolCall> = Vec::new();
    let mut accepted_tool_results: HashMap<String, ToolResultMessage> = HashMap::new();
    let mut ambiguous_tool_calls: HashSet<(String, String)> = HashSet::new();
    // Only unresolved calls poison an exact id+name reuse in the next epoch. A completed epoch may
    // legitimately reuse both. Persisted synthetic results make a repaired transcript idempotent.
    let mut unresolved_tool_calls: HashSet<(String, String)> = HashSet::new();
    // Epoch millis fit u64 for ~584 million years; the u128 duration's millis are the timestamp's convention.
    #[allow(clippy::cast_possible_truncation)]
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);

    let insert_synthetic_tool_results =
        |result: &mut Vec<Message>,
         pending: &mut Vec<ToolCall>,
         accepted: &mut HashMap<String, ToolResultMessage>,
         ambiguous: &mut HashSet<(String, String)>,
         unresolved: &mut HashSet<(String, String)>| {
            for tool_call in pending.drain(..) {
                if !accepted.contains_key(&tool_call.id) {
                    unresolved.insert((tool_call.id.clone(), tool_call.name.clone()));
                    result.push(Message::ToolResult(ToolResultMessage {
                        tool_call_id: tool_call.id.clone(),
                        tool_name: tool_call.name.clone(),
                        content: vec![UserOrToolContent::Text(TextContent {
                            text: "No result provided".to_string(),
                            text_signature: None,
                            rest: Map::default(),
                        })],
                        details: None,
                        is_error: true,
                        timestamp: now,
                        rest: Map::default(),
                    }));
                }
            }
            accepted.clear();
            ambiguous.clear();
        };

    for (index, msg) in transformed.iter().enumerate() {
        if consumed_tool_result_indexes.contains(&index) {
            continue;
        }
        let Some(msg) = msg else {
            insert_synthetic_tool_results(
                &mut result,
                &mut pending_tool_calls,
                &mut accepted_tool_results,
                &mut ambiguous_tool_calls,
                &mut unresolved_tool_calls,
            );
            continue;
        };
        match msg {
            Message::Assistant(assistant) => {
                insert_synthetic_tool_results(
                    &mut result,
                    &mut pending_tool_calls,
                    &mut accepted_tool_results,
                    &mut ambiguous_tool_calls,
                    &mut unresolved_tool_calls,
                );

                // Skip errored/aborted assistant messages entirely. These are incomplete turns that
                // shouldn't be replayed.
                if matches!(
                    assistant.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) {
                    continue;
                }

                let tool_calls: Vec<ToolCall> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::ToolCall(tool_call) => Some(tool_call.clone()),
                        _ => None,
                    })
                    .collect();
                if !tool_calls.is_empty() {
                    ambiguous_tool_calls = tool_calls
                        .iter()
                        .filter(|tool_call| {
                            colliding_normalized_ids.contains(&tool_call.id)
                                || unresolved_tool_calls
                                    .contains(&(tool_call.id.clone(), tool_call.name.clone()))
                        })
                        .map(|tool_call| (tool_call.id.clone(), tool_call.name.clone()))
                        .collect();
                    pending_tool_calls = tool_calls;
                    accepted_tool_results.clear();
                }
                result.push(msg.clone());
            }
            Message::ToolResult(_) => {
                // Unmatched and duplicate results are invalid at every provider boundary.
                append_real_tool_result(
                    &mut result,
                    &pending_tool_calls,
                    &mut accepted_tool_results,
                    &mut ambiguous_tool_calls,
                    msg,
                );
            }
            Message::User(_) => {
                if !pending_tool_calls.is_empty() {
                    for (lookahead, later) in transformed.iter().enumerate().skip(index + 1) {
                        let Some(later) = later else {
                            break;
                        };
                        if matches!(later, Message::Assistant(_)) {
                            break;
                        }
                        if append_real_tool_result(
                            &mut result,
                            &pending_tool_calls,
                            &mut accepted_tool_results,
                            &mut ambiguous_tool_calls,
                            later,
                        ) {
                            consumed_tool_result_indexes.insert(lookahead);
                        }
                    }
                }
                insert_synthetic_tool_results(
                    &mut result,
                    &mut pending_tool_calls,
                    &mut accepted_tool_results,
                    &mut ambiguous_tool_calls,
                    &mut unresolved_tool_calls,
                );
                result.push(msg.clone());
            }
        }
    }

    insert_synthetic_tool_results(
        &mut result,
        &mut pending_tool_calls,
        &mut accepted_tool_results,
        &mut ambiguous_tool_calls,
        &mut unresolved_tool_calls,
    );

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Usage;

    fn user(text: &str) -> Message {
        Message::User(UserMessage {
            content: UserMessageContent::Text(text.to_string()),
            timestamp: 0,
            rest: Map::default(),
        })
    }

    fn assistant_with_tool_call(id: &str) -> Message {
        Message::Assistant(crate::types::AssistantMessage {
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: id.to_string(),
                name: "echo".to_string(),
                arguments: Map::default(),
                thought_signature: None,
                rest: Map::default(),
            })],
            api: "openai-completions".into(),
            provider: "test".into(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            stop_reason_raw: None,
            error_message: None,
            timestamp: 0,
            rest: Map::default(),
        })
    }

    fn test_model() -> Model {
        Model {
            id: "m".into(),
            name: "m".into(),
            api: "openai-completions".into(),
            provider: "test".into(),
            base_url: "http://localhost".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![crate::types::ModelInput::Text],
            cost: crate::types::zero_model_cost(),
            context_window: 128_000,
            max_tokens: 8192,
            max_tokens_explicit: false,
            featured: None,
            headers: None,
            compat: None,
        }
    }

    #[test]
    fn inserts_synthetic_tool_results_for_unanswered_calls() {
        let messages = vec![
            user("hi"),
            assistant_with_tool_call("call-1"),
            user("again"),
        ];
        let transformed =
            transform_messages_with_normalizer(&messages, &test_model(), &|_, _, _| None);
        // tool call assistant, synthetic result, then the user message
        assert_eq!(transformed.len(), 4);
        assert!(matches!(&transformed[0], crate::types::Message::User(_)));
        match &transformed[2] {
            Message::ToolResult(result) => {
                assert_eq!(result.tool_call_id, "call-1");
                assert!(result.is_error);
                assert!(matches!(&result.content[0],
                    UserOrToolContent::Text(text) if text.text == "No result provided"));
            }
            other => panic!("expected synthetic tool result, got {other:?}"),
        }
    }

    #[test]
    fn drops_errored_assistant_turns() {
        let Message::Assistant(mut assistant) = assistant_with_tool_call("call-1") else {
            unreachable!()
        };
        assistant.stop_reason = StopReason::Error;
        let messages = vec![user("hi"), Message::Assistant(assistant)];
        let transformed =
            transform_messages_with_normalizer(&messages, &test_model(), &|_, _, _| None);
        assert_eq!(transformed.len(), 1);
    }

    #[test]
    fn keeps_matched_tool_results_and_drops_orphans() {
        let messages = vec![
            assistant_with_tool_call("call-1"),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "call-1".into(),
                tool_name: "echo".into(),
                content: vec![],
                details: None,
                is_error: false,
                timestamp: 1,
                rest: Map::default(),
            }),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "orphan".into(),
                tool_name: "echo".into(),
                content: vec![],
                details: None,
                is_error: false,
                timestamp: 2,
                rest: Map::default(),
            }),
        ];
        let transformed =
            transform_messages_with_normalizer(&messages, &test_model(), &|_, _, _| None);
        assert_eq!(transformed.len(), 2);
    }

    fn tool_call(id: &str, name: &str) -> AssistantContent {
        AssistantContent::ToolCall(ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: Map::default(),
            thought_signature: None,
            rest: Map::default(),
        })
    }

    fn text(text: &str) -> AssistantContent {
        AssistantContent::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
            rest: Map::default(),
        })
    }

    fn assistant(content: Vec<AssistantContent>, stop_reason: StopReason) -> AssistantMessage {
        let Message::Assistant(assistant) = assistant_with_tool_call("unused") else {
            unreachable!()
        };
        AssistantMessage {
            content,
            stop_reason,
            ..assistant
        }
    }

    fn tool_result(id: &str, name: &str, text: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: name.into(),
            content: vec![UserOrToolContent::Text(TextContent {
                text: text.into(),
                text_signature: None,
                rest: Map::default(),
            })],
            details: None,
            is_error: true,
            timestamp: 0,
            rest: Map::default(),
        })
    }

    #[test]
    fn hoists_a_late_abort_result_before_an_interposed_user_notice() {
        let messages = vec![
            Message::Assistant(assistant(
                vec![tool_call("call-1", "bash")],
                StopReason::ToolUse,
            )),
            user("<prime_agent_update_interrupted>"),
            tool_result("call-1", "bash", "Request was aborted"),
        ];

        let transformed =
            transform_messages_with_normalizer(&messages, &test_model(), &|_, _, _| None);
        let expected = vec![
            messages[0].clone(),
            messages[2].clone(),
            messages[1].clone(),
        ];
        assert_eq!(transformed, expected);
    }

    #[test]
    fn anchors_real_results_from_both_sides_of_a_parallel_tool_boundary() {
        let messages = vec![
            Message::Assistant(assistant(
                vec![
                    tool_call("call-1", "bash"),
                    tool_call("call-2", "bash"),
                    tool_call("call-3", "bash"),
                ],
                StopReason::ToolUse,
            )),
            tool_result("call-1", "bash", "one"),
            user("restart"),
            tool_result("call-2", "bash", "two"),
        ];

        let transformed =
            transform_messages_with_normalizer(&messages, &test_model(), &|_, _, _| None);
        assert_eq!(transformed.len(), 5);
        assert_eq!(transformed[0], messages[0]);
        assert_eq!(transformed[1], messages[1]);
        assert_eq!(transformed[2], messages[3]);
        assert!(matches!(
            &transformed[3],
            Message::ToolResult(result)
                if result.tool_call_id == "call-3"
                    && result.is_error
                    && matches!(&result.content[0], UserOrToolContent::Text(text) if text.text == "No result provided")
        ));
        assert_eq!(transformed[4], messages[2]);
    }

    #[test]
    fn several_notices_keep_results_anchored_and_repair_is_idempotent() {
        let messages = vec![
            Message::Assistant(assistant(
                vec![tool_call("call-1", "bash"), tool_call("call-2", "bash")],
                StopReason::ToolUse,
            )),
            user("[from child:worker] agent message"),
            user("<ipython_state_restored>"),
            tool_result("call-2", "bash", "two"),
            tool_result("call-2", "bash", "duplicate"),
        ];

        let repaired =
            transform_messages_with_normalizer(&messages, &test_model(), &|_, _, _| None);
        assert_eq!(repaired.len(), 5);
        assert!(matches!(
            &repaired[1],
            Message::ToolResult(result)
                if result.tool_call_id == "call-1"
                    && matches!(&result.content[0], UserOrToolContent::Text(text) if text.text == "No result provided")
        ));
        assert!(matches!(
            &repaired[2],
            Message::ToolResult(result)
                if result.tool_call_id == "call-2"
                    && matches!(&result.content[0], UserOrToolContent::Text(text) if text.text == "No result provided")
        ));
        assert!(!repaired.iter().any(|message| matches!(
            message,
            Message::ToolResult(result)
                if matches!(&result.content[0], UserOrToolContent::Text(text) if text.text == "duplicate")
        )));
        assert_eq!(
            transform_messages_with_normalizer(&repaired, &test_model(), &|_, _, _| None),
            repaired
        );
    }

    #[test]
    fn a_reused_call_id_does_not_accept_a_late_result_from_the_prior_epoch() {
        let messages = vec![
            Message::Assistant(assistant(
                vec![tool_call("call-reused", "first-tool")],
                StopReason::ToolUse,
            )),
            user("interrupt"),
            Message::Assistant(assistant(
                vec![tool_call("call-reused", "second-tool")],
                StopReason::ToolUse,
            )),
            tool_result("call-reused", "first-tool", "late-first-result"),
            tool_result("call-reused", "second-tool", "real-second-result"),
        ];

        let transformed =
            transform_messages_with_normalizer(&messages, &test_model(), &|_, _, _| None);
        assert_eq!(transformed.len(), 5);
        assert!(matches!(
            &transformed[1],
            Message::ToolResult(result)
                if result.tool_call_id == "call-reused"
                    && result.tool_name == "first-tool"
                    && matches!(&result.content[0], UserOrToolContent::Text(text) if text.text == "No result provided")
        ));
        assert!(matches!(
            &transformed[4],
            Message::ToolResult(result)
                if result.tool_call_id == "call-reused"
                    && result.tool_name == "second-tool"
                    && matches!(&result.content[0], UserOrToolContent::Text(text) if text.text == "real-second-result")
        ));
        assert!(!transformed.iter().any(|message| matches!(
            message,
            Message::ToolResult(result)
                if matches!(&result.content[0], UserOrToolContent::Text(text) if text.text == "late-first-result")
        )));
    }

    #[test]
    fn same_name_reuse_after_an_unresolved_epoch_fails_closed_and_is_idempotent() {
        let messages = vec![
            Message::Assistant(assistant(
                vec![tool_call("call-reused", "same-tool")],
                StopReason::ToolUse,
            )),
            user("interrupt"),
            Message::Assistant(assistant(
                vec![tool_call("call-reused", "same-tool")],
                StopReason::ToolUse,
            )),
            tool_result("call-reused", "same-tool", "late-first-result"),
            tool_result("call-reused", "same-tool", "claimed-second-result"),
        ];

        let repaired =
            transform_messages_with_normalizer(&messages, &test_model(), &|_, _, _| None);
        assert_eq!(repaired.len(), 5);
        let results = repaired
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(result) => Some(result),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|result| matches!(
            &result.content[0],
            UserOrToolContent::Text(text) if text.text == "No result provided"
        )));
        assert_eq!(
            transform_messages_with_normalizer(&repaired, &test_model(), &|_, _, _| None),
            repaired
        );
    }

    #[test]
    fn cross_model_normalization_keeps_reused_raw_ids_epoch_ambiguous() {
        let mut first = assistant(
            vec![tool_call("raw-reused", "same-tool")],
            StopReason::ToolUse,
        );
        first.model = "source-a".into();
        let mut second = assistant(
            vec![tool_call("raw-reused", "same-tool")],
            StopReason::ToolUse,
        );
        second.model = "source-b".into();
        let messages = vec![
            Message::Assistant(first),
            user("interrupt"),
            Message::Assistant(second),
            tool_result("raw-reused", "same-tool", "late-first-result"),
            tool_result("raw-reused", "same-tool", "claimed-second-result"),
        ];
        let normalize = |id: &str, _model: &Model, assistant: &AssistantMessage| {
            Some(match (id, assistant.model.as_str()) {
                ("raw-reused", "source-a") => "normalized-a".to_string(),
                ("raw-reused", "source-b") => "normalized-b".to_string(),
                _ => id.to_string(),
            })
        };

        let repaired = transform_messages_with_normalizer(&messages, &test_model(), &normalize);
        let results = repaired
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(result) => Some(result),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].tool_call_id, "normalized-a");
        assert_eq!(results[1].tool_call_id, "normalized-b");
        assert!(results.iter().all(|result| matches!(
            &result.content[0],
            UserOrToolContent::Text(text) if text.text == "No result provided"
        )));
        assert_eq!(
            transform_messages_with_normalizer(&repaired, &test_model(), &normalize),
            repaired
        );
    }

    #[test]
    fn cross_model_normalization_assigns_distinct_names_to_their_raw_epochs() {
        let mut first = assistant(
            vec![tool_call("raw-reused", "first-tool")],
            StopReason::ToolUse,
        );
        first.model = "source-a".into();
        let mut second = assistant(
            vec![tool_call("raw-reused", "second-tool")],
            StopReason::ToolUse,
        );
        second.model = "source-b".into();
        let messages = vec![
            Message::Assistant(first),
            user("interrupt"),
            Message::Assistant(second),
            tool_result("raw-reused", "first-tool", "late-first"),
            tool_result("raw-reused", "second-tool", "real-second"),
        ];
        let normalize = |id: &str, _model: &Model, assistant: &AssistantMessage| {
            Some(match (id, assistant.model.as_str()) {
                ("raw-reused", "source-a") => "normalized-a".to_string(),
                ("raw-reused", "source-b") => "normalized-b".to_string(),
                _ => id.to_string(),
            })
        };
        let repaired = transform_messages_with_normalizer(&messages, &test_model(), &normalize);
        assert!(repaired.iter().any(|message| matches!(
            message,
            Message::ToolResult(result)
                if result.tool_call_id == "normalized-a"
                    && matches!(&result.content[0], UserOrToolContent::Text(text) if text.text == "No result provided")
        )));
        assert!(repaired.iter().any(|message| matches!(
            message,
            Message::ToolResult(result)
                if result.tool_call_id == "normalized-b"
                    && matches!(&result.content[0], UserOrToolContent::Text(text) if text.text == "real-second")
        )));
        assert!(!repaired.iter().any(|message| matches!(
            message,
            Message::ToolResult(result)
                if matches!(&result.content[0], UserOrToolContent::Text(text) if text.text == "late-first")
        )));
    }

    #[test]
    fn cross_model_normalization_with_missing_current_result_fails_closed() {
        let mut first = assistant(
            vec![tool_call("raw-reused", "same-tool")],
            StopReason::ToolUse,
        );
        first.model = "source-a".into();
        let mut second = assistant(
            vec![tool_call("raw-reused", "same-tool")],
            StopReason::ToolUse,
        );
        second.model = "source-b".into();
        let messages = vec![
            Message::Assistant(first),
            user("interrupt"),
            Message::Assistant(second),
            tool_result("raw-reused", "same-tool", "late-first"),
        ];
        let normalize = |id: &str, _model: &Model, assistant: &AssistantMessage| {
            Some(match (id, assistant.model.as_str()) {
                ("raw-reused", "source-a") => "normalized-a".to_string(),
                ("raw-reused", "source-b") => "normalized-b".to_string(),
                _ => id.to_string(),
            })
        };
        let repaired = transform_messages_with_normalizer(&messages, &test_model(), &normalize);
        let results = repaired
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(result) => Some(result),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].tool_call_id, "normalized-a");
        assert_eq!(results[1].tool_call_id, "normalized-b");
        assert!(results.iter().all(|result| matches!(
            &result.content[0], UserOrToolContent::Text(text) if text.text == "No result provided"
        )));
    }

    #[test]
    fn noninterrupted_cross_model_reuse_maps_each_result_to_its_epoch() {
        let mut first = assistant(
            vec![tool_call("raw-reused", "same-tool")],
            StopReason::ToolUse,
        );
        first.model = "source-a".into();
        let mut second = assistant(
            vec![tool_call("raw-reused", "same-tool")],
            StopReason::ToolUse,
        );
        second.model = "source-b".into();
        let messages = vec![
            Message::Assistant(first),
            tool_result("raw-reused", "same-tool", "first"),
            user("next"),
            Message::Assistant(second),
            tool_result("raw-reused", "same-tool", "second"),
        ];
        let normalize = |id: &str, _model: &Model, assistant: &AssistantMessage| {
            Some(match (id, assistant.model.as_str()) {
                ("raw-reused", "source-a") => "normalized-a".to_string(),
                ("raw-reused", "source-b") => "normalized-b".to_string(),
                _ => id.to_string(),
            })
        };
        let repaired = transform_messages_with_normalizer(&messages, &test_model(), &normalize);
        assert!(
            matches!(&repaired[1], Message::ToolResult(result) if result.tool_call_id == "normalized-a")
        );
        assert!(
            matches!(&repaired[4], Message::ToolResult(result) if result.tool_call_id == "normalized-b")
        );
    }

    #[test]
    fn distinct_raw_ids_colliding_after_normalization_fail_closed() {
        let mut first = assistant(vec![tool_call("raw-a", "tool")], StopReason::ToolUse);
        first.model = "source-a".into();
        let mut second = assistant(vec![tool_call("raw-b", "tool")], StopReason::ToolUse);
        second.model = "source-b".into();
        let messages = vec![
            Message::Assistant(first),
            user("interrupt"),
            Message::Assistant(second),
            tool_result("raw-a", "tool", "late-first"),
            tool_result("raw-b", "tool", "claimed-second"),
        ];
        let normalize = |id: &str, _model: &Model, _assistant: &AssistantMessage| {
            Some(if matches!(id, "raw-a" | "raw-b") {
                "normalized-collision".to_string()
            } else {
                id.to_string()
            })
        };
        let repaired = transform_messages_with_normalizer(&messages, &test_model(), &normalize);
        let results = repaired
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(result) => Some(result),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|result| matches!(
            &result.content[0], UserOrToolContent::Text(text) if text.text == "No result provided"
        )));
    }

    #[test]
    fn exact_retransmission_dedupes_but_conflicting_duplicates_fail_closed() {
        let assistant = Message::Assistant(assistant(
            vec![tool_call("call-1", "bash")],
            StopReason::ToolUse,
        ));
        let exact = tool_result("call-1", "bash", "same");
        let retransmitted = vec![assistant.clone(), exact.clone(), exact.clone()];
        assert_eq!(
            transform_messages_with_normalizer(&retransmitted, &test_model(), &|_, _, _| None),
            vec![assistant.clone(), exact]
        );

        let conflicting = vec![
            assistant,
            tool_result("call-1", "bash", "first"),
            tool_result("call-1", "bash", "second"),
        ];
        let repaired =
            transform_messages_with_normalizer(&conflicting, &test_model(), &|_, _, _| None);
        assert_eq!(repaired.len(), 2);
        assert!(matches!(
            &repaired[1],
            Message::ToolResult(result)
                if matches!(&result.content[0], UserOrToolContent::Text(text) if text.text == "No result provided")
        ));
    }

    #[test]
    fn completed_epochs_may_legitimately_reuse_the_same_id_and_name() {
        let first = tool_result("call-reused", "same-tool", "first");
        let second = tool_result("call-reused", "same-tool", "second");
        let messages = vec![
            Message::Assistant(assistant(
                vec![tool_call("call-reused", "same-tool")],
                StopReason::ToolUse,
            )),
            first,
            user("next"),
            Message::Assistant(assistant(
                vec![tool_call("call-reused", "same-tool")],
                StopReason::ToolUse,
            )),
            second,
        ];
        assert_eq!(
            transform_messages_with_normalizer(&messages, &test_model(), &|_, _, _| None),
            messages
        );
    }

    #[test]
    fn an_assistant_boundary_prevents_hoisting_a_result_from_a_later_turn() {
        let messages = vec![
            Message::Assistant(assistant(
                vec![tool_call("call-1", "bash")],
                StopReason::ToolUse,
            )),
            user("stop"),
            Message::Assistant(assistant(
                vec![tool_call("call-2", "bash")],
                StopReason::ToolUse,
            )),
            tool_result("call-1", "bash", "late orphan"),
            tool_result("call-2", "bash", "two"),
        ];

        let transformed =
            transform_messages_with_normalizer(&messages, &test_model(), &|_, _, _| None);
        assert!(matches!(
            &transformed[1],
            Message::ToolResult(result)
                if result.tool_call_id == "call-1"
                    && matches!(&result.content[0], UserOrToolContent::Text(text) if text.text == "No result provided")
        ));
        assert!(!transformed.iter().any(|message| matches!(
            message,
            Message::ToolResult(result)
                if matches!(&result.content[0], UserOrToolContent::Text(text) if text.text == "late orphan")
        )));
        assert!(transformed.iter().any(|message| matches!(
            message,
            Message::ToolResult(result)
                if result.tool_call_id == "call-2"
                    && matches!(&result.content[0], UserOrToolContent::Text(text) if text.text == "two")
        )));
    }

    #[test]
    fn a_session_poisoned_by_an_unnamed_tool_call_replays_without_it() {
        // The #3742 transcript: the gateway left the call without a name or id, the
        // loop answered `Tool  not found`, and both rows were persisted.
        let named = tool_call("call-1", "bash");
        let messages = vec![
            user("hi"),
            Message::Assistant(assistant(
                vec![text("probing"), tool_call("", "")],
                StopReason::ToolUse,
            )),
            tool_result("", "", "Tool  not found"),
            Message::Assistant(assistant(vec![tool_call("", "")], StopReason::ToolUse)),
            tool_result("", "", "Tool  not found"),
            Message::Assistant(assistant(
                vec![named.clone(), tool_call("call-2", " ")],
                StopReason::ToolUse,
            )),
            tool_result("call-1", "bash", "ok"),
            tool_result("call-2", " ", "Tool   not found"),
            user("again"),
        ];
        let transformed =
            transform_messages_with_normalizer(&messages, &test_model(), &|_, _, _| None);
        let expected = vec![
            user("hi"),
            Message::Assistant(assistant(vec![text("probing")], StopReason::ToolUse)),
            Message::Assistant(assistant(vec![named], StopReason::ToolUse)),
            tool_result("call-1", "bash", "ok"),
            user("again"),
        ];
        assert_eq!(transformed, expected);
    }

    #[test]
    fn a_dropped_unnamed_turn_does_not_answer_an_earlier_call_with_the_same_id() {
        let named = tool_call("call-1", "bash");
        let messages = vec![
            user("hi"),
            Message::Assistant(assistant(vec![named.clone()], StopReason::ToolUse)),
            tool_result("call-1", "bash", "ok"),
            Message::Assistant(assistant(
                vec![tool_call("call-1", "")],
                StopReason::ToolUse,
            )),
            tool_result("call-1", "", "Tool  not found"),
            user("again"),
        ];
        let transformed =
            transform_messages_with_normalizer(&messages, &test_model(), &|_, _, _| None);
        let expected = vec![
            user("hi"),
            Message::Assistant(assistant(vec![named], StopReason::ToolUse)),
            tool_result("call-1", "bash", "ok"),
            user("again"),
        ];
        assert_eq!(transformed, expected);
    }
}
