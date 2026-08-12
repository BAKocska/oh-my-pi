import { createApiKeyLogin } from "./api-key-login";
import type { ProviderDefinition } from "./types";

export const loginTogether = createApiKeyLogin({
	providerLabel: "Together",
	authUrl: "https://api.together.xyz/settings/api-keys",
	instructions: "Copy your API key from the Together dashboard",
	promptMessage: "Paste your Together API key",
	placeholder: "sk-...",
	validation: {
		// Together serves many models (e.g. moonshotai/Kimi-K2.5) only via dedicated
		// non-serverless endpoints, so a chat-completions probe against any single
		// model can 400 with `model_not_available` even for a valid key (#8328).
		// Validate against the models listing instead — it verifies the key without
		// depending on any one model's serverless availability.
		kind: "models-endpoint",
		provider: "together",
		modelsUrl: "https://api.together.xyz/v1/models",
	},
});

export const togetherProvider = {
	id: "together",
	name: "Together",
	login: (cb: Parameters<typeof loginTogether>[0]) => loginTogether(cb),
} as const satisfies ProviderDefinition;
