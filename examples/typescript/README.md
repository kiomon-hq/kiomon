# TypeScript SDK against a local brain

```bash
npm install                # inside examples/typescript, for @kiomon/kiomon from the registry
node examples/typescript/quickstart.mjs
```

`npm install` is optional in this repository: the script looks for the published package first
and falls back to the sibling SDK checkout at `../../SDK/kiomon-ts/sdk/dist`, so it runs with
nothing installed. Set `KIOMON_TS_SDK` to an explicit `dist/index.js` if your layout differs.

Like the Python example, it starts `kiomon serve` itself on an ephemeral port with a throwaway
database and stops it on exit. Set `KIOMON_URL` to use a server that is already running. There is
no key to fetch: the SDK requires `apiKey`, the local server ignores its value.

## The two lines that matter

```ts
import { Kiomon } from "@kiomon/kiomon";

const kiomon = new Kiomon({
	apiKey: "local",                     // required by the SDK, ignored by the local server
	baseUrl: "http://127.0.0.1:53412",
	workspaceId: "default",
});
```

## What the example walks through

1. **The connection** — `listWorkspaces`, then `selectWorkspace`, so the workspace ambiguity is
   resolved explicitly rather than guessed.
2. **Save** — `draftMemory` twice: one at `confidence: 0.9` (activates) and one at `0.4` (held
   for review). The answer's `promoted`/`pending` is the truth about the write.
3. **Search** — `searchMemory`, and `JSON.parse(hit.tags)` because `tags` is a JSON string on
   the wire, as both published SDKs document.
4. **Retrieve** — `retrieveMemory` for the packet: facts, one procedure, warnings.
5. **Read and manage** — `getMemory`, `manageMemory` (`pin`, `rate`).
6. **Report** — `recordOutcome`, which is what lets the brain re-rank honestly later.
7. **The graph** — `exploreMemoryGraph`, and a `depth: 2` call that refuses with
   `501 unsupported_locally` instead of returning a smaller graph.

## Notes

- `pinned` is an integer (`0`/`1`), matching the hosted row; `document.pinned === 1`, not `true`.
- A failed tool is a `KiomonError` with a `status` and a `code`; the local server's codes are
  the hosted ones, plus `unsupported_locally` for the routes a file-backed engine cannot serve.
