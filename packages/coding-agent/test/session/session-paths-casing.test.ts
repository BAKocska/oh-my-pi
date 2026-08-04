import { afterEach, describe, expect, it } from "bun:test";
import * as fs from "node:fs";
import * as path from "node:path";
import { computeDefaultSessionDir } from "@oh-my-pi/pi-coding-agent/session/session-paths";
import { FileSessionStorage } from "@oh-my-pi/pi-coding-agent/session/session-storage";
import { TempDir } from "@oh-my-pi/pi-utils";

const originalPlatform = process.platform;
const storage = new FileSessionStorage();

function setPlatform(value: NodeJS.Platform): void {
	Object.defineProperty(process, "platform", { value, configurable: true, writable: true });
}

afterEach(() => {
	Object.defineProperty(process, "platform", { value: originalPlatform, configurable: true, writable: true });
});

describe("computeDefaultSessionDir Windows drive-letter/path casing", () => {
	it("maps case-variant paths of one directory to a single win32 session bucket", () => {
		using tmp = TempDir.createSync("omp-session-casing-");
		const sessionsRoot = path.join(tmp.path(), "sessions");
		// cwd outside home/tmp so both variants classify as "abs" like a real drive path.
		const upper = path.join(path.sep, "omp-casing", "Code", "zig");
		const lower = path.join(path.sep, "omp-casing", "code", "zig");

		setPlatform("win32");
		const bucketUpper = computeDefaultSessionDir(upper, storage, sessionsRoot);
		const bucketLower = computeDefaultSessionDir(lower, storage, sessionsRoot);

		expect(bucketUpper).toBe(bucketLower);
	});

	it("keeps macOS/Linux buckets case-sensitive", () => {
		using tmp = TempDir.createSync("omp-session-casing-");
		const sessionsRoot = path.join(tmp.path(), "sessions");
		const upper = path.join(path.sep, "omp-casing", "Code", "zig");
		const lower = path.join(path.sep, "omp-casing", "code", "zig");

		setPlatform("linux");
		const bucketUpper = computeDefaultSessionDir(upper, storage, sessionsRoot);
		const bucketLower = computeDefaultSessionDir(lower, storage, sessionsRoot);

		expect(bucketUpper).not.toBe(bucketLower);
	});

	it("migrates a pre-fix case-preserving bucket into the folded win32 bucket", () => {
		using tmp = TempDir.createSync("omp-session-casing-");
		const sessionsRoot = path.join(tmp.path(), "sessions");
		const cwd = path.join(path.sep, "omp-casing", "Embedded");

		// A bucket written before the fold matches the (unchanged) non-win32 key.
		setPlatform("linux");
		const legacyDir = computeDefaultSessionDir(cwd, storage, sessionsRoot);
		const sessionName = "2026-01-01T00-00-00-000Z_sess.jsonl";
		fs.writeFileSync(path.join(legacyDir, sessionName), `${JSON.stringify({ type: "session", id: "sess", cwd })}\n`);

		setPlatform("win32");
		const foldedDir = computeDefaultSessionDir(cwd, storage, sessionsRoot);

		expect(foldedDir).not.toBe(legacyDir);
		expect(fs.existsSync(path.join(foldedDir, sessionName))).toBe(true);
		expect(fs.existsSync(legacyDir)).toBe(false);
	});
});
