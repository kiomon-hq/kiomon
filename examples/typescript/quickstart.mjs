#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
//
// The TypeScript SDK against a local brain.
//
//   node examples/typescript/quickstart.mjs
//
// No `npm install` needed in this repository: the script finds the SDK at
// ../../SDK/kiomon-ts/sdk/dist when the published package is not installed. Run
// `npm install` in this directory to use `@kiomon/kiomon` from the registry instead.
//
// Set KIOMON_URL and KIOMON_TOKEN to use a server that is already running; with neither set,
// the script starts `kiomon serve --port 0` itself with a throwaway database and stops it on
// exit.

import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repository = resolve(here, "../..");

// -- The SDK -------------------------------------------------------------------
// Try the installed package first - that is what a user following the README has - then the
// sibling checkout this workspace happens to have. `import()` rather than a static import,
// because the specifier that works depends on the environment.
async function loadSdk() {
	const candidates = [
		"@kiomon/kiomon",
		join(repository, "../../SDK/kiomon-ts/sdk/dist/index.js"),
		join(repository, "../SDK/kiomon-ts/sdk/dist/index.js"),
	];
	for (const candidate of candidates) {
		try {
			return await import(candidate.startsWith("@") ? candidate : `file://${candidate}`);
		} catch (error) {
			if (error?.code !== "ERR_MODULE_NOT_FOUND") throw error;
		}
	}
	throw new Error(
		"could not load @kiomon/kiomon. Run `npm install` in examples/typescript, or set KIOMON_TS_SDK to the SDK's dist/index.js.",
	);
}

const { Kiomon, KiomonError } = await loadSdk();

// -- The binary ----------------------------------------------------------------
function binary() {
	if (process.env.KIOMON_BIN) return process.env.KIOMON_BIN;
	for (const name of ["kiomon.exe", "kiomon"]) {
		try {
			const path = join(repository, "target", "release", name);
			readFileSync(path).subarray(0, 1); // exists and readable
			return path;
		} catch {
			// try the next one
		}
	}
	return "kiomon"; // resolved on PATH
}

// -- The brain -----------------------------------------------------------------
// Returns a client and a `close()` that also stops a server this script started.
async function brain() {
	const explicitUrl = process.env.KIOMON_URL;
	const explicitToken = process.env.KIOMON_TOKEN;
	if (explicitUrl && explicitToken) {
		return {
			client: new Kiomon({ apiKey: explicitToken, baseUrl: explicitUrl, workspaceId: "default" }),
			url: explicitUrl,
			close: () => {},
		};
	}

	const directory = mkdtempSync(join(tmpdir(), "kiomon-example-"));
	const database = join(directory, "brain.db");
	const child = spawn(binary(), ["serve", "--port", "0", "--json"], {
		env: { ...process.env, KIOMON_DB: database },
		stdio: ["ignore", "pipe", "ignore"], // `serve` narrates on stderr; this script does its own
	});

	// `serve` prints one JSON line before it accepts requests, so reading the first line
	// cannot race the server.
	const announcement = await new Promise((resolvePromise, rejectPromise) => {
		let buffer = "";
		const timer = setTimeout(() => rejectPromise(new Error("`kiomon serve` did not announce a port")), 10_000);
		child.stdout.on("data", (chunk) => {
			buffer += chunk.toString();
			const newline = buffer.indexOf("\n");
			if (newline === -1) return;
			clearTimeout(timer);
			resolvePromise(JSON.parse(buffer.slice(0, newline)));
		});
		child.on("exit", (code) => {
			clearTimeout(timer);
			rejectPromise(new Error(`\`kiomon serve\` exited with ${code} before announcing a port`));
		});
	});

	const token = readFileSync(announcement.token_file, "utf8").trim();
	console.log(`  brain     ${database}`);
	console.log(`  serve     ${announcement.url}`);

	return {
		client: new Kiomon({ apiKey: token, baseUrl: announcement.url, workspaceId: "default" }),
		url: announcement.url,
		close: () => {
			child.kill();
			rmSync(directory, { recursive: true, force: true });
		},
	};
}

function section(title) {
	console.log(`\n-- ${title} ${"-".repeat(Math.max(0, 62 - title.length))}`);
}

async function main() {
	const { client, url, close } = await brain();
	try {
		section("1. The connection");
		// The SDK defaults to https://api.kiomon.com. Pointing it at a local server changes the
		// host and nothing else: paths, bodies, retries and error codes are the hosted contract.
		console.log(`  base_url  ${url}`);
		const { workspaces } = await client.listWorkspaces();
		console.log(`  workspace ${workspaces[0].id} (${workspaces[0].name}), slug=${JSON.stringify(workspaces[0].slug)}`);
		client.selectWorkspace(workspaces[0].id);

		section("2. Save: an agent reflection");
		// `draftMemory` goes through the write gate. Confidence >= 0.75 on an autonomous
		// workspace activates; below that it waits for review. The answer says which - read
		// `promoted`/`pending` rather than assuming the write is live.
		const drafted = await client.draftMemory({
			kind: "procedural",
			title: "Roll back a bad deploy",
			body: "Promote the previous tag, then run the migrations that tag expects.",
			excerpt: "Previous tag first, migrations second.",
			tags: ["deploy", "rollback"],
			confidence: 0.9,
		});
		console.log(`  promoted  ${JSON.stringify(drafted.promoted)}`);
		console.log(`  pending   ${JSON.stringify(drafted.pending)}`);
		const rollbackId = (drafted.promoted[0] ?? drafted.pending[0]);

		const uncertain = await client.draftMemory({
			kind: "semantic",
			title: "The worker retries twice",
			body: "Unverified: a failed job appears to retry twice before dead-lettering.",
			tags: ["worker"],
			confidence: 0.4,
		});
		console.log(`  held back ${JSON.stringify(uncertain.pending)}`);

		section("3. Query: search");
		// `tags` is a JSON string on the wire - both published SDKs document it that way, and
		// the hosted column is TEXT holding the array. One `JSON.parse`, every time.
		const found = await client.searchMemory({ query: "deploy", limit: 10 });
		for (const hit of found.results) {
			const tags = hit.tags ? JSON.parse(hit.tags) : [];
			console.log(`  ${hit.id}  ${hit.kind.padEnd(10)} ${hit.title}  ${JSON.stringify(tags)}`);
		}
		console.log(`  legs      ${JSON.stringify(found.legs)}  (vector false: no embedder configured)`);

		section("4. Query: retrieve - the packet");
		// The call to make before acting. Facts, at most one procedure, episodes, evidence and
		// warnings - ranked by the intent, not by keyword overlap.
		const packet = await client.retrieveMemory({ intent: "execute", query: "how do I roll back a bad deploy?" });
		console.log(`  summary   ${packet.summary}`);
		for (const fact of packet.facts) {
			console.log(`  fact      ${fact.memory_id}  conf ${fact.confidence.toFixed(2)}  ${fact.claim}`);
		}
		if (packet.procedure) console.log(`  procedure ${packet.procedure.memory_id}  ${packet.procedure.claim}`);
		console.log(`  warnings  ${packet.warnings.length ? packet.warnings.join("; ") : "none"}`);

		section("5. Read and manage one memory");
		const document = await client.getMemory(rollbackId);
		console.log(`  ${document.title}  [${document.kind}, ${document.status}]`);
		console.log(`  pinned    ${document.pinned}  (an integer: 0 or 1, as the hosted row stores it)`);
		await client.manageMemory({ id: rollbackId, action: "pin" });
		await client.manageMemory({ id: rollbackId, action: "rate", value: 1 });
		const after = await client.getMemory(rollbackId);
		console.log(`  after     pinned ${after.pinned}, feedback ${after.feedback_score.toFixed(2)} over ${after.feedback_count}`);

		section("6. Report the outcome");
		// Skip this and nothing can re-rank honestly later: it is the half of the loop that
		// most integrations leave out.
		const outcome = await client.recordOutcome({
			outcome: "success",
			items: [
				{ memory_id: rollbackId, assessment: "helpful" },
				{ memory_id: uncertain.pending[0] ?? rollbackId, assessment: "unused" },
			],
		});
		console.log(`  updated   ${outcome.updated.length} memory(ies)`);

		section("7. The graph, and the honest gap");
		const graph = await client.exploreMemoryGraph({ id: rollbackId });
		console.log(`  nodes     ${graph.totalNodes}  edges ${graph.edges.length}`);
		try {
			await client.exploreMemoryGraph({ id: rollbackId, depth: 2 });
		} catch (error) {
			// A route this engine cannot answer says so with a code, never an empty 200 - the
			// difference between "no neighbours" and "not implemented yet".
			if (error instanceof KiomonError) console.log(`  depth 2   refused: ${error.status} ${error.code}`);
			else throw error;
		}
	} finally {
		close();
	}

	console.log("\nDone. The CLI sees this same brain: examples/cli-tour.sh is the file-level view.");
}

await main();
