/** Current OpenCode CLI release. Zen's free-tier gate matches this User-Agent prefix. */
export const OPENCODE_USER_AGENT = "opencode/1.18.31";

/** OpenCode's own no-credential key for zero-cost Zen models. */
export const OPENCODE_PUBLIC_API_KEY = "public";

export function isOpencodePublicModel(model: { provider: string; cost: { input: number } }): boolean {
	return (model.provider === "opencode" || model.provider === "opencode-go") && model.cost.input === 0;
}

/**
 * Resolves the key a zero-cost Zen model is called with: a configured key wins,
 * otherwise OpenCode's public key. Session/request identity headers are the
 * providers' `withOpenCodeHeaders` concern; the CLI identity the free tier
 * gates on travels in the catalog entry's `headers`.
 */
export function opencodePublicApiKey(
	model: { provider: string; cost: { input: number } },
	apiKey?: string,
): string | undefined {
	if (apiKey) return apiKey;
	return isOpencodePublicModel(model) ? OPENCODE_PUBLIC_API_KEY : undefined;
}
