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
| **MCP server** | v1.1. `kiomon mcp` (stdio) is the natural agent surface and reuses `kiomon-core` verbatim. v1 is CLI so the contract can be proven and stabilized first. |
| **Daemon / HTTP server** | v1.2. Design `kiomon-core` to be daemon-ready; do not build it. |
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
├── benches/                   # criterion: search/add/get at 1k/10k/100k
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
then `kiomon mcp` (v1.1) and `kiomon serve` (v1.2) would either duplicate the
library or force a split under load. `kiomon-core` has no `clap`, no `print!`, and
no `process::exit` — that rule is what makes those later binaries one `main.rs`
each.

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

### 3.2 Schema (`migrations/0001_init.sql`)

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

| Attempt | Query | When |
|---|---|---|
| 1 | `"phrase"` (quoted literal) | always tried first |
| 2 | `"t1"* AND "t2"* AND …` | all terms, prefix-matched |
| 3 | `"t1"* OR "t2"* OR …` | if attempt 2 is empty |

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
`last_retrieved_at = now`, batched in the same transaction as the read's follow-up
(lazily, only above `--record` / default on for `search`, off for `list`). This is
what feeds the decay stretch factor, so it is not telemetry — it is the mechanism.

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

`rate <id> +1|-1` — immediate, human:

| | `strength` | `feedback_score` |
|---|---|---|
| `+1` | `min(strength * 1.15, 1.0)` | `+1` |
| `-1` | `strength * 0.75` | `-1` |

`outcome <success|failure> --item <id>:<assessment>` — the agent-facing credit
attribution, ported from `record_outcome`:

| assessment | effect |
|---|---|
| `helpful` | `strength *= 1.15` (cap 1.0), `feedback_score += 1`, `corroboration_count += 1` |
| `unused` | nothing (omitted memories stay untouched — the tool's own contract) |
| `misleading` | `strength *= 0.5`, `feedback_score -= 1` |
| `outdated` | `strength *= 0.5`, `expires_at = now`, `feedback_score -= 1` |

Every outcome is recorded in `memory_outcomes` before its effect is applied, so
credit is auditable and reversible. All multipliers are named constants in
`lifecycle.rs` and documented in `docs/DESIGN.md` — the platform's threshold
saga (`AUTOPROMOTE_THRESHOLD`, `JACCARD_CONFLICT_THRESHOLD`) is a standing lesson
that an unnamed magic number becomes a permanent argument.

### 5.4 Decay

`kiomon decay` — a command, not a daemon. Runs on demand, and opportunistically
(auto, ≤ once per 24 h, at most one page) on `add`/`search` so a brain that is
never explicitly maintained still ages correctly. `--dry-run` prints the table.

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
               [--snippet-len 12] [--json]
  kiomon get <id> [--relations] [--json]
  kiomon list [--kind] [--status] [--tag] [--workspace] [--limit] [--json]

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

`crates/kiomon-core/benches/` (criterion) + `kiomon bench` (end-to-end, wall clock)
measure, at 1k / 10k / 100k generated memories, with a realistic body-size mix
(60% short ≤ 500 B, 30% medium ≤ 8 KB, 10% long ≤ 100 KB):

- `open` — process + `Database::open` + PRAGMAs (this is pure overhead and it is
  the number that decides whether a daemon is needed)
- `add` — cold process → exit
- `search` — cold process → exit, per ladder attempt, per leg
- `get`, `list`, `relate`
- `decay --full` over the corpus
- RSS high-water and binary size

Gates in CI (absolute, because shared runners are noisy — a ratio gate is a
flaky-test generator):

| Metric @ 10k memories | Gate |
|---|---|
| `open` | ≤ 5 ms |
| `add` p95 | ≤ 40 ms |
| `search` p95 (fts) | ≤ 60 ms |
| `search` p95 (hybrid, local embedder) | ≤ 250 ms |

CI additionally publishes the bench output as a job artifact so a regression is
visible in the diff even when it is under the gate.

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

### WP4 — Lifecycle: gate, review, feedback, decay (2 days)
`gate.rs` (authority, 0.75, review mode, dedupe), `review approve|reject` with
index-on-approve, `pin/archive/restore/rate`, `outcome`, `decay.rs`.
**Accept:** a `pending_approval` row is invisible to `search` (asserted in the
same test that asserts it becomes visible after `approve`); the decay formula is
asserted against hand-computed values at age = 0, 1, and 2 half-lives for all
three decaying kinds; `pinned` and `reference` rows never decay; `--dry-run`
mutates nothing; a `--workspace` with `review_mode='review'` sends a
`confidence=1.0` write to the inbox.

### WP5 — The graph: links, auto-link, relate (2 days)
`links.rs`, `link`/`unlink`/`relate`, the synchronous auto-link pass on activate,
`origin` tagging, `derived_from` from `source_refs`.
**Accept:** auto-link writes ≤5 edges and never a self-edge; `derived_from` edges
are directed and `related_to` are not; `relate` returns 1-hop neighbours with
relation + title + excerpt; purge cascades.

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

**v1 = WP0–WP8 complete. Estimated 2–3 focused weeks.** WP3 precedes WP4 by
design: the ruler has to exist before there is anything worth measuring.

### Progress

| WP | Status | Evidence |
|---|---|---|
| **WP0** | ✅ **DONE** 2026-10-14 | Skeleton, CI, licence, hygiene, and the full CLI surface. `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` are all green (34 tests). `cargo build --release` produces a **777 KiB** binary against the 6 MB budget, from **22** transitive crates against the 60 crate budget — both without `rusqlite`, which WP1 adds. |
| WP1 | ⬜ TODO | |
| WP2 | ⬜ TODO | |
| WP3 | ⬜ TODO | |
| WP4–WP8 | ⬜ TODO | |

WP0's acceptance criteria were met, and two decisions were made while meeting them
that the plan did not anticipate:

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

## 9. Decisions

All seven were put to the owner on 2026-10-14 and answered. The recommendation text
is kept verbatim below so a later reader can see what was traded away.

| # | Decision | Answer |
|---|---|---|
| D1 | Vector leg | **B** — BYO embedder (Ollama / OpenAI-compatible); FTS is the default and the fallback. Bundled ONNX stays a v1.1 opt-in cargo feature. |
| D2 | Naming | `kiomon-hq/kiomon` · binary `kiomon` · crates `kiomon-core` + `kiomon-cli` |
| D3 | Licence | **Apache-2.0** |
| D4 | URL fetch ingest | **Out of v1** (recorded in §10 for the future) |
| D5 | FTS tokenizer | `porter unicode61` |
| D6 | Workspaces | **In v1**, and `--workspace '*'` is part of the contract from WP2 |
| D7 | MCP | **v1.1**, first item |

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

**DECIDED: CLI in v1, `kiomon mcp` first in v1.1.**

---

## 10. Non-goals, and the triggers that would change them

Each of these is deliberately unbuilt. The trigger is the evidence that would make
it the right next thing — not a schedule.

| Deferred | Revisit when |
|---|---|
| Chunk-level retrieval | A single memory's body exceeds ~50 KB and search snippets stop being useful, or the corpus passes ~250k rows. |
| Contradiction / quarantine | `claims` are populated by real agents and users report conflicting facts. (Needs claim normalisation — the platform's own `JACCARD_CONFLICT_THRESHOLD = 0.4` is a heuristic we should not port blindly.) |
| Daemon / `serve` (unix socket + JSON-RPC) | Measured `open` overhead exceeds ~30% of `search` wall time. Not before — a daemon is a lifecycle problem (stale version after upgrade) that a local tool should not buy early. |
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
   v1 is WP0–WP3+WP6 (a fast local search tool) or WP0–WP8 (a local brain).**
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
