//! Side questions: a side thread cloned from the main conversation that answers without
//! interrupting the main work. Each run clones the live main conversation into a fresh loop
//! (KV-cacheable prefix preserved); earlier side turns replay after the clone, tools are
//! blocked (`before_tool_call`), a turn cap is the backstop; results never enter history.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use pa_agent::abort::{race_with_abort, AbortSignal};
use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};
use pa_agent::agent_loop::{BeforeToolCallFn, ShouldStopAfterTurnFn};
use pa_agent::types::{
    AssistantContent, AssistantMessage, Message, StopReason, TextContent, UserContent, UserMessage,
    UserPart,
};

use super::provider_retry::{
    is_agent_lifecycle_failure, is_faux_provider_queue_exhausted,
    is_permanent_provider_failure_kind, provider_retry_delay, provider_stream_failure_kind,
    provider_stream_failure_retry_after_ms, provider_stream_failure_status, ProviderRetryDelay,
    ProviderRetryPolicy,
};

/// Sink receiving partial side-question answers while the run streams.
/// Returning `false` requests the run to abort.
pub type SideQuestionSink = Arc<dyn Fn(&str) -> bool + Send + Sync>;

const SIDE_QUESTION_INSTRUCTION: &str = "The user asked this via `/btw` — a temporary side thread cloned from the main conversation to answer a question without interrupting the main work. Tools (including `python_repl`) are deactivated in this side thread and return an error if called; answer using only the conversation context above. The user may send follow-up side questions. Nothing here is added to the main session, so don't start or plan main-session work from this thread.";

const SIDE_QUESTION_TOOL_BLOCKED: &str =
    "Tools are deactivated in this side thread. Answer from the conversation context.";

const SIDE_QUESTION_SUMMARY_INSTRUCTION: &str = "Summarize the supplied conversation context for a side question. Preserve facts, decisions, constraints, prior side questions and answers, exact identifiers, and unresolved work. The continuing agent must answer only from this summary and the retained recent context, without tools.";

/// Backstop for a model that keeps calling deactivated tools instead of answering.
const SIDE_QUESTION_MAX_TURNS: u32 = 3;

/// Prompt sent to the side loop: the question wrapped in a side-question tag,
/// with the side-thread instruction prepended on the first turn.
#[must_use]
pub fn side_question_prompt(question: &str, is_first_turn: bool) -> String {
    let body = if is_first_turn {
        format!("{SIDE_QUESTION_INSTRUCTION}\n\n{question}")
    } else {
        question.to_string()
    };
    format!("<side_question>\n{body}\n</side_question>")
}

/// One completed side-question exchange, replayed into a follow-up run.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SideQuestionTurn {
    pub question: String,
    pub answer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideQuestionStatus {
    Complete,
    Cancelled,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SideQuestionResult {
    pub status: SideQuestionStatus,
    pub answer: String,
    pub error_message: Option<String>,
    /// The paid completion for this turn. Hosts persist it outside the parent
    /// transcript so `/btw` usage is accounted without entering main context.
    pub assistant: Option<AssistantMessage>,
    /// Paid completions replaced during side-context compaction or overflow recovery.
    pub auxiliary_completions: Vec<AssistantMessage>,
}

impl SideQuestionResult {
    fn cancelled(answer: String) -> Self {
        SideQuestionResult {
            status: SideQuestionStatus::Cancelled,
            answer,
            error_message: None,
            assistant: None,
            auxiliary_completions: Vec::new(),
        }
    }

    fn failed(answer: String, error_message: String) -> Self {
        SideQuestionResult {
            status: SideQuestionStatus::Error,
            answer,
            error_message: Some(error_message),
            assistant: None,
            auxiliary_completions: Vec::new(),
        }
    }
}

/// Text of an assistant message (text blocks joined).
fn assistant_text(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect()
}

fn serialized_tokens(message: &pa_agent::types::AgentMessage) -> u64 {
    serde_json::to_string(message)
        .map_or(0, |json| json.chars().count() as u64)
        .div_ceil(3)
}

fn side_context_tokens(messages: &[pa_agent::types::AgentMessage]) -> u64 {
    let anchor = messages.iter().rposition(|message| {
        matches!(
            message,
            pa_agent::types::AgentMessage::Standard(Message::Assistant(assistant))
                if assistant.stop_reason != StopReason::Error
                    && assistant.stop_reason != StopReason::Aborted
                    && assistant.usage.total_tokens > 0
        )
    });
    match anchor {
        Some(index) => {
            let pa_agent::types::AgentMessage::Standard(Message::Assistant(assistant)) =
                &messages[index]
            else {
                unreachable!("the anchor predicate admits assistant messages only")
            };
            assistant.usage.total_tokens
                + messages[index + 1..]
                    .iter()
                    .map(serialized_tokens)
                    .sum::<u64>()
        }
        None => messages.iter().map(serialized_tokens).sum(),
    }
}

fn side_turn_start(message: &pa_agent::types::AgentMessage) -> bool {
    matches!(
        message,
        pa_agent::types::AgentMessage::Standard(Message::User(_))
            | pa_agent::types::AgentMessage::Custom(_)
    )
}

async fn summarize_side_chunk(
    parent: &Arc<Agent>,
    stream_fn: pa_agent::stream::StreamFn,
    parent_state: &pa_agent::agent::AgentStateSnapshot,
    chunk: String,
    signal: &AbortSignal,
) -> anyhow::Result<AssistantMessage> {
    let summary_agent = Arc::new(Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            system_prompt: Some(SIDE_QUESTION_SUMMARY_INSTRUCTION.to_string()),
            model: Some(parent_state.model.clone()),
            thinking_level: Some(parent_state.thinking_level),
            tools: Some(Vec::new()),
            messages: Some(Vec::new()),
        },
        convert_to_llm: Some(parent.convert_to_llm_fn()),
        transform_context: parent.transform_context_fn(),
        stream_fn: Some(stream_fn),
        get_api_key: parent.get_api_key_fn(),
        session_id: parent.session_id().map(str::to_string),
        transport: Some(pa_agent::types::Transport::Sse),
        ..Default::default()
    }));
    let run = {
        let summary_agent = Arc::clone(&summary_agent);
        async move {
            summary_agent
                .prompt(pa_agent::agent::AgentPromptInput::text(format!(
                    "<conversation_segment>\n{chunk}\n</conversation_segment>"
                )))
                .await?;
            summary_agent.wait_for_idle().await;
            anyhow::Ok(())
        }
    };
    if let Err(error) = race_with_abort(run, signal).await {
        summary_agent.abort();
        return Err(error);
    }
    let state = summary_agent.state().await;
    let assistant = state
        .messages
        .into_iter()
        .rev()
        .find_map(|message| match message {
            pa_agent::types::AgentMessage::Standard(Message::Assistant(assistant)) => {
                Some(assistant)
            }
            _ => None,
        })
        .ok_or_else(|| anyhow::anyhow!("Side-question summary produced no assistant message"))?;
    if assistant.stop_reason == StopReason::Error {
        anyhow::bail!(
            "Side-question summary failed: {}",
            assistant
                .error_message
                .as_deref()
                .unwrap_or("Unknown error")
        );
    }
    if assistant_text(&assistant).trim().is_empty() {
        anyhow::bail!("Side-question summary was empty");
    }
    Ok(assistant)
}

async fn compact_side_messages(
    parent: &Arc<Agent>,
    stream_fn: pa_agent::stream::StreamFn,
    parent_state: &pa_agent::agent::AgentStateSnapshot,
    messages: &[pa_agent::types::AgentMessage],
    settings: &super::compaction::CompactionSettings,
    force: bool,
    signal: &AbortSignal,
) -> anyhow::Result<Option<(Vec<pa_agent::types::AgentMessage>, Vec<AssistantMessage>)>> {
    let system_tokens = (parent_state.system_prompt.chars().count() as u64).div_ceil(3);
    let context_tokens = system_tokens.saturating_add(side_context_tokens(messages));
    if !force
        && !super::compaction::should_compact(
            context_tokens,
            parent_state.model.context_window,
            parent_state.model.max_tokens,
            settings,
        )
    {
        return Ok(None);
    }

    let mut retained_tokens = 0u64;
    let mut cut = messages.len();
    for (index, message) in messages.iter().enumerate().rev() {
        let tokens = serialized_tokens(message);
        let keep_recent_tokens = if force {
            0
        } else {
            settings.keep_recent_tokens
        };
        if retained_tokens.saturating_add(tokens) > keep_recent_tokens {
            break;
        }
        retained_tokens = retained_tokens.saturating_add(tokens);
        cut = index;
    }
    while cut > 0 && (cut == messages.len() || !side_turn_start(&messages[cut])) {
        cut -= 1;
    }
    if cut == 0 {
        return Ok(None);
    }

    let serialized = messages[..cut]
        .iter()
        .map(|message| serde_json::to_string(message).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    let input_token_budget = parent_state
        .model
        .context_window
        .saturating_sub(parent_state.model.max_tokens)
        .saturating_sub(settings.reserve_tokens)
        .max(1);
    let max_chars = usize::try_from(input_token_budget.saturating_mul(3))
        .unwrap_or(usize::MAX)
        .max(1);
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    for character in serialized.chars() {
        if chunk.chars().count() >= max_chars {
            chunks.push(std::mem::take(&mut chunk));
        }
        chunk.push(character);
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }

    let mut completions = Vec::new();
    let mut summaries = Vec::new();
    for chunk in chunks {
        let assistant =
            summarize_side_chunk(parent, stream_fn.clone(), parent_state, chunk, signal).await?;
        summaries.push(assistant_text(&assistant));
        completions.push(assistant);
    }
    let summary = summaries.join("\n\n---\n\n");
    let mut compacted = vec![pa_agent::types::AgentMessage::user(format!(
        "<side_context_summary>\n{summary}\n</side_context_summary>"
    ))];
    compacted.extend_from_slice(&messages[cut..]);
    Ok(Some((compacted, completions)))
}

/// Run one side question against a clone of the parent conversation.
///
/// The side calls run on `stream_fn` (TS `unwrapSemanticEdgeStreamFn`: the
/// parent's pre-semantic fn, so a side call carries no request id and its
/// ledger stays untouched). The run streams partial answers to `sink`
/// (returning `false` aborts the run), honors `signal` for external
/// aborts, and retries provider failures per `retry_policy`. It never
/// mutates the parent agent or its history.
#[allow(clippy::too_many_arguments)] // Parent hooks, policy, cancellation, and stream sink are independent seams.
pub async fn run_side_question(
    parent: &Arc<Agent>,
    stream_fn: pa_agent::stream::StreamFn,
    question: &str,
    previous_turns: &[SideQuestionTurn],
    retry_policy: &ProviderRetryPolicy,
    compaction_settings: Option<&super::compaction::CompactionSettings>,
    signal: &AbortSignal,
    sink: &SideQuestionSink,
) -> SideQuestionResult {
    let parent_state = parent.state().await;
    if parent_state.model == pa_agent::types::Model::unknown() {
        return SideQuestionResult::failed(
            String::new(),
            "Select a model before asking a side question".to_string(),
        );
    }

    // Each turn re-clones the live main conversation, so follow-ups always see
    // the newest main-thread context; earlier side turns replay after it.
    let mut messages = parent_state.messages.clone();
    for (index, turn) in previous_turns.iter().enumerate() {
        messages.push(pa_agent::types::AgentMessage::Standard(Message::User(
            UserMessage {
                content: UserContent::Parts(vec![UserPart::Text(TextContent {
                    text: side_question_prompt(&turn.question, index == 0),
                    text_signature: None,
                })]),
                timestamp: pa_agent::now_ms(),
            },
        )));
        messages.push(pa_agent::types::AgentMessage::Standard(Message::Assistant(
            AssistantMessage {
                content: vec![AssistantContent::Text(TextContent {
                    text: turn.answer.clone(),
                    text_signature: None,
                })],
                api: parent_state.model.api.clone(),
                provider: parent_state.model.provider.clone(),
                model: parent_state.model.id.clone(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                usage: pa_agent::types::Usage::zero(),
                stop_reason: StopReason::Stop,
                stop_reason_raw: None,
                error_message: None,
                timestamp: pa_agent::now_ms(),
            },
        )));
    }
    // A turn-capped run can end on tool results, so its outcome lives in the
    // assistant turns it appended rather than in its last message.
    let cloned_message_count = messages.len();

    let mut auxiliary_completions = Vec::new();
    if let Some(settings) = compaction_settings.filter(|settings| settings.enabled) {
        match compact_side_messages(
            parent,
            stream_fn.clone(),
            &parent_state,
            &messages,
            settings,
            false,
            signal,
        )
        .await
        {
            Ok(Some((compacted, completions))) => {
                messages = compacted;
                auxiliary_completions = completions;
            }
            Ok(None) => {}
            Err(error) => {
                return SideQuestionResult {
                    status: SideQuestionStatus::Error,
                    answer: String::new(),
                    error_message: Some(error.to_string()),
                    assistant: None,
                    auxiliary_completions,
                };
            }
        }
    }

    let overflow_context = messages.clone();
    let turn_count = Arc::new(AtomicU32::new(0));
    let should_stop_after_turn: ShouldStopAfterTurnFn = {
        let turn_count = Arc::clone(&turn_count);
        Arc::new(move |context| {
            let turn_count = Arc::clone(&turn_count);
            Box::pin(async move {
                let count = turn_count.fetch_add(1, Ordering::SeqCst) + 1;
                let no_tool_call = !context
                    .message
                    .content
                    .iter()
                    .any(|block| matches!(block, AssistantContent::ToolCall(_)));
                Ok(count >= SIDE_QUESTION_MAX_TURNS || no_tool_call)
            })
        })
    };
    let before_tool_call: BeforeToolCallFn = Arc::new(|_context, _signal| {
        Box::pin(async move {
            Ok(Some(pa_agent::types::BeforeToolCallResult {
                block: true,
                reason: Some(SIDE_QUESTION_TOOL_BLOCKED.to_string()),
            }))
        })
    });
    let side_agent = Arc::new(Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            system_prompt: Some(parent_state.system_prompt.clone()),
            model: Some(parent_state.model.clone()),
            thinking_level: Some(parent_state.thinking_level),
            tools: Some(parent_state.tools.clone()),
            messages: Some(messages),
        },
        convert_to_llm: Some(parent.convert_to_llm_fn()),
        transform_context: parent.transform_context_fn(),
        stream_fn: Some(stream_fn.clone()),
        get_api_key: parent.get_api_key_fn(),
        before_tool_call: Some(before_tool_call),
        should_stop_after_turn: Some(should_stop_after_turn),
        // TS `side-question.ts`: the side call keeps the parent's identity so it reads the main
        // conversation's prompt cache, and rides SSE so it never takes the session's Codex
        // socket or its continuation.
        session_id: parent.session_id().map(str::to_string),
        transport: Some(pa_agent::types::Transport::Sse),
        ..Default::default()
    }));

    // Streaming events carry one partial turn at a time, so they only fill in
    // text as it arrives; the answer of the whole run is derived at the end.
    let (update_tx, update_rx) = std::sync::mpsc::channel::<AssistantMessage>();
    let update_tx = Arc::new(update_tx);
    let subscription = side_agent
        .subscribe(move |event, _signal| {
            let update_tx = Arc::clone(&update_tx);
            Box::pin(async move {
                let message = match &event {
                    pa_agent::types::AgentEvent::MessageUpdate { message, .. } => Some(&**message),
                    pa_agent::types::AgentEvent::MessageEnd { message } => Some(message),
                    _ => None,
                };
                if let Some(pa_agent::types::AgentMessage::Standard(Message::Assistant(
                    assistant,
                ))) = message
                {
                    let _ = update_tx.send(assistant.clone());
                }
                Ok(())
            })
        })
        .await;

    let mut result = run_attempts(
        &side_agent,
        question,
        previous_turns,
        cloned_message_count,
        retry_policy,
        signal,
        sink,
        &update_rx,
    )
    .await;
    let overflow = result.assistant.as_ref().is_some_and(|assistant| {
        result.status == SideQuestionStatus::Error
            && super::provider_retry::is_context_overflow_failure(
                assistant,
                parent_state.model.context_window,
            )
    });
    if overflow {
        if let (Some(settings), Some(overflow_completion)) = (
            compaction_settings.filter(|settings| settings.enabled),
            result.assistant.take(),
        ) {
            auxiliary_completions.push(overflow_completion);
            match compact_side_messages(
                parent,
                stream_fn,
                &parent_state,
                &overflow_context,
                settings,
                true,
                signal,
            )
            .await
            {
                Ok(Some((compacted, mut completions))) => {
                    auxiliary_completions.append(&mut completions);
                    side_agent.set_messages(compacted).await;
                    turn_count.store(0, Ordering::SeqCst);
                    result = run_attempts(
                        &side_agent,
                        question,
                        previous_turns,
                        side_agent.state().await.messages.len(),
                        retry_policy,
                        signal,
                        sink,
                        &update_rx,
                    )
                    .await;
                }
                Ok(None) => {}
                Err(error) => {
                    result = SideQuestionResult::failed(String::new(), error.to_string());
                }
            }
        }
    }
    let () = subscription.unsubscribe().await;
    auxiliary_completions.append(&mut result.auxiliary_completions);
    result.auxiliary_completions = auxiliary_completions;
    result
}

/// The retrying completion cycle: prompt (first attempt), then drop the failed
/// assistant turn and continue, with the shared provider retry policy (standalone side
/// runs bypass the session auto-retry loop, so retry here).
#[allow(clippy::too_many_arguments)]
async fn run_attempts(
    side_agent: &Arc<Agent>,
    question: &str,
    previous_turns: &[SideQuestionTurn],
    cloned_message_count: usize,
    retry_policy: &ProviderRetryPolicy,
    signal: &AbortSignal,
    sink: &SideQuestionSink,
    update_rx: &std::sync::mpsc::Receiver<AssistantMessage>,
) -> SideQuestionResult {
    let mut answer = String::new();
    let mut latest_streamed_message = None;
    if signal.is_aborted() {
        return SideQuestionResult::cancelled(answer);
    }
    let max_retries = if retry_policy.enabled {
        retry_policy.max_retries
    } else {
        0
    };
    let mut retries_performed = 0u32;
    let mut prompted = false;
    let final_message: anyhow::Result<Option<AssistantMessage>> = loop {
        let prompt = side_question_prompt(question, previous_turns.is_empty());
        let outcome = run_attempt(
            side_agent,
            prompt,
            prompted,
            cloned_message_count,
            signal,
            sink,
            update_rx,
            &mut latest_streamed_message,
            &mut answer,
        )
        .await;
        prompted = true;
        let message = match outcome {
            AttemptOutcome::Aborted => break Ok(None),
            AttemptOutcome::Finished(turn) => match *turn {
                None => {
                    if let Some(message) = latest_streamed_message.clone() {
                        message
                    } else {
                        let error = side_agent.state().await.error_message.unwrap_or_else(|| {
                            "Side question produced no assistant message".to_string()
                        });
                        break Err(anyhow::anyhow!(error));
                    }
                }
                Some(message) => message,
            },
        };
        if message.stop_reason != StopReason::Error {
            break Ok(Some(message));
        }
        if super::provider_retry::is_context_overflow_failure(
            &message,
            side_agent.state().await.model.context_window,
        ) {
            break Ok(Some(message));
        }
        if retries_performed >= max_retries
            || is_agent_lifecycle_failure(&message)
            || is_faux_provider_queue_exhausted(&message)
        {
            break Ok(Some(message));
        }
        let kind = provider_stream_failure_kind(&message);
        let status = provider_stream_failure_status(&message);
        if is_permanent_provider_failure_kind(kind.as_deref(), retries_performed, status) {
            break Ok(Some(message));
        }
        let delay = provider_retry_delay(
            retries_performed + 1,
            provider_stream_failure_retry_after_ms(&message),
            retry_policy,
        );
        let ProviderRetryDelay::Wait { delay_ms } = delay else {
            break Ok(Some(message));
        };
        // A cancel that raced the failure is an abort, not a provider failure.
        if race_with_abort(
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)),
            signal,
        )
        .await
        .is_err()
        {
            break Ok(None);
        }
        retries_performed += 1;
    };

    let message = match final_message {
        Ok(Some(message)) => message,
        Ok(None) => {
            return SideQuestionResult {
                status: SideQuestionStatus::Cancelled,
                answer,
                error_message: None,
                assistant: latest_streamed_message,
                auxiliary_completions: Vec::new(),
            }
        }
        Err(error) => return SideQuestionResult::failed(answer, error.to_string()),
    };
    if message.stop_reason == StopReason::Error {
        let error = message
            .error_message
            .clone()
            .unwrap_or_else(|| "Side question produced no assistant message".to_string());
        return SideQuestionResult {
            status: SideQuestionStatus::Error,
            answer,
            error_message: Some(error),
            assistant: Some(message),
            auxiliary_completions: Vec::new(),
        };
    }
    // A run that ends on a tool turn was answered in an earlier turn; a
    // textless final turn without tool calls is a genuinely empty answer.
    let final_answer = if message
        .content
        .iter()
        .any(|block| matches!(block, AssistantContent::ToolCall(_)))
    {
        assistant_turns_after(side_agent, cloned_message_count)
            .await
            .iter()
            .rev()
            .map(assistant_text)
            .find(|text| !text.is_empty())
            .unwrap_or_default()
    } else {
        assistant_text(&message)
    };
    SideQuestionResult {
        status: SideQuestionStatus::Complete,
        answer: final_answer,
        error_message: None,
        assistant: Some(message),
        auxiliary_completions: Vec::new(),
    }
}

enum AttemptOutcome {
    Finished(Box<Option<AssistantMessage>>),
    Aborted,
}

/// One attempt: prompt (or recover by dropping the failed assistant turn and
/// continuing), stream partial answers into `sink` while the run settles,
/// and stop early on an external abort or a cancelled sink.
#[allow(clippy::too_many_arguments)]
async fn run_attempt(
    side_agent: &Arc<Agent>,
    prompt: String,
    prompted: bool,
    cloned_message_count: usize,
    signal: &AbortSignal,
    sink: &SideQuestionSink,
    update_rx: &std::sync::mpsc::Receiver<AssistantMessage>,
    latest_streamed_message: &mut Option<AssistantMessage>,
    answer: &mut String,
) -> AttemptOutcome {
    let attempt: tokio::task::JoinHandle<anyhow::Result<Option<AssistantMessage>>> = {
        let side_agent = Arc::clone(side_agent);
        tokio::spawn(async move {
            let admitted: anyhow::Result<()> = if prompted {
                // Session-loop recovery: drop the failed assistant turn and re-run.
                let state = side_agent.state().await;
                let mut messages = state.messages;
                messages.pop();
                side_agent.set_messages(messages).await;
                side_agent.continue_run().await
            } else {
                side_agent
                    .prompt(pa_agent::agent::AgentPromptInput::text(prompt))
                    .await
            };
            admitted?;
            side_agent.wait_for_idle().await;
            Ok(assistant_turns_after(&side_agent, cloned_message_count)
                .await
                .pop())
        })
    };
    let mut aborted = false;
    loop {
        while let Ok(message) = update_rx.try_recv() {
            let text = assistant_text(&message);
            *latest_streamed_message = Some(message);
            if !text.is_empty() && text != *answer {
                answer.clone_from(&text);
                if !sink(answer) {
                    aborted = true;
                    side_agent.abort();
                }
            }
        }
        if signal.is_aborted() && !aborted {
            aborted = true;
            side_agent.abort();
        }
        if attempt.is_finished() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let result = attempt.await;
    // The terminal aborted assistant (with its provider usage) can queue
    // between the last poll and the task join. Drain it before returning the
    // cancelled outcome so persistence never loses paid partial work.
    while let Ok(message) = update_rx.try_recv() {
        let text = assistant_text(&message);
        *latest_streamed_message = Some(message);
        if !text.is_empty() {
            answer.clone_from(&text);
        }
    }
    if aborted {
        return AttemptOutcome::Aborted;
    }
    match result {
        Ok(Ok(message)) => AttemptOutcome::Finished(Box::new(message)),
        // The attempt task only errors when the prompt was rejected; the
        // outcome then lives in the agent's error state.
        Ok(Err(_)) | Err(_) => AttemptOutcome::Finished(Box::new(None)),
    }
}

/// The assistant messages the side run appended after the cloned prefix.
async fn assistant_turns_after(
    side_agent: &Arc<Agent>,
    cloned_message_count: usize,
) -> Vec<AssistantMessage> {
    let state = side_agent.state().await;
    state
        .messages
        .get(cloned_message_count..)
        .into_iter()
        .flatten()
        .filter_map(|message| match message {
            pa_agent::types::AgentMessage::Standard(Message::Assistant(assistant)) => {
                Some(assistant.clone())
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::provider_retry::UNBOUNDED_BACKOFF_MS;
    use super::*;
    use crate::session_engine::tool_bridge::bridge_tool;
    use crate::tools::tool_definition::{ToolDefinition, ToolExecutionResult};
    use pa_agent::abort::AbortController;
    use pa_agent::scripted::ScriptedProvider;
    use pa_agent::types::AgentTool;
    use std::sync::atomic::AtomicBool;

    fn test_model() -> pa_agent::types::Model {
        serde_json::from_value(serde_json::json!({
            "id": "m", "name": "m", "api": "openai-completions", "provider": "test",
            "baseUrl": "http://localhost", "reasoning": false, "input": ["text"],
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000, "maxTokens": 100
        }))
        .unwrap()
    }

    fn fast_retry_policy() -> ProviderRetryPolicy {
        ProviderRetryPolicy {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 1,
            max_retry_delay_ms: 1000,
            max_delay_ms: UNBOUNDED_BACKOFF_MS,
        }
    }

    /// A parent agent with one completed main-thread exchange.
    fn parent_agent(provider: &Arc<ScriptedProvider>) -> Arc<Agent> {
        parent_agent_with_tools(provider, Vec::new())
    }

    fn parent_agent_with_tools(
        provider: &Arc<ScriptedProvider>,
        tools: Vec<Arc<dyn AgentTool>>,
    ) -> Arc<Agent> {
        Arc::new(Agent::new(AgentOptions {
            initial_state: AgentInitialState {
                system_prompt: Some("main system prompt".to_string()),
                model: Some(test_model()),
                thinking_level: None,
                tools: Some(tools),
                messages: Some(vec![
                    pa_agent::types::AgentMessage::user("main thread context"),
                    pa_agent::types::AgentMessage::Standard(Message::Assistant(AssistantMessage {
                        content: vec![AssistantContent::Text(TextContent {
                            text: "main thread reply".to_string(),
                            text_signature: None,
                        })],
                        api: "openai-completions".to_string(),
                        provider: "test".to_string(),
                        model: "m".to_string(),
                        response_model: None,
                        response_id: None,
                        diagnostics: None,
                        usage: pa_agent::types::Usage::zero(),
                        stop_reason: StopReason::Stop,
                        stop_reason_raw: None,
                        error_message: None,
                        timestamp: 0,
                    })),
                ]),
            },
            stream_fn: Some(provider.stream_fn()),
            ..Default::default()
        }))
    }

    fn flag_tool(executed: Arc<AtomicBool>) -> ToolDefinition {
        ToolDefinition {
            name: "echo".to_string(),
            label: "Echo".to_string(),
            description: "Echoes its input".to_string(),
            prompt_snippet: String::new(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": []
            }),
            execution_mode: None,
            prepare_arguments: None,
            execute: Arc::new(move |_id, _params, _signal, _on_update| {
                let executed = Arc::clone(&executed);
                Box::pin(async move {
                    executed.store(true, Ordering::SeqCst);
                    Ok(ToolExecutionResult::text("tool ran"))
                })
            }),
        }
    }

    async fn run(
        parent: &Arc<Agent>,
        question: &str,
        previous_turns: &[SideQuestionTurn],
        signal: &AbortSignal,
        sink: &SideQuestionSink,
    ) -> SideQuestionResult {
        let stream_fn = parent
            .stream_fn()
            .cloned()
            .expect("the parent agent has a stream fn");
        run_side_question(
            parent,
            stream_fn,
            question,
            previous_turns,
            &fast_retry_policy(),
            None,
            signal,
            sink,
        )
        .await
    }

    fn context_text(provider: &ScriptedProvider) -> String {
        serde_json::to_string(provider.calls().first().expect("one call")).unwrap()
    }

    #[tokio::test]
    async fn answers_from_the_cloned_conversation() {
        let provider = Arc::new(ScriptedProvider::new(test_model()));
        provider.push_text_turn("the side answer");
        let parent = parent_agent(&provider);
        let sink: SideQuestionSink = Arc::new(|_| true);
        let result = run(
            &parent,
            "what is the answer?",
            &[],
            &AbortSignal::never(),
            &sink,
        )
        .await;
        assert_eq!(result.status, SideQuestionStatus::Complete);
        assert_eq!(result.answer, "the side answer");
        assert_eq!(result.error_message, None);

        // The side loop saw the cloned main conversation plus the wrapped
        // question (instruction on the first turn).
        let context = context_text(&provider);
        assert!(context.contains("main thread context"));
        assert!(context.contains("main thread reply"));
        assert!(context.contains("<side_question>"));
        assert!(context.contains("deactivated in this side thread"));
        assert!(context.contains("what is the answer?"));

        // Side results never touch the parent history.
        assert_eq!(parent.state().await.messages.len(), 2);
    }

    #[tokio::test]
    async fn side_calls_keep_the_parent_identity_on_sse() {
        let provider = Arc::new(ScriptedProvider::new(test_model()));
        provider.push_text_turn("the side answer");
        let parent = Arc::new(Agent::new(AgentOptions {
            initial_state: AgentInitialState {
                model: Some(test_model()),
                ..Default::default()
            },
            session_id: Some("parent-session".to_string()),
            ..Default::default()
        }));
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = Arc::clone(&observed);
        let inner = provider.stream_fn();
        let stream_fn: pa_agent::stream::StreamFn = Arc::new(move |model, context, options| {
            capture
                .lock()
                .unwrap()
                .push((options.session_id.clone(), options.transport));
            inner(model, context, options)
        });
        let sink: SideQuestionSink = Arc::new(|_| true);
        let result = run_side_question(
            &parent,
            stream_fn,
            "what is the answer?",
            &[],
            &fast_retry_policy(),
            None,
            &AbortSignal::never(),
            &sink,
        )
        .await;
        assert_eq!(result.status, SideQuestionStatus::Complete);
        assert_eq!(
            *observed.lock().unwrap(),
            vec![(
                Some("parent-session".to_string()),
                Some(pa_agent::types::Transport::Sse)
            )]
        );
    }

    #[tokio::test]
    async fn previous_turns_replay_after_the_clone() {
        let provider = Arc::new(ScriptedProvider::new(test_model()));
        provider.push_text_turn("follow-up answer");
        let parent = parent_agent(&provider);
        let previous = vec![SideQuestionTurn {
            question: "first question".to_string(),
            answer: "first answer".to_string(),
        }];
        let sink: SideQuestionSink = Arc::new(|_| true);
        let result = run(
            &parent,
            "follow-up question",
            &previous,
            &AbortSignal::never(),
            &sink,
        )
        .await;
        assert_eq!(result.status, SideQuestionStatus::Complete);
        assert_eq!(result.answer, "follow-up answer");

        // The replayed turn carries the instruction; the new question does not
        // (it is not the first turn anymore).
        let context = context_text(&provider);
        assert!(context.contains("first question"));
        assert!(context.contains("first answer"));
        assert!(context.contains("follow-up question"));
        assert_eq!(
            context.matches("deactivated in this side thread").count(),
            1
        );
    }

    #[tokio::test]
    async fn oversized_side_context_compacts_with_parent_hooks_and_reports_paid_summary() {
        let mut model = test_model();
        model.context_window = 10_000;
        let provider = Arc::new(ScriptedProvider::new(model.clone()));
        provider.push_text_turn("side context summary");
        provider.push_text_turn("answer after compaction");
        let transform_calls = Arc::new(AtomicU32::new(0));
        let count = Arc::clone(&transform_calls);
        let transform: pa_agent::agent_loop::TransformContextFn =
            Arc::new(move |messages, _signal| {
                count.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move { Ok(messages) })
            });
        let parent = Arc::new(Agent::new(AgentOptions {
            initial_state: AgentInitialState {
                system_prompt: Some("parent system".to_string()),
                model: Some(model),
                messages: Some(
                    (0..4)
                        .map(|index| {
                            pa_agent::types::AgentMessage::user(format!(
                                "old context {index}: {}",
                                "x".repeat(500)
                            ))
                        })
                        .collect(),
                ),
                ..Default::default()
            },
            transform_context: Some(transform),
            stream_fn: Some(provider.stream_fn()),
            ..Default::default()
        }));
        let sink: SideQuestionSink = Arc::new(|_| true);
        let settings = super::super::compaction::CompactionSettings {
            enabled: true,
            threshold_percent: 5.0,
            reserve_tokens: 10,
            keep_recent_tokens: 10,
        };
        let result = run_side_question(
            &parent,
            provider.stream_fn(),
            "answer this",
            &[],
            &fast_retry_policy(),
            Some(&settings),
            &AbortSignal::never(),
            &sink,
        )
        .await;

        assert_eq!(
            result.status,
            SideQuestionStatus::Complete,
            "side result: {result:?}"
        );
        assert_eq!(result.answer, "answer after compaction");
        assert_eq!(result.auxiliary_completions.len(), 1);
        assert_eq!(
            assistant_text(&result.auxiliary_completions[0]),
            "side context summary"
        );
        assert!(transform_calls.load(Ordering::SeqCst) >= 2);
        let calls = provider.calls();
        assert_eq!(calls.len(), 2, "one summary call and one answer call");
        let answer_context = serde_json::to_string(&calls[1]).unwrap();
        assert!(answer_context.contains("side_context_summary"));
        assert!(answer_context.contains("side context summary"));
        assert_eq!(parent.state().await.messages.len(), 4);
    }

    #[tokio::test]
    async fn context_overflow_compacts_once_and_records_replaced_completion() {
        let mut model = test_model();
        model.context_window = 10_000;
        let provider = Arc::new(ScriptedProvider::new(model.clone()));
        provider.push_stream_failure_turn(
            "overflow partial",
            "This model's maximum context length is 100 tokens. However, your messages resulted in 200 tokens.",
        );
        provider.push_text_turn("overflow recovery summary");
        provider.push_text_turn("answer after overflow recovery");
        let parent = Arc::new(Agent::new(AgentOptions {
            initial_state: AgentInitialState {
                model: Some(model),
                messages: Some(vec![
                    pa_agent::types::AgentMessage::user("old first turn"),
                    pa_agent::types::AgentMessage::user("old second turn"),
                    pa_agent::types::AgentMessage::user("recent turn"),
                ]),
                ..Default::default()
            },
            stream_fn: Some(provider.stream_fn()),
            ..Default::default()
        }));
        let sink: SideQuestionSink = Arc::new(|_| true);
        let settings = super::super::compaction::CompactionSettings {
            enabled: true,
            threshold_percent: 95.0,
            reserve_tokens: 10,
            keep_recent_tokens: 10,
        };
        let result = run_side_question(
            &parent,
            provider.stream_fn(),
            "recover this",
            &[],
            &fast_retry_policy(),
            Some(&settings),
            &AbortSignal::never(),
            &sink,
        )
        .await;

        assert_eq!(
            result.status,
            SideQuestionStatus::Complete,
            "side result: {result:?}"
        );
        assert_eq!(result.answer, "answer after overflow recovery");
        assert_eq!(result.auxiliary_completions.len(), 2);
        assert_eq!(
            result.auxiliary_completions[0].stop_reason,
            StopReason::Error
        );
        assert_eq!(
            assistant_text(&result.auxiliary_completions[1]),
            "overflow recovery summary"
        );
        assert_eq!(provider.calls().len(), 3);
    }

    #[tokio::test]
    async fn provider_failures_are_retried() {
        let provider = Arc::new(ScriptedProvider::new(test_model()));
        provider.push_stream_failure_turn("partial stream", "provider stream broke");
        provider.push_text_turn("recovered answer");
        let parent = parent_agent(&provider);
        let sink: SideQuestionSink = Arc::new(|_| true);
        let result = run(&parent, "retry me", &[], &AbortSignal::never(), &sink).await;
        assert_eq!(result.status, SideQuestionStatus::Complete);
        assert_eq!(result.answer, "recovered answer");
        assert_eq!(provider.calls().len(), 2);
    }

    #[tokio::test]
    async fn exhausted_retries_fail_the_run() {
        let provider = Arc::new(ScriptedProvider::new(test_model()));
        provider.push_stream_failure_turn("", "provider stream broke");
        provider.push_stream_failure_turn("", "provider stream broke again");
        let parent = parent_agent(&provider);
        let sink: SideQuestionSink = Arc::new(|_| true);
        let result = run(&parent, "fail me", &[], &AbortSignal::never(), &sink).await;
        // Two stream failures retry (transient); the third attempt exhausts
        // the scripted provider, which the loop records as an agent lifecycle
        // failure (never retried) and the run fails with.
        assert_eq!(result.status, SideQuestionStatus::Error);
        assert_eq!(provider.calls().len(), 3);
        assert!(result
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("ScriptedProvider exhausted")));
    }

    #[tokio::test]
    async fn abort_mid_run_cancels_with_partial_answer() {
        let provider = Arc::new(ScriptedProvider::new(test_model()));
        provider.push_stalled_turn("partial answer");
        let parent = parent_agent(&provider);
        let controller = AbortController::new();
        let aborter = {
            let controller = controller.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                controller.abort();
            })
        };
        let sink: SideQuestionSink = Arc::new(|_| true);
        let result = run(&parent, "long question", &[], &controller.signal(), &sink).await;
        aborter.await.unwrap();
        assert_eq!(result.status, SideQuestionStatus::Cancelled);
        assert_eq!(result.answer, "partial answer");
        assert_eq!(result.error_message, None);
        let assistant = result.assistant.expect("cancelled partial assistant");
        assert_eq!(assistant_text(&assistant), "partial answer");
        assert_eq!(assistant.stop_reason, StopReason::Aborted);
    }

    #[tokio::test]
    async fn cancelled_sink_aborts_the_run() {
        let provider = Arc::new(ScriptedProvider::new(test_model()));
        provider.push_stalled_turn("streaming text");
        let parent = parent_agent(&provider);
        let controller = AbortController::new();
        let signal = controller.signal();
        // The first partial answer cancels the run.
        let sink: SideQuestionSink = Arc::new(str::is_empty);
        let result = run(&parent, "cancel me", &[], &signal, &sink).await;
        assert_eq!(result.status, SideQuestionStatus::Cancelled);
        assert_eq!(result.answer, "streaming text");
    }

    #[tokio::test]
    async fn tool_calls_are_blocked_and_turn_capped() {
        let provider = Arc::new(ScriptedProvider::new(test_model()));
        provider.push_tool_call_turn(None, vec![("call-1", "echo", serde_json::json!({}))]);
        provider.push_tool_call_turn(None, vec![("call-2", "echo", serde_json::json!({}))]);
        provider.push_tool_call_turn(None, vec![("call-3", "echo", serde_json::json!({}))]);
        let executed = Arc::new(AtomicBool::new(false));
        let tool = bridge_tool(flag_tool(Arc::clone(&executed)));
        let parent = parent_agent_with_tools(&provider, vec![tool]);
        let sink: SideQuestionSink = Arc::new(|_| true);
        let result = run(
            &parent,
            "keep calling tools",
            &[],
            &AbortSignal::never(),
            &sink,
        )
        .await;
        // The backstop stops the run at the turn cap; no tool ever executed.
        assert_eq!(provider.calls().len(), 3);
        assert!(!executed.load(Ordering::SeqCst));
        assert_eq!(result.status, SideQuestionStatus::Complete);
        assert_eq!(result.answer, "");
    }

    #[tokio::test]
    async fn missing_model_fails_the_run() {
        let provider = Arc::new(ScriptedProvider::new(test_model()));
        provider.push_text_turn("never reached");
        let parent = Arc::new(Agent::new(AgentOptions {
            initial_state: AgentInitialState {
                system_prompt: Some("prompt".to_string()),
                model: None,
                thinking_level: None,
                tools: None,
                messages: None,
            },
            stream_fn: Some(provider.stream_fn()),
            ..Default::default()
        }));
        let sink: SideQuestionSink = Arc::new(|_| true);
        let result = run(&parent, "question", &[], &AbortSignal::never(), &sink).await;
        assert_eq!(result.status, SideQuestionStatus::Error);
        assert_eq!(
            result.error_message.as_deref(),
            Some("Select a model before asking a side question")
        );
    }
}
