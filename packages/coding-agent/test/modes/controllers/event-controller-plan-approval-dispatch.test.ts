import { afterEach, beforeAll, describe, expect, it, vi } from "bun:test";
import { EventController } from "@oh-my-pi/pi-coding-agent/modes/controllers/event-controller";
import { initTheme } from "@oh-my-pi/pi-coding-agent/modes/theme/theme";
import type { InteractiveModeContext } from "@oh-my-pi/pi-coding-agent/modes/types";
import type { AgentSessionEvent } from "@oh-my-pi/pi-coding-agent/session/agent-session";
import { PROPOSE_DEVICE_NAME } from "@oh-my-pi/pi-coding-agent/tools/resolve";

beforeAll(() => {
	initTheme();
});

/**
 * A completed `write xd://propose` execution with `mode: "execute"` — the event
 * that drives `#handleToolExecutionEnd` into `ctx.handlePlanApproval`.
 */
function proposeExecuteEnd(): AgentSessionEvent {
	return {
		type: "tool_execution_end",
		toolCallId: "propose-1",
		toolName: "write",
		isError: false,
		result: {
			content: [{ type: "text", text: "Plan ready for approval." }],
			details: {
				xdev: {
					tool: PROPOSE_DEVICE_NAME,
					mode: "execute",
					args: { title: "demo" },
					inner: { planFilePath: "local://demo-plan.md", title: "demo", planExists: true },
				},
			},
		},
	} as unknown as AgentSessionEvent;
}

describe("EventController plan-approval dispatch", () => {
	afterEach(() => {
		vi.restoreAllMocks();
	});

	it("keeps dispatching during approve-and-compact compaction and execution (issue #7684)", async () => {
		// Contract: "Approve and compact context" first awaits compaction, then
		// `session.prompt` for the WHOLE execution run. The propose write's
		// `tool_execution_end` handler runs inside `EventController`'s serialized
		// dispatch chain (`#runSerialized`), so awaiting `handlePlanApproval` there
		// froze every later agent_start / message_start / tool / message_update
		// event during BOTH stages. The approval must be detached so events keep
		// dispatching through compaction and the subsequent execution.
		let listener: ((event: AgentSessionEvent) => void | Promise<void>) | undefined;
		const compactionStarted = Promise.withResolvers<void>();
		const compaction = Promise.withResolvers<void>();
		const executionStarted = Promise.withResolvers<void>();
		const executionTurn = Promise.withResolvers<void>();
		const handlePlanApproval = vi.fn(async () => {
			compactionStarted.resolve();
			await compaction.promise;
			executionStarted.resolve();
			await executionTurn.promise;
		});

		const ctx = {
			isInitialized: true,
			session: {
				subscribe: (fn: (event: AgentSessionEvent) => void | Promise<void>) => {
					listener = fn;
					return () => {};
				},
			},
			viewSession: { isStreaming: false },
			pendingTools: new Map(),
			ui: { requestRender: vi.fn() },
			handlePlanApproval,
		} as unknown as InteractiveModeContext;

		const controller = new EventController(ctx);
		controller.subscribeToAgent();
		if (!listener) throw new Error("subscribeToAgent did not register a listener");
		const dispatch = listener;

		const handleEventSpy = vi.spyOn(controller, "handleEvent");

		// The propose completion opens Plan Review; the mocked approval represents
		// selecting "Approve and compact context" and stopping inside compaction.
		// The serialized link must settle despite the unresolved compaction.
		await dispatch(proposeExecuteEnd());
		await compactionStarted.promise;
		expect(handlePlanApproval).toHaveBeenCalledTimes(1);

		// `turn_start` rides the same `#runSerialized` gate as agent_start /
		// message_start / tool events / the coalesced message_update flush.
		await dispatch({ type: "turn_start" } as AgentSessionEvent);
		expect(handleEventSpy.mock.calls.filter(([event]) => event.type === "turn_start")).toHaveLength(1);

		// Finish compaction and hold the approved execution turn open. A second
		// event must still dispatch before that turn settles.
		compaction.resolve();
		await executionStarted.promise;
		await dispatch({ type: "turn_start" } as AgentSessionEvent);
		expect(handleEventSpy.mock.calls.filter(([event]) => event.type === "turn_start")).toHaveLength(2);

		executionTurn.resolve();
		await executionTurn.promise;
	});
});
