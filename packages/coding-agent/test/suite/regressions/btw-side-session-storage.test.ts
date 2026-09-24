import { chmodSync, existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fauxAssistantMessage } from "@earendil-works/pi-ai";
import { describe, expect, it } from "vitest";
import {
	type CustomEntry,
	type SessionEntry,
	SessionManager,
	type SessionMessageEntry,
} from "../../../src/core/session-manager.js";
import { startSideQuestion } from "../../../src/core/side-question.js";
import {
	createSideQuestionStore,
	SIDE_QUESTION_POINTER_TYPE,
	SIDE_QUESTION_STATUS_TYPE,
	SideQuestionPaneRegistry,
	type SideQuestionPointer,
	type SideQuestionRecorder,
} from "../../../src/core/side-question-store.js";
import { createHarness } from "../harness.js";

function pointerEntries(entries: SessionEntry[]): CustomEntry<SideQuestionPointer>[] {
	return entries.filter(
		(entry): entry is CustomEntry<SideQuestionPointer> =>
			entry.type === "custom" && entry.customType === SIDE_QUESTION_POINTER_TYPE,
	);
}

function readTranscript(sessionFile: string): Record<string, unknown>[] {
	return readFileSync(sessionFile, "utf8")
		.split("\n")
		.filter((line) => line.trim().length > 0)
		.map((line) => JSON.parse(line) as Record<string, unknown>);
}

async function seededHarness() {
	const harness = await createHarness({ persistSession: true });
	harness.setResponses([fauxAssistantMessage("seed response")]);
	await harness.session.prompt("seed prompt");
	return harness;
}

describe("side question storage", () => {
	it("persists a side conversation as a linked sub-session without touching main context", async () => {
		const harness = await seededHarness();
		try {
			const parentFile = harness.sessionManager.getSessionFile();
			expect(parentFile).toBeDefined();
			const contextBefore = structuredClone(harness.session.messages);

			const recorder = createSideQuestionStore({
				parent: harness.sessionManager,
				model: harness.getModel(),
				btwId: "pane1",
			});

			harness.setResponses([fauxAssistantMessage("side answer")]);
			await startSideQuestion(harness.session.agent, "turn-1", "What is the codename?", () => {}, [], {
				getCompactionSettings: () => harness.settingsManager.getCompactionSettings(),
				getRequestAuth: (model) => harness.session.getRequestAuth(model),
				recorder,
			}).done;

			// The pointer is the only thing the parent gains, and `custom` entries
			// are ignored by buildSessionContext, so the main context is untouched.
			const pointers = pointerEntries(harness.sessionManager.getEntries());
			expect(pointers).toHaveLength(1);
			expect(harness.session.messages).toEqual(contextBefore);

			const pointer = pointers[0].data as SideQuestionPointer;
			expect(pointer.btwId).toBe("pane1");
			expect(pointer.question).toBe("What is the codename?");
			expect(existsSync(pointer.sessionFile)).toBe(true);
			expect(pointer.sessionFile).toContain(
				join("session-artifacts", harness.sessionManager.getSessionId(), "btw-pane1"),
			);

			const transcript = readTranscript(pointer.sessionFile);
			expect(transcript[0]).toMatchObject({ type: "session", parentSession: parentFile });

			const messages = transcript.filter((entry) => entry.type === "message");
			expect(messages).toHaveLength(2);
			expect((messages[0] as { message: { role: string } }).message.role).toBe("user");
			const assistant = (messages[1] as { message: { role: string; usage?: unknown } }).message;
			expect(assistant.role).toBe("assistant");
			// Usage is the whole point: an unrecorded side answer is unbilled spend.
			expect(assistant.usage).toBeDefined();
		} finally {
			harness.cleanup();
		}
	});

	it("never copies the replayed parent context into the transcript", async () => {
		const harness = await seededHarness();
		try {
			const recorder = createSideQuestionStore({
				parent: harness.sessionManager,
				model: harness.getModel(),
				btwId: "pane2",
			});

			harness.setResponses([fauxAssistantMessage("first"), fauxAssistantMessage("second")]);
			for (const [index, question] of ["first side?", "second side?"].entries()) {
				await startSideQuestion(
					harness.session.agent,
					`turn-${index}`,
					question,
					() => {},
					index === 0 ? [] : [{ question: "first side?", answer: "first" }],
					{
						getCompactionSettings: () => harness.settingsManager.getCompactionSettings(),
						getRequestAuth: (model) => harness.session.getRequestAuth(model),
						recorder,
					},
				).done;
			}

			const pointers = pointerEntries(harness.sessionManager.getEntries());
			// Two turns in one pane stay one transcript and one pointer.
			expect(pointers).toHaveLength(1);

			const transcript = readTranscript((pointers[0].data as SideQuestionPointer).sessionFile);
			const messages = transcript.filter((entry) => entry.type === "message");
			// Exactly two turns: the parent conversation the side run replays is
			// never copied forward, which would duplicate the whole session per ask.
			expect(messages).toHaveLength(4);
			const texts = messages.map((entry) => JSON.stringify((entry as { message: unknown }).message));
			expect(texts.some((text) => text.includes("seed prompt"))).toBe(false);
			expect(texts.some((text) => text.includes("seed response"))).toBe(false);
		} finally {
			harness.cleanup();
		}
	});

	it("records a terminal status when a turn fails", async () => {
		const harness = await seededHarness();
		try {
			const recorder = createSideQuestionStore({
				parent: harness.sessionManager,
				model: harness.getModel(),
				btwId: "pane3",
			});

			harness.setResponses([fauxAssistantMessage("answered")]);
			await startSideQuestion(harness.session.agent, "turn-ok", "ok?", () => {}, [], {
				getCompactionSettings: () => harness.settingsManager.getCompactionSettings(),
				getRequestAuth: (model) => harness.session.getRequestAuth(model),
				recorder,
			}).done;

			recorder.recordStatus("cancelled");

			const pointer = pointerEntries(harness.sessionManager.getEntries())[0].data as SideQuestionPointer;
			const statuses = readTranscript(pointer.sessionFile).filter(
				(entry) => entry.type === "custom" && entry.customType === SIDE_QUESTION_STATUS_TYPE,
			);
			expect(statuses).toHaveLength(1);
			expect((statuses[0] as { data: { status: string } }).data.status).toBe("cancelled");
		} finally {
			harness.cleanup();
		}
	});

	it("writes nothing when a pane is dismissed before any answer", async () => {
		const harness = await seededHarness();
		try {
			const entriesBefore = structuredClone(harness.sessionManager.getEntries());
			const recorder = createSideQuestionStore({
				parent: harness.sessionManager,
				model: harness.getModel(),
				btwId: "pane4",
			});

			recorder.recordStatus("cancelled");

			// A pane cancelled before its first answer leaves no empty transcript
			// and no pointer to one.
			expect(harness.sessionManager.getEntries()).toEqual(entriesBefore);
			expect(
				existsSync(join(harness.tempDir, "session-artifacts", harness.sessionManager.getSessionId(), "btw-pane4")),
			).toBe(false);
		} finally {
			harness.cleanup();
		}
	});

	it("keeps answering when storage fails", async () => {
		const harness = await seededHarness();
		try {
			const errors: unknown[] = [];
			const recorder = createSideQuestionStore({
				parent: harness.sessionManager,
				model: harness.getModel(),
				btwId: "pane5",
				createSessionManager: () => {
					throw new Error("disk full");
				},
				onError: (error) => {
					errors.push(error);
				},
			});

			harness.setResponses([fauxAssistantMessage("still answered")]);
			const events: string[] = [];
			await startSideQuestion(
				harness.session.agent,
				"turn-fail",
				"does storage break me?",
				(event) => {
					events.push(event.status);
				},
				[],
				{
					getCompactionSettings: () => harness.settingsManager.getCompactionSettings(),
					getRequestAuth: (model) => harness.session.getRequestAuth(model),
					recorder,
				},
			).done;

			expect(events.at(-1)).toBe("complete");
			expect(errors).toHaveLength(1);
			// The failure disables storage instead of retrying into a broken file.
			expect(pointerEntries(harness.sessionManager.getEntries())).toHaveLength(0);
		} finally {
			harness.cleanup();
		}
	});

	it("records a failed turn that still returned usage", async () => {
		const harness = await seededHarness();
		try {
			const recorder = createSideQuestionStore({
				parent: harness.sessionManager,
				model: harness.getModel(),
				btwId: "pane6",
			});

			const errored = fauxAssistantMessage("partial answer");
			harness.setResponses([{ ...errored, stopReason: "error", errorMessage: "provider exploded" }]);
			await startSideQuestion(harness.session.agent, "turn-err", "will this bill me?", () => {}, [], {
				getCompactionSettings: () => harness.settingsManager.getCompactionSettings(),
				getRequestAuth: (model) => harness.session.getRequestAuth(model),
				recorder,
			}).done;

			// A provider can bill for a turn it then fails; dropping it would make
			// that spend permanently invisible.
			const pointers = pointerEntries(harness.sessionManager.getEntries());
			expect(pointers).toHaveLength(1);
			const transcript = readTranscript((pointers[0].data as SideQuestionPointer).sessionFile);
			expect(transcript.filter((entry) => entry.type === "message")).toHaveLength(2);
			const statuses = transcript.filter(
				(entry) => entry.type === "custom" && entry.customType === SIDE_QUESTION_STATUS_TYPE,
			);
			expect((statuses[0] as { data: { status: string } }).data.status).toBe("error");
		} finally {
			harness.cleanup();
		}
	});

	it("does not label the transcript an RLM child", async () => {
		const harness = await seededHarness();
		try {
			const recorder = createSideQuestionStore({
				parent: harness.sessionManager,
				model: harness.getModel(),
				btwId: "pane7",
			});
			harness.setResponses([fauxAssistantMessage("answer")]);
			await startSideQuestion(harness.session.agent, "turn-depth", "depth?", () => {}, [], {
				getCompactionSettings: () => harness.settingsManager.getCompactionSettings(),
				getRequestAuth: (model) => harness.session.getRequestAuth(model),
				recorder,
			}).done;

			const pointer = pointerEntries(harness.sessionManager.getEntries())[0].data as SideQuestionPointer;
			const header = readTranscript(pointer.sessionFile)[0];
			// A side question is not a subagent run. Lineage is carried by
			// parentSession; a derived rlmDepth would misclassify it for every
			// consumer keyed on child depth.
			expect(header).toMatchObject({ rlmDepth: 0 });
		} finally {
			harness.cleanup();
		}
	});

	it("leaves the parent session intact when the pointer cannot be written", async () => {
		const harness = await seededHarness();
		try {
			const parentFile = harness.sessionManager.getSessionFile() as string;
			const entriesBefore = structuredClone(harness.sessionManager.getEntries());
			const errors: unknown[] = [];
			const recorder = createSideQuestionStore({
				parent: harness.sessionManager,
				model: harness.getModel(),
				btwId: "pane8",
				onError: (error) => {
					errors.push(error);
				},
			});

			harness.setResponses([fauxAssistantMessage("answer")]);
			chmodSync(parentFile, 0o400);
			try {
				await startSideQuestion(harness.session.agent, "turn-ro", "read only?", () => {}, [], {
					getCompactionSettings: () => harness.settingsManager.getCompactionSettings(),
					getRequestAuth: (model) => harness.session.getRequestAuth(model),
					recorder,
				}).done;
			} finally {
				chmodSync(parentFile, 0o600);
			}

			expect(errors).toHaveLength(1);
			// A rolled-back append must not leave the leaf pointing at an entry that
			// never reached disk: later main-session appends would hang off a
			// parentId no reader can resolve, truncating the session on reopen.
			expect(harness.sessionManager.getEntries()).toEqual(entriesBefore);
			harness.setResponses([fauxAssistantMessage("after")]);
			await harness.session.prompt("after storage failure");
			const reopened = SessionManager.open(parentFile);
			const texts = reopened
				.getBranch()
				.filter((entry): entry is SessionMessageEntry => entry.type === "message")
				.map((entry) => JSON.stringify(entry.message));
			expect(texts.some((text) => text.includes("seed prompt"))).toBe(true);
			expect(texts.some((text) => text.includes("after storage failure"))).toBe(true);
		} finally {
			harness.cleanup();
		}
	});
});

describe("side question pane registry", () => {
	const stub = (): SideQuestionRecorder => ({ recordTurn: () => {}, recordStatus: () => {} });

	it("keeps one recorder per pane id across a reconnect", () => {
		const registry = new SideQuestionPaneRegistry();
		const pane = registry.resolve("session:pane-a", true, stub);
		// A reconnect re-sends the same pane id on a different socket; the pane the
		// user is still looking at must stay one transcript.
		expect(registry.resolve("session:pane-a", true, stub)).toBe(pane);
		expect(registry.resolve("session:pane-b", true, stub)).not.toBe(pane);
	});

	it("opens a new recorder for a scope key without prior turns", () => {
		const registry = new SideQuestionPaneRegistry();
		const first = registry.resolve("client:session", false, stub);
		expect(registry.resolve("client:session", true, stub)).toBe(first);
		expect(registry.resolve("client:session", false, stub)).not.toBe(first);
	});

	it("releases panes by session prefix and evicts the oldest beyond the cap", () => {
		const registry = new SideQuestionPaneRegistry(2);
		const first = registry.resolve("s1:a", true, stub);
		registry.resolve("s1:b", true, stub);
		registry.resolve("s2:c", true, stub);
		// Dismissing a pane is client-side and never observed here, so the oldest
		// entry is evicted rather than pinning its SessionManager forever.
		expect(registry.resolve("s1:a", true, stub)).not.toBe(first);

		registry.releaseMatching("s1:");
		const reopened = registry.resolve("s1:a", true, stub);
		expect(reopened).not.toBe(first);
	});
});
