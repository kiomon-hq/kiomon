-- 0004_link_supersedes.sql — the relation vocabulary grows by one.
--
-- Forward-only, like 0001–0003. `PRAGMA user_version` moves to 4 in the same transaction
-- as the statements below.
--
-- `0001`'s `memory_links` has a CHECK on `relation` listing the seven names the plan's
-- schema table carried. The packet (WP5.5), ported from the hosted product, reads
-- `relation IN ('supersedes', 'contradicts')` — and `supersedes` was not in the list, so
-- nothing this build could write would ever produce the packet's supersession warning. The
-- hosted product writes the edge from its contradiction resolver; locally, WP5's `link` is
-- the writer, and this migration is what makes it able to.
--
-- SQLite cannot alter a CHECK constraint, so the table is rebuilt the way the SQLite
-- documentation's "making other kinds of table schema changes" section describes: create
-- the new shape beside the old, copy every row, drop the old, rename, and recreate the
-- indexes. Nothing references `memory_links` by foreign key, and the rows copied keep ids
-- that exist in `memories`, so this is safe with `foreign_keys = ON` — which the migration
-- runs under and which cannot be turned off inside a transaction anyway.
--
-- The three columns `0001` did not have and `0003` added are carried through: `valid_to`.
-- (The `UNIQUE` and both indexes are part of the new table, so an edge written before this
-- migration is neither duplicated nor lost.)
--
-- A note on `UNIQUE (source_id, target_id, relation)`: the hosted table is
-- `UNIQUE (source_id, target_id)`, which cannot hold a `contradicts` edge and a
-- `related_to` edge for the same pair. The local schema keeps the relation in the key,
-- because the packet reads one of those relations and the graph reads the other, and a
-- conflict is a different statement from an association.
CREATE TABLE memory_links_new (
    id         INTEGER PRIMARY KEY,
    source_id  TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    target_id  TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    relation   TEXT NOT NULL DEFAULT 'related_to'
               CHECK (relation IN ('derived_from', 'supports', 'contradicts', 'supersedes',
                                   'related_to', 'precedes', 'follows', 'part_of')),
    directed   INTEGER NOT NULL DEFAULT 0 CHECK (directed IN (0, 1)),
    score      REAL,
    origin     TEXT NOT NULL DEFAULT 'manual'
               CHECK (origin IN ('manual', 'auto', 'agent')),
    metadata   TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL,
    valid_to   TEXT,
    UNIQUE(source_id, target_id, relation)
);

INSERT INTO memory_links_new
    (id, source_id, target_id, relation, directed, score, origin, metadata, created_at, valid_to)
SELECT id, source_id, target_id, relation, directed, score, origin, metadata, created_at, valid_to
  FROM memory_links;

DROP TABLE memory_links;
ALTER TABLE memory_links_new RENAME TO memory_links;

CREATE INDEX idx_link_source ON memory_links(source_id);
CREATE INDEX idx_link_target ON memory_links(target_id);
