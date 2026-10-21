/**
 * The TypeScript SDK's conformance run — the second of the two published clients, against the
 * same local `serve`.
 *
 * The Python harness proves the field shapes; this one proves the parts only a second
 * implementation can: that its URL and query assembly (`kind` as a comma list, `center_id`,
 * `legs`) parses on the other side, and that its `fetch` path — headers, `Content-Type`,
 * retries, `204`/empty-body handling — survives a real socket rather than a stub.
 *
 * Run it where the SDK is built:
 *
 *     node kiomon/scripts/sdk_conformance_ts.mjs \
 *         --client ../SDK/kiomon-ts/sdk/dist/index.js --bin kiomon/target/release/kiomon
 *
 * It is deliberately dependency-free (node: builtins only) so it runs anywhere the SDK does.
 */

import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const failures = [];
const check = (ok, what) => {
	if (!ok) failures.push(what);
};
const required = (body, fields, where) => {
	for (const field of fields) check(field in body, `${where} is missing \`${field}\``);
};

const args = process.argv.slice(2);
const argOf = (name, fallback) => {
	const index = args.indexOf(`--${name}`);
	return index === -1 ? fallback : args[index + 1];
};
const here = dirname(fileURLToPath(import.meta.url));
const binary = resolve(argOf("bin", join(here, "..", "target", "release", "kiomon")));
// The SDKs live beside this project (`MEMORY-INFRASTRUCTURE/SDK`), one level above the
// `KIOMON` directory this repository sits in — so the default climbs out of both.
const clientPath = resolve(argOf("client", join(here, "..", "..", "..", "SDK", "kiomon-ts", "sdk", "dist", "index.js")));

const home = mkdtempSync(join(tmpdir(), "kiomon-conformance-ts-"));
const config = join(home, "config.toml");
writeFileSync(config, "", "utf8");
const env = { ...process.env, KIOMON_DB: join(home, "brain.db"), KIOMON_CONFIG: config };

const runBinary = (extra) =>
	new Promise((done, fail) => {
		const child = spawn(binary, extra, { env, stdio: ["ignore", "pipe", "pipe"] });
		let stderr = "";
		child.stderr.on("data", (chunk) => {
			stderr += chunk;
		});
		child.on("close", (code) => (code === 0 ? done() : fail(new Error(`${extra.join(" ")}: ${stderr}`))));
	});

const main = async () => {
	// One memory with a tag and one without: the tagless one is what catches a `tags` field
	// that is absent instead of `"[]"`.
	await runBinary(["add", "--kind", "semantic", "--title", "Staging deploys need a migration", "--body", "Run migrations before deploying to staging.", "--confidence", "0.9", "--tag", "deploy", "-q"]);
	await runBinary(["add", "--kind", "procedural", "--title", "How to deploy staging", "--body", "Migrate, then deploy.", "--confidence", "0.85", "-q"]);

	const server = spawn(binary, ["serve", "--port", "0", "--json", "-q"], { env, stdio: ["ignore", "pipe", "ignore"] });
	const url = await new Promise((done, fail) => {
		let buffer = "";
		const timer = setTimeout(() => fail(new Error("serve printed no handshake")), 15_000);
		server.stdout.on("data", (chunk) => {
			buffer += chunk;
			const newline = buffer.indexOf("\n");
			if (newline !== -1) {
				clearTimeout(timer);
				done(JSON.parse(buffer.slice(0, newline)).url);
			}
		});
	});
	// The SDK demands a credential; a local server ignores its value. `local` is the
	// documented placeholder rather than a secret.
	const token = "local";

	try {
		const { Kiomon, KiomonError } = await import(pathToFileURL(clientPath).href);
		const client = new Kiomon({ apiKey: token, baseUrl: url });

		// `listWorkspaces` is the SDK's own default-resolution path: one workspace resolves
		// silently, and `pickWorkspace` reads `id` and `slug` off the row.
		const listed = await client.listWorkspaces();
		required(listed, ["workspaces"], "GET /api/workspaces");
		check(listed.workspaces.length === 1, `expected one local workspace, got ${listed.workspaces.length}`);
		const workspace = listed.workspaces[0];
		required(workspace, ["id", "user_id", "name", "slug", "created_at"], "Workspace");
		const selected = new Kiomon({ apiKey: token, baseUrl: url, workspaceId: workspace.id });

		const found = await selected.searchMemory({ query: "deploy", limit: 5 });
		required(found, ["results"], "GET /api/search");
		check(found.results.length === 2, `expected both seeded rows, got ${found.results.length}`);
		const hit = found.results[0];
		required(hit, ["id", "title", "source_url", "excerpt", "snippet", "tags", "created_at"], "SearchHit");
		check(typeof hit.tags === "string", `SearchHit.tags should be a JSON string, got ${typeof hit.tags}`);
		check(typeof hit.source_url === "string", "SearchHit.source_url should be a string");
		try {
			check(Array.isArray(JSON.parse(hit.tags)), "SearchHit.tags must decode to an array");
		} catch (error) {
			check(false, `SearchHit.tags is not decodable JSON: ${error}`);
		}

		const document = await selected.getMemory(hit.id);
		required(
			document,
			["id", "source_url", "title", "author", "excerpt", "tags", "user_notes", "body", "pinned", "created_at", "workspace_id", "source", "kind", "confidence", "corroboration_count", "importance", "strength", "retrieval_count", "last_retrieved_at", "feedback_score", "feedback_count", "scope", "status", "expires_at"],
			"MemoryDocument",
		);
		check(typeof document.pinned === "number", `MemoryDocument.pinned should be a number, got ${typeof document.pinned}`);
		check(typeof document.tags === "string", `MemoryDocument.tags should be a JSON string, got ${typeof document.tags}`);
		check(typeof document.scope === "string", "MemoryDocument.scope should be a string");

		// The pin path sends `{ pinned: 1 }`, and the route reads an integer.
		await selected.manageMemory({ id: hit.id, action: "pin" });
		const pinned = await selected.getMemory(hit.id);
		check(pinned.pinned === 1, `pinned should be 1 after a pin, got ${pinned.pinned}`);
		await selected.manageMemory({ id: hit.id, action: "unpin" });

		// `kind` as a single value and as a list: the two spellings the SDK encodes differently.
		const one = await selected.searchMemory({ query: "deploy", kind: "semantic" });
		check(one.results.length >= 1, "a single-kind search should still find the semantic row");
		const several = await selected.searchMemory({ query: "deploy", kind: ["semantic", "procedural"] });
		check(several.results.length >= 2, "a kind-list search should find both rows");

		const graph = await selected.exploreMemoryGraph({ id: hit.id });
		required(graph, ["nodes", "edges"], "GraphResult");
		check(graph.nodes.some((node) => node.id === hit.id), "the graph should contain the centre node");
		check(Array.isArray(graph.entityLinks), "GraphResult should carry `entityLinks`, even empty");

		const packet = await selected.retrieveMemory({ intent: "execute", query: "deploy staging" });
		required(packet, ["summary", "facts", "procedure", "episodes", "evidence", "warnings", "conflicts", "retrieved_ids"], "ContextPacket");
		if (packet.procedure) {
			required(packet.procedure, ["memory_id", "kind", "claim", "confidence", "authority", "applicability"], "PacketItem");
		}

		await selected.recordOutcome({
			outcome: "success",
			items: packet.retrieved_ids.slice(0, 1).map((memory_id) => ({ memory_id, assessment: "helpful" })),
		});

		const drafted = await selected.reflectSession({
			learnings: [{ kind: "semantic", title: "Conformance run wrote this", body: "A learning from the TypeScript harness.", confidence: 0.9, tags: ["conformance"], source_refs: [hit.id] }],
		});
		required(drafted, ["drafted", "promoted", "pending", "memories"], "BatchDraftResult");

		const context = await selected.getWorkspaceContext({ topic: "deploy" });
		check(context && typeof context === "object", "getWorkspaceContext should answer an object");

		// What cannot be answered is refused by name, not as an empty success.
		for (const [label, call] of [
			["briefings", () => selected.getLatestBriefing()],
			["revert", () => selected.manageMemory({ id: "k_01ARRZ0W6M3CQ8Y5X0Z4T9V1Q2", action: "revert" })],
		]) {
			try {
				await call();
				failures.push(`${label} answered instead of refusing`);
			} catch (error) {
				check(error instanceof KiomonError, `${label} should raise KiomonError`);
				check(error.code === "unsupported_locally", `${label} should refuse with unsupported_locally, got ${error.code}`);
			}
		}

		// The door, from the SDK's own fetch path. No credential is demanded by default --
		// a local brain has no accounts to check a key against -- so the SDK's placeholder is
		// ignored and a bare request is answered. What is still refused is the browser: a
		// foreign Origin, before routing. (`--require-token` adds 401 for a shared machine.)
		for (const [label, headers, expected] of [
			["no credential at all", {}, 200],
			["the SDK's placeholder", { "X-API-Key": "local" }, 200],
			["foreign Origin", { Origin: "http://evil.example" }, 403],
		]) {
			const response = await fetch(`${url}/api/workspaces`, { headers });
			check(response.status === expected, `${label} should be ${expected}, got ${response.status}`);
		}
	} finally {
		server.kill();
	}

	if (failures.length > 0) {
		console.error(`\n${failures.length} divergence(s) (working directory: ${home}):`);
		for (const line of failures) console.error(`  - ${line}`);
		process.exitCode = 1;
		return;
	}
	console.log("the TypeScript SDK's declared wire and the local engine agree");
};

main().catch((error) => {
	console.error(error);
	process.exitCode = 2;
});
