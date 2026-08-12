import { describe, expect, it, vi } from "bun:test";
import { loginTogether } from "@oh-my-pi/pi-ai/registry/together";
import type { FetchImpl } from "@oh-my-pi/pi-ai/types";

describe("together login", () => {
	it("validates against the models endpoint, not a specific-model chat probe", async () => {
		const fetchMock: FetchImpl = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
			const url = typeof input === "string" ? input : input.toString();
			// Regression guard for #8328: a chat-completions probe against a
			// non-serverless model (moonshotai/Kimi-K2.5) 400s even for a valid
			// key, so login must not depend on any single model's availability.
			expect(url).toBe("https://api.together.xyz/v1/models");
			expect(init?.method).toBe("GET");
			expect(init?.headers).toEqual({ Authorization: "Bearer sk-together-test" });
			return new Response(JSON.stringify({ object: "list", data: [] }), {
				status: 200,
				headers: { "Content-Type": "application/json" },
			});
		});

		const apiKey = await loginTogether({
			onPrompt: async () => "sk-together-test",
			fetch: fetchMock,
		});

		expect(apiKey).toBe("sk-together-test");
		expect(fetchMock).toHaveBeenCalledTimes(1);
	});

	it("surfaces validation errors from the models endpoint", async () => {
		const fetchMock: FetchImpl = vi.fn(async () => {
			return new Response('{"error":{"code":"invalid_api_key"}}', {
				status: 401,
				headers: { "Content-Type": "application/json" },
			});
		});

		await expect(
			loginTogether({
				onPrompt: async () => "sk-together-test",
				fetch: fetchMock,
			}),
		).rejects.toThrow("together API key validation failed (401)");
	});
});
