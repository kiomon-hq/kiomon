# Examples

Working code, not sketches. Every file here was run against a build of this repository, and
the output quoted in the comments is the output it printed. Start with the CLI tour, then pick
the door you actually want to call from.

| Example | What it teaches | Needs |
|---|---|---|
| [`cli-tour.sh`](cli-tour.sh) | Install-less setup: `init`, `add`, `search`, `retrieve`, `link`, `review`, `decay`, `export`. The whole lifecycle in one script, on a throwaway brain. | `bash`, the `kiomon` binary |
| [`curl.sh`](curl.sh) | The HTTP wire with no SDK and no dependency: start `serve`, authenticate, `POST /api/retrieve`, `GET /api/search`. | `curl`, the `kiomon` binary |
| [`python/`](python/) | The Python SDK against a local brain: `retrieve_memory`, `draft_memory`, `search_memory`, `manage_memory`, `record_outcome`. | Python 3.10+, the `kiomon` package |
| [`typescript/`](typescript/) | The same, from Node: the TypeScript SDK pointed at `127.0.0.1`. | Node 18+, `@kiomon/kiomon` |
| [`mcp/`](mcp/) | An agent that drives the brain through MCP over stdio — the config to paste into a client, and a raw JSON-RPC session you can pipe in. | any MCP client, or `bash` |
| [`../crates/kiomon-core/examples/quickstart.rs`](../crates/kiomon-core/examples/quickstart.rs) | The Rust SDK, which is `kiomon-core` itself — in-process, no socket, no server. | the Rust toolchain |

## 1. Set up the engine

One binary, one SQLite file, no account and no network. Pick one:

```bash
# macOS and Linux
curl -LsSf https://github.com/kiomon-hq/kiomon/releases/latest/download/install.sh | sh

# Windows PowerShell
irm https://github.com/kiomon-hq/kiomon/releases/latest/download/install.ps1 | iex

# or from source, which is what this repository builds
cargo install --path crates/kiomon-cli
```

Then create a brain and check it:

```bash
kiomon init
kiomon doctor
```

`init` creates the database and a `default` workspace. `doctor` prints the paths it resolved
(database, config, schema version, embedder) — it is the first thing to run when something
looks wrong, because almost every "why is this empty" is the command and the query looking at
two different files.

**Where the brain lives.** `--db <path>`, then `$KIOMON_DB`, then the config file, then the
platform data directory (`~/.local/share/kiomon/brain.db` on Linux,
`%APPDATA%\kiomon\brain.db` on Windows, `~/Library/Application Support/kiomon` on macOS).
The order is fixed and `doctor` tells you which one won.

**Semantic search is opt-in and bring-your-own.** A brain with no embedder is a full-text
brain and says so on stderr rather than pretending. To add one, write a config file —
`kiomon doctor` prints its path — with:

```toml
default_workspace = "default"

[embedder]
provider = "ollama"                 # or "openai"
model = "nomic-embed-text"
base_url = "http://127.0.0.1:11434"
```

then `kiomon reindex --embed` to embed what is already stored. From then on writes embed
themselves and `search` fuses full text with meaning.

## 2. The doors into the same brain

| Door | Command | Use it when |
|---|---|---|
| CLI | `kiomon search "deploy"` | a human, a shell script, or an agent with a shell |
| HTTP | `kiomon serve` then the SDK or `curl` | an SDK, a browser-adjacent tool, or a process that already speaks HTTP |
| MCP | `kiomon mcp` | an agent client that speaks MCP (Claude Code, Cursor, any MCP host) |
| In-process | `kiomon-core` | Rust code that wants the store without a process boundary |

All four call the same functions over the same file. The CLI is the reference: whatever the
socket answers, the command answers, and the tests assert it.

## 3. Connecting an SDK to a local brain

The published SDKs default to `https://api.kiomon.com`. Pointing one at `127.0.0.1` is two
lines — `baseUrl`, and the token that `serve` printed:

```bash
kiomon serve --port 0 --json      # prints {"url": "...", "token_file": "..."} on one line
```

```python
from kiomon import Kiomon
from pathlib import Path

client = Kiomon(
    api_key=Path(token_file).read_text().strip(),   # the local token, sent as X-API-Key
    base_url="http://127.0.0.1:53412",
    workspace_id="default",
)
```

The Python and TypeScript examples do exactly this, including starting `serve` themselves on
an ephemeral port with a throwaway brain, so `python examples/python/quickstart.py` works from
a clean checkout with nothing running. Set `KIOMON_URL` and `KIOMON_TOKEN` to use a server you
already have instead.

## 4. What to remember, and what to expect back

The lifecycle the examples demonstrate, in the order a real agent uses it:

1. **Write** — `add` from the CLI, `draft_memory` from an SDK. A user's own save activates
   immediately; an agent's reflection at confidence >= 0.75 activates, below that it waits in
   the review inbox (`kiomon review list`).
2. **Retrieve** — `retrieve` / `retrieve_memory` returns a *packet*: facts, at most one
   procedure, episodes, evidence and warnings, ranked by intent (`execute`, `plan`, `answer`,
   `verify`, `debug`). This is the call to make before acting, not `search`.
3. **Act**, then **record the outcome** — `record_outcome` with `helpful`, `unused`,
   `misleading` or `outdated` per memory. This is what moves `confidence` and what stops a
   memory that never helped from staying at the top forever.
4. **Forget** — `decay` ages strength on a half-life per kind and archives what falls below
   0.05; `pin` exempts a memory from that; `forget` deletes it and its edges.

Putting a packet in front of a model and never reporting the outcome is the one thing that
makes a brain worse over time rather than better.

## Running everything

```bash
cargo build --release

bash   examples/cli-tour.sh
bash   examples/curl.sh
python examples/python/quickstart.py
python examples/python/agent_loop.py
node   examples/typescript/quickstart.mjs
bash   examples/mcp/session.sh
cargo run --example quickstart -p kiomon-core
```

Each script finds the binary at `target/release/kiomon`, or on `PATH`, or wherever
`KIOMON_BIN` points, and each one uses a throwaway database under a temp directory unless you
tell it otherwise. Nothing here touches your real brain.
