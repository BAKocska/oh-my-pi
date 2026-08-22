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

	it("advertises Esc while active and retires the affordance once the test settles", () => {
		const block = new McpTestHintBlock("weather");
		block.mount({ requestRender: () => {} });

		const running = block.render(80).join("\n");
		expect(running).toContain('Testing connection to "weather"');
		expect(running).toContain("esc to cancel");
		expect(block.isTranscriptBlockFinalized()).toBe(false);

		block.settle();

		const settledLines = block.render(80).join("\n");
		expect(settledLines).toContain('Testing connection to "weather"');
		expect(settledLines).not.toContain("esc to cancel");
		// A finished block freezes so the transcript stops treating it as live.
		expect(block.isTranscriptBlockFinalized()).toBe(true);
	});
});
