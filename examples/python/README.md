# Python SDK against a local brain

```bash
pip install kiomon                      # the published SDK
python examples/python/quickstart.py    # save, search, retrieve, report
python examples/python/agent_loop.py    # retrieve -> act -> report, with the write gate
```

Neither script needs anything running. Each starts `kiomon serve` on an ephemeral port with a
throwaway database, points the real SDK at it, and stops it on exit. In this repository they
also find the SDK at `../../SDK/kiomon-py/src` when `kiomon` is not installed, so the scripts
run from a clean checkout.

## The two lines that matter

```python
from kiomon import Kiomon

client = Kiomon(
    api_key=token,                   # the token from the file `serve` printed
    base_url="http://127.0.0.1:53412",
    workspace_id="default",
)
```

Everything else is the published SDK: `retrieve_memory`, `search_memory`, `draft_memory`,
`manage_memory`, `record_outcome`, `explore_memory_graph`. The local server answers the hosted
`/api/*` routes, so the client's paths, bodies, retry policy and error codes are unchanged.

## Files

| File | What it is |
|---|---|
| `local.py` | The 60 lines that start a server and load its token. Copy it, or replace it with two constants. |
| `quickstart.py` | The API surface, in order: connection, two kinds of write, search, packet, read, feedback, lifecycle, graph. |
| `agent_loop.py` | The integration pattern: render a packet into a prompt, report the outcome per memory, and let a low-confidence write wait for a human. |

## Using a server that is already running

```bash
# terminal 1
kiomon serve --port 0 --json      # prints {"url": ..., "token_file": ...}

# terminal 2
export KIOMON_URL=http://127.0.0.1:53412
export KIOMON_TOKEN=$(cat /path/to/serve.token)
python examples/python/quickstart.py
```

## Keeping the brain

By default the throwaway brain is deleted on exit. `KIOMON_EXAMPLE_DB=./my-brain.db` keeps it,
and the script prints the `kiomon --db …` commands that reach the same file — the SDK and the
CLI are looking at one database, which is what makes a local brain debuggable.

## What local does differently

Three things, and the scripts say so where they hit them:

- `get_latest_briefing` answers `501 unsupported_locally` — a briefing is composed by a model.
- `explore_memory_graph` supports `depth=1`; a deeper walk is a path search this engine does
  not do, so it refuses instead of returning a smaller graph.
- A workspace is a read boundary: no verb moves a memory between workspaces.
