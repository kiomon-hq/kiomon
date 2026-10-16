# 001 — Kiomon Local: v1 core

**Type:** Product + architecture plan
**Status:** ✅ **DECIDED 2026-10-14** — all §9 decisions answered; WP0 in execution
**Repo:** `kiomon/` (this directory) · planned upstream `kiomon-hq/kiomon`
**Written:** 2026-10-14

---

## 0. Thesis

The hosted platform (`platform/`) is a multi-service, multi-tenant, LLM-dependent
pipeline: Worker + D1/Cloud SQL + Qdrant/pgvector + Cloud Run (vectorizer,
connectors, graph) + Vertex Gemini + billing + connectors. Almost none of that is
required for the thing users actually feel: **my agent remembers what I told it,
and finds it again in a few milliseconds.**

So the open option is not a port. It is a **reduction**: one binary, one SQLite
file, no network, no API key, no account. The claim we have to earn locally is
not "we have a memory stack" — it is **"retrieval is faster than your model can
finish typing its next token."**

Three numbers define v1:

| Claim | Target (10k memories, warm OS cache) |
|---|---|
| `kiomon search` end-to-end, process spawn included | **p50 ≤ 25 ms**, p95 ≤ 60 ms |
| `kiomon add` end-to-end | **p50 ≤ 15 ms**, p95 ≤ 40 ms |
| Resident memory / binary size | ≤ 25 MB RSS · ≤ 6 MB stripped binary |

Everything in §1 is chosen to protect those numbers. Everything in §10 is cut to
protect them.

---

## 1. The v1 cut

### In scope — the core loop

```
  add / import ──▶ dedupe + write gate ──▶ SQLite (FTS5 + lifecycle)
                          │                        │
                          │                        ▼
                          └── auto-link ────▶ search  (lexical [+ vector]) ──▶ RRF fuse ──▶ ranked window
                                                                                              │
                                                                              record retrieval counters
                                                                                              │
                                                       outcome/rate ──▶ strength ──▶ decay (offline)
```

1. **Four memory kinds** — `reference | semantic | procedural | episodic`, with the
   platform's friendly names (*Files / Facts / Routines / Moments*), and the
   platform's per-kind metadata envelope (`domain`, `entities`, `claims`,
   `source_refs`, `occurred_at`, `trigger`, `steps`, …).
2. **Hybrid retrieval** — FTS5 `bm25()` lexical leg fused with an optional vector
   leg by reciprocal rank fusion (`k = 60`), collapsed to memory rows.
3. **Lifecycle** — `status` (`active | pending_approval | archived`), `confidence`,
   `importance`, `strength`, `pinned`, `expires_at`, `retrieval_count`,
   `last_retrieved_at`, `feedback_score`; plus the platform's decay formula.
4. **Minimal write gate** — authority + confidence decides *activate* vs *pending
   review*; `review_mode` per workspace; exact-duplicate discard. Soft, revertible.
5. **Knowledge graph (local)** — typed directed `memory_links`
   (`derived_from | supports | contradicts | related_to | precedes | follows | part_of`),
   plus a cheap auto-link pass on write and a 1-hop `relate` read.
6. **Feedback** — `rate` and `outcome` credit, moving `strength` and
   `feedback_score`. This is the loop that makes the brain compound locally.
7. **Workspaces** — a column, not a tenancy system. `default` plus user-named
   ones. Isolation is a `WHERE` clause.
8. **Agent-grade CLI** — every command has `--json`, stable exit codes, and a
   token-cheap default output. Plus `import`/`export` (JSONL) so the brain is
   never hostage to the tool.

### Out of scope for v1 — and why

| Cut | Why |
|---|---|
| **LLM anything** (auto-tagging, entity/claim extraction, RAG chat, briefings, `reflect_session`) | Requires a provider, a key, a budget, and latency. The platform's own `draft_memory` accepts `entities`/`claims`/`domain` from the agent precisely to skip the paid pass — locally the agent *always* supplies them, so extraction is dead weight. |
| **Embeddings in the default build** | See §9-D1. Default is FTS-only; the vector leg is a config-gated provider (BYO Ollama / OpenAI-compatible). No ONNX weights in the v1 binary. |
| **Connectors** (Notion, Drive, Confluence, RSS, Scholar) | Network, OAuth, and sync state machines. `import` covers the migration case. |
| **Browser extension, dashboard, landing page** | Not a CLI product's problem. |
| **Auth, API keys, plans, quotas, billing, orgs, multi-tenancy** | The whole point of local. `workspace_id` is a filter; there is no `user_id`. |
| **MCP server** | v1.1, and it ships *with* `serve` rather than before it: one verb layer, two transports, one milestone (§9-D8). v1 is CLI so the native contract can be proven first. |
| **HTTP server / SDK backend** | v1.1 — `kiomon serve`, loopback only, speaking the hosted `/api/*` contract so the existing Python and TypeScript SDKs work unchanged. **Self-hosting the engine is a first-class product**, not a power-user path (§9-D8). |
| **PDF parsing, OCR, reader mode** | Local files + stdin + URL fetch only. |
| **Contradiction engine / quarantine** | Needs claim comparison. v1.1, after `claims` are being stored. |
| **Cloud sync, hosted parity, telemetry** | Off the table by definition. |

### Deliberate fidelity choices

Where the platform's semantics are cheap to keep, we keep them exactly — so a
local brain and a hosted brain mean the same thing, and so docs/vocabulary can be
shared:

- RRF constant `k = 60`, `score = Σ 1/(k + rank)`, one `fuse()` function.
- Decay: `strength = importance * 0.5^(age_days / effective_half_life)`, half-lives
  `episodic 30d / procedural 90d / semantic 180d`, retrieval stretch
  `min(1 + 0.1 * retrieval_count, 2.0)`, floor `0.05` → archive, `reference` never
  decays, `pinned` exempt.
- Auto-promote threshold `0.75`; high-authority sources (`user_saved`, `import`)
  always auto-activate.
- Tool-facing vocabulary (`kind` values, relation names, friendly kind nouns) matches
  `platform/mcp/src/handlers/`.

Where the platform's shape is an artifact of the cloud, we diverge (and say so):
no `user_id`, no `index_status` split, no queue, no `document_chunks` table in v1.

---

## 2. Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│ kiomon (single static binary)                                    │
│                                                                  │
│  kiomon-cli          clap parser · output shaping · exit codes   │
│      │                (the ONLY place that prints or exits)      │
│      ▼                                                           │
│  kiomon-core         Database · Store · Search · Lifecycle ·     │
│                      Links · Gate · Decay · Embed · Import       │
│      │                                                           │
│      ▼                                                           │
│  rusqlite (bundled SQLite, static)  ──▶  ~/.kiomon/brain.db      │
└──────────────────────────────────────────────────────────────────┘
```

**No async, no runtime.** Every v1 operation is local I/O bound by one SQLite
file. `tokio` buys nothing and costs binary size and cold-start variance. The one
exception is the optional embedder's HTTP call, which is a blocking `ureq` request
on a path the user explicitly opted into — v1.1 can revisit if it hurts.

**Workspace layout:**

```
kiomon/
├── Cargo.toml                 # [workspace] members = ["crates/*"]
├── crates/
│   ├── kiomon-core/           # library. no printing, no process::exit, no clap
│   │   ├── src/
│   │   │   ├── db.rs          # open, PRAGMAs, migrations, transactions
│   │   │   ├── model.rs       # MemoryKind, Memory, Link, Relation, Status, NewMemory
│   │   │   ├── store.rs       # add/get/update/list/purge
│   │   │   ├── search.rs      # lexical leg, sanitizer, fallback ladder
│   │   │   ├── fuse.rs        # reciprocal rank fusion (k=60)
│   │   │   ├── vector.rs      # brute-force cosine over BLOBs (feature-gated)
│   │   │   ├── embed.rs       # Embedder trait + Ollama/OpenAI-compatible impl
│   │   │   ├── gate.rs        # authority, confidence, dedupe, review mode
│   │   │   ├── lifecycle.rs   # pin/archive/restore/rate/outcome
│   │   │   ├── decay.rs       # the strength formula
│   │   │   ├── links.rs       # typed edges, auto-link pass, 1-hop read
│   │   │   ├── import.rs      # JSONL in/out, markdown export
│   │   │   ├── config.rs      # ~/.kiomon/config.toml + env
│   │   │   └── error.rs       # Error + exit-code mapping (no stderr writes)
│   │   └── migrations/0001_init.sql
│   └── kiomon-cli/            # binary: `kiomon`
│       └── src/
│           ├── main.rs
│           ├── cli.rs         # clap definitions
│           ├── commands/      # one module per subcommand
│           └── out.rs         # human + --json rendering, exit codes
├── benches/                   # in-process: search/add/get at 1k/10k/100k
├── docs/
│   ├── CONTRACT.md            # the agent-facing CLI contract (stable)
│   ├── DESIGN.md              # schema + retrieval + lifecycle rationale
│   └── AGENTS.md              # drop-in block telling a coding agent to use `kiomon`
├── plans/                     # this file and successors
├── .github/workflows/ci.yml
├── LICENSE
└── README.md
```

**Two crates, not one.** A single crate would be "more minimal" for about a week,
then `kiomon mcp` and `kiomon serve` (both v1.1, §9-D8) would either duplicate the
library or force a split under load. `kiomon-core` has no `clap`, no `print!`, and
no `process::exit` — that rule is what makes those later binaries one `main.rs`
each.

**One verb layer, thin surfaces.** Every operation is a function over `&Database`
that returns owned data, plus a JSON shape `docs/CONTRACT.md` documents. The CLI is
the first adapter for that layer, not the only planned one: WP9 adds HTTP and stdio
over the same functions. This is the only architectural rule the v1.1 decision adds
to v1, and it earns its place immediately — anything that prints inside
`kiomon-core`, reads `std::env`, or decides an exit code cannot be served, so a
command that breaks the rule is a bug *today* and not a refactor later.

---

## 3. Storage

### 3.1 File and connection setup

Default path resolution (first match wins):

1. `--db <path>`
2. `$KIOMON_DB`
3. `$XDG_DATA_HOME/kiomon/brain.db` (Linux) ·
   `~/Library/Application Support/kiomon/brain.db` (macOS) ·
   `%APPDATA%\kiomon\brain.db` (Windows)
4. `~/.kiomon/brain.db` (fallback)

`~/.kiomon/config.toml` holds `default_workspace`, `review_mode`, and the optional
`[embedder]` block. Env overrides config; flags override env.

On every open:

```sql
PRAGMA journal_mode = WAL;        -- readers never block the writer
PRAGMA synchronous  = NORMAL;     -- WAL-safe; full fsync only at checkpoint
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
PRAGMA temp_store   = MEMORY;
PRAGMA cache_size   = -16000;     -- 16 MB page cache
PRAGMA mmap_size    = 268435456;  -- 256 MB; read path skips the copy
PRAGMA analysis_limit = 1000;
```

`mmap_size` and `cache_size` are the two knobs that matter for our latency claim;
both are set explicitly rather than left to SQLite's defaults. All PRAGMAs are
applied on every open, not only at `init`, because `journal_mode`/`mmap_size` are
per-connection for some builds — the cost is microseconds and the failure mode of
forgetting is a 10× latency surprise on someone else's machine.

### 3.2 Schema

> **The shipped schema is `crates/kiomon-core/migrations/0001_init.sql`**, and it is the
> authority. The sketch below is the design as argued, kept because it records what was
> intended and therefore where the differences came from. Four of them, all forced by
> WP1 and all recorded in §8: `memory_fts` is a plain FTS5 table rather than an
> external-content one (so a pending draft can be absent from it without
> `integrity-check` calling that corruption); `tags_text` exists only so the index has a
> column to weight; the schema version is `PRAGMA user_version` rather than a `meta`
> row; and there are two indexes — `idx_mem_stats`, `idx_mem_order` — that exist for
> measured reasons and that nothing in the design predicted.

```sql
-- Integer rowid is required: FTS5 external-content tables address their content
-- by INTEGER rowid, so the public string id is a UNIQUE column, not the key.
-- (platform/migrations/0001_init.sql declares `content_rowid='id'` against a TEXT
--  primary key — that is a latent bug we must not port.)
CREATE TABLE memories (
    mem_id            INTEGER PRIMARY KEY,
    id                TEXT NOT NULL UNIQUE,       -- k_<ulid>, sortable, agent-facing
    workspace_id      TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    kind              TEXT NOT NULL DEFAULT 'reference'
                      CHECK (kind IN ('reference','semantic','procedural','episodic')),
    title             TEXT NOT NULL,
    excerpt           TEXT,
    body              TEXT NOT NULL DEFAULT '',
    tags              TEXT NOT NULL DEFAULT '[]', -- JSON array, canonical
    tags_text         TEXT NOT NULL DEFAULT '',   -- space-joined, FTS-facing
    memory_metadata   TEXT NOT NULL DEFAULT '{}', -- JSON, per-kind envelope
    source            TEXT NOT NULL DEFAULT 'user_saved',
    source_url        TEXT,
    confidence        REAL,
    importance        REAL NOT NULL DEFAULT 0.5,
    strength          REAL NOT NULL DEFAULT 1.0,
    corroboration_count INTEGER NOT NULL DEFAULT 0,
    retrieval_count   INTEGER NOT NULL DEFAULT 0,
    last_retrieved_at TEXT,
    feedback_score    REAL NOT NULL DEFAULT 0,
    pinned            INTEGER NOT NULL DEFAULT 0,
    status            TEXT NOT NULL DEFAULT 'active'
                      CHECK (status IN ('active','pending_approval','archived')),
    expires_at        TEXT,
    content_hash      TEXT,                        -- sha256(kind|norm(title)|norm(body))
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL
);

CREATE INDEX idx_mem_ws_status_created ON memories(workspace_id, status, created_at DESC);
CREATE INDEX idx_mem_ws_kind_status    ON memories(workspace_id, kind, status);
CREATE INDEX idx_mem_dedupe            ON memories(content_hash) WHERE content_hash IS NOT NULL;
CREATE INDEX idx_mem_strength          ON memories(status, strength) WHERE status = 'active';

-- External content: the body is NOT duplicated. `tags_text` exists so the FTS
-- column set can match the content table's columns by name, with tags already
-- tokenized (a JSON array would tokenize as punctuation soup).
CREATE VIRTUAL TABLE memory_fts USING fts5(
    title, excerpt, body, tags_text,
    content='memories', content_rowid='mem_id',
    tokenize='porter unicode61',           -- no `remove_diacritics=2`: see §9-D5
    columnsize=0, detail='full'
);
CREATE TRIGGER mem_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memory_fts(rowid, title, excerpt, body, tags_text)
    VALUES (new.mem_id, new.title, new.excerpt, new.body, new.tags_text);
END;
CREATE TRIGGER mem_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, title, excerpt, body, tags_text)
    VALUES ('delete', old.mem_id, old.title, old.excerpt, old.body, old.tags_text);
END;
CREATE TRIGGER mem_au AFTER UPDATE OF title, excerpt, body, tags_text ON memories BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, title, excerpt, body, tags_text)
    VALUES ('delete', old.mem_id, old.title, old.excerpt, old.body, old.tags_text);
    INSERT INTO memory_fts(rowid, title, excerpt, body, tags_text)
    VALUES (new.mem_id, new.title, new.excerpt, new.body, new.tags_text);
END;

CREATE TABLE memory_links (
    id         INTEGER PRIMARY KEY,
    source_id  TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    target_id  TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    relation   TEXT NOT NULL DEFAULT 'related_to',
    directed   INTEGER NOT NULL DEFAULT 0,
    score      REAL,
    origin     TEXT NOT NULL DEFAULT 'manual',   -- manual | auto | agent
    metadata   TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL,
    UNIQUE(source_id, target_id, relation)
);
CREATE INDEX idx_link_source ON memory_links(source_id);
CREATE INDEX idx_link_target ON memory_links(target_id);

-- Optional vector leg. BLOB = little-endian f32, cosine-normalised on write.
CREATE TABLE memory_embeddings (
    mem_id     INTEGER NOT NULL REFERENCES memories(mem_id) ON DELETE CASCADE,
    model      TEXT NOT NULL,
    dim        INTEGER NOT NULL,
    vec        BLOB NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (mem_id, model)
) WITHOUT ROWID;

CREATE TABLE memory_outcomes (
    id         INTEGER PRIMARY KEY,
    memory_id  TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    outcome    TEXT NOT NULL,       -- success | failure
    assessment TEXT NOT NULL,       -- helpful | unused | misleading | outdated
    created_at TEXT NOT NULL
);

CREATE TABLE workspaces (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    review_mode TEXT NOT NULL DEFAULT 'autonomous' CHECK (review_mode IN ('autonomous','review')),
    created_at TEXT NOT NULL
);

CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- seeded: schema_version, created_by, decay_last_run_at, embedder_model
```

**Decisions worth naming:**

- `workspaces` is a real table (not a free-text column) so `review_mode` has a home
  and `list --workspaces` is a query rather than a `GROUP BY` over the corpus.
- `tags_text` duplicates the tags. The alternative (parse JSON at query time) makes
  the FTS path allocate and defeats the index. Duplication here is ~50 bytes/row.
- `content_hash` exists only to make writes idempotent (§5.1). It is
  `sha256(kind ‖ norm(title) ‖ norm(body))` with `norm` = NFC + lowercase +
  collapse whitespace, computed in Rust — not a SQL expression, because SQLite's
  `lower()` is ASCII-only and we want stable dedupe across scripts.
- Auto-link edges carry `origin='auto'`, so a future "recompute links" can delete
  exactly the machine-produced ones and never touch a human's.
- No `document_chunks` table in v1. Chunk-level retrieval is a scale device: at
  ≤100k memories, ranking memory rows directly is both simpler and *faster* than
  ranking chunks and rolling up. §10 lists chunking as the deliberate v2 trigger.

### 3.3 Migrations

Numbered files `NNNN_slug.sql`, applied in one transaction each, recorded in
`meta.schema_version`. `kiomon init` creates; every command calls
`Database::open()` which applies pending migrations before returning. Forward-only,
no down-migrations, no SQL migration framework dependency — the whole applier is
~40 lines and needs `PRAGMA user_version` plus a file list.

`kiomon doctor` checks `PRAGMA integrity_check`, `PRAGMA foreign_key_check`, and
`INSERT INTO memory_fts(memory_fts) VALUES('integrity-check')` — the three ways a
local brain silently rots.

---

## 4. Retrieval

### 4.1 The sanitizer and the fallback ladder

Port of `SearchService.sanitizeFtsQuery`, widened into a ladder because a local
user typing a sentence must not get zero results:

| Rung | Query | When |
|---|---|---|
| 1 | `"phrase"` (quoted literal) | always tried first |
| 2 | `"t1"* AND "t2"* AND …` | all terms, prefix-matched |
| 3 | `"t1"* OR "t2"* OR …` | if rung 2 is empty |

Built by `search::Ladder` and asserted against this table character for character in
`search::tests::the_ladder_looks_like_the_documented_table`, so the design and the
expressions cannot drift apart. A query with no terms is not an error and not an empty
set of rungs: it is `attempt: nothing_asked`, which is a different fact from
`exhausted` and takes no query to establish.

First attempt with a non-empty ranked window wins. `legs`/`attempt` are reported in
`--json` so an agent can tell *why* it got what it got. Terms are FTS5-quoted
rather than regex-stripped: quoting is strictly safer (the platform's
`replace(/[^\w\s]/g, "")` deletes CJK and accented input outright, which is a bug
we fix by construction).

### 4.2 The legs

**Lexical leg (always on):**

```sql
SELECT mem_id, bm25(memory_fts, 10.0, 5.0, 1.0, 8.0) AS rank
  FROM memory_fts
 WHERE memory_fts MATCH :query
 ORDER BY rank                       -- bm25() returns NEGATIVE values; ascending is best-first
 LIMIT :k
```

Column weights are `title 10 · excerpt 5 · body 1 · tags 8`. (Note: the platform's
`ORDER BY bm25(...)` ascending is correct — `bm25()` is negative — but it is worth
stating because it reads like a bug.)

**Vector leg (opt-in, §9-D1):**

1. Embed the query via the configured provider.
2. Load `(mem_id, vec)` for candidate rows: all rows in scope when the corpus is
   small; above `VECTOR_SCAN_LIMIT = 200_000` rows, prefilter by the lexical leg's
   attempt-3 OR query plus the top-N by `retrieval_count`, and report
   `degraded: true` in `--json`. **Never silently approximate** — the platform
   learned this lesson the hard way (`search.ts` refuses to downgrade a
   `legs=vector` request to a lexical answer).
3. Cosine similarity (vectors stored L2-normalised → dot product), SIMD-friendly
   `f32` slices, `rayon` above 50k rows.

Vector-leg row ids are ranked by similarity descending, then passed to the same
fusion function.

### 4.3 Fusion and ranking

```rust
pub const RRF_K: u32 = 60;
/// score = Σ 1/(k + rank)
pub fn fuse(lists: &[Vec<i64>], limit: usize) -> Vec<(i64, f32)>
```

One function, ported from `platform/worker/src/lib/rrf.ts`, used by search today and
by the graph walk in v1.1. Fusion returns ids + scores only; the caller owns the row
map — the same seam the platform settled on, for the same reason (a third leg should
be one call, not another copy of the loop).

Final ordering is explicit and documented rather than emergent:

```
ORDER BY pinned DESC, fused_score DESC
```

with `--sort relevance|recent|strength` for the non-default cases. `pinned` ranks
above everything because the platform promises it, and because a pinned memory the
user cannot find is a bug report.

**Retrieval is recorded.** Every returned row gets `retrieval_count += 1` and
`last_retrieved_at = now`, in one transaction with one statement. On by default for
`search` and off for `list`; `--no-record` turns it off, and `--json` reports `recorded`
so a caller can tell the two apart. This feeds the decay stretch factor, so it is not
telemetry — it is the mechanism. The recording is WP2's; the stretch factor it exists
for is WP4's.

### 4.4 What search returns (the row shape)

Deliberately the same shape the MCP `search_memory` handler formats, so the v1.1
MCP server is a thin adapter and the docs are shared:

```json
{
  "id": "k_01JC...",
  "kind": "semantic",
  "kind_label": "Facts",
  "title": "…",
  "excerpt": "…",
  "snippet": "…match context with **highlights**…",
  "tags": ["…"],
  "source_url": "…",
  "confidence": 0.9,
  "strength": 0.84,
  "pinned": false,
  "score": 0.0317,
  "created_at": "2026-10-14T…Z"
}
```

`snippet()` comes from FTS5 (`snippet(memory_fts, 2, '**', '**', '…', 12)`) — zero
extra cost, no text processing in Rust.

> **"Zero extra cost" was wrong, and wrong by 34 ms.** `snippet()` is evaluated in the
> select list, so SQLite computes it for every matching row *before* the `ORDER BY` and
> `LIMIT` discard most of them. It is now a query of its own, run for the page rather
> than the window, and it is still FTS5's snippet — no text processing in Rust. §8 has
> the measurements. The row shape it serves is `MemorySummaryJson` plus `score` and
> `snippet`, so a search result *is* a list row with two fields added and the two cannot
> drift apart.

---

## 5. Writes

### 5.1 `add` pipeline

```
parse flags → validate (kind, metadata shape, tag cap 20) → normalise (NFC, trim)
   → content_hash → dedupe lookup
        hit  → return existing id, `duplicate: true`, exit 0
        miss → write gate decision
                 ├─ auto-promote (source authoritative, or confidence ≥ 0.75)
                 │     → INSERT status='active' + FTS (via trigger) + auto-link
                 │     → embed (if configured)   [best-effort, never fails the write]
                 └─ review (review_mode='review', or confidence < 0.75)
                       → INSERT status='pending_approval'
                       → NO FTS, NO links, NO embed  (invisible until approved, by design)
```

**Pending items are not indexed.** This is the platform's rule
(`CONTEXT.md` → Memory Drafting: *"Drafts are never indexed — chunks and
embeddings are created by `promote()` at activation, so pending items stay
invisible to retrieval by design"*) and it is load-bearing: a review inbox that
leaks into retrieval is worse than no review inbox. Indexing happens in `approve`,
in one transaction with the status flip.

Duplicates never create a second row and never error — an idempotent save is a
feature (`todo: the agent saved this twice`). `--force` bypasses dedupe for the
rare genuine repeat.

Validation is fail-open exactly as the platform specifies: a malformed
`memory_metadata` degrades to `{}` with a warning rather than losing the memory;
an unknown `kind` is rejected (agent-facing endpoints may reject; ingest
discards).

### 5.2 Auto-link pass

Synchronous, bounded, no LLM — the highest-value cheap win:

1. Take the new memory's title + first 1 KB of body.
2. Run the lexical ladder with `k = 20`, excluding self.
3. Insert the top `AUTO_LINK_TOP_N = 5` above `AUTO_LINK_MIN_SCORE` as
   `relation='related_to', directed=0, origin='auto'`, score = normalised bm25.
4. If `source_refs` were supplied, write `derived_from` edges (directed) —
   these are the provenance trails the platform wires on activation.

Cost: one extra FTS query per write (~1 ms at 10k rows). Contradiction detection
(`contradicts` edges, quarantine) is deliberately v1.1: it needs claim-level
comparison and is where the platform itself still runs a heuristic.

### 5.3 Feedback

`rate <id> +1|-1` — immediate, human. `outcome <success|failure> --item
<id>:<assessment>` — the agent-facing credit call, up to 50 items, each recorded in
`memory_outcomes` **before** its effect is applied so every number that moved can be
explained.

> **Corrected by WP4.** The tables that were here said `rate` and `outcome` multiply
> `strength`, and that `feedback_score` moves by ±1. Both were wrong, and the first was
> not merely wrong but *incoherent*: §5.4 recomputes `strength` from `importance` on
> every pass, so a strength multiplier written by a rating survives exactly until the
> next `kiomon decay`. The ported behaviour — the hosted product's `credit.ts` and
> `documents/service.ts` — moves `confidence`, a **running average** `feedback_score`
> with a `feedback_count` denominator, and `expires_at`. See §8's WP4 record.

| assessment | `confidence` (from `c`, the claim or `0.5`) | `expires_at` | average | corroboration |
|---|---|---|---|---|
| `helpful` | `c + (1 - c) · 0.1`, diminishing, never certain | — | `+1` | `+1` |
| `unused` | **untouched** — an absence of information must not invent a claim | — | `0` | — |
| `misleading` | `max(0.05, c · 0.85)` | — | `-1` | — |
| `outdated` | `max(0.05, c · 0.9)` | **now** | `-1` | `+1` |

`rate` is the same average without an assessment: `score = (score · n + rating) / (n +
1)`, rounded to two places on every step. The rounding gives the average a fixed point,
which is the hosted product's arithmetic and is kept — see WP4's record for what the
forty-first `unused` assessment does (nothing, and that is fine).

`outcome` carries the task's end state and never uses it: a `helpful` memory in a failed
task is still a helpful memory, and the session's result is a fact about the session.

All numbers are named constants in `lifecycle.rs` — the platform's threshold saga
(`AUTOPROMOTE_THRESHOLD`, `JACCARD_CONFLICT_THRESHOLD`) is a standing lesson that an
unnamed magic number becomes a permanent argument. `docs/CONTRACT.md` carries the same
table for callers; `docs/DESIGN.md` is still a WP8 item.

### 5.4 Decay

`kiomon decay` — a command, not a daemon. Runs on demand, and opportunistically
(auto, ≤ once per 24 h, one nudge of 100 rows, resuming from a cursor in `meta`) on
`add` so that a brain nobody maintains still ages. `--dry-run` scans everything and
writes nothing.

> **Corrected by WP4, twice.** `search` no longer nudges: the nudge measured 48 ms of
> scattered index writes at 10 000 memories, and a read whose promise is that it answers
> a question should not spend that. And the nudge is a *walk*: without a cursor it
> rewrote the same hundred rows every day for as long as they stayed active. Both
> corrections are in §8's WP4 record, with the numbers that produced them.

```
strength        = importance * 0.5 ^ (age_days / effective_half_life)
effective_half_life = HALF_LIFE[kind] * min(1 + 0.1 * retrieval_count, 2.0)
HALF_LIFE = { episodic: 30, procedural: 90, semantic: 180 }   // reference: never
if strength < 0.05 || expires_at < now  →  status = 'archived', strength = 0.05
pinned = 1  →  skipped
```

Archived, never purged. `forget` is the only destructive verb, it requires the
`forget` capability to be typed explicitly (no `--yes` shorthand), and it purges
row + FTS + links + embeddings + outcomes in one transaction.

---

## 6. The CLI contract

`docs/CONTRACT.md` is a **stable, versioned document** — it is the thing agent
integrations are written against, so it changes deliberately and never
incidentally.

### 6.1 Commands

```
Setup & health
  kiomon init [--db <path>] [--workspace <name>]
  kiomon doctor [--json]                     # integrity, schema version, config, embedder probe
  kiomon stats [--kind] [--json]

Write
  kiomon add --kind <k> (--title <t> | --from-file <f>) [--body <b>|--body-file <f>|stdin]
             [--excerpt] [--tag <t>]... [--url <u>] [--confidence <f>]
             [--entities a,b] [--claims a,b] [--domain <d>] [--ref <id>]...
             [--workspace <w>] [--source <s>] [--force] [--json]
  kiomon import <file.jsonl|file.md|dir> [--workspace <w>] [--json]
  kiomon update <id> [--title|--body|--tags|--kind|--confidence|--metadata|--importance]
  kiomon link <src> <dst> [--relation <r>] | kiomon unlink <src> <dst>
  kiomon relate <id> [--relation <r>] [--depth 1]

Read
  kiomon search <query> [--kind k1,k2] [--workspace <w>] [--limit 10]
               [--legs auto|fts|vector] [--sort relevance|recent|strength]
               [--snippet-len 12] [--no-record] [--json]
  kiomon get <id> [--relations] [--json]
  kiomon list [--kind] [--status active|pending_approval|archived|any] [--tag]
              [--workspace <w>|'*'] [--limit 20] [--recent] [--json]

Lifecycle
  kiomon pin|unpin <id>...
  kiomon archive|restore <id>...
  kiomon rate <id> (+1|-1)
  kiomon review list [--json] | kiomon review approve <id>... | kiomon review reject <id>...
  kiomon outcome <success|failure> --item <id>:<assessment>... [--json]
  kiomon decay [--dry-run]

Portability
  kiomon export [--format jsonl|md] [--workspace <w>] [--status active] > brain.jsonl
  kiomon forget <id>... --yes            # the only destructive verb
```

### 6.2 Output discipline

- **Default output is for an agent's eyes and a human's terminal at once:**
  one-line-per-item, no tables, no color when `!isatty()`, no trailing whitespace.
  Width is bounded so a 4 KB memory never bloats a context window — `add` and
  `search` print at most ~120 chars per field unless `--full`.
- **`--json` is the machine contract:** always a single JSON document on stdout,
  newline-terminated, no logging mixed in. `{"ok":true,"data":…,"meta":{…}}` on
  success; `{"ok":false,"error":{"code":"…","message":"…"}}` on failure.
- **Diagnostics never pollute stdout.** Progress, warnings, and embedder failures go
  to stderr. A pipe into `jq` must always work.
- **Exit codes are stable:**

  | code | meaning |
  |---|---|
  | 0 | success (including a duplicate `add`, and an empty search) |
  | 1 | internal error (I/O, SQLite, corruption) |
  | 2 | not found |
  | 3 | invalid arguments / validation failure |
  | 4 | configuration or capability missing (e.g. `--legs vector` with no embedder) |

  The `2` vs `1` distinction matters: the `kiomon-memory` skill in this workspace
  already tells agents `get_memory` 404s mean *"it may have been archived or
  deleted"* — that semantic must survive as a distinct exit code, not a generic
  failure.

### 6.3 The agent handshake

`docs/AGENTS.md` contains a drop-in block instructing a coding agent to
(`add` after each learn → `search` before answering → `outcome` when the task
ends). That plus `CONTRACT.md` is what makes v1 usable by an agent today, without
an MCP server. v1.1's `kiomon mcp` then exposes the same operations as tools with
these same names and semantics.

---

## 7. The performance budget

**The ruler comes before the optimization.** Platform lesson, stated in its own
plan index: *"a corpus + metric harness first (the ruler)."*

`crates/kiomon-core/benches/` + `kiomon bench` (end-to-end, wall clock) measure, at
1k / 10k / 100k generated memories, with a realistic body-size mix (60% short ≤ 500 B, 30%
medium ≤ 8 KB, 10% long ≤ 100 KB). The shipped corpus builds 1k and 10k by default and
100k on request, because 100k is 1.9 GiB and takes a hundred seconds to generate:

- `open` — process + `Database::open` + PRAGMAs (this is pure overhead and it is
  the number that decides whether a daemon is needed)
- `add` — cold process → exit
- `search` — cold process → exit, per ladder attempt, per leg
- `get`, `list`, `relate`
- `decay --full` over the corpus
- RSS high-water and binary size

Gates in CI (absolute, because shared runners are noisy — a ratio gate is a
flaky-test generator):

| Metric @ 10k memories, all read against `work` | Gate |
|---|---|
| `open` p50 | ≤ 5 ms |
| `add` p95 | ≤ 40 ms |
| `search` p95 (fts) | ≤ 60 ms |
| `search` p95 (hybrid, local embedder) | ≤ 250 ms |

> **The budget measures work, not wall time, and WP1 showed why.** Process spawn alone
> is 8.6 ms on Windows, so a `search` that does nothing costs 8.6 ms and a `search` that
> does everything allowed costs 8.6 + `open` + the query. The harness must therefore
> report the spawn floor alongside the command time, or a fast machine and a slow
> machine will be compared as if the difference were the database's. It must also report
> the corpus's *body-size mix*: every measurement that mattered in WP1 only appeared
> because the bodies were 5 KB rather than 200 bytes.

CI additionally publishes the bench output as a job artifact so a regression is
visible in the diff even when it is under the gate.

> **WP3 read these gates, and changed two things about how.** They read `work` and not
> `total`: on Windows the spawn floor alone is 8.2 ms, so `open ≤ 5 ms` read literally is
> unreachable on a platform this project ships for, before any code runs. And CI reads them
> at **200 samples per row**, because on a general-purpose machine the 95th percentile of
> thirty samples is the twenty-ninth and one operating-system interruption decides the gate:
> the first `--gate` run of this suite failed at 60.41 ms against the 60 ms limit and
> measured 38.9 ms at n = 200. Both are argued in §8's WP3 entry, and both are stated in the
> report the command prints rather than left for a reader to discover.

**The optimization order is fixed, and each step is measured before the next:**
(1) the PRAGMAs above, (2) prepared statements + one transaction per command,
(3) `snippet()` in SQL instead of text processing in Rust, (4) `mmap` + 16 MB cache,
(5) only if `open` still dominates, the v1.2 daemon. Binary-size budget: `strip`,
`lto = "fat"`, `codegen-units = 1`, `panic = "abort"` in the release profile.

---

## 8. Work packages

Ordered so each is independently shippable and testable. Acceptance criteria are
what a reviewer checks — not what the executor reports.

### WP0 — Skeleton, CI, and the licence (½ day)
`[workspace]` with two empty crates, `rustfmt` + `clippy -D warnings`, GitHub Actions
matrix (linux-x86_64, linux-aarch64, macos-aarch64, windows-x86_64), LICENSE,
README stating the thesis and the three numbers, CONTRIBUTING imported from the org
`.github` repo.
**Accept:** `cargo build --release` on all four targets in CI; binary runs and
prints a usage error; `cargo clippy --all-targets -- -D warnings` is green.

### WP1 — Database, schema, and the store (2 days)
`Database::open` with the PRAGMAs, the migration applier, `model.rs`, `store.rs`
(add/get/update/list/forget), ULID ids, `content_hash`, tag cap, per-kind metadata
validation, `kiomon init`/`doctor`.
**Accept:** `cargo test` covers: migrations idempotent across reopen; add→get
round-trips every kind and every metadata field; unknown kind rejected; malformed
metadata degrades to `{}` and still writes; `forget` leaves zero rows in
`memories`, `memory_fts`, `memory_links`, `memory_embeddings`, `memory_outcomes`;
`integrity-check` passes after 1k writes and 1k purges (this is what catches a
wrong FTS trigger).

### WP2 — Lexical search and the JSON contract (2 days)
Sanitizer + three-attempt ladder, `bm25()` weights, `snippet()`, `fuse.rs`,
`out.rs` (human + JSON + exit codes), `docs/CONTRACT.md` v1.
**Accept:** a table-driven test proves the ladder (phrase → AND → OR) picks the
first non-empty attempt and reports which; CJK and accented queries return hits
(the platform's regex sanitizer's failure case); `--json` output parses and every
documented field is present; exit codes 0/2/3/4 each asserted; `search | jq` works.

### WP3 — The bench harness (1 day) — **the ruler**
`benches/` at three corpus sizes + `kiomon bench`, CI artifact, the gate table from §7.
**Accept:** the numbers are reproducible locally within ±15% across three runs on
the same machine; CI fails if a gate is exceeded; the README's three headline
numbers are replaced by measured ones or the README is corrected (**never leave a
claimed number unmeasured** — the platform's own `PRODUCT.md` principle 5).

### WP4 — Lifecycle: gate, review, feedback, decay (2 days) — **DONE**
`gate.rs` (authority, 0.75, review mode, dedupe), `review approve|reject` with
index-on-approve, `pin/archive/restore/rate`, `outcome`, `decay.rs`, and migration
`0002_lifecycle.sql` (`feedback_count`, `pending_derived_from`).
**Accept:** a `pending_approval` row is invisible to `search` (asserted in the
same test that asserts it becomes visible after `approve`); the decay formula is
asserted against hand-computed values at age = 0, 1, and 2 half-lives for all
three decaying kinds; `pinned` and `reference` rows never decay; `--dry-run`
mutates nothing; a `--workspace` with `review_mode='review'` sends a
`confidence=1.0` write to the inbox.

### WP5 — The graph: links, auto-link, relate (2 days) — **DEFERRED (§9-D9)**
`links.rs`, `link`/`unlink`/`relate`, the synchronous auto-link pass on activate,
`origin` tagging, `derived_from` from `source_refs`.
**Accept:** auto-link writes ≤5 edges and never a self-edge; `derived_from` edges
are directed and `related_to` are not; `relate` returns 1-hop neighbours with
relation + title + excerpt; purge cascades.

**Deferred to the next sprint by the owner on 2026-10-17.** Nothing below this line
changes; what changes is that v1 does not wait for it. What already exists, and is tested:
the `memory_links` table (directed and symmetric, with the `related_to` canonicalisation
rule), `add --ref` writing a `derived_from` edge, `forget` cascading to every edge that
touches the row, and a draft's `pending_derived_from` staging that `approve` wires up.
What v1 does not do: write `related_to`, write `contradicts`/`supersedes`, read any of it
back through a verb, or auto-link a memory against its neighbours. The three verbs and
`get --relations` therefore stay stubs — but they now say **deferred**, because a message
naming work package 5 of an in-flight plan reads as "coming in this release".

### WP5.5 — `retrieveMemory`: the intent-aware packet (1 day) — **in v1, per §9-D8**
The `retrieve` verb and the packet: `summary`, `facts` / `procedure` (**one**, not a list)
/ `episodes`, `evidence`, `warnings`, `conflicts`, `retrieved_ids`; every item carrying
`memory_id`, `kind`, `claim`, `confidence`, `authority`, `applicability` and optionally
`condition` / `source_ref`. `--intent execute|plan|answer|verify|debug` decides which kind
leads, how the candidates are re-ranked, and how deep each bucket goes. Pure composition
over `search`, the kinds, the envelope and the typed edges — no model, no network, no
provider.

> **Corrected before it was written.** This block said the intents were
> `understand|execute|recall`. The contract has five — `execute`, `plan`, `answer`,
> `verify`, `debug` — and they are the keys of `INTENT_KIND_PRIORITY` in the platform's
> `worker/src/lib/retrieve.ts`, which is what `/api/retrieve` *and* the MCP tool validate
> against (`platform/mcp/src/handlers/retrieveMemory.ts`, the tool's own JSON schema). A
> packet fetched by an SDK saying `verify` and answered with a 400 would be a poor first
> impression for a self-hosting backend, so the five are the contract and the three were a
> misremembering. The rest of what only reading the implementation could have told us is in
> WP5.5's record in §8.

**Where the graph was assumed.** The platform's packet reads `supersedes` and `contradicts`
edges to fill `conflicts` and to warn about superseded memories, so the one place WP5.5
depends on WP5 is three statements over `memory_links`. With the graph deferred (§9-D9)
nothing writes those two relations, so the local packet reports `conflicts: []` and no
superseded warnings — **the truth, not a stub.** The reads stay, because they are indexed
seeks over the candidate ids, they cost less than the `warnings` list they feed, and
deleting them to re-add them with WP5 is how a capability gets forgotten.

**Accept:** one query under two intents returns the same underlying ids in a different
shape (set equality on ids *and* inequality on section order, in one test); a
`pending_approval` memory never reaches a packet; the JSON field names match the hosted
handler's, asserted against a fixture transcribed from the platform and checked in; the
packet is assembled in a bounded number of statements whatever the corpus size — a packet
that costs one query per hit is the thing that would make `search`'s 26.8 ms look cheap;
and `retrieve` → `outcome` works as one loop, because `retrieved_ids` is exactly what
`outcome` is fed.

### WP6 — Import, export, and portability (1 day)
JSONL in/out, markdown out, directory import, `--workspace` mapping, id
preservation on import.
**Accept:** export → fresh DB → import → `search` returns the same ids and titles;
import of a partial/hostile JSONL (missing fields, wrong types, huge body)
reports per-line errors on stderr and imports the valid lines; a 100 MB export
streams without loading it into memory.

### WP7 — Vector leg (2 days, **in v1 per §9-D1**)
`Embedder` trait, Ollama + OpenAI-compatible over `ureq`, `vector.rs` brute-force
cosine, `--legs auto|fts|vector`, fusion with two legs, `degraded` reporting,
`kiomon reindex --embed`.
**Accept:** with no embedder configured, `--legs vector` exits 4 with a message that
names the config key; with one configured, hybrid returns rows the lexical leg
misses (a semantic-only query fixture); a failed embed never fails a write and says
so on stderr; a `--legs vector` request is never answered from the lexical leg.

### WP8 — Packaging and release (1 day)
`cargo-dist` (or `release.yml` + `taplo`-style cross-compile), shell + PowerShell
installers, `kiomon init` onboarding text, crates.io publish for `kiomon-core`.
**Accept:** a clean machine installs via one command and `kiomon init && kiomon add
&& kiomon search` works; `kiomon --version` reports a git sha; the release workflow
builds all four targets from a tag with no manual steps.

### WP9 — `serve` + `mcp`: two transports, one verb layer (2½ days) — **v1.1, per §9-D8**
Loopback HTTP/1.1 — `std::net` and threads, no async runtime — answering the hosted
`/api/*` routes in Appendix C from the same functions the CLI calls, plus stdio MCP over
the same layer, plus a token file in the data directory and `--port 0` printing the port
it chose. Broad enough that the two published SDKs work unchanged; honest enough that the
routes this engine cannot answer say so.
**Accept:** `../../SDK/kiomon-py`'s and `../../SDK/kiomon-ts`'s own test suites pass against
`127.0.0.1:<port>` in CI; every route in Appendix C either answers or returns
`unsupported_locally`, asserted route by route, so a route cannot be forgotten into a
404 that reads like an empty success; a request without the token and a request with a
foreign `Origin` are both refused; an MCP session lists the same operations the CLI has;
`serve` adds no dependency with an async runtime and the binary stays inside §7's budget.

**v1 = WP0–WP4 + WP5.5 + WP6–WP8 complete (WP5 deferred, §9-D9). Estimated 2–3 focused
weeks.** WP3 precedes WP4 by
design: the ruler has to exist before there is anything worth measuring. WP5.5 is in v1
because `retrieveMemory` is the verb both SDKs lead with and because it is the payload
WP9 exists to deliver (§9-D8) — building it after the socket would design the flagship
shape last.

### Progress

| WP | Status | Evidence |
|---|---|---|
| **WP0** | ✅ **DONE** 2026-10-14 | Commit `e951d23`. Skeleton, CI, licence, hygiene, and the full CLI surface. `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` green. |
| **WP1** | ✅ **DONE** 2026-10-14 | Database, pragmas, migration applier, integrity checks, store (`add`/`get`/`update`/`list`/`forget`/`stats`), ids, content hash, metadata envelope, write gate, workspace resolution, and the CLI verbs `init`, `doctor`, `stats`, `add`, `update`, `get`, `list`, `forget`. **158 tests**, clippy clean. `init`/`add`/`get`/`list`/`stats`/`forget`/`doctor` exercised end to end against real database files in `crates/kiomon-cli/tests/cli.rs`. |
| **WP2** | ✅ **DONE** 2026-10-14 | `search.rs` (the quoted ladder, the ranked window, the snippet query, the retrieval record), `fuse.rs` (RRF, ported from `rrf.ts`), the filter and ordering flags, `docs/CONTRACT.md` §Reading rewritten as v1, and the `--json` shapes for a search. **204 tests** (`cargo test --workspace`), clippy clean, `cargo fmt --check` clean. Exit codes 0/2/3/4 each asserted through the binary, `search | jq`'s property asserted rather than assumed. |
| **WP3** | ✅ **DONE** 2026-10-14 | `kiomon bench` (cold-process timings, the plan's gates, `--json`, the corpus generator it ships with), `crates/kiomon-core/benches/store.rs` (in-process, no Criterion — see below), a CI job that fails on a missed gate and publishes the numbers as an artifact, and the README's numbers replaced with measured ones. **146 core + 9 integration tests**, clippy clean. |
| **WP4** | ✅ **DONE** 2026-10-16 | `lifecycle.rs` (the review inbox, `pin`/`archive`/`restore`/`rate`/`outcome` with the hosted product's credit rules), `decay.rs` (the formula, the paged command, the resumable daily nudge), migration `0002_lifecycle.sql`, the five lifecycle commands wired end to end, `doctor` reporting when decay last ran, and the CONTRACT's lifecycle section rewritten around what the verbs *actually* move. **274 tests** (192 core + 58 CLI + 24 others), clippy clean, 3 324 KiB binary. |
| **WP5** | ⏸ **DEFERRED** to the next sprint (§9-D9, 2026-10-17) — a scope cut, not a design change: the spec below is unchanged and is the next sprint's first work package | |
| **WP5.5** | ⬜ **NEXT** — the packet the SDKs lead with | |
| **WP6** | ⬜ TODO | |
| **WP7** | ⬜ TODO | |
| **WP8** | ⬜ TODO | |
| **WP9** | ⬜ TODO (v1.1) | |

### The numbers so far, and what they cost

Measured on one developer machine — Intel Core i5-9300H, Windows, `--release`, warm
OS cache, **5 000 memories with 5 KB bodies (43.5 MiB)**. The command times include
process spawn, which alone is **8.6 ms** on this platform and is therefore most of
what a fast command costs. Read the difference, not the total.

| Command | p50 | p95 | Budget (§7) |
|---|---|---|---|
| `--version` (spawn only) | 8.6 ms | 11.3 ms | — |
| `add` | 12.5 ms | 14.8 ms | p50 ≤ 15, p95 ≤ 40 ✅ |
| `get` | 11.6 ms | 14.7 ms | — |
| `init` (re-open, idempotent) | 12.1 ms | 16.3 ms | — |
| `list` (20) | 14.8 ms | 25.3 ms | — |
| `list --recent` (20) | 14.2 ms | 23.0 ms | — |
| `stats` | 14.3 ms | 19.5 ms | — |
| `list --workspace '*'` (20) | **55.7 ms** | **91.9 ms** | — ⚠ |
| `doctor` | 262 ms | 329 ms | — |
| bulk `add`, sustained | 36.6 ms each | | ⚠ |

Binary **3 231 KiB** against the 6 MB budget (WP3 added the corpus generator and the
harness, and the binary grew 257 KiB for them); **33** transitive crates against the 60-crate
budget; ~8.7 KiB stored per 5 KB memory, which is the full-text index's second copy of the
text plus overhead.

### WP3's numbers, and the five things the ruler found

The corpus is the deliverable here, not the table. `kiomon bench` prints what it built
before it times anything: the size distribution it actually produced against the one it was
asked for, and how many rows each probe query actually matched. That last column is the one
that makes the times mean something — WP2's first corpus matched every term against all ten
thousand memories, and a corpus like that cannot tell a ranking from a scan.

**Ten thousand memories, 197 MiB on disk, 7.67 KiB mean body, 1 446 rows matching
`deploy`.** Same machine and same reading rule as before: `work` = `total` − the spawn
floor, measured moments before each sample.

| Row | work p50 | work p95 | total p50 |
|---|---|---|---|
| `--version` (the spawn floor) | — | — | 8.2 ms |
| `open` | 2.0 ms | 5.2 ms | 10.2 ms |
| `add (cold file)` | 16.6 ms | — | 23.9 ms |
| `add` | 3.0 ms | 6.6 ms | 11.2 ms |
| `get` | 2.5 ms | 6.1 ms | 10.6 ms |
| `list` | 2.9 ms | 7.7 ms | 11.1 ms |
| `list --recent` | 2.8 ms | 7.2 ms | 10.9 ms |
| `stats` | 4.4 ms | 8.8 ms | 12.4 ms |
| `search absent` (0 rows) | 3.3 ms | 8.2 ms | 11.4 ms |
| `search cjk` (10 rows) | 14.6 ms | 21.0 ms | 23.1 ms |
| `search rare` (40 rows) | 13.3 ms | 19.8 ms | 21.6 ms |
| `search uncommon` (250 rows) | 15.5 ms | 21.3 ms | 23.7 ms |
| `search common` (1 446 rows) | **26.8 ms** | **38.9 ms** | 34.9 ms |
| `search common --no-record` | 18.0 ms | 26.3 ms | 26.4 ms |
| `search common --limit 50` | 21.9 ms | 33.1 ms | 30.5 ms |

And at **one hundred thousand** memories, 1.9 GiB, 14 457 rows matching `deploy`:

| Row | work p50 | work p95 |
|---|---|---|
| `open` | 1.9 ms | 4.4 ms |
| `add` | 2.2 ms | 3.9 ms |
| `get` / `list` | 2.0 / 2.4 ms | 3.7 / 3.5 ms |
| `stats` | 19.7 ms | 27.8 ms |
| `search cjk` (100 rows) | 17.4 ms | 21.4 ms |
| `search rare` (400 rows) | 24.6 ms | 28.3 ms |
| `search uncommon` (2 500 rows) | 56.7 ms | 62.1 ms |
| `search common` (14 457 rows) | **187.2 ms** | 205.0 ms |
| `search common --limit 50` | 179.5 ms | 192.4 ms |

1. **A search costs what it has to rank, and `--limit` does not bound it.** The corpus makes
   this visible on purpose — its planted terms span four orders of magnitude of selectivity
   — and the numbers are almost exactly linear in the match count: 0 rows 3.3 ms, 40 rows
   13 ms, 250 rows 15.5 ms, 1 446 rows 26.8 ms, 2 500 rows 56.7 ms, 14 457 rows 187 ms. At
   100 000 memories, **`--limit 50` costs 179 ms against `--limit 10`'s 187** — the page
   size is nearly free and the window is everything. The plan's gates are written at 10 000
   rows where they hold with room; what has to be watched as a brain grows is the *match*
   count, and nothing in v1 bounds it. The fix is not this work package's to make — the
   honest v1 statement is that a common word in a large brain is slow, that the number is
   measured, and that `docs/CONTRACT.md` says so rather than implying otherwise.
2. **The retrieval record is a write, and it costs 8.8 ms of a 26.8 ms search.** In-process
   the same UPDATE costs 0.05 ms; the other 8.75 is the commit. That is 33 % of every search
   spent on a counter, and the counter is what stretches the decay clock — so it is
   mechanism rather than telemetry, and it is also the single cheapest thing in the whole
   read path to make optional. `--no-record` exists for callers that only want to look.
   **WP4 owns the decision**: it owns decay, and the question "should a search that nothing
   acts on still move a memory's clock" is a lifecycle question and not a performance one.
3. **The first write to a file costs 16.6 ms; the second costs 3.0.** Creating the database,
   its write-ahead log and its shared-memory file is the expensive part, and it happens once
   per *file* and not once per process — which is why the measured `add` rows are cheap on a
   corpus that already exists while `init` on a fresh path costs what the plan's WP1 numbers
   said it did. It is a row in the report because it is the one cost that a user meets
   exactly once and then never again, and because a benchmark that hid it would make the
   first-run experience look like the steady state.
4. **The daemon question, answered with numbers.** `open` costs 2.0 ms and an in-process
   `add` costs 0.048 ms, so **roughly 80 % of an `add` is opening the database** — the
   per-process design is what a daemon would fix, and for a write-heavy agent loop it is
   worth about 2 ms per command. For `search` it is worth 7 % of the total, and the thing
   that actually costs the time (ranking 1 446 rows) would not change at all. §7 asks for
   the `open` gate precisely so this trade can be seen before a daemon is built; the answer
   is that it depends entirely on which command the agent is running, and that no v1.2
   daemon should be built on an averaged figure.
5. **p95 on a general-purpose computer measures the computer.** The three controls that
   settle it: `--version` — which opens no file, allocates almost nothing and prints one
   line — reaches **47 ms** against a 9 ms median; `search --no-record` (a pure read)
   reaches 84 ms; `get` reaches 114 ms. The spike arrives at roughly one in forty spawns on
   this laptop and lands wherever it lands, and at thirty samples per row the 95th
   percentile *is* the 29th sample, so a single interruption decides a gate.
   **This is why the CI job reads two hundred samples** — ten samples of slack at the 95th
   percentile — and why the medians are the numbers to compare across machines. The first
   `--gate` run of this suite failed at `search common` p95 = 60.41 ms against a 60 ms
   limit; at n = 200 the same binary on the same corpus measured 38.9 ms.

### WP3's acceptance criteria

| Criterion | Result |
|---|---|
| The numbers are reproducible locally within ±15% across three runs on the same machine | ✅ **for every row whose work exceeds about 4 ms, and provably not below that.** At `--runs 50 --repeat 3`, the per-pass work medians of the read rows agree within 5 %: `search common` 27.2 / 27.4 / 28.3, `search (no record)` 18.0 / 19.1 / 17.9, `search rare` 14.0 / 13.9 / 13.5, `stats` 3.9 / 4.9 / 4.5. The rows that do not agree are the ones whose *work* is 2–3 ms (`open`, `list`, `get`): ±15% of 2.5 ms is ±0.4 ms, and this machine's floor moves by twice that between two passes. **The criterion is met above the measurement's resolution and cannot be met below it**, which is a property of the platform and not a defect in the harness. Both facts are printed on every run. |
| CI fails if a gate is exceeded | ✅ a `performance budget` job runs `kiomon bench --sizes 1000,10000 --runs 200 --gate --json` and exits non-zero when a gate is missed; the artifact is uploaded even when it fails, and the numbers are echoed into the log so a regression is visible in a diff. |
| The README's three headline numbers are replaced by measured ones | ✅ every number in the README's table now comes from `kiomon bench`, with the corpus's shape, the match counts and the machine named beside it. |

### Where WP3 contradicted the plan

- **No Criterion.** §7 says `crates/kiomon-core/benches/` (criterion) and the plan is right
  that an in-process ruler is worth having; it is wrong about the tool. Criterion would add
  **twenty-two crates** to a build whose whole shipped dependency list is thirty-three, it
  is compiled by `cargo test` on three operating systems in CI for benchmarks that CI does
  not run, and its statistical machinery answers a question this project is not asking —
  it detects 2 % changes in microsecond-scale functions, and every gate here is a
  cold-process budget at tens of milliseconds. `benches/store.rs` is sixty lines of nearest
  rank and a warm loop, and the plan's real requirement — a repeatable in-process number —
  is met. Recorded here rather than quietly swapped.
- **`kiomon bench` ships in the released binary.** The plan lists it as a harness without
  saying who can run it. It is in the shipped command set because a number nobody can check
  is a claim, and because `kiomon bench` on a reader's own machine is the difference between
  a README they have to believe and one they can falsify. It is documented as the only
  command that exists for this project's benefit rather than a user's.
- **The gates are read against `work`, not `total`.** §7 says `open ≤ 5 ms`, and on Windows
  the process spawn alone is 8.2 ms — so read literally, the gate is unreachable on a
  platform this project ships for, before any code runs. The plan's own note about the spawn
  floor says why, and this is that note turned into arithmetic.
- **The corpus is on the system volume, and the path is in the report.** §7 does not mention
  where the corpus lives. WP2 measured a forty-fold difference in write cost between two
  disks holding the same file, so the corpus's location is a measurement parameter and the
  report prints it.
- **`--repeat 3` is not the default, and one warm-up pass always runs.** The plan's budget is
  wall-clock, and building a 197 MiB corpus leaves the kernel flushing for a minute
  afterwards: the `add` row's median was **17.1 ms in the first pass and 3.5 ms in the
  second**, which reads as an unstable command and is an unflushed benchmark. One pass is
  now run and discarded before the measured ones, which is what makes every published figure
  a warm-cache figure.
- **Two of the plan's §7 rows are reported as unmeasurable rather than dropped.** `relate`
  (WP5) and `decay --full` (WP4) are printed under "not measured by this build", and the
  hybrid-search gate is printed with the reason it cannot fail yet — a budget with a silent
  hole in it is a budget that looks better than it is.

### What WP4 inherits

1. **The retrieval record costs 8.8 ms of a 26.8 ms search** (finding 2 above), and whether
   a read that nothing acts on should move a memory's decay clock is WP4's question.
2. **`stats` is 4.4 ms at 10 000 rows and 19.7 ms at 100 000** — it reads count aggregates,
   and at 100 000 it is the second most expensive command in the report. WP4 adds columns to
   that table (strength updates, expiry) and will be tempted to add more counts to the same
   query.
3. **`add` is 3.0 ms and `open` is 2.0 of it** — a lifecycle write that adds a second
   statement or a second index lookup per add is adding it to the smaller half.

### WP4's numbers, and the six things it found

10 000 memories × ~1 KB on the same laptop as every other table here (release build, warm
cache, `C:`), read against `work` so the 8.38 ms spawn floor is not counted as the
database's:

| Command | work p50 / p95 (ms) | total p50 |
|---|---|---|
| `open` | 2.45 / 4.84 | 10.89 |
| `add` | 3.36 / 6.02 | 12.00 |
| `add` (first write to a new file) | 20.57 / 23.04 | 28.46 |
| `search common` | 28.16 / 83.78 ⚠ | 36.86 |
| `get` | 2.69 / 7.75 | 10.88 |
| `stats` | 4.74 / 12.69 | 13.03 |
| **`decay --dry-run` over the corpus** | **68.05** / 84.51 | **77.31** |

Six findings, in the order they arrived:

1. **A nudge costs pages, not rows.** The first write of the day paid **62 ms** more
   than an `add` with nothing to do when the opportunistic page was 500 rows, and
   **48 ms** when it was cut to 100 (medians of twenty and fifteen samples, same binary,
   same corpus, marker deleted between samples). Cutting the page by 80% saved 23% of the
   cost, because the price is the pages a write touches on a cold cache — scattered across
   a 200 MB file by the two indexes that hold `strength` — and not the number of rows. It
   also means the nudge has an irreducible floor of about 50 ms on a corpus this size,
   which no page size fixes.
2. **The nudge was rewriting the same hundred rows forever.** A page from the beginning
   of the id order is the same page every day, and rows that never fall below the floor
   never leave the scan — so the nudge reached 100 of 10 000 memories and no more. Fixed
   with a cursor in `meta` that resumes where the last nudge stopped and wraps when a lap
   ends; the test asserts the two pages do not overlap.
3. **A read must not run it.** §5.4 put the nudge on `add` *and* `search`. Forty-eight
   milliseconds of hidden write on the read path is exactly what §10's "a read nothing
   acts on should not move a memory's decay clock" was asking about, so `search` no longer
   does it. This is WP3's second finding, answered.
4. **§5.3's strength multipliers could not have worked.** Decay recomputes `strength` from
   `importance` on every pass, so a rating's multiplier would be erased by the next
   `kiomon decay` — the value would look like a memory and behave like a scratch variable.
   The hosted product had already solved this the other way: use stretches the half-life
   (through `retrieval_count`), and *quality* moves `confidence`. That is what is ported.
5. **A running average needs a denominator, and `0001` had shipped.** `feedback_count` is
   new in `0002`; without it, `unused` would be a no-op and twenty ignored retrievals would
   look exactly like never having been asked. **The first schema change after the first
   release is a migration, not an edit** — the discipline §3.3 asked for, exercised for
   real.
6. **The p95 gate fails at fifty samples on a busy machine.** `search common` p95 read
   83.78 ms against the plan's 60 ms limit while its p50 was 28.16 ms, in a run whose
   floor was 8.38 ms and whose worst `search` total was 204.70 ms — the same
   interruption-driven spread WP3's fifth finding describes. The gate is read at 200
   samples in CI for this reason, and the harness's `--runs` help says so.

Two smaller corrections, both in the code: an opportunistic nudge that changes anything
now says so on stderr (a silent write is a write nobody can account for), and the decay
pass skips rows whose strength would move by less than a fiftieth of the floor — without
that epsilon the recomputed value never equals the stored one and *every* pass rewrites
the entire corpus (`1e-9` was the first guess; the measured cost is why it is `5e-4`).

`decay` is the most expensive row in the report and the only one that scales with the
whole corpus: 68 ms at 10 000 memories, one page of 500 at a time, reading every active
non-reference row. It is not gated — the plan's §7 gates `open`, `add` and `search` — and
the recorded trigger is a covering index over
`(status, pinned, kind, id, importance, retrieval_count, strength, created_at,
expires_at)` if decay ever becomes a gate or runs more often than daily. It is not worth
one now: a wide index would slow every write in the system to speed up a maintenance
command that runs once a day and costs 68 ms.

### WP4's acceptance criteria

| # | Criterion | Where it is asserted |
|---|---|---|
| 1 | A `pending_approval` row is invisible to `search`, in the same test that asserts it becomes visible after `approve` | `lifecycle::tests::a_draft_is_not_indexed_and_an_approval_indexes_it` (on the index itself, not on a query), `a_draft_is_invisible_to_search_until_it_is_approved` through the binary |
| 2 | The decay formula, hand-computed at age 0, 1 and 2 half-lives for all three decaying kinds | `decay::tests::the_formula_halves_at_every_half_life`, `decay_weakens_a_row_by_the_formula`, `a_retrieved_memory_outlives_the_same_memory_unretrieved` |
| 3 | `pinned` and `reference` rows never decay | `a_pinned_memory_is_never_scanned_and_never_archived`, `a_reference_memory_is_never_scanned_and_never_archived`, `a_row_with_no_kind_to_decay_is_left_out_of_the_scan_entirely` |
| 4 | `--dry-run` mutates nothing | `a_dry_run_reports_the_truth_and_writes_nothing` (row equality, and no `meta` marker), `a_decay_dry_run_reports_the_scan_and_writes_nothing` through the binary |
| 5 | A `review_mode = 'review'` workspace sends a `confidence = 1.0` write to the inbox | `store::tests::a_review_workspace_holds_back_even_a_certain_write`, plus `lifecycle::tests::the_inbox_can_be_filtered_by_confidence_and_scope` for what the inbox then contains |

Beyond the plan's five: a draft's `--derived-from` survives until approval and is
discarded on rejection; a restored memory has `expires_at = NULL` so the next pass does
not re-archive it; `rate` never moves `strength`; `unused` never invents a confidence;
the state verbs exit `2` when nothing moved and `0` when something did; and the nudge
walks rather than loops.

### Where WP4 contradicted the plan

1. **§5.3's feedback tables were wrong in four ways** (strength multipliers, `±1` score,
   `corroboration_count` on `helpful` only, and a `strength` that was never touched at
   all). §5.3 now carries the corrected table and a pointer to this record.
2. **`feedback_count` was not in `0001`.** A running average needs its denominator, so
   WP4 ships the first migration this repository has ever needed. The estimate was one
   day; a migration plus the credit rules plus the review inbox was two.
3. **`unused` does not write confidence, and the hosted product does.** Its fallback of
   `0.5` is invented data on a row whose writer claimed nothing. Deliberate divergence,
   asserted by test.
4. **`restore` clears `expires_at`, and the hosted product does not.** Keeping the deadline
   means the next decay pass re-archives the row, so its restore does not work for exactly
   the memories most likely to need one. Deliberate divergence, asserted by test.
5. **`search` does not run the nudge** (§5.4 corrected), and neither does anything else
   except `add`.
6. **The nudge is not "at most one page"** — it is 100 rows, a resumable cursor, and an
   explicit statement in the code that it is not the mechanism.
7. **`doctor` grew a `decay` line.** The plan's doctor spec was "integrity, schema version,
   config, embedder probe"; the decay timestamp is the fifth thing a user needs, because
   the opportunistic nudge cannot keep up and the explicit command can.
8. **The plan's `decay --full` row is measured as `decay --dry-run`.** A real pass rewrites
   almost every strength in the corpus, which would make `--repeat` compare a corpus with
   what it became; the read cost is what scales with the corpus, and the write cost is
   `add`-shaped per row that moved.
9. **`docs/DESIGN.md` still does not exist.** §2 lists it and §5.3 referenced it as the
   place the formulas are documented; `docs/CONTRACT.md` carries the formulas for callers
   and `lifecycle.rs`/`decay.rs` carry them for maintainers. Still a WP8 item.

### What WP5 inherits

1. **The auto-link pass is untouched and still specified in §5.2.** It belongs on the
   activation path, which now has two entries — `add` (already wiring `derived_from`) and
   `approve` (wiring the staging column) — so the hook goes where `link_derived_from` is,
   not in one of the two callers.
2. **`relate` is the last deferred row in the bench report.** `get --relations` still exits
   4 naming WP5, and the harness prints `relate — no graph until WP5`. WP4 removed the
   `decay --full` line from that list; WP5 removes the last one.
3. **A draft's edges are never written, by construction.** `link_derived_from` is only
   reached for an active row or from `approve`, so `relate` cannot leak a draft's title
   without WP5 adding a new path — and if it does, the test that guards the inbox is in
   `lifecycle.rs`.
4. **The cross-workspace ordering gap is still open.** `list --workspace '*'` was 55.7/91.9
   ms in WP1 and no index in `0001` covers `ORDER BY pinned DESC, strength DESC, id DESC`
   without a leading `workspace_id`. WP5 is the natural place to decide, since `relate`
   will add its own cross-workspace query.
5. **`gate.rs` has never been exercised through the CLI.** The review inbox is tested
   through `add` with `--source agent_reflection --confidence 0.4` and through a
   review-mode workspace in `store.rs`; the three authority sources, the threshold
   boundary, and the unrecognised-source default are unit tests only.

### WP2's numbers, and the four things WP2 found

Same machine and same reading rule as above — subtract the 8.9 ms spawn floor. **10 000
memories, 47 MiB, one memory in seven mentioning `deploy`**, so a query for it has a
real ranking problem rather than a scan. The corpus is generated rather than collected,
which is the weakest thing about these numbers and WP3's first thing to fix.

| Command | p50 | p95 | Budget (§7) |
|---|---|---|---|
| `search deploy` | 26.0 ms | 31.8 ms | — |
| `search` (five-word sentence, rung 3) | 30.8 ms | 35.6 ms | p95 ≤ 60 ms ✅ |
| `search` (no match at all) | 12.7 ms | 14.6 ms | — |
| `search --no-record` | 21.5 ms | 24.6 ms | — |
| `search --kind semantic` | 29.0 ms | 37.9 ms | — |
| `search --workspace '*'` | 29.8 ms | 36.7 ms | — |
| `search --limit 50` | 48.1 ms | **63.7 ms** | — ⚠ |
| `list` (20) | 12.6 ms | 16.2 ms | — |
| `stats` | 14.1 ms | 21.0 ms | — |

Each search is roughly 17 ms of work over the floor: about 9 ms for the ranked window,
under 1 ms for the snippets, and the rest for the row loads and the retrieval record. At
`--limit 50` the window is 150 rows and the p95 crosses the gate, which the contract now
says out loud rather than leaving to be found.

1. **The plan was wrong about `snippet()`, and the error was 34 ms.** §4.4 says
   "`snippet()` comes from FTS5 — zero extra cost, no text processing in Rust". It is
   not free. `snippet()` is an FTS5 auxiliary evaluated in the select list, and SQLite
   evaluates a select list for every row the `WHERE` clause admits — *before* the
   `ORDER BY` and `LIMIT` that throw most of them away. So one query ranking and
   snippetting at once tokenised 9 000 matching bodies to keep ten: **47 ms against
   13 ms** for the same query without it. The snippet is now a statement of its own, run
   for the page and not the window, because a `MATCH` inside an `IN` list is re-evaluated
   per rowid — measured at 0.8 ms per row, linear, which is exactly why the page and not
   the window is the right thing to ask about. 34 ms → 0.3 ms.
2. **The first write in a process is not a property of the code.** A search records a
   retrieval, so a search is a write. Measured on the volume this repository lives on,
   the first write in a fresh process cost **47–145 ms** and the connection close another
   **49–105 ms**, against **2.9–5.9 ms** and 6 ms for the same binary on the same
   database file copied to `C:`. Same code, same SQLite, same 49 MiB file, forty times
   apart. Every benchmark here therefore runs on `C:`, and **WP3 must record the volume
   and not only the machine** — a budget can be met or missed by which disk the harness
   happened to use, and that is a bigger effect than anything §7 is trying to measure.
3. **The first corpus could not measure anything.** Its bodies were drawn from the same
   twenty-nine words, so *every* term matched all ten thousand memories and `deploy`
   matched 10 000 rows. A corpus like that cannot tell a ranking from a scan — it was
   found by printing the match counts, not by suspecting the generator. The corpus now
   gives each memory its own vocabulary out of a three-thousand-word pool, with `deploy`
   in one memory in seven. WP3 must assert the shape of its corpus, or the harness will
   report a number that means nothing.
4. **Rank-fusion scores compress to nothing with one leg.** `1/61`, `1/62`, `1/63`: the
   ordering is exactly bm25's, but the magnitudes carry no information. RRF is still the
   right call — it is what makes the score's *meaning* survive the vector leg arriving in
   WP7 — but the contract now says the score is only comparable within one response, and
   the human output omits it entirely rather than printing three meaningless decimals.

Two smaller corrections:

- **`record_retrieval` is one statement with an `IN` list, not one per id.** 0.29 ms for
  ten against 0.76 ms for ten of their own, and a page can be fifty. Both plans are an
  index search on the unique id, so the saving is per-statement overhead and not a better
  plan — worth saying, because "batch the writes" is usually advice about a plan change
  and here it is not.
- **`refresh` the corpus's `ANALYZE`.** The probe ran it and the applier's own corpus did
  not, and the two disagreed about the join order. Nothing depends on the difference
  today; WP3 should decide whether `analyze` belongs in the migration applier.

### WP2's acceptance criteria

| Criterion | Result |
|---|---|
| A table-driven test proves the ladder picks the first non-empty attempt and reports which | ✅ eight rows in `search::tests::the_ladder_takes_the_first_rung_that_returns_anything`, covering rung 1, rung 2, rung 3, exhaustion and punctuation-only, each asserting the rung *and* the hit count |
| CJK and accented queries return hits (the platform's sanitizer's failure case) | ✅ `日本語` reaches `日本語のメモ` through rung 2, `café`/`cafe`/`CAFÉ` all reach `Café hours`, asserted both in the library and through the binary. The failure case is pinned too: a Japanese *suffix* is unreachable, because `unicode61` does not segment CJK, and §10 records the trigger |
| `--json` output parses and every documented field is present | ✅ `the_search_json_carries_every_field_the_contract_documents` walks both lists from the document itself rather than from the struct |
| Exit codes 0/2/3/4 each asserted | ✅ `the_search_exit_codes_are_the_contract_codes`: 0 found *and* 0 not-found, 2 for an unknown workspace, 3 for four rejected arguments, 4 for `--legs vector` |
| `search \| jq` works | ✅ asserted as the property and not the tool: stdout is one newline-terminated JSON document, every diagnostic is on stderr, and the human path keeps stdout for the answer |

### Where WP2 contradicted the plan

- **§4.1's ladder is as designed, and the rung-1/rung-2 split earns its keep for a
  reason the plan did not name.** For a single term the two rungs are different
  expressions (`"deploy"` and `"deploy"*`), and that is what makes the ladder more than
  a fallback: `deploy` as a phrase finds the token, and `deploy*` finds `deploys`.
- **§4.4's row shape gained `workspace_id`**, because `list` carries it and the contract
  now promises that a search result *is* a list row with two fields added — a promise
  that is enforced by `SearchHitJson` flattening `MemorySummaryJson` rather than by
  retyping it.
- **§4.3's "pinned ranks above everything" is now "above `relevance` and `strength`"**,
  for the reason WP1 gave when `list --recent` was corrected: a recency view that
  silently reorders itself is not a recency view.
- **§4.2's `VECTOR_SCAN_LIMIT` and `degraded` are declared but unused**, which is what
  WP7 is for. `degraded` is a documented constant `false` rather than an absent field,
  so the shape does not change when there is an embedder to degrade.
- **`retrieval_count` recording moved from §5.3's neighbourhood into WP2**, because §4.3
  puts it on the read path and the decay clock it feeds is WP4's. The *stretch factor*
  that consumes it is still WP4's.
- **A snippet marks every term in the expression, including stop words.** Searching
  "why did the deploy fail" highlights the three `the`s in the body as well as `deploy`,
  because that is what FTS5's `snippet()` does: it marks what matched, and `the` did
  match. Suppressing it needs a stop-word list, and a stop-word list is a language
  commitment this project has not made — `unicode61` is deliberately language-neutral
  and §10 is where the trigger for changing that belongs. Left as it is, noted so the
  next reader does not file it as a bug.

Three of the older WP1 findings need a decision that only WP3's harness should make:

- **`list --workspace '*'` at 55.7 ms** is a full scan plus a sort of the corpus. The
  unscoped ordering `pinned DESC, strength DESC, id DESC` has no matching index, and
  `idx_mem_order` cannot serve it because an unused leading column (`workspace_id`)
  means the index is not ordered by the remaining columns. The fix is
  `(status, pinned DESC, strength DESC, id DESC)`; whether it is worth a fourth index
  on a table whose write path is already 36 ms is WP3's question, not this one's.
- **`doctor` at 262 ms** is `PRAGMA integrity_check` and FTS `integrity-check`
  visiting every page. That is O(corpus) by nature and correct, but it is not
  documented as such anywhere a user would look.
- **Sustained `add` at 36.6 ms** against 12.5 ms for a single add is the interesting
  one: the rate is flat from 1 000 to 5 000 memories, so it is not growing index
  maintenance — it is a constant per-write cost that a single invocation does not pay.
  WAL checkpointing is the first suspect and is unmeasured.

### WP1's acceptance criteria

| Criterion | Result |
|---|---|
| Migrations idempotent across reopen | ✅ three opens asserted, plus a workspace's review mode surviving one |
| `add`→`get` round-trips every kind and every metadata field | ✅ unit test per field, plus an integration test over all four kinds |
| Unknown kind rejected | ✅ exit 3, and the message lists the valid ones |
| Malformed metadata degrades to `{}` and still writes | ✅ with the warning on stderr and the memory intact |
| `forget` leaves zero rows in `memories`, `memory_fts`, `memory_links`, `memory_embeddings`, `memory_outcomes` | ✅ by cascade and trigger, asserted after both a single and a bulk purge |
| `integrity-check` passes after 1k writes and 1k purges | ✅ `crates/kiomon-core/tests/store.rs`, purging in a different order than the writes |

### Where WP1 contradicted this plan

Recorded rather than quietly fixed, because the reasoning is the part a reader needs.

1. **`memory_fts` is not an external-content table, and §3.2 said it would be.** The
   plan's own WP1 criterion — "`integrity-check` passes after 1k writes and 1k purges" —
   is what forced the change. FTS5's `integrity-check` verifies the index against the
   content table and reports an *intentionally incomplete* index as corruption; a
   pending draft must be absent from the index, so the design broke the very check the
   criterion named. A plain FTS5 table also makes `DELETE FROM memory_fts WHERE rowid = ?`
   ordinary SQL, safe when the row is absent — which external-content deletes are not.
   The price is a second copy of the text, measured above at ~8.7 KiB per 5 KB memory.
   §3.2's `tags_text` therefore exists only so the index has a column to weight, not to
   keep the column sets matching.
2. **`PRAGMA user_version`, not `meta.schema_version`.** The plan said a `meta` row.
   `user_version` lives in the database header and is written inside the same
   transaction as the migration it describes, so the version and the schema cannot
   disagree — which is the one failure a version row cannot rule out. `meta` survives
   for values with no other home.
3. **No datetime, hashing or random crate.** `time`/`chrono` are unnecessary because
   SQLite is the clock (`strftime` on write, `julianday` for the decay arithmetic), and
   one clock means one answer. `sha2` is unnecessary because `content_hash` is a lookup
   key that is always followed by an exact comparison, so a 128-bit FNV-1a is safe and
   is 20 lines. ULIDs are ~60 lines over `getrandom` rather than the `ulid` crate and its
   `rand` tree. Three dependencies not taken, each with its reason written down.
4. **Metadata defaults are not injected.** The hosted product's zod schemas fill absent
   array fields with `[]` because its TypeScript consumers rely on the shape. Nothing
   here is untyped, so the shape does not need pre-filling and the envelope stores what
   it was given.
5. **Unicode NFC normalisation is deferred.** `Café` composed and decomposed hash
   differently and are two memories. Detecting it needs `unicode-normalization` and its
   tables, and the failure is an extra row rather than a wrong answer — so it waits for
   evidence, and §10 is where the trigger is recorded.
6. **No config file yet.** §3.1 assigns `~/.kiomon/config.toml` three keys; WP1 uses
   `$KIOMON_DB` and `$KIOMON_WORKSPACE` and no file at all. A config format with one key
   in it is a parser to maintain and a second place for the answer to be wrong — and the
   first thing that genuinely needs one is the embedder, which brings a real TOML
   dependency with it in WP7 rather than a hand-rolled subset now.
7. **`init --force` was declared in WP0 and removed.** `init` is idempotent by
   construction: `open` applies migrations and ensures the default workspace, so there is
   nothing for a `--force` to force. A flag that does nothing is worse than a flag that
   is absent.
8. **`stats` moved from WP4 to WP1.** It is twelve lines of counting and it is what makes
   `init` and `doctor` say anything useful; deferring it would have meant two of WP1's
   own commands printing less than they knew.
9. **`list --recent` no longer hoists pinned rows.** "Newest first" with pinned rows on
   top is not newest first, and the hoist cost a full scan plus a sort because no index
   covers a leading `pinned` term: 41.5 ms against 14.2 ms, measured. §6.1's promise that
   "pinned memories sort first under every one of these" now applies to the relevance
   ordering only, and `docs/CONTRACT.md` says so.

### The ruler earned its place before it existed

WP3 is the bench harness, and WP1 already needed it. Two schema indexes —
`idx_mem_stats` and `idx_mem_order` — exist because a probe on a 5 000-memory corpus
measured what nothing in the design predicted:

- `stats` was rewritten from twelve `count(*)` statements into one `GROUP BY`, on the
  obvious theory that one scan beats twelve. **It got slower: 17.7 ms → 27.5 ms.** The
  twelve statements had each been answered by a *covering* index, and the `GROUP BY`
  needed `status` and `pinned`, which no index held — so it fell back to a table lookup
  per row and read every body in the corpus. Twenty-five milliseconds of that, against
  0.01 ms for a bare `count(*)`, was page reads.
- `list`'s default ordering had no matching index either, so SQLite scanned and sorted
  the corpus: **29.9 ms**. An index matching the `ORDER BY` exactly took it to **13.2 ms**.

The lesson is not "add indexes". It is that **an index is not covering unless it holds
every column the query reads**, and that the cost of getting it wrong is invisible until
the bodies are large — a corpus of short notes would have hidden both, and the plan's
own §7 budget would have been met while the design was wrong. That is the argument for
WP3 in one paragraph, and it arrived before WP3 did.

### Two things WP0 found before there was anything to measure

- **clap's usage-error exit code is 2, which the contract spends on "not found".**
  The binary catches parse failures and remaps them to 3 (`out::parse_failure`), and
  `crates/kiomon-cli/tests/cli.rs` asserts an unknown flag is 3 and not 2. This is the
  kind of collision that is invisible until an agent cannot tell a typo from a
  deleted memory, so it is worth having found it in WP0.
- **A required `cli::ArgGroup` cannot be named after an argument.** `add`'s body group
  was called `source` alongside a `--source` flag; clap's `debug_assert` caught it in a
  test. The parser's internal consistency is now a test
  (`cli::tests::the_parser_is_internally_consistent`), which is cheaper than finding
  the next one in the field.

---

### The v1.1 decision: the local engine as an SDK backend (2026-10-15)

Four SDK repositories already exist beside this one (`../../SDK/`): `kiomon-py` and
`kiomon-ts` are real, published HTTP clients; `kiomon-go` and `kiomon-rs` are empty
directories. `DEFAULT_BASE_URL` is `https://api.kiomon.com` and `baseUrl` overrides it, so
those two clients already pin the wire contract a local engine would have to speak. That
contract — transcribed from `_specs.py`, and reproduced route by route in Appendix C — is
13 endpoints, and the happy coincidence is that 6 of them are already implemented in WP1
and WP2 and the rest are scheduled in WP4, WP5 and WP5.5.

The owner's answer: **self-hosting the engine is a first-class product.** That turns "a
local brain" from a better CLI into a *backend*, and it makes the transport decision a
product decision rather than an engineering preference. §9-D8 records the shape.

What the decision does **not** change: v1 is still the CLI. WP0–WP8 still ship without a
listener, and the contract the CLI proves is still the native one (`docs/CONTRACT.md`, the
`--json` envelope, the exit codes).

What it changes, and only this:

1. **WP5.5, in v1 — `retrieveMemory`.** It is the verb both SDKs lead their READMEs with,
   and it is the one local capability with no counterpart at all: `search` returns hits,
   the packet returns `facts` / `procedure` / `episodes` with provenance, conflicts and
   warnings. It is pure composition over WP2 + WP4 + WP5 — **no model, no provider, no
   network** — so it is about a day, it is worth having on the command line by itself, and
   it is exactly the shape `/api/retrieve` must return. Building it after the socket would
   mean designing the product's flagship payload as an afterthought.
2. **WP9, v1.1 — `serve` + `mcp` as one milestone.** The old ordering put MCP first "so
   the contract can be proven first" (D7); that has happened, and proving a *second*
   contract is now the whole risk. Two transports over one verb layer cost ~15% more than
   either alone and they serve two different consumers: stdio for agent harnesses that
   spawn it per session, HTTP for applications that hold a client for the life of a
   process.
3. **The daemon trigger is met, read per command** (§10) — and the CLI's answer stays "no
   daemon", because both zero-process outcomes are covered: stdio for agents, a user-run
   `serve` for applications.

**The risk is the payload, not the socket.** A listener over a wrong payload is worse than
no listener: a client authenticates, gets a `200`, and hands the user a shape the hosted
API never returns. So WP5.5 lands first, its packet is deliberately the hosted item shape,
and WP9's acceptance test belongs to somebody else — `../../SDK/kiomon-py` and
`../../SDK/kiomon-ts`'s own suites, pointed at `127.0.0.1`.

---

## 9. Decisions

The first seven were put to the owner on 2026-10-14 and answered; **D8** was put on
2026-10-15, after WP3's numbers made the question concrete. The recommendation text is
kept verbatim below so a later reader can see what was traded away.

| # | Decision | Answer |
|---|---|---|
| D1 | Vector leg | **B** — BYO embedder (Ollama / OpenAI-compatible); FTS is the default and the fallback. Bundled ONNX stays a v1.1 opt-in cargo feature. |
| D2 | Naming | `kiomon-hq/kiomon` · binary `kiomon` · crates `kiomon-core` + `kiomon-cli` |
| D3 | Licence | **Apache-2.0** |
| D4 | URL fetch ingest | **Out of v1** (recorded in §10 for the future) |
| D5 | FTS tokenizer | `porter unicode61` |
| D6 | Workspaces | **In v1**, and `--workspace '*'` is part of the contract from WP2 |
| D7 | MCP | **v1.1** — as one transport of the WP9 milestone, not a milestone of its own (amended by D8) |
| D8 | The local engine as an SDK backend | **A** — loopback HTTP speaking the hosted `/api/*` contract, plus stdio MCP, one verb layer; `retrieveMemory` in v1 |
| D9 | The knowledge graph | **Deferred to the next sprint** (owner's call, 2026-10-17): `link`, `unlink`, `relate`, the auto-link pass and `get --relations` leave v1 |

Consequences that follow from these answers, and are now binding on the work
packages:

- **D1:** WP7 is in v1 — `Embedder` trait, `memory_embeddings`,
  `--legs auto|fts|vector`, `degraded` reporting. No ONNX, no bundled weights, no
  model download. A cargo feature `native-embed` is reserved (never enabled by
  default) so option C stays additive.
- **D4:** §10's URL-fetch row is a v1.1 candidate, not a non-goal. `add` takes
  `--body` / `--body-file` / stdin only; a `reference` memory is created by the user
  or the agent, never fetched.
- **D6:** `--workspace '*'` is honoured by `search`, `list`, and `stats` from WP2.
  It is a `WHERE` clause with no predicate, so it costs nothing — but it must be
  *tested*, because a bare `--workspace` that silently searches everything is a
  data-leak-shaped bug in a shared brain. `--workspace` absent still means the
  configured default workspace, never `*`.
- **D7:** `kiomon-core` must not assume a one-shot process. Every public function
  takes `&Database` and returns owned data, so the v1.1 MCP server holds one
  connection for the life of the session.
- **D8:** the verb layer is the product and the CLI is merely its first surface. Every
  new operation lands as a function over `&Database` returning owned data plus a
  documented JSON shape, because two more callers are coming (WP9) and they are the
  consumers that cannot be refactored quietly. Review-enforced.
- **D9:** the three graph verbs and `get --relations` stay unimplemented, and now say
  *deferred* rather than *work package 5* — a message naming a package in a plan that is
  still being executed reads as "coming in this release", and the contract's own rule is
  that a caller must be able to tell "absent" from "not implemented". v1 keeps everything
  WP1 and WP4 already shipped: the `memory_links` schema (directed, with `related_to`
  stored once), `add --ref` writing `derived_from`, edge cascades on `forget`, and a
  draft's `pending_derived_from` staging. v1 does not *write* `related_to` or
  `contradicts`/`supersedes`, read them back through a verb, or auto-link a new memory
  against its neighbours — so `retrieve` (WP5.5) reports no conflicts and no superseded
  warnings, and Appendix C's `/api/graph` answers `unsupported_locally` until WP5 lands.

### D1 — Is the vector leg in v1's default build? ⚠️ biggest fork

| Option | Cost | Consequence |
|---|---|---|
| **A. FTS-only, no vector code at all** | 0 | Fastest to ship, smallest binary. "Find by meaning" — the platform's headline capability — is absent locally. |
| **B. BYO embedder (Ollama / OpenAI-compatible HTTP), FTS is the default and the fallback** | ~1 day, no model weights, no new heavy deps | Hybrid works today for anyone with Ollama running (`nomic-embed-text`); the platform's own CHANGELOG already treats the self-hosted nomic pair as first-class. Requires a config block and a `reindex --embed`. **Recommended.** |
| **C. Bundled local model (`fastembed` / ONNX, bge-small-384)** | +100–300 MB, +ONNX runtime, +build complexity, first-run download | Truly zero-config semantic search, offline, no daemon to install. Heaviest binary, most likely to break on a new platform. |

**Recommendation: B now, C as an opt-in cargo feature in v1.1.** The `Embedder`
trait and `memory_embeddings` table make C an additive change; choosing A would
require re-plumbing the write path later.

**DECIDED: B.**

### D2 — Repo name, binary name, and crate names
Recommendation: repo `kiomon-hq/kiomon`, binary `kiomon`, crates `kiomon-core` +
`kiomon-cli`, published crate `kiomon-core`. The binary must be `kiomon` — it is what
every doc, script, and `AGENTS.md` block will say. **Confirm the GitHub repo does not
collide with something already reserved, and that `kiomon` is free on crates.io.**

**DECIDED: as recommended.**

### D3 — Licence
No `LICENSE` exists in the org, in `platform/`, or in the `.github` repo. Every
`platform/` source file carries `Copyright (c) 2026 Kiomon. All rights reserved.` —
that header must **not** be copied into the OSS repo.
Recommendation: **Apache-2.0** (patent grant, matches the OSS models already
evaluated in `TODO.md` §11), with the hosted product remaining closed. Alternatives:
MIT (simpler, no patent grant), or AGPL-3.0 (protects the hosted business but
disqualifies some corporate users). **Owner decision.**

**DECIDED: Apache-2.0.** Note that `kiomon-hq/.github/CONTRIBUTING.md` says
contributions are licensed under *MIT* — that org-level default is now wrong for
this repository, which carries its own `CONTRIBUTING.md`. **The org default should
be corrected.**

### D4 — Is `reference` ingest (URL fetch) in v1?
Recommendation: **local files + stdin + text in v1; URL fetch in v1.1.** Fetching
implies `reqwest`+TLS (binary size), an HTML extractor, robots/rate limits, and a
"why is this body garbage" support surface. `add --body-file` covers the real local
workflow. Confirm the target user (a coding agent's own learnings vs. a
bookmark-manager replacement) — it changes this answer.

**DECIDED: no URL fetch in v1** — local files, directories, and stdin only. It stays
on the §10 revisit list, which is where the target-user question gets answered with
evidence rather than now.

### D5 — FTS tokenizer: `porter unicode61` vs `trigram`
`porter unicode61` matches the platform and gives clean prefix matching + stemming.
`trigram` gives substring and CJK matching but is ~3× larger and much slower on
large bodies. Recommendation: **`porter unicode61`** (parity, speed), with the
substring gap accepted and noted. (The platform's tokenizer omits
`remove_diacritics`; we keep the default, which is a small fidelity improvement.)

**DECIDED: `porter unicode61`.**

### D6 — Workspaces in v1?
Recommendation: **yes** (they are one table plus a `WHERE` clause, and they are the
one structural concept that is painful to retrofit). Confirm we never need
cross-workspace search in v1 — if yes, `--workspace '*'` must be designed now, not
bolted on.

**DECIDED: workspaces in v1, and `--workspace '*'` designed in from WP2.** The
absence of `--workspace` means the configured default; `'*'` is an explicit opt-in to
search everything, so an agent can never widen its own scope by forgetting a flag.

### D7 — Does v1 ship `kiomon mcp`?
The user's brief says CLI, so the plan puts MCP in v1.1. But it is the difference
between "an agent can use this via shell" and "an agent can use this natively".
Recommendation: **hold the line at CLI for v1** (it is what proves the contract),
**and schedule MCP as the first v1.1 item** — it is a thin adapter over
`kiomon-core` because §4.4's row shape is already the MCP handler's.

**DECIDED: CLI in v1, `kiomon mcp` first in v1.1.** *(Amended 2026-10-15 by D8: MCP
is the stdio transport of the WP9 milestone, and it ships alongside `serve`.)*

### D8 — Does the local engine speak the hosted API?

Asked after WP3, once the owner's requirement was explicit: *users run the engine on their
own machine and connect to it with the SDKs*. Four shapes were on the table.

| Shape | Cost | Verdict |
|---|---|---|
| **A. Loopback HTTP answering the hosted `/api/*` contract, plus stdio MCP, over one verb layer** | ~2½ days, ~2 crates, +~250 KiB | **Chosen.** The Python and TypeScript SDKs work with **no release of their own** — `new Kiomon({ baseUrl: "http://127.0.0.1:8765", apiKey: "local" })` is already supported, because the client checks that a credential is *present*, never what it *is*. Their own test suites then become this milestone's conformance suite, written by someone who did not write the server. |
| B. Local HTTP with our own JSON contract | same, plus a fourth SDK to keep in step | Rejected: it makes compatibility a permanent obligation in four languages, when the four clients already exist. |
| C. MCP only, as originally planned | ~1 day | Rejected as the *only* surface: agent harnesses get memory, applications do not, and the SDK program stays pointed at the cloud. Kept as the second transport — it is the one with no port and no lifecycle. |
| D. No server; the SDKs shell out to `kiomon --json` | ~0 | Rejected: a process per operation, no batching, no long-lived session, and a subprocess parser in four languages instead of one listener in Rust. |

Binding consequences:

- **Loopback only, and authenticated.** `127.0.0.1`, never `0.0.0.0`; no TLS; a random
  token written to the data directory and required on every request; `Origin` and `Host`
  checked — a browser tab can reach `localhost`, so a local brain with no CSRF story is a
  local brain any web page can read.
- **No daemon lifecycle.** `serve` is started by the user, or by a script with `--port 0`,
  and never auto-spawned by an SDK: an auto-spawned server is a stale binary after every
  upgrade and a zombie per crashed client, and §10 already refused that once. stdio MCP is
  the no-daemon path, and it is what an agent harness should use.
- **No tokio, no axum.** Loopback HTTP/1.1 over `std::net` and threads; `serde_json` is
  already in the tree. §7 holds 27 crates and 2.8 MiB of slack and an async runtime spends
  both on a socket that serves one local client.
- **`kiomon-core` *is* the Rust SDK.** `../../SDK/kiomon-rs` stays empty on purpose — a Rust
  user with the engine on the same machine calls `kiomon-core` in-process rather than
  paying for a socket to their own SQLite file. `../../SDK/kiomon-go` is the HTTP client it
  has to be.
- **What cannot be answered locally is never faked.** `POST /api/memories/{id}/revert` and
  `GET /api/workspaces/{id}/briefings/latest` return a typed `unsupported_locally` error
  naming the reason, never an empty `200`. An SDK that reports "no briefings" because the
  feature does not exist is an SDK lying to its user, and that is the failure mode
  self-hosting cannot afford.
- **The native contract stays native.** `/api/*` is a translation layer with its own tests;
  the CLI's envelope and exit codes do not move to accommodate it. One contract per
  surface, one owner each.
- **`--db` remains the only scope.** No users, no accounts, no tenancy: `serve` is
  single-brain by construction, and `GET /api/workspaces` lists that brain's workspaces.
  The engine has nothing to check an API key against, and inventing an account model
  locally would be the first piece of tenancy §9-D6 refused from the start.

**DECIDED: A. `retrieveMemory` (WP5.5) is in v1; v1.1 is `serve` + `mcp` in one milestone
over one verb layer; the acceptance criterion is the two published SDKs' test suites
against `127.0.0.1`.**

**DECIDED: the graph is not v1.** Put to the owner on 2026-10-17, after WP4 landed and the
remaining scope could be read as a list rather than a plan: the graph is the next sprint's
first work package, and v1 ships the packet, portability, the vector leg and packaging
without it. The product cost is bounded and known — `retrieve` is honest about an empty
`conflicts` because nothing can create a contradiction yet — and the alternative cost is
paid per day by every user who cannot install the thing.

---

## 10. Non-goals, and the triggers that would change them

Each of these is deliberately unbuilt. The trigger is the evidence that would make
it the right next thing — not a schedule.

| Deferred | Revisit when |
|---|---|
| Chunk-level retrieval | A single memory's body exceeds ~50 KB and search snippets stop being useful, or the corpus passes ~250k rows. |
| Contradiction / quarantine | `claims` are populated by real agents and users report conflicting facts. (Needs claim normalisation — the platform's own `JACCARD_CONFLICT_THRESHOLD = 0.4` is a heuristic we should not port blindly.) |
| Server / `serve` | **Trigger met 2026-10-15 — and it was the wrong shape.** Measured per command (§7): `open` is 2.0 ms of a 26.8 ms `search` (7.5%, *under* the 30% as originally worded, because search's own work dominates) but ~**80% of an `add`** and most of a `get`; and an application holding an SDK client also deletes the 8.9 ms process-spawn floor entirely. The old trigger asked the question of the CLI, where one process per command is right. The answer arrives from the other direction: for a long-lived client, one process per operation is the cost. Scheduled as WP9 (v1.1), with the lifecycle objection answered rather than ignored — §9-D8. |
| Consolidation (`consolidate-memories`) | Duplicate/near-duplicate rate in `list` is visibly annoying. |
| `reflect_session`, briefings, RAG | Never locally, unless the user opts into a provider — this is where the local product would stop being fast and start being a cloud client. |
| Connectors | Never. `import` + a script is the honest local answer. |
| Native embeddings (option C) | D1 answered C, or enough users lack Ollama that setup friction is the top complaint. |
| Multi-user / sharing | Never. |
| Sync to the hosted platform | Only if the hosted product wants a local-capture mode. That is a platform plan, not this one. |

---

## 11. Risks

1. **The "minimal" scope is still three weeks.** WP4 (lifecycle) and WP5 (graph) are
   the two that could double. Mitigation: WP0–WP3 ship a genuinely useful product
   (`add`/`search`/`get`/`list`/`stats`, fast). Lifecycle and graph are additive and
   can be cut from v1 without breaking the contract — **decide at WP3's exit whether
   v1 is WP0–WP3+WP6 (a fast local search tool) or WP0–WP8 (a local brain).** **Resolved
   2026-10-17: the cut was the graph (D9) and not the lifecycle**, so v1 is
   WP0–WP4 + WP5.5 + WP6–WP8: a local brain that cannot draw its own graph yet.
2. **The FTS-only default undersells the product.** A user comparing `kiomon search`
   to the hosted hybrid search will notice the difference. Mitigation: D1-B so
   hybrid is one config block away, and honest README wording — `PRODUCT.md`'s
   principle 5 applies to this repo too.
3. **Schema rework after v1.** SQLite migrations are forward-only and users will have
   data. Mitigation: `content_hash`, `memory_embeddings`, and `meta` exist from
   `0001` specifically so the v1.1 features (dedupe tuning, embeddings, decay
   bookkeeping) do not need table rewrites.
4. **`--json` becomes a de-facto frozen API before it is good.** Mitigation:
   `CONTRACT.md` is versioned from WP2 and `meta.contract_version` is in every JSON
   response.
5. **Windows.** `mmap_size`, path resolution, and the `isatty` check all differ.
   Mitigation: Windows is in the CI matrix from WP0, not added at WP8.
6. **The hosted contract drifts away from the local one.** Two implementations of one
   wire format, one of them in another repository, is how an SDK starts lying to its
   users. Mitigation: Appendix C is the route-by-route checklist, every route is asserted
   either to answer or to refuse explicitly, and the SDKs' own suites run against
   `127.0.0.1` in CI (§9-D8) — the conformance test is theirs, not ours.

---

## Appendix A — What is ported from the platform, and where

| Platform source | Local counterpart | Fidelity |
|---|---|---|
| `worker/src/lib/rrf.ts` | `kiomon-core/src/fuse.rs` | Verbatim formula, `k = 60` |
| `worker/src/lib/search.ts` `sanitizeFtsQuery` | `search.rs` ladder | Improved (quoting, not regex-stripping) |
| `worker/src/lib/memory/schemas.ts` | `model.rs` metadata validation | Same envelopes, same fail-open rule |
| `worker/src/lib/write-gate.ts` | `gate.rs` | Same `0.75`, same authority rule; `DISCARD`/`MERGE` reduced to dedupe |
| `worker/src/jobs/decay-memories.ts` | `decay.rs` | Verbatim constants and formula |
| `worker/src/lib/memory/lifecycle.ts` `promote()` | `review approve` | Same rule: index at activation, never before |
| `worker/migrations/0042_memory_kinds.sql` | `0001_init.sql` | Same kinds, same envelope columns, same relation names |
| `mcp/src/handlers/retrieveMemory.ts`, `searchMemory.ts`, `getMemory.ts`, `draftMemory.ts`, `recordOutcome.ts` | `CONTRACT.md` + CLI verbs | Same vocabulary, same framing, same output shape |
| `../../SDK/kiomon-{py,ts}` wire specs (`_specs.py`, `sdk/dist/client.js`) | `serve` (WP9, Appendix C) | Same paths, methods and bodies; the SDKs point at `127.0.0.1` with no release of their own |
| `agent/src/` (Rust, Rig) | — | Not ported: that is a model-driven turn loop, not memory |
| `migrations/0001_init.sql` FTS setup | `0001_init.sql` | **Bug fixed**: `content_rowid` points at an INTEGER rowid |

## Appendix B — Repository hygiene

- `README.md` — the thesis, the three measured numbers, a 60-second quickstart, and
  an explicit "what this is not" section pointing at the hosted product.
- `LICENSE` (D3), `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md`,
  `SUPPORT.md` — imported from `kiomon-hq/.github` where they already exist; the
  license and the code-of-conduct contact must be checked before publish.
- No `Copyright (c) 2026 Kiomon. All rights reserved.` headers. OSS files carry
  SPDX identifiers (`// SPDX-License-Identifier: Apache-2.0`) instead.
- `deny.toml` + `cargo-deny` in CI: a dependency is a promise, and a local binary
  with 300 transitive crates is not "minimal". Target: **< 60 direct+transitive
  crates in the default build**.

## Appendix C — The routes `serve` must answer (WP9)

Transcribed from `../../SDK/kiomon-py/src/kiomon/_specs.py` and `_client.py`, which is where
the wire contract actually lives: those two SDKs already agree with each other, and a third
opinion invented here would be a fourth implementation of a format nobody owns.

| Method + path | SDK verb | Local |
|---|---|---|
| `GET /api/search?q&limit&workspace_id&kind` | `searchMemory` | `search` (WP2) |
| `POST /api/retrieve` | `retrieveMemory` | `retrieve` (WP5.5) |
| `GET /api/documents/{id}` | `getMemory` | `get` (WP1) |
| `POST /api/memories/batch-draft` | `draftMemory` | `add`, batched (`insert_many`, WP3) |
| `POST /api/memories/record-outcome` | `recordOutcome` | `outcome` (WP4) |
| `GET /api/graph?id&relation&depth` | `exploreMemoryGraph` | ❌ `unsupported_locally` — deferred with WP5 (§9-D9), and never an empty `200` |
| `PATCH /api/documents/{id}` `{"pinned": 0\|1}` | `manageMemory` pin/unpin | `pin`/`unpin` (WP1) |
| `POST /api/documents/{id}/rate` `{"rating": n}` | `manageMemory` rate | `rate` (WP4) |
| `POST /api/documents/{id}/{action}` | `manageMemory` archive/restore/forget/approve/reject | WP4 + WP5 verbs |
| `GET /api/workspaces` | `listWorkspaces` | the `workspaces` table (WP1) |
| `GET /api/workspaces/{id}/context?topic` | `getWorkspaceContext` | partial — pinned + recent + kinds, composable from WP1/WP2 |
| `POST /api/memories/{id}/revert` | `manageMemory` revert | ❌ `unsupported_locally` — this engine keeps no revision history |
| `GET /api/workspaces/{id}/briefings/latest` | `getLatestBriefing` | ❌ `unsupported_locally` — briefings are a hosted composition, and §10 says never locally |

Auth: `X-API-Key` or `Authorization: Bearer`. The client refuses to construct without one,
so `serve` accepts any non-empty key on a loopback bind and separately requires its own
token file; there is nothing local to validate a key against, and inventing an account
model to have something to check would be the tenancy §9-D6 refused from the start.
