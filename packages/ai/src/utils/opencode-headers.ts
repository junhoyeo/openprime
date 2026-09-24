/** Current OpenCode CLI release. Zen's free-tier gate matches this User-Agent prefix. */
export const OPENCODE_USER_AGENT = "opencode/1.18.31";

/** OpenCode's own no-credential key for zero-cost Zen models. */
export const OPENCODE_PUBLIC_API_KEY = "public";

const PROCESS_SESSION_ID = zenId("ses");

export function isOpencodePublicModel(model: { provider: string; cost: { input: number } }): boolean {
	return (model.provider === "opencode" || model.provider === "opencode-go") && model.cost.input === 0;
}

export function opencodePublicApiKey(
	model: { provider: string; cost: { input: number } },
	apiKey?: string,
): string | undefined {
	if (apiKey) return apiKey;
	return isOpencodePublicModel(model) ? OPENCODE_PUBLIC_API_KEY : undefined;
}

export function applyOpencodeZenHeaders(headers: Record<string, string> = {}): Record<string, string> {
	const next = { ...headers };
	if (!next["User-Agent"]?.startsWith("opencode/")) {
		next["User-Agent"] = OPENCODE_USER_AGENT;
	}
	if (!next["x-opencode-client"] || next["x-opencode-client"] === "ai-gateway") {
		next["x-opencode-client"] = "cli";
	}
	if (!next["x-opencode-session"]) {
		next["x-opencode-session"] = PROCESS_SESSION_ID;
	}
	if (!next["x-opencode-request"]) {
		next["x-opencode-request"] = zenId("msg");
	}
	return next;
}

function zenId(prefix: "ses" | "msg"): string {
	const digest = randomHex(16) + Date.now().toString(16);
	return `${prefix}_${digest.slice(0, 26)}`;
}

/**
 * Web Crypto only - this module is bundled for the browser by
 * `npm run check:browser-smoke`, so `node:crypto` must not be imported.
 */
function randomHex(byteLength: number): string {
	const bytes = new Uint8Array(byteLength);
	const webCrypto = globalThis.crypto;
	if (typeof webCrypto?.getRandomValues === "function") {
		webCrypto.getRandomValues(bytes);
	} else {
		for (let i = 0; i < byteLength; i += 1) bytes[i] = Math.floor(Math.random() * 256);
	}
	let out = "";
	for (const byte of bytes) out += byte.toString(16).padStart(2, "0");
	return out;
}
