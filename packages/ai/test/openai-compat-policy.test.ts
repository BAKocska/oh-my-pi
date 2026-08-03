import { describe, expect, it } from "bun:test";
import type { ResponseCreateParamsStreaming } from "@oh-my-pi/pi-ai/providers/openai-responses-wire";
import {
	applyChatCompletionsCompatPolicy,
	applyResponsesCompatPolicy,
	type OpenAICompletionsParams,
	resolveOpenAICompatPolicy,
} from "@oh-my-pi/pi-ai/providers/openai-shared";
import type { Model, ModelSpec, OpenAICompat } from "@oh-my-pi/pi-ai/types";
import { buildModel } from "@oh-my-pi/pi-catalog/build";
import { Effort } from "@oh-my-pi/pi-catalog/effort";
import { getBundledModel } from "@oh-my-pi/pi-catalog/models";

function chatModel(compat: OpenAICompat): Model<"openai-completions"> {
	return buildModel({
		id: "compat-reasoner",
		name: "Compat Reasoner",
		api: "openai-completions",
		provider: "test-provider",
		baseUrl: "https://example.com/v1",
		reasoning: true,
		input: ["text"],
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
		contextWindow: 128_000,
		maxTokens: 4096,
		compat,
	} satisfies ModelSpec<"openai-completions">);
}

function responsesModel(compat: OpenAICompat): Model<"openai-responses"> {
	return buildModel({
		id: "compat-reasoner",
		name: "Compat Reasoner",
		api: "openai-responses",
		provider: "test-provider",
		baseUrl: "https://example.com/v1",
		reasoning: true,
		input: ["text"],
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
		contextWindow: 128_000,
		maxTokens: 4096,
		compat,
	} satisfies ModelSpec<"openai-responses">);
}

function chatParams(): OpenAICompletionsParams {
	return { model: "compat-reasoner", messages: [], stream: true };
}

function responsesParams(): ResponseCreateParamsStreaming {
	return { model: "compat-reasoner", input: [], stream: true };
}

describe("OpenAI compat policy", () => {
	it("suppresses reasoning on forced tool choice for both endpoints", () => {
		const compat: OpenAICompat = {
			disableReasoningOnForcedToolChoice: true,
			thinkingFormat: "openrouter",
			reasoningDisableMode: "openrouter-enabled-false",
		};
		const toolChoice = { type: "function", name: "search" };
		const chatPolicy = resolveOpenAICompatPolicy(chatModel(compat), {
			endpoint: "chat-completions",
			reasoning: Effort.High,
			toolChoice,
		});
		const responsesPolicy = resolveOpenAICompatPolicy(responsesModel(compat), {
			endpoint: "responses",
			reasoning: Effort.High,
			toolChoice,
		});

		expect(chatPolicy.reasoning.enabled).toBe(false);
		expect(responsesPolicy.reasoning.enabled).toBe(false);
		expect(chatPolicy.reasoning.disableReason).toBe("forced-tool-choice");
		expect(responsesPolicy.reasoning.disableReason).toBe("forced-tool-choice");
	});

	it("encodes OpenRouter disabled reasoning through both wire adapters", () => {
		const compat: OpenAICompat = {
			thinkingFormat: "openrouter",
			reasoningDisableMode: "openrouter-enabled-false",
		};
		const chatBody = chatParams();
		const responseBody = responsesParams();

		applyChatCompletionsCompatPolicy(
			chatBody,
			resolveOpenAICompatPolicy(chatModel(compat), {
				endpoint: "chat-completions",
				disableReasoning: true,
			}),
		);
		applyResponsesCompatPolicy(
			responseBody,
			resolveOpenAICompatPolicy(responsesModel(compat), { endpoint: "responses", disableReasoning: true }),
			undefined,
		);

		expect(chatBody.reasoning).toEqual({ enabled: false });
		expect(responseBody.reasoning as unknown).toEqual({ enabled: false });
	});

	it("omits effort for both wire adapters from one catalog flag", () => {
		const compat: OpenAICompat = { omitReasoningEffort: true };
		const chatBody = chatParams();
		const responseBody = responsesParams();

		applyChatCompletionsCompatPolicy(
			chatBody,
			resolveOpenAICompatPolicy(chatModel(compat), { endpoint: "chat-completions", reasoning: Effort.High }),
		);
		applyResponsesCompatPolicy(
			responseBody,
			resolveOpenAICompatPolicy(responsesModel(compat), { endpoint: "responses", reasoning: Effort.High }),
			undefined,
		);

		expect(chatBody.reasoning_effort).toBeUndefined();
		expect(responseBody.reasoning).toBeUndefined();
	});

	it("leaves Responses input unchanged when reasoning is not requested", () => {
		const responseBody = responsesParams();

		applyResponsesCompatPolicy(
			responseBody,
			resolveOpenAICompatPolicy(responsesModel({}), { endpoint: "responses" }),
			undefined,
		);

		expect(responseBody.reasoning).toBeUndefined();
		expect(responseBody.input).toEqual([]);
	});

	it("exposes reasoning replay constraints independent of endpoint", () => {
		const compat: OpenAICompat = {
			requiresReasoningContentForToolCalls: true,
			requiresReasoningContentForAllAssistantTurns: true,
			allowsSyntheticReasoningContentForToolCalls: false,
			reasoningContentField: "reasoning_content",
		};
		const chatPolicy = resolveOpenAICompatPolicy(chatModel(compat), { endpoint: "chat-completions" });
		const responsesPolicy = resolveOpenAICompatPolicy(responsesModel(compat), { endpoint: "responses" });

		expect(chatPolicy.reasoning.requiresReasoningContentForToolCalls).toBe(true);
		expect(responsesPolicy.reasoning.requiresReasoningContentForToolCalls).toBe(true);
		expect(chatPolicy.reasoning.requiresReasoningContentForAllAssistantTurns).toBe(true);
		expect(responsesPolicy.reasoning.requiresReasoningContentForAllAssistantTurns).toBe(true);
		expect(chatPolicy.reasoning.allowsSyntheticReasoningContentForToolCalls).toBe(false);
		expect(responsesPolicy.reasoning.allowsSyntheticReasoningContentForToolCalls).toBe(false);
	});

	it("exposes tool id and cumulative reasoning stream constraints for both endpoints", () => {
		const compat: OpenAICompat = { requiresMistralToolIds: true, reasoningDeltasMayBeCumulative: true };
		const chatPolicy = resolveOpenAICompatPolicy(chatModel(compat), { endpoint: "chat-completions" });
		const responsesPolicy = resolveOpenAICompatPolicy(responsesModel(compat), { endpoint: "responses" });

		expect(chatPolicy.tools.toolCallIdKind).toBe("mistral-9-alnum");
		expect(responsesPolicy.tools.toolCallIdKind).toBe("mistral-9-alnum");
		expect(chatPolicy.stream.reasoningDeltasMayBeCumulative).toBe(true);
		expect(responsesPolicy.stream.reasoningDeltasMayBeCumulative).toBe(true);
	});

	it("routes the reasoning_effort ladder for the bundled Token Plan qwen3.8-max", () => {
		// qwen3.8-max drives reasoning through the OpenAI-standard
		// `reasoning_effort` control, not the legacy `enable_thinking` toggle the
		// older Qwen3.x Token Plan builds ride. Build from the real bundled entry
		// so the dialect classification — not a hardcoded override — is exercised.
		const model = getBundledModel<"openai-completions">("alibaba-token-plan", "qwen3.8-max");
		for (const [effort, wire] of [
			[Effort.Low, "low"],
			[Effort.Medium, "medium"],
			[Effort.XHigh, "xhigh"],
		] as const) {
			const params = chatParams();
			applyChatCompletionsCompatPolicy(
				params,
				resolveOpenAICompatPolicy(model, { endpoint: "chat-completions", reasoning: effort }),
			);
			expect(params.reasoning_effort).toBe(wire);
			expect(params.enable_thinking).toBeUndefined();
			expect(params.chat_template_kwargs).toBeUndefined();
		}

		// Disabling reasoning clamps to the lowest supported effort rather than
		// emitting the legacy binary toggle.
		const disabled = chatParams();
		applyChatCompletionsCompatPolicy(
			disabled,
			resolveOpenAICompatPolicy(model, { endpoint: "chat-completions", disableReasoning: true }),
		);
		expect(disabled.reasoning_effort).toBe("low");
		expect(disabled.enable_thinking).toBeUndefined();
	});
});
