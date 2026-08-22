import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "bun:test";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";
import * as mcpClient from "@oh-my-pi/pi-coding-agent/mcp/client";
import * as mcpConfigWriter from "@oh-my-pi/pi-coding-agent/mcp/config-writer";
import type { MCPConfigFile, MCPServerConnection } from "@oh-my-pi/pi-coding-agent/mcp/types";
import {
	MCPCommandController,
	McpTestHintBlock,
} from "@oh-my-pi/pi-coding-agent/modes/controllers/mcp-command-controller";
import { initTheme } from "@oh-my-pi/pi-coding-agent/modes/theme/theme";
import { getConfigRootDir, getProjectDir, removeWithRetries, setAgentDir, setProjectDir } from "@oh-my-pi/pi-utils";

const originalProjectDir = getProjectDir();
const originalAgentDir = process.env.PI_CODING_AGENT_DIR;
const fallbackAgentDir = path.join(getConfigRootDir(), "agent");

describe("interactive /mcp test", () => {
	let projectDir = "";
	let agentDir = "";

	beforeAll(() => {
		initTheme();
	});

	beforeEach(async () => {
		projectDir = await fs.mkdtemp(path.join(os.tmpdir(), "omp-issue-956-project-"));
		agentDir = await fs.mkdtemp(path.join(os.tmpdir(), "omp-issue-956-agent-"));
		setProjectDir(projectDir);
		setAgentDir(agentDir);

		await fs.writeFile(
			path.join(projectDir, ".mcp.json"),
			JSON.stringify(
				{
					mcpServers: {
						github: {
							type: "stdio",
							command: "github-mcp-server",
							args: ["serve"],
						},
					},
				},
				null,
				2,
			),
		);
	});

	afterEach(async () => {
		vi.useRealTimers();
		vi.restoreAllMocks();
		setProjectDir(originalProjectDir);
		if (originalAgentDir) {
			setAgentDir(originalAgentDir);
		} else {
			setAgentDir(fallbackAgentDir);
			delete process.env.PI_CODING_AGENT_DIR;
		}
		await removeWithRetries(projectDir);
		await removeWithRetries(agentDir);
	});

	it("tests a discovered server and reports a settled Esc during the cancellation grace", async () => {
		vi.useFakeTimers();
		const transport = {
			connected: true,
			request: vi.fn(),
			notify: vi.fn(),
			close: vi.fn(async () => {}),
		};
		const connection = {
			name: "github",
			config: { type: "stdio" as const, command: "github-mcp-server", args: ["serve"] },
			transport,
			serverInfo: { name: "GitHub MCP", version: "1.0.0" },
			capabilities: {},
		};
		const showError = vi.fn();
		const showStatus = vi.fn();
		const requestRender = vi.fn();
		const addChild = vi.fn();
		const refreshMCPTools = vi.fn();
		const connectToServer = vi.spyOn(mcpClient, "connectToServer").mockResolvedValue(connection);
		const listTools = vi.spyOn(mcpClient, "listTools").mockResolvedValue([{ name: "search_issues" }] as never);
		const disconnectServer = vi.spyOn(mcpClient, "disconnectServer").mockResolvedValue();
		const mcpTestEscapeHandlers = new Set<() => void>();
		const controller = new MCPCommandController({
			mcpTestEscapeHandlers,
			chatContainer: { addChild },
			present: (content: unknown) => {
				for (const item of Array.isArray(content) ? content : [content]) addChild(item);
				requestRender();
			},
			presentCommandOutput: (content: unknown) => {
				for (const item of Array.isArray(content) ? content : [content]) addChild(item);
				requestRender();
			},
			ui: { requestRender },
			editor: {},
			showError,
			showStatus,
			session: { refreshMCPTools },
			mcpManager: {
				prepareConfig: vi.fn(async config => config),
				getConnectionStatus: vi.fn(() => "connected"),
			},
		} as never);

		await controller.handle("/mcp test github");
		const signal = connectToServer.mock.calls[0]?.[2]?.signal;
		expect(signal?.aborted).toBe(false);
		expect(mcpTestEscapeHandlers).toHaveLength(1);
		for (const handler of mcpTestEscapeHandlers) handler();
		expect(signal?.aborted).toBe(false);
		expect(showStatus).toHaveBeenCalledWith('MCP test for "github" already finished');
		vi.advanceTimersByTime(4_999);
		expect(mcpTestEscapeHandlers).toHaveLength(1);
		vi.advanceTimersByTime(1);
		expect(mcpTestEscapeHandlers).toHaveLength(0);

		expect(showError).not.toHaveBeenCalled();
		expect(connectToServer).toHaveBeenCalledWith(
			"github",
			expect.objectContaining({ command: "github-mcp-server", args: ["serve"] }),
			expect.objectContaining({ signal: expect.any(AbortSignal) }),
		);
		expect(listTools).toHaveBeenCalledWith(connection, expect.objectContaining({ signal: expect.any(AbortSignal) }));
		expect(disconnectServer).toHaveBeenCalledWith(connection);
		expect(requestRender).toHaveBeenCalled();
	});

	it("retires the live hint immediately when a pending test is cancelled", async () => {
		const connectToServer = vi.spyOn(mcpClient, "connectToServer").mockImplementation((_name, _config, options) => {
			const { promise, reject } = Promise.withResolvers<MCPServerConnection>();
			const signal = options?.signal;
			if (!signal) return promise;
			const abort = () => {
				const error = new Error("aborted");
				error.name = "AbortError";
				reject(error);
			};
			if (signal.aborted) {
				abort();
			} else {
				signal.addEventListener("abort", abort, { once: true });
			}
			return promise;
		});
		vi.spyOn(mcpClient, "disconnectServer").mockResolvedValue();
		const { promise: lookup, resolve: resolveLookup } = Promise.withResolvers<MCPConfigFile>();
		vi.spyOn(mcpConfigWriter, "readMCPConfigFile").mockReturnValue(lookup);
		const showStatus = vi.fn();
		const requestRender = vi.fn();
		let hint: McpTestHintBlock | undefined;
		const { promise: hintPresented, resolve: resolveHintPresented } = Promise.withResolvers<void>();
		const mcpTestEscapeHandlers = new Set<() => void>();
		const controller = new MCPCommandController({
			mcpTestEscapeHandlers,
			chatContainer: { addChild: vi.fn() },
			present: (content: unknown) => {
				if (!(content instanceof McpTestHintBlock)) return;
				hint = content;
				content.mount({ requestRender });
				resolveHintPresented();
			},
			presentCommandOutput: vi.fn(),
			ui: { requestRender },
			editor: {},
			showError: vi.fn(),
			showStatus,
			session: { refreshMCPTools: vi.fn() },
			mcpManager: {
				prepareConfig: vi.fn(async config => config),
				getConnectionStatus: vi.fn(() => "connected"),
			},
		} as never);

		const pending = controller.handle("/mcp test github");
		expect(mcpTestEscapeHandlers).toHaveLength(1);
		resolveLookup({
			mcpServers: { github: { type: "stdio", command: "github-mcp-server", args: ["serve"] } },
		});
		await hintPresented;
		if (!hint) throw new Error("MCP test hint was not presented");
		expect(hint.isTranscriptBlockFinalized()).toBe(false);

		const owners = [...mcpTestEscapeHandlers];
		mcpTestEscapeHandlers.clear();
		for (const owner of owners) owner();

		// Cancellation rewrites and freezes the hint synchronously, before the
		// connection stack finishes unwinding from the abort.
		const cancelledHint = hint.render(80).join("\n");
		expect(cancelledHint).toContain('Cancelled connection test for "github".');
		expect(cancelledHint).not.toContain("(esc to cancel)");
		expect(hint.isTranscriptBlockFinalized()).toBe(true);

		await pending;
		expect(showStatus).toHaveBeenCalledWith('Cancelled MCP test for "github"');
		expect(showStatus).not.toHaveBeenCalledWith('MCP test for "github" already finished');
		expect(mcpTestEscapeHandlers).toHaveLength(0);
		expect(connectToServer).toHaveBeenCalledTimes(1);
	});

	it("cancels during the awaited lookup without publishing an Esc hint", async () => {
		const { promise: lookup, resolve } = Promise.withResolvers<MCPConfigFile>();
		vi.spyOn(mcpConfigWriter, "readMCPConfigFile").mockReturnValue(lookup);
		const connectToServer = vi.spyOn(mcpClient, "connectToServer");
		const present = vi.fn();
		const showStatus = vi.fn();
		const mcpTestEscapeHandlers = new Set<() => void>();
		const controller = new MCPCommandController({
			mcpTestEscapeHandlers,
			chatContainer: { addChild: vi.fn() },
			present,
			presentCommandOutput: vi.fn(),
			ui: { requestRender: vi.fn() },
			editor: {},
			showError: vi.fn(),
			showStatus,
			session: { refreshMCPTools: vi.fn() },
			mcpManager: {
				getServerConfig: vi.fn(() => undefined),
				getSource: vi.fn(() => undefined),
				prepareConfig: vi.fn(async config => config),
				getConnectionStatus: vi.fn(() => "connected"),
			},
		} as never);

		const pending = controller.handle("/mcp test github");
		expect(mcpTestEscapeHandlers).toHaveLength(1);

		const owners = [...mcpTestEscapeHandlers];
		mcpTestEscapeHandlers.clear();
		for (const owner of owners) owner();
		resolve({
			mcpServers: { github: { type: "stdio", command: "github-mcp-server", args: ["serve"] } },
		});
		await pending;

		expect(present).not.toHaveBeenCalled();
		expect(connectToServer).not.toHaveBeenCalled();
		expect(showStatus).toHaveBeenCalledWith('Cancelled MCP test for "github"');
		expect(mcpTestEscapeHandlers).toHaveLength(0);
	});

	it("claims Esc ownership before the awaited server lookup", async () => {
		const connection = {
			name: "github",
			config: { type: "stdio" as const, command: "github-mcp-server", args: ["serve"] },
			transport: { connected: true, request: vi.fn(), notify: vi.fn(), close: vi.fn(async () => {}) },
			serverInfo: { name: "GitHub MCP", version: "1.0.0" },
			capabilities: {},
		};
		vi.spyOn(mcpClient, "connectToServer").mockResolvedValue(connection);
		vi.spyOn(mcpClient, "listTools").mockResolvedValue([{ name: "search_issues" }] as never);
		vi.spyOn(mcpClient, "disconnectServer").mockResolvedValue();
		const mcpTestEscapeHandlers = new Set<() => void>();
		const controller = new MCPCommandController({
			mcpTestEscapeHandlers,
			chatContainer: { addChild: vi.fn() },
			present: vi.fn(),
			presentCommandOutput: vi.fn(),
			ui: { requestRender: vi.fn() },
			editor: {},
			showError: vi.fn(),
			showStatus: vi.fn(),
			session: { refreshMCPTools: vi.fn() },
			mcpManager: {
				prepareConfig: vi.fn(async config => config),
				getConnectionStatus: vi.fn(() => "connected"),
			},
		} as never);

		// Do not await: the handler must be registered synchronously, before the
		// awaited `#resolveServerForAuth()` config read can suspend and let Esc
		// fall through to aborting the agent turn.
		const pending = controller.handle("/mcp test github");
		expect(mcpTestEscapeHandlers).toHaveLength(1);
		await pending;
	});

	it("releases Esc immediately when lookup fails before the hint is shown", async () => {
		vi.spyOn(mcpConfigWriter, "readMCPConfigFile").mockRejectedValue(new Error("EACCES: config unreadable"));
		const connectToServer = vi.spyOn(mcpClient, "connectToServer");
		const showError = vi.fn();
		const mcpTestEscapeHandlers = new Set<() => void>();
		const controller = new MCPCommandController({
			mcpTestEscapeHandlers,
			chatContainer: { addChild: vi.fn() },
			present: vi.fn(),
			presentCommandOutput: vi.fn(),
			ui: { requestRender: vi.fn() },
			editor: {},
			showError,
			showStatus: vi.fn(),
			session: { refreshMCPTools: vi.fn() },
			mcpManager: {
				getServerConfig: vi.fn(() => undefined),
				getSource: vi.fn(() => undefined),
			},
		} as never);

		await controller.handle("/mcp test github");

		// The "(esc to cancel)" hint never rendered, so no grace window applies:
		// Esc must be free again immediately instead of being swallowed for 5s.
		expect(mcpTestEscapeHandlers).toHaveLength(0);
		expect(connectToServer).not.toHaveBeenCalled();
		expect(showError).toHaveBeenCalled();
	});
});
