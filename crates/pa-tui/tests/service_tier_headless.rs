//! Headless e2e for the `/tier` service-tier command (TS #2144): the bare
//! command reports current and available tiers; an unsupported tier errors
//! with the list; a supported tier applies through `set_service_tier` and
//! the tray badge shows the non-default tier.
#![cfg(unix)]
// Casts: structurally bounded terminal-layout arithmetic; guarded conversions add panic paths.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
// Render routes are flat tables (one arm per route); splitting adds indirection.
#![allow(clippy::too_many_lines)]
// Widget state structs carry independent flag bits.
#![allow(clippy::struct_excessive_bools, clippy::fn_params_excessive_bools)]
// Futures are bounded by the surface's lifetime; boxing adds a steady-state allocation.
#![allow(clippy::large_futures)]
// The wrappers preserve a uniform Result-returning API surface.
#![allow(clippy::unnecessary_wraps)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use pa_tui::interactive::{
    run_interactive, HeadlessPlan, HeadlessStep, InteractiveOptions, ModelSelection,
    SessionSelection, UiMode,
};
use serde_json::{json, Value};

struct MockSupervisor {
    listener: UnixListener,
    model: Value,
    requests: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
}

/// Reset each syscall's timeout from one deadline, including partial line reads/writes.
struct DeadlineStream {
    stream: UnixStream,
    deadline: std::time::Instant,
}

impl DeadlineStream {
    fn remaining(&self) -> std::io::Result<std::time::Duration> {
        self.deadline
            .checked_duration_since(std::time::Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "mock deadline expired")
            })
    }
}

impl Read for DeadlineStream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.stream.set_read_timeout(Some(self.remaining()?))?;
        self.stream.read(buffer)
    }
}

impl Write for DeadlineStream {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

/// The `OpenRouter` catalog entry as the daemon's `get_model_catalog` answer carries it:
/// completions API, so flex and priority are both eligible. The same json seeds
/// `InteractiveOptions::model_catalog` (the composition-root snapshot the picker serves before the
/// fetch lands).
fn openrouter_model_value() -> Value {
    json!({
        "id": "openai/gpt-5.5",
        "name": "OpenAI: GPT-5.5",
        "api": "openai-completions",
        "provider": "openrouter",
        "baseUrl": "https://openrouter.ai/api/v1",
        "reasoning": true,
        "input": ["text"],
        "cost": { "input": 5, "output": 30, "cacheRead": 0.5, "cacheWrite": 0 },
        "contextWindow": 272_000,
        "maxTokens": 128_000
    })
}

impl MockSupervisor {
    fn bind(socket: &std::path::Path, model: Value) -> Self {
        MockSupervisor {
            listener: UnixListener::bind(socket).expect("bind mock socket"),
            model,
            requests: std::sync::Arc::default(),
        }
    }

    /// Serve one connection: attach an OpenRouter-model session; `set_service_tier` applies,
    /// `get_state` reports the tier.
    fn serve(self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        self.listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let accept_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("accept runtime");
        let stream = accept_runtime.block_on(async {
            let listener =
                tokio::net::UnixListener::from_std(self.listener).expect("async mock listener");
            let (stream, _) = tokio::time::timeout_at(
                tokio::time::Instant::from_std(deadline),
                listener.accept(),
            )
            .await
            .expect("bounded mock accept")
            .expect("accept");
            stream.into_std().expect("blocking mock stream")
        });
        stream.set_nonblocking(false).expect("blocking mock I/O");
        let mut tier = "default".to_string();
        let write_stream = stream.try_clone().expect("clone mock socket");
        let mut writer = DeadlineStream {
            stream: write_stream,
            deadline,
        };
        let mut reader = BufReader::new(DeadlineStream { stream, deadline });

        let hello = json!({
            "type": "daemon_hello",
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "serverCapabilities": [],
            "clientId": "mock",
        });
        write_json(&mut writer, &hello);

        let mut line = String::new();
        loop {
            let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
                break;
            };
            if remaining.is_zero() {
                break;
            }
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(envelope) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            let id = envelope.get("id").and_then(Value::as_str).unwrap_or("");
            let command = envelope.get("command").cloned().unwrap_or(Value::Null);
            let command_type = command
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            match command_type.as_str() {
                "create" => {
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "create",
                            "success": true,
                            "data": {
                                "activeSessionId": "s1",
                                "id": "s1",
                                "sessionId": "sess-1",
                                "sessionFile": "/tmp/sess-1.jsonl",
                            },
                        }),
                    );
                }
                "attach" => {
                    write_json(&mut writer, &attach_data(id, &self.model));
                }
                "set_service_tier" => {
                    tier = command["serviceTier"]
                        .as_str()
                        .expect("requested tier")
                        .to_string();
                    self.requests.lock().unwrap().push(command.clone());
                    // The daemon arm answers success with no data; the follow-up get_state read
                    // reports the applied tier.
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "set_service_tier",
                            "success": true,
                            "data": {},
                        }),
                    );
                }
                "get_model_catalog" => {
                    // The startup fetch (`spawn_model_catalog_refresh`) replaces the
                    // composition-root catalog; serve the same OpenRouter entry the eligibility
                    // reads run against.
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "get_model_catalog",
                            "success": true,
                            "data": {
                                "models": [self.model.clone()],
                                "configuredProviders": [self.model["provider"].clone()],
                            },
                        }),
                    );
                }
                "get_connection_state" => {
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "get_connection_state",
                            "success": true,
                            "data": {
                                "activeSessionId": "s1",
                                "sessionId": "sess-1",
                                "serviceTier": tier,
                                "steeringMode": "one-at-a-time",
                                "model": self.model,
                                "isStreaming": false,
                            },
                        }),
                    );
                }
                "detach" => {
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "detach",
                            "success": true,
                        }),
                    );
                }
                _ => {
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": command_type,
                            "success": true,
                            "data": {},
                        }),
                    );
                }
            }
        }
    }
}

fn write_json(writer: &mut DeadlineStream, value: &Value) {
    let mut line = serde_json::to_string(value).expect("serialize mock frame");
    line.push('\n');
    writer.write_all(line.as_bytes()).expect("write mock frame");
    writer.flush().expect("flush mock frame");
}

/// The slim attach result: one session on an `OpenRouter` completions model (the catalog entry the
/// eligibility predicate reads).
fn attach_data(id: &str, model: &Value) -> Value {
    json!({
        "type": "response",
        "id": id,
        "command": "attach",
        "success": true,
        "data": {
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "activeSessionId": "s1",
            "snapshot": {
                "activeSessionId": "s1",
                "summary": { "id": "s1", "cwd": "/tmp" },
                "state": {
                    "activeSessionId": "s1",
                    "cwd": "/tmp",
                    "sessionId": "sess-1",
                    "sessionName": "tier session",
                    "model": model,
                    "isStreaming": false,
                    "isCompacting": false,
                    "sessionActions": { "queuedCount": 0, "steering": [], "followUps": [] },
                },
                "messages": [],
                "lastEventSequence": 0,
                "lastEventCursor": null,
            },
            "client": { "id": "mock", "capabilities": [] },
            "lastEventSequence": 0,
            "lastEventCursor": null,
        },
    })
}

/// The composition-root catalog seed (the options snapshot).
fn model_catalog(model: Value) -> Vec<pa_types::ai::Model> {
    serde_json::from_value(model)
        .map(|model| vec![model])
        .expect("catalog model")
}

fn options(socket: PathBuf, model: Value) -> InteractiveOptions {
    InteractiveOptions {
        models: None,
        socket_path: socket,
        cwd: PathBuf::from("/tmp"),
        session_dir: None,
        script_path: None,
        model_selection: ModelSelection::default(),
        model_catalog: model_catalog(model),
        model_configured_providers: std::collections::HashSet::default(),
        model_recent_models: Vec::new(),
        default_thinking_level: None,
        no_session: false,
        session: SessionSelection::New,
        restore_dock_focus: false,
        initial_message: None,
        show_images: true,
        fullscreen_mouse: true,
        theme: "prime".to_string(),
        code_block_indent: "  ".to_string(),
        tree_filter_mode: String::new(),
        branch_summary_skip_prompt: false,
        version: "0.0.0".to_string(),
        onboarding: None,
        telemetry_disabled: None,
        client_auth: None,
        traces: None,
        provider_auth: None,
        update_commands: None,
        telemetry: None,
        keybindings: pa_tui::keybindings::KeybindingsManager::new(),
        session_rlm_depth: None,
        prompt_stash: std::sync::Arc::default(),
        session_has_children: false,
        client_settings: None,
    }
}

fn run_plan(steps: Vec<HeadlessStep>, model: Value) -> (Vec<String>, Vec<Value>) {
    std::env::remove_var("TMUX");
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("tui.sock");
    let supervisor = MockSupervisor::bind(&socket, model.clone());
    let requests = supervisor.requests.clone();
    let handle = std::thread::spawn(move || supervisor.serve());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let plan = HeadlessPlan {
        steps,
        width: 240,
        height: 30,
    };
    let outcome = runtime
        .block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(15),
                run_interactive(options(socket, model), UiMode::Headless(plan)),
            )
            .await
            .expect("bounded interactive run")
        })
        .expect("interactive run");
    handle.join().expect("mock supervisor finished");
    let requests = requests.lock().unwrap().clone();
    (outcome.frames, requests)
}

#[test]
fn tier_command_shows_applies_and_rejects() {
    let steps = vec![
        HeadlessStep::WaitRender {
            needle: "openai/gpt-5.5".to_string(),
            timeout_ms: 5000,
        },
        HeadlessStep::Submit("/tier".to_string()),
        HeadlessStep::WaitRender {
            needle: "Service tier: default".to_string(),
            timeout_ms: 5000,
        },
        HeadlessStep::Submit("/tier scale".to_string()),
        HeadlessStep::WaitRender {
            needle: "Service tier 'scale' is not available".to_string(),
            timeout_ms: 5000,
        },
        HeadlessStep::Submit("/tier flex".to_string()),
        HeadlessStep::WaitRender {
            needle: "Service tier: flex".to_string(),
            timeout_ms: 5000,
        },
    ];
    let (frames, _) = run_plan(steps, openrouter_model_value());
    assert!(!frames.is_empty(), "frames were captured");
    let all = frames.join("\n");
    assert!(
        all.contains("Service tier: default (available: default, flex, priority)"),
        "the show note rendered:\n{all}"
    );
    // `scale` is not a user-facing choice: the TS error names the tier and lists the available ones
    // (the rendered row wraps the list at the frame width, so the assertion stays on the
    // single-line head).
    assert!(
        all.contains("Service tier 'scale' is not available for the current model"),
        "the unsupported error rendered:\n{all}"
    );
    assert!(
        all.contains("Available: default, flex,"),
        "the error lists the available tiers:\n{all}"
    );
    assert!(
        all.contains("Service tier: flex"),
        "the applied-tier note rendered:\n{all}"
    );
    assert!(
        frames
            .iter()
            .any(|frame| frame.contains("openai/gpt-5.5 \u{00b7} flex")),
        "the tray badge rendered the applied tier:\n{all}"
    );
}

/// Drive the real slash-command route with the daemon's safe capability metadata.
/// Arbitrary gateway ids must work; a false/missing opt-in and non-Responses APIs
/// must show actionable configuration advice without issuing a tier mutation.
#[test]
fn fast_command_honors_custom_gateway_capability_and_explains_opt_in() {
    const HINT: &str = "Fast mode requires a model that supports the priority service tier. For custom gateways, use openai-responses and set compat.supportsFastMode to true in models.json";
    for (api, capability, provider, model_id, eligible) in [
        (
            "openai-responses",
            Some(true),
            "custom-gateway",
            "gateway-arbitrary-model",
            true,
        ),
        (
            "openai-completions",
            None,
            "openrouter",
            "openai/gpt-5.5",
            true,
        ),
        (
            "openai-responses",
            Some(false),
            "custom-gateway",
            "gateway-arbitrary-model",
            false,
        ),
        (
            "openai-responses",
            None,
            "custom-gateway",
            "gateway-arbitrary-model",
            false,
        ),
        (
            "openai-completions",
            Some(true),
            "custom-gateway",
            "gateway-arbitrary-model",
            false,
        ),
    ] {
        let mut model = json!({
            "id": model_id, "name": "Gateway arbitrary model",
            "api": api, "provider": provider, "baseUrl": "http://127.0.0.1:1/v1",
            "reasoning": true, "input": ["text"],
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
            "contextWindow": 32768, "maxTokens": 4096,
        });
        if let Some(capability) = capability {
            model["compat"] = json!({"supportsFastMode": capability});
        }
        let expected = if eligible { "Fast mode: on" } else { HINT };
        let steps = vec![
            HeadlessStep::WaitRender {
                needle: model_id.to_string(),
                timeout_ms: 5000,
            },
            HeadlessStep::Submit("/fast".to_string()),
            HeadlessStep::WaitRender {
                needle: if eligible {
                    expected
                } else {
                    "Fast mode requires"
                }
                .to_string(),
                timeout_ms: 5000,
            },
        ];
        let (frames, requests) = run_plan(steps, model);
        let rendered = frames.join("\n");
        assert!(
            rendered.contains(expected),
            "{api} {capability:?}: {rendered}"
        );
        let tiers: Vec<_> = requests
            .iter()
            .map(|request| request["serviceTier"].clone())
            .collect();
        assert_eq!(
            tiers,
            if eligible {
                vec![json!("priority")]
            } else {
                vec![]
            }
        );
        if eligible {
            assert!(
                !rendered.contains(HINT),
                "eligible gateway received rejection advice"
            );
        }
    }
}
