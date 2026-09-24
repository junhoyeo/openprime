import type { AgentEvent } from "@earendil-works/pi-agent-core";
import { type AssistantMessage, fauxAssistantMessage } from "@earendil-works/pi-ai";
import { afterEach, describe, expect, it } from "vitest";
import { createHarness, getAssistantTexts, type Harness } from "../harness.js";

function provider401Message(): AssistantMessage {
	return {
		...fauxAssistantMessage("", {
			stopReason: "error",
			errorMessage: "401 Unauthorized: invalid API key",
		}),
		diagnostics: [
			{
				type: "provider_stream_failure",
				timestamp: Date.now(),
				details: { kind: "auth", status: 401 },
			},
		],
	};
}

function bareProvider401Message(): AssistantMessage {
	return fauxAssistantMessage("", {
		stopReason: "error",
		errorMessage: "401 status code (no body)",
	});
}

function provider500Message(): AssistantMessage {
	return {
		...fauxAssistantMessage("", {
			stopReason: "error",
			errorMessage: "500 Internal Server Error",
		}),
		diagnostics: [
			{
				type: "provider_stream_failure",
				timestamp: Date.now(),
				details: { kind: "server_error", status: 500 },
			},
		],
	};
}

describe("provider authentication failures preserve configured credentials", () => {
	const harnesses: Harness[] = [];

	afterEach(() => {
		while (harnesses.length > 0) {
			harnesses.pop()?.cleanup();
		}
	});

	it("retries structured provider auth failures once, then preserves credentials for the next prompt", async () => {
		const harness = await createHarness({
			settings: { retry: { enabled: true, maxRetries: 2, baseDelayMs: 1 } },
		});
		harnesses.push(harness);
		harness.setResponses([provider401Message(), provider401Message(), provider401Message()]);

		await harness.session.prompt("hello");

		expect(harness.faux.state.callCount).toBe(2);
		expect(harness.eventsOfType("auto_retry_start").map((event) => event.attempt)).toEqual([1]);
		expect(harness.eventsOfType("auto_retry_end").map((event) => event.success)).toEqual([false]);
		expect(harness.eventsOfType("auth_stale")).toHaveLength(0);

		const provider = harness.getModel().provider;
		expect(harness.authStorage.hasAuth(provider)).toBe(true);
		expect(harness.authStorage.getAuthStatus(provider)).not.toMatchObject({ source: "stale" });
		await expect(harness.authStorage.getApiKey(provider)).resolves.toBe("faux-key");

		const assistantMessages = harness.session.messages.filter(
			(message): message is AssistantMessage => message.role === "assistant",
		);
		const finalAssistant = assistantMessages[assistantMessages.length - 1];
		expect(finalAssistant?.errorMessage).toContain("401 Unauthorized");
		expect(finalAssistant?.errorMessage).toContain("Run /login to update credentials.");
	});

	it("does not emit stale auth source tokens after bare 401 auth failures", async () => {
		const harness = await createHarness({
			provider: "prime-inference",
			settings: { retry: { enabled: true, maxRetries: 0, baseDelayMs: 1 } },
		});
		harnesses.push(harness);
		harness.setResponses([bareProvider401Message()]);

		await harness.session.prompt("hello");

		expect(harness.eventsOfType("auth_stale")).toHaveLength(0);
		expect(harness.authStorage.getAuthStatus("prime-inference")).not.toMatchObject({ source: "stale" });
	});

	it("classifies bare status-code auth failures before login guidance is appended", async () => {
		const harness = await createHarness({
			provider: "prime-inference",
			settings: { retry: { enabled: true, maxRetries: 1, baseDelayMs: 1 } },
		});
		harnesses.push(harness);
		const message = bareProvider401Message();
		const event = { type: "agent_end", messages: [message] } as AgentEvent;
		const session = harness.session as unknown as {
			_createRetryPromiseForAgentEnd(event: AgentEvent): void;
		};

		session._createRetryPromiseForAgentEnd(event);

		expect(harness.session.isRetrying).toBe(true);
		harness.session.abortRetry();
	});

	it("creates retry promises for exhausted structured auth failures so cleanup is awaited", async () => {
		const harness = await createHarness({
			settings: { retry: { enabled: true, maxRetries: 2, baseDelayMs: 1 } },
		});
		harnesses.push(harness);
		const event = { type: "agent_end", messages: [provider401Message()] } as AgentEvent;
		const session = harness.session as unknown as {
			_retryAttempt: number;
			_createRetryPromiseForAgentEnd(event: AgentEvent): void;
		};
		session._retryAttempt = 1;

		session._createRetryPromiseForAgentEnd(event);

		expect(harness.session.isRetrying).toBe(true);
		harness.session.abortRetry();
	});

	it("preserves credentials when retry backoff is cancelled", async () => {
		const harness = await createHarness({
			settings: { retry: { enabled: true, maxRetries: 3, baseDelayMs: 100 } },
		});
		harnesses.push(harness);
		harness.setResponses([provider401Message(), provider401Message()]);

		const sawRetryStart = new Promise<void>((resolve) => {
			const unsubscribe = harness.session.subscribe((event) => {
				if (event.type === "auto_retry_start") {
					unsubscribe();
					resolve();
				}
			});
		});

		const promptPromise = harness.session.prompt("hello");
		await sawRetryStart;
		harness.session.abortRetry();
		await promptPromise;

		expect(harness.faux.state.callCount).toBe(1);
		expect(harness.eventsOfType("auth_stale")).toHaveLength(0);
		expect(harness.eventsOfType("auto_retry_end").map((event) => event.finalError)).toContain("Retry cancelled");
		expect(harness.authStorage.hasAuth(harness.getModel().provider)).toBe(true);
		await expect(harness.authStorage.getApiKey(harness.getModel().provider)).resolves.toBe("faux-key");
	});

	it("allows the next prompt to succeed when the gateway recovers", async () => {
		const harness = await createHarness({
			settings: { retry: { enabled: true, maxRetries: 1, baseDelayMs: 5 } },
		});
		harnesses.push(harness);
		harness.setResponses([provider401Message(), provider401Message(), fauxAssistantMessage("gateway recovered")]);

		await harness.session.prompt("hello");
		await harness.session.prompt("retry after recovery");

		expect(harness.faux.state.callCount).toBe(3);
		expect(harness.eventsOfType("auth_stale")).toHaveLength(0);
		await expect(harness.authStorage.getApiKey(harness.getModel().provider)).resolves.toBe("faux-key");
		expect(getAssistantTexts(harness)).toContain("gateway recovered");
	});

	it("preserves credentials when a later retry failure is not authentication-related", async () => {
		const harness = await createHarness({
			settings: { retry: { enabled: true, maxRetries: 2, baseDelayMs: 1 } },
		});
		harnesses.push(harness);
		harness.setResponses([provider401Message(), provider500Message(), provider500Message()]);

		await harness.session.prompt("hello");

		expect(harness.faux.state.callCount).toBe(3);
		expect(harness.eventsOfType("auto_retry_start").map((event) => event.attempt)).toEqual([1, 2]);
		expect(harness.eventsOfType("auto_retry_end").map((event) => event.success)).toEqual([false]);
		expect(harness.authStorage.hasAuth(harness.getModel().provider)).toBe(true);
		await expect(harness.authStorage.getApiKey(harness.getModel().provider)).resolves.toBe("faux-key");

		const assistantMessages = harness.session.messages.filter(
			(message): message is AssistantMessage => message.role === "assistant",
		);
		const finalAssistant = assistantMessages[assistantMessages.length - 1];
		expect(finalAssistant?.errorMessage).toContain("500 Internal Server Error");
		expect(finalAssistant?.errorMessage).not.toContain("Run /login to update credentials.");
	});

	it("preserves credentials when retry is disabled", async () => {
		const harness = await createHarness({
			settings: { retry: { enabled: false } },
		});
		harnesses.push(harness);
		harness.setResponses([provider401Message()]);

		await harness.session.prompt("hello");

		expect(harness.faux.state.callCount).toBe(1);
		expect(harness.eventsOfType("auto_retry_start")).toHaveLength(0);
		expect(harness.eventsOfType("auth_stale")).toHaveLength(0);
		expect(harness.authStorage.hasAuth(harness.getModel().provider)).toBe(true);
		await expect(harness.authStorage.getApiKey(harness.getModel().provider)).resolves.toBe("faux-key");
	});

	it("resolves retry state for auth failures surfaced only on agent_end", async () => {
		const harness = await createHarness({
			settings: { retry: { enabled: true, maxRetries: 0, baseDelayMs: 1 } },
		});
		harnesses.push(harness);
		const message = provider401Message();
		const event = { type: "agent_end", messages: [message] } as AgentEvent;
		const session = harness.session as unknown as {
			_createRetryPromiseForAgentEnd(event: AgentEvent): void;
			_processAgentEvent(event: AgentEvent): Promise<void>;
		};

		session._createRetryPromiseForAgentEnd(event);
		await session._processAgentEvent(event);

		expect(harness.session.isRetrying).toBe(false);
		expect(harness.eventsOfType("auto_retry_end").map((retryEvent) => retryEvent.success)).toEqual([false]);
		expect(harness.eventsOfType("auth_stale")).toHaveLength(0);
		expect(harness.authStorage.hasAuth(harness.getModel().provider)).toBe(true);
		expect(message.errorMessage).toContain("Run /login to update credentials.");
	});
});
