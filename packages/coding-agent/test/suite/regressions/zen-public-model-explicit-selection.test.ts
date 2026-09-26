import type { Api, Model } from "@earendil-works/pi-ai";
import { fauxAssistantMessage } from "@earendil-works/pi-ai";
import { afterEach, describe, expect, it } from "vitest";
import { createHarness, type Harness } from "../harness.js";

const ZEN_PROVIDER = "opencode";
const ZEN_MODEL_ID = "union-alpha";

interface RlmModelResolver {
	_resolveRlmSubagentModel(reference: string | undefined, target?: string): Promise<{ model: Model<Api> }>;
}

describe("zero-cost OpenCode Zen models are explicitly selectable without a credential", () => {
	const harnesses: Harness[] = [];

	afterEach(() => {
		while (harnesses.length > 0) harnesses.pop()?.cleanup();
	});

	async function setup() {
		const harness = await createHarness({ provider: "faux-zen-public", models: [{ id: "parent-model" }] });
		harnesses.push(harness);
		const registry = harness.session.modelRegistry;
		const zenModel = registry.find(ZEN_PROVIDER, ZEN_MODEL_ID);
		expect(zenModel).toBeDefined();
		// The fixture must reproduce the reported situation: no OpenCode credential.
		expect(registry.hasConfiguredAuth(zenModel!)).toBe(false);
		return { harness, registry, zenModel: zenModel! };
	}

	it("setModel commits the switch and the run resolves the public key", async () => {
		const { harness, registry, zenModel } = await setup();

		await harness.session.setModel(zenModel);

		expect(harness.session.model).toMatchObject({ provider: ZEN_PROVIDER, id: ZEN_MODEL_ID });
		await expect(harness.session.getRequestAuth(zenModel)).resolves.toMatchObject({ apiKey: "public" });
		// The run gate that produced "No API key found for opencode." now passes.
		await expect(
			(harness.session as unknown as { _validateCanStartAgentRun(): Promise<void> })._validateCanStartAgentRun(),
		).resolves.toBeUndefined();
		// Narrowing is unchanged: discovery still omits the model.
		expect(registry.hasConfiguredAuth(zenModel)).toBe(false);
		expect(
			registry.getAvailable().some((model) => model.provider === ZEN_PROVIDER && model.id === ZEN_MODEL_ID),
		).toBe(false);
	});

	it("rejects a keyless model that has no public fallback", async () => {
		const { harness, registry } = await setup();
		const paidZenModel = registry.getAll().find((model) => model.provider === ZEN_PROVIDER && model.cost.input > 0);
		expect(paidZenModel).toBeDefined();

		await expect(harness.session.setModel(paidZenModel!)).rejects.toThrow(
			`No API key for ${paidZenModel!.provider}/${paidZenModel!.id}`,
		);
		expect(harness.session.model?.provider).toBe("faux-zen-public");
	});

	it("keeps the free Zen model out of cycling and subagent discovery while an explicit spawn resolves it", async () => {
		const { harness, registry, zenModel } = await setup();
		const selector = `${ZEN_PROVIDER}/${ZEN_MODEL_ID}`;

		const discovered = await harness.session.findRlmModels(ZEN_MODEL_ID, 8);
		expect(discovered.models.map((model) => model.selector)).not.toContain(selector);

		const resolved = await (harness.session as unknown as RlmModelResolver)._resolveRlmSubagentModel(selector);
		expect(resolved.model).toMatchObject({ provider: ZEN_PROVIDER, id: ZEN_MODEL_ID });

		// Cycling stays on the configured faux provider instead of landing on the Zen model.
		await harness.session.setModel(zenModel);
		harness.setResponses([fauxAssistantMessage("cycled")]);
		const cycled = await harness.session.cycleModel("forward");
		expect(cycled?.model).not.toMatchObject({ provider: ZEN_PROVIDER, id: ZEN_MODEL_ID });
		expect(
			registry.getAvailable().some((model) => model.provider === ZEN_PROVIDER && model.id === ZEN_MODEL_ID),
		).toBe(false);
	});
});
