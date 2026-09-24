import { randomUUID } from "node:crypto";
import { dirname, join } from "node:path";
import type { Api, AssistantMessage, Model } from "@earendil-works/pi-ai";
import { SessionManager } from "./session-manager.js";

/**
 * Durable storage for `/btw` side conversations.
 *
 * A side question is a real model run that spends real tokens, but it is
 * deliberately kept out of the main session context. Historically that meant it
 * was kept out of the session *file* too, so both the transcript and its usage
 * were lost when the pane closed and no usage ledger could ever account for it.
 *
 * Storage mirrors how RLM children are already persisted: the transcript is a
 * normal session file under the parent's artifact directory, linked back by its
 * `parentSession` header, and the parent records a pointer to it.
 *
 *   ~/.prime/agent/sessions/<parent>.jsonl                           (pointer entry)
 *   ~/.prime/agent/session-artifacts/<parent>/btw-<id>/<side>.jsonl  (transcript)
 *
 * Three invariants matter and are covered by tests:
 *
 * 1. The parent session gains exactly one `custom` entry per pane. `custom`
 *    entries are ignored by buildSessionContext, so `/btw` still never enters
 *    the main context — only the on-disk record changes.
 * 2. The pointer is written once and never rewritten. Session files are
 *    append-only, so mutable state (turn count, terminal status) lives in the
 *    transcript, which is the source of truth for the conversation.
 * 3. Only the new side turns are written. The parent context that a side
 *    question replays is never copied into the transcript: copy-forward would
 *    duplicate the whole main conversation, and its usage, per side question.
 */

/** customType of the pointer entry appended to the parent session. */
export const SIDE_QUESTION_POINTER_TYPE = "side_question";

/** customType of the terminal status entry appended to the transcript. */
export const SIDE_QUESTION_STATUS_TYPE = "side_question_status";

/** Directory prefix for side transcripts, distinct from RLM `sub-` children. */
const SIDE_QUESTION_DIR_PREFIX = "btw-";

export type SideQuestionRecordStatus = "complete" | "cancelled" | "error";

/** Immutable pointer persisted in the parent session. */
export interface SideQuestionPointer {
	/** Stable id of the pane this transcript belongs to. */
	btwId: string;
	/** Absolute path of the side-question transcript. */
	sessionFile: string;
	/** Id of the side-question session, matching the transcript header. */
	sessionId: string;
	/** First question asked in the pane, so listings need not open the file. */
	question: string;
	/** ISO timestamp of the first recorded turn. */
	startedAt: string;
}

/** Terminal status entry appended to the transcript when a pane settles. */
export interface SideQuestionStatusRecord {
	status: SideQuestionRecordStatus;
	errorMessage?: string;
}

/**
 * Records side-question turns as they settle. Every method is best-effort: a
 * storage failure must never take down the answer the user is reading, so the
 * recorder disables itself after the first error instead of propagating.
 */
export interface SideQuestionRecorder {
	/**
	 * Record a completed turn. `assistant` carries the usage for the answer the
	 * user kept. `auxiliary` carries any other completions the run paid for —
	 * compaction summaries and a context-overflow answer that was retried — so
	 * the transcript accounts for the whole run, not only its final call.
	 */
	recordTurn(question: string, assistant: AssistantMessage, auxiliary?: readonly AssistantMessage[]): void;
	/** Record how the pane settled. Ignored when no turn was ever recorded. */
	recordStatus(status: SideQuestionRecordStatus, errorMessage?: string): void;
}

export interface SideQuestionStoreOptions {
	/** Parent session manager; supplies the artifact root, cwd, and pointer entry. */
	parent: SessionManager;
	/** Model of the side run, recorded so the transcript reports its own model. */
	model: Model<Api>;
	/** Stable pane id. Reused across follow-ups so one pane is one transcript. */
	btwId: string;
	/** Test seam. Defaults to the real SessionManager. */
	createSessionManager?: (cwd: string, sessionDir: string) => SessionManager;
	/** Test seam for observing suppressed failures. */
	onError?: (error: unknown) => void;
}

/**
 * `session-artifacts` lives beside the session directory. This mirrors
 * getSessionArtifactPath() in session-manager.ts, which is not exported.
 */
function artifactDirFor(parent: SessionManager, parentSessionId: string): string {
	return join(dirname(parent.getSessionDir()), "session-artifacts", parentSessionId);
}

export function createSideQuestionStore(options: SideQuestionStoreOptions): SideQuestionRecorder {
	const { parent, model, btwId } = options;
	const createSessionManager =
		options.createSessionManager ?? ((sessionCwd: string, dir: string) => SessionManager.create(sessionCwd, dir));

	let manager: SessionManager | undefined;
	// One failure disables storage for the rest of the pane: a half-written
	// transcript that keeps throwing is worse than no transcript at all.
	let disabled = false;

	/**
	 * Created lazily on the first settled turn so a pane cancelled before any
	 * answer arrives leaves no empty transcript behind.
	 */
	const ensureStarted = (question: string): SessionManager | undefined => {
		if (manager) {
			return manager;
		}
		const parentFile = parent.getSessionFile();
		if (!parentFile) {
			// An unpersisted parent (headless/in-memory) has nowhere to anchor a
			// child transcript, so skip storage rather than orphan one.
			disabled = true;
			return undefined;
		}
		const sessionDir = join(artifactDirFor(parent, parent.getSessionId()), `${SIDE_QUESTION_DIR_PREFIX}${btwId}`);
		// The transcript inherits the parent's cwd so git context and workspace
		// attribution match the session it belongs to.
		const created = createSessionManager(parent.getCwd(), sessionDir);
		// rlmDepth is explicit: omitting it derives parent depth + 1, which would
		// label the transcript an RLM child. A side question is not a subagent
		// run, and consumers keyed on rlmDepth (Agents View rows, child-usage
		// reconciliation) must not treat it as one. The `parentSession` link is
		// what carries the lineage.
		created.newSession({ parentSession: parentFile, rlmDepth: 0 });
		created.appendModelChange(model.provider, model.id);
		const pointer: SideQuestionPointer = {
			btwId,
			sessionFile: created.getSessionFile() ?? "",
			sessionId: created.getSessionId(),
			question,
			startedAt: new Date().toISOString(),
		};
		// Rollback variant: a plain append indexes the entry before persisting it,
		// so a write failure would leave the parent's in-memory leaf pointing at an
		// entry that is not on disk. Every later main-session append would then
		// hang off a parentId that no reader can resolve, truncating the session on
		// reopen. Side-question storage must never be able to damage the parent.
		parent.appendCustomEntryWithRollback(SIDE_QUESTION_POINTER_TYPE, pointer);
		// The append alone is not durable on a brand-new parent: _persist drops
		// non-lifecycle entries until an assistant message exists, so a /btw that
		// settles before the parent's first response would leave the pointer in
		// memory only. The transcript itself is flushed by its own assistant
		// message, so an abrupt exit there orphans it and loses the sole link back
		// to the parent. flushNow bypasses that guard.
		parent.flushNow();
		manager = created;
		return created;
	};

	return {
		recordTurn(question: string, assistant: AssistantMessage, auxiliary?: readonly AssistantMessage[]): void {
			if (disabled) {
				return;
			}
			try {
				const target = ensureStarted(question);
				if (!target) {
					return;
				}
				target.appendMessage({
					role: "user",
					content: [{ type: "text", text: question }],
					timestamp: Date.now(),
				});
				// Compaction summaries and a context-overflow answer that was retried
				// are real completions with real usage. They precede the answer the
				// user kept, so they are written first and the transcript totals the
				// whole run rather than just its last call.
				for (const completion of auxiliary ?? []) {
					target.appendMessage(completion);
				}
				target.appendMessage(assistant);
			} catch (error) {
				disabled = true;
				options.onError?.(error);
			}
		},
		recordStatus(status: SideQuestionRecordStatus, errorMessage?: string): void {
			// Nothing was recorded, so there is no transcript to annotate.
			if (disabled || !manager) {
				return;
			}
			try {
				const record: SideQuestionStatusRecord = { status, ...(errorMessage ? { errorMessage } : {}) };
				manager.appendCustomEntry(SIDE_QUESTION_STATUS_TYPE, record);
			} catch (error) {
				disabled = true;
				options.onError?.(error);
			}
		},
	};
}

/**
 * Tracks one recorder per open `/btw` pane.
 *
 * Each turn arrives as its own side-question run, so pane identity has to be
 * reconstructed from the request: a pane's first question carries no prior
 * turns, and every follow-up carries the turns already answered in that pane.
 * Reusing the recorder keeps a multi-turn conversation in one transcript
 * instead of scattering it across one file per turn.
 */
export class SideQuestionPaneRegistry {
	private readonly panes = new Map<string, SideQuestionRecorder>();

	/**
	 * Dismissing a pane is a client-side action the daemon never observes, so
	 * entries are also evicted by age. Each one pins a SessionManager, and a
	 * long-lived daemon would otherwise accumulate one per side conversation
	 * for the lifetime of the session.
	 */
	constructor(private readonly maxPanes = 32) {}

	/**
	 * @param key Pane identity. A caller-supplied pane id when available,
	 *   otherwise a client/session scope shared by every pane on that session.
	 * @param reuseExisting Whether a recorder already held under `key` still
	 *   describes this pane. Always true for a real pane id; derived from
	 *   "the request carries prior turns" for scope keys.
	 * @param factory Builds a recorder when a new pane starts.
	 */
	resolve(key: string, reuseExisting: boolean, factory: () => SideQuestionRecorder): SideQuestionRecorder {
		const existing = reuseExisting ? this.panes.get(key) : undefined;
		if (existing) {
			return existing;
		}
		// A first question opens a fresh transcript, so a stale recorder from a
		// dismissed pane can never absorb the new conversation.
		const recorder = factory();
		this.panes.delete(key);
		this.panes.set(key, recorder);
		// Map iterates in insertion order, so the first key is the oldest pane.
		while (this.panes.size > this.maxPanes) {
			const oldest = this.panes.keys().next();
			if (oldest.done) {
				break;
			}
			this.panes.delete(oldest.value);
		}
		return recorder;
	}

	release(key: string): void {
		this.panes.delete(key);
	}

	/** Release every pane whose key starts with `prefix`, e.g. one session's. */
	releaseMatching(prefix: string): void {
		for (const key of [...this.panes.keys()]) {
			if (key.startsWith(prefix)) {
				this.panes.delete(key);
			}
		}
	}

	clear(): void {
		this.panes.clear();
	}
}

/** Generate a pane id for a new side conversation. */
export function createSideQuestionPaneId(): string {
	return randomUUID().slice(0, 8);
}
