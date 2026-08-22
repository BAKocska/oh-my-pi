import { beforeAll, describe, expect, it } from "bun:test";
import { McpTestHintBlock } from "@oh-my-pi/pi-coding-agent/modes/controllers/mcp-command-controller";
import { initTheme } from "@oh-my-pi/pi-coding-agent/modes/theme/theme";

/**
 * The `/mcp test` hint must stop advertising Esc the moment the test settles, so
 * scrollback never keeps offering a cancellation that Esc no longer performs
 * once ownership lapses (#9310).
 */
describe("McpTestHintBlock", () => {
	beforeAll(() => {
		initTheme();
	});

	it("advertises Esc while active and seals the successful outcome on settle", () => {
		const block = new McpTestHintBlock("weather");
		block.mount({ requestRender: () => {} });

		const running = block.render(80).join("\n");
		expect(running).toContain('Testing connection to "weather"');
		expect(running).toContain("esc to cancel");
		expect(block.isTranscriptBlockFinalized()).toBe(false);

		block.settle("succeeded");

		const settledLines = block.render(80).join("\n");
		expect(settledLines).toContain('Tested connection to "weather".');
		expect(settledLines).not.toContain("esc to cancel");
		// A finished block freezes so the transcript stops treating it as live.
		expect(block.isTranscriptBlockFinalized()).toBe(true);
	});

	it("keeps an immediate cancellation outcome when final cleanup settles again", () => {
		const block = new McpTestHintBlock("weather");
		block.mount({ requestRender: () => {} });

		block.settle("cancelled");
		block.settle("succeeded");

		const rendered = block.render(80).join("\n");
		expect(rendered).toContain('Cancelled connection test for "weather".');
		expect(rendered).not.toContain("esc to cancel");
		expect(rendered).not.toContain('Tested connection to "weather".');
		expect(block.isTranscriptBlockFinalized()).toBe(true);
	});

	it("distinguishes a failed test from successful completion", () => {
		const block = new McpTestHintBlock("weather");
		block.mount({ requestRender: () => {} });

		block.settle("failed");

		const rendered = block.render(80).join("\n");
		expect(rendered).toContain('Connection test for "weather" failed.');
		expect(rendered).not.toContain("esc to cancel");
		expect(block.isTranscriptBlockFinalized()).toBe(true);
	});
});
