import { PassThrough } from "node:stream";
import type { AgentMessage } from "@earendil-works/pi-agent-core";
import { fauxAssistantMessage } from "@earendil-works/pi-ai";
import { describe, expect, it, vi } from "vitest";
import { DaemonAgentConnection } from "../src/modes/agent-connection/daemon-agent-connection.js";
import type {
	AgentConnectionEvent,
	AgentConnectionRlmChildAgentSnapshot,
	AgentConnectionState,
} from "../src/modes/agent-connection/types.js";
import {
	DaemonCapabilityUnavailableError,
	type DaemonClientCloseListener,
	type DaemonClientMessageListener,
	type DaemonClientRequestOptions,
	type DaemonHello,
	DaemonSocketClosedError,
	type DaemonTransportClient,
} from "../src/modes/daemon/daemon-client.js";
import {
	DAEMON_PROTOCOL_INFO,
	DAEMON_SCHEMA_REVISION,
	type DaemonAttachResult,
	type DaemonCommand,
	type DaemonOutbound,
	type DaemonResponse,
	failure,
	success,
} from "../src/modes/daemon/daemon-protocol.js";
import { DaemonRoutedClient } from "../src/modes/daemon/daemon-routed-client.js";
import type { DaemonWorkerClient } from "../src/modes/daemon/daemon-worker-client.js";
import { InteractiveMode } from "../src/modes/interactive/interactive-mode.js";
import { attachJsonlLineReader, serializeJsonLine } from "../src/modes/rpc/jsonl.js";
import { getCodingAgentFixtureModel } from "./fixture-models.js";

class FakeDaemonClient {
	readonly requests: DaemonCommand[] = [];
	readonly requestTimeouts: number[] = [];
	attachResultFactory: ((command: Extract<DaemonCommand, { type: "attach" }>) => DaemonAttachResult) | undefined;
	restoredAttachGate: Promise<void> | undefined;
	restoredAttachCompleted = 0;
	closeCount = 0;
	emitCloseOnClose = false;
	connected = true;
	reconnectCount = 0;
	resetTransportCount = 0;
	reconnectError: Error | undefined;
	attachFailures = 0;
	readonly attachGates = new Map<string, Promise<void>>();
	readonly attachFailureIds = new Set<string>();
	connectionStateGate: Promise<void> | undefined;
	connectionStateFactory: ((activeSessionId: string) => AgentConnectionState) | undefined;
	rlmChildren: AgentConnectionRlmChildAgentSnapshot[] = [];
	rlmChildrenEventSequence = 12;
	rlmChildrenGate: Promise<void> | undefined;
	abortBashUnknownCommand = false;
	abortAndClearQueueUnknownCommand = false;
	abortAndSendQueuedUnknownCommand = false;
	inputPauseAcquireGate: Promise<void> | undefined;
	cronAddGate: Promise<void> | undefined;
	promptGate: Promise<void> | undefined;
	promptError: Error | undefined;
	promptResponseError: string | undefined;
	readonly deadActiveSessionIds = new Set<string>();
	readonly attachedIds = new Set<string>();
	omitWasAttached = false;
	createResponseError: string | undefined;
	createGate: Promise<void> | undefined;
	revivedAttachGate: Promise<void> | undefined;
	switchSessionAlreadyActiveId: string | undefined;
	switchSessionSucceeds = false;
	connectionStateFailures = 0;
	cancelPromptAdmissionStatus: "cancelled" | "owned" | "unknown" = "owned";
	serverCapabilities = new Set<string>();
	updateRestartSessions: Array<Record<string, unknown>> = [];
	hello: DaemonHello | undefined = {
		type: "daemon_hello",
		socketPath: "/tmp/fake.sock",
		protocol: DAEMON_PROTOCOL_INFO,
		schemaRevision: DAEMON_SCHEMA_REVISION,
		clientId: "fake-client",
		serverCapabilities: ["prompt_admission_cancellation", "session_input_admission"],
	};
	private readonly messageListeners = new Set<DaemonClientMessageListener>();
	private readonly closeListeners = new Set<DaemonClientCloseListener>();

	async request(
		command: DaemonCommand,
		timeoutMs = 30000,
		options: DaemonClientRequestOptions = {},
	): Promise<DaemonResponse> {
		this.requests.push(command);
		this.requestTimeouts.push(timeoutMs);
		switch (command.type) {
			case "prompt":
				if (this.promptGate) await this.promptGate;
				if (this.deadActiveSessionIds.has(command.activeSessionId)) {
					return {
						type: "response",
						command: command.type,
						success: false,
						error: `Unknown active session: ${command.activeSessionId}`,
					};
				}
				if (this.promptError) throw this.promptError;
				if (this.promptResponseError) {
					return { type: "response", command: command.type, success: false, error: this.promptResponseError };
				}
				return { type: "response", command: command.type, success: true };
			case "prompt_and_wait":
				if (this.deadActiveSessionIds.has(command.activeSessionId)) {
					return {
						type: "response",
						command: command.type,
						success: false,
						error: `Unknown active session: ${command.activeSessionId}`,
					};
				}
				if (this.promptGate) await this.promptGate;
				if (this.promptError) throw this.promptError;
				return { type: "response", command: command.type, success: true };
			case "create":
				if (this.createGate) await this.createGate;
				if (this.createResponseError) {
					return { type: "response", command: command.type, success: false, error: this.createResponseError };
				}
				return {
					type: "response",
					command: command.type,
					success: true,
					data: { id: "active-revived", activeSessionId: "active-revived", sessionId: "session-revived" },
				};
			case "complete_owned_session":
				return { type: "response", command: command.type, success: true };
			case "cancel_prompt_admission":
				return {
					type: "response",
					command: command.type,
					success: true,
					data: { status: this.cancelPromptAdmissionStatus },
				};
			case "list":
				return {
					type: "response",
					command: command.type,
					success: true,
					data: { sessions: this.updateRestartSessions },
				};
			case "attach": {
				if (this.attachFailures > 0) {
					this.attachFailures--;
					throw new Error("attach failed");
				}
				await this.attachGates.get(command.activeSessionId);
				if (this.attachFailureIds.delete(command.activeSessionId)) {
					throw new Error(`attach failed: ${command.activeSessionId}`);
				}
				if (command.activeSessionId === "active-revived" && this.revivedAttachGate) {
					await this.revivedAttachGate;
				}
				if (command.activeSessionId === "active-restored" && this.restoredAttachGate) {
					await this.restoredAttachGate;
					this.restoredAttachCompleted++;
				}
				if (command.activeSessionId === "missing") {
					return {
						type: "response",
						command: command.type,
						success: false,
						error: "Unknown active session: missing",
					};
				}
				{
					// Mirror the supervisor's per-socket attachment Set: wasAttached
					// reports whether this attach created the entry. omitWasAttached
					// models a pre-revision-15 daemon.
					const wasAttached = this.attachedIds.has(command.activeSessionId);
					this.attachedIds.add(command.activeSessionId);
					const response: DaemonResponse = {
						type: "response",
						command: command.type,
						success: true,
						data: {
							...(this.attachResultFactory?.(command) ??
								createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12)),
							...(this.omitWasAttached ? {} : { wasAttached }),
						},
					};
					options.onResponse?.(response);
					return response;
				}
			}
			case "get_queue":
				return {
					type: "response",
					command: command.type,
					success: true,
					data: { steering: ["steer"], followUp: ["follow"] },
				};
			case "get_connection_state":
				await this.connectionStateGate;
				if (this.deadActiveSessionIds.has(command.activeSessionId)) {
					return {
						type: "response",
						command: command.type,
						success: false,
						error: `Unknown active session: ${command.activeSessionId}`,
					};
				}
				if (this.connectionStateFailures > 0) {
					this.connectionStateFailures--;
					return { type: "response", command: command.type, success: false, error: "state read failed" };
				}
				return {
					type: "response",
					command: command.type,
					success: true,
					data:
						this.connectionStateFactory?.(command.activeSessionId) ??
						createConnectionState(command.activeSessionId, "session-current"),
				};
			case "get_messages":
				return {
					type: "response",
					command: command.type,
					success: true,
					data: { messages: [{ role: "user", content: "current prompt", timestamp: 4 }] },
				};
			case "get_rlm_children":
				await this.rlmChildrenGate;
				return {
					type: "response",
					command: command.type,
					success: true,
					data: { children: this.rlmChildren, eventSequence: this.rlmChildrenEventSequence },
				};
			case "get_resource_snapshot":
				return {
					type: "response",
					command: command.type,
					success: true,
					data: {
						contextFiles: [{ path: "/tmp/AGENTS.md" }],
						skills: [
							{
								name: "demo-skill",
								description: "Demo skill",
								filePath: "/tmp/skills/demo-skill/SKILL.md",
								sourceInfo: {
									path: "/tmp/skills/demo-skill/SKILL.md",
									source: "local",
									scope: "project",
									origin: "top-level",
									baseDir: "/tmp/skills",
								},
							},
						],
						prompts: [],
						extensions: [],
						themes: [],
						diagnostics: {
							skills: [],
							prompts: [],
							extensions: [],
							themes: [],
						},
					},
				};
			case "replace_acp_mcp_servers":
				return { type: "response", command: command.type, success: true };
			case "get_model_catalog":
				return {
					type: "response",
					command: command.type,
					success: true,
					data: {
						models: [getCodingAgentFixtureModel("openai", "gpt-5.1")],
						configuredProviders: ["openai"],
					},
				};
			case "get_available_models":
				return {
					type: "response",
					command: command.type,
					success: true,
					data: { models: [getCodingAgentFixtureModel("openai", "gpt-5.1")] },
				};
			case "get_session_context":
				return {
					type: "response",
					command: command.type,
					success: true,
					data: {
						context: {
							messages: [{ role: "user", content: "context prompt", timestamp: 3 }],
							thinkingLevel: "medium",
							model: { provider: "anthropic", modelId: "claude-sonnet-4-5" },
						},
					},
				};
			case "get_session_tree":
				return {
					type: "response",
					command: command.type,
					success: true,
					data: {
						flatNodes: [
							{
								entry: {
									type: "message",
									id: "user-1",
									parentId: null,
									timestamp: "2026-01-01T00:00:00.000Z",
									message: { role: "user", content: "hello", timestamp: 1 },
								},
							},
						],
						leafId: "user-1",
					},
				};
			case "get_context_tree":
				return {
					type: "response",
					command: command.type,
					success: true,
					data: {
						id: "root",
						label: "active-1 name",
						status: "active",
						ownUsage: {
							input: 0,
							output: 0,
							cacheRead: 0,
							cacheWrite: 0,
							totalTokens: 0,
							cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
						},
						totalUsage: {
							input: 0,
							output: 0,
							cacheRead: 0,
							cacheWrite: 0,
							totalTokens: 0,
							cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
						},
						children: [],
					},
				};
			case "get_tool_definition":
				return {
					type: "response",
					command: command.type,
					success: true,
					data: {
						toolDefinition: {
							name: command.name,
							label: command.name,
							description: `${command.name} description`,
							promptSnippet: `${command.name} prompt`,
							promptGuidelines: [`Use ${command.name}`],
							parameters: { type: "object" },
							renderShell: "self",
						},
					},
				};
			case "abort":
				return success(command.id, command.type);
			case "clear_queue":
				return {
					type: "response",
					command: command.type,
					success: true,
					data: { steering: ["cleared"], followUp: [] },
				};
			case "abort_and_clear_queue":
				if (this.abortAndClearQueueUnknownCommand) {
					return {
						type: "response",
						command: command.type,
						success: false,
						error: "Unknown daemon command: abort_and_clear_queue",
					};
				}
				return {
					type: "response",
					command: command.type,
					success: true,
					data: { steering: ["aborted"], followUp: ["cleared"] },
				};
			case "abort_and_send_queued":
				return this.abortAndSendQueuedUnknownCommand
					? failure(command.id, command.type, "Unknown daemon command: abort_and_send_queued")
					: success(command.id, command.type);
			case "acquire_session_input_pause":
				if (this.inputPauseAcquireGate) await this.inputPauseAcquireGate;
				return {
					type: "response",
					command: command.type,
					success: true,
					data: { pauseId: "pause-1" },
				};
			case "release_session_input_pause":
				return { type: "response", command: command.type, success: true };
			case "heartbeats_list":
				return this.serverCapabilities.has("heartbeat_catalog")
					? { type: "response", command: command.type, success: true, data: { heartbeats: [] } }
					: {
							type: "response",
							command: command.type,
							success: false,
							error: "Unknown daemon command: heartbeats_list",
						};
			case "heartbeat_manage":
				return this.serverCapabilities.has("heartbeat_management")
					? {
							type: "response",
							command: command.type,
							success: true,
							data: { heartbeat: { id: command.jobId } },
						}
					: {
							type: "response",
							command: command.type,
							success: false,
							error: "Unknown daemon command: heartbeat_manage",
						};
			case "list_saved_sessions": {
				const activeSessionId = "activeSessionId" in command ? command.activeSessionId : undefined;
				options.onProgress?.({
					id: "daemon_test",
					type: "session_list_progress",
					command: "list_saved_sessions",
					...(activeSessionId ? { activeSessionId } : {}),
					loaded: 1,
					total: 2,
				});
				options.onProgress?.({
					id: "daemon_test",
					type: "session_list_item",
					command: "list_saved_sessions",
					...(activeSessionId ? { activeSessionId } : {}),
					session: {
						path: "/tmp/session-a.jsonl",
						id: "session-a",
						cwd: "/tmp",
						name: "Saved session",
						created: "2026-01-01T00:00:00.000Z",
						modified: "2026-01-02T00:00:00.000Z",
						messageCount: 2,
						firstMessage: "hello",
						allMessagesText: "hello world",
					},
				});
				options.onProgress?.({
					id: "daemon_test",
					type: "session_list_progress",
					command: "list_saved_sessions",
					...(activeSessionId ? { activeSessionId } : {}),
					loaded: 2,
					total: 2,
				});
				return {
					type: "response",
					command: command.type,
					success: true,
					data: {
						sessions: [
							{
								path: "/tmp/session-a.jsonl",
								id: "session-a",
								cwd: "/tmp",
								name: "Saved session",
								created: "2026-01-01T00:00:00.000Z",
								modified: "2026-01-02T00:00:00.000Z",
								messageCount: 2,
								firstMessage: "hello",
								allMessagesText: "hello world",
							},
						],
					},
				};
			}
			case "wait_for_headless_completion":
				return {
					type: "response",
					command: command.type,
					success: true,
					data: {
						enabled: false,
						continuationsUsed: 0,
						turnsUsed: 0,
						tokensUsed: 0,
						limits: { maxContinuations: 0 },
					},
				};
			case "roster_subscribe":
				return { type: "response", command: command.type, success: true, data: { roster: [] } };
			case "wait_for_idle":
			case "set_scoped_models":
			case "rename_saved_session":
			case "extension_ui_response":
				return { type: "response", command: command.type, success: true };
			case "detach":
				if (typeof command.activeSessionId === "string") {
					this.attachedIds.delete(command.activeSessionId);
				}
				return { type: "response", command: command.type, success: true };
			case "cancel_rlm_child":
				if (command.childId === "stale-daemon") {
					return {
						type: "response",
						command: command.type,
						success: false,
						error: "Unknown daemon command: cancel_rlm_child",
					};
				}
				return {
					type: "response",
					command: command.type,
					success: true,
					data: { cancelled: command.childId === "child-1" },
				};
			case "execute_bash":
				if (command.command === "stale-daemon") {
					return {
						type: "response",
						command: command.type,
						success: false,
						error: "Unknown daemon command: execute_bash",
					};
				}
				return { type: "response", command: command.type, success: true };
			case "abort_bash":
				if (this.abortBashUnknownCommand) {
					return {
						type: "response",
						command: command.type,
						success: false,
						error: "Unknown daemon command: abort_bash",
					};
				}
				return { type: "response", command: command.type, success: true };
			case "start_side_question":
				return { type: "response", command: command.type, success: true };
			case "delete_saved_session":
				return {
					type: "response",
					command: command.type,
					success: true,
					data: { ok: true, method: "trash" },
				};
			case "refine":
				return {
					type: "response",
					command: command.type,
					success: true,
					data: {
						id: "refine_daemon",
						summary: "Daemon refinement",
						rationale: "Test daemon refine timeout",
						expectedOutcome: "Refine request completes",
						appliedEdits: [],
						harnessStatePath: "/tmp/harness_state.json",
					},
				};
			case "cron_add":
				await this.cronAddGate;
				return {
					type: "response",
					command: command.type,
					success: true,
					data: {
						job: {
							id: `cron-${this.requests.filter((request) => request.type === "cron_add").length}`,
							status: "active",
							source: "cron",
							activeSessionId: command.activeSessionId,
							sessionId: "session-current",
							sessionFile: "/tmp/session-current.jsonl",
							cwd: "/tmp/project",
							prompt: command.prompt,
							schedule: { kind: "cron", expression: command.schedule },
							createdAt: "2026-01-01T00:00:00.000Z",
							updatedAt: "2026-01-01T00:00:00.000Z",
							runCount: 0,
						},
					},
				};
			case "switch_session":
				if (this.switchSessionSucceeds) {
					return { type: "response", command: command.type, success: true, data: { cancelled: false } };
				}
				if (this.switchSessionAlreadyActiveId) {
					return {
						type: "response",
						command: command.type,
						success: false,
						error: "Session already active",
						errorInfo: {
							code: "session_already_active",
							sessionPath: command.sessionPath,
							activeSessionId: this.switchSessionAlreadyActiveId,
						},
					};
				}
				return {
					type: "response",
					command: command.type,
					success: false,
					error: "Stored session working directory does not exist: /tmp/missing\nSession file: /tmp/session.jsonl\nCurrent working directory: /tmp/current",
					errorInfo: {
						code: "missing_session_cwd",
						issue: {
							sessionFile: "/tmp/session.jsonl",
							sessionCwd: "/tmp/missing",
							fallbackCwd: "/tmp/current",
						},
					},
				};
			case "import_jsonl":
				return {
					type: "response",
					command: command.type,
					success: false,
					error: "File not found: /tmp/not-found.jsonl",
					errorInfo: {
						code: "session_import_file_not_found",
						filePath: "/tmp/not-found.jsonl",
					},
				};
			default:
				throw new Error(`Unexpected command: ${command.type}`);
		}
	}

	supportsServerCapability(capability: string): boolean {
		return this.serverCapabilities.has(capability);
	}

	onMessage(listener: DaemonClientMessageListener): () => void {
		this.messageListeners.add(listener);
		return () => {
			this.messageListeners.delete(listener);
		};
	}

	onClose(listener: DaemonClientCloseListener): () => void {
		this.closeListeners.add(listener);
		return () => {
			this.closeListeners.delete(listener);
		};
	}

	emitMessage(message: DaemonOutbound): void {
		for (const listener of [...this.messageListeners]) {
			listener(message);
		}
	}

	emitClose(error: Error): void {
		this.attachedIds.clear();
		for (const listener of [...this.closeListeners]) {
			listener(error);
		}
	}

	enableRequestRecovery(): void {}

	async connect(): Promise<void> {
		if (this.connected) {
			throw new Error("Prime Agent daemon client is already connected");
		}
		this.reconnectCount++;
		if (this.reconnectError) {
			throw this.reconnectError;
		}
		this.connected = true;
	}

	async waitForHello(): Promise<DaemonHello> {
		return this.hello!;
	}

	resetTransportForReconnect(): void {
		this.resetTransportCount++;
		this.connected = false;
	}

	async reconnect(): Promise<void> {
		if (this.connected) {
			return;
		}
		this.reconnectCount++;
		if (this.reconnectError) {
			throw this.reconnectError;
		}
		this.connected = true;
	}

	get isConnected(): boolean {
		return this.connected;
	}

	getMessageListenerCount(): number {
		return this.messageListeners.size;
	}

	getCloseListenerCount(): number {
		return this.closeListeners.size;
	}

	close(): void {
		this.closeCount++;
		this.connected = false;
		if (this.emitCloseOnClose) {
			this.emitClose(new Error("Daemon socket closed"));
		}
	}

	disconnectForReconnect(reason: "shutdown" | "update"): void {
		this.closeCount++;
		this.connected = false;
		this.emitClose(new DaemonSocketClosedError("/tmp/prime-agent.sock", reason));
	}
}

/**
 * Emit the daemon's streamed snapshot for one session: the chunked replacement or
 * resync frames a switch consumes, optionally preceded by the inline
 * session_replaced marker and optionally failing instead of completing.
 */
function emitChunkedSnapshot(
	fakeClient: FakeDaemonClient,
	options: {
		purpose: "replacement" | "resync";
		sessionId: string;
		messages?: AgentMessage[];
		sequence?: number;
		inline?: boolean;
		fail?: string;
		omitEnd?: boolean;
		sessionFile?: string;
	},
): void {
	const messages = options.messages ?? [];
	const sequence = options.sequence ?? 14;
	const baseState = createConnectionState("active-1", options.sessionId);
	const state = options.sessionFile === undefined ? baseState : { ...baseState, sessionFile: options.sessionFile };
	const snapshotId = `${options.purpose}-${options.sessionId}`;
	if (options.inline) {
		fakeClient.emitMessage({
			type: "session_replaced",
			activeSessionId: "active-1",
			state,
			messages: [],
			snapshotFollows: true,
		});
	}
	const { messages: _omitted, ...snapshot } = createAttachResult("active-1", "client-1", undefined, sequence, {
		state,
		messages,
	}).snapshot;
	fakeClient.emitMessage({
		type: "session_snapshot_begin",
		activeSessionId: "active-1",
		snapshotId,
		snapshot,
		messageCount: messages.length,
		targetChunkBytes: 512 * 1024,
		purpose: options.purpose,
	});
	if (options.fail) {
		fakeClient.emitMessage({
			type: "session_snapshot_failed",
			activeSessionId: "active-1",
			snapshotId,
			error: options.fail,
		});
		return;
	}
	fakeClient.emitMessage({
		type: "session_snapshot_chunk",
		activeSessionId: "active-1",
		snapshotId,
		index: 0,
		messages,
	});
	if (options.omitEnd) return;
	fakeClient.emitMessage({
		type: "session_snapshot_end",
		activeSessionId: "active-1",
		snapshotId,
		chunkCount: 1,
		lastEventSequence: sequence,
		lastEventCursor: { generation: "generation-active-1", sequence },
	});
}

function asDaemonClient(client: FakeDaemonClient): DaemonTransportClient {
	return client as unknown as DaemonTransportClient;
}

function createConnectionState(activeSessionId: string, sessionId: string): AgentConnectionState {
	return {
		activeSessionId,
		cwd: "/tmp/project",
		model: undefined,
		thinkingLevel: "medium",
		serviceTier: "default",
		availableThinkingLevels: ["minimal", "low", "medium", "high", "xhigh"],
		isStreaming: false,
		isCompacting: false,
		isBashRunning: false,
		retryAttempt: 0,
		steeringMode: "all",
		followUpMode: "one-at-a-time",
		sessionFile: `/tmp/${sessionId}.jsonl`,
		sessionId,
		sessionName: `${sessionId} name`,
		sessionDir: "/tmp/sessions",
		leafId: `${sessionId}-leaf`,
		autoCompactionEnabled: true,
		messageCount: 1,
		sessionActions: { queuedCount: 0, steering: [], followUps: [] },
		compactionCount: 0,
		goal: {
			active: false,
			status: "idle",
			tokensUsed: 0,
			timeUsedSeconds: 0,
			continuationsUsed: 0,
		},
		scopedModels: [],
		activeToolNames: ["ipython"],
		contextUsage: undefined,
	};
}

interface CreateAttachResultOptions {
	state?: AgentConnectionState;
	messages?: AgentMessage[];
	streamingMessage?: AgentMessage;
	sessionContext?: DaemonAttachResult["snapshot"]["sessionContext"];
	omitSessionContext?: boolean;
	sessionTree?: DaemonAttachResult["snapshot"]["sessionTree"];
	parent?: DaemonAttachResult["snapshot"]["parent"];
	children?: DaemonAttachResult["snapshot"]["children"];
	replay?: DaemonAttachResult["replay"];
}

function createAttachResult(
	activeSessionId: string,
	clientId: string | undefined,
	capabilities: readonly string[] | undefined,
	lastEventSequence: number,
	options: CreateAttachResultOptions = {},
): DaemonAttachResult {
	const state = options.state ?? createConnectionState(activeSessionId, "session-current");
	const messages = options.messages ?? [];
	const lastEventCursor = { generation: `generation-${activeSessionId}`, sequence: lastEventSequence };
	const summary = {
		id: activeSessionId,
		activeSessionId,
		lifecycle: "live" as const,
		activity: "idle" as const,
		isSessionActive: state.isStreaming,
		sessionId: state.sessionId,
		cwd: "/tmp/project",
		isStreaming: state.isStreaming,
		isCompacting: false,
		attachedClients: 1,
		messageCount: messages.length,
		sessionActions: { queuedCount: 0, steering: [], followUps: [] },
		...(options.streamingMessage ? { streamingMessage: options.streamingMessage } : {}),
	};
	// Slim shape: the daemon omits top-level state/messages for clients with the
	// "slim_attach" capability, which DaemonAgentConnection always advertises.
	return {
		protocol: DAEMON_PROTOCOL_INFO,
		activeSessionId,
		snapshot: {
			activeSessionId,
			summary,
			state,
			messages,
			...(options.omitSessionContext
				? {}
				: {
						sessionContext:
							options.sessionContext ??
							({
								messages,
								thinkingLevel: state.thinkingLevel,
								serviceTier: state.serviceTier,
								model: state.model ? { provider: state.model.provider, modelId: state.model.id } : null,
							} satisfies NonNullable<DaemonAttachResult["snapshot"]["sessionContext"]>),
					}),
			sessionTree: options.sessionTree ?? { tree: [], leafId: state.leafId },
			lastEventSequence,
			lastEventCursor,
			...(options.parent ? { parent: options.parent } : {}),
			...(options.children ? { children: options.children } : {}),
		},
		replay: options.replay ?? {
			status: "complete",
			toSequence: lastEventSequence,
			toCursor: lastEventCursor,
		},
		lastEventSequence,
		lastEventCursor,
		client: {
			id: clientId ?? "client-1",
			capabilities: (capabilities ?? ["attach_snapshot", "event_sequence"]).filter(
				(capability): capability is DaemonAttachResult["client"]["capabilities"][number] =>
					capability === "attach_snapshot" ||
					capability === "event_sequence" ||
					capability === "extension_ui" ||
					capability === "slim_attach" ||
					capability === "chunked_snapshot",
			),
		},
	};
}

function gateFirstRevivedAttachSnapshot(client: FakeDaemonClient, snapshotId: string): () => void {
	let armed = true;
	let emitSnapshot: () => void = () => {
		throw new Error(`Revived attach ${snapshotId} has not started`);
	};
	client.attachResultFactory = (command) => {
		const full = createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 23);
		if (command.activeSessionId !== "active-revived" || !armed) return full;
		armed = false;
		const { messages: _messages, ...snapshotHeader } = full.snapshot;
		emitSnapshot = () => {
			client.emitMessage({
				type: "session_snapshot_begin",
				activeSessionId: command.activeSessionId,
				snapshotId,
				snapshot: snapshotHeader,
				messageCount: 0,
				targetChunkBytes: 512 * 1024,
			});
			client.emitMessage({
				type: "session_snapshot_end",
				activeSessionId: command.activeSessionId,
				snapshotId,
				chunkCount: 0,
				lastEventSequence: 23,
			});
		};
		return {
			...full,
			snapshot: { ...full.snapshot, messages: [] },
			snapshotStream: { id: snapshotId, messageCount: 0, targetChunkBytes: 512 * 1024 },
		};
	};
	return () => emitSnapshot();
}

function emitRlmChildUpdate(
	client: FakeDaemonClient,
	activeSessionId: string,
	sequence: number,
	child: AgentConnectionRlmChildAgentSnapshot,
): void {
	client.emitMessage({
		type: "session_event",
		activeSessionId,
		event: { type: "rlm_child_update", child },
		meta: {
			id: `${activeSessionId}:${sequence}`,
			protocol: DAEMON_PROTOCOL_INFO,
			activeSessionId,
			sequence,
			cursor: { generation: `generation-${activeSessionId}`, sequence },
			emittedAt: "2026-01-01T00:00:00.000Z",
		},
	});
}

function emitSequencedQueueUpdate(client: FakeDaemonClient, activeSessionId: string, sequence: number): void {
	client.emitMessage({
		type: "session_event",
		activeSessionId,
		event: {
			type: "session_action_update",
			actions: { queuedCount: 0, steering: [], followUps: [] },
		},
		meta: {
			id: `${activeSessionId}:${sequence}`,
			protocol: DAEMON_PROTOCOL_INFO,
			activeSessionId,
			sequence,
			cursor: { generation: `generation-${activeSessionId}`, sequence },
			emittedAt: "2026-01-01T00:00:00.000Z",
		},
	});
}

describe("DaemonAgentConnection", () => {
	it("falls back to the supervisor when the direct socket closes during initial attach", async () => {
		const supervisor = new FakeDaemonClient();
		const closeListeners = new Set<(error: Error) => void>();
		let directConnected = true;
		const direct = {
			get isConnected() {
				return directConnected;
			},
			onMessage: () => () => {},
			onClose: (listener: (error: Error) => void) => {
				closeListeners.add(listener);
				return () => closeListeners.delete(listener);
			},
			request: async () => {
				directConnected = false;
				const error = new Error("direct attach socket closed");
				for (const listener of [...closeListeners]) listener(error);
				throw error;
			},
			close: () => {
				directConnected = false;
			},
		} as unknown as DaemonWorkerClient;
		const routed = new DaemonRoutedClient(asDaemonClient(supervisor), direct);

		const connection = await DaemonAgentConnection.attach(routed, "active-1");

		await expect(connection.getInitialSnapshot()).resolves.toMatchObject({
			state: { activeSessionId: "active-1" },
		});
		expect(supervisor.requests.filter((request) => request.type === "attach")).toHaveLength(1);
		await connection.dispose();
	});

	it("keeps serving the session on the direct link and reconnects a lost supervisor socket", async () => {
		const supervisor = new FakeDaemonClient();
		const directRequests: DaemonCommand["type"][] = [];
		let closeSent = false;
		const direct = {
			isConnected: true,
			hello: supervisor.hello,
			supportsServerCapability: (capability: string) => supervisor.supportsServerCapability(capability),
			onMessage: () => () => {},
			onClose: () => () => {},
			request: async (command: DaemonCommand) => {
				directRequests.push(command.type);
				if (!closeSent) {
					closeSent = true;
					supervisor.connected = false;
					supervisor.emitClose(new Error("supervisor closed during direct attach"));
				}
				return {
					type: "response" as const,
					command: command.type,
					success: true as const,
					data:
						command.type === "attach"
							? createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12)
							: undefined,
				};
			},
			close: () => {},
		} as unknown as DaemonWorkerClient;
		const routed = new DaemonRoutedClient(asDaemonClient(supervisor), direct);

		const connection = await DaemonAgentConnection.attach(routed, "active-1");
		await vi.waitFor(() => expect(supervisor.reconnectCount).toBe(1));

		expect(routed.hasDirectTransport).toBe(true);
		// Held-direct recovery is control-plane only: no re-attach crosses either socket.
		expect(directRequests.filter((type) => type === "attach")).toHaveLength(1);
		expect(supervisor.requests.filter((request) => request.type === "attach")).toHaveLength(0);
		await connection.dispose();
	});

	it("rejects the fallback attach with the authoritative shutdown instead of parking it", async () => {
		const supervisor = new FakeDaemonClient();
		const attachOptions: (DaemonClientRequestOptions | undefined)[] = [];
		const originalRequest = supervisor.request.bind(supervisor);
		supervisor.request = async (command: DaemonCommand, timeoutMs?: number, options?: DaemonClientRequestOptions) => {
			if (command.type === "attach") {
				attachOptions.push(options);
				throw new Error("supervisor socket is gone");
			}
			return originalRequest(command, timeoutMs ?? 30000, options ?? {});
		};
		const closeListeners = new Set<(error: Error) => void>();
		let directConnected = true;
		const direct = {
			get isConnected() {
				return directConnected;
			},
			hello: supervisor.hello,
			supportsServerCapability: (capability: string) => supervisor.supportsServerCapability(capability),
			onMessage: () => () => {},
			onClose: (listener: (error: Error) => void) => {
				closeListeners.add(listener);
				return () => closeListeners.delete(listener);
			},
			request: async () => {
				// The authoritative stop lands while the direct link is still up; then the direct attach dies.
				supervisor.connected = false;
				supervisor.emitClose(new DaemonSocketClosedError("/tmp/prime-agent.sock", "shutdown"));
				directConnected = false;
				const error = new Error("direct attach socket closed");
				for (const listener of [...closeListeners]) listener(error);
				throw error;
			},
			close: () => {
				directConnected = false;
			},
		} as unknown as DaemonWorkerClient;
		const routed = new DaemonRoutedClient(asDaemonClient(supervisor), direct);

		await expect(DaemonAgentConnection.attach(routed, "active-1")).rejects.toThrow("Reason: shutdown");
		expect(attachOptions).toEqual([expect.objectContaining({ recoverable: false })]);
	});

	it("absorbs a direct loss inside the held roster re-attach into a session-plane reattach", async () => {
		const supervisor = new FakeDaemonClient();
		supervisor.serverCapabilities.add("agent_roster");
		const closeListeners = new Set<(error: Error) => void>();
		let directConnected = true;
		let rosterSubscribes = 0;
		const originalRequest = supervisor.request.bind(supervisor);
		supervisor.request = async (command: DaemonCommand, timeoutMs?: number, options?: DaemonClientRequestOptions) => {
			if (command.type === "roster_subscribe") {
				rosterSubscribes++;
				if (rosterSubscribes === 2) {
					directConnected = false;
					for (const listener of [...closeListeners]) listener(new Error("direct worker socket died"));
				}
			}
			return originalRequest(command, timeoutMs ?? 30000, options ?? {});
		};
		const direct = {
			get isConnected() {
				return directConnected;
			},
			hello: supervisor.hello,
			supportsServerCapability: (capability: string) => supervisor.supportsServerCapability(capability),
			onMessage: () => () => {},
			onClose: (listener: (error: Error) => void) => {
				closeListeners.add(listener);
				return () => closeListeners.delete(listener);
			},
			request: async (command: Extract<DaemonCommand, { type: "attach" }>) => ({
				type: "response" as const,
				command: "attach" as const,
				success: true as const,
				data: createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12),
			}),
			close: () => {
				directConnected = false;
			},
		} as unknown as DaemonWorkerClient;
		const routed = new DaemonRoutedClient(asDaemonClient(supervisor), direct);
		const connection = await DaemonAgentConnection.attach(routed, "active-1");
		await connection.subscribeAgentRoster(() => {});
		const events: AgentConnectionEvent[] = [];
		connection.subscribe(async (event) => {
			events.push(event);
		});
		supervisor.hello = { ...supervisor.hello! };

		supervisor.connected = false;
		supervisor.emitClose(new Error("supervisor socket lost"));

		await vi.waitFor(() => expect(events.filter((event) => event.type === "session_resynced")).toHaveLength(1));
		expect(events.some((event) => event.type === "closed")).toBe(false);
		expect(supervisor.requests.filter((request) => request.type === "attach")).toHaveLength(1);
		await connection.dispose();
	});

	it("stands down for update restoration when the update close lands inside the held roster re-attach", async () => {
		const supervisor = new FakeDaemonClient();
		supervisor.serverCapabilities.add("agent_roster");
		supervisor.updateRestartSessions = [
			{
				id: "active-restored",
				activeSessionId: "active-restored",
				sessionId: "session-current",
				sessionFile: "/tmp/session-current.jsonl",
			},
		];
		let rosterSubscribes = 0;
		const originalRequest = supervisor.request.bind(supervisor);
		supervisor.request = async (command: DaemonCommand, timeoutMs?: number, options?: DaemonClientRequestOptions) => {
			if (command.type === "roster_subscribe") {
				rosterSubscribes++;
				if (rosterSubscribes === 2) {
					supervisor.connected = false;
					supervisor.emitClose(new DaemonSocketClosedError("/tmp/prime-agent.sock", "update"));
				}
			}
			return originalRequest(command, timeoutMs ?? 30000, options ?? {});
		};
		const direct = {
			isConnected: true,
			hello: supervisor.hello,
			supportsServerCapability: (capability: string) => supervisor.supportsServerCapability(capability),
			onMessage: () => () => {},
			onClose: () => () => {},
			request: async (command: Extract<DaemonCommand, { type: "attach" }>) => ({
				type: "response" as const,
				command: "attach" as const,
				success: true as const,
				data: createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12),
			}),
			close: () => {},
		} as unknown as DaemonWorkerClient;
		const routed = new DaemonRoutedClient(asDaemonClient(supervisor), direct);
		// A short deadline would abort the restore if the held loop wrongly stayed its owner.
		const connection = await DaemonAgentConnection.attach(routed, "active-1", { reconnectTimeoutMs: 30 });
		await connection.subscribeAgentRoster(() => {});
		const events: AgentConnectionEvent[] = [];
		connection.subscribe(async (event) => {
			events.push(event);
		});
		supervisor.hello = { ...supervisor.hello! };

		supervisor.connected = false;
		supervisor.emitClose(new Error("supervisor socket lost"));

		await vi.waitFor(() => expect(events.some((event) => event.type === "session_resynced")).toBe(true), {
			timeout: 5000,
		});
		expect(events.find((event) => event.type === "session_resynced")).toMatchObject({
			snapshot: { state: { activeSessionId: "active-restored" } },
		});
		expect(events.some((event) => event.type === "closed")).toBe(false);
		await connection.dispose();
	});

	it("rebinds the roster subscription onto the recovered supervisor socket while the direct link holds", async () => {
		const supervisor = new FakeDaemonClient();
		supervisor.serverCapabilities.add("agent_roster");
		const direct = {
			isConnected: true,
			hello: supervisor.hello,
			supportsServerCapability: (capability: string) => supervisor.supportsServerCapability(capability),
			onMessage: () => () => {},
			onClose: () => () => {},
			request: async (command: Extract<DaemonCommand, { type: "attach" }>) => ({
				type: "response" as const,
				command: "attach" as const,
				success: true as const,
				data: createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12),
			}),
			close: () => {},
		} as unknown as DaemonWorkerClient;
		const routed = new DaemonRoutedClient(asDaemonClient(supervisor), direct);
		const connection = await DaemonAgentConnection.attach(routed, "active-1");
		await connection.subscribeAgentRoster(() => {});
		// A reconnect delivers a fresh hello object; the roster store keys its subscription on it.
		supervisor.hello = { ...supervisor.hello! };

		supervisor.connected = false;
		supervisor.emitClose(new Error("supervisor socket lost"));

		await vi.waitFor(() =>
			expect(supervisor.requests.filter((request) => request.type === "roster_subscribe")).toHaveLength(2),
		);
		expect(routed.hasDirectTransport).toBe(true);
		expect(supervisor.requests.filter((request) => request.type === "attach")).toHaveLength(0);
		await connection.dispose();
	});

	it("resets a half-open supervisor handshake without closing the healthy direct socket", async () => {
		const supervisor = new FakeDaemonClient();
		const direct = {
			isConnected: true,
			hello: supervisor.hello,
			supportsServerCapability: (capability: string) => supervisor.supportsServerCapability(capability),
			onMessage: () => () => {},
			onClose: () => () => {},
			request: async (command: Extract<DaemonCommand, { type: "attach" }>) => ({
				type: "response" as const,
				command: "attach" as const,
				success: true as const,
				data: createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12),
			}),
			close: () => {},
		} as unknown as DaemonWorkerClient;
		const routed = new DaemonRoutedClient(asDaemonClient(supervisor), direct);
		const connection = await DaemonAgentConnection.attach(routed, "active-1");
		const events: AgentConnectionEvent[] = [];
		connection.subscribe(async (event) => {
			events.push(event);
		});
		let helloAttempts = 0;
		supervisor.waitForHello = vi.fn(async () => {
			helloAttempts++;
			if (helloAttempts === 1) throw new Error("hello timed out on half-open socket");
			return supervisor.hello!;
		});
		supervisor.connected = false;
		supervisor.emitClose(new Error("supervisor socket closed"));

		await vi.waitFor(() =>
			expect(events.some((event) => event.type === "connection_status" && event.status === "connected")).toBe(true),
		);

		expect(supervisor.reconnectCount).toBe(2);
		expect(supervisor.resetTransportCount).toBe(1);
		expect(routed.hasDirectTransport).toBe(true);
		await connection.dispose();
	});

	it("routes an update close through update restoration and drops the stale direct link", async () => {
		const supervisor = new FakeDaemonClient();
		supervisor.updateRestartSessions = [
			{
				id: "active-restored",
				activeSessionId: "active-restored",
				sessionId: "session-current",
				sessionFile: "/tmp/session-current.jsonl",
			},
		];
		const direct = {
			isConnected: true,
			hello: supervisor.hello,
			supportsServerCapability: (capability: string) => supervisor.supportsServerCapability(capability),
			onMessage: () => () => {},
			onClose: () => () => {},
			request: async (command: Extract<DaemonCommand, { type: "attach" }>) => ({
				type: "response" as const,
				command: "attach" as const,
				success: true as const,
				data: createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12),
			}),
			close: () => {},
		} as unknown as DaemonWorkerClient;
		const routed = new DaemonRoutedClient(asDaemonClient(supervisor), direct);
		const connection = await DaemonAgentConnection.attach(routed, "active-1");
		const restored = new Promise<AgentConnectionEvent>((resolve) => {
			connection.subscribe(async (event) => {
				if (event.type === "session_resynced") resolve(event);
			});
		});

		supervisor.connected = false;
		supervisor.emitClose(new DaemonSocketClosedError("/tmp/prime-agent.sock", "update"));

		await expect(restored).resolves.toMatchObject({ type: "session_resynced" });
		expect(routed.hasDirectTransport).toBe(false);
		await connection.dispose();
	});

	it("falls back to the supervisor when the direct socket dies mid-session without recoverDaemon", async () => {
		const supervisor = new FakeDaemonClient();
		const closeListeners = new Set<(error: Error) => void>();
		let directConnected = true;
		const direct = {
			get isConnected() {
				return directConnected;
			},
			hello: supervisor.hello,
			supportsServerCapability: (capability: string) => supervisor.supportsServerCapability(capability),
			onMessage: () => () => {},
			onClose: (listener: (error: Error) => void) => {
				closeListeners.add(listener);
				return () => closeListeners.delete(listener);
			},
			request: async (command: Extract<DaemonCommand, { type: "attach" }>) => ({
				type: "response" as const,
				command: "attach" as const,
				success: true as const,
				data: createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12),
			}),
			close: () => {
				directConnected = false;
			},
		} as unknown as DaemonWorkerClient;
		const routed = new DaemonRoutedClient(asDaemonClient(supervisor), direct);
		const connection = await DaemonAgentConnection.attach(routed, "active-1");
		const events: AgentConnectionEvent[] = [];
		connection.subscribe(async (event) => {
			events.push(event);
		});
		supervisor.serverCapabilities.add("session_input_pause");
		const pause = await connection.acquireSessionInputPause("lease-1");

		directConnected = false;
		for (const listener of [...closeListeners]) listener(new Error("direct worker socket died"));

		await vi.waitFor(() => expect(events.filter((event) => event.type === "session_resynced")).toHaveLength(1));
		expect(events.some((event) => event.type === "closed")).toBe(false);
		expect(supervisor.requests.filter((request) => request.type === "attach")).toHaveLength(1);
		// The fence died with the direct link; its holder learns on release while the session lives on.
		await expect(pause.release()).rejects.toThrow("invalidated by a daemon reconnect");
		await connection.dispose();
	});

	it("takes update restoration when the direct link closes for an update while a pause is held", async () => {
		const supervisor = new FakeDaemonClient();
		supervisor.serverCapabilities.add("session_input_pause");
		supervisor.updateRestartSessions = [
			{
				id: "active-restored",
				activeSessionId: "active-restored",
				sessionId: "session-current",
				sessionFile: "/tmp/session-current.jsonl",
			},
		];
		const closeListeners = new Set<(error: Error) => void>();
		let directConnected = true;
		const direct = {
			get isConnected() {
				return directConnected;
			},
			hello: supervisor.hello,
			supportsServerCapability: (capability: string) => supervisor.supportsServerCapability(capability),
			onMessage: () => () => {},
			onClose: (listener: (error: Error) => void) => {
				closeListeners.add(listener);
				return () => closeListeners.delete(listener);
			},
			request: async (command: Extract<DaemonCommand, { type: "attach" }>) => ({
				type: "response" as const,
				command: "attach" as const,
				success: true as const,
				data: createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12),
			}),
			close: () => {
				directConnected = false;
			},
		} as unknown as DaemonWorkerClient;
		const routed = new DaemonRoutedClient(asDaemonClient(supervisor), direct);
		const connection = await DaemonAgentConnection.attach(routed, "active-1");
		const events: AgentConnectionEvent[] = [];
		connection.subscribe(async (event) => {
			events.push(event);
		});
		await connection.acquireSessionInputPause("lease-1");

		directConnected = false;
		for (const listener of [...closeListeners]) {
			listener(new DaemonSocketClosedError("/tmp/worker.sock", "update"));
		}

		await vi.waitFor(() => expect(events.some((event) => event.type === "session_resynced")).toBe(true));
		expect(events.some((event) => event.type === "closed")).toBe(false);
		await connection.dispose();
	});

	it("outlives the reconnect deadline while the direct link streams, then bounds recovery once it dies", async () => {
		const supervisor = new FakeDaemonClient();
		const closeListeners = new Set<(error: Error) => void>();
		let directConnected = true;
		const direct = {
			get isConnected() {
				return directConnected;
			},
			hello: supervisor.hello,
			supportsServerCapability: (capability: string) => supervisor.supportsServerCapability(capability),
			onMessage: () => () => {},
			onClose: (listener: (error: Error) => void) => {
				closeListeners.add(listener);
				return () => closeListeners.delete(listener);
			},
			request: async (command: Extract<DaemonCommand, { type: "attach" }>) => ({
				type: "response" as const,
				command: "attach" as const,
				success: true as const,
				data: createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12),
			}),
			close: () => {
				directConnected = false;
			},
		} as unknown as DaemonWorkerClient;
		const routed = new DaemonRoutedClient(asDaemonClient(supervisor), direct);
		const connection = await DaemonAgentConnection.attach(routed, "active-1", { reconnectTimeoutMs: 60 });
		const events: AgentConnectionEvent[] = [];
		connection.subscribe(async (event) => {
			events.push(event);
		});

		supervisor.connected = false;
		supervisor.reconnectError = new Error("supervisor is down");
		supervisor.emitClose(new Error("supervisor socket lost"));
		// Three failed control-plane attempts span well past the 60ms deadline.
		await vi.waitFor(() => expect(supervisor.resetTransportCount).toBeGreaterThanOrEqual(3));

		expect(events.some((event) => event.type === "closed")).toBe(false);
		expect(supervisor.requests.filter((request) => request.type === "attach")).toHaveLength(0);

		directConnected = false;
		for (const listener of [...closeListeners]) listener(new Error("direct worker socket died"));

		await vi.waitFor(() => expect(events.some((event) => event.type === "closed")).toBe(true), { timeout: 8000 });
		expect(events.find((event) => event.type === "closed")).toMatchObject({
			error: expect.stringContaining("Daemon reconnection failed"),
		});
		expect(events.filter((event) => event.type === "session_resynced")).toHaveLength(0);
		await connection.dispose();
	}, 10_000);

	it("keeps the source direct socket when a cross-worker reattach is rejected", async () => {
		const supervisor = new FakeDaemonClient();
		supervisor.request = vi.fn(async (command: DaemonCommand) => ({
			type: "response" as const,
			command: command.type,
			success: false as const,
			error: "target disappeared",
		}));
		const close = vi.fn();
		const direct = {
			isConnected: true,
			onMessage: () => () => {},
			onClose: () => () => {},
			close,
		} as unknown as DaemonWorkerClient;
		const routed = new DaemonRoutedClient(asDaemonClient(supervisor), direct);

		const response = await routed.request({
			type: "reattach",
			activeSessionId: "source-active",
			targetActiveSessionId: "missing-target",
		});

		expect(response.success).toBe(false);
		expect(close).not.toHaveBeenCalled();
		expect(routed.hasDirectTransport).toBe(true);
		routed.close();
	});

	it("carries an opt-out-only telemetry policy on attach", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			telemetryDisabled: true,
		});

		await connection.attach();

		expect(fakeClient.requests[0]).toMatchObject({
			type: "attach",
			activeSessionId: "active-1",
			telemetryDisabled: true,
		});
	});

	it.each([true, false])("capability-gates owned-session recovery context: %s", async (supported) => {
		const fakeClient = new FakeDaemonClient();
		if (supported) fakeClient.serverCapabilities.add("owned_session_recovery_context");
		const recoveryConfig = { cwd: "/tmp/fresh-owner" };
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-owned", {
			ownedSession: true,
			ownedSessionRecoveryConfig: recoveryConfig,
		});

		await connection.attach();

		const request = fakeClient.requests[0];
		if (supported) expect(request).toMatchObject({ recoveryConfig });
		else expect(request).not.toHaveProperty("recoveryConfig");
	});

	it("forwards queueIfBusy for prompt admission", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");

		await connection.prompt("queued input", { streamingBehavior: "followUp", queueIfBusy: true });

		expect(fakeClient.requests.at(-1)).toMatchObject({
			type: "prompt",
			activeSessionId: "active-1",
			message: "queued input",
			streamingBehavior: "followUp",
			queueIfBusy: true,
		});
	});

	it("forwards signal-backed prompts with a unique cancellable admission id", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
		const abort = new AbortController();

		await connection.prompt("startup", { signal: abort.signal, queueIfBusy: true });

		expect(fakeClient.requests.at(-1)).toMatchObject({
			type: "prompt",
			activeSessionId: "active-1",
			message: "startup",
			queueIfBusy: true,
			admissionId: expect.stringMatching(/^prompt-admission:/),
		});
	});

	it("cancels rejected signal-backed admission and removes its abort listener", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.promptError = new Error("transport failed");
		fakeClient.cancelPromptAdmissionStatus = "cancelled";
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
		const abort = new AbortController();
		const add = vi.spyOn(abort.signal, "addEventListener");
		const remove = vi.spyOn(abort.signal, "removeEventListener");

		await expect(connection.prompt("startup", { signal: abort.signal })).rejects.toMatchObject({
			message: "transport failed",
			cancelled: true,
		});
		expect(fakeClient.requests.map((request) => request.type)).toEqual(["prompt", "cancel_prompt_admission"]);
		expect(add).toHaveBeenCalledOnce();
		expect(remove).toHaveBeenCalledOnce();
		expect(remove.mock.calls[0]?.[1]).toBe(add.mock.calls[0]?.[1]);
	});

	it("preserves a definitive prompt rejection when the signal remains live", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.promptResponseError = "session rejected prompt";
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");

		await expect(connection.prompt("startup", { signal: new AbortController().signal })).rejects.toEqual(
			new Error("session rejected prompt"),
		);
		expect(fakeClient.requests.map((request) => request.type)).toEqual(["prompt"]);
	});

	it("sends zero requests for a pre-aborted prompt", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
		const abort = new AbortController();
		abort.abort();

		await expect(connection.prompt("startup", { signal: abort.signal })).rejects.toMatchObject({
			status: "cancelled",
		});
		expect(fakeClient.requests).toEqual([]);
	});

	it("accepts a successful prompt response when cancellation reports unknown", async () => {
		const fakeClient = new FakeDaemonClient();
		let releasePrompt = () => {};
		fakeClient.promptGate = new Promise<void>((resolve) => {
			releasePrompt = resolve;
		});
		fakeClient.cancelPromptAdmissionStatus = "unknown";
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
		const abort = new AbortController();

		const prompt = connection.prompt("startup", { signal: abort.signal });
		abort.abort();
		await vi.waitFor(() =>
			expect(fakeClient.requests.map((request) => request.type)).toContain("cancel_prompt_admission"),
		);
		expect(fakeClient.requests.find((request) => request.type === "cancel_prompt_admission")).not.toHaveProperty(
			"cancelOwned",
		);
		releasePrompt();

		await expect(prompt).resolves.toBeUndefined();
	});

	it("requests owned prompt cancellation when the daemon advertises it", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.serverCapabilities.add("owned_prompt_cancellation");
		let releasePrompt = () => {};
		fakeClient.promptGate = new Promise<void>((resolve) => {
			releasePrompt = resolve;
		});
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
		const abort = new AbortController();

		const prompt = connection.prompt("startup", { signal: abort.signal });
		abort.abort();
		await vi.waitFor(() =>
			expect(fakeClient.requests.map((request) => request.type)).toContain("cancel_prompt_admission"),
		);
		expect(fakeClient.requests.find((request) => request.type === "cancel_prompt_admission")).toMatchObject({
			cancelOwned: true,
		});
		releasePrompt();
		await expect(prompt).resolves.toBeUndefined();
	});

	it("preserves a definitive prompt rejection when cancellation reports owned", async () => {
		const fakeClient = new FakeDaemonClient();
		let releasePrompt = () => {};
		fakeClient.promptGate = new Promise<void>((resolve) => {
			releasePrompt = resolve;
		});
		fakeClient.promptResponseError = "post-ownership validation failed";
		fakeClient.cancelPromptAdmissionStatus = "owned";
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
		const abort = new AbortController();

		const prompt = connection.prompt("startup", { signal: abort.signal });
		abort.abort();
		await vi.waitFor(() =>
			expect(fakeClient.requests.map((request) => request.type)).toContain("cancel_prompt_admission"),
		);
		releasePrompt();

		await expect(prompt).rejects.toThrow("post-ownership validation failed");
	});

	it("revives an archived session from its saved file when a prompt hits an unknown session", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		const events: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			events.push(event);
		});
		fakeClient.deadActiveSessionIds.add("active-1");

		await connection.prompt("continue");

		expect(fakeClient.requests.find((request) => request.type === "create")).toMatchObject({
			sessionPath: "/tmp/session-current.jsonl",
			continueRecent: false,
		});
		const promptRequests = fakeClient.requests.filter((request) => request.type === "prompt");
		expect(promptRequests.map((request) => request.activeSessionId)).toEqual(["active-1", "active-revived"]);
		await vi.waitFor(() => expect(events.some((event) => event.type === "session_resynced")).toBe(true));
	});

	it("revives an archived session for a signal-carrying prompt", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		fakeClient.deadActiveSessionIds.add("active-1");

		await connection.prompt("continue", { signal: new AbortController().signal });

		const promptRequests = fakeClient.requests.filter((request) => request.type === "prompt");
		expect(promptRequests.map((request) => request.activeSessionId)).toEqual(["active-1", "active-revived"]);
	});

	it("does not revive when the prompt's admission was cancelled by its abort signal", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		fakeClient.deadActiveSessionIds.add("active-1");
		fakeClient.cancelPromptAdmissionStatus = "cancelled";
		let releasePrompt = () => {};
		fakeClient.promptGate = new Promise<void>((resolve) => {
			releasePrompt = resolve;
		});
		const controller = new AbortController();

		const prompt = connection.prompt("continue", { signal: controller.signal });
		await vi.waitFor(() =>
			expect(fakeClient.requests.filter((request) => request.type === "prompt")).toHaveLength(1),
		);
		// The abort races the unknown-session response: the admission wrapper
		// rethrows the failure carrying the same message, which must not
		// authorize a revival that restarts the session the user just
		// cancelled out of.
		controller.abort();
		releasePrompt();

		await expect(prompt).rejects.toThrow("Unknown active session: active-1");
		expect(fakeClient.requests.filter((request) => request.type === "create")).toHaveLength(0);
		const promptRequests = fakeClient.requests.filter((request) => request.type === "prompt");
		expect(promptRequests.map((request) => request.activeSessionId)).toEqual(["active-1"]);
	});

	it("surfaces the original unknown-session error when revival fails", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.createResponseError = "Session not found: /tmp/session-current.jsonl";
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		fakeClient.deadActiveSessionIds.add("active-1");

		await expect(connection.prompt("continue")).rejects.toThrow("Unknown active session: active-1");
		expect(fakeClient.requests.filter((request) => request.type === "prompt")).toHaveLength(1);
	});

	it("does not attempt revival without a saved session file", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
		fakeClient.deadActiveSessionIds.add("active-1");

		await expect(connection.prompt("continue")).rejects.toThrow("Unknown active session: active-1");
		expect(fakeClient.requests.map((request) => request.type)).toEqual(["prompt"]);
	});

	it("revives via promptAndWait and detaches the dead session id", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		fakeClient.deadActiveSessionIds.add("active-1");

		await connection.promptAndWait("continue");

		const waitRequests = fakeClient.requests.filter((request) => request.type === "prompt_and_wait");
		expect(waitRequests.map((request) => request.activeSessionId)).toEqual(["active-1", "active-revived"]);
		await vi.waitFor(() =>
			expect(fakeClient.requests).toContainEqual(
				expect.objectContaining({ type: "detach", activeSessionId: "active-1" }),
			),
		);
	});

	it("ignores an unknown-session error naming a different session than the prompt targeted", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.promptResponseError = "Unknown active session: some-other-session";
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");

		await expect(connection.prompt("continue")).rejects.toThrow("Unknown active session: some-other-session");
		expect(fakeClient.requests.filter((request) => request.type === "create")).toHaveLength(0);
	});

	it("rolls back the binding when the post-revival attach fails, then recovers on the next prompt", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		fakeClient.deadActiveSessionIds.add("active-1");
		fakeClient.attachFailures = 1;

		await expect(connection.prompt("first")).rejects.toThrow("Unknown active session: active-1");
		// The rolled-back binding must keep targeting the original session, not
		// the half-revived one this client never attached to.
		const promptsAfterRollback = fakeClient.requests.filter((request) => request.type === "prompt");
		expect(promptsAfterRollback.map((request) => request.activeSessionId)).toEqual(["active-1"]);

		await connection.prompt("second");
		const prompts = fakeClient.requests.filter((request) => request.type === "prompt");
		expect(prompts.map((request) => request.activeSessionId)).toEqual(["active-1", "active-1", "active-revived"]);
	});

	it("shares a single revival between concurrent failing prompts", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		fakeClient.deadActiveSessionIds.add("active-1");
		let releaseCreate = () => {};
		fakeClient.createGate = new Promise<void>((resolve) => {
			releaseCreate = resolve;
		});

		const first = connection.prompt("first");
		const second = connection.prompt("second");
		await vi.waitFor(() =>
			expect(fakeClient.requests.filter((request) => request.type === "create")).toHaveLength(1),
		);
		releaseCreate();
		await Promise.all([first, second]);

		expect(fakeClient.requests.filter((request) => request.type === "create")).toHaveLength(1);
		const revivedPrompts = fakeClient.requests.filter(
			(request) => request.type === "prompt" && request.activeSessionId === "active-revived",
		);
		expect(revivedPrompts).toHaveLength(2);
	});

	it("releases an owned session revived after disposal began without touching its attachment", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1", {
			ownedSession: true,
		});
		fakeClient.deadActiveSessionIds.add("active-1");
		let releaseCreate = () => {};
		fakeClient.createGate = new Promise<void>((resolve) => {
			releaseCreate = resolve;
		});

		const prompt = connection.prompt("continue");
		await vi.waitFor(() =>
			expect(fakeClient.requests.filter((request) => request.type === "create")).toHaveLength(1),
		);
		const dispose = connection.dispose();
		releaseCreate();
		await expect(prompt).rejects.toThrow("Unknown active session: active-1");
		await dispose;

		// The create claimed ownership, so the owned session is completed; but
		// this revival never attached, so no detach may be issued — on a shared
		// socket it would deafen a sibling connection attached to the same id.
		await vi.waitFor(() =>
			expect(fakeClient.requests).toContainEqual(
				expect.objectContaining({ type: "complete_owned_session", activeSessionId: "active-revived" }),
			),
		);
		expect(fakeClient.requests).not.toContainEqual(
			expect.objectContaining({ type: "detach", activeSessionId: "active-revived" }),
		);
	});

	it("does not detach a revived id a sibling connection is attached to on the shared client", async () => {
		const fakeClient = new FakeDaemonClient();
		const sibling = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-revived");
		const siblingEvents: AgentConnectionEvent[] = [];
		sibling.subscribe((event) => {
			siblingEvents.push(event);
		});
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		fakeClient.deadActiveSessionIds.add("active-1");
		let releaseCreate = () => {};
		fakeClient.createGate = new Promise<void>((resolve) => {
			releaseCreate = resolve;
		});

		const prompt = connection.prompt("continue");
		await vi.waitFor(() =>
			expect(fakeClient.requests.filter((request) => request.type === "create")).toHaveLength(1),
		);
		const dispose = connection.dispose();
		releaseCreate();
		await expect(prompt).rejects.toThrow("Unknown active session: active-1");
		await dispose;

		expect(fakeClient.requests).not.toContainEqual(
			expect.objectContaining({ type: "detach", activeSessionId: "active-revived" }),
		);
		// The sibling must still be live on the shared socket.
		emitSequencedQueueUpdate(fakeClient, "active-revived", 13);
		await vi.waitFor(() => expect(siblingEvents.some((event) => event.type === "session_event")).toBe(true));
	});

	it("propagates the client-owned lifecycle when reviving an owned session", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1", {
			ownedSession: true,
		});
		fakeClient.deadActiveSessionIds.add("active-1");

		await connection.prompt("continue");

		expect(fakeClient.requests.find((request) => request.type === "create")).toMatchObject({
			sessionPath: "/tmp/session-current.jsonl",
			lifecycle: "client_owned",
		});
	});

	it("completes an owned revived session when the post-revival attach fails", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1", {
			ownedSession: true,
		});
		fakeClient.deadActiveSessionIds.add("active-1");
		fakeClient.attachFailures = 1;

		await expect(connection.prompt("continue")).rejects.toThrow("Unknown active session: active-1");
		// An owned revived worker must not outlive the failed revival: the
		// failed attach releases it with complete_owned_session. The attach
		// THREW without a supersede, so it acquired no attachment - a detach
		// here could deafen a sibling connection on the shared socket.
		await vi.waitFor(() => {
			expect(fakeClient.requests).toContainEqual(
				expect.objectContaining({ type: "complete_owned_session", activeSessionId: "active-revived" }),
			);
		});
		expect(fakeClient.requests).not.toContainEqual(
			expect.objectContaining({ type: "detach", activeSessionId: "active-revived" }),
		);
	});

	it("ignores an unknown-session error whose id the attempted id merely prefixes", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.promptResponseError = "Unknown active session: active-10";
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");

		await expect(connection.prompt("continue")).rejects.toThrow("Unknown active session: active-10");
		expect(fakeClient.requests.filter((request) => request.type === "create")).toHaveLength(0);
	});

	it("keeps prompts on the dead binding until the revived attach completes", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		fakeClient.deadActiveSessionIds.add("active-1");
		let releaseAttach = () => {};
		fakeClient.revivedAttachGate = new Promise<void>((resolve) => {
			releaseAttach = resolve;
		});

		const first = connection.prompt("first");
		await vi.waitFor(() =>
			expect(fakeClient.requests.filter((request) => request.type === "attach")).toHaveLength(2),
		);
		// The revived binding must not be observable while its attach is still in
		// flight: a prompt started now targets the (dead) published binding and
		// joins the revival instead of racing ahead of the attach.
		const third = connection.prompt("third");
		await vi.waitFor(() =>
			expect(fakeClient.requests.filter((request) => request.type === "prompt")).toHaveLength(2),
		);
		releaseAttach();
		await Promise.all([first, third]);

		expect(fakeClient.requests.filter((request) => request.type === "create")).toHaveLength(1);
		const prompts = fakeClient.requests.filter((request) => request.type === "prompt");
		expect(prompts.map((request) => request.activeSessionId)).toEqual([
			"active-1",
			"active-1",
			"active-revived",
			"active-revived",
		]);
	});

	it("cedes to a session switch that lands during revival without detaching the unacquired id", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		fakeClient.deadActiveSessionIds.add("active-1");
		fakeClient.switchSessionAlreadyActiveId = "active-b";
		let releaseCreate = () => {};
		fakeClient.createGate = new Promise<void>((resolve) => {
			releaseCreate = resolve;
		});

		const prompt = connection.prompt("continue");
		await vi.waitFor(() =>
			expect(fakeClient.requests.filter((request) => request.type === "create")).toHaveLength(1),
		);
		await connection.switchSession("/tmp/session-b.jsonl");
		releaseCreate();
		await expect(prompt).rejects.toThrow("Unknown active session: active-1");

		// The revival must not stomp the switched binding, and — having never
		// attached to the revived id — must not detach it either (on a shared
		// socket that could deafen a sibling attached to the same id). The
		// revived worker stays resident for the idle sweeps.
		expect(fakeClient.requests).not.toContainEqual(
			expect.objectContaining({ type: "detach", activeSessionId: "active-revived" }),
		);
		await connection.prompt("next");
		const prompts = fakeClient.requests.filter((request) => request.type === "prompt");
		expect(prompts[prompts.length - 1]).toMatchObject({ activeSessionId: "active-b" });
		expect(prompts.every((request) => request.activeSessionId !== "active-revived")).toBe(true);
	});

	it("does not release the revived session when a concurrent switch published the same binding", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		fakeClient.deadActiveSessionIds.add("active-1");
		// The concurrent switch lands on the SAME session the create revives:
		// the daemon reports it as already active under the revived id.
		fakeClient.switchSessionAlreadyActiveId = "active-revived";
		let releaseCreate = () => {};
		fakeClient.createGate = new Promise<void>((resolve) => {
			releaseCreate = resolve;
		});

		const prompt = connection.prompt("continue");
		await vi.waitFor(() =>
			expect(fakeClient.requests.filter((request) => request.type === "create")).toHaveLength(1),
		);
		await connection.switchSession("/tmp/session-current.jsonl");
		releaseCreate();
		await expect(prompt).rejects.toThrow("Unknown active session: active-1");

		// The switch owns the binding and it IS the revived session: releasing
		// it would detach the currently published binding and leave the
		// connection deaf to daemon events.
		await connection.prompt("next");
		expect(fakeClient.requests).not.toContainEqual(
			expect.objectContaining({ type: "detach", activeSessionId: "active-revived" }),
		);
		expect(fakeClient.requests).not.toContainEqual(
			expect.objectContaining({ type: "complete_owned_session", activeSessionId: "active-revived" }),
		);
		const prompts = fakeClient.requests.filter((request) => request.type === "prompt");
		expect(prompts[prompts.length - 1]).toMatchObject({ activeSessionId: "active-revived" });
	});

	it("admits revived-session snapshot frames that arrive before the attach response continuation", async () => {
		const fakeClient = new FakeDaemonClient();
		const messages: AgentMessage[] = [{ role: "user", content: "revived history", timestamp: 1 }];
		fakeClient.attachResultFactory = (command) => {
			if (command.activeSessionId !== "active-revived") {
				return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
			}
			const full = createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 23, {
				state: createConnectionState(command.activeSessionId, "session-revived"),
				messages,
			});
			const { messages: _messages, ...snapshotHeader } = full.snapshot;
			// Model the daemon writing the response and the snapshot frames into
			// one socket buffer: the frames are processed before the awaiting
			// attach continuation publishes the revived binding.
			queueMicrotask(() => {
				fakeClient.emitMessage({
					type: "session_snapshot_begin",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-revived",
					snapshot: snapshotHeader,
					messageCount: messages.length,
					targetChunkBytes: 512 * 1024,
				});
				fakeClient.emitMessage({
					type: "session_snapshot_chunk",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-revived",
					index: 0,
					messages: [messages[0]!],
				});
				fakeClient.emitMessage({
					type: "session_snapshot_end",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-revived",
					chunkCount: 1,
					lastEventSequence: 23,
				});
			});
			return {
				...full,
				snapshot: { ...full.snapshot, messages: [] },
				snapshotStream: {
					id: "snapshot-revived",
					messageCount: messages.length,
					targetChunkBytes: 512 * 1024,
				},
			};
		};
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1", {
			snapshotTimeoutMs: 200,
		});
		const events: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			events.push(event);
		});
		fakeClient.deadActiveSessionIds.add("active-1");

		await connection.prompt("continue");

		const prompts = fakeClient.requests.filter((request) => request.type === "prompt");
		expect(prompts.map((request) => request.activeSessionId)).toEqual(["active-1", "active-revived"]);
		await vi.waitFor(() => expect(events.some((event) => event.type === "session_resynced")).toBe(true));
		const resynced = events.find(
			(event): event is Extract<AgentConnectionEvent, { type: "session_resynced" }> =>
				event.type === "session_resynced",
		);
		expect(resynced?.snapshot.messages).toEqual(messages);
	});

	it("retries the resync in the background when a legacy revived attach cannot read its snapshot", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.attachResultFactory = (command) => {
			if (command.activeSessionId !== "active-revived") {
				return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
			}
			// Legacy daemons answer attach with a bare summary; the snapshot then
			// comes from separate reads that can fail transiently.
			return {
				id: command.activeSessionId,
				activeSessionId: command.activeSessionId,
				sessionId: "session-revived",
				sessionFile: "/tmp/session-revived.jsonl",
			} as unknown as DaemonAttachResult;
		};
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		const events: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			events.push(event);
		});
		fakeClient.deadActiveSessionIds.add("active-1");
		fakeClient.connectionStateFailures = 1;

		// The prompt must succeed even though the revival's resync read failed…
		await connection.prompt("continue");
		const prompts = fakeClient.requests.filter((request) => request.type === "prompt");
		expect(prompts.map((request) => request.activeSessionId)).toEqual(["active-1", "active-revived"]);
		expect(events.some((event) => event.type === "session_resynced")).toBe(false);

		// …and the background retry must deliver the resync so the window does
		// not keep rendering the dead transcript.
		await vi.waitFor(() => expect(events.some((event) => event.type === "session_resynced")).toBe(true), {
			timeout: 3000,
		});
	});

	it("does not retry into a transcript replaced by an in-worker switch during revival", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.attachResultFactory = (command) => {
			if (command.activeSessionId !== "active-revived") {
				return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
			}
			// Bare-summary attach: the revival's snapshot comes from separate
			// reads, giving a concurrent in-worker switch a window to land
			// during the revival's tail.
			return {
				id: command.activeSessionId,
				activeSessionId: command.activeSessionId,
				sessionId: "session-revived",
				sessionFile: "/tmp/session-current.jsonl",
			} as unknown as DaemonAttachResult;
		};
		let releaseStateRead = () => {};
		fakeClient.connectionStateGate = new Promise<void>((resolve) => {
			releaseStateRead = resolve;
		});
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		const events: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			events.push(event);
		});
		fakeClient.deadActiveSessionIds.add("active-1");

		const prompt = connection.prompt("continue");
		await vi.waitFor(() =>
			expect(
				fakeClient.requests.filter(
					(request) => request.type === "get_connection_state" && request.activeSessionId === "active-revived",
				),
			).toHaveLength(1),
		);
		// An in-worker switch replaces the runtime transcript WITHOUT changing
		// the active-session id: the daemon streams a replacement snapshot for
		// the newly selected session under the same id.
		const replacement = createAttachResult("active-revived", undefined, undefined, 40, {
			state: createConnectionState("active-revived", "session-b"),
		});
		const { messages: _replacementMessages, ...replacementHeader } = replacement.snapshot;
		fakeClient.emitMessage({
			type: "session_snapshot_begin",
			activeSessionId: "active-revived",
			snapshotId: "snapshot-inworker-switch",
			snapshot: replacementHeader,
			messageCount: 0,
			targetChunkBytes: 512 * 1024,
			purpose: "replacement",
		});
		fakeClient.emitMessage({
			type: "session_snapshot_end",
			activeSessionId: "active-revived",
			snapshotId: "snapshot-inworker-switch",
			chunkCount: 0,
			lastEventSequence: 40,
		});
		releaseStateRead();

		// The failed prompt targeted the revived transcript; re-sending it now
		// would inject it into the switched one. Surface the original error.
		await expect(prompt).rejects.toThrow("Unknown active session: active-1");
		const prompts = fakeClient.requests.filter((request) => request.type === "prompt");
		expect(prompts.map((request) => request.activeSessionId)).toEqual(["active-1"]);
		// And the revival's own resync must not follow the session_replaced
		// with a pre-switch snapshot, which would revert the window to the
		// replaced transcript.
		expect(events.some((event) => event.type === "session_replaced")).toBe(true);
		expect(events.filter((event) => event.type === "session_resynced")).toHaveLength(0);
	});

	it("carries the telemetry opt-out into the revived worker's create command", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1", {
			telemetryDisabled: true,
		});
		fakeClient.deadActiveSessionIds.add("active-1");

		await connection.prompt("continue");

		expect(fakeClient.requests.find((request) => request.type === "create")).toMatchObject({
			sessionPath: "/tmp/session-current.jsonl",
			config: { telemetryDisabled: true },
		});
	});

	it("does not revive or retry when the binding moved to another session before the failure", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		fakeClient.deadActiveSessionIds.add("active-1");
		fakeClient.switchSessionAlreadyActiveId = "active-b";
		let releasePrompt = () => {};
		fakeClient.promptGate = new Promise<void>((resolve) => {
			releasePrompt = resolve;
		});

		const prompt = connection.prompt("continue");
		await vi.waitFor(() =>
			expect(fakeClient.requests.filter((request) => request.type === "prompt")).toHaveLength(1),
		);
		await connection.switchSession("/tmp/session-b.jsonl");
		releasePrompt();

		// The prompt targeted the dead session; after the user switched to
		// another transcript the failure must surface instead of the message
		// being revived-and-retried into the switched session.
		await expect(prompt).rejects.toThrow("Unknown active session: active-1");
		expect(fakeClient.requests.filter((request) => request.type === "create")).toHaveLength(0);
		expect(fakeClient.requests.filter((request) => request.type === "prompt")).toHaveLength(1);
	});

	it("recovers via snapshot reads when the revived attach stream fails after publication", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.attachResultFactory = (command) => {
			if (command.activeSessionId !== "active-revived") {
				return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
			}
			const full = createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 23);
			queueMicrotask(() => {
				fakeClient.emitMessage({
					type: "session_snapshot_failed",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-revive-fail",
					error: "snapshot encoder failed",
				});
			});
			return {
				...full,
				snapshot: { ...full.snapshot, messages: [] },
				snapshotStream: { id: "snapshot-revive-fail", messageCount: 0, targetChunkBytes: 512 * 1024 },
			};
		};
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		const events: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			events.push(event);
		});
		fakeClient.deadActiveSessionIds.add("active-1");

		// The attach response was applied (binding published, client attached);
		// only the streamed snapshot failed. The revival must not strand that
		// coherent binding: it falls through to the snapshot reads and the
		// prompt is still delivered.
		await connection.prompt("continue");

		const prompts = fakeClient.requests.filter((request) => request.type === "prompt");
		expect(prompts.map((request) => request.activeSessionId)).toEqual(["active-1", "active-revived"]);
		await vi.waitFor(() => expect(events.some((event) => event.type === "session_resynced")).toBe(true));
	});

	it("discards a late revived snapshot when a switch lands during the stream and releases the revived session", async () => {
		const fakeClient = new FakeDaemonClient();
		// A negotiated ownership response proves this revival created the
		// attachment, so superseded cleanup can safely release it.
		fakeClient.serverCapabilities.add("attach_ownership");
		let emitRevivedSnapshot = () => {};
		fakeClient.attachResultFactory = (command) => {
			if (command.activeSessionId !== "active-revived") {
				return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
			}
			const full = createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 23, {
				state: createConnectionState(command.activeSessionId, "session-revived"),
			});
			const { messages: _messages, ...snapshotHeader } = full.snapshot;
			emitRevivedSnapshot = () => {
				fakeClient.emitMessage({
					type: "session_snapshot_begin",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-revive-race",
					snapshot: snapshotHeader,
					messageCount: 0,
					targetChunkBytes: 512 * 1024,
				});
				fakeClient.emitMessage({
					type: "session_snapshot_end",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-revive-race",
					chunkCount: 0,
					lastEventSequence: 23,
				});
			};
			return {
				...full,
				snapshot: { ...full.snapshot, messages: [] },
				snapshotStream: { id: "snapshot-revive-race", messageCount: 0, targetChunkBytes: 512 * 1024 },
			};
		};
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		const events: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			events.push(event);
		});
		fakeClient.deadActiveSessionIds.add("active-1");
		fakeClient.switchSessionAlreadyActiveId = "active-b";

		const prompt = connection.prompt("continue");
		// Wait until the revived attach response has been applied (its snapshot
		// stream is still pending), then complete a switch to another session.
		await vi.waitFor(() =>
			expect(
				fakeClient.requests.filter(
					(request) => request.type === "attach" && request.activeSessionId === "active-revived",
				),
			).toHaveLength(1),
		);
		await connection.switchSession("/tmp/session-b.jsonl");
		emitRevivedSnapshot();

		await expect(prompt).rejects.toThrow("Unknown active session: active-1");
		// The late revived snapshot must not masquerade as the switched session:
		// no resync describing the revived session, and the revived attachment
		// is released.
		const resyncs = events.filter(
			(event): event is Extract<AgentConnectionEvent, { type: "session_resynced" }> =>
				event.type === "session_resynced",
		);
		expect(resyncs.every((event) => event.snapshot.state.sessionId !== "session-revived")).toBe(true);
		await vi.waitFor(() =>
			expect(fakeClient.requests).toContainEqual(
				expect.objectContaining({ type: "detach", activeSessionId: "active-revived" }),
			),
		);
		await connection.prompt("next");
		const prompts = fakeClient.requests.filter((request) => request.type === "prompt");
		expect(prompts[prompts.length - 1]).toMatchObject({ activeSessionId: "active-b" });
	});

	it("drops non-snapshot frames addressed to a pending revival target", async () => {
		const fakeClient = new FakeDaemonClient();
		let emitRevivedSnapshot = () => {};
		fakeClient.attachResultFactory = (command) => {
			if (command.activeSessionId !== "active-revived") {
				return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
			}
			const full = createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 23);
			const { messages: _messages, ...snapshotHeader } = full.snapshot;
			emitRevivedSnapshot = () => {
				fakeClient.emitMessage({
					type: "session_snapshot_begin",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-revive-filter",
					snapshot: snapshotHeader,
					messageCount: 0,
					targetChunkBytes: 512 * 1024,
				});
				fakeClient.emitMessage({
					type: "session_snapshot_end",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-revive-filter",
					chunkCount: 0,
					lastEventSequence: 23,
				});
			};
			return {
				...full,
				snapshot: { ...full.snapshot, messages: [] },
				snapshotStream: { id: "snapshot-revive-filter", messageCount: 0, targetChunkBytes: 512 * 1024 },
			};
		};
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		const events: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			events.push(event);
		});
		fakeClient.deadActiveSessionIds.add("active-1");
		fakeClient.switchSessionAlreadyActiveId = "active-b";

		const prompt = connection.prompt("continue");
		await vi.waitFor(() =>
			expect(
				fakeClient.requests.filter(
					(request) => request.type === "attach" && request.activeSessionId === "active-revived",
				),
			).toHaveLength(1),
		);
		// The switch supersedes the revival while its snapshot is still in
		// flight; the revived id stays admitted (pending) until the waiter
		// finishes. Live frames addressed to it must be dropped: admitting the
		// session_closed would emit a terminal close for a session this window
		// is no longer showing.
		await connection.switchSession("/tmp/session-b.jsonl");
		fakeClient.emitMessage({ type: "session_closed", activeSessionId: "active-revived", reason: "killed" });
		emitRevivedSnapshot();
		await expect(prompt).rejects.toThrow("Unknown active session: active-1");

		expect(events.filter((event) => event.type === "closed")).toHaveLength(0);
		await connection.prompt("next");
		const prompts = fakeClient.requests.filter((request) => request.type === "prompt");
		expect(prompts[prompts.length - 1]).toMatchObject({ activeSessionId: "active-b" });
	});

	it("applies a catch-up resync buffered while the revived binding was pending", async () => {
		const fakeClient = new FakeDaemonClient();
		const catchupMessages: AgentMessage[] = [{ role: "user", content: "caught-up", timestamp: 9 }];
		fakeClient.attachResultFactory = (command) => {
			if (command.activeSessionId !== "active-revived") {
				return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
			}
			const full = createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 23, {
				state: createConnectionState(command.activeSessionId, "session-revived"),
			});
			const { messages: _messages, ...snapshotHeader } = full.snapshot;
			queueMicrotask(() => {
				// The attach snapshot and a daemon catch-up resync (events that
				// landed during the attach) share the socket buffer and complete
				// before the attach continuation publishes the revived binding.
				fakeClient.emitMessage({
					type: "session_snapshot_begin",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-revive-attach",
					snapshot: snapshotHeader,
					messageCount: 0,
					targetChunkBytes: 512 * 1024,
				});
				fakeClient.emitMessage({
					type: "session_snapshot_end",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-revive-attach",
					chunkCount: 0,
					lastEventSequence: 23,
				});
				fakeClient.emitMessage({
					type: "session_snapshot_begin",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-revive-catchup",
					snapshot: snapshotHeader,
					messageCount: catchupMessages.length,
					targetChunkBytes: 512 * 1024,
					purpose: "resync",
				});
				fakeClient.emitMessage({
					type: "session_snapshot_chunk",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-revive-catchup",
					index: 0,
					messages: [catchupMessages[0]!],
				});
				fakeClient.emitMessage({
					type: "session_snapshot_end",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-revive-catchup",
					chunkCount: 1,
					lastEventSequence: 30,
				});
			});
			return {
				...full,
				snapshot: { ...full.snapshot, messages: [] },
				snapshotStream: { id: "snapshot-revive-attach", messageCount: 0, targetChunkBytes: 512 * 1024 },
			};
		};
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		const events: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			events.push(event);
		});
		fakeClient.deadActiveSessionIds.add("active-1");

		await connection.prompt("continue");

		await vi.waitFor(() => expect(events.some((event) => event.type === "session_resynced")).toBe(true));
		const resynced = events.find(
			(event): event is Extract<AgentConnectionEvent, { type: "session_resynced" }> =>
				event.type === "session_resynced",
		);
		// The resync delivered after revival must carry the buffered catch-up
		// (the newer events), not the older attach snapshot it raced.
		expect(resynced?.snapshot.messages).toEqual(catchupMessages);
		expect(resynced?.snapshot.lastEventSequence).toBe(30);
	});

	it("does not run terminal snapshot recovery for a catch-up that fails while its target is pending", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.attachResultFactory = (command) => {
			if (command.activeSessionId !== "active-revived") {
				return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
			}
			const full = createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 23, {
				state: createConnectionState(command.activeSessionId, "session-revived"),
			});
			const { messages: _messages, ...snapshotHeader } = full.snapshot;
			queueMicrotask(() => {
				// The attach snapshot completes, and a catch-up resync FAILS, both
				// before the attach continuation publishes the revived binding.
				fakeClient.emitMessage({
					type: "session_snapshot_begin",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-revive-attach2",
					snapshot: snapshotHeader,
					messageCount: 0,
					targetChunkBytes: 512 * 1024,
				});
				fakeClient.emitMessage({
					type: "session_snapshot_end",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-revive-attach2",
					chunkCount: 0,
					lastEventSequence: 23,
				});
				fakeClient.emitMessage({
					type: "session_snapshot_begin",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-catchup-fail",
					snapshot: snapshotHeader,
					messageCount: 1,
					targetChunkBytes: 512 * 1024,
					purpose: "resync",
				});
				fakeClient.emitMessage({
					type: "session_snapshot_failed",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-catchup-fail",
					error: "catch-up encoder failed",
				});
			});
			return {
				...full,
				snapshot: { ...full.snapshot, messages: [] },
				snapshotStream: { id: "snapshot-revive-attach2", messageCount: 0, targetChunkBytes: 512 * 1024 },
			};
		};
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		const events: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			events.push(event);
		});
		fakeClient.deadActiveSessionIds.add("active-1");

		// Recovery for the failed pending catch-up would read state against the
		// still-published archived selector, fail, and emit a terminal close —
		// killing the window even though the revival is about to succeed.
		await connection.prompt("continue");

		expect(events.filter((event) => event.type === "closed")).toHaveLength(0);
		const prompts = fakeClient.requests.filter((request) => request.type === "prompt");
		expect(prompts.map((request) => request.activeSessionId)).toEqual(["active-1", "active-revived"]);
		await vi.waitFor(() => expect(events.some((event) => event.type === "session_resynced")).toBe(true));
		// The failed catch-up carried events the cached attach snapshot predates;
		// the revival's resync must come from a fresh daemon read, not the cache.
		const resynced = events.find(
			(event): event is Extract<AgentConnectionEvent, { type: "session_resynced" }> =>
				event.type === "session_resynced",
		);
		expect(resynced?.snapshot.messages).toEqual([{ role: "user", content: "current prompt", timestamp: 4 }]);
	});

	it("prefers a later successful catch-up over an earlier failed one for a pending target", async () => {
		const fakeClient = new FakeDaemonClient();
		const catchupMessages: AgentMessage[] = [{ role: "user", content: "caught-up-after-failure", timestamp: 11 }];
		fakeClient.attachResultFactory = (command) => {
			if (command.activeSessionId !== "active-revived") {
				return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
			}
			const full = createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 23, {
				state: createConnectionState(command.activeSessionId, "session-revived"),
			});
			const { messages: _messages, ...snapshotHeader } = full.snapshot;
			queueMicrotask(() => {
				fakeClient.emitMessage({
					type: "session_snapshot_begin",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-revive-attach3",
					snapshot: snapshotHeader,
					messageCount: 0,
					targetChunkBytes: 512 * 1024,
				});
				fakeClient.emitMessage({
					type: "session_snapshot_end",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-revive-attach3",
					chunkCount: 0,
					lastEventSequence: 23,
				});
				fakeClient.emitMessage({
					type: "session_snapshot_begin",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-catchup-fail2",
					snapshot: snapshotHeader,
					messageCount: 1,
					targetChunkBytes: 512 * 1024,
					purpose: "resync",
				});
				fakeClient.emitMessage({
					type: "session_snapshot_failed",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-catchup-fail2",
					error: "catch-up encoder failed",
				});
				fakeClient.emitMessage({
					type: "session_snapshot_begin",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-catchup-late",
					snapshot: snapshotHeader,
					messageCount: catchupMessages.length,
					targetChunkBytes: 512 * 1024,
					purpose: "resync",
				});
				fakeClient.emitMessage({
					type: "session_snapshot_chunk",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-catchup-late",
					index: 0,
					messages: [catchupMessages[0]!],
				});
				fakeClient.emitMessage({
					type: "session_snapshot_end",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-catchup-late",
					chunkCount: 1,
					lastEventSequence: 31,
				});
			});
			return {
				...full,
				snapshot: { ...full.snapshot, messages: [] },
				snapshotStream: { id: "snapshot-revive-attach3", messageCount: 0, targetChunkBytes: 512 * 1024 },
			};
		};
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		const events: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			events.push(event);
		});
		fakeClient.deadActiveSessionIds.add("active-1");

		await connection.prompt("continue");

		await vi.waitFor(() => expect(events.some((event) => event.type === "session_resynced")).toBe(true));
		const resynced = events.find(
			(event): event is Extract<AgentConnectionEvent, { type: "session_resynced" }> =>
				event.type === "session_resynced",
		);
		// Newest wins: the successful catch-up supersedes the earlier failure, is
		// applied from the buffer, and no re-read is forced.
		expect(resynced?.snapshot.messages).toEqual(catchupMessages);
		expect(resynced?.snapshot.lastEventSequence).toBe(31);
		expect(fakeClient.requests.filter((request) => request.type === "get_connection_state")).toHaveLength(0);
	});

	it("drops a buffered catch-up when a later one fails for a pending target", async () => {
		const fakeClient = new FakeDaemonClient();
		const staleCatchupMessages: AgentMessage[] = [{ role: "user", content: "stale-catch-up", timestamp: 10 }];
		fakeClient.attachResultFactory = (command) => {
			if (command.activeSessionId !== "active-revived") {
				return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
			}
			const full = createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 23, {
				state: createConnectionState(command.activeSessionId, "session-revived"),
			});
			const { messages: _messages, ...snapshotHeader } = full.snapshot;
			queueMicrotask(() => {
				fakeClient.emitMessage({
					type: "session_snapshot_begin",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-revive-attach4",
					snapshot: snapshotHeader,
					messageCount: 0,
					targetChunkBytes: 512 * 1024,
				});
				fakeClient.emitMessage({
					type: "session_snapshot_end",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-revive-attach4",
					chunkCount: 0,
					lastEventSequence: 23,
				});
				// A catch-up SUCCEEDS at T1…
				fakeClient.emitMessage({
					type: "session_snapshot_begin",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-catchup-t1",
					snapshot: snapshotHeader,
					messageCount: staleCatchupMessages.length,
					targetChunkBytes: 512 * 1024,
					purpose: "resync",
				});
				fakeClient.emitMessage({
					type: "session_snapshot_chunk",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-catchup-t1",
					index: 0,
					messages: [staleCatchupMessages[0]!],
				});
				fakeClient.emitMessage({
					type: "session_snapshot_end",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-catchup-t1",
					chunkCount: 1,
					lastEventSequence: 27,
				});
				// …and a NEWER catch-up fails at T2: the buffered T1 snapshot now
				// predates the events the failed catch-up carried.
				fakeClient.emitMessage({
					type: "session_snapshot_begin",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-catchup-t2",
					snapshot: snapshotHeader,
					messageCount: 1,
					targetChunkBytes: 512 * 1024,
					purpose: "resync",
				});
				fakeClient.emitMessage({
					type: "session_snapshot_failed",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-catchup-t2",
					error: "catch-up encoder failed",
				});
			});
			return {
				...full,
				snapshot: { ...full.snapshot, messages: [] },
				snapshotStream: { id: "snapshot-revive-attach4", messageCount: 0, targetChunkBytes: 512 * 1024 },
			};
		};
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		const events: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			events.push(event);
		});
		fakeClient.deadActiveSessionIds.add("active-1");

		await connection.prompt("continue");

		await vi.waitFor(() => expect(events.some((event) => event.type === "session_resynced")).toBe(true));
		const resynced = events.find(
			(event): event is Extract<AgentConnectionEvent, { type: "session_resynced" }> =>
				event.type === "session_resynced",
		);
		// The re-read must supersede the stale buffered snapshot.
		expect(resynced?.snapshot.messages).toEqual([{ role: "user", content: "current prompt", timestamp: 4 }]);
		expect(fakeClient.requests.filter((request) => request.type === "get_connection_state").length).toBeGreaterThan(
			0,
		);
	});

	it("keeps a newer catch-up applied to the published binding while the attach snapshot was streaming", async () => {
		const fakeClient = new FakeDaemonClient();
		const catchupMessages: AgentMessage[] = [{ role: "user", content: "post-publication-catch-up", timestamp: 12 }];
		let revivedSnapshotHeader: Record<string, unknown> | undefined;
		fakeClient.attachResultFactory = (command) => {
			if (command.activeSessionId !== "active-revived") {
				return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
			}
			const full = createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 23, {
				state: createConnectionState(command.activeSessionId, "session-revived"),
			});
			const { messages: _messages, ...snapshotHeader } = full.snapshot;
			revivedSnapshotHeader = snapshotHeader;
			// No frames yet: the attach response resolves, the continuation
			// publishes the revived binding and parks on waitForSnapshot.
			return {
				...full,
				snapshot: { ...full.snapshot, messages: [] },
				snapshotStream: { id: "snapshot-post-pub", messageCount: 0, targetChunkBytes: 512 * 1024 },
			};
		};
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		const events: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			events.push(event);
		});
		fakeClient.deadActiveSessionIds.add("active-1");

		const prompt = connection.prompt("continue");
		await vi.waitFor(() =>
			expect(
				fakeClient.requests.filter(
					(request) => request.type === "attach" && request.activeSessionId === "active-revived",
				),
			).toHaveLength(1),
		);
		// One socket read delivers the attach snapshot end AND the queued
		// catch-up resync: both land on the (already published) revived binding
		// before the waitForSnapshot continuation resumes.
		fakeClient.emitMessage({
			type: "session_snapshot_begin",
			activeSessionId: "active-revived",
			snapshotId: "snapshot-post-pub",
			snapshot: revivedSnapshotHeader as never,
			messageCount: 0,
			targetChunkBytes: 512 * 1024,
		});
		fakeClient.emitMessage({
			type: "session_snapshot_end",
			activeSessionId: "active-revived",
			snapshotId: "snapshot-post-pub",
			chunkCount: 0,
			lastEventSequence: 23,
		});
		fakeClient.emitMessage({
			type: "session_snapshot_begin",
			activeSessionId: "active-revived",
			snapshotId: "snapshot-post-pub-catchup",
			snapshot: revivedSnapshotHeader as never,
			messageCount: catchupMessages.length,
			targetChunkBytes: 512 * 1024,
			purpose: "resync",
		});
		fakeClient.emitMessage({
			type: "session_snapshot_chunk",
			activeSessionId: "active-revived",
			snapshotId: "snapshot-post-pub-catchup",
			index: 0,
			messages: [catchupMessages[0]!],
		});
		fakeClient.emitMessage({
			type: "session_snapshot_end",
			activeSessionId: "active-revived",
			snapshotId: "snapshot-post-pub-catchup",
			chunkCount: 1,
			lastEventSequence: 33,
		});
		await prompt;

		// The newer resync must survive the attach continuation: the revival's
		// own resync emission (served from the still-cached snapshot) must not
		// regress to the older attach snapshot.
		await vi.waitFor(() => {
			const resyncs = events.filter(
				(event): event is Extract<AgentConnectionEvent, { type: "session_resynced" }> =>
					event.type === "session_resynced",
			);
			expect(resyncs.length).toBeGreaterThan(0);
			expect(resyncs[resyncs.length - 1]?.snapshot.messages).toEqual(catchupMessages);
			expect(resyncs[resyncs.length - 1]?.snapshot.lastEventSequence).toBe(33);
		});
	});

	it("re-reads when a live event lands between the attach snapshot end and the continuation", async () => {
		const fakeClient = new FakeDaemonClient();
		let revivedSnapshotHeader: Record<string, unknown> | undefined;
		fakeClient.attachResultFactory = (command) => {
			if (command.activeSessionId !== "active-revived") {
				return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
			}
			const full = createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 23, {
				state: createConnectionState(command.activeSessionId, "session-revived"),
			});
			const { messages: _messages, ...snapshotHeader } = full.snapshot;
			revivedSnapshotHeader = snapshotHeader;
			return {
				...full,
				snapshot: { ...full.snapshot, messages: [] },
				snapshotStream: { id: "snapshot-live-race", messageCount: 0, targetChunkBytes: 512 * 1024 },
			};
		};
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		const events: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			events.push(event);
		});
		fakeClient.deadActiveSessionIds.add("active-1");

		const prompt = connection.prompt("continue");
		await vi.waitFor(() =>
			expect(
				fakeClient.requests.filter(
					(request) => request.type === "attach" && request.activeSessionId === "active-revived",
				),
			).toHaveLength(1),
		);
		// The attach snapshot ends at sequence 23 and a live event with
		// sequence 24 is parsed before the continuation resumes: the cursor now
		// claims 24 while the attach snapshot's content stops at 23.
		fakeClient.emitMessage({
			type: "session_snapshot_begin",
			activeSessionId: "active-revived",
			snapshotId: "snapshot-live-race",
			snapshot: revivedSnapshotHeader as never,
			messageCount: 0,
			targetChunkBytes: 512 * 1024,
		});
		fakeClient.emitMessage({
			type: "session_snapshot_end",
			activeSessionId: "active-revived",
			snapshotId: "snapshot-live-race",
			chunkCount: 0,
			lastEventSequence: 23,
		});
		emitSequencedQueueUpdate(fakeClient, "active-revived", 24);
		await prompt;

		// The cache must not be served as fresh state that silently omits the
		// live event: the revival's resync comes from a re-read.
		await vi.waitFor(() => {
			const resyncs = events.filter(
				(event): event is Extract<AgentConnectionEvent, { type: "session_resynced" }> =>
					event.type === "session_resynced",
			);
			expect(resyncs.length).toBeGreaterThan(0);
			expect(resyncs[resyncs.length - 1]?.snapshot.messages).toEqual([
				{ role: "user", content: "current prompt", timestamp: 4 },
			]);
		});
		expect(fakeClient.requests.filter((request) => request.type === "get_connection_state").length).toBeGreaterThan(
			0,
		);
	});

	it("keeps a newer catch-up even when a live event cleared the freshness flag", async () => {
		const fakeClient = new FakeDaemonClient();
		const catchupMessages: AgentMessage[] = [{ role: "user", content: "catch-up-then-live", timestamp: 13 }];
		let revivedSnapshotHeader: Record<string, unknown> | undefined;
		fakeClient.attachResultFactory = (command) => {
			if (command.activeSessionId !== "active-revived") {
				return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
			}
			const full = createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 23, {
				state: createConnectionState(command.activeSessionId, "session-revived"),
			});
			const { messages: _messages, ...snapshotHeader } = full.snapshot;
			revivedSnapshotHeader = snapshotHeader;
			return {
				...full,
				snapshot: { ...full.snapshot, messages: [] },
				snapshotStream: { id: "snapshot-live-race2", messageCount: 0, targetChunkBytes: 512 * 1024 },
			};
		};
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		const events: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			events.push(event);
		});
		fakeClient.deadActiveSessionIds.add("active-1");

		const prompt = connection.prompt("continue");
		await vi.waitFor(() =>
			expect(
				fakeClient.requests.filter(
					(request) => request.type === "attach" && request.activeSessionId === "active-revived",
				),
			).toHaveLength(1),
		);
		fakeClient.emitMessage({
			type: "session_snapshot_begin",
			activeSessionId: "active-revived",
			snapshotId: "snapshot-live-race2",
			snapshot: revivedSnapshotHeader as never,
			messageCount: 0,
			targetChunkBytes: 512 * 1024,
		});
		fakeClient.emitMessage({
			type: "session_snapshot_end",
			activeSessionId: "active-revived",
			snapshotId: "snapshot-live-race2",
			chunkCount: 0,
			lastEventSequence: 23,
		});
		fakeClient.emitMessage({
			type: "session_snapshot_begin",
			activeSessionId: "active-revived",
			snapshotId: "snapshot-live-catchup",
			snapshot: revivedSnapshotHeader as never,
			messageCount: catchupMessages.length,
			targetChunkBytes: 512 * 1024,
			purpose: "resync",
		});
		fakeClient.emitMessage({
			type: "session_snapshot_chunk",
			activeSessionId: "active-revived",
			snapshotId: "snapshot-live-catchup",
			index: 0,
			messages: [catchupMessages[0]!],
		});
		fakeClient.emitMessage({
			type: "session_snapshot_end",
			activeSessionId: "active-revived",
			snapshotId: "snapshot-live-catchup",
			chunkCount: 1,
			lastEventSequence: 33,
		});
		emitSequencedQueueUpdate(fakeClient, "active-revived", 34);
		await prompt;

		// The live event cleared the freshness flag; that must not let the
		// older attach snapshot overwrite the newer catch-up, and the final
		// state must come from a re-read that includes everything.
		await vi.waitFor(() => {
			const resyncs = events.filter(
				(event): event is Extract<AgentConnectionEvent, { type: "session_resynced" }> =>
					event.type === "session_resynced",
			);
			expect(resyncs.length).toBeGreaterThan(0);
			expect(resyncs[resyncs.length - 1]?.snapshot.messages).toEqual([
				{ role: "user", content: "current prompt", timestamp: 4 },
			]);
		});
	});

	it("does not detach a sibling's attachment when a superseded revival had a duplicate attach", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.serverCapabilities.add("attach_ownership");
		// A sibling connection on the shared client already holds the
		// attachment for the session the revival will resume.
		const sibling = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-revived");
		const siblingEvents: AgentConnectionEvent[] = [];
		sibling.subscribe((event) => {
			siblingEvents.push(event);
		});
		let emitRevivedSnapshot = () => {};
		fakeClient.attachResultFactory = (command) => {
			if (command.activeSessionId !== "active-revived") {
				return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
			}
			const full = createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 23);
			const { messages: _messages, ...snapshotHeader } = full.snapshot;
			emitRevivedSnapshot = () => {
				fakeClient.emitMessage({
					type: "session_snapshot_begin",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-dup-attach",
					snapshot: snapshotHeader,
					messageCount: 0,
					targetChunkBytes: 512 * 1024,
				});
				fakeClient.emitMessage({
					type: "session_snapshot_end",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-dup-attach",
					chunkCount: 0,
					lastEventSequence: 23,
				});
			};
			return {
				...full,
				snapshot: { ...full.snapshot, messages: [] },
				snapshotStream: { id: "snapshot-dup-attach", messageCount: 0, targetChunkBytes: 512 * 1024 },
			};
		};
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		fakeClient.deadActiveSessionIds.add("active-1");
		fakeClient.switchSessionAlreadyActiveId = "active-b";

		const prompt = connection.prompt("continue");
		await vi.waitFor(() =>
			expect(
				fakeClient.requests.filter(
					(request) => request.type === "attach" && request.activeSessionId === "active-revived",
				),
			).toHaveLength(2),
		);
		// The switch supersedes the revival while its snapshot streams. The
		// revival's attach got a response, but it was a DUPLICATE attach
		// (wasAttached) - the socket entry belongs to the sibling, and the
		// cleanup must not remove it.
		await connection.switchSession("/tmp/session-b.jsonl");
		emitRevivedSnapshot();
		await expect(prompt).rejects.toThrow("Unknown active session: active-1");

		expect(fakeClient.requests).not.toContainEqual(
			expect.objectContaining({ type: "detach", activeSessionId: "active-revived" }),
		);
		// The sibling must still be live on the shared socket.
		emitSequencedQueueUpdate(fakeClient, "active-revived", 24);
		await vi.waitFor(() => expect(siblingEvents.some((event) => event.type === "session_event")).toBe(true));
	});

	it("keeps a legacy daemon sibling attached when revival ownership is unknowable", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.omitWasAttached = true;
		const sibling = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-revived");
		const siblingEvents: AgentConnectionEvent[] = [];
		sibling.subscribe((event) => {
			siblingEvents.push(event);
		});
		let emitRevivedSnapshot = () => {};
		fakeClient.attachResultFactory = (command) => {
			if (command.activeSessionId !== "active-revived") {
				return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
			}
			const full = createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 23);
			const { messages: _messages, ...snapshotHeader } = full.snapshot;
			emitRevivedSnapshot = () => {
				fakeClient.emitMessage({
					type: "session_snapshot_begin",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-legacy-owner",
					snapshot: snapshotHeader,
					messageCount: 0,
					targetChunkBytes: 512 * 1024,
				});
				fakeClient.emitMessage({
					type: "session_snapshot_end",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-legacy-owner",
					chunkCount: 0,
					lastEventSequence: 23,
				});
			};
			return {
				...full,
				snapshot: { ...full.snapshot, messages: [] },
				snapshotStream: { id: "snapshot-legacy-owner", messageCount: 0, targetChunkBytes: 512 * 1024 },
			};
		};
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		fakeClient.deadActiveSessionIds.add("active-1");
		fakeClient.switchSessionAlreadyActiveId = "active-b";

		const prompt = connection.prompt("continue");
		await vi.waitFor(() =>
			expect(
				fakeClient.requests.filter(
					(request) => request.type === "attach" && request.activeSessionId === "active-revived",
				),
			).toHaveLength(2),
		);
		await connection.switchSession("/tmp/session-b.jsonl");
		emitRevivedSnapshot();
		await expect(prompt).rejects.toThrow("Unknown active session: active-1");

		// Legacy responses cannot prove which connection created the shared
		// socket attachment. Cleanup must preserve it rather than risk removing
		// the sibling's subscription.
		expect(fakeClient.requests).not.toContainEqual(
			expect.objectContaining({ type: "detach", activeSessionId: "active-revived" }),
		);
		emitSequencedQueueUpdate(fakeClient, "active-revived", 24);
		await vi.waitFor(() => expect(siblingEvents.some((event) => event.type === "session_event")).toBe(true));
	});

	it("detaches a legacy revival with no sibling so idle eviction stays possible", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.omitWasAttached = true;
		const emitRevivedSnapshot = gateFirstRevivedAttachSnapshot(fakeClient, "snapshot-legacy-only");
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		fakeClient.deadActiveSessionIds.add("active-1");
		fakeClient.switchSessionAlreadyActiveId = "active-b";

		const prompt = connection.prompt("continue");
		await vi.waitFor(() =>
			expect(
				fakeClient.requests.filter(
					(request) => request.type === "attach" && request.activeSessionId === "active-revived",
				),
			).toHaveLength(1),
		);
		await connection.switchSession("/tmp/session-b.jsonl");
		emitRevivedSnapshot();
		await expect(prompt).rejects.toThrow("Unknown active session: active-1");

		await vi.waitFor(() =>
			expect(fakeClient.requests).toContainEqual(
				expect.objectContaining({ type: "detach", activeSessionId: "active-revived" }),
			),
		);
		expect(fakeClient.attachedIds.has("active-revived")).toBe(false);
	});

	it("defers legacy revival cleanup across a sibling attach in flight", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.omitWasAttached = true;
		const emitRevivedSnapshot = gateFirstRevivedAttachSnapshot(fakeClient, "snapshot-legacy-pending-sibling");
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		fakeClient.deadActiveSessionIds.add("active-1");
		fakeClient.switchSessionAlreadyActiveId = "active-b";

		const prompt = connection.prompt("continue");
		await vi.waitFor(() =>
			expect(
				fakeClient.requests.filter(
					(request) => request.type === "attach" && request.activeSessionId === "active-revived",
				),
			).toHaveLength(1),
		);
		let releaseSiblingAttach = () => {};
		fakeClient.revivedAttachGate = new Promise<void>((resolve) => {
			releaseSiblingAttach = resolve;
		});
		const siblingAttach = DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-revived");
		await vi.waitFor(() =>
			expect(
				fakeClient.requests.filter(
					(request) => request.type === "attach" && request.activeSessionId === "active-revived",
				),
			).toHaveLength(2),
		);

		await connection.switchSession("/tmp/session-b.jsonl");
		emitRevivedSnapshot();
		await expect(prompt).rejects.toThrow("Unknown active session: active-1");
		expect(fakeClient.requests).not.toContainEqual(
			expect.objectContaining({ type: "detach", activeSessionId: "active-revived" }),
		);

		releaseSiblingAttach();
		const sibling = await siblingAttach;
		expect(fakeClient.requests).not.toContainEqual(
			expect.objectContaining({ type: "detach", activeSessionId: "active-revived" }),
		);
		await sibling.dispose();
		expect(fakeClient.attachedIds.has("active-revived")).toBe(false);
		await connection.dispose();
		expect(fakeClient.attachedIds.size).toBe(0);
	});

	it("detaches a displaced revival source after a pending sibling attach fails", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.omitWasAttached = true;
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		fakeClient.deadActiveSessionIds.add("active-1");

		let releaseSiblingAttach = () => {};
		fakeClient.attachGates.set(
			"active-1",
			new Promise<void>((resolve) => {
				releaseSiblingAttach = resolve;
			}),
		);
		fakeClient.attachFailureIds.add("active-1");
		const siblingAttach = DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		await vi.waitFor(() =>
			expect(
				fakeClient.requests.filter(
					(request) => request.type === "attach" && request.activeSessionId === "active-1",
				),
			).toHaveLength(2),
		);

		await connection.prompt("continue");
		// Publishing the revived binding moved the only holder off active-1,
		// but its known socket attachment must survive until the pending sibling
		// resolves. A failed sibling then exposes the deferred cleanup.
		expect(fakeClient.attachedIds.has("active-1")).toBe(true);
		expect(fakeClient.requests).not.toContainEqual(
			expect.objectContaining({ type: "detach", activeSessionId: "active-1" }),
		);

		releaseSiblingAttach();
		await expect(siblingAttach).rejects.toThrow("attach failed: active-1");
		await vi.waitFor(() => expect(fakeClient.attachedIds.has("active-1")).toBe(false));
		expect(fakeClient.requests).toContainEqual(
			expect.objectContaining({ type: "detach", activeSessionId: "active-1" }),
		);

		await connection.dispose();
		expect(fakeClient.attachedIds.size).toBe(0);
	});

	it("detaches a legacy revival disposed after its attach response", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.omitWasAttached = true;
		const emitRevivedSnapshot = gateFirstRevivedAttachSnapshot(fakeClient, "snapshot-legacy-disposed");
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		fakeClient.deadActiveSessionIds.add("active-1");

		const prompt = connection.prompt("continue");
		await vi.waitFor(() =>
			expect(
				fakeClient.requests.filter(
					(request) => request.type === "attach" && request.activeSessionId === "active-revived",
				),
			).toHaveLength(1),
		);
		await connection.dispose();
		emitRevivedSnapshot();
		await expect(prompt).rejects.toThrow("Unknown active session: active-1");

		const revivedDetaches = fakeClient.requests.filter(
			(request) => request.type === "detach" && request.activeSessionId === "active-revived",
		);
		expect(revivedDetaches).toHaveLength(1);
		expect(fakeClient.attachedIds.has("active-revived")).toBe(false);
	});

	it("suppresses a background revival resync after an in-worker transcript switch", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.attachResultFactory = (command) => {
			if (command.activeSessionId !== "active-revived") {
				return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
			}
			// Legacy bare-summary attach: the resync comes from separate reads.
			return {
				id: command.activeSessionId,
				activeSessionId: command.activeSessionId,
				sessionId: "session-revived",
				sessionFile: "/tmp/session-revived.jsonl",
			} as unknown as DaemonAttachResult;
		};
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		const events: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			events.push(event);
		});
		fakeClient.deadActiveSessionIds.add("active-1");
		// The revival's synchronous snapshot read fails, scheduling the
		// background retry.
		fakeClient.connectionStateFailures = 1;

		await connection.prompt("continue");
		expect(events.some((event) => event.type === "session_resynced")).toBe(false);

		// An in-worker switch replaces the transcript WITHOUT changing the
		// active id before the background retry fires.
		fakeClient.emitMessage({
			type: "session_replaced",
			activeSessionId: "active-revived",
			state: createConnectionState("active-revived", "session-b"),
			messages: [],
		});

		// The background retry must observe the moved transcript identity and
		// stay silent: a resync of the pre-switch snapshot would revert the
		// window. Wait past the retry window before asserting.
		await new Promise((resolveDelay) => setTimeout(resolveDelay, 900));
		const resyncs = events.filter(
			(event): event is Extract<AgentConnectionEvent, { type: "session_resynced" }> =>
				event.type === "session_resynced",
		);
		expect(resyncs).toHaveLength(0);
	});

	it("recreates the revived session with the invocation's launch context", async () => {
		const fakeClient = new FakeDaemonClient();
		vi.stubEnv("HERDR_PANE_ID", "pane-42");
		try {
			const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1", {
				sendClientEnv: true,
				reviveConfig: {
					cwd: "/tmp/project",
					model: "test-provider/test-model",
					systemPromptOverride: "custom prompt",
				} as never,
			});
			fakeClient.deadActiveSessionIds.add("active-1");

			await connection.prompt("continue");

			// The original launch context travels with the recreate: runtime
			// config and the client environment, so the revived worker does not
			// silently run under daemon defaults.
			expect(fakeClient.requests.find((request) => request.type === "create")).toMatchObject({
				sessionPath: "/tmp/session-current.jsonl",
				config: {
					cwd: "/tmp/project",
					model: "test-provider/test-model",
					systemPromptOverride: "custom prompt",
				},
				env: { HERDR_PANE_ID: "pane-42" },
			});
		} finally {
			vi.unstubAllEnvs();
		}
	});

	it("drops the invocation cwd when reviving a transcript adopted by a session switch", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1", {
			reviveConfig: { cwd: "/tmp/original-project", model: "test-provider/test-model" } as never,
		});
		// An in-worker switch replaces the transcript without changing the
		// active id; the invocation config's cwd belongs to the previous
		// project.
		fakeClient.emitMessage({
			type: "session_replaced",
			activeSessionId: "active-1",
			state: createConnectionState("active-1", "session-b"),
			messages: [],
		});
		await vi.waitFor(() => expect(fakeClient.requests.length).toBeGreaterThan(0));
		fakeClient.deadActiveSessionIds.add("active-1");

		await connection.prompt("continue");

		const create = fakeClient.requests.find((request) => request.type === "create");
		// The switched transcript revives in ITS own directory: the stale cwd
		// would act as an explicit override into the previous project, while
		// the rest of the invocation config still applies.
		expect(create).toMatchObject({
			sessionPath: "/tmp/session-b.jsonl",
			config: { model: "test-provider/test-model" },
		});
		expect(create && "config" in create ? create.config?.cwd : "present").toBeUndefined();
	});

	it("does not adopt a mid-stream replacement as the revived transcript identity", async () => {
		const fakeClient = new FakeDaemonClient();
		let emitFrames = () => {};
		fakeClient.attachResultFactory = (command) => {
			if (command.activeSessionId !== "active-revived") {
				return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
			}
			const full = createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 23, {
				state: createConnectionState(command.activeSessionId, "session-revived"),
			});
			const { messages: _messages, ...snapshotHeader } = full.snapshot;
			emitFrames = () => {
				// The attach snapshot completes, and an in-worker switch's
				// replacement rewrites the attached identity in the same parse
				// batch - both processed before the awaiting attach continuation
				// resumes and the revival captures its transcript identity.
				fakeClient.emitMessage({
					type: "session_snapshot_begin",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-identity-race",
					snapshot: snapshotHeader,
					messageCount: 0,
					targetChunkBytes: 512 * 1024,
				});
				fakeClient.emitMessage({
					type: "session_snapshot_end",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-identity-race",
					chunkCount: 0,
					lastEventSequence: 23,
				});
				fakeClient.emitMessage({
					type: "session_replaced",
					activeSessionId: command.activeSessionId,
					state: createConnectionState(command.activeSessionId, "session-b"),
					messages: [],
				});
			};
			return {
				...full,
				snapshot: { ...full.snapshot, messages: [] },
				snapshotStream: { id: "snapshot-identity-race", messageCount: 0, targetChunkBytes: 512 * 1024 },
			};
		};
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		fakeClient.deadActiveSessionIds.add("active-1");

		const prompt = connection.prompt("continue");
		await vi.waitFor(() =>
			expect(
				fakeClient.requests.filter(
					(request) => request.type === "attach" && request.activeSessionId === "active-revived",
				),
			).toHaveLength(1),
		);
		emitFrames();

		// The revived binding must be the identity the attach RESPONSE
		// published (session-revived), not the switched transcript - adopting
		// the replacement would let the retry inject the failed prompt into it.
		await expect(prompt).rejects.toThrow("Unknown active session: active-1");
		const prompts = fakeClient.requests.filter((request) => request.type === "prompt");
		expect(prompts.map((request) => request.activeSessionId)).toEqual(["active-1"]);
	});

	it("revives a switched transcript with the fallback cwd it was opened with", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.switchSessionSucceeds = true;
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1", {
			reviveConfig: { cwd: "/tmp/original-project", model: "test-provider/test-model" } as never,
		});
		// The user resumes a transcript whose recorded directory is missing,
		// selecting a fallback cwd - the switch only succeeds because of it.
		await connection.switchSession("/tmp/session-b.jsonl", { cwdOverride: "/tmp/fallback-b" });
		fakeClient.emitMessage({
			type: "session_replaced",
			activeSessionId: "active-1",
			state: createConnectionState("active-1", "session-b"),
			messages: [],
		});
		await vi.waitFor(() =>
			expect(fakeClient.requests.some((request) => request.type === "switch_session")).toBe(true),
		);
		fakeClient.deadActiveSessionIds.add("active-1");

		await connection.prompt("continue");

		// Reviving the switched transcript must reuse the selected fallback
		// cwd: without it the recreate fails on the missing recorded directory
		// and the prompt dead-ends again.
		expect(fakeClient.requests.find((request) => request.type === "create")).toMatchObject({
			sessionPath: "/tmp/session-b.jsonl",
			config: { model: "test-provider/test-model", cwd: "/tmp/fallback-b" },
		});
	});

	it("retains the fallback cwd when the switch attaches to a live worker", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.switchSessionAlreadyActiveId = "active-b";
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");

		// The transcript's recorded directory is missing; the user selects a
		// fallback cwd, and the switch resolves as an attach because another
		// worker already owns the transcript.
		await connection.switchSession("/tmp/session-b.jsonl", { cwdOverride: "/tmp/fallback-b" });
		fakeClient.deadActiveSessionIds.add("active-b");

		await connection.prompt("continue");

		// Reviving the attached transcript must reuse the selected fallback
		// cwd, exactly like an in-worker switch. The override is keyed to the
		// canonical file the attach reported (the fake's default state), not
		// the caller's path spelling.
		expect(fakeClient.requests.find((request) => request.type === "create")).toMatchObject({
			sessionPath: "/tmp/session-current.jsonl",
			config: { cwd: "/tmp/fallback-b" },
		});
	});

	it.each([
		{ mode: "legacy", negotiated: false },
		{ mode: "negotiated", negotiated: true },
	])("preserves a source sibling during a $mode live-session switch", async ({ negotiated }) => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.omitWasAttached = !negotiated;
		if (negotiated) fakeClient.serverCapabilities.add("attach_ownership");
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		const sibling = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		const siblingEvents: AgentConnectionEvent[] = [];
		sibling.subscribe((event) => {
			siblingEvents.push(event);
		});
		fakeClient.switchSessionAlreadyActiveId = "active-b";

		await connection.switchSession("/tmp/session-b.jsonl");

		// Protocol reattach would remove active-1 from the shared socket and
		// deafen the sibling. Plain attach keeps both Set entries until each
		// logical holder releases its binding.
		expect(fakeClient.requests).not.toContainEqual(
			expect.objectContaining({ type: "reattach", activeSessionId: "active-1" }),
		);
		expect(fakeClient.requests).toContainEqual(
			expect.objectContaining({ type: "attach", activeSessionId: "active-b" }),
		);
		expect(fakeClient.attachedIds).toEqual(new Set(["active-1", "active-b"]));
		emitSequencedQueueUpdate(fakeClient, "active-1", 13);
		await vi.waitFor(() => expect(siblingEvents.some((event) => event.type === "session_event")).toBe(true));

		await connection.dispose();
		expect(fakeClient.attachedIds).toEqual(new Set(["active-1"]));
		await sibling.dispose();
		expect(fakeClient.attachedIds.size).toBe(0);
	});

	it("keeps supervisor siblings attached when a direct connection switches workers", async () => {
		const supervisor = new FakeDaemonClient();
		const direct = new FakeDaemonClient();
		direct.switchSessionAlreadyActiveId = "active-b";
		const routed = new DaemonRoutedClient(asDaemonClient(supervisor), direct as unknown as DaemonWorkerClient);
		const connection = new DaemonAgentConnection(routed, "active-1");
		await connection.attach();
		const sibling = await DaemonAgentConnection.attach(asDaemonClient(supervisor), "active-1");
		const siblingEvents: AgentConnectionEvent[] = [];
		sibling.subscribe((event) => {
			siblingEvents.push(event);
		});

		await connection.switchSession("/tmp/session-b.jsonl");

		// Cross-worker attach goes through the supervisor, but must not use the
		// protocol reattach whose socket-wide source detach would deafen sibling.
		expect(direct.requests).toContainEqual(expect.objectContaining({ type: "switch_session" }));
		expect(supervisor.requests).toContainEqual(
			expect.objectContaining({ type: "attach", activeSessionId: "active-b" }),
		);
		expect(supervisor.requests).not.toContainEqual(expect.objectContaining({ type: "reattach" }));
		expect(supervisor.requests).not.toContainEqual(
			expect.objectContaining({ type: "detach", activeSessionId: "active-1" }),
		);
		expect(routed.hasDirectTransport).toBe(false);
		expect(supervisor.attachedIds).toEqual(new Set(["active-1", "active-b"]));

		emitSequencedQueueUpdate(supervisor, "active-1", 13);
		await vi.waitFor(() => expect(siblingEvents.some((event) => event.type === "session_event")).toBe(true));

		await connection.dispose();
		expect(supervisor.attachedIds).toEqual(new Set(["active-1"]));
		await sibling.dispose();
		expect(supervisor.attachedIds.size).toBe(0);
	});

	it("keeps the source binding and snapshot when the live switch target attach fails", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		const before = await connection.getInitialSnapshot();
		fakeClient.switchSessionAlreadyActiveId = "active-b";
		fakeClient.attachFailureIds.add("active-b");

		await expect(connection.switchSession("/tmp/session-b.jsonl")).rejects.toThrow("attach failed: active-b");

		expect(fakeClient.attachedIds).toEqual(new Set(["active-1"]));
		expect(fakeClient.requests).not.toContainEqual(
			expect.objectContaining({ type: "detach", activeSessionId: "active-1" }),
		);
		await expect(connection.getInitialSnapshot()).resolves.toBe(before);
		await connection.prompt("still source");
		expect(fakeClient.requests.at(-1)).toMatchObject({ type: "prompt", activeSessionId: "active-1" });
		await connection.dispose();
	});

	it("preserves a source sibling that attaches while the switch target is pending", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		fakeClient.switchSessionAlreadyActiveId = "active-b";
		let releaseTargetAttach = () => {};
		fakeClient.attachGates.set(
			"active-b",
			new Promise<void>((resolve) => {
				releaseTargetAttach = resolve;
			}),
		);

		const switching = connection.switchSession("/tmp/session-b.jsonl");
		await vi.waitFor(() =>
			expect(
				fakeClient.requests.filter(
					(request) => request.type === "attach" && request.activeSessionId === "active-b",
				),
			).toHaveLength(1),
		);

		// This source attach starts after switching has begun, but before the
		// target attach can publish. It must acquire a holder before source
		// cleanup is requested, rather than racing an atomic server-side delete.
		const sibling = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		const siblingEvents: AgentConnectionEvent[] = [];
		sibling.subscribe((event) => {
			siblingEvents.push(event);
		});
		releaseTargetAttach();
		await switching;

		expect(fakeClient.requests).not.toContainEqual(expect.objectContaining({ type: "reattach" }));
		expect(fakeClient.attachedIds).toEqual(new Set(["active-1", "active-b"]));
		emitSequencedQueueUpdate(fakeClient, "active-1", 13);
		await vi.waitFor(() => expect(siblingEvents.some((event) => event.type === "session_event")).toBe(true));

		await connection.dispose();
		expect(fakeClient.attachedIds).toEqual(new Set(["active-1"]));
		await sibling.dispose();
		expect(fakeClient.attachedIds.size).toBe(0);
	});

	it("commits a successful pending source attach before deferred cleanup", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.omitWasAttached = true;
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		let releaseSiblingAttach = () => {};
		fakeClient.attachGates.set(
			"active-1",
			new Promise<void>((resolve) => {
				releaseSiblingAttach = resolve;
			}),
		);
		const siblingAttach = DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		await vi.waitFor(() =>
			expect(
				fakeClient.requests.filter(
					(request) => request.type === "attach" && request.activeSessionId === "active-1",
				),
			).toHaveLength(2),
		);
		fakeClient.switchSessionAlreadyActiveId = "active-b";

		await connection.switchSession("/tmp/session-b.jsonl");
		// The source cleanup is deferred while the sibling attempt is pending.
		expect(fakeClient.requests).not.toContainEqual(
			expect.objectContaining({ type: "detach", activeSessionId: "active-1" }),
		);

		releaseSiblingAttach();
		const sibling = await siblingAttach;
		// Successful completion publishes the holder in the same tracker step
		// that removes the pending attempt, so deferred cleanup cannot run in
		// between and deafen the newly attached sibling.
		expect(fakeClient.requests).not.toContainEqual(
			expect.objectContaining({ type: "detach", activeSessionId: "active-1" }),
		);
		const siblingEvents: AgentConnectionEvent[] = [];
		sibling.subscribe((event) => {
			siblingEvents.push(event);
		});
		emitSequencedQueueUpdate(fakeClient, "active-1", 13);
		await vi.waitFor(() => expect(siblingEvents.some((event) => event.type === "session_event")).toBe(true));

		await connection.dispose();
		expect(fakeClient.attachedIds).toEqual(new Set(["active-1"]));
		await sibling.dispose();
		expect(fakeClient.attachedIds.size).toBe(0);
		expect(
			fakeClient.requests.filter((request) => request.type === "detach" && request.activeSessionId === "active-1"),
		).toHaveLength(1);
	});

	it("defers switch source cleanup through a pending sibling attach failure", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		let releaseSiblingAttach = () => {};
		fakeClient.attachGates.set(
			"active-1",
			new Promise<void>((resolve) => {
				releaseSiblingAttach = resolve;
			}),
		);
		fakeClient.attachFailureIds.add("active-1");
		const siblingAttach = DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		await vi.waitFor(() =>
			expect(
				fakeClient.requests.filter(
					(request) => request.type === "attach" && request.activeSessionId === "active-1",
				),
			).toHaveLength(2),
		);
		fakeClient.switchSessionAlreadyActiveId = "active-b";

		await connection.switchSession("/tmp/session-b.jsonl");
		expect(fakeClient.requests).not.toContainEqual(expect.objectContaining({ type: "reattach" }));
		expect(fakeClient.attachedIds.has("active-1")).toBe(true);
		expect(fakeClient.requests).not.toContainEqual(
			expect.objectContaining({ type: "detach", activeSessionId: "active-1" }),
		);

		releaseSiblingAttach();
		await expect(siblingAttach).rejects.toThrow("attach failed: active-1");
		await vi.waitFor(() => expect(fakeClient.attachedIds.has("active-1")).toBe(false));
		await connection.dispose();
		expect(fakeClient.attachedIds.size).toBe(0);
	});

	it("attaches the target before releasing an unshared switch source", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		fakeClient.switchSessionAlreadyActiveId = "active-b";

		await connection.switchSession("/tmp/session-b.jsonl");

		expect(fakeClient.requests.map((request) => request.type)).toEqual([
			"attach",
			"switch_session",
			"attach",
			"detach",
		]);
		expect(fakeClient.attachedIds).toEqual(new Set(["active-b"]));
		await connection.dispose();
		expect(fakeClient.attachedIds.size).toBe(0);
	});

	it("ignores unnegotiated ownership fields and keeps the sibling attached", async () => {
		const fakeClient = new FakeDaemonClient();
		// New daemon field without the negotiated capability must be ignored;
		// ownership remains unknowable, just as when an old daemon omits it.
		expect(fakeClient.serverCapabilities.has("attach_ownership")).toBe(false);
		const sibling = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-revived");
		const siblingEvents: AgentConnectionEvent[] = [];
		sibling.subscribe((event) => {
			siblingEvents.push(event);
		});
		let emitRevivedSnapshot = () => {};
		fakeClient.attachResultFactory = (command) => {
			if (command.activeSessionId !== "active-revived") {
				return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
			}
			const full = createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 23);
			const { messages: _messages, ...snapshotHeader } = full.snapshot;
			emitRevivedSnapshot = () => {
				fakeClient.emitMessage({
					type: "session_snapshot_begin",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-old-client",
					snapshot: snapshotHeader,
					messageCount: 0,
					targetChunkBytes: 512 * 1024,
				});
				fakeClient.emitMessage({
					type: "session_snapshot_end",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-old-client",
					chunkCount: 0,
					lastEventSequence: 23,
				});
			};
			return {
				...full,
				snapshot: { ...full.snapshot, messages: [] },
				snapshotStream: { id: "snapshot-old-client", messageCount: 0, targetChunkBytes: 512 * 1024 },
			};
		};
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1");
		fakeClient.deadActiveSessionIds.add("active-1");
		fakeClient.switchSessionAlreadyActiveId = "active-b";

		const prompt = connection.prompt("continue");
		await vi.waitFor(() =>
			expect(
				fakeClient.requests.filter(
					(request) => request.type === "attach" && request.activeSessionId === "active-revived",
				),
			).toHaveLength(2),
		);
		await connection.switchSession("/tmp/session-b.jsonl");
		emitRevivedSnapshot();
		await expect(prompt).rejects.toThrow("Unknown active session: active-1");

		// wasAttached=true travels in the response, but without the capability
		// it cannot authorize cleanup of the shared attachment.
		expect(fakeClient.requests).not.toContainEqual(
			expect.objectContaining({ type: "detach", activeSessionId: "active-revived" }),
		);
		emitSequencedQueueUpdate(fakeClient, "active-revived", 24);
		await vi.waitFor(() => expect(siblingEvents.some((event) => event.type === "session_event")).toBe(true));
	});

	it("drops the launch cwd when a fileless chat later revives a resumed transcript", async () => {
		const fakeClient = new FakeDaemonClient();
		// A --no-session chat: the initial attach has no session file.
		fakeClient.attachResultFactory = (command) => {
			const full = createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
			if (command.activeSessionId !== "active-1") {
				return full;
			}
			const state = { ...full.snapshot.state, sessionFile: undefined };
			return { ...full, snapshot: { ...full.snapshot, state, summary: { ...full.snapshot.summary } } };
		};
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1", {
			reviveConfig: { cwd: "/tmp/launch-dir", model: "test-provider/test-model" } as never,
		});
		fakeClient.attachResultFactory = undefined;
		// The user resumes a saved transcript in-worker; the launch cwd belongs
		// to the fileless session, not to this transcript.
		fakeClient.emitMessage({
			type: "session_replaced",
			activeSessionId: "active-1",
			state: createConnectionState("active-1", "session-b"),
			messages: [],
		});
		await vi.waitFor(() => expect(fakeClient.requests.length).toBeGreaterThan(0));
		fakeClient.deadActiveSessionIds.add("active-1");

		await connection.prompt("continue");

		const create = fakeClient.requests.find((request) => request.type === "create");
		expect(create).toMatchObject({
			sessionPath: "/tmp/session-b.jsonl",
			config: { model: "test-provider/test-model" },
		});
		expect(create && "config" in create ? create.config?.cwd : "present").toBeUndefined();
	});

	it("keeps the fileless marker across reconnect attaches", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.attachResultFactory = (command) => {
			const full = createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
			if (command.activeSessionId !== "active-1") {
				return full;
			}
			const state = { ...full.snapshot.state, sessionFile: undefined };
			return { ...full, snapshot: { ...full.snapshot, state, summary: { ...full.snapshot.summary } } };
		};
		const connection = await DaemonAgentConnection.attach(asDaemonClient(fakeClient), "active-1", {
			reviveConfig: { cwd: "/tmp/launch-dir", model: "test-provider/test-model" } as never,
		});
		fakeClient.attachResultFactory = undefined;
		// Resume a saved transcript in-worker, then re-attach (reconnect): the
		// second attach must not adopt the resumed transcript as the config's
		// own - the launch cwd still belongs to the fileless session.
		fakeClient.emitMessage({
			type: "session_replaced",
			activeSessionId: "active-1",
			state: createConnectionState("active-1", "session-b"),
			messages: [],
		});
		await vi.waitFor(() => expect(fakeClient.requests.length).toBeGreaterThan(0));
		// The reconnect attach reports the now-current resumed transcript.
		fakeClient.attachResultFactory = (command) =>
			createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 13, {
				state: createConnectionState(command.activeSessionId, "session-b"),
			});
		await connection.attach();
		fakeClient.attachResultFactory = undefined;
		fakeClient.deadActiveSessionIds.add("active-1");

		await connection.prompt("continue");

		const create = fakeClient.requests.find((request) => request.type === "create");
		expect(create).toMatchObject({ sessionPath: "/tmp/session-b.jsonl" });
		expect(create && "config" in create ? create.config?.cwd : "present").toBeUndefined();
	});

	it("treats a lost prompt response plus unknown cancellation as uncertain", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.promptError = new Error("lost response");
		fakeClient.cancelPromptAdmissionStatus = "unknown";
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");

		await expect(connection.prompt("startup", { signal: new AbortController().signal })).rejects.toMatchObject({
			message: "lost response",
			status: "unknown",
			cancelled: false,
		});
	});

	it("translates unsupported admission cancellation into an admission error", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.promptError = new DaemonCapabilityUnavailableError("prompt", "prompt_admission_cancellation");
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");

		await expect(connection.prompt("startup", { signal: new AbortController().signal })).rejects.toMatchObject({
			status: "unsupported",
		});
		expect(fakeClient.requests.map((request) => request.type)).toEqual(["prompt"]);
	});

	it("uses cancellable admission for promptAndWait", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.promptError = new Error("lost response");
		fakeClient.cancelPromptAdmissionStatus = "cancelled";
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");

		await expect(connection.promptAndWait("wait", { signal: new AbortController().signal })).rejects.toMatchObject({
			status: "cancelled",
		});
		expect(fakeClient.requests.map((request) => request.type)).toEqual([
			"prompt_and_wait",
			"cancel_prompt_admission",
		]);
	});

	it("uses fleet heartbeat scope for residents and session scope for owned workers", async () => {
		const residentClient = new FakeDaemonClient();
		residentClient.serverCapabilities.add("heartbeat_catalog");
		const resident = new DaemonAgentConnection(asDaemonClient(residentClient), "resident-1");
		await resident.listHeartbeats();

		const ownedClient = new FakeDaemonClient();
		ownedClient.serverCapabilities.add("heartbeat_catalog");
		const owned = new DaemonAgentConnection(asDaemonClient(ownedClient), "owned-1", { ownedSession: true });
		await owned.listHeartbeats();

		expect(residentClient.requests.at(-1)).toEqual(expect.objectContaining({ type: "heartbeats_list" }));
		expect(residentClient.requests.at(-1)).not.toHaveProperty("activeSessionId");
		expect(ownedClient.requests.at(-1)).toEqual(
			expect.objectContaining({ type: "heartbeats_list", activeSessionId: "owned-1" }),
		);
	});

	it("serializes concurrent owned-session promotion commands", async () => {
		const fakeClient = new FakeDaemonClient();
		let releaseFirst = () => {};
		fakeClient.cronAddGate = new Promise<void>((resolve) => {
			releaseFirst = resolve;
		});
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			ownedSession: true,
		});

		const first = connection.addCronJob("0 * * * *", "first");
		const second = connection.addCronJob("30 * * * *", "second");
		await vi.waitFor(() => {
			expect(fakeClient.requests.filter((request) => request.type === "cron_add")).toHaveLength(1);
		});
		releaseFirst();
		await Promise.all([first, second]);

		const commands = fakeClient.requests.filter(
			(command): command is Extract<DaemonCommand, { type: "cron_add" }> => command.type === "cron_add",
		);
		expect(commands.map((command) => command.promoteOwnedSession)).toEqual([true, false]);
	});

	it("gates side-question follow-up transcripts on the daemon capability", async () => {
		const oldDaemonClient = new FakeDaemonClient();
		const oldConnection = new DaemonAgentConnection(asDaemonClient(oldDaemonClient), "active-original");

		// First questions carry no transcript and must keep working on old daemons.
		await oldConnection.startSideQuestion("turn-1", "What changed?");
		expect(oldDaemonClient.requests.map((request) => request.type)).toEqual(["start_side_question"]);

		// Follow-ups would silently lose their side context on an old daemon.
		await expect(
			oldConnection.startSideQuestion("turn-2", "And then?", [{ question: "What changed?", answer: "The parser." }]),
		).rejects.toThrow("older build without side-conversation follow-ups");
		expect(oldDaemonClient.requests).toHaveLength(1);

		const newDaemonClient = new FakeDaemonClient();
		newDaemonClient.serverCapabilities.add("side_question_transcript");
		const newConnection = new DaemonAgentConnection(asDaemonClient(newDaemonClient), "active-original");

		await newConnection.startSideQuestion("turn-2", "And then?", [
			{ question: "What changed?", answer: "The parser." },
		]);
		const sent = newDaemonClient.requests.find(
			(command): command is Extract<DaemonCommand, { type: "start_side_question" }> =>
				command.type === "start_side_question",
		);
		expect(sent?.previousTurns).toEqual([{ question: "What changed?", answer: "The parser." }]);
	});

	it("gates side-question pane ids on the daemon capability", async () => {
		// New client, old daemon: the pane id must be dropped rather than sent to a
		// daemon that would accept and ignore it. Unlike follow-up transcripts this
		// degrades instead of failing, because losing pane grouping does not make
		// the answer wrong.
		const oldDaemonClient = new FakeDaemonClient();
		const oldConnection = new DaemonAgentConnection(asDaemonClient(oldDaemonClient), "active-original");

		await oldConnection.startSideQuestion("turn-1", "What changed?", undefined, "pane-1");
		const oldSent = oldDaemonClient.requests.find(
			(command): command is Extract<DaemonCommand, { type: "start_side_question" }> =>
				command.type === "start_side_question",
		);
		expect(oldSent).toBeDefined();
		expect(oldSent && "paneId" in oldSent).toBe(false);

		// side_question_transcript predates pane ids, so it must not be read as
		// implying them.
		const transcriptOnlyClient = new FakeDaemonClient();
		transcriptOnlyClient.serverCapabilities.add("side_question_transcript");
		const transcriptOnlyConnection = new DaemonAgentConnection(
			asDaemonClient(transcriptOnlyClient),
			"active-original",
		);

		await transcriptOnlyConnection.startSideQuestion("turn-1", "What changed?", undefined, "pane-1");
		const transcriptOnlySent = transcriptOnlyClient.requests.find(
			(command): command is Extract<DaemonCommand, { type: "start_side_question" }> =>
				command.type === "start_side_question",
		);
		expect(transcriptOnlySent && "paneId" in transcriptOnlySent).toBe(false);

		// New client, new daemon: the pane id goes over the wire.
		const newDaemonClient = new FakeDaemonClient();
		newDaemonClient.serverCapabilities.add("side_question_pane_id");
		const newConnection = new DaemonAgentConnection(asDaemonClient(newDaemonClient), "active-original");

		await newConnection.startSideQuestion("turn-1", "What changed?", undefined, "pane-1");
		const newSent = newDaemonClient.requests.find(
			(command): command is Extract<DaemonCommand, { type: "start_side_question" }> =>
				command.type === "start_side_question",
		);
		expect(newSent?.paneId).toBe("pane-1");

		// Old client, new daemon: a capable daemon still accepts a command that
		// carries no pane id at all.
		const capableDaemonClient = new FakeDaemonClient();
		capableDaemonClient.serverCapabilities.add("side_question_pane_id");
		const legacyCallerConnection = new DaemonAgentConnection(asDaemonClient(capableDaemonClient), "active-original");

		await legacyCallerConnection.startSideQuestion("turn-1", "What changed?");
		const legacySent = capableDaemonClient.requests.find(
			(command): command is Extract<DaemonCommand, { type: "start_side_question" }> =>
				command.type === "start_side_question",
		);
		expect(legacySent && "paneId" in legacySent).toBe(false);
	});

	it("gates transient bash on the daemon capability", async () => {
		const oldDaemonClient = new FakeDaemonClient();
		const oldConnection = new DaemonAgentConnection(asDaemonClient(oldDaemonClient), "active-original");

		// Regular bash keeps working on old daemons.
		await oldConnection.executeBash("ls");
		expect(oldDaemonClient.requests.map((request) => request.type)).toEqual(["execute_bash"]);

		// A transient run on an old daemon would be recorded into the session.
		await expect(oldConnection.executeBash("ls", { transient: true })).rejects.toThrow(
			"older build without side-conversation bash",
		);
		expect(oldDaemonClient.requests).toHaveLength(1);

		const newDaemonClient = new FakeDaemonClient();
		newDaemonClient.serverCapabilities.add("transient_bash");
		const newConnection = new DaemonAgentConnection(asDaemonClient(newDaemonClient), "active-original");

		await newConnection.executeBash("ls", { excludeFromContext: true, transient: true, runId: "side-run-1" });
		const sent = newDaemonClient.requests.find(
			(command): command is Extract<DaemonCommand, { type: "execute_bash" }> => command.type === "execute_bash",
		);
		expect(sent).toMatchObject({ command: "ls", excludeFromContext: true, transient: true, runId: "side-run-1" });
	});

	it("degrades an unavailable heartbeat catalog without sending an unsupported command", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-original");

		await expect(connection.listHeartbeats()).resolves.toEqual([]);
		expect(fakeClient.requests).toEqual([]);
		await expect(connection.manageHeartbeat("active-original", "job-1", "pause")).rejects.toThrow(
			"requires a newer Prime Agent daemon",
		);
		expect(fakeClient.requests).toEqual([]);
	});

	it.each([false, true])("reattaches after an update restart (deferring=%s)", async (deferSessionEvents) => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.emitCloseOnClose = true;
		const restoredMessages: AgentMessage[] = [{ role: "user", content: "restored prompt", timestamp: 2 }];
		// The restarted daemon lists the same session under a new active id.
		fakeClient.updateRestartSessions = [
			{ id: "r1", activeSessionId: "active-restored", sessionId: "session-current", sessionFile: "/tmp/f.jsonl" },
		];
		fakeClient.attachResultFactory = (command) =>
			createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 1, {
				state: createConnectionState(command.activeSessionId, "session-current"),
				messages: command.activeSessionId === "active-restored" ? restoredMessages : [],
			});
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-original", {
			deferSessionEvents,
		});
		const events: AgentConnectionEvent[] = [];
		const restored = new Promise<AgentConnectionEvent>((resolve) => {
			connection.subscribe((event) => {
				events.push(event);
				if (event.type === "session_resynced") {
					resolve(event);
				}
			});
		});
		await connection.attach();
		if (deferSessionEvents) emitSequencedSessionEvent(fakeClient, "active-original", 100);

		fakeClient.emitMessage({
			type: "session_closed",
			activeSessionId: "active-original",
			reason: "update",
		});
		await vi.waitFor(() => {
			expect(fakeClient.closeCount).toBe(1);
			expect(fakeClient.reconnectCount).toBe(1);
		});

		await expect(restored).resolves.toMatchObject({
			type: "session_resynced",
			snapshot: {
				state: { activeSessionId: "active-restored", sessionId: "session-current" },
				messages: restoredMessages,
			},
		});
		await connection.flushBufferedSessionEvents();
		expect(fakeClient.reconnectCount).toBe(1);
		expect(fakeClient.requests.map((request) => request.type)).toEqual(["attach", "list", "attach"]);
		expect(fakeClient.requests.at(-1)).toMatchObject({
			type: "attach",
			activeSessionId: "active-restored",
			resumeCursor: undefined,
		});
		await vi.waitFor(() => {
			expect(events).toEqual([
				expect.objectContaining({ type: "connection_status", status: "reconnecting" }),
				expect.objectContaining({
					type: "session_resynced",
					snapshot: expect.objectContaining({
						state: expect.objectContaining({ activeSessionId: "active-restored" }),
					}),
				}),
				{ type: "connection_status", status: "connected" },
			]);
		});
	});

	it("reattaches when an update socket close arrives before the session notice", async () => {
		const fakeClient = new FakeDaemonClient();
		const restoredMessages: AgentMessage[] = [{ role: "user", content: "restored prompt", timestamp: 2 }];
		fakeClient.updateRestartSessions = [
			{
				id: "active-restored",
				activeSessionId: "active-restored",
				sessionId: "session-current",
				sessionFile: "/tmp/session-current.jsonl",
			},
		];
		fakeClient.attachResultFactory = (command) =>
			createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 1, {
				state: createConnectionState(command.activeSessionId, "session-current"),
				messages: command.activeSessionId === "active-restored" ? restoredMessages : [],
			});
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-original");
		const restored = new Promise<AgentConnectionEvent>((resolve) => {
			connection.subscribe((event) => {
				if (event.type === "session_resynced") {
					resolve(event);
				}
			});
		});
		await connection.attach();

		fakeClient.emitClose(new DaemonSocketClosedError("/tmp/prime-agent.sock", "update"));

		await expect(restored).resolves.toMatchObject({
			type: "session_resynced",
			snapshot: {
				state: { activeSessionId: "active-restored", sessionId: "session-current" },
				messages: restoredMessages,
			},
		});
		expect(fakeClient.reconnectCount).toBe(1);
		expect(fakeClient.requests.map((request) => request.type)).toEqual(["attach", "list", "attach"]);
	});

	it("coordinates one transport reconnect across connections sharing a daemon client", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.emitCloseOnClose = true;
		fakeClient.updateRestartSessions = [
			{
				id: "restored-a",
				activeSessionId: "restored-a",
				sessionId: "session-a",
				sessionFile: "/tmp/session-a.jsonl",
			},
			{
				id: "restored-b",
				activeSessionId: "restored-b",
				sessionId: "session-b",
				sessionFile: "/tmp/session-b.jsonl",
			},
		];
		const sessionIds: Record<string, string> = {
			"active-a": "session-a",
			"active-b": "session-b",
			"restored-a": "session-a",
			"restored-b": "session-b",
		};
		fakeClient.attachResultFactory = (command) =>
			createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 1, {
				state: createConnectionState(command.activeSessionId, sessionIds[command.activeSessionId]!),
			});
		const connectionA = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-a");
		const connectionB = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-b");
		await connectionA.attach();
		await connectionB.attach();
		const restoredA = new Promise<AgentConnectionEvent>((resolve) => {
			connectionA.subscribe((event) => {
				if (event.type === "session_resynced") {
					resolve(event);
				}
			});
		});
		const restoredB = new Promise<AgentConnectionEvent>((resolve) => {
			connectionB.subscribe((event) => {
				if (event.type === "session_resynced") {
					resolve(event);
				}
			});
		});

		fakeClient.emitMessage({ type: "session_closed", activeSessionId: "active-a", reason: "update" });

		await expect(Promise.all([restoredA, restoredB])).resolves.toEqual([
			expect.objectContaining({
				type: "session_resynced",
				snapshot: expect.objectContaining({
					state: expect.objectContaining({ sessionId: "session-a" }),
				}),
			}),
			expect.objectContaining({
				type: "session_resynced",
				snapshot: expect.objectContaining({
					state: expect.objectContaining({ sessionId: "session-b" }),
				}),
			}),
		]);
		expect(fakeClient.closeCount).toBe(1);
		expect(fakeClient.reconnectCount).toBe(1);
	});

	it("does not reconnect after a shutdown session stop that never announced the daemon closing", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-original");
		const closedEvents: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			if (event.type === "closed") closedEvents.push(event);
		});
		await connection.attach();
		// No daemon_closing notice: an explicit session stop stays stopped.
		fakeClient.emitMessage({ type: "session_closed", activeSessionId: "active-original", reason: "shutdown" });
		fakeClient.emitClose(new Error("Daemon socket closed"));
		await Promise.resolve();

		expect(fakeClient.reconnectCount).toBe(0);
		expect(closedEvents).toHaveLength(1);
		expect(closedEvents[0]).toMatchObject({
			type: "closed",
			error: expect.stringContaining("The Prime Agent daemon shut down while this window was attached."),
		});
		const closedError = closedEvents[0]?.type === "closed" ? closedEvents[0].error : undefined;
		expect(closedError).toContain("Session ID: session-current.");
		expect(closedError).toContain("Session file: /tmp/session-current.jsonl.");
		expect(closedError).toContain("Diagnostic log:");
	});

	function announceClose(reason: "shutdown" | "killed", fakeClient: FakeDaemonClient) {
		fakeClient.emitMessage({ type: "daemon_closing", reason: "shutdown" });
		fakeClient.emitMessage({ type: "session_closed", activeSessionId: "active-original", reason });
	}

	it.each([
		[
			"shutdown socket close",
			(fakeClient: FakeDaemonClient) =>
				fakeClient.emitClose(new DaemonSocketClosedError("/tmp/prime-agent.sock", "shutdown")),
		],
		// An orderly supervisor shutdown archive-stops its workers, so attached windows
		// read the relayed close as "killed"; a direct worker link closes as "shutdown".
		["announced daemon shutdown", (fakeClient: FakeDaemonClient) => announceClose("shutdown", fakeClient)],
		["announced supervisor shutdown", (fakeClient: FakeDaemonClient) => announceClose("killed", fakeClient)],
	])("recovers a %s by reconnecting, then a bare session stop is terminal", async (_closeKind, triggerClose) => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.hello = { ...fakeClient.hello!, appVersion: "test-daemon-version" };
		// The restarted daemon lists the same session under a new active id.
		fakeClient.updateRestartSessions = [{ id: "r1", activeSessionId: "restored", sessionId: "session-current" }];
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-original");
		const events: AgentConnectionEvent[] = [];
		const connected = new Promise<AgentConnectionEvent>((resolveConnected) => {
			connection.subscribe((event) => {
				events.push(event);
				if (event.type === "connection_status" && event.status === "connected") resolveConnected(event);
			});
		});
		await connection.attach();

		triggerClose(fakeClient);

		await expect(connected).resolves.toMatchObject({ daemonVersion: "test-daemon-version" });
		expect(events.filter((event) => event.type === "closed")).toEqual([]);
		expect(events.filter((event) => event.type === "session_resynced").length).toBeGreaterThan(0);
		// The re-attach cleared the daemon_closing notice: a later bare stop of the recovered session is terminal again.
		fakeClient.emitMessage({ type: "session_closed", activeSessionId: "restored", reason: "killed" });
		expect(events.filter((event) => event.type === "closed")).toHaveLength(1);
		await connection.dispose();
	});

	it("a generic reconnect yields once a restart recovery restored the session", async () => {
		vi.useFakeTimers();
		try {
			const fakeClient = new FakeDaemonClient();
			fakeClient.updateRestartSessions = [{ id: "r1", activeSessionId: "restored", sessionId: "session-current" }];
			const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-original", {
				reconnectTimeoutMs: 1000,
				recoverDaemon: async () => undefined,
			});
			const events: AgentConnectionEvent[] = [];
			connection.subscribe((event) => {
				events.push(event);
			});
			await connection.attach();

			fakeClient.emitClose(new Error("Daemon socket closed"));
			fakeClient.emitClose(new DaemonSocketClosedError("/tmp/prime-agent.sock", "shutdown"));
			await vi.advanceTimersByTimeAsync(2_000);
			// The restart recovery owns the outcome: one resync, no duplicate, no terminal close.
			expect(events.filter((event) => event.type === "session_resynced")).toHaveLength(1);
			expect(events.filter((event) => event.type === "closed")).toEqual([]);
			await connection.dispose();
		} finally {
			vi.useRealTimers();
		}
	});

	it("a shutdown recovery does not duplicate an update recovery's resync", async () => {
		vi.useFakeTimers();
		try {
			const fakeClient = new FakeDaemonClient();
			const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-original");
			const events: AgentConnectionEvent[] = [];
			connection.subscribe((event) => void events.push(event));
			await connection.attach();
			fakeClient.emitClose(new DaemonSocketClosedError("/tmp/prime-agent.sock", "shutdown"));
			await vi.advanceTimersByTimeAsync(50); // parks the shutdown recovery on its retry delay
			fakeClient.updateRestartSessions = [{ id: "r1", activeSessionId: "restored", sessionId: "session-current" }];
			fakeClient.emitMessage({ type: "session_closed", activeSessionId: "active-original", reason: "update" });
			await vi.advanceTimersByTimeAsync(150); // the update recovery restores before the shutdown loop wakes
			const connected = events.filter((event) => event.type === "connection_status" && event.status === "connected");
			expect(events.filter((event) => event.type === "session_resynced")).toHaveLength(1);
			expect(connected).toHaveLength(1);
			await connection.dispose();
		} finally {
			vi.useRealTimers();
		}
	});

	it("keeps the saved-transcript close after shutdown recovery times out", async () => {
		vi.useFakeTimers();
		try {
			const fakeClient = new FakeDaemonClient();
			fakeClient.reconnectError = new Error("daemon unavailable");
			const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-original", {
				reconnectTimeoutMs: 5000,
			});
			const closedEvents: AgentConnectionEvent[] = [];
			connection.subscribe((event) => {
				if (event.type === "closed") {
					closedEvents.push(event);
				}
			});
			await connection.attach();

			fakeClient.emitClose(new DaemonSocketClosedError("/tmp/prime-agent.sock", "shutdown"));
			await vi.advanceTimersByTimeAsync(5100);

			expect(closedEvents).toHaveLength(1);
			const closedError = closedEvents[0]?.type === "closed" ? closedEvents[0].error : undefined;
			expect(closedError).toContain("The Prime Agent daemon shut down while this window was attached.");
			await connection.dispose();
		} finally {
			vi.useRealTimers();
		}
	});

	it("does not emit a restored session after disposal begins", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.updateRestartSessions = [
			{
				id: "active-restored",
				activeSessionId: "active-restored",
				sessionId: "session-current",
				sessionFile: "/tmp/session-current.jsonl",
			},
		];
		let releaseRestoredAttach: (() => void) | undefined;
		fakeClient.restoredAttachGate = new Promise<void>((resolve) => {
			releaseRestoredAttach = resolve;
		});
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-original");
		const events: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			events.push(event);
		});
		await connection.attach();

		fakeClient.emitClose(new DaemonSocketClosedError("/tmp/prime-agent.sock", "update"));
		await vi.waitFor(() => {
			expect(
				fakeClient.requests.some(
					(request) => request.type === "attach" && request.activeSessionId === "active-restored",
				),
			).toBe(true);
		});
		await connection.dispose();
		releaseRestoredAttach?.();
		await vi.waitFor(() => {
			expect(fakeClient.restoredAttachCompleted).toBe(1);
		});
		for (let flush = 0; flush < 5; flush++) {
			await Promise.resolve();
		}

		expect(events).toEqual([expect.objectContaining({ type: "connection_status", status: "reconnecting" })]);
		expect(fakeClient.requests.at(-1)).toMatchObject({ type: "detach", activeSessionId: "active-restored" });
	});

	it("returns to normal close handling after update restoration times out", async () => {
		vi.useFakeTimers();
		try {
			const fakeClient = new FakeDaemonClient();
			fakeClient.reconnectError = new Error("daemon unavailable");
			const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-original");
			const closedEvents: AgentConnectionEvent[] = [];
			connection.subscribe((event) => {
				if (event.type === "closed") {
					closedEvents.push(event);
				}
			});
			await connection.attach();

			fakeClient.emitMessage({
				type: "session_closed",
				activeSessionId: "active-original",
				reason: "update",
			});
			await vi.advanceTimersByTimeAsync(120100);

			expect(closedEvents).toHaveLength(1);
			const closedError = closedEvents[0]?.type === "closed" ? closedEvents[0].error : undefined;
			expect(closedError).toContain(
				"The Prime Agent daemon restarted for an update, but this window could not reconnect",
			);
			expect(closedError).toContain("Last error: daemon unavailable");
			expect(closedError).toContain("restart Prime Agent and reopen it from Agents View");
			expect(closedError).toContain("Session ID: session-current.");
			expect(closedError).toContain("Session file: /tmp/session-current.jsonl.");
			expect(closedError).toContain("Diagnostic log:");
			const reconnectCountAfterFailure = fakeClient.reconnectCount;
			fakeClient.emitClose(new Error("Daemon socket closed"));
			await Promise.resolve();

			expect(fakeClient.reconnectCount).toBe(reconnectCountAfterFailure);
			expect(closedEvents).toHaveLength(1);
			await connection.dispose();
		} finally {
			vi.useRealTimers();
		}
	});

	it("reattaches using the replacement session identity after a session switch", async () => {
		const fakeClient = new FakeDaemonClient();
		const restoredMessages: AgentMessage[] = [{ role: "user", content: "switched prompt", timestamp: 3 }];
		fakeClient.updateRestartSessions = [
			{
				id: "active-restored",
				activeSessionId: "active-restored",
				sessionId: "session-next",
				sessionFile: "/tmp/session-next.jsonl",
			},
		];
		fakeClient.attachResultFactory = (command) => {
			const sessionId = command.activeSessionId === "active-restored" ? "session-next" : "session-current";
			return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 1, {
				state: createConnectionState(command.activeSessionId, sessionId),
				messages: command.activeSessionId === "active-restored" ? restoredMessages : [],
			});
		};
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-original");
		await connection.attach();
		fakeClient.emitMessage({
			type: "session_replaced",
			activeSessionId: "active-original",
			state: createConnectionState("active-original", "session-next"),
			messages: [{ role: "user", content: "switched prompt", timestamp: 2 }],
		});

		const restored = new Promise<AgentConnectionEvent>((resolve) => {
			connection.subscribe((event) => {
				if (event.type === "session_resynced") {
					resolve(event);
				}
			});
		});
		fakeClient.emitMessage({
			type: "session_closed",
			activeSessionId: "active-original",
			reason: "update",
		});
		fakeClient.emitClose(new Error("Daemon socket closed"));

		await expect(restored).resolves.toMatchObject({
			type: "session_resynced",
			snapshot: {
				state: { activeSessionId: "active-restored", sessionId: "session-next" },
				messages: restoredMessages,
			},
		});
		expect(fakeClient.requests.map((request) => request.type)).toEqual(["attach", "list", "attach"]);
		expect(fakeClient.requests.at(-1)).toMatchObject({
			type: "attach",
			activeSessionId: "active-restored",
		});
	});

	it("forwards catch-up snapshots as non-destructive resync events", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
		const events: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			events.push(event);
		});
		await connection.attach();
		const messages: AgentMessage[] = [{ role: "user", content: "caught up", timestamp: 2 }];
		const streamingMessage = {
			role: "assistant",
			content: [{ type: "thinking", thinking: "Still reasoning" }],
		} as AgentMessage;
		const snapshot = createAttachResult("active-1", "client-1", undefined, 13, {
			state: { ...createConnectionState("active-1", "session-current"), isStreaming: true },
			messages,
			streamingMessage,
		}).snapshot;

		fakeClient.emitMessage({
			type: "session_resynced",
			activeSessionId: "active-1",
			snapshot,
			meta: {
				id: "active-1:13",
				protocol: DAEMON_PROTOCOL_INFO,
				activeSessionId: "active-1",
				sequence: 13,
				cursor: { generation: "generation-active-1", sequence: 13 },
				emittedAt: "2026-01-01T00:00:00.000Z",
			},
		});

		expect(events).toEqual([
			{
				type: "session_resynced",
				snapshot: expect.objectContaining({
					state: expect.objectContaining({ sessionId: "session-current" }),
					messages,
					streamingMessage,
					lastEventSequence: 13,
				}),
			},
		]);
	});

	it("exposes attach snapshots as the initial connection snapshot", async () => {
		const fakeClient = new FakeDaemonClient();
		const snapshotMessage: AgentMessage = { role: "user", content: "snapshot prompt", timestamp: 1 };
		const messages: AgentMessage[] = [snapshotMessage];
		fakeClient.attachResultFactory = (command) =>
			createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 15, {
				state: createConnectionState(command.activeSessionId, "session-snapshot"),
				messages,
				sessionContext: {
					messages,
					thinkingLevel: "medium",
					serviceTier: "default",
					model: null,
				},
				sessionTree: {
					tree: [
						{
							entry: {
								type: "message",
								id: "user-1",
								parentId: null,
								timestamp: "2026-01-01T00:00:00.000Z",
								message: snapshotMessage,
							},
							children: [],
						},
					],
					leafId: "user-1",
				},
				parent: {
					activeSessionId: "parent-active",
					sessionId: "parent-session",
					nodeId: "parent-node",
					childId: "child-1",
				},
			});
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");

		await connection.attach();

		await expect(connection.getInitialSnapshot()).resolves.toMatchObject({
			state: {
				activeSessionId: "active-1",
				sessionId: "session-snapshot",
			},
			messages,
			sessionContext: {
				messages,
				thinkingLevel: "medium",
				model: null,
			},
			sessionTree: {
				leafId: "user-1",
			},
			parent: {
				activeSessionId: "parent-active",
				sessionId: "parent-session",
				nodeId: "parent-node",
				childId: "child-1",
			},
			lastEventSequence: 15,
			replay: {
				status: "complete",
				toSequence: 15,
			},
		});
		await expect(connection.getState()).resolves.toMatchObject({
			sessionId: "session-snapshot",
		});
		await expect(connection.getMessages()).resolves.toEqual(messages);
		expect(fakeClient.requests.map((request) => request.type)).toEqual(["attach"]);
	});

	it("times out an attach whose streamed snapshot never completes", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.attachResultFactory = (command) => {
			const result = createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
			return {
				...result,
				snapshotStream: { id: "snapshot-stalled", messageCount: 0, targetChunkBytes: 512 * 1024 },
			};
		};
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			snapshotTimeoutMs: 10,
		});

		await expect(connection.attach()).rejects.toThrow("Timed out waiting for snapshot snapshot-stalled");
		fakeClient.emitMessage({
			type: "session_snapshot_begin",
			activeSessionId: "active-1",
			snapshotId: "snapshot-stalled",
			snapshot: createAttachResult("active-1", "client-1", undefined, 12).snapshot,
			messageCount: 0,
			targetChunkBytes: 512 * 1024,
		});
		fakeClient.emitMessage({
			type: "session_snapshot_end",
			activeSessionId: "active-1",
			snapshotId: "snapshot-stalled",
			chunkCount: 0,
			lastEventSequence: 12,
		});
		expect((connection as unknown as { snapshotAssemblies: Map<string, unknown> }).snapshotAssemblies.size).toBe(0);
	});

	it("rejects one failed snapshot without interrupting another session on the shared client", async () => {
		const fakeClient = new FakeDaemonClient();
		const sibling = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-2");
		await sibling.attach();
		const siblingEvents: AgentConnectionEvent[] = [];
		sibling.subscribe((event) => {
			siblingEvents.push(event);
		});
		fakeClient.attachResultFactory = (command) => {
			const result = createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
			if (command.activeSessionId !== "active-1") {
				return result;
			}
			const { messages: _messages, ...snapshot } = result.snapshot;
			queueMicrotask(() => {
				fakeClient.emitMessage({
					type: "session_snapshot_begin",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-failed",
					snapshot,
					messageCount: 0,
					targetChunkBytes: 512 * 1024,
				});
				fakeClient.emitMessage({
					type: "session_snapshot_failed",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-failed",
					error: "snapshot encoder failed",
				});
			});
			return {
				...result,
				snapshot: { ...result.snapshot, messages: [] },
				snapshotStream: { id: "snapshot-failed", messageCount: 0, targetChunkBytes: 512 * 1024 },
			};
		};
		const failed = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");

		await expect(failed.attach()).rejects.toThrow("snapshot encoder failed");
		emitSequencedQueueUpdate(fakeClient, "active-2", 13);
		await vi.waitFor(() => expect(siblingEvents).toHaveLength(1));

		expect(siblingEvents[0]).toMatchObject({ type: "session_event", event: { type: "session_action_update" } });
		expect(fakeClient.closeCount).toBe(0);
		await failed.dispose();
		await sibling.dispose();
	});

	it("assembles chunked attach snapshots even when chunks arrive before the attach response continuation", async () => {
		const fakeClient = new FakeDaemonClient();
		const messages: AgentMessage[] = [
			{ role: "user", content: "first", timestamp: 1 },
			{ role: "user", content: "second", timestamp: 2 },
		];
		fakeClient.attachResultFactory = (command) => {
			const full = createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 23, {
				state: createConnectionState(command.activeSessionId, "session-streamed"),
				messages,
			});
			const { messages: _messages, ...snapshotHeader } = full.snapshot;
			queueMicrotask(() => {
				fakeClient.emitMessage({
					type: "session_snapshot_begin",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-streamed",
					snapshot: snapshotHeader,
					messageCount: messages.length,
					targetChunkBytes: 512 * 1024,
				});
				fakeClient.emitMessage({
					type: "session_snapshot_chunk",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-streamed",
					index: 0,
					messages: [messages[0]!],
				});
				fakeClient.emitMessage({
					type: "session_snapshot_chunk",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-streamed",
					index: 1,
					messages: [messages[1]!],
				});
				fakeClient.emitMessage({
					type: "session_snapshot_end",
					activeSessionId: command.activeSessionId,
					snapshotId: "snapshot-streamed",
					chunkCount: 2,
					lastEventSequence: 23,
				});
			});
			return {
				...full,
				snapshot: { ...full.snapshot, messages: [] },
				snapshotStream: {
					id: "snapshot-streamed",
					messageCount: messages.length,
					targetChunkBytes: 512 * 1024,
				},
			};
		};
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
		const events: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			events.push(event);
		});

		await connection.attach();

		await expect(connection.getInitialSnapshot()).resolves.toMatchObject({
			state: { sessionId: "session-streamed" },
			messages,
			lastEventSequence: 23,
		});
		expect(events).toEqual([]);
	});

	it.each([
		["headless", "message_end"],
		["headless", "message_update"],
		["reconnect", "message_end"],
		["reconnect", "message_update"],
	] as const)("preserves %s %s coalesced with attach snapshot completion", async (mode, eventType) => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			deferSessionEvents: mode === "reconnect",
			recoverDaemon: async () => {},
		});
		const input = new PassThrough();
		const detachReader = attachJsonlLineReader(input, (line) => fakeClient.emitMessage(JSON.parse(line)));
		try {
			if (mode === "reconnect") {
				await connection.attach();
				await connection.flushBufferedSessionEvents();
			}
			const message = fauxAssistantMessage("new response");
			fakeClient.attachResultFactory = (command) => {
				const full = createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12, {
					state: { ...createConnectionState("active-1", "session-current"), isStreaming: true },
					streamingMessage: fauxAssistantMessage("old partial response"),
				});
				const { messages: _messages, ...snapshot } = full.snapshot;
				const records: DaemonOutbound[] = [
					{
						type: "session_snapshot_begin",
						activeSessionId: "active-1",
						snapshotId: "coalesced",
						snapshot,
						messageCount: 0,
						targetChunkBytes: 512 * 1024,
					},
					{
						type: "session_snapshot_end",
						activeSessionId: "active-1",
						snapshotId: "coalesced",
						chunkCount: 0,
						lastEventSequence: 12,
						lastEventCursor: full.lastEventCursor,
					},
					{
						type: "session_event",
						activeSessionId: "active-1",
						event:
							eventType === "message_end"
								? { type: eventType, message }
								: {
										type: eventType,
										message,
										assistantMessageEvent: {
											type: "text_delta",
											contentIndex: 0,
											delta: "response",
											partial: message,
										},
									},
						meta: {
							id: "active-1:13",
							protocol: DAEMON_PROTOCOL_INFO,
							activeSessionId: "active-1",
							sequence: 13,
							cursor: { generation: "generation-active-1", sequence: 13 },
							emittedAt: "2026-01-01T00:00:00.000Z",
						},
					},
				];
				// All records dispatch before the attach response's promise continuation.
				queueMicrotask(() => input.write(records.map(serializeJsonLine).join("")));
				return { ...full, snapshotStream: { id: "coalesced", messageCount: 0, targetChunkBytes: 512 * 1024 } };
			};
			const events: AgentConnectionEvent[] = [];
			connection.subscribe((event) => {
				events.push(event);
			});
			if (mode === "reconnect") {
				fakeClient.connected = false;
				fakeClient.emitClose(new Error("socket closed"));
				await vi.waitFor(() => expect(events.some((event) => event.type === "session_resynced")).toBe(true));
			} else {
				await connection.attach();
			}
			const snapshot = await connection.getInitialSnapshot();
			expect(snapshot.streamingMessage).toEqual(eventType === "message_end" ? undefined : message);
			expect(snapshot.lastEventCursor).toEqual({ generation: "generation-active-1", sequence: 13 });
			expect(fakeClient.requests.map((request) => request.type)).toContain("get_messages");
			expect(events).toContainEqual(
				expect.objectContaining({ type: "session_event", event: expect.objectContaining({ type: eventType }) }),
			);
			for (const event of events) {
				if (event.type === "session_resynced")
					expect(event.snapshot.streamingMessage).toEqual(snapshot.streamingMessage);
			}
		} finally {
			detachReader();
			input.destroy();
			await connection.dispose();
		}
	});

	it("distinguishes chunked catch-up snapshots from runtime replacements", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
		await connection.attach();
		const events: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			events.push(event);
		});

		const emitSnapshot = (purpose: "replacement" | "resync", sequence: number, sessionId: string) =>
			emitChunkedSnapshot(fakeClient, {
				purpose,
				sessionId,
				messages: [{ role: "user", content: purpose, timestamp: sequence }],
				sequence,
			});

		emitSnapshot("resync", 13, "session-current");
		emitSnapshot("replacement", 14, "session-next");
		await vi.waitFor(() => expect(events).toHaveLength(2));

		expect(events).toEqual([
			expect.objectContaining({
				type: "session_resynced",
				snapshot: expect.objectContaining({
					state: expect.objectContaining({ sessionId: "session-current" }),
				}),
			}),
			expect.objectContaining({
				type: "session_replaced",
				state: expect.objectContaining({ sessionId: "session-next" }),
			}),
		]);
		expect((connection as unknown as { snapshotAssemblies: Map<string, unknown> }).snapshotAssemblies.size).toBe(0);
	});

	it.each(["replacement", "resync"] as const)(
		"recovers a failed chunked %s snapshot without interrupting a sibling session",
		async (purpose) => {
			const fakeClient = new FakeDaemonClient();
			const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
			const sibling = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-2");
			await Promise.all([connection.attach(), sibling.attach()]);
			const events: AgentConnectionEvent[] = [];
			const siblingEvents: AgentConnectionEvent[] = [];
			connection.subscribe((event) => {
				events.push(event);
			});
			sibling.subscribe((event) => {
				siblingEvents.push(event);
			});
			const recoveredSessionId = purpose === "replacement" ? "session-next" : "session-current";
			fakeClient.connectionStateFactory = (activeSessionId) =>
				createConnectionState(
					activeSessionId,
					activeSessionId === "active-1" ? recoveredSessionId : "session-sibling",
				);
			fakeClient.requests.length = 0;
			const snapshotId = `snapshot-failed-${purpose}`;
			const full = createAttachResult("active-1", "client-1", undefined, 13, {
				state: createConnectionState("active-1", recoveredSessionId),
			});
			const { messages: _messages, ...snapshot } = full.snapshot;
			if (purpose === "replacement") {
				fakeClient.emitMessage({
					type: "session_replaced",
					activeSessionId: "active-1",
					state: createConnectionState("active-1", recoveredSessionId),
					messages: [],
					snapshotFollows: true,
					meta: {
						id: "active-1:13",
						protocol: DAEMON_PROTOCOL_INFO,
						activeSessionId: "active-1",
						sequence: 13,
						cursor: { generation: "generation-active-1", sequence: 13 },
						emittedAt: "2026-01-01T00:00:00.000Z",
					},
				});
			}
			fakeClient.emitMessage({
				type: "session_snapshot_begin",
				activeSessionId: "active-1",
				snapshotId,
				snapshot,
				messageCount: 1,
				targetChunkBytes: 512 * 1024,
				purpose,
			});
			fakeClient.emitMessage({
				type: "session_snapshot_failed",
				activeSessionId: "active-1",
				snapshotId,
				error: `${purpose} snapshot failed`,
			});

			await vi.waitFor(() => expect(events).toHaveLength(1));
			if (purpose === "replacement") {
				expect(events[0]).toMatchObject({
					type: "session_replaced",
					state: { sessionId: recoveredSessionId },
					messages: [{ role: "user", content: "current prompt", timestamp: 4 }],
				});
			} else {
				expect(events[0]).toMatchObject({
					type: "session_resynced",
					snapshot: {
						state: { sessionId: recoveredSessionId },
						messages: [{ role: "user", content: "current prompt", timestamp: 4 }],
					},
				});
			}
			expect(fakeClient.requests.map((request) => request.type)).toEqual([
				"get_connection_state",
				"get_messages",
				"get_session_context",
			]);
			emitSequencedQueueUpdate(fakeClient, "active-2", 13);
			await vi.waitFor(() => expect(siblingEvents).toHaveLength(1));
			expect(siblingEvents[0]).toMatchObject({ type: "session_event", event: { type: "session_action_update" } });
			expect(fakeClient.closeCount).toBe(0);
			expect((connection as unknown as { snapshotAssemblies: Map<string, unknown> }).snapshotAssemblies.size).toBe(
				0,
			);
			await connection.dispose();
			await sibling.dispose();
		},
	);

	it("#2399: consumes the streamed replacement snapshot on a warm switch without refetching the transcript", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			snapshotTimeoutMs: 5_000,
		});
		await connection.attach();
		const replaced = new Promise<AgentConnectionEvent>((resolve) => {
			connection.subscribe((event) => {
				if (event.type === "session_replaced") resolve(event);
			});
		});
		const switchedMessages: AgentMessage[] = [{ role: "user", content: "switched prompt", timestamp: 5 }];
		const request = fakeClient.request.bind(fakeClient);
		vi.spyOn(fakeClient, "request").mockImplementation(async (command, ...options) => {
			if (command.type !== "switch_session") return request(command, ...options);
			fakeClient.requests.push(command);
			// The daemon writes session_replaced (snapshotFollows) and streams the
			// chunked replacement snapshot before the switch responds.
			emitChunkedSnapshot(fakeClient, {
				purpose: "replacement",
				sessionId: "session-switched",
				messages: switchedMessages,
				inline: true,
			});
			return { type: "response", command: command.type, success: true, data: { cancelled: false } };
		});
		fakeClient.requests.length = 0;

		const switchedSessionFile = "/tmp/session-switched.jsonl";
		await expect(connection.switchSession(switchedSessionFile)).resolves.toEqual({ cancelled: false });
		expect(fakeClient.requests.map((request) => request.type)).toEqual(["switch_session"]);
		const snapshot = await connection.getInitialSnapshot();
		expect(snapshot).toMatchObject({ state: { sessionId: "session-switched" }, messages: switchedMessages });
		// The history crossed the wire once, as the chunked snapshot the warm switch
		// consumed: neither get_messages nor get_session_context refetched it.
		expect(fakeClient.requests.map((request) => request.type)).toEqual(["switch_session"]);
		await expect(replaced).resolves.toMatchObject({ type: "session_replaced", messages: switchedMessages });
		await connection.dispose();
	});

	it("#2399: falls back to refetching the transcript when the replacement snapshot stream fails", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.connectionStateFactory = (activeSessionId) =>
			createConnectionState(activeSessionId, "session-switched");
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			snapshotTimeoutMs: 5_000,
		});
		await connection.attach();
		const replaced = new Promise<AgentConnectionEvent>((resolve) => {
			connection.subscribe((event) => {
				if (event.type === "session_replaced") resolve(event);
			});
		});
		const request = fakeClient.request.bind(fakeClient);
		vi.spyOn(fakeClient, "request").mockImplementation(async (command, ...options) => {
			if (command.type !== "switch_session") return request(command, ...options);
			fakeClient.requests.push(command);
			emitChunkedSnapshot(fakeClient, {
				purpose: "replacement",
				sessionId: "session-switched",
				inline: true,
				fail: "replacement snapshot failed",
			});
			return { type: "response", command: command.type, success: true, data: { cancelled: false } };
		});
		fakeClient.requests.length = 0;

		// The failed stream must not fail the switch: the caller proceeds and the
		// recovery refetches the transcript.
		await expect(connection.switchSession("/tmp/session-switched.jsonl")).resolves.toEqual({ cancelled: false });
		// The recovery emits the replacement event only after the refetch, so the
		// event is the completion signal for the whole fallback.
		await expect(replaced).resolves.toMatchObject({
			type: "session_replaced",
			state: { sessionId: "session-switched" },
			messages: [{ role: "user", content: "current prompt", timestamp: 4 }],
		});
		expect(fakeClient.requests.map((request) => request.type)).toEqual([
			"switch_session",
			"get_connection_state",
			"get_messages",
			"get_session_context",
		]);
		const snapshot = await connection.getInitialSnapshot();
		expect(snapshot).toMatchObject({
			state: { sessionId: "session-switched" },
			messages: [{ role: "user", content: "current prompt", timestamp: 4 }],
		});
		expect(fakeClient.requests.map((request) => request.type)).toEqual([
			"switch_session",
			"get_connection_state",
			"get_messages",
			"get_session_context",
		]);
		await connection.dispose();
	});

	it("#2399: refetches the switched session when the replacement snapshot never arrives", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.connectionStateFactory = (activeSessionId) =>
			createConnectionState(activeSessionId, "session-switched");
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			snapshotTimeoutMs: 10,
		});
		await connection.attach();
		const request = fakeClient.request.bind(fakeClient);
		vi.spyOn(fakeClient, "request").mockImplementation(async (command, ...options) => {
			if (command.type !== "switch_session") return request(command, ...options);
			fakeClient.requests.push(command);
			// The daemon accepts the switch but never streams a replacement snapshot.
			return { type: "response", command: command.type, success: true, data: { cancelled: false } };
		});
		fakeClient.requests.length = 0;

		// The bounded wait ends without a replacement, so the switch still returns.
		await expect(connection.switchSession("/tmp/session-switched.jsonl")).resolves.toEqual({ cancelled: false });
		expect(fakeClient.requests.map((request) => request.type)).toEqual(["switch_session"]);
		// The stale pre-switch cache must never be served as the switched session.
		const snapshot = await connection.getInitialSnapshot();
		expect(snapshot).toMatchObject({
			state: { sessionId: "session-switched" },
			messages: [{ role: "user", content: "current prompt", timestamp: 4 }],
		});
		expect(fakeClient.requests.map((request) => request.type)).toEqual([
			"switch_session",
			"get_connection_state",
			"get_messages",
			"get_session_context",
		]);
		await connection.dispose();
	});

	it.each([
		{
			// Another client's switch on the same daemon session replaces the session
			// first, so this client receives a replacement snapshot it never asked for.
			requested: "/tmp/session-switched.jsonl",
			applied: "/tmp/session-elsewhere.jsonl",
			resolved: "/tmp/session-switched.jsonl",
		},
		{
			// The daemon resolves a relative request into another directory, and the
			// replacement this client receives only shares its file name.
			requested: "session-x.jsonl",
			applied: "/tmp/elsewhere/session-x.jsonl",
			resolved: "/tmp/target/session-x.jsonl",
		},
		{
			// Without the daemon's resolution, a relative request can only be matched by
			// file name, so it must not be trusted on its own.
			requested: "session-x.jsonl",
			applied: "/tmp/elsewhere/session-x.jsonl",
		},
	])(
		"#2432: does not serve a replacement for another session as the switched transcript ($requested)",
		async (scenario) => {
			const fakeClient = new FakeDaemonClient();
			const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
				snapshotTimeoutMs: 5_000,
			});
			await connection.attach();
			const foreignMessages: AgentMessage[] = [{ role: "user", content: "foreign prompt", timestamp: 6 }];
			const request = fakeClient.request.bind(fakeClient);
			vi.spyOn(fakeClient, "request").mockImplementation(async (command, ...options) => {
				if (command.type !== "switch_session") return request(command, ...options);
				fakeClient.requests.push(command);
				emitChunkedSnapshot(fakeClient, {
					purpose: "replacement",
					sessionId: "session-elsewhere",
					sessionFile: scenario.applied,
					messages: foreignMessages,
					inline: true,
				});
				return {
					type: "response",
					command: command.type,
					success: true,
					data: { cancelled: false, ...(scenario.resolved ? { sessionFile: scenario.resolved } : {}) },
				};
			});
			fakeClient.requests.length = 0;

			await expect(connection.switchSession(scenario.requested)).resolves.toEqual({ cancelled: false });
			// The replacement belongs to another session, so the switched transcript is
			// reloaded rather than served from that snapshot.
			const snapshot = await connection.getInitialSnapshot();
			expect(snapshot.messages).toEqual([{ role: "user", content: "current prompt", timestamp: 4 }]);
			expect(fakeClient.requests.map((request) => request.type)).toEqual([
				"switch_session",
				"get_connection_state",
				"get_messages",
				"get_session_context",
			]);
			await connection.dispose();
		},
	);

	it("#2432: keeps the newer switch's snapshot fresh when a superseded switch settles late", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			snapshotTimeoutMs: 5_000,
		});
		await connection.attach();
		const newerMessages: AgentMessage[] = [{ role: "user", content: "newer prompt", timestamp: 9 }];
		let releaseOlder: (response: DaemonResponse) => void = () => {};
		const olderResponse = new Promise<DaemonResponse>((resolve) => {
			releaseOlder = resolve;
		});
		const request = fakeClient.request.bind(fakeClient);
		vi.spyOn(fakeClient, "request").mockImplementation(async (command, ...options) => {
			if (command.type !== "switch_session") return request(command, ...options);
			fakeClient.requests.push(command);
			if (command.sessionPath === "/tmp/session-older.jsonl") {
				// The older switch's response lands only after the newer switch finished.
				return olderResponse;
			}
			emitChunkedSnapshot(fakeClient, {
				purpose: "replacement",
				sessionId: "session-newer",
				sessionFile: "/tmp/session-newer.jsonl",
				messages: newerMessages,
				inline: true,
			});
			return {
				type: "response",
				command: command.type,
				success: true,
				data: { cancelled: false, sessionFile: "/tmp/session-newer.jsonl" },
			};
		});
		fakeClient.requests.length = 0;

		const switchedOlder = connection.switchSession("/tmp/session-older.jsonl");
		const switchedNewer = connection.switchSession("/tmp/session-newer.jsonl");
		await expect(switchedNewer).resolves.toEqual({ cancelled: false });
		releaseOlder({
			type: "response",
			command: "switch_session",
			success: true,
			data: { cancelled: false, sessionFile: "/tmp/session-older.jsonl" },
		});
		await expect(switchedOlder).resolves.toEqual({ cancelled: false });

		const snapshot = await connection.getInitialSnapshot();
		expect(snapshot).toMatchObject({
			state: { sessionId: "session-newer" },
			messages: newerMessages,
		});
		// The newer switch's streamed snapshot served the history once: the
		// superseded switch's late cleanup must not mark it stale and force a
		// full transcript refetch after the rapid session change.
		expect(fakeClient.requests.map((request) => request.type)).toEqual(["switch_session", "switch_session"]);
		await connection.dispose();
	});

	it("#2432: ends a switch wait when the reconnect fails instead of relaying the timeout", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.connectionStateFactory = (activeSessionId) =>
			createConnectionState(activeSessionId, "session-switched");
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			recoverDaemon: async () => undefined,
			reconnectTimeoutMs: 300,
			snapshotTimeoutMs: 5_000,
		});
		const closedEvents: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			if (event.type === "closed") closedEvents.push(event);
		});
		await connection.attach();
		fakeClient.reconnectError = new Error("daemon unavailable");
		const request = fakeClient.request.bind(fakeClient);
		vi.spyOn(fakeClient, "request").mockImplementation(async (command, ...options) => {
			if (command.type !== "switch_session") return request(command, ...options);
			fakeClient.requests.push(command);
			// The transport closes before any replacement frame and never comes back.
			fakeClient.connected = false;
			fakeClient.emitClose(new Error("socket closed"));
			return { type: "response", command: command.type, success: true, data: { cancelled: false } };
		});
		fakeClient.requests.length = 0;

		const startedAt = Date.now();
		await expect(connection.switchSession("/tmp/session-switched.jsonl")).resolves.toEqual({ cancelled: false });
		// The reconnect gave up, so the wait ends with the connection error rather
		// than after snapshotTimeoutMs.
		expect(Date.now() - startedAt).toBeLessThan(1_000);
		expect(closedEvents[0]).toMatchObject({ type: "closed", error: expect.stringContaining("reconnection failed") });
		await connection.dispose();
	});

	it("#2432: keeps a switch wait alive when a recoverable close abandons the replacement stream", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			recoverDaemon: async () => undefined,
			snapshotTimeoutMs: 5_000,
		});
		await connection.attach();
		const syncedMessages: AgentMessage[] = [{ role: "user", content: "synced prompt", timestamp: 7 }];
		// The re-attach lands on the session the switch moved to, and streams it back.
		fakeClient.attachResultFactory = (command) =>
			createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 14, {
				state: createConnectionState(command.activeSessionId, "session-switched"),
				messages: syncedMessages,
			});
		let releaseReattach: () => void = () => {};
		const reattachGate = new Promise<void>((resolve) => {
			releaseReattach = resolve;
		});
		let holdReattach = false;
		const request = fakeClient.request.bind(fakeClient);
		vi.spyOn(fakeClient, "request").mockImplementation(async (command, ...options) => {
			if (holdReattach && command.type === "attach") await reattachGate;
			if (command.type !== "switch_session") return request(command, ...options);
			fakeClient.requests.push(command);
			// The replacement stream has begun, and the transport closes before its end
			// frame, so the assembly is abandoned mid-transfer.
			emitChunkedSnapshot(fakeClient, {
				purpose: "replacement",
				sessionId: "session-switched",
				messages: syncedMessages,
				omitEnd: true,
			});
			await nextMessageLoopTurn();
			holdReattach = true;
			fakeClient.connected = false;
			fakeClient.emitClose(new Error("socket closed"));
			return { type: "response", command: command.type, success: true, data: { cancelled: false } };
		});
		fakeClient.requests.length = 0;

		const switched = connection.switchSession("/tmp/session-switched.jsonl");
		let settled = false;
		void switched.then(() => {
			settled = true;
		});
		// The abandoned stream must not settle the switch; only the re-attach can.
		await nextMessageLoopTurn();
		await nextMessageLoopTurn();
		expect(settled).toBe(false);

		releaseReattach();
		await expect(switched).resolves.toEqual({ cancelled: false });
		const snapshot = await connection.getInitialSnapshot();
		expect(snapshot.messages).toEqual(syncedMessages);
		// The re-attached snapshot serves the transcript, so nothing is refetched.
		expect(fakeClient.requests.filter((request) => request.type.startsWith("get_"))).toEqual([]);
		await connection.dispose();
	});

	it("#2432: ends a switch wait when the transport closes before the replacement begins", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.connectionStateFactory = (activeSessionId) =>
			createConnectionState(activeSessionId, "session-switched");
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			snapshotTimeoutMs: 5_000,
		});
		await connection.attach();
		const request = fakeClient.request.bind(fakeClient);
		vi.spyOn(fakeClient, "request").mockImplementation(async (command, ...options) => {
			if (command.type !== "switch_session") return request(command, ...options);
			fakeClient.requests.push(command);
			// The daemon accepts the switch, then the connection dies before any
			// replacement frame arrives.
			fakeClient.connected = false;
			fakeClient.emitClose(new Error("socket closed"));
			return { type: "response", command: command.type, success: true, data: { cancelled: false } };
		});
		fakeClient.requests.length = 0;

		const startedAt = Date.now();
		await expect(connection.switchSession("/tmp/session-switched.jsonl")).resolves.toEqual({ cancelled: false });
		// Nothing can settle the wait once the transport is gone, so it must not
		// sit out snapshotTimeoutMs before the caller reloads the session.
		expect(Date.now() - startedAt).toBeLessThan(1_000);
		await connection.dispose();
	});

	it("#2399: keeps the cached snapshot fresh across get_context_tree reads", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
		await connection.attach();
		const snapshot = await connection.getInitialSnapshot();
		expect(fakeClient.requests.map((request) => request.type)).toEqual(["attach"]);
		fakeClient.requests.length = 0;

		// get_context_tree is a pure read: it must not invalidate the snapshot.
		await expect(connection.getContextTree()).resolves.toMatchObject({ id: "root" });
		await expect(connection.getInitialSnapshot()).resolves.toBe(snapshot);
		expect(fakeClient.requests.map((request) => request.type)).toEqual(["get_context_tree"]);
		await connection.dispose();
	});

	it("keeps attach snapshots usable when the daemon omits duplicate session context", async () => {
		const fakeClient = new FakeDaemonClient();
		const messages: AgentMessage[] = [{ role: "user", content: "snapshot prompt", timestamp: 1 }];
		fakeClient.attachResultFactory = (command) =>
			createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 15, {
				state: createConnectionState(command.activeSessionId, "session-snapshot"),
				messages,
				omitSessionContext: true,
			});
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");

		await connection.attach();

		const snapshot = await connection.getInitialSnapshot();
		expect(snapshot).toMatchObject({
			state: {
				activeSessionId: "active-1",
				sessionId: "session-snapshot",
			},
			messages,
			lastEventSequence: 15,
		});
		expect(snapshot.sessionContext).toBeUndefined();
		expect(fakeClient.requests.map((request) => request.type)).toEqual(["attach"]);

		await expect(connection.getSessionContext()).resolves.toMatchObject({
			messages: [{ role: "user", content: "context prompt", timestamp: 3 }],
		});
		expect(fakeClient.requests.map((request) => request.type)).toEqual(["attach", "get_session_context"]);
	});

	it("keeps reconnect usable from attach snapshots when replay is unavailable", async () => {
		const fakeClient = new FakeDaemonClient();
		const reconnectedMessages: AgentMessage[] = [{ role: "user", content: "reconnected prompt", timestamp: 2 }];
		let attachCount = 0;
		fakeClient.attachResultFactory = (command) => {
			attachCount++;
			if (attachCount === 1) {
				return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12, {
					state: createConnectionState(command.activeSessionId, "session-initial"),
				});
			}
			return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 20, {
				state: createConnectionState(command.activeSessionId, "session-reconnected"),
				messages: reconnectedMessages,
				sessionContext: {
					messages: reconnectedMessages,
					thinkingLevel: "medium",
					serviceTier: "default",
					model: null,
				},
				replay: {
					status: "unavailable",
					fromSequence: 14,
					toSequence: 20,
					reason: "event_replay_not_available",
				},
			});
		};
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");

		await connection.attach();
		emitSequencedQueueUpdate(fakeClient, "active-1", 14);
		await connection.attach();

		expect(fakeClient.requests.at(-1)).toMatchObject({
			type: "attach",
			activeSessionId: "active-1",
			capabilities: ["attach_snapshot", "event_sequence", "extension_ui", "slim_attach", "chunked_snapshot"],
			resumeCursor: {
				activeSessionId: "active-1",
				generation: "generation-active-1",
				sequence: 14,
			},
		});
		await expect(connection.getInitialSnapshot()).resolves.toMatchObject({
			state: {
				sessionId: "session-reconnected",
			},
			messages: reconnectedMessages,
			replay: {
				status: "unavailable",
				fromSequence: 14,
				toSequence: 20,
				reason: "event_replay_not_available",
			},
			lastEventSequence: 20,
		});
		await expect(connection.getState()).resolves.toMatchObject({
			sessionId: "session-reconnected",
		});
		await expect(connection.getMessages()).resolves.toEqual(reconnectedMessages);
		expect(fakeClient.requests.map((request) => request.type)).toEqual(["attach", "attach"]);
	});

	it("resets a connected transport when reattach fails during supervisor recovery", async () => {
		const fakeClient = new FakeDaemonClient();
		const recoverDaemon = vi.fn(async () => undefined);
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			recoverDaemon,
			reconnectTimeoutMs: 2000,
		});
		const statuses: string[] = [];
		let resyncs = 0;
		connection.subscribe((event) => {
			if (event.type === "connection_status") {
				statuses.push(event.status);
			}
			if (event.type === "session_resynced") {
				resyncs++;
			}
		});
		await connection.attach();

		fakeClient.attachFailures = 1;
		fakeClient.connected = false;
		fakeClient.emitClose(new Error("Daemon socket closed"));

		await vi.waitFor(() => expect(statuses).toEqual(["reconnecting", "connected"]));
		expect(recoverDaemon).toHaveBeenCalledTimes(2);
		expect(fakeClient.reconnectCount).toBe(2);
		expect(fakeClient.resetTransportCount).toBe(1);
		expect(fakeClient.requests.filter((request) => request.type === "attach")).toHaveLength(3);
		expect(resyncs).toBe(1);
	});

	it.each([
		["reconnect", "attach"],
		["reconnect", "snapshot"],
		["reconnect", "backoff"],
		["update", "attach"],
		["update", "snapshot"],
		["update", "backoff"],
	] as const)("stops %s after a terminal close during %s", async (recovery, stage) => {
		vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			recoverDaemon: async () => undefined,
			reconnectTimeoutMs: 1000,
		});
		let release!: () => void;
		const gate = new Promise<void>((resolve) => {
			release = resolve;
		});
		try {
			await connection.attach();
			fakeClient.updateRestartSessions = [{ activeSessionId: "active-1", sessionId: "session-current" }];
			const request = fakeClient.request.bind(fakeClient);
			vi.spyOn(fakeClient, "request").mockImplementation(async (...args) => {
				const response = await request(...args);
				if (args[0].type === "attach") {
					if (stage === "backoff") throw new Error("attach temporarily unavailable");
					if (stage === "attach") await gate;
				}
				return response;
			});
			const getInitialSnapshot = connection.getInitialSnapshot.bind(connection);
			const snapshotRead = vi.spyOn(connection, "getInitialSnapshot").mockImplementation(async (...args) => {
				const snapshot = await getInitialSnapshot(...args);
				if (stage === "snapshot") await gate;
				return snapshot;
			});
			const events: AgentConnectionEvent[] = [];
			connection.subscribe((event) => {
				events.push(event);
			});
			fakeClient.connected = false;
			fakeClient.emitClose(
				recovery === "update"
					? new DaemonSocketClosedError("/tmp/fake.sock", "update")
					: new Error("Daemon socket closed"),
			);
			await vi.advanceTimersByTimeAsync(0);
			expect(fakeClient.requests.filter((request) => request.type === "attach")).toHaveLength(2);
			if (stage === "snapshot") expect(snapshotRead).toHaveBeenCalledOnce();
			const requestCount = fakeClient.requests.length;
			const reconnectCount = fakeClient.reconnectCount;
			const resetCount = fakeClient.resetTransportCount;
			const closeCount = fakeClient.closeCount;
			fakeClient.emitMessage({ type: "session_closed", activeSessionId: "active-1", reason: "killed" });
			release();
			await vi.advanceTimersByTimeAsync(120100);
			expect(fakeClient.requests).toHaveLength(requestCount);
			expect(fakeClient.reconnectCount).toBe(reconnectCount);
			expect(fakeClient.resetTransportCount).toBe(resetCount);
			expect(fakeClient.closeCount).toBe(closeCount);
			expect(events).toEqual([
				expect.objectContaining({ type: "connection_status", status: "reconnecting" }),
				expect.objectContaining({
					type: "closed",
					error: expect.stringContaining("The daemon stopped this agent session."),
				}),
			]);
		} finally {
			release();
			await connection.dispose();
			vi.useRealTimers();
		}
	});

	it("does not reconnect after disposal while daemon recovery is pending", async () => {
		const fakeClient = new FakeDaemonClient();
		let finishRecovery: (() => void) | undefined;
		const recoverDaemon = vi.fn(
			() =>
				new Promise<void>((resolve) => {
					finishRecovery = resolve;
				}),
		);
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			recoverDaemon,
			reconnectTimeoutMs: 2000,
		});
		await connection.attach();

		fakeClient.connected = false;
		fakeClient.emitClose(new Error("Daemon socket closed"));
		await vi.waitFor(() => expect(recoverDaemon).toHaveBeenCalledOnce());
		await connection.dispose();
		finishRecovery?.();
		for (let flush = 0; flush < 5; flush++) {
			await Promise.resolve();
		}

		expect(fakeClient.reconnectCount).toBe(0);
	});

	it("isolates subscriber failures during transport recovery", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			recoverDaemon: async () => undefined,
			reconnectTimeoutMs: 2000,
		});
		const statuses: string[] = [];
		connection.subscribe(async () => {
			throw new Error("broken subscriber");
		});
		connection.subscribe((event) => {
			if (event.type === "connection_status") {
				statuses.push(event.status);
			}
		});
		await connection.attach();

		fakeClient.connected = false;
		fakeClient.emitClose(new Error("Daemon socket closed"));

		await vi.waitFor(() => expect(statuses).toEqual(["reconnecting", "connected"]));
		expect(fakeClient.reconnectCount).toBe(1);
	});

	it("does not let a stalled subscriber block update recovery", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.updateRestartSessions = [
			{
				id: "active-restored",
				activeSessionId: "active-restored",
				sessionId: "session-current",
				sessionFile: "/tmp/session-current.jsonl",
			},
		];
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-original");
		const statuses: string[] = [];
		connection.subscribe(() => new Promise<void>(() => undefined));
		connection.subscribe((event) => {
			if (event.type === "connection_status") {
				statuses.push(event.status);
			}
		});
		await connection.attach();

		fakeClient.emitClose(new DaemonSocketClosedError("/tmp/prime-agent.sock", "update"));

		await vi.waitFor(() => expect(statuses).toEqual(["reconnecting", "connected"]));
		expect(fakeClient.requests.at(-1)).toMatchObject({
			type: "attach",
			activeSessionId: "active-restored",
		});
	});

	it("preserves a newer live child update over an in-flight roster read", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.serverCapabilities.add("authoritative_child_roster");
		let releaseRoster!: () => void;
		fakeClient.rlmChildrenGate = new Promise<void>((resolve) => {
			releaseRoster = resolve;
		});
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
		await connection.attach();

		const children = connection.getRlmChildSnapshots();
		emitRlmChildUpdate(fakeClient, "active-1", 13, {
			id: "child-live",
			label: "live child",
			status: "running",
			sessionDir: "/tmp/child-live",
		});
		releaseRoster();

		await expect(children).resolves.toEqual([expect.objectContaining({ id: "child-live", status: "running" })]);
	});

	it("refreshes initial snapshots after live events make the cached snapshot stale", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.attachResultFactory = (command) =>
			createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12, {
				state: createConnectionState(command.activeSessionId, "session-attached"),
				messages: [{ role: "user", content: "attached prompt", timestamp: 1 }],
			});
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");

		await connection.attach();
		emitSequencedQueueUpdate(fakeClient, "active-1", 13);

		const snapshot = await connection.getInitialSnapshot();
		expect(snapshot).toMatchObject({
			state: {
				sessionId: "session-current",
			},
			messages: [{ role: "user", content: "current prompt", timestamp: 4 }],
			sessionContext: {
				messages: [{ role: "user", content: "context prompt", timestamp: 3 }],
			},
		});
		// The session tree is fetched lazily (only when the tree/branch selector is
		// opened), so refreshing the initial snapshot must not request it.
		expect(snapshot.sessionTree).toBeUndefined();
		expect(fakeClient.requests.map((request) => request.type)).toEqual([
			"attach",
			"get_connection_state",
			"get_messages",
			"get_session_context",
		]);
	});

	it("ignores older sequenced events after an attach snapshot", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
		const events: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			events.push(event);
		});

		await connection.attach();
		fakeClient.emitMessage({
			type: "session_replaced",
			activeSessionId: "active-1",
			state: createConnectionState("active-1", "session-old"),
			messages: [{ role: "user", content: "old prompt", timestamp: 1 }],
			meta: {
				id: "active-1:10",
				protocol: DAEMON_PROTOCOL_INFO,
				activeSessionId: "active-1",
				sequence: 10,
				emittedAt: "2026-01-01T00:00:00.000Z",
			},
		});
		fakeClient.emitMessage({
			type: "session_replaced",
			activeSessionId: "active-1",
			state: createConnectionState("active-1", "session-new"),
			messages: [{ role: "user", content: "new prompt", timestamp: 2 }],
			meta: {
				id: "active-1:13",
				protocol: DAEMON_PROTOCOL_INFO,
				activeSessionId: "active-1",
				sequence: 13,
				emittedAt: "2026-01-01T00:00:00.000Z",
			},
		});

		expect(events).toEqual([
			{
				type: "session_replaced",
				state: expect.objectContaining({
					sessionId: "session-new",
				}),
				messages: [{ role: "user", content: "new prompt", timestamp: 2 }],
			},
		]);
		await expect(connection.getState()).resolves.toMatchObject({
			sessionId: "session-new",
		});
	});

	it("fails closed when a daemon disconnect invalidates an input pause", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.serverCapabilities.add("session_input_pause");
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
		await connection.attach();
		const events: AgentConnectionEvent[] = [];
		connection.subscribe((event) => {
			events.push(event);
		});

		const pause = await connection.acquireSessionInputPause("lease-1");
		fakeClient.disconnectForReconnect("shutdown");

		await expect(pause.release()).rejects.toThrow("invalidated by a daemon reconnect");
		await expect(connection.acquireSessionInputPause("lease-1")).rejects.toThrow("connection is closed");
		expect(events).toContainEqual(
			expect.objectContaining({
				type: "closed",
				error: expect.stringContaining("fence was invalidated"),
			}),
		);
	});

	it("releases an input pause whose acquisition resolves after disconnect", async () => {
		const fakeClient = new FakeDaemonClient();
		fakeClient.serverCapabilities.add("session_input_pause");
		let releaseAcquire!: () => void;
		fakeClient.inputPauseAcquireGate = new Promise<void>((resolve) => {
			releaseAcquire = resolve;
		});
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
		await connection.attach();

		const acquisition = connection.acquireSessionInputPause("lease-1");
		await vi.waitFor(() =>
			expect(fakeClient.requests.some((request) => request.type === "acquire_session_input_pause")).toBe(true),
		);
		fakeClient.disconnectForReconnect("shutdown");
		releaseAcquire();

		await expect(acquisition).rejects.toThrow("acquisition was invalidated by a daemon reconnect");
		expect(fakeClient.requests.filter((request) => request.type === "release_session_input_pause")).toHaveLength(1);
	});

	it("ignores delayed events from a retired daemon generation", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
		const steeringUpdates: Array<readonly string[]> = [];
		connection.subscribe((event) => {
			if (event.type === "session_event" && event.event.type === "session_action_update") {
				steeringUpdates.push(event.event.actions.steering);
			}
		});
		await connection.attach();
		const emitQueue = (generation: string, sequence: number, steering: string) => {
			fakeClient.emitMessage({
				type: "session_event",
				activeSessionId: "active-1",
				event: { type: "session_action_update", actions: { queuedCount: 1, steering: [steering], followUps: [] } },
				meta: {
					id: `${generation}:${sequence}`,
					protocol: DAEMON_PROTOCOL_INFO,
					activeSessionId: "active-1",
					sequence,
					cursor: { generation, sequence },
					emittedAt: "2026-01-01T00:00:00.000Z",
				},
			});
		};

		emitQueue("generation-new", 1, "new");
		emitQueue("generation-active-1", 13, "old");
		await vi.waitFor(() => expect(steeringUpdates).toEqual([["new"]]));

		await connection.attach();
		expect(fakeClient.requests.at(-1)).toMatchObject({
			type: "attach",
			resumeCursor: { generation: "generation-new", sequence: 1 },
		});
	});

	it("advertises the heartbeat_catalog capability on attach only when it tracks heartbeats", async () => {
		const attach = async (tracksHeartbeats?: boolean) => {
			const client = new FakeDaemonClient();
			const connection = await DaemonAgentConnection.attach(asDaemonClient(client), "active-1", {
				closeClientOnDispose: true,
				tracksHeartbeats,
			});
			await connection.dispose();
			return client.requests.find((request) => request.type === "attach") as { capabilities?: string[] };
		};
		expect((await attach(true)).capabilities).toContain("heartbeat_catalog");
		expect((await attach()).capabilities).not.toContain("heartbeat_catalog");
	});
});

const DEFERRAL_EVENT_BASE_SEQUENCE = 13;

function emitSequencedSessionEvent(client: FakeDaemonClient, activeSessionId: string, sequence: number): void {
	client.emitMessage({
		type: "session_event",
		activeSessionId,
		event: { type: "session_info_changed", name: String(sequence) },
		meta: {
			id: `${activeSessionId}:${sequence}`,
			protocol: DAEMON_PROTOCOL_INFO,
			activeSessionId,
			sequence,
			cursor: { generation: `generation-${activeSessionId}`, sequence },
			emittedAt: "2026-01-01T00:00:00.000Z",
		},
	} satisfies DaemonOutbound);
}

async function nextMessageLoopTurn(): Promise<void> {
	await new Promise<void>((resolve) => setImmediate(resolve));
	await new Promise<void>((resolve) => setImmediate(resolve));
}

describe("DaemonAgentConnection deferred session events", () => {
	it("defers events between attach and flush so the attach snapshot stays usable", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			deferSessionEvents: true,
		});
		try {
			await connection.attach();
			const delivered: AgentConnectionEvent[] = [];
			connection.subscribe((event) => {
				delivered.push(event);
			});

			emitSequencedSessionEvent(fakeClient, "active-1", DEFERRAL_EVENT_BASE_SEQUENCE);
			await nextMessageLoopTurn();
			expect(delivered).toEqual([]);

			fakeClient.requests.length = 0;
			const snapshot = await connection.getInitialSnapshot();
			expect(fakeClient.requests.map((request) => request.type)).not.toContain("get_messages");
			expect(snapshot.state.activeSessionId).toBe("active-1");

			await connection.flushBufferedSessionEvents();
			expect(delivered).toHaveLength(1);
			expect(delivered[0]).toMatchObject({ type: "session_event" });

			emitSequencedSessionEvent(fakeClient, "active-1", DEFERRAL_EVENT_BASE_SEQUENCE + 1);
			await nextMessageLoopTurn();
			expect(delivered).toHaveLength(2);
		} finally {
			await connection.dispose();
		}
	});

	it("delivers events live and refetches when deferral is not opted in", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
		try {
			await connection.attach();
			const delivered: AgentConnectionEvent[] = [];
			connection.subscribe((event) => {
				delivered.push(event);
			});

			emitSequencedSessionEvent(fakeClient, "active-1", DEFERRAL_EVENT_BASE_SEQUENCE);
			await nextMessageLoopTurn();
			expect(delivered).toHaveLength(1);

			fakeClient.requests.length = 0;
			await connection.getInitialSnapshot();
			expect(fakeClient.requests.map((request) => request.type)).toContain("get_messages");
		} finally {
			await connection.dispose();
		}
	});

	it("drops deferred events a resync snapshot already contains", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			deferSessionEvents: true,
		});
		try {
			await connection.attach();
			const delivered: AgentConnectionEvent[] = [];
			connection.subscribe((event) => {
				delivered.push(event);
			});

			const resyncSequence = DEFERRAL_EVENT_BASE_SEQUENCE + 2;
			emitSequencedSessionEvent(fakeClient, "active-1", DEFERRAL_EVENT_BASE_SEQUENCE);
			emitSequencedSessionEvent(fakeClient, "active-1", DEFERRAL_EVENT_BASE_SEQUENCE + 1);
			fakeClient.emitMessage({
				type: "session_resynced",
				activeSessionId: "active-1",
				snapshot: createAttachResult("active-1", undefined, undefined, resyncSequence).snapshot,
				meta: {
					id: `active-1:${resyncSequence}`,
					protocol: DAEMON_PROTOCOL_INFO,
					activeSessionId: "active-1",
					sequence: resyncSequence,
					cursor: { generation: "generation-active-1", sequence: resyncSequence },
					emittedAt: "2026-01-01T00:00:00.000Z",
				},
			} satisfies DaemonOutbound);
			await nextMessageLoopTurn();
			expect(delivered.filter((event) => event.type === "session_resynced")).toHaveLength(1);
			expect(delivered.filter((event) => event.type === "session_event")).toEqual([]);

			await connection.flushBufferedSessionEvents();
			expect(delivered.filter((event) => event.type === "session_event")).toEqual([]);

			emitSequencedSessionEvent(fakeClient, "active-1", resyncSequence + 1);
			await connection.flushBufferedSessionEvents();
			expect(delivered.filter((event) => event.type === "session_event")).toHaveLength(1);
		} finally {
			await connection.dispose();
		}
	});

	it("resyncs an already rendered snapshot when deferral overflows", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			deferSessionEvents: true,
		});
		try {
			await connection.attach();
			const renderedSnapshot = await connection.getInitialSnapshot();
			const delivered: AgentConnectionEvent[] = [];
			connection.subscribe((event) => {
				delivered.push(event);
			});

			// The initial renderer already has its snapshot when the buffer overflows.
			for (let index = 0; index <= 1000; index++) {
				emitSequencedSessionEvent(fakeClient, "active-1", DEFERRAL_EVENT_BASE_SEQUENCE + index);
			}
			await nextMessageLoopTurn();
			expect(delivered).toHaveLength(0);
			const recoveredMessages: AgentMessage[] = [{ role: "user", content: "recovered", timestamp: 1 }];
			fakeClient.attachResultFactory = (command) => {
				emitSequencedSessionEvent(fakeClient, "active-1", 1014);
				return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 1013, {
					messages: recoveredMessages,
				});
			};
			await connection.flushBufferedSessionEvents();
			expect(renderedSnapshot.messages).toEqual([]);
			expect(delivered).toEqual([
				expect.objectContaining({
					type: "session_resynced",
					snapshot: expect.objectContaining({ messages: recoveredMessages }),
				}),
				{ type: "session_event", event: { type: "session_info_changed", name: "1014" } },
			]);

			emitSequencedSessionEvent(fakeClient, "active-1", 1015);
			await nextMessageLoopTurn();
			expect(delivered).toHaveLength(3);
		} finally {
			await connection.dispose();
		}
	});

	it.each([true, false])(
		"finishes overflow recovery under continuous traffic (snapshot covers overflow=%s)",
		async (covered) => {
			const fakeClient = new FakeDaemonClient();
			const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
				deferSessionEvents: true,
			});
			let release!: () => void;
			const rendering = new Promise<void>((resolve) => {
				release = resolve;
			});
			try {
				await connection.attach();
				let sequence = 12;
				for (let index = 0; index <= 1000; index++) emitSequencedSessionEvent(fakeClient, "active-1", ++sequence);
				let attaches = 0;
				fakeClient.attachResultFactory = (command) => {
					if (++attaches > 3) throw new Error("overflow recovery did not terminate");
					const before = sequence;
					if (covered || attaches === 1) {
						for (let index = 0; index <= 1000; index++)
							emitSequencedSessionEvent(fakeClient, "active-1", ++sequence);
					}
					return createAttachResult(
						command.activeSessionId,
						command.clientId,
						command.capabilities,
						!covered && attaches === 1 ? before : sequence,
					);
				};
				const delivered: AgentConnectionEvent[] = [];
				connection.subscribe((event) => {
					delivered.push(event);
					if (event.type === "session_resynced") return rendering;
				});
				const flush = connection.flushBufferedSessionEvents();
				await nextMessageLoopTurn();
				expect(attaches).toBe(covered ? 1 : 2);
				expect(delivered.at(-1)).toMatchObject({
					type: "session_resynced",
					snapshot: { lastEventSequence: sequence },
				});
				const beforeLive = delivered.length;
				for (let index = 0; index < 2000; index++) emitSequencedSessionEvent(fakeClient, "active-1", ++sequence);
				expect(delivered.slice(beforeLive)).toHaveLength(2000);
				release();
				await flush;
				expect(attaches).toBe(covered ? 1 : 2);
				expect(delivered.some((event) => event.type === "closed")).toBe(false);
			} finally {
				release();
				await connection.dispose();
			}
		},
	);

	it.each([
		["replacement", false],
		["replacement", true],
		["streamed replacement", true],
		["reattach", false],
		["reattach", true],
	] as const)("discards overflow recovery across %s (recovery started=%s)", async (change, recoveryStarted) => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			deferSessionEvents: true,
		});
		let release!: () => void;
		const gate = new Promise<void>((resolve) => {
			release = resolve;
		});
		try {
			await connection.attach();
			for (let sequence = 13; sequence <= 1013; sequence++)
				emitSequencedSessionEvent(fakeClient, "active-1", sequence);
			const request = fakeClient.request.bind(fakeClient);
			vi.spyOn(fakeClient, "request").mockImplementation(async (command, ...options) => {
				if (command.type === "attach" && change === "streamed replacement") {
					const result = createAttachResult("active-1", undefined, undefined, 1013);
					result.snapshotStream = { id: "old-recovery", messageCount: 0, targetChunkBytes: 512 * 1024 };
					fakeClient.emitMessage({
						type: "session_snapshot_begin",
						activeSessionId: "active-1",
						snapshotId: "old-recovery",
						snapshot: result.snapshot,
						messageCount: 0,
						targetChunkBytes: 512 * 1024,
						purpose: "attach",
					});
					return { type: "response", command: command.type, success: true, data: result };
				}
				if (command.type === "attach") await gate;
				if (command.type === "switch_session")
					return {
						type: "response",
						command: command.type,
						success: false,
						error: "Session already active",
						errorInfo: {
							code: "session_already_active",
							activeSessionId: "active-2",
							sessionPath: "/tmp/target.jsonl",
						},
					};
				if (command.type === "reattach")
					return {
						type: "response",
						command: command.type,
						success: true,
						data: createAttachResult("active-2", undefined, undefined, 1),
					};
				return request(command, ...options);
			});
			const delivered: AgentConnectionEvent[] = [];
			connection.subscribe((event) => {
				delivered.push(event);
			});
			const flush = recoveryStarted ? connection.flushBufferedSessionEvents() : undefined;
			await nextMessageLoopTurn();
			if (change === "reattach") {
				await connection.switchSession("/tmp/target.jsonl");
			} else {
				fakeClient.emitMessage({
					type: "session_replaced",
					activeSessionId: "active-1",
					state: createConnectionState("active-1", "replacement"),
					messages: [],
				});
			}
			const activeSessionId = change === "reattach" ? "active-2" : "active-1";
			const sequence = change === "reattach" ? 2 : 1014;
			emitSequencedSessionEvent(fakeClient, activeSessionId, sequence);
			if (change === "streamed replacement") {
				fakeClient.emitMessage({
					type: "session_snapshot_end",
					activeSessionId: "active-1",
					snapshotId: "old-recovery",
					chunkCount: 0,
					lastEventSequence: 1013,
					lastEventCursor: { generation: "generation-active-1", sequence: 1013 },
				});
			}
			release();
			await (flush ?? connection.flushBufferedSessionEvents());
			expect(delivered.map((event) => event.type)).toEqual(["session_replaced", "session_event"]);
			expect(delivered[1]).toEqual({
				type: "session_event",
				event: { type: "session_info_changed", name: String(sequence) },
			});
			if (change !== "reattach")
				expect(
					(connection as unknown as { latestSnapshot: { state: AgentConnectionState } }).latestSnapshot.state
						.sessionId,
				).toBe("replacement");
			expect((await connection.getState()).activeSessionId).toBe(activeSessionId);
		} finally {
			release();
			await connection.dispose();
		}
	});

	it("does not advance event cursors for an attach superseded before its snapshot finishes", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			deferSessionEvents: true,
		});
		try {
			await connection.attach();
			fakeClient.attachResultFactory = (command) => ({
				...createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 100),
				snapshotStream: { id: "superseded", messageCount: 0, targetChunkBytes: 512 * 1024 },
			});
			const pendingAttach = connection.attach();
			await nextMessageLoopTurn();
			fakeClient.emitMessage({
				type: "session_snapshot_begin",
				activeSessionId: "active-1",
				snapshotId: "superseded",
				snapshot: createAttachResult("active-1", undefined, undefined, 100).snapshot,
				messageCount: 0,
				targetChunkBytes: 512 * 1024,
				purpose: "attach",
			});
			fakeClient.emitMessage({
				type: "session_replaced",
				activeSessionId: "active-1",
				state: createConnectionState("active-1", "replacement"),
				messages: [],
			});
			fakeClient.emitMessage({
				type: "session_snapshot_end",
				activeSessionId: "active-1",
				snapshotId: "superseded",
				chunkCount: 0,
				lastEventSequence: 100,
				lastEventCursor: { generation: "generation-active-1", sequence: 100 },
			});
			await pendingAttach;
			const delivered: AgentConnectionEvent[] = [];
			connection.subscribe((event) => {
				delivered.push(event);
			});
			emitSequencedSessionEvent(fakeClient, "active-1", 13);
			await connection.flushBufferedSessionEvents();
			expect(delivered).toEqual([{ type: "session_event", event: { type: "session_info_changed", name: "13" } }]);
			const snapshot = await connection.getInitialSnapshot();
			expect(snapshot.lastEventCursor).toEqual({ generation: "generation-active-1", sequence: 13 });
		} finally {
			await connection.dispose();
		}
	});

	it.each([0, 2, 3])(
		"releases a superseded attach with %i snapshot frames received before its response",
		async (frameCount) => {
			const fakeClient = new FakeDaemonClient();
			const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
			let release!: () => void;
			const responseGate = new Promise<void>((resolve) => {
				release = resolve;
			});
			try {
				await connection.attach();
				const result = createAttachResult("active-1", undefined, undefined, 100);
				fakeClient.attachResultFactory = () => ({
					...result,
					snapshotStream: { id: "abandoned", messageCount: 1, targetChunkBytes: 512 * 1024 },
				});
				const request = fakeClient.request.bind(fakeClient);
				vi.spyOn(fakeClient, "request").mockImplementation(async (...args) => {
					const response = await request(...args);
					if (args[0].type === "attach") await responseGate;
					return response;
				});
				const frames: DaemonOutbound[] = [
					{
						type: "session_snapshot_begin",
						activeSessionId: "active-1",
						snapshotId: "abandoned",
						snapshot: result.snapshot,
						messageCount: 1,
						targetChunkBytes: 512 * 1024,
						purpose: "attach",
					},
					{
						type: "session_snapshot_chunk",
						activeSessionId: "active-1",
						snapshotId: "abandoned",
						index: 0,
						messages: [{ role: "user", content: "old transcript", timestamp: 1 }],
					},
					{
						type: "session_snapshot_end",
						activeSessionId: "active-1",
						snapshotId: "abandoned",
						chunkCount: 1,
						lastEventSequence: 100,
					},
				];
				const pendingAttach = connection.attach();
				for (const frame of frames.slice(0, frameCount)) fakeClient.emitMessage(frame);
				fakeClient.emitMessage({
					type: "session_replaced",
					activeSessionId: "active-1",
					state: createConnectionState("active-1", "replacement"),
					messages: [],
				});
				release();
				await pendingAttach;
				expect(
					(connection as unknown as { snapshotAssemblies: Map<string, unknown> }).snapshotAssemblies.size,
				).toBe(0);
				for (const frame of frames.slice(frameCount)) fakeClient.emitMessage(frame);
				await nextMessageLoopTurn();
				expect(
					(connection as unknown as { snapshotAssemblies: Map<string, unknown> }).snapshotAssemblies.size,
				).toBe(0);
				expect((await connection.getInitialSnapshot()).state.sessionId).toBe("replacement");
			} finally {
				release();
				await connection.dispose();
			}
		},
	);

	it.each(["before response", "during stream", "after end"] as const)(
		"rejects a closed attach immediately %s",
		async (timing) => {
			const fakeClient = new FakeDaemonClient();
			const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
			const result = createAttachResult("active-1", undefined, undefined, 12);
			fakeClient.attachResultFactory = () => ({
				...result,
				snapshotStream: { id: "closed", messageCount: 0, targetChunkBytes: 512 * 1024 },
			});
			const rejected = vi.fn();
			const resolved = vi.fn();
			const events: AgentConnectionEvent[] = [];
			connection.subscribe((event) => {
				events.push(event);
			});
			const pendingAttach = connection.attach().then(resolved, rejected);
			try {
				if (timing !== "before response") await nextMessageLoopTurn();
				fakeClient.emitMessage({
					type: "session_snapshot_begin",
					activeSessionId: "active-1",
					snapshotId: "closed",
					snapshot: result.snapshot,
					messageCount: 0,
					targetChunkBytes: 512 * 1024,
					purpose: "attach",
				});
				if (timing === "after end") {
					fakeClient.emitMessage({
						type: "session_snapshot_end",
						activeSessionId: "active-1",
						snapshotId: "closed",
						chunkCount: 0,
						lastEventSequence: 12,
					});
				}
				fakeClient.emitMessage({ type: "session_closed", activeSessionId: "active-1", reason: "killed" });
				await nextMessageLoopTurn();
				expect(rejected).toHaveBeenCalledExactlyOnceWith(expect.any(Error));
				expect(resolved).not.toHaveBeenCalled();
				expect(
					(connection as unknown as { snapshotAssemblies: Map<string, unknown> }).snapshotAssemblies.size,
				).toBe(0);
				expect(fakeClient.closeCount).toBe(0);
				for (const purpose of ["attach", "replacement", "resync"] as const) {
					fakeClient.emitMessage({
						type: "session_snapshot_begin",
						activeSessionId: "active-1",
						snapshotId: purpose,
						snapshot: result.snapshot,
						messageCount: 1,
						targetChunkBytes: 512 * 1024,
						purpose,
					});
					fakeClient.emitMessage({
						type: "session_snapshot_chunk",
						activeSessionId: "active-1",
						snapshotId: purpose,
						index: 0,
						messages: [{ role: "user", content: "late transcript", timestamp: 1 }],
					});
					fakeClient.emitMessage({
						type: "session_snapshot_end",
						activeSessionId: "active-1",
						snapshotId: purpose,
						chunkCount: 1,
						lastEventSequence: 12,
					});
				}
				fakeClient.emitMessage({
					type: "session_snapshot_failed",
					activeSessionId: "active-1",
					snapshotId: "failed-after-close",
					error: "stream closed",
				});
				expect(
					(connection as unknown as { snapshotAssemblies: Map<string, unknown> }).snapshotAssemblies.size,
				).toBe(0);
				fakeClient.emitMessage({ type: "session_closed", activeSessionId: "active-1", reason: "killed" });
				await nextMessageLoopTurn();
				expect(
					(connection as unknown as { snapshotAssemblies: Map<string, unknown> }).snapshotAssemblies.size,
				).toBe(0);
				expect(events).toEqual([expect.objectContaining({ type: "closed" })]);
				const requestCount = fakeClient.requests.length;
				await expect(connection.attach()).rejects.toThrow("closed");
				expect(fakeClient.requests).toHaveLength(requestCount);
			} finally {
				await connection.dispose();
				await pendingAttach;
			}
		},
	);

	it.each(["session_replaced", "session_resynced"] as const)(
		"drops a %s superseded during the initial render",
		async (type) => {
			const fakeClient = new FakeDaemonClient();
			const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
			let release!: () => void;
			const initialRenderPromise = new Promise<void>((resolve) => {
				release = resolve;
			});
			const ui = {
				agentConnection: connection,
				sessionEventQueue: Promise.resolve(),
				sessionEventGeneration: 0,
				initialRenderPromise,
				refreshCommandCatalogForCurrentSession: vi.fn(async () => {}),
				renderResyncedSession: vi.fn(async () => {}),
				resetSideQuestion: vi.fn(),
				resetExtensionUI: vi.fn(),
				applyConnectionStateSnapshot: vi.fn(),
				resetCurrentSessionRenderState: vi.fn(),
				rebindCurrentSession: vi.fn(async () => {}),
				renderInitialMessages: vi.fn(async () => {}),
				ui: { requestRender: vi.fn() },
				showError: vi.fn(),
			};
			try {
				await connection.attach();
				(InteractiveMode.prototype as unknown as { subscribeToAgent(this: typeof ui): void }).subscribeToAgent.call(
					ui,
				);
				const snapshot = createAttachResult("active-1", undefined, undefined, 12).snapshot;
				fakeClient.emitMessage(
					type === "session_replaced"
						? { type, activeSessionId: "active-1", state: snapshot.state, messages: [] }
						: { type, activeSessionId: "active-1", snapshot },
				);
				await nextMessageLoopTurn();
				const state = createConnectionState("active-1", "latest");
				fakeClient.emitMessage({ type: "session_replaced", activeSessionId: "active-1", state, messages: [] });
				release();
				await ui.sessionEventQueue;
				expect(ui.renderResyncedSession).not.toHaveBeenCalled();
				expect(ui.resetSideQuestion).toHaveBeenCalledOnce();
				expect(ui.resetExtensionUI).toHaveBeenCalledOnce();
				expect(ui.applyConnectionStateSnapshot).toHaveBeenCalledExactlyOnceWith(state);
				expect(ui.resetCurrentSessionRenderState).toHaveBeenCalledOnce();
				expect(ui.rebindCurrentSession).toHaveBeenCalledOnce();
				expect(ui.renderInitialMessages).toHaveBeenCalledOnce();
				expect(ui.ui.requestRender).toHaveBeenCalledOnce();
				expect(ui.showError).not.toHaveBeenCalled();
			} finally {
				release();
				await connection.dispose();
			}
		},
	);

	it("keeps live events behind the whole replay and shares concurrent flushes", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			deferSessionEvents: true,
		});
		let release!: () => void;
		const gate = new Promise<void>((resolve) => {
			release = resolve;
		});
		try {
			await connection.attach();
			const delivered: string[] = [];
			const ui = {
				agentConnection: connection,
				sessionEventQueue: Promise.resolve(),
				sessionEventGeneration: 0,
				handleEvent: async (event: { type: string; name?: string }) => {
					delivered.push(event.name!);
					if (delivered.length === 1) await gate;
				},
				showError: vi.fn(),
			};
			(InteractiveMode.prototype as unknown as { subscribeToAgent(this: typeof ui): void }).subscribeToAgent.call(
				ui,
			);
			emitSequencedSessionEvent(fakeClient, "active-1", 13);
			emitSequencedSessionEvent(fakeClient, "active-1", 14);
			const flush = connection.flushBufferedSessionEvents();
			await nextMessageLoopTurn();
			emitSequencedSessionEvent(fakeClient, "active-1", 15);
			expect(connection.flushBufferedSessionEvents()).toBe(flush);
			expect(delivered).toEqual(["13"]);
			release();
			await flush;
			await ui.sessionEventQueue;
			expect(delivered).toEqual(["13", "14", "15"]);
			expect(ui.showError).not.toHaveBeenCalled();
		} finally {
			release();
			await connection.dispose();
		}
	});

	it("leaves streaming state in the attach snapshot unchanged until replay", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			deferSessionEvents: true,
		});
		try {
			await connection.attach();
			const message = fauxAssistantMessage("streaming response");
			fakeClient.emitMessage({
				type: "session_event",
				activeSessionId: "active-1",
				event: { type: "message_start", message },
			});
			const snapshot = await connection.getInitialSnapshot();
			expect(snapshot.streamingMessage).toBeUndefined();
			const delivered: AgentConnectionEvent[] = [];
			connection.subscribe((event) => {
				delivered.push(event);
			});
			await connection.flushBufferedSessionEvents();
			expect(delivered).toEqual([{ type: "session_event", event: { type: "message_start", message } }]);
			expect((await connection.getInitialSnapshot()).streamingMessage).toEqual(message);
		} finally {
			await connection.dispose();
		}
	});

	it.each([true, false])(
		"delivers attach-time extension prompts without skipping transcript events (subscribed=%s)",
		async (subscribed) => {
			const fakeClient = new FakeDaemonClient();
			const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
				deferSessionEvents: true,
			});
			try {
				const delivered: AgentConnectionEvent[] = [];
				const listener = (event: AgentConnectionEvent) => {
					delivered.push(event);
				};
				if (subscribed) connection.subscribe(listener);
				fakeClient.attachResultFactory = (command) => {
					fakeClient.emitMessage({
						type: "extension_ui_request",
						activeSessionId: "active-1",
						id: "editor-request",
						method: "editor",
						payload: { title: "Edit" },
						meta: {
							id: "active-1:14",
							protocol: DAEMON_PROTOCOL_INFO,
							activeSessionId: "active-1",
							sequence: 14,
							cursor: { generation: "generation-active-1", sequence: 14 },
							emittedAt: "2026-01-01T00:00:00.000Z",
						},
					});
					return createAttachResult(command.activeSessionId, command.clientId, command.capabilities, 12);
				};
				await connection.attach();
				// The transport releases earlier transcript events only after the snapshot.
				emitSequencedSessionEvent(fakeClient, "active-1", 13);
				if (!subscribed) {
					expect(delivered).toEqual([]);
					connection.subscribe(listener);
				}
				expect(delivered).toEqual([
					{
						type: "extension_ui_request",
						request: { id: "editor-request", method: "editor", payload: { title: "Edit" } },
					},
				]);
				await connection.flushBufferedSessionEvents();
				expect(delivered.at(-1)).toEqual({
					type: "session_event",
					event: { type: "session_info_changed", name: "13" },
				});
				const additionalListener = vi.fn();
				connection.subscribe(additionalListener);
				expect(additionalListener).not.toHaveBeenCalled();
			} finally {
				await connection.dispose();
			}
		},
	);

	it.each(["notify", "setStatus", "setWidget"])(
		"bounds attach-time %s updates without dropping queued dialogs",
		async (method) => {
			const fakeClient = new FakeDaemonClient();
			const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
			try {
				await connection.attach();
				fakeClient.emitMessage({
					type: "extension_ui_request",
					activeSessionId: "active-1",
					id: "dialog",
					method: "editor",
					payload: { title: "Edit" },
				});
				for (let index = 0; index < 2000; index++) {
					fakeClient.emitMessage({
						type: "extension_ui_request",
						activeSessionId: "active-1",
						id: String(index),
						method,
						payload: {
							message: String(index),
							statusKey: "progress",
							statusText: String(index),
							widgetKey: "progress",
							widgetLines: [String(index)],
						},
					});
				}
				const listener = vi.fn();
				connection.subscribe(listener);
				expect(listener).toHaveBeenCalledTimes(128);
				expect(listener).toHaveBeenNthCalledWith(1, {
					type: "extension_ui_request",
					request: { id: "dialog", method: "editor", payload: { title: "Edit" } },
				});
				expect(listener).toHaveBeenLastCalledWith({
					type: "extension_ui_request",
					request: expect.objectContaining({ id: "1999", method }),
				});
				expect(fakeClient.requests.filter((request) => request.type === "extension_ui_response")).toEqual([]);
			} finally {
				await connection.dispose();
			}
		},
	);

	it.each(["disposed", "replaced", "restarted", "update-session", "update-transport"])(
		"discards queued extension prompts when %s before subscription",
		async (boundary) => {
			const fakeClient = new FakeDaemonClient();
			const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1");
			try {
				await connection.attach();
				fakeClient.emitMessage({
					type: "extension_ui_request",
					activeSessionId: "active-1",
					id: "old",
					method: "editor",
					payload: { title: "Old" },
					meta: {
						id: "old:13",
						protocol: DAEMON_PROTOCOL_INFO,
						activeSessionId: "active-1",
						sequence: 13,
						cursor: { generation: "generation-active-1", sequence: 13 },
						emittedAt: "2026-01-01T00:00:00.000Z",
					},
				});
				if (boundary === "disposed") await connection.dispose();
				else if (boundary === "replaced")
					fakeClient.emitMessage({
						type: "session_replaced",
						activeSessionId: "active-1",
						state: createConnectionState("active-1", "replacement"),
						messages: [],
					});
				else if (boundary.startsWith("update-")) {
					fakeClient.updateRestartSessions = [{ activeSessionId: "active-1", sessionId: "session-current" }];
					if (boundary === "update-session") {
						fakeClient.emitMessage({ type: "session_closed", activeSessionId: "active-1", reason: "update" });
					} else {
						fakeClient.emitClose(new DaemonSocketClosedError("/tmp/prime-agent.sock", "update"));
					}
				} else {
					const snapshot = createAttachResult("active-1", undefined, undefined, 1).snapshot;
					snapshot.lastEventCursor = { generation: "restarted", sequence: 1 };
					fakeClient.emitMessage({ type: "session_resynced", activeSessionId: "active-1", snapshot });
				}
				let resolveConnected!: () => void;
				const connected = new Promise<void>((resolve) => {
					resolveConnected = resolve;
				});
				const listener = vi.fn((event: AgentConnectionEvent) => {
					if (event.type === "connection_status" && event.status === "connected") resolveConnected();
				});
				connection.subscribe(listener);
				expect(listener).not.toHaveBeenCalled();
				if (boundary.startsWith("update-")) {
					await connected;
					expect(listener).toHaveBeenCalledWith({ type: "connection_status", status: "connected" });
					expect(listener).not.toHaveBeenCalledWith(expect.objectContaining({ type: "extension_ui_request" }));
					fakeClient.emitMessage({
						type: "extension_ui_request",
						activeSessionId: "active-1",
						id: "new",
						method: "editor",
						payload: { title: "New" },
					});
					expect(listener).toHaveBeenLastCalledWith({
						type: "extension_ui_request",
						request: { id: "new", method: "editor", payload: { title: "New" } },
					});
				}
			} finally {
				await connection.dispose();
			}
		},
	);

	it.each([1, 1001])(
		"drops %i buffered events and overflow superseded by a restarted worker snapshot",
		async (count) => {
			const fakeClient = new FakeDaemonClient();
			const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
				deferSessionEvents: true,
			});
			try {
				await connection.attach();
				for (let index = 0; index < count; index++) emitSequencedSessionEvent(fakeClient, "active-1", 100 + index);
				const snapshot = createAttachResult("active-1", undefined, undefined, 1).snapshot;
				snapshot.lastEventCursor = { generation: "restarted", sequence: 1 };
				fakeClient.emitMessage({
					type: "session_snapshot_begin",
					activeSessionId: "active-1",
					snapshotId: "restart",
					snapshot,
					messageCount: 0,
					targetChunkBytes: 512 * 1024,
					purpose: "resync",
				});
				fakeClient.emitMessage({
					type: "session_snapshot_end",
					activeSessionId: "active-1",
					snapshotId: "restart",
					chunkCount: 0,
					lastEventSequence: 1,
					lastEventCursor: snapshot.lastEventCursor,
				});
				await nextMessageLoopTurn();
				const delivered: AgentConnectionEvent[] = [];
				connection.subscribe((event) => {
					delivered.push(event);
				});
				await connection.flushBufferedSessionEvents();
				expect(delivered).toEqual([]);
				expect(fakeClient.requests.filter((request) => request.type === "attach")).toHaveLength(1);
				expect((await connection.getInitialSnapshot()).lastEventSequence).toBe(1);
			} finally {
				await connection.dispose();
			}
		},
	);

	it("replays only newer target-session events when reattaching with an inline snapshot", async () => {
		const fakeClient = new FakeDaemonClient();
		const connection = new DaemonAgentConnection(asDaemonClient(fakeClient), "active-1", {
			deferSessionEvents: true,
		});
		try {
			await connection.attach();
			emitSequencedSessionEvent(fakeClient, "active-1", 13);
			const request = fakeClient.request.bind(fakeClient);
			vi.spyOn(fakeClient, "request").mockImplementation(async (command, ...options) => {
				if (command.type === "switch_session")
					return {
						type: "response",
						command: command.type,
						success: false,
						error: "Session already active",
						errorInfo: {
							code: "session_already_active",
							activeSessionId: "active-2",
							sessionPath: "/tmp/target.jsonl",
						},
					};
				if (command.type === "reattach") {
					emitSequencedSessionEvent(fakeClient, "active-2", 2);
					return {
						type: "response",
						command: command.type,
						success: true,
						data: createAttachResult("active-2", undefined, undefined, 1),
					};
				}
				return request(command, ...options);
			});
			await connection.switchSession("/tmp/target.jsonl");
			const delivered: AgentConnectionEvent[] = [];
			connection.subscribe((event) => {
				delivered.push(event);
			});
			await connection.flushBufferedSessionEvents();
			expect(delivered).toEqual([{ type: "session_event", event: { type: "session_info_changed", name: "2" } }]);
		} finally {
			await connection.dispose();
		}
	});

	it("sends abort_and_send_queued only behind the advertised daemon capability", async () => {
		const send = (client: FakeDaemonClient) =>
			new DaemonAgentConnection(asDaemonClient(client), "active-1").abortAndSendQueued();
		const capable = new FakeDaemonClient();
		capable.serverCapabilities.add("abort_and_send_queued");
		await expect(send(capable)).resolves.toBeUndefined();
		expect(capable.requests).toEqual([{ type: "abort_and_send_queued", activeSessionId: "active-1" }]);
		const older = new FakeDaemonClient();
		await expect(send(older)).resolves.toBeUndefined();
		expect(older.requests).toEqual([{ type: "abort", activeSessionId: "active-1" }]);
		const stale = Object.assign(new FakeDaemonClient(), { abortAndSendQueuedUnknownCommand: true });
		stale.serverCapabilities.add("abort_and_send_queued");
		await expect(send(stale)).resolves.toBeUndefined();
		expect(stale.requests.map(({ type }) => type)).toEqual(["abort_and_send_queued", "abort"]);
	});
});
