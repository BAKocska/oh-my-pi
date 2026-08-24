import { afterEach, describe, expect, it } from "bun:test";
import { findFreeCdpPort } from "@oh-my-pi/pi-coding-agent/tools/browser/attach";
import type {
	ExtToRelayMessage,
	RelayRpcRequest,
	RelayToExtMessage,
	TabSnapshot,
} from "@oh-my-pi/pi-coding-agent/tools/browser/relay/protocol";
import { type RelayServer, startRelayServer } from "@oh-my-pi/pi-coding-agent/tools/browser/relay/server";

const EXTENSION_HELLO = {
	t: "hello",
	userAgent: "test",
	browserVersion: "Chrome/151.0.0.0",
	tabs: [],
	attachedTabIds: [],
} as const;

async function rawGet(port: number, requestBytes: string): Promise<string> {
	const { promise, resolve, reject } = Promise.withResolvers<string>();
	let response = "";
	await Bun.connect({
		hostname: "127.0.0.1",
		port,
		socket: {
			open(socket) {
				socket.write(requestBytes);
			},
			data(_socket, chunk) {
				response += chunk.toString("latin1");
			},
			error(_socket, error) {
				reject(error);
			},
			close() {
				resolve(response);
			},
		},
	});
	return promise;
}

function decodeChunkedBody(body: string): string {
	let decoded = "";
	let offset = 0;
	while (true) {
		const lineEnd = body.indexOf("\r\n", offset);
		if (lineEnd === -1) throw new Error("Invalid chunked response: missing chunk size");
		const lengthText = body.slice(offset, lineEnd).split(";", 1)[0]!;
		const length = Number.parseInt(lengthText, 16);
		if (!Number.isFinite(length) || length < 0) throw new Error("Invalid chunked response: invalid chunk size");
		offset = lineEnd + 2;
		if (length === 0) return decoded;
		if (body.length < offset + length + 2) throw new Error("Invalid chunked response: truncated chunk");
		decoded += body.slice(offset, offset + length);
		offset += length;
		if (body.slice(offset, offset + 2) !== "\r\n")
			throw new Error("Invalid chunked response: missing chunk terminator");
		offset += 2;
	}
}

function parseVersion(response: string): Record<string, string> {
	const boundary = response.indexOf("\r\n\r\n");
	if (boundary === -1) throw new Error("Invalid HTTP response: missing header boundary");
	const headers = response.slice(0, boundary);
	const body = response.slice(boundary + 4);
	expect(headers).toContain("200");
	return JSON.parse(/\r\ntransfer-encoding:\s*chunked\b/i.test(headers) ? decodeChunkedBody(body) : body) as Record<
		string,
		string
	>;
}

async function connectExtension(port: number): Promise<WebSocket> {
	const { promise, resolve, reject } = Promise.withResolvers<WebSocket>();
	const ws = new WebSocket(`ws://127.0.0.1:${port}/ext`);
	ws.addEventListener(
		"open",
		() => {
			ws.send(JSON.stringify(EXTENSION_HELLO));
			resolve(ws);
		},
		{ once: true },
	);
	ws.addEventListener("error", () => reject(new Error("Extension socket failed to connect")), { once: true });
	return promise;
}

async function waitForDiscovery(port: number): Promise<void> {
	const deadline = Date.now() + 1_000;
	while (Date.now() < deadline) {
		const response = await fetch(`http://127.0.0.1:${port}/json/version`);
		if (response.status === 200) return;
	}
	throw new Error("Relay discovery endpoint did not become ready");
}

describe("browser relay discovery endpoint", () => {
	let relay: RelayServer | undefined;
	let extension: WebSocket | undefined;

	afterEach(() => {
		extension?.close();
		relay?.stop();
		extension = undefined;
		relay = undefined;
	});

	async function startReadyRelay(): Promise<number> {
		const port = await findFreeCdpPort();
		relay = startRelayServer({ port });
		extension = await connectExtension(port);
		await waitForDiscovery(port);
		return port;
	}

	it("advertises the requested Host authority so a remote Puppeteer client dials the relay", async () => {
		const port = await startReadyRelay();
		const response = await rawGet(
			port,
			"GET /json/version HTTP/1.1\r\nHost: 100.100.92.97:12803\r\nConnection: close\r\n\r\n",
		);
		expect(parseVersion(response).webSocketDebuggerUrl).toBe("ws://100.100.92.97:12803/cdp");
	});

	it("uses the loopback discovery URL when an HTTP/1.0 request has no Host header", async () => {
		const port = await startReadyRelay();
		const response = await rawGet(port, "GET /json/version HTTP/1.0\r\n\r\n");
		expect(parseVersion(response).webSocketDebuggerUrl).toBe(`ws://127.0.0.1:${port}/cdp`);
	});

	it("uses the loopback discovery URL when Host is empty", async () => {
		const port = await startReadyRelay();
		const response = await rawGet(port, "GET /json/version HTTP/1.1\r\nHost: \r\nConnection: close\r\n\r\n");
		expect(parseVersion(response).webSocketDebuggerUrl).toBe(`ws://127.0.0.1:${port}/cdp`);
	});

	it("uses the loopback discovery URL when Host would produce an unusable WebSocket authority", async () => {
		const port = await startReadyRelay();
		const response = await rawGet(
			port,
			"GET /json/version HTTP/1.1\r\nHost: bad/host@evil\r\nConnection: close\r\n\r\n",
		);
		expect(parseVersion(response).webSocketDebuggerUrl).toBe(`ws://127.0.0.1:${port}/cdp`);
	});

	it("reports 503 while the extension handshake is pending so the relay daemon keeps polling", async () => {
		const port = await findFreeCdpPort();
		relay = startRelayServer({ port });
		const response = await fetch(`http://127.0.0.1:${port}/json/version`);
		expect(response.status).toBe(503);
	});
});

interface ScriptedExtension {
	ws: WebSocket;
	/** Every RPC the relay drove the extension with, in arrival order. */
	rpcs: RelayRpcRequest[];
	/** Settle any detach RPCs withheld via the `deferDetach` option (echo + result). */
	flushDetach(): void;
}

/** Extension that acknowledges every RPC and records it, so tests can assert relay-driven upstream traffic. */
async function connectScriptedExtension(
	port: number,
	tabs: TabSnapshot[],
	opts: { deferDetach?: boolean } = {},
): Promise<ScriptedExtension> {
	const rpcs: RelayRpcRequest[] = [];
	const withheld: Array<{ id: number; tabId: number }> = [];
	const opened = Promise.withResolvers<void>();
	const ws = new WebSocket(`ws://127.0.0.1:${port}/ext`);
	ws.addEventListener("open", () => opened.resolve(), { once: true });
	ws.addEventListener("error", () => opened.reject(new Error("Extension socket failed to connect")), { once: true });
	// Mirror Chrome: chrome.debugger.detach() would fire onDetach, which the real
	// extension forwards as `detached` before the detach RPC result settles.
	const settleDetach = (id: number, tabId: number): void => {
		ws.send(JSON.stringify({ t: "detached", tabId, reason: "target_closed" } satisfies ExtToRelayMessage));
		ws.send(JSON.stringify({ t: "rpcResult", id, ok: true, result: {} } satisfies ExtToRelayMessage));
	};
	ws.addEventListener("message", event => {
		const msg = JSON.parse(String(event.data)) as RelayToExtMessage;
		if (msg.t !== "rpc") return;
		const { id, t: _t, ...rpc } = msg;
		rpcs.push(rpc);
		if (rpc.op === "detach") {
			if (opts.deferDetach) withheld.push({ id, tabId: rpc.tabId });
			else settleDetach(id, rpc.tabId);
			return;
		}
		ws.send(JSON.stringify({ t: "rpcResult", id, ok: true, result: {} } satisfies ExtToRelayMessage));
	});
	await opened.promise;
	ws.send(
		JSON.stringify({
			t: "hello",
			userAgent: "test",
			browserVersion: "Chrome/150.0.0.0",
			tabs,
			attachedTabIds: [],
		} satisfies ExtToRelayMessage),
	);
	const flushDetach = (): void => {
		for (const { id, tabId } of withheld.splice(0)) settleDetach(id, tabId);
	};
	return { ws, rpcs, flushDetach };
}

interface CdpResponse {
	id: number;
	result?: Record<string, unknown>;
	error?: unknown;
}

interface CdpClient {
	ws: WebSocket;
	send(method: string, params?: Record<string, unknown>): Promise<CdpResponse>;
}

/** Minimal downstream CDP client: numbered request/response over the /cdp socket. */
async function connectCdpClient(port: number): Promise<CdpClient> {
	const opened = Promise.withResolvers<void>();
	const ws = new WebSocket(`ws://127.0.0.1:${port}/cdp`);
	ws.addEventListener("open", () => opened.resolve(), { once: true });
	ws.addEventListener("error", () => opened.reject(new Error("CDP socket failed to connect")), { once: true });
	await opened.promise;
	let nextId = 0;
	const pending = new Map<number, (msg: CdpResponse) => void>();
	ws.addEventListener("message", event => {
		const msg = JSON.parse(String(event.data)) as CdpResponse;
		if (typeof msg.id === "number") pending.get(msg.id)?.(msg);
	});
	const send = (method: string, params: Record<string, unknown> = {}): Promise<CdpResponse> => {
		const id = ++nextId;
		const { promise, resolve } = Promise.withResolvers<CdpResponse>();
		pending.set(id, resolve);
		ws.send(JSON.stringify({ id, method, params }));
		return promise;
	};
	return { ws, send };
}

function fakeTab(tabId: number, url: string): TabSnapshot {
	return { tabId, url, title: url, active: tabId === 1, windowId: 1, pinned: false, groupId: -1 };
}

describe("browser relay session detach", () => {
	let relay: RelayServer | undefined;
	let extension: WebSocket | undefined;
	const clients: WebSocket[] = [];

	afterEach(() => {
		for (const ws of clients) ws.close();
		clients.length = 0;
		extension?.close();
		relay?.stop();
		extension = undefined;
		relay = undefined;
	});

	// Two tabs so a later attach on tab 2 can serve as a deterministic marker:
	// the ext socket is FIFO, so once the tab-2 attach round-trips, any upstream
	// detach the tab-1 release dispatched is already recorded.
	async function startReadyRelay(
		opts: { deferDetach?: boolean } = {},
	): Promise<{ port: number; rpcs: RelayRpcRequest[]; ext: ScriptedExtension }> {
		const port = await findFreeCdpPort();
		relay = startRelayServer({ port });
		const ext = await connectScriptedExtension(
			port,
			[fakeTab(1, "https://one.example/"), fakeTab(2, "https://two.example/")],
			opts,
		);
		extension = ext.ws;
		await waitForDiscovery(port);
		return { port, rpcs: ext.rpcs, ext };
	}

	async function openClient(port: number): Promise<CdpClient> {
		const client = await connectCdpClient(port);
		clients.push(client.ws);
		return client;
	}

	it("detaches the tab upstream when the last session is released via Target.detachFromTarget", async () => {
		const { port, rpcs } = await startReadyRelay();
		const client = await openClient(port);
		const attached = await client.send("Target.attachToTarget", { targetId: "PAGE1", flatten: true });
		expect(typeof attached.result?.sessionId).toBe("string");

		await client.send("Target.detachFromTarget", { sessionId: attached.result?.sessionId });
		// Marker attach on tab 2 — once it resolves the tab-1 detach (if any) is recorded.
		await client.send("Target.attachToTarget", { targetId: "PAGE2", flatten: true });
		expect(rpcs.filter(rpc => rpc.op === "detach" && rpc.tabId === 1)).toHaveLength(1);
	});

	it("keeps the tab attached while another connection still holds a session", async () => {
		const { port, rpcs } = await startReadyRelay();
		const a = await openClient(port);
		const b = await openClient(port);
		const attachedA = await a.send("Target.attachToTarget", { targetId: "PAGE1", flatten: true });
		await b.send("Target.attachToTarget", { targetId: "PAGE1", flatten: true });

		await a.send("Target.detachFromTarget", { sessionId: attachedA.result?.sessionId });
		// Marker attach on tab 2 through the surviving holder flushes the ext socket.
		await b.send("Target.attachToTarget", { targetId: "PAGE2", flatten: true });
		expect(rpcs.filter(rpc => rpc.op === "detach" && rpc.tabId === 1)).toHaveLength(0);
	});

	it("re-attaches a tab after the extension echoes the relay-initiated detach", async () => {
		const { port } = await startReadyRelay();
		const client = await openClient(port);
		const attached = await client.send("Target.attachToTarget", { targetId: "PAGE1", flatten: true });

		await client.send("Target.detachFromTarget", { sessionId: attached.result?.sessionId });
		// Marker attach on tab 2 is FIFO-ordered after the tab-1 detach and its
		// `detached` echo, so once it resolves the echo has been processed.
		await client.send("Target.attachToTarget", { targetId: "PAGE2", flatten: true });

		// The echo must not have banned the tab: re-attach still succeeds.
		const reattached = await client.send("Target.attachToTarget", { targetId: "PAGE1", flatten: true });
		expect(reattached.error).toBeUndefined();
		expect(typeof reattached.result?.sessionId).toBe("string");
	});

	it("serializes a reattach behind an in-flight relay-initiated detach", async () => {
		const { port, rpcs, ext } = await startReadyRelay({ deferDetach: true });
		const a = await openClient(port);
		const attached = await a.send("Target.attachToTarget", { targetId: "PAGE1", flatten: true });
		// Release the last session; the detach RPC is now in flight (withheld).
		await a.send("Target.detachFromTarget", { sessionId: attached.result?.sessionId });

		// A second client reattaches while the detach is unresolved.
		const b = await openClient(port);
		const reattach = b.send("Target.attachToTarget", { targetId: "PAGE1", flatten: true });
		// Browser.getVersion never parks, so once it replies the relay has already
		// processed b's attach and is waiting on the pending detach: no new attach
		// RPC has gone out yet — only the original one.
		await b.send("Browser.getVersion");
		expect(rpcs.filter(rpc => rpc.op === "attach" && rpc.tabId === 1)).toHaveLength(1);

		// Settle the detach; the serialized reattach now proceeds and succeeds.
		ext.flushDetach();
		const reattached = await reattach;
		expect(reattached.error).toBeUndefined();
		expect(typeof reattached.result?.sessionId).toBe("string");
		expect(rpcs.filter(rpc => rpc.op === "attach" && rpc.tabId === 1)).toHaveLength(2);
	});
});
