// SPDX-License-Identifier: Apache-2.0

//! Reading and writing memories.
//!
//! Every method here is one short transaction on the caller's connection. There is no
//! connection pool and no statement cache of our own: a CLI invocation does one thing,
//! and a second connection would cost more than the work.
//!
//! # How user input reaches SQL
//!
//! Two rules, and they are worth stating because the code applies them by hand:
//!
//! - **A value a user typed is always a bound parameter.** Workspace ids, tags, the
//!   limit. Nothing that came off a command line is ever concatenated.
//! - **A value from one of this crate's enums is interpolated as a literal.** `kind`,
//!   `status` and `relation` only ever produce `MemoryKind::as_str()` and friends —
//!   compile-time constants, unrepresentable as anything else. Interpolating them is
//!   what keeps `list` and `stats` free of the binding-order bugs that a dynamically
//!   built `IN` list invites, and the alternative would be a bug class rather than a
//!   line saved.
//!
//! # `unchecked_transaction` rather than `transaction`
//!
//! [`rusqlite::Connection::transaction`] needs `&mut Connection`, which would make
//! every method here take `&mut Database` and force each caller to hold a mutable
//! borrow across the whole command. `unchecked_transaction` takes `&self` and is
//! unchecked in exactly one way — it does not stop a nested transaction from being
//! started. This crate never nests them.

use rusqlite::types::Value as Sql;
use rusqlite::{Row, params};
use serde_json::Value;

use crate::db::Database;
use crate::error::{Error, Result};
use crate::gate;
use crate::hash;
use crate::id;
use crate::metadata;
use crate::model::{
    AddOutcome, ForgetOutcome, KindCount, ListQuery, ListSort, Memory, MemoryKind, MemoryPatch,
    NewMemory, Relation, Scope, Stats, Status, StatusCount, UpdateOutcome,
};

/// Every column a [`Memory`] needs, in one place so a query cannot half-populate a row.
///
/// Named columns and lookups by name rather than by position: the mapper is written
/// against these names, so adding a column to the table cannot silently shift every
/// field of every row.
pub(crate) const MEMORY_COLUMNS: &str = "
    id, workspace_id, kind, title, excerpt, body, tags, memory_metadata, source,
    source_url, confidence, importance, strength, corroboration_count, retrieval_count,
    last_retrieved_at, feedback_score, pinned, status, expires_at, created_at, updated_at
";

/// The timestamp expression every date column is written with: RFC 3339, UTC, second
/// precision, produced by SQLite's clock so there is exactly one clock in the system.
///
/// Seconds are enough because `id` is time-sortable and carries the milliseconds; and a
/// fixed-width UTC string compares lexicographically, which is what lets `expires_at`
/// and the decay window be filtered in SQL rather than parsed on the way out.
const INSERT_MEMORY_SQL: &str = "INSERT INTO memories (
        id, workspace_id, kind, title, excerpt, body, tags, tags_text,
        memory_metadata, source, source_url, confidence, importance, status,
        content_hash, created_at, updated_at
     ) VALUES (
        ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
        strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
        strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
     )";

/// The update statement, with `COALESCE` for "leave this alone".
///
/// Static rather than assembled from the fields the patch happens to set, because the
/// `mem_au` trigger in `0001_init.sql` fires on `UPDATE OF` and a `SET` clause that
/// mentions the indexed columns only sometimes would leave the full-text index stale
/// only sometimes. The cost is rebuilding the index row when just `--confidence` moved,
/// which is microseconds and buys the guarantee that an edit cannot be missed.
///
/// The consequence of `COALESCE` is that `NULL` means "unchanged", so a patch cannot
/// *clear* `confidence`, `source_url` or the metadata envelope. Nothing in v1 needs to.
const UPDATE_MEMORY_SQL: &str = "UPDATE memories SET
        title           = COALESCE(?2, title),
        excerpt         = COALESCE(?3, excerpt),
        body            = COALESCE(?4, body),
        tags            = COALESCE(?5, tags),
        tags_text       = COALESCE(?6, tags_text),
        kind            = COALESCE(?7, kind),
        confidence      = COALESCE(?8, confidence),
        importance      = COALESCE(?9, importance),
        source_url      = COALESCE(?10, source_url),
        memory_metadata = COALESCE(?11, memory_metadata),
        content_hash    = COALESCE(?12, content_hash),
        updated_at      = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
     WHERE id = ?1";

impl Database {
    /// Save a memory, or find the one that already holds the same content.
    ///
    /// One transaction. Either the memory exists and is indexed, or an identical one
    /// already did and its id comes back with `duplicate: true`, or nothing was written
    /// at all. There is no state in which half a memory is stored.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Invalid`] for an empty title, a confidence or importance outside
    /// `0.0..=1.0`, or more than [`MAX_TAGS`] tags. Returns [`Error::NotFound`] when the
    /// workspace does not exist.
    pub fn add(&self, new: &NewMemory) -> Result<AddOutcome> {
        let title = new.title.trim();
        if title.is_empty() {
            return Err(Error::Invalid(
                "a memory needs a title; pass --title, or --from-file to take one from a file"
                    .to_owned(),
            ));
        }
        check_unit_range("--confidence", new.confidence)?;
        check_unit_range("--importance", Some(new.importance))?;
        if new.source.trim().is_empty() {
            return Err(Error::Invalid(
                "--source needs a value; omit it to record a `user_saved` memory".to_owned(),
            ));
        }

        let mut warnings = Vec::new();
        let tags = metadata::normalize_tags(&new.tags).map_err(Error::Invalid)?;
        let validated = metadata::validate(new.kind, new.memory_metadata.as_ref());
        warnings.extend(validated.warning);

        let workspace = self.resolve_workspace(&new.workspace_id)?;
        let content_hash = hash::content_hash(new.kind, title, &new.body);

        let transaction = self.conn().unchecked_transaction()?;

        if !new.force {
            if let Some(existing) =
                find_duplicate_near(&transaction, &workspace.id, &content_hash, new)
            {
                transaction.commit()?;
                return Ok(AddOutcome {
                    id: existing.id,
                    workspace_id: existing.workspace_id,
                    status: existing.status,
                    duplicate: true,
                    warnings,
                });
            }
        }

        let status = gate::decide(&new.source, new.confidence, workspace.review_mode).status();
        let new_id = id::new_memory_id()?;

        transaction.execute(
            INSERT_MEMORY_SQL,
            params![
                new_id,
                workspace.id,
                new.kind,
                title,
                new.excerpt,
                new.body,
                encode_tags(&tags),
                metadata::tags_text(&tags),
                validated.value.to_string(),
                new.source.trim(),
                new.source_url,
                new.confidence,
                new.importance,
                status,
                content_hash,
            ],
        )?;

        // Provenance is wired at activation, never for a draft: a `derived_from` edge
        // from a memory nobody has approved would point at something retrieval cannot
        // return.
        if status == Status::Active && !new.derived_from.is_empty() {
            warnings.extend(link_derived_from(&transaction, &new_id, &new.derived_from)?);
        }

        transaction.commit()?;

        Ok(AddOutcome {
            id: new_id,
            workspace_id: workspace.id,
            status,
            duplicate: false,
            warnings,
        })
    }

    /// The memory that already holds this content, if there is one.
    ///
    /// Matches on the hash and then **compares the normalised text again**, which is what
    /// makes a 128-bit non-cryptographic hash safe here: a collision costs a missed
    /// dedupe, never a lost memory. See [`crate::hash`].
    ///
    /// Where several rows share the hash — which `--force` can produce — the active one
    /// wins, and among equals the oldest does. The oldest is the one with the history:
    /// the retrievals that stretched its decay clock and the ratings that moved its
    /// strength.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the query fails.
    pub fn find_duplicate(&self, workspace_id: &str, new: &NewMemory) -> Result<Option<Memory>> {
        let content_hash = hash::content_hash(new.kind, new.title.trim(), &new.body);
        Ok(find_duplicate_near(
            self.conn(),
            workspace_id,
            &content_hash,
            new,
        ))
    }

    /// One memory by id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] when there is no such memory. That distinction is part
    /// of the contract — exit code 2, not a generic failure — which is why this does not
    /// return `Option`.
    pub fn get(&self, id: &str) -> Result<Memory> {
        self.get_optional(id)?.ok_or_else(|| {
            Error::NotFound(format!(
                "no memory with id `{id}`. It may have been forgotten, archived, or \
                 exported from a different database."
            ))
        })
    }

    /// One memory by id, or `None`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the query fails.
    pub fn get_optional(&self, id: &str) -> Result<Option<Memory>> {
        let mut statement = self.conn().prepare(&format!(
            "SELECT {MEMORY_COLUMNS} FROM memories WHERE id = ?1"
        ))?;
        let mut rows = statement.query_map([id], row_to_memory)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Change a memory in place. `None` in the patch means "leave it alone".
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] for an unknown id, and [`Error::Invalid`] for a value
    /// out of range, an emptied title, or more than [`MAX_TAGS`] tags.
    pub fn update(&self, id: &str, patch: &MemoryPatch) -> Result<UpdateOutcome> {
        let existing = self.get(id)?;
        let mut warnings = Vec::new();

        if patch.is_empty() {
            return Ok(UpdateOutcome {
                id: existing.id,
                changed: 0,
                warnings,
            });
        }

        check_unit_range("--confidence", patch.confidence)?;
        check_unit_range("--importance", patch.importance)?;
        if patch
            .title
            .as_ref()
            .is_some_and(|title| title.trim().is_empty())
        {
            return Err(Error::Invalid(
                "--title cannot be emptied; change what the memory says with --body, or \
                 remove it with `kiomon forget`"
                    .to_owned(),
            ));
        }

        let tags = patch
            .tags
            .as_ref()
            .map(|tags| metadata::normalize_tags(tags))
            .transpose()
            .map_err(Error::Invalid)?;

        let validated = patch
            .memory_metadata
            .as_ref()
            .map(|raw| metadata::validate(patch.kind.unwrap_or(existing.kind), Some(raw)));
        if let Some(validated) = &validated {
            warnings.extend(validated.warning.clone());
        }

        // The hash covers kind, title and body, so it is recomputed only when one of those
        // three moved. `None` means "keep the existing value".
        let content_hash = if patch.title.is_some() || patch.body.is_some() || patch.kind.is_some()
        {
            Some(hash::content_hash(
                patch.kind.unwrap_or(existing.kind),
                patch.title.as_deref().unwrap_or(&existing.title),
                patch.body.as_deref().unwrap_or(&existing.body),
            ))
        } else {
            None
        };

        let changed = changed_fields(patch);

        self.conn().execute(
            UPDATE_MEMORY_SQL,
            params![
                existing.id,
                patch.title,
                patch.excerpt,
                patch.body,
                tags.as_deref().map(encode_tags),
                tags.as_deref().map(metadata::tags_text),
                patch.kind,
                patch.confidence,
                patch.importance,
                patch.source_url,
                validated.as_ref().map(|value| value.value.to_string()),
                content_hash,
            ],
        )?;

        Ok(UpdateOutcome {
            id: existing.id,
            changed,
            warnings,
        })
    }

    /// List memories without ranking them.
    ///
    /// `list` is not `search`: it is unranked, it does not touch the retrieval counters,
    /// and its ordering is a fact about the rows rather than an opinion about the query.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if a query fails.
    pub fn list(&self, query: &ListQuery) -> Result<Vec<Memory>> {
        // `fmt::Write` is what lets a clause be appended with `write!` instead of
        // `push_str(&format!(..))`, which would allocate a throwaway string per clause.
        use std::fmt::Write as _;

        let mut sql = format!("SELECT {MEMORY_COLUMNS} FROM memories WHERE 1 = 1");
        let mut values: Vec<Sql> = Vec::new();

        if let Some(workspace) = query.scope.one() {
            sql.push_str(" AND workspace_id = ?");
            values.push(Sql::Text(workspace.to_owned()));
        }
        if let Some(status) = query.status {
            // `write!` rather than `push_str(&format!(..))`: the latter allocates a
            // temporary string for every clause, and this one is built per query.
            let _ = write!(sql, " AND status = '{}'", status.as_str());
        }
        sql.push_str(&kind_filter(None, &query.kinds));
        for tag in &query.tags {
            // One `EXISTS` per requested tag, which makes the filter "has all of these"
            // rather than "has any of these". `lower()` on both sides for a
            // case-insensitive match; SQLite's `lower()` is ASCII-only, which is a
            // documented limit rather than a silent one — a non-ASCII tag has to be
            // spelled the way it was stored.
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM json_each(memories.tags) \
                 WHERE lower(json_each.value) = lower(?))",
            );
            values.push(Sql::Text(tag.clone()));
        }
        // `strength` is a relevance ordering, so pinned sorts first: a pinned memory the
        // user cannot find is a bug report, and `list` is where they would look.
        //
        // `recent` deliberately does not. "Newest first" with pinned rows hoisted to the
        // top is not newest first, and the cost of the hoist was measured: a leading
        // `pinned` term no index in `0001_init.sql` covers took `list --recent` from an
        // index walk to a full scan of the corpus plus a sort (41.5 ms against 16.8 ms at
        // five thousand memories). A recency view that is honest about being one is also
        // the faster of the two.
        match query.sort {
            ListSort::Strength => sql.push_str(" ORDER BY pinned DESC, strength DESC, id DESC"),
            ListSort::Recent => sql.push_str(" ORDER BY created_at DESC, id DESC"),
        }
        sql.push_str(" LIMIT ?");
        values.push(Sql::Integer(i64::from(query.limit)));

        let mut statement = self.conn().prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(values.iter()), row_to_memory)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Count one retrieval against each id, in one transaction, and report how many rows
    /// that actually moved.
    ///
    /// Neither column in the `SET` clause appears in the `mem_au` trigger's `UPDATE OF`
    /// list, so this does **not** rebuild the full-text index. That is not an
    /// optimisation — it is the reason the trigger has a column list at all, and it is why
    /// this is a separate statement rather than a flag on
    /// [`Database::update`]. A search that rewrote every hit's index row would make the
    /// ranking cost grow with how often the corpus is read.
    ///
    /// Keyed by the ULID rather than the rowid, because a `Memory` carries the former and
    /// the latter is an implementation detail of the index. `id` is `UNIQUE`, so the update
    /// is an index lookup either way.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if a statement fails. An id that no longer exists is
    /// not an error: it counts zero and the rest still counts.
    pub fn record_retrieval(&self, ids: &[String]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        // One statement with an `IN` list rather than one statement per id: measured at
        // 0.29 ms for ten against 0.76 ms for ten of their own, and a page can be fifty.
        // Both plans are an index search on the `UNIQUE` id either way, so the saving is
        // per-statement overhead and not a better plan.
        let placeholders = vec!["?"; ids.len()].join(", ");
        let sql = format!(
            "UPDATE memories
                SET retrieval_count = retrieval_count + 1,
                    last_retrieved_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
              WHERE id IN ({placeholders})"
        );
        let values: Vec<Sql> = ids.iter().map(|id| Sql::Text(id.clone())).collect();
        // A transaction for the atomicity, not for the batching: it is one statement, so
        // there is nothing to batch. Either the whole page is counted or none of it is,
        // and a crash cannot leave a search half-recorded.
        let transaction = self.conn().unchecked_transaction()?;
        let moved = transaction.execute(&sql, rusqlite::params_from_iter(values.iter()))?;
        transaction.commit()?;
        Ok(moved)
    }

    /// Count what is in scope.
    ///
    /// `kinds` is the same filter `--kind` applies everywhere else; empty means every
    /// kind, and `by_kind` then reports all four.
    ///
    /// One grouped scan rather than a dozen `count(*)` statements. The first version of
    /// this method ran twelve of them — a total, a pinned count, four kinds and three
    /// statuses, each counting the whole table — and measured 9.5 ms of work at ten
    /// thousand memories against 3 ms for an `add`. `stats` is supposed to be cheap
    /// enough to call on a whim, and one `GROUP BY` is one scan.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the query fails.
    pub fn stats(&self, scope: &Scope, kinds: &[MemoryKind]) -> Result<Stats> {
        let (scope_clause, bindings) = scope_clause(None, scope);
        let kinds_clause = kind_filter(None, kinds);
        let sql = format!(
            "SELECT kind, status, pinned, count(*) FROM memories              WHERE 1 = 1{scope_clause}{kinds_clause}              GROUP BY kind, status, pinned"
        );

        let mut statement = self.conn().prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(bindings.iter()), |row| {
            Ok(Cell {
                kind: row.get(0)?,
                status: row.get(1)?,
                pinned: row.get(2)?,
                count: row.get(3)?,
            })
        })?;
        let mut cells: Vec<Cell> = Vec::new();
        for row in rows {
            cells.push(row?);
        }

        let reported: &[MemoryKind] = if kinds.is_empty() {
            &MemoryKind::ALL
        } else {
            kinds
        };

        let total = cells.iter().map(|cell| cell.count).sum();
        let pinned = cells
            .iter()
            .filter(|cell| cell.pinned && cell.status == Status::Active)
            .map(|cell| cell.count)
            .sum();
        let workspaces = match scope {
            Scope::One(id) => i64::from(self.workspace(id)?.is_some()),
            Scope::All => self.count("SELECT count(*) FROM workspaces", &[])?,
        };

        // Every kind is reported, including the ones with no rows: a caller rendering a
        // table should not have to invent the missing rows, and an empty kind is a fact
        // worth showing.
        let by_kind = reported
            .iter()
            .map(|kind| KindCount {
                kind: kind.as_str(),
                kind_label: kind.label(),
                count: cells
                    .iter()
                    .filter(|cell| cell.kind == *kind)
                    .map(|cell| cell.count)
                    .sum(),
            })
            .collect();

        let by_status = Status::ALL
            .iter()
            .map(|status| StatusCount {
                status: status.as_str(),
                count: cells
                    .iter()
                    .filter(|cell| cell.status == *status)
                    .map(|cell| cell.count)
                    .sum(),
            })
            .collect();

        Ok(Stats {
            total,
            by_kind,
            by_status,
            pinned,
            workspaces,
        })
    }

    fn count(&self, sql: &str, bindings: &[Sql]) -> Result<i64> {
        let mut statement = self.conn().prepare(sql)?;
        let value = statement.query_row(rusqlite::params_from_iter(bindings.iter()), |row| {
            row.get(0)
        })?;
        Ok(value)
    }

    /// Delete memories, and everything keyed to them, permanently.
    ///
    /// The only destructive operation in the tool. Everything else — decay, `review
    /// reject`, `archive` — is a soft state that can be undone; this one cannot, which is
    /// why the command that reaches it refuses without an explicit `--yes`.
    ///
    /// One `DELETE` per id is the whole implementation, because the cascades in
    /// `0001_init.sql` remove the links, embeddings and outcomes and the `mem_ad` trigger
    /// removes the index row. That is the point of putting them in the schema: a second
    /// statement here would be a second thing to forget.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if a delete fails. A missing id is reported in the
    /// outcome rather than raised, because deleting five memories of which one was
    /// already gone should not undo the other four.
    pub fn forget(&self, ids: &[String]) -> Result<ForgetOutcome> {
        let transaction = self.conn().unchecked_transaction()?;
        let mut deleted = Vec::with_capacity(ids.len());
        let mut missing = Vec::new();
        for id in ids {
            let affected = transaction.execute("DELETE FROM memories WHERE id = ?1", [id])?;
            if affected == 0 {
                missing.push(id.clone());
            } else {
                deleted.push(id.clone());
            }
        }
        transaction.commit()?;
        Ok(ForgetOutcome { deleted, missing })
    }

    /// How many rows the full-text index holds.
    ///
    /// Worth its own method rather than a raw count because it is a diagnostic, not an
    /// internal detail: the index is maintained in Rust and by triggers, and comparing
    /// this against `stats().total` for a given scope is how a caller finds out that one
    /// of them stopped doing its job. `doctor` reports it; the store's integration tests
    /// assert on it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the count cannot be read.
    pub fn indexed_count(&self) -> Result<i64> {
        self.count("SELECT count(*) FROM memory_fts", &[])
    }
}

// ---------------------------------------------------------------------------
// Free functions over a connection, so code inside a transaction is the same code as
// code outside one.
// ---------------------------------------------------------------------------

/// The duplicate lookup, against any connection so it can see uncommitted rows.
fn find_duplicate_near(
    conn: &rusqlite::Connection,
    workspace_id: &str,
    content_hash: &str,
    new: &NewMemory,
) -> Option<Memory> {
    let mut statement = conn
        .prepare(&format!(
            "SELECT {MEMORY_COLUMNS} FROM memories
             WHERE workspace_id = ?1 AND content_hash = ?2
             ORDER BY (status = 'active') DESC, created_at ASC, id ASC
             LIMIT 1"
        ))
        .ok()?;
    let mut rows = statement
        .query_map(params![workspace_id, content_hash], row_to_memory)
        .ok()?;
    let candidate = rows.next()?.ok()?;

    // The comparison that makes the hash safe: a hash match alone is not a duplicate.
    let same = candidate.kind == new.kind
        && hash::normalize(&candidate.title) == hash::normalize(new.title.trim())
        && hash::normalize(&candidate.body) == hash::normalize(&new.body);
    same.then_some(candidate)
}

/// Wire `derived_from` edges for ids that exist.
///
/// Returns a warning for each id that does not, rather than failing: an agent citing a
/// memory that has since been forgotten should still get its new memory stored, and a
/// write that fails because of an earlier deletion is a worse outcome than a missing
/// edge with a note about it.
fn link_derived_from(
    conn: &rusqlite::Connection,
    source_id: &str,
    targets: &[String],
) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    let mut insert = conn.prepare(
        "INSERT OR IGNORE INTO memory_links
            (source_id, target_id, relation, directed, origin, created_at)
         VALUES (?1, ?2, ?3, 1, 'agent', strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))",
    )?;
    let mut exists = conn.prepare("SELECT 1 FROM memories WHERE id = ?1")?;
    for target in targets {
        let trimmed = target.trim();
        if trimmed.is_empty() || trimmed == source_id {
            continue;
        }
        let found: Option<i64> = exists.query_row([trimmed], |row| row.get(0)).ok();
        if found.is_none() {
            warnings.push(format!(
                "no memory with id `{trimmed}` to derive from; skipping that link"
            ));
            continue;
        }
        insert.execute(params![source_id, trimmed, Relation::DerivedFrom])?;
    }
    Ok(warnings)
}

/// `WHERE kind IN (…)`, built from this crate's own enum.
///
/// Interpolated rather than bound on purpose: the values come from `MemoryKind::as_str()`
/// and cannot be anything else, and a literal keeps the binding order of the caller's
/// other parameters trivially correct. See the module comment.
///
/// `qualifier` is the table prefix to use. Empty for a query that reads one table;
/// `memories.` when the full-text index is joined in, where the index also has a `title`,
/// an `excerpt` and a `body` and an unqualified clause would resolve by luck.
pub(crate) fn kind_filter(qualifier: Option<&str>, kinds: &[MemoryKind]) -> String {
    if kinds.is_empty() {
        return String::new();
    }
    let list: Vec<&str> = kinds.iter().map(|kind| kind.as_str()).collect();
    format!(
        " AND {}kind IN ('{}')",
        qualifier.unwrap_or(""),
        list.join("', '")
    )
}

/// `(clause, bindings)` for a scope, ready to append to a `WHERE 1 = 1`.
///
/// `qualifier` as in [`kind_filter`].
pub(crate) fn scope_clause(qualifier: Option<&str>, scope: &Scope) -> (String, Vec<Sql>) {
    match scope {
        Scope::One(id) => (
            format!(" AND {}workspace_id = ?", qualifier.unwrap_or("")),
            vec![Sql::Text(id.clone())],
        ),
        // The explicit whole-corpus case. There is no clause, which is exactly why it
        // has to be asked for: forgetting `--workspace` must never produce this.
        Scope::All => (String::new(), Vec::new()),
    }
}

/// One row of the `stats` group-by: a kind/status/pinned tuple and how many memories are
/// in it. At most 4 x 3 x 2 = 24 rows, whatever the corpus size.
struct Cell {
    kind: MemoryKind,
    status: Status,
    pinned: bool,
    count: i64,
}

/// How many fields a patch sets, for the `changed` count a caller reports.
fn changed_fields(patch: &MemoryPatch) -> usize {
    [
        patch.title.is_some(),
        patch.excerpt.is_some(),
        patch.body.is_some(),
        patch.tags.is_some(),
        patch.kind.is_some(),
        patch.confidence.is_some(),
        patch.importance.is_some(),
        patch.source_url.is_some(),
        patch.memory_metadata.is_some(),
    ]
    .iter()
    .filter(|set| **set)
    .count()
}

/// Reject a value outside `0.0..=1.0`, and NaN.
///
/// Checked here rather than left to the `CHECK` constraint so the message names the flag
/// the user typed. A constraint violation would arrive as "database error: CHECK
/// constraint failed", which tells them nothing about which argument was wrong. `NaN`
/// fails the range test, which is the behaviour we want.
fn check_unit_range(flag: &str, value: Option<f64>) -> Result<()> {
    if let Some(value) = value {
        if !(0.0..=1.0).contains(&value) {
            return Err(Error::Invalid(format!(
                "{flag} must be between 0.0 and 1.0, got {value}"
            )));
        }
    }
    Ok(())
}

/// Render a tag list for storage.
fn encode_tags(tags: &[String]) -> String {
    serde_json::to_string(tags).unwrap_or_else(|_| "[]".to_owned())
}

/// Read a JSON column, falling back rather than failing.
///
/// The column is `NOT NULL DEFAULT '{}'` and only ever written by this module, so a
/// value that does not parse means the file was edited outside the tool. Failing the
/// read would make `kiomon list` unusable because of one bad row, which is a much worse
/// outcome than showing that row with an empty envelope.
fn decode_object(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::Object(serde_json::Map::new()))
}

fn decode_tags(text: &str) -> Vec<String> {
    serde_json::from_str(text).unwrap_or_default()
}

pub(crate) fn row_to_memory(row: &Row<'_>) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: row.get("id")?,
        workspace_id: row.get("workspace_id")?,
        kind: row.get("kind")?,
        title: row.get("title")?,
        excerpt: row.get("excerpt")?,
        body: row.get("body")?,
        tags: decode_tags(&row.get::<_, String>("tags")?),
        memory_metadata: decode_object(&row.get::<_, String>("memory_metadata")?),
        source: row.get("source")?,
        source_url: row.get("source_url")?,
        confidence: row.get("confidence")?,
        importance: row.get("importance")?,
        strength: row.get("strength")?,
        corroboration_count: row.get("corroboration_count")?,
        retrieval_count: row.get("retrieval_count")?,
        last_retrieved_at: row.get("last_retrieved_at")?,
        feedback_score: row.get("feedback_score")?,
        pinned: row.get("pinned")?,
        status: row.get("status")?,
        expires_at: row.get("expires_at")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MAX_TAGS, ReviewMode};
    use crate::testing;

    fn add_semantic(db: &Database, title: &str, body: &str) -> String {
        testing::add(db, MemoryKind::Semantic, title, body)
    }

    #[test]
    fn a_memory_round_trips_through_every_field() {
        let db = testing::db();
        let new = NewMemory::new(MemoryKind::Procedural, "Release checklist")
            .body("1. Tag\n2. Push\n3. Deploy")
            .excerpt("How we ship")
            .tags(vec!["release".to_owned(), "process".to_owned()])
            .source("agent_reflection")
            .source_url("https://example.com/runbook")
            .confidence(0.9)
            .importance(0.8)
            .metadata(Some(serde_json::json!({
                "trigger": "when cutting a release",
                "steps": ["tag", "push", "deploy"],
                "priority": "high",
            })));

        let outcome = db.add(&new).unwrap();
        assert!(!outcome.duplicate);
        assert_eq!(outcome.status, Status::Active);

        let stored = db.get(&outcome.id).unwrap();
        assert_eq!(stored.id, outcome.id);
        assert_eq!(stored.workspace_id, "default");
        assert_eq!(stored.kind, MemoryKind::Procedural);
        assert_eq!(stored.title, "Release checklist");
        assert_eq!(stored.excerpt, "How we ship");
        assert_eq!(stored.body, "1. Tag\n2. Push\n3. Deploy");
        assert_eq!(stored.tags, vec!["release", "process"]);
        assert_eq!(stored.source, "agent_reflection");
        assert_eq!(
            stored.source_url.as_deref(),
            Some("https://example.com/runbook")
        );
        assert_eq!(stored.confidence, Some(0.9));
        assert!((stored.importance - 0.8).abs() < f64::EPSILON);
        assert!((stored.strength - 1.0).abs() < f64::EPSILON);
        assert_eq!(stored.status, Status::Active);
        assert!(!stored.pinned);
        assert_eq!(stored.retrieval_count, 0);
        assert_eq!(stored.corroboration_count, 0);
        assert!((stored.feedback_score).abs() < f64::EPSILON);
        assert_eq!(
            stored.memory_metadata["priority"],
            serde_json::json!("high")
        );
        assert!(!stored.created_at.is_empty());
        assert_eq!(stored.created_at, stored.updated_at);
        assert_eq!(stored.kind_label(), "Routines");
        assert_eq!(stored.source_label(), "reflection");
    }

    #[test]
    fn a_minimal_memory_has_the_documented_defaults() {
        let db = testing::db();
        let memory = testing::add_and_get(&db, MemoryKind::Reference, "A file", "content");
        assert_eq!(memory.excerpt, "");
        assert!(memory.tags.is_empty());
        assert_eq!(memory.memory_metadata, serde_json::json!({}));
        assert_eq!(memory.source, "user_saved");
        assert_eq!(memory.source_label(), "saved");
        assert_eq!(memory.confidence, None);
        assert!((memory.importance - 0.5).abs() < f64::EPSILON);
        assert_eq!(memory.kind_label(), "Files");
    }

    #[test]
    fn an_empty_title_is_rejected_with_a_message_naming_the_flag() {
        let db = testing::db();
        let err = db
            .add(&NewMemory::new(MemoryKind::Semantic, "   "))
            .unwrap_err();
        assert_eq!(err.exit_code(), crate::ExitCode::InvalidArgs);
        assert!(err.to_string().contains("--title"), "{err}");
    }

    #[test]
    fn a_confidence_outside_zero_to_one_is_rejected_before_sqlite_sees_it() {
        let db = testing::db();
        let err = db
            .add(&NewMemory::new(MemoryKind::Semantic, "T").confidence(1.5))
            .unwrap_err();
        assert_eq!(err.exit_code(), crate::ExitCode::InvalidArgs);
        assert!(err.to_string().contains("--confidence"), "{err}");
        assert!(
            !err.to_string().contains("CHECK constraint"),
            "the message must name the flag, not the constraint: {err}"
        );
    }

    #[test]
    fn more_than_twenty_tags_is_rejected() {
        let db = testing::db();
        let tags: Vec<String> = (0..=MAX_TAGS).map(|index| format!("t{index}")).collect();
        let err = db
            .add(&NewMemory::new(MemoryKind::Semantic, "T").tags(tags))
            .unwrap_err();
        assert_eq!(err.exit_code(), crate::ExitCode::InvalidArgs);
    }

    #[test]
    fn an_unknown_workspace_is_a_not_found_that_lists_what_exists() {
        let db = testing::db();
        let err = db
            .add(&NewMemory::new(MemoryKind::Semantic, "T").workspace("nowhere"))
            .unwrap_err();
        assert_eq!(err.exit_code(), crate::ExitCode::NotFound);
        assert!(err.to_string().contains("default"), "{err}");
    }

    #[test]
    fn a_duplicate_add_writes_nothing_and_reports_the_existing_id() {
        let db = testing::db();
        let first = add_semantic(&db, "Deploys need migrations", "Run them first");
        let second = db
            .add(
                &NewMemory::new(MemoryKind::Semantic, "Deploys need migrations")
                    .body("Run them first"),
            )
            .unwrap();

        assert!(second.duplicate);
        assert_eq!(second.id, first);
        assert_eq!(second.status, Status::Active);
        assert_eq!(
            db.count("SELECT count(*) FROM memories", &[]).unwrap(),
            1,
            "a duplicate must not write a second row"
        );
    }

    #[test]
    fn a_duplicate_carries_the_status_so_the_caller_has_a_lever() {
        // The defect the hosted product's plan 028 records: a duplicate reply that says
        // only `duplicate: true` leaves a stranded memory with no way out.
        let db = testing::db();
        let first = db
            .add(
                &NewMemory::new(MemoryKind::Semantic, "Uncertain claim")
                    .body("maybe")
                    .source("agent_reflection")
                    .confidence(0.2),
            )
            .unwrap();
        assert_eq!(first.status, Status::PendingApproval);

        let second = db
            .add(&NewMemory::new(MemoryKind::Semantic, "Uncertain claim").body("maybe"))
            .unwrap();
        assert!(second.duplicate);
        assert_eq!(
            second.status,
            Status::PendingApproval,
            "a caller must be able to see that the existing row is waiting for review"
        );
    }

    #[test]
    fn a_duplicate_is_matched_through_case_and_whitespace() {
        let db = testing::db();
        let first = add_semantic(&db, "Deploy Step", "Run migrations");
        let second = db
            .add(
                &NewMemory::new(MemoryKind::Semantic, "  deploy   step ").body("run  MIGRATIONS\n"),
            )
            .unwrap();
        assert!(second.duplicate, "normalised text should be the same");
        assert_eq!(second.id, first);
    }

    #[test]
    fn force_stores_a_genuine_repeat() {
        let db = testing::db();
        add_semantic(&db, "Same", "Same");
        let forced = db
            .add(
                &NewMemory::new(MemoryKind::Semantic, "Same")
                    .body("Same")
                    .force(true),
            )
            .unwrap();
        assert!(!forced.duplicate);
        assert_eq!(db.count("SELECT count(*) FROM memories", &[]).unwrap(), 2);
    }

    #[test]
    fn a_forced_repeat_still_reports_the_older_row_as_the_duplicate() {
        // The oldest row is the one with the history: the retrievals that stretched its
        // decay clock and the ratings that moved its strength.
        let db = testing::db();
        let first = add_semantic(&db, "Same", "Same");
        db.add(
            &NewMemory::new(MemoryKind::Semantic, "Same")
                .body("Same")
                .force(true),
        )
        .unwrap();
        let found = db
            .add(&NewMemory::new(MemoryKind::Semantic, "Same").body("Same"))
            .unwrap();
        assert!(found.duplicate);
        assert_eq!(found.id, first);
    }

    #[test]
    fn the_same_text_in_a_different_kind_is_a_different_memory() {
        let db = testing::db();
        add_semantic(&db, "Same words", "Same body");
        let other = db
            .add(&NewMemory::new(MemoryKind::Episodic, "Same words").body("Same body"))
            .unwrap();
        assert!(!other.duplicate);
        assert_eq!(db.count("SELECT count(*) FROM memories", &[]).unwrap(), 2);
    }

    #[test]
    fn the_same_text_in_a_different_workspace_is_a_different_memory() {
        let db = testing::db();
        db.create_workspace("Other", ReviewMode::Autonomous)
            .unwrap();
        add_semantic(&db, "Same", "Same");
        let other = db
            .add(
                &NewMemory::new(MemoryKind::Semantic, "Same")
                    .body("Same")
                    .workspace("other"),
            )
            .unwrap();
        assert!(!other.duplicate);
    }

    #[test]
    fn an_agent_write_below_the_threshold_lands_in_the_review_inbox() {
        let db = testing::db();
        let outcome = db
            .add(
                &NewMemory::new(MemoryKind::Semantic, "Half-remembered")
                    .body("probably true")
                    .source("agent_reflection")
                    .confidence(0.4),
            )
            .unwrap();
        assert_eq!(outcome.status, Status::PendingApproval);
    }

    #[test]
    fn a_review_workspace_holds_back_even_a_certain_write() {
        let db = testing::db();
        db.create_workspace("Careful", ReviewMode::Review).unwrap();
        let outcome = db
            .add(
                &NewMemory::new(MemoryKind::Semantic, "Certain")
                    .source("user_saved")
                    .confidence(1.0)
                    .workspace("careful"),
            )
            .unwrap();
        assert_eq!(outcome.status, Status::PendingApproval);
    }

    #[test]
    fn a_metadatum_of_the_wrong_type_is_discarded_and_the_memory_still_lands() {
        let db = testing::db();
        let outcome = db
            .add(
                &NewMemory::new(MemoryKind::Semantic, "T")
                    .metadata(Some(serde_json::json!({"entities": "not a list"}))),
            )
            .unwrap();
        assert!(!outcome.duplicate);
        assert_eq!(outcome.warnings.len(), 1, "{:?}", outcome.warnings);
        assert!(outcome.warnings[0].contains("entities"));
        let stored = db.get(&outcome.id).unwrap();
        assert_eq!(stored.memory_metadata, serde_json::json!({}));
        assert_eq!(stored.title, "T", "the memory itself must survive");
    }

    #[test]
    fn derived_from_edges_are_wired_only_for_targets_that_exist() {
        let db = testing::db();
        let source = add_semantic(&db, "Source", "the origin");
        let outcome = db
            .add(
                &NewMemory::new(MemoryKind::Semantic, "Derived")
                    .derived_from(vec![source.clone(), "k_does_not_exist".to_owned()]),
            )
            .unwrap();

        assert_eq!(outcome.warnings.len(), 1);
        assert!(outcome.warnings[0].contains("k_does_not_exist"));

        let links = db
            .count(
                "SELECT count(*) FROM memory_links WHERE source_id = ?1",
                &[Sql::Text(outcome.id.clone())],
            )
            .unwrap();
        assert_eq!(links, 1);
    }

    #[test]
    fn derived_from_edges_are_not_written_for_a_draft() {
        // A graph edge from a memory nobody has approved points at something retrieval
        // cannot return.
        let db = testing::db();
        let source = add_semantic(&db, "Source", "the origin");
        let draft = db
            .add(
                &NewMemory::new(MemoryKind::Semantic, "Draft")
                    .source("agent_reflection")
                    .confidence(0.1)
                    .derived_from(vec![source]),
            )
            .unwrap();
        assert_eq!(draft.status, Status::PendingApproval);
        assert_eq!(
            db.count("SELECT count(*) FROM memory_links", &[]).unwrap(),
            0
        );
    }

    #[test]
    fn get_names_the_id_it_could_not_find() {
        let db = testing::db();
        let err = db.get("k_01JC9W5K2Q").unwrap_err();
        assert_eq!(err.exit_code(), crate::ExitCode::NotFound);
        assert!(err.to_string().contains("k_01JC9W5K2Q"), "{err}");
        assert_eq!(db.get_optional("k_01JC9W5K2Q").unwrap(), None);
    }

    #[test]
    fn an_empty_patch_changes_nothing_and_says_so() {
        let db = testing::db();
        let id = add_semantic(&db, "T", "B");
        let before = db.get(&id).unwrap();
        let outcome = db.update(&id, &MemoryPatch::default()).unwrap();
        assert_eq!(outcome.changed, 0);
        assert_eq!(db.get(&id).unwrap(), before, "not even updated_at may move");
    }

    #[test]
    fn a_patch_moves_only_the_fields_it_sets() {
        let db = testing::db();
        let id = add_semantic(&db, "Title", "Body");
        let outcome = db
            .update(
                &id,
                &MemoryPatch {
                    confidence: Some(0.25),
                    ..MemoryPatch::default()
                },
            )
            .unwrap();
        assert_eq!(outcome.changed, 1);
        let after = db.get(&id).unwrap();
        assert_eq!(after.confidence, Some(0.25));
        assert_eq!(after.title, "Title");
        assert_eq!(after.body, "Body");
        assert_eq!(after.kind, MemoryKind::Semantic);
    }

    #[test]
    fn editing_an_indexed_field_rehashes_the_memory() {
        let db = testing::db();
        let id = add_semantic(&db, "Original", "Body");
        db.update(
            &id,
            &MemoryPatch {
                title: Some("Rewritten".to_owned()),
                ..MemoryPatch::default()
            },
        )
        .unwrap();
        // The new text dedupes against the edited row, which is only true if the hash
        // moved with it.
        let again = db
            .add(&NewMemory::new(MemoryKind::Semantic, "Rewritten").body("Body"))
            .unwrap();
        assert!(again.duplicate);
        assert_eq!(again.id, id);
    }

    #[test]
    fn a_patch_that_empties_the_title_is_rejected() {
        let db = testing::db();
        let id = add_semantic(&db, "T", "B");
        let err = db
            .update(
                &id,
                &MemoryPatch {
                    title: Some("   ".to_owned()),
                    ..MemoryPatch::default()
                },
            )
            .unwrap_err();
        assert_eq!(err.exit_code(), crate::ExitCode::InvalidArgs);
    }

    #[test]
    fn updating_an_unknown_id_is_a_not_found() {
        let db = testing::db();
        let err = db
            .update(
                "k_nope",
                &MemoryPatch {
                    title: Some("x".to_owned()),
                    ..MemoryPatch::default()
                },
            )
            .unwrap_err();
        assert_eq!(err.exit_code(), crate::ExitCode::NotFound);
    }

    #[test]
    fn list_shows_active_memories_and_hides_the_inbox() {
        let db = testing::db();
        add_semantic(&db, "Visible", "body");
        db.add(
            &NewMemory::new(MemoryKind::Semantic, "Waiting")
                .source("agent_reflection")
                .confidence(0.1),
        )
        .unwrap();

        let rows = db.list(&ListQuery::default()).unwrap();
        assert_eq!(rows.len(), 1, "a bare list must not show the review inbox");
        assert_eq!(rows[0].title, "Visible");
    }

    #[test]
    fn list_filters_by_kind_status_and_tag() {
        let db = testing::db();
        add_semantic(&db, "A fact", "body");
        testing::add(&db, MemoryKind::Episodic, "A moment", "body");
        let tagged = db
            .add(
                &NewMemory::new(MemoryKind::Semantic, "Tagged")
                    .body("body")
                    .tags(vec!["deploy".to_owned()]),
            )
            .unwrap();

        let episodic = db
            .list(&ListQuery {
                kinds: vec![MemoryKind::Episodic],
                ..ListQuery::default()
            })
            .unwrap();
        assert_eq!(episodic.len(), 1);
        assert_eq!(episodic[0].title, "A moment");

        let by_tag = db
            .list(&ListQuery {
                tags: vec!["DEPLOY".to_owned()],
                ..ListQuery::default()
            })
            .unwrap();
        assert_eq!(by_tag.len(), 1);
        assert_eq!(by_tag[0].id, tagged.id);

        let two_tags = db
            .list(&ListQuery {
                tags: vec!["deploy".to_owned(), "absent".to_owned()],
                ..ListQuery::default()
            })
            .unwrap();
        assert!(two_tags.is_empty(), "every requested tag must be present");

        let inbox = db
            .list(&ListQuery {
                status: Some(Status::PendingApproval),
                ..ListQuery::default()
            })
            .unwrap();
        assert!(inbox.is_empty(), "nothing is pending in this fixture");
    }

    #[test]
    fn list_can_show_every_status_at_once() {
        let db = testing::db();
        add_semantic(&db, "Active", "body");
        db.add(
            &NewMemory::new(MemoryKind::Semantic, "Draft")
                .source("agent_reflection")
                .confidence(0.1),
        )
        .unwrap();
        let rows = db
            .list(&ListQuery {
                status: None,
                ..ListQuery::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn the_relevance_ordering_puts_pinned_first() {
        let db = testing::db();
        let weak = add_semantic(&db, "First", "body");
        let strong = add_semantic(&db, "Second", "body");
        db.conn()
            .execute("UPDATE memories SET pinned = 1 WHERE id = ?1", [&weak])
            .unwrap();
        db.conn()
            .execute("UPDATE memories SET strength = 0.1 WHERE id = ?1", [&weak])
            .unwrap();

        let rows = db.list(&ListQuery::default()).unwrap();
        assert_eq!(
            rows[0].id, weak,
            "a pinned memory must be findable in the list"
        );
        assert_eq!(rows[1].id, strong);
    }

    #[test]
    fn the_recency_ordering_is_honestly_recent() {
        // A pinned row must *not* jump the queue here. Hoisting it would make "newest
        // first" mean "pinned first, then newest", and it also cost a full corpus scan:
        // see the ordering comment in `list`.
        let db = testing::db();
        let first = add_semantic(&db, "First", "body");
        db.conn()
            .execute("UPDATE memories SET pinned = 1 WHERE id = ?1", [&first])
            .unwrap();
        let second = add_semantic(&db, "Second", "body");
        db.conn()
            .execute(
                "UPDATE memories SET created_at = '2099-01-01T00:00:00Z' WHERE id = ?1",
                [&second],
            )
            .unwrap();

        let rows = db
            .list(&ListQuery {
                sort: ListSort::Recent,
                ..ListQuery::default()
            })
            .unwrap();
        assert_eq!(rows[0].id, second, "the newer row comes first");
        assert_eq!(rows[1].id, first);
    }

    #[test]
    fn a_scope_of_all_crosses_workspaces_and_a_scope_of_one_does_not() {
        let db = testing::db();
        db.create_workspace("Other", ReviewMode::Autonomous)
            .unwrap();
        add_semantic(&db, "Here", "body");
        db.add(
            &NewMemory::new(MemoryKind::Semantic, "There")
                .body("body")
                .workspace("other"),
        )
        .unwrap();

        let one = db.list(&ListQuery::default()).unwrap();
        assert_eq!(one.len(), 1, "the default scope is one workspace");
        let all = db
            .list(&ListQuery {
                scope: Scope::All,
                ..ListQuery::default()
            })
            .unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn list_respects_its_limit() {
        let db = testing::db();
        for index in 0..10 {
            add_semantic(&db, &format!("Memory {index}"), "body");
        }
        let rows = db
            .list(&ListQuery {
                limit: 3,
                ..ListQuery::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn forget_removes_the_row_the_index_and_every_edge() {
        let db = testing::db();
        let source = add_semantic(&db, "Source", "body");
        let derived = db
            .add(
                &NewMemory::new(MemoryKind::Semantic, "Derived")
                    .body("body")
                    .derived_from(vec![source.clone()]),
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO memory_outcomes (memory_id, outcome, assessment, created_at)
                 VALUES (?1, 'success', 'helpful', 'now')",
                [&source],
            )
            .unwrap();

        assert_eq!(testing::fts_rows(&db).unwrap(), 2);

        let outcome = db.forget(&[source.clone(), derived.id.clone()]).unwrap();
        assert_eq!(outcome.deleted.len(), 2);
        assert!(outcome.missing.is_empty());

        assert_eq!(db.count("SELECT count(*) FROM memories", &[]).unwrap(), 0);
        assert_eq!(db.count("SELECT count(*) FROM memory_fts", &[]).unwrap(), 0);
        assert_eq!(
            db.count("SELECT count(*) FROM memory_links", &[]).unwrap(),
            0
        );
        assert_eq!(
            db.count("SELECT count(*) FROM memory_outcomes", &[])
                .unwrap(),
            0
        );
        assert!(db.get_optional(&source).unwrap().is_none());
        assert!(db.integrity().unwrap().is_ok());
    }

    #[test]
    fn forget_reports_missing_ids_rather_than_undoing_the_rest() {
        let db = testing::db();
        let id = add_semantic(&db, "Real", "body");
        let outcome = db.forget(&[id.clone(), "k_01JC9W5K2Q".to_owned()]).unwrap();
        assert_eq!(outcome.deleted, vec![id]);
        assert_eq!(outcome.missing, vec!["k_01JC9W5K2Q"]);
    }

    #[test]
    fn stats_count_by_kind_and_status() {
        let db = testing::db();
        add_semantic(&db, "Fact", "body");
        testing::add(&db, MemoryKind::Episodic, "Moment", "body");
        db.add(
            &NewMemory::new(MemoryKind::Semantic, "Draft")
                .source("agent_reflection")
                .confidence(0.1),
        )
        .unwrap();

        let stats = db.stats(&Scope::One("default".to_owned()), &[]).unwrap();
        assert_eq!(stats.total, 3);
        assert_eq!(stats.workspaces, 1);
        assert_eq!(stats.by_kind.len(), MemoryKind::ALL.len());
        let semantic = stats
            .by_kind
            .iter()
            .find(|entry| entry.kind == "semantic")
            .unwrap();
        assert_eq!(semantic.count, 2);
        assert_eq!(semantic.kind_label, "Facts");
        let pending = stats
            .by_status
            .iter()
            .find(|entry| entry.status == "pending_approval")
            .unwrap();
        assert_eq!(pending.count, 1);
        assert_eq!(
            stats.by_status.iter().map(|entry| entry.count).sum::<i64>(),
            3,
            "every row has exactly one status"
        );
    }

    #[test]
    fn stats_honours_a_kind_filter_in_every_number_it_reports() {
        let db = testing::db();
        add_semantic(&db, "Fact", "body");
        testing::add(&db, MemoryKind::Episodic, "Moment", "body");

        let stats = db
            .stats(&Scope::One("default".to_owned()), &[MemoryKind::Semantic])
            .unwrap();
        assert_eq!(stats.total, 1);
        assert_eq!(stats.by_kind.len(), 1);
        assert_eq!(stats.by_kind[0].kind, "semantic");
        assert_eq!(
            stats.by_status.iter().map(|entry| entry.count).sum::<i64>(),
            stats.total,
            "the status counts must agree with the total they are part of"
        );
    }

    #[test]
    fn stats_can_cross_workspaces_when_asked() {
        let db = testing::db();
        db.create_workspace("Other", ReviewMode::Autonomous)
            .unwrap();
        add_semantic(&db, "Here", "body");
        db.add(&NewMemory::new(MemoryKind::Semantic, "There").workspace("other"))
            .unwrap();

        let one = db.stats(&Scope::One("default".to_owned()), &[]).unwrap();
        assert_eq!(one.total, 1);
        let all = db.stats(&Scope::All, &[]).unwrap();
        assert_eq!(all.total, 2);
        assert_eq!(all.workspaces, 2);
    }
}
