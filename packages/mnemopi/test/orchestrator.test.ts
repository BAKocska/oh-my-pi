import { afterEach, describe, expect, it } from "bun:test";
import { type BeamMemoryState, initBeam, type RecallResult } from "@oh-my-pi/pi-mnemopi/core/beam";
import { recall as beamRecall, recallEnhanced as beamRecallEnhanced } from "@oh-my-pi/pi-mnemopi/core/beam/recall";
import {
	type OrchestratorBeam,
	OrchestratorQueryCache,
	orchestrateRecall,
} from "@oh-my-pi/pi-mnemopi/core/orchestrator";
import { PolyphonicRecallEngine } from "@oh-my-pi/pi-mnemopi/core/polyphonic-recall";
import { closeQuietly, openDatabase } from "@oh-my-pi/pi-mnemopi/db";

interface FakeBeam extends BeamMemoryState {
	linearCalls: number;
	enhancedCalls: number;
	recall: (query: string, topK?: number) => Promise<RecallResult[]>;
	recallEnhanced: (query: string, topK?: number) => Promise<RecallResult[]>;
}

function fakeBeam(): FakeBeam {
	const db = openDatabase(":memory:", { create: true, readwrite: true });
	initBeam(db);
	const beam: FakeBeam = {
		db,
		sessionId: "orchestrator-test",
		authorId: null,
		authorType: null,
		channelId: "orchestrator-test",
		useCloud: false,
		pluginManager: null,
		annotations: null,
		triples: null,
		episodicGraph: null,
		veracityConsolidator: null,
		caches: { timestampParse: new Map(), extractionBuffer: [] },
		config: {
			workingMemoryLimit: 1000,
			workingMemoryTtlHours: 24,
			recencyHalflifeHours: 72,
			vecWeight: 0.5,
			ftsWeight: 0.3,
			importanceWeight: 0.2,
			useCloud: false,
			localLlmEnabled: false,
			maxEpisodeChars: 100_000,
		},
		linearCalls: 0,
		enhancedCalls: 0,
		async recall(query: string, topK = 20): Promise<RecallResult[]> {
			this.linearCalls += 1;
			return [{ id: "linear", content: `${query}:${topK}`, score: 1 }];
		},
		async recallEnhanced(query: string, topK = 20): Promise<RecallResult[]> {
			this.enhancedCalls += 1;
			return [{ id: "enhanced", content: `${query}:${topK}`, score: 2 }];
		},
	};
	return beam;
}

function insertWorking(
	beam: BeamMemoryState,
	id: string,
	content: string,
	options: { sessionId?: string; scope?: string } = {},
): void {
	const now = new Date().toISOString();
	beam.db.run(
		`INSERT INTO working_memory
			(id, content, source, timestamp, session_id, importance, metadata_json, veracity, memory_type, scope, created_at)
			VALUES (?, ?, 'test', ?, ?, 0.8, '{}', 'unknown', 'unknown', ?, ?)`,
		[id, content, now, options.sessionId ?? beam.sessionId, options.scope ?? "global", now],
	);
}

const previousPolyphonic = process.env.MNEMOPI_POLYPHONIC_RECALL;
const previousEnhancedRecall = process.env.MNEMOPI_ENHANCED_RECALL;

afterEach(() => {
	if (previousPolyphonic === undefined) delete process.env.MNEMOPI_POLYPHONIC_RECALL;
	else process.env.MNEMOPI_POLYPHONIC_RECALL = previousPolyphonic;
	if (previousEnhancedRecall === undefined) delete process.env.MNEMOPI_ENHANCED_RECALL;
	else process.env.MNEMOPI_ENHANCED_RECALL = previousEnhancedRecall;
});

describe("orchestrateRecall", () => {
	it("delegates to the Beam linear recall surface when the polyphonic gate is off", async () => {
		const beam = fakeBeam();
		try {
			process.env.MNEMOPI_POLYPHONIC_RECALL = "0";
			const results = await orchestrateRecall(beam, "needle", 7);
			expect(results).toEqual([{ id: "linear", content: "needle:7", score: 1 }]);
			expect(beam.linearCalls).toBe(1);
			expect(beam.enhancedCalls).toBe(0);
		} finally {
			closeQuietly(beam.db);
		}
	});

	it("delegates to enhanced recall when requested on the non-polyphonic path", async () => {
		const beam = fakeBeam();
		try {
			delete process.env.MNEMOPI_POLYPHONIC_RECALL;
			const results = await orchestrateRecall(beam, "needle", 3, { enhanced: true });
			expect(results).toEqual([{ id: "enhanced", content: "needle:3", score: 2 }]);
			expect(beam.linearCalls).toBe(0);
			expect(beam.enhancedCalls).toBe(1);
		} finally {
			closeQuietly(beam.db);
		}
	});

	it("uses polyphonic recall instead of fake Beam recall when the gate is on", async () => {
		const beam = fakeBeam();
		try {
			const engine = new PolyphonicRecallEngine({ db: beam.db });
			insertWorking(beam, "m-poly", "Alice orchestrator polyphonic memory");
			beam.db.run(
				`INSERT INTO gists (id, text, timestamp, participants_json, memory_id)
					VALUES ('gist_m-poly', 'Alice orchestrator gist', ?, ?, 'm-poly')`,
				[new Date().toISOString(), JSON.stringify(["Alice"])],
			);
			beam.caches.polyphonicEngine = engine;
			process.env.MNEMOPI_POLYPHONIC_RECALL = "1";
			const results = await orchestrateRecall(beam, "Alice", 5);
			expect(beam.linearCalls).toBe(0);
			expect(beam.enhancedCalls).toBe(0);
			expect(results[0]?.id).toBe("m-poly");
			// Weighted RRF: graph contributes voiceWeights.graph / (RRF_K + rank).
			expect(results[0]?.voice_scores).toEqual({ graph: 0.55 / 61 });
		} finally {
			closeQuietly(beam.db);
		}
	});

	it("forceLinear bypasses the env gate for A/B callers", async () => {
		const beam = fakeBeam();
		try {
			process.env.MNEMOPI_POLYPHONIC_RECALL = "1";
			const results = await orchestrateRecall(beam, "needle", 2, { forceLinear: true });
			expect(results[0]?.id).toBe("linear");
			expect(beam.linearCalls).toBe(1);
		} finally {
			closeQuietly(beam.db);
		}
	});
});

describe("cacheDiscriminator visibility widening", () => {
	function visibilityBeam(sessionId: string): OrchestratorBeam & { close(): void } {
		const db = openDatabase(":memory:", { create: true, readwrite: true });
		initBeam(db);
		const beam: OrchestratorBeam & { close(): void } = {
			db,
			sessionId,
			authorId: null,
			authorType: null,
			channelId: sessionId,
			useCloud: false,
			pluginManager: null,
			annotations: null,
			triples: null,
			episodicGraph: null,
			veracityConsolidator: null,
			caches: { timestampParse: new Map(), extractionBuffer: [] },
			config: {
				workingMemoryLimit: 1000,
				workingMemoryTtlHours: 24,
				recencyHalflifeHours: 72,
				vecWeight: 0.5,
				ftsWeight: 0.3,
				importanceWeight: 0.2,
				useCloud: false,
				localLlmEnabled: false,
				maxEpisodeChars: 100_000,
			},
			// Wire the real linear recall path (not a stub) so `buildWhere`'s session-visibility
			// filter -- the thing this test is actually exercising -- runs for real.
			recall: (query, topK, options) => beamRecall(beam, query, topK, options),
			recallEnhanced: (query, topK, options) => beamRecallEnhanced(beam, query, topK, options),
			close() {
				closeQuietly(db);
			},
		};
		return beam;
	}

	it("never lets a visibility-widened call poison a session-scoped call's cache bucket", async () => {
		process.env.MNEMOPI_ENHANCED_RECALL = "1";
		process.env.MNEMOPI_POLYPHONIC_RECALL = "0";
		const beam = visibilityBeam("session-a");
		try {
			// One beam, two sessions' rows, neither `global`-scoped -- session-a's own recall must
			// never see session-b's row unless a call explicitly widens visibility.
			insertWorking(beam, "a-1", "zylophant migration checklist engineering rollout", {
				sessionId: "session-a",
				scope: "session",
			});
			insertWorking(beam, "b-1", "zylophant migration checklist finance rollout", {
				sessionId: "session-b",
				scope: "session",
			});

			const query = "zylophant migration checklist rollout";
			// `queryEmbedding: null` opts out of auto-embedding so this only exercises the FTS +
			// `buildWhere` visibility path, not tier2/3 cosine matching.
			const scopedOptions = { queryEmbedding: null } as const;

			const scoped = await orchestrateRecall(beam, query, 5, scopedOptions);
			expect(scoped.map(result => result.id)).toEqual(["a-1"]);

			// Same query and topK as the scoped call above -- only `ignoreSessionScope` differs --
			// which is exactly the shape a poisoned shared bucket could not tell apart pre-fix.
			const widened = await orchestrateRecall(beam, query, 5, { ...scopedOptions, ignoreSessionScope: true });
			expect(widened.map(result => result.id).sort()).toEqual(["a-1", "b-1"]);

			const cache = beam.caches.queryCache;
			if (!(cache instanceof OrchestratorQueryCache)) throw new Error("expected an OrchestratorQueryCache");
			// Two distinct discriminators must mean two distinct physical buckets, each holding its
			// own entry for the identical query text.
			expect(cache.stats().size).toBe(2);

			// Repeating the scoped call byte-identically must still be a cache hit, and it must
			// still be scoped to session-a only -- unaffected by the widened call in between.
			const repeat = await orchestrateRecall(beam, query, 5, scopedOptions);
			expect(repeat).toEqual(scoped);
			expect(cache.stats().hits).toBeGreaterThanOrEqual(1);
		} finally {
			beam.close();
		}
	});

	it("separates cache buckets by length-normalization mode", async () => {
		process.env.MNEMOPI_ENHANCED_RECALL = "1";
		process.env.MNEMOPI_POLYPHONIC_RECALL = "0";
		const beam = visibilityBeam("session-a");
		try {
			insertWorking(beam, "long", `quokka protocol ${"background ".repeat(500)}`, {
				sessionId: "session-a",
				scope: "session",
			});
			insertWorking(beam, "short", "quokka protocol concise answer", {
				sessionId: "session-a",
				scope: "session",
			});
			const common = { queryEmbedding: null } as const;

			await orchestrateRecall(beam, "quokka protocol", 2, {
				...common,
				lengthNormalization: "none",
			} as never);
			await orchestrateRecall(beam, "quokka protocol", 2, {
				...common,
				lengthNormalization: "log",
			} as never);
			await orchestrateRecall(beam, "quokka protocol", 2, {
				...common,
				lengthNormalization: "log",
				scoreFloor: 1,
			} as never);

			const cache = beam.caches.queryCache;
			if (!(cache instanceof OrchestratorQueryCache)) throw new Error("expected an OrchestratorQueryCache");
			expect(cache.stats().size).toBe(3);
		} finally {
			beam.close();
		}
	});
});
