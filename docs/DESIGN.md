# Design

<!-- SPDX-License-Identifier: Apache-2.0 -->

Why the machine is shaped the way it is. `docs/CONTRACT.md` says what a caller can rely on;
this says what the pieces are for and what was rejected getting there. Where the two
disagree, one of them is a bug and the pull request says which.

Every decision below was made against a measurement or a specific failure, and most of the
failures are written down in `plans/001-local-v1-core.md` — the work package records are the
long form of this document, and this is the version somebody reads before changing the code.

## The shape

One binary, one SQLite file, no network, no daemon, no account. A command is a process: it
opens the file, does one thing, writes one answer to stdout, and exits.

That shape buys four things that the more usual architecture does not have:

- **Nothing to run.** No service to start, no port to bind, no crash loop. `kiomon` works in
  a `Makefile`, a git hook, a container `RUN`, and an agent's tool call, because there is no
  ordering relationship between commands beyond "the file exists".
- **Nothing to secure.** The database is a file with file permissions. There is no listening
  socket to authenticate against, no token to leak, and no second copy of the user's memory
  on somebody's server. The only network calls in the binary are the embedder's, to an
  endpoint the user configures.
- **Nothing to trust.** Every number in the README is reproducible with `kiomon bench` on
  the reader's own machine, and the report says which build produced it.
- **Nothing to break.** A process that crashes mid-write leaves a SQLite WAL to recover, and
  the next command recovers it. There is no in-memory state whose loss changes an answer.

The cost is also real and worth stating: **a cold process pays for its own start** — around
2 ms to open and 9 ms to spawn on Windows — and every command re-opens the database. For a
search at 10 000 memories that is 7% of the answer, and for an `add` it is most of it. §7 of
the plan records the trigger for revisiting this (a `serve` at v1.2) and the measurement
that says the decision is command-specific rather than an average.

## The rules

Each of these exists because something specific went wrong, or would.

1. **stdout is the answer, stderr is everything else.** `kiomon … | jq` always works, in every
   mode. `kiomon-core` contains no `print!` at all, so no code path can break this by
   accident. Colors are never emitted: clap is built without its `color` feature, so escape
   sequences cannot appear even on a terminal.
2. **The database is the only state.** No lock files, no cache directory, no marker for
   "initialised". `init` is idempotent because it has nothing to remember; `doctor`
   diagnoses one file. A tool whose state is split across two places has two answers to
   "what is here", and the second one is always stale.
3. **SQLite is the clock.** Every timestamp is `strftime('%Y-%m-%dT%H:%M:%SZ','now')` in
   SQL. The binary has no datetime dependency, two machines cannot disagree about a
   memory's age within one database, and the decay maths never has to convert a timezone.
4. **A write is one transaction.** `add` either exists and is indexed, or it did not happen.
5. **A read writes nothing except the retrieval counter.** `search` and `retrieve` record
   which ids they returned (that counter is the stretch factor on the decay clock, not
   telemetry) and `--no-record` turns even that off. Nothing else on the read path writes,
   which is why decay is a command and not a side effect.
6. **Untrusted input is never interpolated into SQL.** Values are bound. The only string
   built into a statement is an enum's `as_str()` — a `&'static str` from a closed set — and
   each such site says so.
7. **Text limits are shared.** Title 300 characters, excerpt 600, body 1 MiB, enforced in the
   store and therefore by every door: `add`, `update`, `review approve`, `import`. A limit
   per door is a memory the file may hold and the CLI may not, which is a memory the user
   cannot fix.
8. **A failure explains the next action.** Exit `4` means "valid request, wrong environment";
   its message names the config key, not the error. `--legs vector` with no embedder is exit
   `4` and never a quiet fallback to full-text search, because returning something other than
   what was asked for is undetectable by the caller.
9. **Nothing is inferred about the user's intent.** No default workspace is guessed from the
   current directory, no review mode is inferred from content, no confidence is invented. An
   absent value stays absent and the caller sees it.

## Layers

```text
kiomon-cli        argv → verbs, streams, exit codes, JSON envelope
   │              commands/, cli.rs (the parser is CONTRACT.md), out.rs (the renderers)
   ▼
kiomon-core       the engine: no stdout, no argv, no process
                  db.rs (pragmas, migrations, connection)
                  store.rs (the writes and the row shapes)
                  search.rs · fuse.rs · vector.rs · retrieve.rs (the read paths)
                  lifecycle.rs · decay.rs (what happens over time)
                  import.rs (the portability format) · embed.rs (the model client)
                  bench.rs (a generated corpus) · model.rs (the vocabulary)
```

`kiomon-core` is the whole engine and is published on its own, because the Rust SDK should
*be* the engine rather than an HTTP client for the local case. The CLI is one consumer; WP9's
`serve` and `mcp` are two more, and they share the verb layer rather than re-implementing it.

## Storage

One table per idea, and the interesting ones are the indexes.

- **`memories`** — the row. `id` is a ULID: sortable by creation time, which is why `list`
  is an index scan and why the export cursor is free. `content_hash` is FNV-1a over the text
  that makes a memory the same memory; a collision would miss a duplicate, never lose a
  memory, which is the right direction for that failure.
- **`memory_fts`** — FTS5, a **plain table maintained by triggers**, not
  `external content`. The reason is the review inbox: a `pending_approval` memory has no row
  in the index by design, and an external-content table declares that a missing row is
  corruption — `integrity-check` would flag the intended state as damage.
- **`memory_links`** — the edge table, with `valid_to` so a resolved relation can be
  retired rather than deleted. `link` writes one edge by hand, `add --ref` writes
  `derived_from`, the auto-link pass writes `related_to`, and `forget` cascades.
- **`memory_embeddings`** — one vector per (memory, model), keyed by model name so switching
  models cannot serve a query from a space the vectors were not built in.
- **`workspaces`**, **`memory_outcomes`**, **`meta`** — the scope, the outcome log, and the
  few pieces of durable bookkeeping (the decay cursor) that must survive a process.

**Covering indexes, not clever queries.** `idx_mem_stats` exists after a WP1 measurement
showed a "covering" index that was not covering — it held three of the four columns the
query read, so every row was a table lookup anyway. The rule that came out of it is written
in the migration: an index is not covering unless it holds every column the query reads.

**Pragmas** (`db.rs`, one string, applied to every connection): WAL so a reader never blocks
a writer, `synchronous = NORMAL` because a local memory that loses the last retrieval
counter to a power cut is not a memory that lost anything, `foreign_keys = ON` so the
cascade from `forget` is the database's job and not the code's, a 16 MiB page cache, and a
256 MiB `mmap_size` — mapped, not resident, and the reason a memory report should quote
private bytes rather than the working set.

**Migrations are append-only and numbered**, applied in order inside a transaction, with
`PRAGMA user_version` as the version. `0001_init.sql` is committed and is never edited: a
schema change is `0004_*.sql`, because a migration history that can be rewritten is a
migration history that disagrees with the databases in the field.

## The write path

```text
add ─▶ workspace resolve ─▶ kind + text validation ─▶ embedder present? (no further work here)
     ─▶ content hash ─▶ dedupe lookup ─▶ INSERT (one transaction)
     ─▶ the trigger indexes it, or refuses to: draft rows do not appear in FTS
     ─▶ commit
     ─▶ embed (post-commit; failure is a warning naming `reindex --embed`)
     ─▶ decay nudge (paged, resumable, cursor in `meta`)
```

Three of those steps are decisions worth knowing:

- **The embed is after the commit**, because a memory whose vector failed is a memory that
  exists and is findable lexically. A write that rolls back because a model was down would
  turn an unrelated outage into data loss.
- **The decay nudge lives on `add`**, walks a page at a time from a cursor, and is not on any
  read path. Running decay inside `search` would make a read write, and the cost would land
  on the slowest command a user runs interactively. WP4 measured it: 62 ms per 500-row page,
  and a nudge that rewrote the same rows every time until it was made to skip them.
- **Deduplication is by content hash, scoped to the workspace.** `--force` is how a caller
  says "I know, store it twice"; without it the existing row is returned with
  `duplicate: true` and exit `0`, because "you already told me that" is not a failure.

## The read path

### Search

```text
query ─▶ FTS5 ladder ─▶ lexical candidates ─┐
                                          ├─▶ reciprocal rank fusion ─▶ 3×limit window
        query ─▶ embed ─▶ cosine window ────┘                            │
                                                              rank, filter, cut the page
                                                                         │
                                                       snippet post-pass on the page's ids
                                                                         │
                                                        record the retrieval counters
```

- **The ladder is three attempts**: the phrase, then all terms as prefixes, then any term.
  Every term is quoted, so no query is FTS5 syntax — `NEAR`, `*`, `-`, `(`, `)` are text.
  Quoting is also why accents and CJK work; the alternative (stripping to `[A-Za-z0-9_]`)
  deletes a Japanese query outright.
- **Fusion is always on**, even with one leg. One code path means a second leg cannot change
  the ordering of the first, and WP7's vector leg plugged in without touching the lexical
  path.
- **The snippet query is separate from the ranked query.** Running it inline cost 47 ms at
  10 000 memories; run for the page's ids after the cut, it costs 0.30 ms. The ranking query
  reads no text it does not rank.
- **The vector leg reads blobs, it does not compute.** 10 000 × 384 floats is 15 MB and
  about 2 ms of arithmetic; the fetch shape dominates. A SQL join cost 78.6 ms against 28.4
  ms for a sequential scan plus a Rust-side id set, so `vector_window` is two statements.
- **The similarity floor is zero.** Cosine ≤ 0 is not a candidate; anything higher is
  reported per row so a caller can apply the threshold their own model needs, because
  "how similar is similar" depends on the model's anisotropy and is not a constant this
  engine can know.

### The packet

`retrieve` is a search plus a decision about what a caller should *do*, and it is a verbatim
port of the hosted product's wire payload — field names, absent-not-null keys, and all —
because the local build and `/api/retrieve` (WP9) have to agree about what a packet is.

```text
candidates (same pool search would rank)
   ─▶ applicability: does the memory's text carry the caller's constraint terms?
   ─▶ rank within the intent: 0.5 · relevance + 0.3 · strength + 0.2 · recency
   ─▶ bucket by kind: facts · procedure (one) · episodes · evidence
   ─▶ warnings: expired · low confidence · superseded · contradicted
   ─▶ summary sentence, retrieved_ids, conflicts
```

- **An intent re-ranks, it does not filter.** The candidate pool is the same; what changes is
  the weight of a kind, and therefore which candidates fit when the pool is larger than the
  page. On a corpus that fits in one page, two intents differ only in the summary — and that
  is asserted by a test, because the plan claimed something stronger and was wrong.
- **One procedure per packet.** Two competing routines is a decision the caller cannot make
  better than the engine can; the runner-up stays in `retrieved_ids`.
- **`conflicts` is `[]` because nothing can write a contradiction yet**, not because the code
  is a stub. The query runs, is indexed, and finds nothing.
- **Expired memories are demoted, never dropped.** A caller that cannot see the expired
  memory cannot see why it was wrong.

## The graph

A link is a row: source, target, relation, `origin`, `score`, `created_at`. Three rules make
it a graph rather than a table of pairs.

**A symmetric relation is stored once.** `related_to` has no direction, and the table's
`UNIQUE (source_id, target_id, relation)` cannot tell that `(A, B)` and `(B, A)` are the same
statement. So a symmetric edge is canonicalised before it is written — the smaller id is the
source, which for ULIDs means the older memory — and `relate` reads both columns. Two
memories written from either end is one row, and a replay is a no-op.

**A draft has no edges, in either direction.** The inbox's promise is that a memory waiting
for approval cannot be reached by *any* read path, and a graph edge is one: `relate` on the
parent would print the draft's title. So `link` refuses an id that is `pending_approval`, the
auto-link pass only ever sees active rows through the ladder's own status filter, and `relate`
filters a pending neighbour out as well. The write-side refusal is the rule; the read-side
filter is what makes it hold if a future writer forgets.

**Auto-linking is bounded, relative, and local.** Activation — `add` or `review approve` —
runs one ladder climb over the memory's title and the first kilobyte of its body, and writes
at most five `related_to` edges to neighbours scoring at least half the best match's `bm25`.
The floor is relative because `bm25` is not comparable across corpora; the terms are capped
at twelve because rung 3 of the ladder is an `OR` and a kilobyte of text is ~150 terms whose
ranking cost is the whole corpus (measured: 60 ms per write, and the fix is in the plan's WP5
record); the workspace is fixed because an edge is a read path and a workspace is a boundary.
The pass runs inside the write's transaction, and a failure is a warning rather than a failed
save — the alternative would make a *ranking* query a way to lose memories.

`unlink` removes by pair or by pair-and-relation, and reports zero rather than raising: exit
`2` is "already the way you wanted it", which is the same rule `pin` and `restore` follow.
Nothing writes `valid_to` yet, and the read paths already honour it — a resolution has to be
expressible before it can be recorded, and `unlink` is what a caller uses today.

The packet is the graph's main consumer: `contradicts` and `supersedes` edges among the
candidates become a conflict note and a demotion. That read was ported from the hosted
product in WP5.5, when nothing could write the edge it looks for; the last thing that made it
reachable was `supersedes` in the relation vocabulary, which is what migration `0004`
rebuilt the table's `CHECK` constraint for.

## The lifecycle

Nothing about a memory changes by itself, and what does change is a formula with one owner:

| Field | Moved by | Rule |
|---|---|---|
| `strength` | `decay` only | `importance · 0.5^(age / half-life)`, stretched by retrievals up to 2× |
| `confidence` | `rate`, `outcome`, `import` | never by time |
| `expires_at` | `rate`, `outcome`, `import`, `restore` | `restore` clears it: coming back is a claim about now |
| `status` | `review`, `pin`, `archive`, `restore`, `forget`, `decay` | |
| `feedback_score`, `feedback_count` | `rate`, `outcome` | the rating's own number |

Two rules that look arbitrary and are not: **ratings do not touch `strength`** (decay owns
it, and two writers to one field is how a formula becomes unexplainable), and **`reference`
never decays** — the user's library is not a claim that gets less true.

One gap, recorded rather than papered over: `add` cannot set an expiry. The column exists,
`decay` acts on it, the packet warns about an expired memory, and a rating, an outcome or an
import writes it — but the flag a user would reach for at the moment they save something does
not exist yet, and it belongs with the hosted write shape rather than invented beside it.

`decay` is a command, so a scheduled `kiomon decay` is a line in a crontab rather than a
thread in a binary, and `--dry-run` is how a user decides whether they want it. The archive
threshold is a constant in `decay.rs` with the plan's reasoning beside it.

## The embedder

Bring your own model: Ollama, any OpenAI-compatible `/embeddings` endpoint, or the
deterministic stub the tests and the benchmark use. There is no bundled model, and
`native-embed` is a reserved feature name rather than a stub of one.

Two details are load-bearing. **`ureq` 3 with no TLS**: the dependency tree with TLS is 71
crates against a budget of 60, and every runtime this is aimed at serves plain HTTP on
localhost, so `https://` is refused by name with the reason and the workaround. And
**vectors are per model**: a search reads only the configured model's rows, `reindex --embed`
replaces another model's, and the cost of switching models is a full re-embed — the honest
price of a corpus that contains one search space at a time.

## Portability

`export` writes the stored rows, one JSON object per line, and `import` reads them back.
The format is a *wire*, not a rendering: `export` serialises the same struct `import`
deserialises, so the two directions cannot drift, and a line carries no derived labels.

Three things are deliberately absent, and each is stated where a user will read it:

- **Vectors** — model-specific, 4 KB each, regenerable, and a backup that silently drops
  half of retrieval is the worst kind of omission. `import` re-embeds what it writes;
  `export`'s summary says `vectors_included: false`.
- **Edges** — nothing can write one yet, so a file that carried them would describe a tool
  that does not exist.
- **A workspace's review mode** — it is a property of the workspace row; inferring `review`
  from an import that happens to contain a draft would be inferring a trust decision from a
  status.

**The stability promise.** A line's field names, the markdown frontmatter's eleven keys, and
the `exists`/`duplicate`/`written` verdict order are what a file somebody kept depends on.
They change only by adding: a new field is fine, a renamed one is a new export format with a
new `contract_version`, exactly like the JSON envelope.

## Measurement

The budget in the plan measures **work**, not wall time: `total` minus the spawn floor, where
the floor is measured moments before the probe on the same machine. A `open ≤ 5 ms` gate read
on wall time is unreachable on Windows before any code runs, and comparing wall time across
two operating systems compares two operating systems.

The harness is `kiomon bench`, in the binary and not in a test directory, for one reason: a
number nobody can reproduce is a claim. It generates a deterministic corpus, *describes* it
in the report (how many bodies are short, medium and long; how many rows each probe query
matched), prints the volume it wrote to, and applies the plan's gates — in CI, at two corpus
sizes, so a regression is a red build rather than a smaller number in a pull request.

Things this project has learned the hard way, and which the harness now makes visible:
process spawn is 7.7–9.0 ms on Windows; the first write into a fresh file costs 47–145 ms on
one volume and 3–5 ms on another for the same binary; a RAM-resident page cache makes the
second run of anything look fast; and an export's peak *working set* says more about
`mmap_size` than about the code, so memory claims quote private bytes.

## Two transports, one verb layer

`serve` and `mcp` are in this release and a daemon is not. The distinction is the whole of
the design: a **transport** answers requests somebody else is already making, and a
**daemon** is a process the tool depends on. The first is a hundred lines of framing around
verbs that already exist; the second is a supervisor, a lifecycle, a warm cache and a new
class of failure — "is the daemon up" — in front of a tool whose promise is that it works
because the file is there.

So there are three layers and the boundaries are load-bearing:

| Layer | Owns | Never |
|---|---|---|
| `kiomon-core` | Every decision: what activates, what a search ranks, what a write stores. | Knows that HTTP or JSON-RPC exists. |
| `service.rs` | The wire shape: which field, which name, which status. | Decides anything; it calls one core function per verb. |
| `http.rs` / `mcp.rs` | Framing: bytes in, bytes out. | Reads the database, or knows what a memory is. |

The test of the split is that neither transport owns an answer. `POST /api/retrieve` returns
the packet's own JSON — not a reshaped copy of it — and `tests/serve.rs` asserts that the
HTTP packet equals the packet the CLI builds from the same file. A field that appears in one
door and not the other would be a bug with no place to hide.

### Why the wire is the hosted product's

The published SDKs speak `https://api.kiomon.com`, and `baseUrl` is what points them at a
local brain. That makes the hosted wire a *contract this engine does not own*: if the local
`GET /api/graph` spelled its centre `id` while the SDK sent `center_id`, the SDK would work
online and break offline. So the hosted spellings are primary (`center_id`, `hybrid`,
`{updated, skipped}`, `review_inbox_url`) and this engine's extras are additive and named —
never a rename. Where this engine has *more* to say — why a legacy was skipped, which rung
answered a search, whether a leg degraded — the extra travels beside the hosted field, and
a hosted client ignores it.

The one place this engine deliberately disagrees is the graph's `origin = 'auto'` filter.
The hosted query hides auto-links because its auto-link pass is a background scan that
writes thousands of similarity edges and would flood a drawing; here the only edges most
brains have are the ≤5 per activation that `add` wrote, so hiding them would make the view
empty *and* disagree with `relate`, which reads the same rows. `origin` travels on every
edge, so a client that wants the hosted view can filter — and the divergence is written down
where a reader of the route will find it.

### The local token, and the three refusals

A loopback socket is not private. Any process on the machine can connect to it; so can a
page in a browser, which is the one that matters, because a browser can reach `127.0.0.1`
from a page served by anyone.

Three checks, in order, before routing sees the path:

1. **`Host` must be a loopback name.** DNS rebinding is the attack the token does not stop:
   `evil.example` resolves to `127.0.0.1`, the browser sends `Host: evil.example`, and the
   page is now same-origin with this server. The header is the one thing it cannot forge.
2. **`Origin`, if present, must be loopback.** A cross-origin page's `Origin` is unforgeable;
   absent means "not a browser", which is what every SDK is.
3. **The token must match.** `X-API-Key` (what the SDKs send) or `Authorization: Bearer`.
   This is the check that turns "loopback" into "mine", and it runs before routing so an
   unauthenticated client cannot map the surface by comparing `401` with `404`.

There is no CORS header, so even a same-origin-in-appearance fetch gets an opaque response.
The token lives in `<data dir>/serve.token` — beside the database, so two brains are two
credentials — is created on first run with mode `0600`, and is printed once, because a
credential nobody can find is a credential nobody uses.

### What a request costs

Measured on the 10 000-memory corpus with `curl` as the client (Python's socket stack costs
~14 ms per connection on this host, which is bigger than most of the routes, so it is not a
usable ruler):

| Route | p50 | Note |
|---|---|---|
| `GET /health` | 0.5 ms | the transport floor: accept, parse, write |
| `GET /api/documents/{id}` | 2.3 ms | vs 2.4 ms of `get` work on the CLI |
| `GET /api/search?q=deploy` | 27.2 ms | vs 26.1 ms of `search` work: the door costs ~1 ms |
| `POST /api/retrieve` | 31.4 ms | the packet plus the retrieval record |

The server adds about a millisecond to work that already existed, and removes the ~7.5 ms a
process spawn costs on this platform. That is the argument against a daemon for latency
reasons: there is nothing left to win. The remaining argument is push (streaming, background
decay on a timer), and §9-D8 keeps that for v1.2 behind a trigger.

**One connection per request, and a connection per thread.** `rusqlite::Connection` is not
`Sync`, so a shared handle needs a mutex — and a mutex around the read path serialises the
one thing worth parallelising. Each request opens the file instead, exactly as a fresh
`kiomon` process does, into a connection nothing can contend for. Eight workers, one request
each, `Connection: close`: there is no keep-alive state and no pipelining, which is *why* a
hand-written parser is safe here rather than foolhardy.

### What is refused by name

`501 unsupported_locally` for `revert` (this engine keeps no revision history), briefings (a
model composes them), and `depth > 1` on the graph (a path search is not a neighbourhood,
and answering one with the other is undetectable to the caller). `400` for a graph query with
no centre, because a whole-graph overview needs a most-connected-nodes ranking this engine
does not compute.

Every one of those is a route that *exists* in the table and refuses, not a route that falls
off the end into a `404`. The difference is what a client branches on: "ask again" versus
"stop asking", and a `404` reads as the first.

### The MCP server

The same verb layer over newline-delimited JSON-RPC on stdin and stdout. Two rules it keeps:

**stdout is the protocol.** Every diagnostic goes to stderr, and the test that proves it
parses every line the process wrote. A stray `println!` here is not an error a client sees —
it is a client that hangs, and the failure is a hundred lines away from the cause.

**The tool names are the hosted product's.** An agent's prompt is written against
`retrieve_memory` and `record_outcome`; a local server that renamed them would make every
such prompt wrong, and running locally is the thing that is *not* supposed to be visible. The
three local-only tools (`get_memory_relations`, `link_memories`, `unlink_memories`,
`list_review_inbox`) are additive, and `get_latest_briefing` is present so that calling it
says "not available locally" instead of "no such tool" — the same
absent-versus-not-implemented rule as the routes.

A failed tool answers `isError: true` in its *result*, with the code in `kiomon_error_code`,
not a JSON-RPC error: a client that receives a protocol error may treat the tool as missing
rather than as refused, and a model that does that stops using a tool that works.

## What is deliberately absent

- **A graph leg inside a search.** The graph is here and `relate` walks it, but a walk
  *fused into a search's ranking* is a different operation: the hosted implementation is a
  PageRank over the seed set, fused as a third RRF list, and §4.3 of the plan gives it to
  v1.1. `--legs graph` is in the vocabulary and refuses, rather than being silently answered
  by another leg.
- **Edges in an export.** The portability format carries memories, not relationships: a
  restore comes back with the graph missing, and the export says so. A vector can be
  rebuilt from text; an edge cannot, so this is the one absence in the format that is a real
  loss rather than a saving, and it is recorded as the next thing the format should grow.
- **A daemon.** `serve` and `mcp` are transports: they answer requests somebody else is
  already making, and neither is a process anything depends on. A daemon — warm cache,
  background decay on a timer, streaming — is §9-D8's v1.2 work behind a trigger, and the
  measurement is the argument: the transport adds ~1 ms to work that already existed and
  removes the 7.5 ms a spawn costs, so there is nothing left for a daemon to win on latency.

## Changing it

- A schema change is a new `NNNN_*.sql`; `0001_init.sql` is never edited.
- A change to a field name, a type, or an exit code is a contract change: bump
  `CONTRACT_VERSION` and say so in `docs/CONTRACT.md`, in the same commit.
- Where reality contradicts the plan, the plan is amended in the same commit as the code.
  The work-package records exist so that "we found out otherwise" is a fact on the record
  rather than a story somebody remembers.
- A comment explains *why*. A test name states the invariant, not the method.
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and
  `cargo test --workspace` are the floor; CI runs them on three operating systems, and its
  budget job runs `kiomon bench --gate` so a performance regression is a red build.
