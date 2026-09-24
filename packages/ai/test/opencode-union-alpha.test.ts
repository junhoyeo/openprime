import { describe, expect, it } from "vitest";
import { getModel, isOpencodePublicModel, opencodePublicApiKey } from "../src/models.js";
import {
	applyOpencodeZenHeaders,
	OPENCODE_PUBLIC_API_KEY,
	OPENCODE_USER_AGENT,
} from "../src/utils/opencode-headers.js";

describe("opencode union-alpha", () => {
	it("is in the catalog as a zero-cost Anthropic-shaped Zen model", () => {
		const model = getModel("opencode", "union-alpha");
		expect(model).toBeDefined();
		expect(model.id).toBe("union-alpha");
		expect(model.provider).toBe("opencode");
		expect(model.api).toBe("anthropic-messages");
		expect(model.baseUrl).toBe("https://opencode.ai/zen");
		expect(model.cost.input).toBe(0);
		expect(isOpencodePublicModel(model)).toBe(true);
		expect(opencodePublicApiKey(model)).toBe(OPENCODE_PUBLIC_API_KEY);
		expect(model.headers?.["User-Agent"]).toBe(OPENCODE_USER_AGENT);
		expect(model.headers?.["x-opencode-client"]).toBe("cli");
	});

	it("fills official Zen client headers", () => {
		const headers = applyOpencodeZenHeaders();
		expect(headers["User-Agent"]).toBe(OPENCODE_USER_AGENT);
		expect(headers["x-opencode-client"]).toBe("cli");
		expect(headers["x-opencode-session"]).toMatch(/^ses_[0-9a-f]{26}$/);
		expect(headers["x-opencode-request"]).toMatch(/^msg_[0-9a-f]{26}$/);
	});
});
