import { beforeEach, describe, expect, it } from "bun:test";
import { resetSettingsForTest, Settings } from "@oh-my-pi/pi-coding-agent/config/settings";
import type { MnemopiBackendConfig } from "@oh-my-pi/pi-coding-agent/mnemopi/config";
import {
	loadMnemopi,
	loadMnemopiCore,
	MnemopiSessionState,
	setMnemopiSessionState,
} from "@oh-my-pi/pi-coding-agent/mnemopi/state";
import type { AgentSessionEventListener } from "@oh-my-pi/pi-coding-agent/session/agent-session";
import { TempDir } from "@oh-my-pi/pi-utils";

// Mnemopi is lazy-loaded at runtime; preload it for synchronous state construction.
await Promise.all([loadMnemopi(), loadMnemopiCore()]);

const TEST_SESSION_ID = "test-session-id";
let registeredMnemopiState: MnemopiSessionState | undefined;
let tempDbDir: ReturnType<typeof TempDir.createSync> | undefined;
let tempDbPath: string | undefined;

function makeMnemopiConfig(
	overrides: (Partial<MnemopiBackendConfig> & Record<string, unknown>) | undefined = {},
): MnemopiBackendConfig {
	if (!tempDbPath) {
		tempDbDir = TempDir.createSync(`@mnemopi-test-${Date.now()}-`);
		tempDbPath = tempDbDir.join("mnemopi.db");
	}
	return {
		dbPath: tempDbPath,
		bank: "test-bank",
		autoRecall: true,
		autoRetain: true,
		polyphonicRecall: false,
		enhancedRecall: false,
		proactiveLinking: false,
		retainEveryNTurns: 3,
		retentionChunkMaxChars: 0,
		consolidateEveryNTurns: 0,
		recallLimit: 10,
		recallContextTurns: 1,
		recallMaxQueryChars: 800,
		injectionTokenLimit: 1024,
		debug: false,
		recallLengthNormalization: "none",
		recallScoreFloor: 0,
		providerOptions: {
			noEmbeddings: true,
			embeddingModel: undefined,
			embeddingApiUrl: undefined,
			embeddingApiKey: undefined,
			llm: false,
		},
		llmMode: "none",
		llmBaseUrl: undefined,
		llmApiKey: undefined,
		llmModel: undefined,
		...overrides,
	};
}

interface RegisterMnemopiStateOptions {
	cwd?: string;
	sessionId?: string;
	entries?: () => unknown[];
	listeners?: Set<AgentSessionEventListener>;
}

function registerMnemopiState(
	config?: MnemopiBackendConfig,
	options: RegisterMnemopiStateOptions = {},
): MnemopiSessionState {
	const finalConfig = config ?? makeMnemopiConfig();
	const sessionId = options.sessionId ?? TEST_SESSION_ID;
	registeredMnemopiState = new MnemopiSessionState({
		sessionId,
		config: finalConfig,
		session: {
			sessionId,
			settings: Settings.isolated({
				"memory.backend": "mnemopi",
				"mnemopi.noEmbeddings": true,
				"mnemopi.llmMode": "none",
			}),
			modelRegistry: {
				getApiKeyForProvider: async () => undefined,
				resolver: () => async () => undefined,
			} as never,
			sessionManager: {
				getEntries: options.entries ?? (() => []),
				getCwd: () => options.cwd ?? "/tmp",
			} as never,
			emitNotice: () => {},
			getHindsightSessionState: () => undefined,
			subscribe: (listener: AgentSessionEventListener) => {
				options.listeners?.add(listener);
				return () => options.listeners?.delete(listener);
			},
		} as never,
	});
	setMnemopiSessionState(registeredMnemopiState.session as never, registeredMnemopiState);
	return registeredMnemopiState;
}

/**
 * Regression: retention chunking wrote every chunk of one oversized message through
 * `rememberInScope` without an explicit id. The store dedupes by CONTENT, so two byte-identical
 * chunks (a long repeated payload) collapsed into a single row that kept only the FIRST chunk's
 * ranges -- the repeat was silently lost and exact reconstruction became impossible.
 *
 * This drives the real retention entry point and inspects the rows that actually land, so it fails
 * if `state.ts` stops supplying ids -- which a test of the id-derivation helper alone could not
 * detect.
 */
describe("retention chunking supplies per-chunk memory ids", () => {
	beforeEach(() => {
		resetSettingsForTest();
	});

	it("stores byte-identical chunks of one message as separate rows", async () => {
		// One oversized message whose chunks come out byte-identical. A uniform payload guarantees
		// that whatever the split offsets are: a multi-character unit only yields identical chunks
		// when the boundaries happen to land in phase with it, which would make this test vacuous.
		const state = registerMnemopiState(makeMnemopiConfig({ retentionChunkMaxChars: 500, bank: "chunk-ids-bank" }), {
			// Own session and bank: the temp database is shared across tests in this file, the retained
			// turn cursor is per session, and rows carry their bank in working_memory.session_id.
			sessionId: "chunk-ids-session",
			cwd: "/work/chunk-ids",
			entries: () => [{ type: "message", message: { role: "user", content: "x".repeat(3000) } }],
		});

		await state.forceRetainCurrentSession();

		const rows = state.memory.beam.db
			.prepare(`
				SELECT id, content, metadata_json
				FROM working_memory
				WHERE json_extract(metadata_json, '$.chunk_index') IS NOT NULL
				  AND session_id = 'chunk-ids-bank'
				ORDER BY json_extract(metadata_json, '$.chunk_index')
			`)
			.all() as { id: string; content: string; metadata_json: string }[];

		// The message is oversized, so it must have produced several chunk rows. Before the fix the
		// identical ones collapsed and this count came up short.
		expect(rows.length).toBeGreaterThan(1);
		expect(new Set(rows.map(row => row.id)).size).toBe(rows.length);

		// The point of the fix: chunks sharing identical text still occupy distinct rows...
		const byContent = new Map<string, number>();
		for (const row of rows) byContent.set(row.content, (byContent.get(row.content) ?? 0) + 1);
		expect(Math.max(...byContent.values())).toBeGreaterThan(1);

		// ...and each row kept its OWN ranges, which is what reconstruction slices by.
		const ranges = rows.map(row => JSON.stringify(JSON.parse(row.metadata_json).ranges));
		expect(new Set(ranges).size).toBe(rows.length);

		await state.dispose({ consolidate: false });
	});
});
