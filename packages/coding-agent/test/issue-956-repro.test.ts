import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "bun:test";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";
import * as mcpClient from "@oh-my-pi/pi-coding-agent/mcp/client";
import * as mcpConfigWriter from "@oh-my-pi/pi-coding-agent/mcp/config-writer";
import type { MCPConfigFile, MCPServerConnection } from "@oh-my-pi/pi-coding-agent/mcp/types";
import { MCPCommandController } from "@oh-my-pi/pi-coding-agent/modes/controllers/mcp-command-controller";
import { initTheme } from "@oh-my-pi/pi-coding-agent/modes/theme/theme";
import { getConfigRootDir, getProjectDir, removeWithRetries, setAgentDir, setProjectDir } from "@oh-my-pi/pi-utils";

const originalProjectDir = getProjectDir();
const originalAgentDir = process.env.PI_CODING_AGENT_DIR;
const fallbackAgentDir = path.join(getConfigRootDir(), "agent");

type Renderable = { render: (width: number) => readonly string[] };

/**
 * Minimal stand-in for the anchored `statusContainer`: records live children
 * (present after add, gone after remove) and every child ever added, and
 * resolves {@link firstAdd} the first time the `/mcp test` hint is anchored.
 */
function createStatusContainer() {
	const children: Renderable[] = [];
	const added: Renderable[] = [];
	const first = Promise.withResolvers<void>();
	return {
		children,
		added,
		firstAdd: first.promise,
		addChild(child: unknown) {
			children.push(child as Renderable);
			added.push(child as Renderable);
			first.resolve();
		},
		removeChild(child: unknown) {
			const index = children.indexOf(child as Renderable);
			if (index !== -1) children.splice(index, 1);
		},
	};
}

/** UI stub carrying the render hooks a live {@link Loader} drives. */
function fakeUi() {
	return { requestRender: vi.fn(), requestComponentRender: vi.fn(), requestDirectWrite: vi.fn() };
}

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

	it("advertises Esc in the anchored status container, retires it on settle, and keeps the grace", async () => {
		vi.useFakeTimers();
		const connection = {
			name: "github",
			config: { type: "stdio" as const, command: "github-mcp-server", args: ["serve"] },
			transport: { connected: true, request: vi.fn(), notify: vi.fn(), close: vi.fn(async () => {}) },
			serverInfo: { name: "GitHub MCP", version: "1.0.0" },
			capabilities: {},
		};
		const showError = vi.fn();
		const showStatus = vi.fn();
		const transcript: Renderable[] = [];
		const connectToServer = vi.spyOn(mcpClient, "connectToServer").mockResolvedValue(connection);
		const listTools = vi.spyOn(mcpClient, "listTools").mockResolvedValue([{ name: "search_issues" }] as never);
		const disconnectServer = vi.spyOn(mcpClient, "disconnectServer").mockResolvedValue();
		const status = createStatusContainer();
		const mcpTestEscapeHandlers = new Set<() => void>();
		const controller = new MCPCommandController({
			mcpTestEscapeHandlers,
			chatContainer: { addChild: vi.fn() },
			statusContainer: status,
			present: vi.fn(),
			presentCommandOutput: (content: unknown) => {
				const items = Array.isArray(content) ? content : [content];
				for (const item of items) {
					if (item && typeof (item as Renderable).render === "function") transcript.push(item as Renderable);
				}
			},
			ui: fakeUi(),
			editor: {},
			showError,
			showStatus,
			session: { refreshMCPTools: vi.fn() },
			mcpManager: {
				prepareConfig: vi.fn(async config => config),
				getConnectionStatus: vi.fn(() => "connected"),
			},
		} as never);

		await controller.handle("/mcp test github");
		const signal = connectToServer.mock.calls[0]?.[2]?.signal;
		expect(signal?.aborted).toBe(false);

		// The hint advertised Esc from the anchored status container while the
		// test ran, then was retired in place on settle — never committed to the
		// transcript or native scrollback.
		const hintText = status.added.map(child => child.render(80).join("\n")).join("\n");
		expect(hintText).toContain("(esc to cancel)");
		expect(status.children).toHaveLength(0);
		const finalized = transcript.map(block => block.render(80).join("\n")).join("\n");
		expect(finalized).toContain('Successfully connected to "github"');
		expect(finalized).not.toContain("(esc to cancel)");

		// The grace window still holds while untouched...
		expect(mcpTestEscapeHandlers).toHaveLength(1);
		vi.advanceTimersByTime(4_999);
		expect(mcpTestEscapeHandlers).toHaveLength(1);

		// ...and a press inside it gives feedback instead of silently aborting
		// the already-settled controller.
		for (const handler of [...mcpTestEscapeHandlers]) {
			mcpTestEscapeHandlers.delete(handler); // mirrors InputController's consume-on-dispatch
			handler();
		}
		expect(showStatus).toHaveBeenCalledWith('MCP test for "github" already finished');
		expect(signal?.aborted).toBe(false);
		vi.advanceTimersByTime(1);
		expect(mcpTestEscapeHandlers).toHaveLength(0);

		expect(showError).not.toHaveBeenCalled();
		expect(listTools).toHaveBeenCalledWith(connection, expect.objectContaining({ signal: expect.any(AbortSignal) }));
		expect(disconnectServer).toHaveBeenCalledWith(connection);
	});

	it("retires the anchored Esc hint immediately when a pending test is cancelled", async () => {
		const connectToServer = vi.spyOn(mcpClient, "connectToServer").mockImplementation((_name, _config, options) => {
			const { promise, reject } = Promise.withResolvers<MCPServerConnection>();
			const signal = options?.signal;
			if (!signal) return promise;
			const abort = () => {
				const error = new Error("aborted");
				error.name = "AbortError";
				reject(error);
			};
			if (signal.aborted) abort();
			else signal.addEventListener("abort", abort, { once: true });
			return promise;
		});
		vi.spyOn(mcpClient, "disconnectServer").mockResolvedValue();
		const { promise: lookup, resolve: resolveLookup } = Promise.withResolvers<MCPConfigFile>();
		vi.spyOn(mcpConfigWriter, "readMCPConfigFile").mockReturnValue(lookup);
		const showStatus = vi.fn();
		const status = createStatusContainer();
		const mcpTestEscapeHandlers = new Set<() => void>();
		const controller = new MCPCommandController({
			mcpTestEscapeHandlers,
			chatContainer: { addChild: vi.fn() },
			statusContainer: status,
			present: vi.fn(),
			presentCommandOutput: vi.fn(),
			ui: fakeUi(),
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
		// Wait until the hint is anchored (connect still pending): this exercises
		// the post-hint cancellation path, not the pre-hint bailout.
		await status.firstAdd;
		expect(status.children).toHaveLength(1);
		expect(status.children[0]?.render(80).join("\n")).toContain("(esc to cancel)");

		const owners = [...mcpTestEscapeHandlers];
		mcpTestEscapeHandlers.clear();
		for (const owner of owners) owner();

		// The affordance leaves the anchored container synchronously, before the
		// connection stack finishes unwinding from the abort.
		expect(status.children).toHaveLength(0);

		await pending;
		expect(showStatus).toHaveBeenCalledWith('Cancelled MCP test for "github"');
		expect(showStatus).not.toHaveBeenCalledWith('MCP test for "github" already finished');
		expect(mcpTestEscapeHandlers).toHaveLength(0);
		expect(connectToServer).toHaveBeenCalledTimes(1);
	});

	it("cancels during the awaited lookup without anchoring an Esc hint", async () => {
		const { promise: lookup, resolve } = Promise.withResolvers<MCPConfigFile>();
		vi.spyOn(mcpConfigWriter, "readMCPConfigFile").mockReturnValue(lookup);
		const connectToServer = vi.spyOn(mcpClient, "connectToServer");
		const showStatus = vi.fn();
		const status = createStatusContainer();
		const mcpTestEscapeHandlers = new Set<() => void>();
		const controller = new MCPCommandController({
			mcpTestEscapeHandlers,
			chatContainer: { addChild: vi.fn() },
			statusContainer: status,
			present: vi.fn(),
			presentCommandOutput: vi.fn(),
			ui: fakeUi(),
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

		expect(status.added).toHaveLength(0);
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
			statusContainer: createStatusContainer(),
			ui: fakeUi(),
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
