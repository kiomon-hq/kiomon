# MCP: an agent that drives the brain over stdio

Three ways in, from least to most code:

| File | Use |
|---|---|
| `agent-config.json` | Copy the `mcpServers` block into your MCP host's config. That is the whole setup. |
| `session.jsonl` | `kiomon mcp < examples/mcp/session.jsonl` — five frames, four answers, no client needed. |
| `session.py` | A real client: handshake, `tools/list`, `draft_memory`, `search_memory`, `retrieve_memory`, and a refusal. Run it to see what a host sees. |

```bash
# the raw stream, with a scratch brain
KIOMON_DB=/tmp/brain.db kiomon mcp < examples/mcp/session.jsonl

# the same thing through a client that can chain calls
python examples/mcp/session.py
```

## Adding it to a host

[`agent-config.json`](agent-config.json) is the block to copy. It is deliberately a valid config
rather than a commented one, because a client parses it:

```json
{
  "mcpServers": {
    "kiomon-local": {
      "command": "/REPLACE/WITH/ABSOLUTE/PATH/TO/kiomon",
      "args": ["mcp"],
      "env": { "KIOMON_DB": "/REPLACE/WITH/ABSOLUTE/PATH/TO/brain.db" }
    }
  }
}
```

Two things to change and one to know:

- **The path must be absolute.** A GUI host does not inherit your shell's `PATH`, and a server
  that fails to start usually shows up as a tool that "does not exist".
- **`env` is optional.** Without it the server resolves the database the way every other verb
  does: `$KIOMON_DB`, then the config file, then the platform data directory.
- On Windows the same block with `C:\Users\you\.local\bin\kiomon.exe` and a
  `C:\...` database path; JSON needs the double backslashes.

## Four things that surprise people

1. **There is no token, and that is the design.** An MCP client starts the process itself, so
   the process boundary is the permission boundary. `serve` needs a token because a socket is
   reachable by anything on the machine; stdio is not.
2. **stdout is protocol only.** Diagnostics go to stderr. If you ever see a non-frame line in a
   client's log, that is a server bug — and the reason `session.py` leaves stderr alone instead
   of folding it into stdout for tidiness.
3. **Tool results are rendered text, not JSON.** They are written for a model to read
   ("1 drafted: 1 active, 0 waiting for review"), so an id appears inside a line. Use the HTTP
   door (`kiomon serve`) when a program needs structured ids; use MCP when an agent needs to
   read.
4. **A refusal is a result.** `revert` answers `isError: true` with a reason — revision history
   is not kept locally — so an agent says "not available" instead of retrying, and the stream
   stays in sync because nothing at the protocol level failed.

## The tools

The hosted product's twelve, under the hosted names an agent's prompt already says
(`retrieve_memory`, `search_memory`, `get_memory`, `draft_memory`, `reflect_session`,
`record_outcome`, `manage_memory`, `explore_memory_graph`, `get_workspace_context`,
`list_workspaces`, `get_latest_briefing`, `select_workspace`), plus four this engine has verbs
for: `get_memory_relations`, `link_memories`, `unlink_memories`, `list_review_inbox`.

`list_review_inbox` exists because locally the review inbox is a real queue on the same machine
— an autonomous agent's low-confidence writes land there and a human clears them. On the hosted
product that queue is a page in the web app; here the agent can at least see it.
