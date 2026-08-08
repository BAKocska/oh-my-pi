import { afterEach, expect, it } from "bun:test";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";
import { loadCustomCommands } from "../../src/extensibility/custom-commands/loader";

let tempRoot: string | undefined;

afterEach(async () => {
	if (tempRoot) {
		await fs.rm(tempRoot, { recursive: true, force: true });
		tempRoot = undefined;
	}
});

it("preserves the omptype module namespace in the custom command API", async () => {
	tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "omp-custom-command-loader-"));
	const commandDir = path.join(tempRoot, "commands", "namespace-schema");
	await fs.mkdir(commandDir, { recursive: true });
	await Bun.write(
		path.join(commandDir, "index.ts"),
		`export default api => {
	const schema = api.arktype.type({ note: api.arktype.type("string") });
	const parsed = schema.assert({ note: "schema built through namespace" });
	return {
		name: "namespace_schema",
		description: parsed.note,
		async execute() {
			const next = schema.assert({ note: "ok" });
			return next.note;
		},
	};
};`,
	);

	const result = await loadCustomCommands({ cwd: tempRoot, agentDir: tempRoot });
	const loaded = result.commands.find(entry => entry.command.name === "namespace_schema");

	expect(result.errors).toEqual([]);
	expect(loaded?.command.description).toBe("schema built through namespace");
});
