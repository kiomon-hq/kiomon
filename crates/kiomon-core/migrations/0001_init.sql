-- 0001_init.sql — the schema of a local Kiomon brain.
--
-- Forward-only. Once this file has shipped it is never edited; a change is a new
-- numbered file. The applier records how far it got in `PRAGMA user_version`, which
-- is transactional (unlike a row in `meta`), so the schema version and the schema
-- can never disagree.

-- ---------------------------------------------------------------------------
-- Workspaces. Isolation is a `WHERE` clause, and `review_mode` is why this is a
-- table rather than a free-text column on `memories`.
-- ---------------------------------------------------------------------------
CREATE TABLE workspaces (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    review_mode TEXT NOT NULL DEFAULT 'autonomous'
                CHECK (review_mode IN ('autonomous', 'review')),
    created_at  TEXT NOT NULL
);

-- ---------------------------------------------------------------------------
-- Memories.
--
-- `mem_id` is an INTEGER PRIMARY KEY because it is the rowid, and every index and
-- every join in the read path prefers an integer over a 26-character string. `id`
-- is the agent-facing handle, `k_<ulid>`, time-prefixed to the millisecond: for rows
-- more than a millisecond apart `ORDER BY id` is `ORDER BY created_at`, and inside one
-- millisecond the order is the random half — stable, not chronological. Anything that
-- means "the row that was here first" orders by `mem_id`.
-- ---------------------------------------------------------------------------
CREATE TABLE memories (
    mem_id            INTEGER PRIMARY KEY,
    id                TEXT NOT NULL UNIQUE,
    workspace_id      TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,

    kind              TEXT NOT NULL DEFAULT 'reference'
                      CHECK (kind IN ('reference', 'semantic', 'procedural', 'episodic')),
    title             TEXT NOT NULL,
    excerpt           TEXT NOT NULL DEFAULT '',
    body              TEXT NOT NULL DEFAULT '',

    -- JSON array, canonical. `tags_text` is the same tags space-joined, written by the
    -- same function in the same statement, and exists only so the full-text index has
    -- a column to weight: JSON punctuation tokenizes as noise. Tag *filtering* reads
    -- the JSON (via `json_each`), never `tags_text`, because a space-joined string
    -- cannot be matched exactly at its boundaries.
    tags              TEXT NOT NULL DEFAULT '[]',
    tags_text         TEXT NOT NULL DEFAULT '',

    -- Per-kind envelope, JSON. Validated in Rust; see `metadata.rs`.
    memory_metadata   TEXT NOT NULL DEFAULT '{}',

    source            TEXT NOT NULL DEFAULT 'user_saved',
    source_url        TEXT,

    -- The envelope. `confidence` is the writer's claim (nullable: an imported file
    -- makes no claim). `importance` is the user's, and it sets the ceiling that decay
    -- falls away from. `strength` is decay's own output and the only one that moves
    -- on its own.
    confidence        REAL CHECK (confidence IS NULL OR (confidence >= 0.0 AND confidence <= 1.0)),
    importance        REAL NOT NULL DEFAULT 0.5
                      CHECK (importance >= 0.0 AND importance <= 1.0),
    strength          REAL NOT NULL DEFAULT 1.0 CHECK (strength >= 0.0),
    corroboration_count INTEGER NOT NULL DEFAULT 0,
    retrieval_count   INTEGER NOT NULL DEFAULT 0,
    last_retrieved_at TEXT,
    feedback_score    REAL NOT NULL DEFAULT 0,

    pinned            INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
    status            TEXT NOT NULL DEFAULT 'active'
                      CHECK (status IN ('active', 'pending_approval', 'archived')),
    expires_at        TEXT,

    -- A 128-bit FNV-1a over `kind|norm(title)|norm(body)`. It is a lookup key, not a
    -- decision — `find_duplicate` compares the normalised text again after matching on
    -- it, so a collision costs a missed dedupe, never a lost memory. That is what
    -- makes a non-cryptographic hash the right choice here.
    content_hash      TEXT,

    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL
);

-- The library list: newest-or-strongest active rows in one workspace. The trailing
-- `id DESC` is not decoration: `created_at` has second precision, so ties are the norm
-- rather than the exception, and without the tiebreak in the index SQLite has to sort
-- the whole result set to produce a stable order.
CREATE INDEX idx_mem_ws_status_created ON memories(workspace_id, status, created_at DESC, id DESC);
CREATE INDEX idx_mem_ws_kind_status    ON memories(workspace_id, kind, status);
-- The duplicate lookup. Not UNIQUE: `add --force` is allowed to store a genuine
-- repeat, and SQLite serialises writers, so a read-then-insert in one transaction
-- cannot race with itself.
CREATE INDEX idx_mem_dedupe            ON memories(workspace_id, content_hash)
                                       WHERE content_hash IS NOT NULL;
-- Decay scans active rows by strength.
CREATE INDEX idx_mem_strength          ON memories(strength) WHERE status = 'active';

-- ---------------------------------------------------------------------------
-- Two indexes that exist for measured reasons, not for symmetry. Both were added
-- after a probe on a 5 000-memory corpus with 5 KB bodies (42 MiB), because in both
-- cases the obvious query was several times slower than it looked:
--
--   * `idx_mem_stats` covers `stats`, whose `GROUP BY kind, status, pinned` was
--     planned as an index search on `workspace_id` followed by a *table* lookup for
--     the two columns no index held. That read every body in the corpus: 25 ms, against
--     0.01 ms for a bare `count(*)`. With this index the whole aggregate is answered
--     from the index and the bodies are never touched.
--   * `idx_mem_order` matches `list`'s default `ORDER BY pinned DESC, strength DESC,
--     id DESC` exactly, so SQLite can walk it and fetch only the rows it will return
--     instead of scanning and sorting the corpus. Measured 29.9 ms -> 13.2 ms.
-- The lesson is in `plans/001-local-v1-core.md`: an index is not "covered" unless it
-- holds every column the query reads, and the cost of getting that wrong is invisible
-- until the bodies are large.
CREATE INDEX idx_mem_stats ON memories(workspace_id, kind, status, pinned);
CREATE INDEX idx_mem_order ON memories(workspace_id, status, pinned DESC, strength DESC, id DESC);

-- ---------------------------------------------------------------------------
-- Full-text index.
--
-- Deliberately NOT an external-content (`content='memories'`) table, which the
-- hosted schema uses. Two reasons, both learned from this repository's own plan:
--
--   1. A pending draft must not be retrievable, so the index has to hold a *subset*
--      of `memories`. FTS5's `integrity-check` verifies the index against the content
--      table and reports an intentionally incomplete index as corruption — so with
--      external content, the check that would catch a broken trigger is the check the
--      design breaks.
--   2. `DELETE FROM memory_fts WHERE rowid = ?` is ordinary SQL that is safe when the
--      row is absent. External-content deletes need the exact old column values and
--      corrupt the index when the row was never there, which is precisely the case a
--      status transition produces.
--
-- The cost is that the text is stored twice. It is paid knowingly: the alternative is
-- a class of silent index corruption in the one part of the system whose whole job is
-- to be trustworthy.
-- ---------------------------------------------------------------------------
CREATE VIRTUAL TABLE memory_fts USING fts5(
    title,
    excerpt,
    body,
    tags_text,
    tokenize = 'porter unicode61'
);

-- Index a memory exactly when it is retrievable, and never otherwise. `WHEN
-- new.status = 'active'` is the review inbox's boundary expressed in the schema: a
-- `pending_approval` row is inserted without an index row, so no read path can reach
-- it. A review inbox that leaks into retrieval is worse than no review inbox.
CREATE TRIGGER mem_ai AFTER INSERT ON memories
WHEN new.status = 'active'
BEGIN
    INSERT INTO memory_fts(rowid, title, excerpt, body, tags_text)
    VALUES (new.mem_id, new.title, new.excerpt, new.body, new.tags_text);
END;

-- A delete is unconditional: the row may or may not be indexed, and deleting a rowid
-- that is not there is a no-op rather than a corruption.
CREATE TRIGGER mem_ad AFTER DELETE ON memories
BEGIN
    DELETE FROM memory_fts WHERE rowid = old.mem_id;
END;

-- `UPDATE OF` is what keeps the fast path fast: this trigger does not fire for a
-- counter bump (`retrieval_count`, `strength`, `last_retrieved_at`), only when a
-- column retrieval can see is in the statement's SET clause. Adding a column here
-- without adding it to the searcher's UPDATE is how a memory becomes unfindable after
-- an edit, so the list is the contract between `store::update` and the index.
CREATE TRIGGER mem_au AFTER UPDATE OF title, excerpt, body, tags_text, status ON memories
BEGIN
    DELETE FROM memory_fts WHERE rowid = old.mem_id;
    INSERT INTO memory_fts(rowid, title, excerpt, body, tags_text)
    SELECT new.mem_id, new.title, new.excerpt, new.body, new.tags_text
    WHERE new.status = 'active';
END;

-- ---------------------------------------------------------------------------
-- The graph. Typed, and `origin` so a future re-link can delete exactly the
-- machine-made edges and never touch a human's.
-- ---------------------------------------------------------------------------
CREATE TABLE memory_links (
    id         INTEGER PRIMARY KEY,
    source_id  TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    target_id  TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    relation   TEXT NOT NULL DEFAULT 'related_to'
               CHECK (relation IN ('derived_from', 'supports', 'contradicts',
                                   'related_to', 'precedes', 'follows', 'part_of')),
    directed   INTEGER NOT NULL DEFAULT 0 CHECK (directed IN (0, 1)),
    score      REAL,
    origin     TEXT NOT NULL DEFAULT 'manual'
               CHECK (origin IN ('manual', 'auto', 'agent')),
    metadata   TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL,
    UNIQUE(source_id, target_id, relation)
);
CREATE INDEX idx_link_source ON memory_links(source_id);
CREATE INDEX idx_link_target ON memory_links(target_id);

-- ---------------------------------------------------------------------------
-- The optional vector leg (WP7). A BLOB of little-endian f32, L2-normalised on
-- write so the read path is a dot product. Null until an embedder is configured.
-- ---------------------------------------------------------------------------
CREATE TABLE memory_embeddings (
    mem_id     INTEGER NOT NULL REFERENCES memories(mem_id) ON DELETE CASCADE,
    model      TEXT NOT NULL,
    dim        INTEGER NOT NULL,
    vec        BLOB NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (mem_id, model)
) WITHOUT ROWID;

-- ---------------------------------------------------------------------------
-- Credit attribution (WP4). Recorded before its effect is applied, so a strength
-- that moved can always be explained.
-- ---------------------------------------------------------------------------
CREATE TABLE memory_outcomes (
    id         INTEGER PRIMARY KEY,
    memory_id  TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    outcome    TEXT NOT NULL CHECK (outcome IN ('success', 'failure')),
    assessment TEXT NOT NULL
               CHECK (assessment IN ('helpful', 'unused', 'misleading', 'outdated')),
    created_at TEXT NOT NULL
);
CREATE INDEX idx_outcome_memory ON memory_outcomes(memory_id);

-- ---------------------------------------------------------------------------
-- Bookkeeping with no other home. The schema version is NOT here — it is
-- `PRAGMA user_version`, because that is written in the same transaction as the
-- migration and a row is not.
-- ---------------------------------------------------------------------------
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
