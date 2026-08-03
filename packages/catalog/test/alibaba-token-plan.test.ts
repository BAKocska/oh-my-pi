import { describe, expect, test } from "bun:test";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";
import { Effort } from "@oh-my-pi/pi-catalog/effort";
import { resolveProviderModels } from "@oh-my-pi/pi-catalog/model-manager";
import { CATALOG_PROVIDERS } from "@oh-my-pi/pi-catalog/provider-models/descriptors";
import {
	ALIBABA_TOKEN_PLAN_BASE_URL,
	ALIBABA_TOKEN_PLAN_STATIC_MODELS,
	alibabaTokenPlanModelManagerOptions,
} from "@oh-my-pi/pi-catalog/provider-models/openai-compat";
import type { FetchImpl, ModelSpec } from "@oh-my-pi/pi-catalog/types";
import { serializeAlibabaTokenPlanCredential } from "@oh-my-pi/pi-catalog/wire/alibaba-token-plan";

describe("QwenCloud Token Plan provider", () => {
	test("ships curated metadata for every advertised chat model", () => {
		expect(
			ALIBABA_TOKEN_PLAN_STATIC_MODELS.map(({ id, contextWindow, maxTokens }) => ({
				id,
				contextWindow,
				maxTokens,
			})),
		).toEqual([
			{ id: "qwen3.6-plus", contextWindow: 1_000_000, maxTokens: 65_536 },
			{ id: "qwen3.6-flash", contextWindow: 1_000_000, maxTokens: 65_536 },
			{ id: "qwen3.7-max", contextWindow: 1_000_000, maxTokens: 131_072 },
			{ id: "qwen3.7-plus", contextWindow: 1_000_000, maxTokens: 65_536 },
			{ id: "qwen3.8-max-preview", contextWindow: 1_000_000, maxTokens: 131_072 },
			{ id: "qwen3.8-max", contextWindow: 1_000_000, maxTokens: 131_072 },
			{ id: "deepseek-v4-pro", contextWindow: 1_000_000, maxTokens: 384_000 },
			{ id: "deepseek-v4-flash", contextWindow: 1_000_000, maxTokens: 384_000 },
			{ id: "deepseek-v4-flash-0731", contextWindow: 1_000_000, maxTokens: 384_000 },
			{ id: "deepseek-v3.2", contextWindow: 131_072, maxTokens: 65_536 },
			{ id: "glm-5.2", contextWindow: 1_000_000, maxTokens: 131_072 },
			{ id: "glm-5.1", contextWindow: 202_752, maxTokens: 128_000 },
			{ id: "glm-5", contextWindow: 202_752, maxTokens: 16_384 },
			{ id: "kimi-k2.7-code", contextWindow: 262_144, maxTokens: 262_144 },
			{ id: "kimi-k2.6", contextWindow: 262_144, maxTokens: 262_144 },
			{ id: "kimi-k2.5", contextWindow: 262_144, maxTokens: 98_304 },
			{ id: "MiniMax-M2.5", contextWindow: 196_608, maxTokens: 32_768 },
		]);

		const preview = ALIBABA_TOKEN_PLAN_STATIC_MODELS.find(model => model.id === "qwen3.8-max-preview");
		expect(preview).toMatchObject({
			provider: "alibaba-token-plan",
			baseUrl: ALIBABA_TOKEN_PLAN_BASE_URL,
			input: ["text", "image"],
			thinking: {
				efforts: [Effort.Low, Effort.High, Effort.XHigh],
				requiresEffort: true,
			},
			compat: {
				supportsDeveloperRole: false,
				supportsReasoningEffort: true,
			},
		});

		expect(ALIBABA_TOKEN_PLAN_STATIC_MODELS.find(model => model.id === "glm-5.2")?.thinking?.efforts).toEqual([
			Effort.Minimal,
			Effort.Low,
			Effort.Medium,
			Effort.High,
			Effort.Max,
		]);
	});

	test("discovers subscribed chat models from the native models endpoint", async () => {
		let requestedUrl = "";
		let authorization = "";
		const fetchMock: FetchImpl = (input, init) => {
			requestedUrl = String(input);
			authorization = new Headers(init?.headers).get("Authorization") ?? "";
			return Promise.resolve(
				Response.json({
					data: [
						{
							id: "qwen3.7-plus",
							name: "server metadata must not replace curated metadata",
							owned_by: "qwencloud",
							context_length: 262_144,
							max_completion_tokens: 16_384,
						},
						{ id: "deepseek-v4-flash", owned_by: "qwencloud" },
						{ id: "deepseek-v4-flash-0731", owned_by: "qwencloud" },
						{ id: "kimi-k2.7-code", owned_by: "qwencloud" },
						{ id: "MiniMax-M2.5", owned_by: "qwencloud" },
						{ id: "fun-asr", owned_by: "qwencloud" },
						{ id: "qwen-image-2.0-pro", owned_by: "qwencloud" },
						{ id: "qwen-audio-3.0-tts-plus", owned_by: "qwencloud" },
						{ id: "happyhorse-1.1-t2v", owned_by: "qwencloud" },
						{ id: "text-embedding-v4", owned_by: "qwencloud" },
						{ id: "wan2.7-image", owned_by: "qwencloud" },
					],
				}),
			);
		};

		const apiKey = `  ${serializeAlibabaTokenPlanCredential("sk-sp-test", "session_id=test")}  `;
		const options = alibabaTokenPlanModelManagerOptions({ apiKey, fetch: fetchMock });
		const models = await options.fetchDynamicModels?.();

		expect(requestedUrl).toBe(`${ALIBABA_TOKEN_PLAN_BASE_URL}/models`);
		expect(authorization).toBe("Bearer sk-sp-test");
		expect(models?.map(model => model.id)).toEqual([
			"deepseek-v4-flash",
			"deepseek-v4-flash-0731",
			"kimi-k2.7-code",
			"MiniMax-M2.5",
			"qwen3.7-plus",
		]);
		expect(
			Object.fromEntries(
				models?.map(model => [model.id, { contextWindow: model.contextWindow, maxTokens: model.maxTokens }]) ?? [],
			),
		).toEqual({
			"deepseek-v4-flash": { contextWindow: 1_000_000, maxTokens: 384_000 },
			"deepseek-v4-flash-0731": { contextWindow: 1_000_000, maxTokens: 384_000 },
			"kimi-k2.7-code": { contextWindow: 262_144, maxTokens: 262_144 },
			"MiniMax-M2.5": { contextWindow: 196_608, maxTokens: 32_768 },
			"qwen3.7-plus": { contextWindow: 1_000_000, maxTokens: 65_536 },
		});
		expect(options.dynamicModelsAuthoritative).toBe(true);
	});

	test("routes discovery to the credential's region when it is China (Beijing)", async () => {
		let requestedUrl = "";
		const fetchMock: FetchImpl = input => {
			requestedUrl = String(input);
			return Promise.resolve(Response.json({ data: [{ id: "qwen3.7-plus", owned_by: "qwencloud" }] }));
		};

		const apiKey = serializeAlibabaTokenPlanCredential(
			"sk-sp-beijing",
			"",
			"https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
		);
		const options = alibabaTokenPlanModelManagerOptions({ apiKey, fetch: fetchMock });
		const models = await options.fetchDynamicModels?.();

		expect(requestedUrl).toBe("https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1/models");
		expect(models?.[0]).toMatchObject({
			id: "qwen3.7-plus",
			baseUrl: "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
		});
	});

	test("drops pre-metadata cache rows while preserving the credential region offline", async () => {
		const tempDir = await fs.mkdtemp(path.join(os.tmpdir(), "pi-catalog-alibaba-token-plan-cache-"));
		const cacheDbPath = path.join(tempDir, "models.db");
		const cacheProviderId = "alibaba-token-plan-metadata-migration-test";
		const beijingBaseUrl = "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1";
		const staleKimi: ModelSpec<"openai-completions"> = {
			id: "kimi-k2.7-code",
			name: "kimi-k2.7-code",
			api: "openai-completions",
			provider: "alibaba-token-plan",
			baseUrl: beijingBaseUrl,
			reasoning: false,
			input: ["text"],
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
			contextWindow: null,
			maxTokens: null,
		};
		const oldStaticModelIds: Record<string, true> = {
			"qwen3.8-max-preview": true,
			"qwen3.7-max": true,
			"qwen3.7-plus": true,
			"qwen3.6-flash": true,
			"glm-5.2": true,
			"deepseek-v4-pro": true,
		};

		try {
			await resolveProviderModels(
				{
					providerId: "alibaba-token-plan",
					cacheProviderId,
					cacheDbPath,
					staticModels: ALIBABA_TOKEN_PLAN_STATIC_MODELS.filter(model => oldStaticModelIds[model.id]),
					dynamicModelsAuthoritative: true,
					fetchDynamicModels: async () => [staleKimi],
				},
				"online",
			);

			const apiKey = serializeAlibabaTokenPlanCredential("sk-sp-beijing", "", beijingBaseUrl);
			const offline = await resolveProviderModels(
				{
					...alibabaTokenPlanModelManagerOptions({ apiKey }),
					cacheProviderId,
					cacheDbPath,
				},
				"offline",
			);

			expect(offline.models.find(model => model.id === staleKimi.id)).toMatchObject({
				baseUrl: beijingBaseUrl,
				input: ["text", "image"],
				contextWindow: 262_144,
				maxTokens: 262_144,
			});
		} finally {
			await fs.rm(tempDir, { recursive: true, force: true });
		}
	});

	test("rejects malformed compound credentials before model discovery", () => {
		let fetched = false;
		const fetchMock: FetchImpl = () => {
			fetched = true;
			return Promise.resolve(Response.json({ data: [] }));
		};

		const options = alibabaTokenPlanModelManagerOptions({
			apiKey: '  {"token":"sk-sp-test","cookie":"session=secret"',
			fetch: fetchMock,
		});
		expect(options.fetchDynamicModels).toBeUndefined();
		expect(fetched).toBe(false);
	});

	test("uses Token Plan-specific environment keys and authoritative discovery", () => {
		const descriptor = CATALOG_PROVIDERS.find(provider => provider.id === "alibaba-token-plan");
		expect(descriptor).toMatchObject({
			defaultModel: "qwen3.7-plus",
			envVars: ["ALIBABA_TOKEN_PLAN_API_KEY", "BAILIAN_TOKEN_PLAN_API_KEY"],
			dynamicModelsAuthoritative: true,
			catalogDiscovery: { label: "QwenCloud Token Plan" },
		});
	});
});
