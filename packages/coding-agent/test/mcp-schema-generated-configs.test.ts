/**
 * Regression: the published `mcp-schema.json` (referenced via `$schema` in every
 * generated `mcp.json`) must accept every server shape OMP can write, and still
 * reject typos and cross-transport mixing.
 *
 * Before the fix, each transport def composed `serverBase` with `allOf` while its
 * own branch set `additionalProperties: false`. Per JSON Schema, that keyword only
 * sees the branch it sits in, so the base-branch properties (`auth`, `oauth`,
 * `enabled`, `timeout`, `requestIdFormat`) counted as "additional" and were
 * rejected — the exact failure editors like Zed reported (#7948). The fix moves
 * closure to `unevaluatedProperties: false` at the composed level, which is aware
 * of the sibling `allOf` branch while still catching unknown keys.
 */

import { describe, expect, test } from "bun:test";
import Ajv2020 from "ajv/dist/2020";
import mcpSchema from "../src/config/mcp-schema.json";

const validate = new Ajv2020({ allErrors: true, strict: false }).compile(mcpSchema);

function config(server: Record<string, unknown>) {
	return { $schema: mcpSchema.$id, mcpServers: { example: server } };
}

describe("published mcp-schema.json accepts every generated server shape", () => {
	// Each entry mirrors a shape OMP writes via config-writer / oauth-flow.
	const accepted: Record<string, Record<string, unknown>> = {
		stdio: { type: "stdio", command: "node", args: ["server.js"], env: { KEY: "val" }, cwd: "/tmp" },
		"stdio implicit type": { command: "uvx", args: ["some-server"] },
		http: { type: "http", url: "https://example.com/mcp", headers: { Authorization: "Bearer x" } },
		sse: { type: "sse", url: "https://example.com/sse" },
		"http + oauth auth + oauth client (issue #7948)": {
			type: "http",
			url: "https://example.com/mcp",
			auth: {
				type: "oauth",
				credentialId: "cred-123",
				tokenUrl: "https://example.com/oauth/token",
				clientId: "client-abc",
				resource: "https://example.com/mcp",
			},
			oauth: { clientId: "client-abc" },
		},
		"http + apikey auth": {
			type: "http",
			url: "https://example.com/mcp",
			auth: { type: "apikey" },
		},
		"base options on every transport": {
			type: "sse",
			url: "https://example.com/sse",
			enabled: false,
			timeout: 0,
			requestIdFormat: "string",
		},
	};

	for (const name in accepted) {
		test(name, () => {
			const ok = validate(config(accepted[name]));
			expect(validate.errors).toBeNull();
			expect(ok).toBe(true);
		});
	}

	const rejected: Record<string, Record<string, unknown>> = {
		"unknown property (typo detection preserved)": {
			type: "http",
			url: "https://example.com/mcp",
			bogusKey: true,
		},
		"stdio must not carry url": { type: "stdio", command: "node", url: "https://example.com/mcp" },
		"http must not carry command": { type: "http", url: "https://example.com/mcp", command: "node" },
	};

	for (const name in rejected) {
		test(`rejects: ${name}`, () => {
			expect(validate(config(rejected[name]))).toBe(false);
		});
	}
});
