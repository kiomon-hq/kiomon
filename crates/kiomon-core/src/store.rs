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
use crate::import::{ExportRecord, ImportBatch, ImportOutcome, ImportRow, ImportState};
use crate::metadata;
use crate::model::{
    AddOutcome, EmbedReport, EmbedState, ForgetOutcome, KindCount, ListQuery, ListSort,
    MAX_BODY_BYTES, MAX_EXCERPT, MAX_TITLE, Memory, MemoryKind, MemoryPatch, NewMemory, Relation,
    ReviewMode, Scope, Stats, Status, StatusCount, UpdateOutcome,
};

/// Every column a [`Memory`] needs, in one place so a query cannot half-populate a row.
///
/// Named columns and lookups by name rather than by position: the mapper is written
/// against these names, so adding a column to the table cannot silently shift every
/// field of every row.
pub(crate) const MEMORY_COLUMNS: &str = "
    id, workspace_id, kind, title, excerpt, body, tags, memory_metadata, source,
    source_url, confidence, importance, strength, corroboration_count, retrieval_count,
    last_retrieved_at, feedback_score, feedback_count, pinned, status, expires_at,
    created_at, updated_at
";

/// How many columns [`MEMORY_COLUMNS`] holds.
///
/// The export's `SELECT` appends one column to that list, and a mapper that read by
/// position would need to know where the list ends. Counting here, once, next to the list
/// itself, is what keeps the two from drifting — `MEMORY_COLUMNS` is a string, so nothing
/// else can count it.
pub(crate) const MEMORY_COLUMN_COUNT: usize = 23;

/// The timestamp expression every date column is written with: RFC 3339, UTC, second
/// precision, produced by SQLite's clock so there is exactly one clock in the system.
///
/// Seconds are enough because `id` is time-sortable and carries the milliseconds; and a
/// fixed-width UTC string compares lexicographically, which is what lets `expires_at`
/// and the decay window be filtered in SQL rather than parsed on the way out.
const INSERT_MEMORY_SQL: &str = "INSERT INTO memories (
        id, workspace_id, kind, title, excerpt, body, tags, tags_text,
        memory_metadata, source, source_url, confidence, importance, status,
        content_hash, pending_derived_from, created_at, updated_at
     ) VALUES (
        ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
        strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
        strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
     )";

/// The text limits every write shares, checked in one place.
///
/// One rule for `add`, `update` and `import` rather than a stricter importer: a memory the
/// file may hold and the CLI may not is a memory the user cannot fix, and the numbers are
/// the hosted route's own (see [`MAX_TITLE`]) so a local memory is one the hosted product
/// would accept too.
fn check_text_limits(title: &str, excerpt: &str, body: &str) -> Result<()> {
    let title_len = title.chars().count();
    if title_len > MAX_TITLE {
        return Err(Error::Invalid(format!(
            "the title is {title_len} characters, over the {MAX_TITLE}-character limit"
        )));
    }
    let excerpt_len = excerpt.chars().count();
    if excerpt_len > MAX_EXCERPT {
        return Err(Error::Invalid(format!(
            "the excerpt is {excerpt_len} characters, over the {MAX_EXCERPT}-character limit"
        )));
    }
    if body.len() > MAX_BODY_BYTES {
        return Err(Error::Invalid(format!(
            "the body is {} bytes, over the {MAX_BODY_BYTES}-byte limit. A memory is a              memory, not a file: save the path instead.",
            body.len()
        )));
    }
    Ok(())
}

/// The insert an imported line uses, with the id and the lifecycle supplied.
///
/// `created_at` and `updated_at` are `COALESCE`d rather than required: a hand-written line
/// has no reason to carry them, and "now" is the only other honest answer. Everything else
/// is the file's own value, including the counters and `strength` — see [`insert_imported`].
const INSERT_IMPORTED_SQL: &str = "INSERT INTO memories (
        id, workspace_id, kind, title, excerpt, body, tags, tags_text,
        memory_metadata, source, source_url, confidence, importance, strength,
        corroboration_count, retrieval_count, last_retrieved_at, feedback_score,
        feedback_count, pinned, status, expires_at, content_hash, pending_derived_from,
        created_at, updated_at
     ) VALUES (
        ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
        ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24,
        COALESCE(?25, strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
        COALESCE(?26, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
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
    /// # Errors
    ///
    /// Returns [`Error::Invalid`] for an empty title, a confidence or importance outside
    /// `0.0..=1.0`, or more than [`MAX_TAGS`] tags. Returns [`Error::NotFound`] when the
    /// workspace does not exist.
    pub fn add(&self, new: &NewMemory) -> Result<AddOutcome> {
        let workspace = self.resolve_workspace(&new.workspace_id)?;
        let transaction = self.conn().unchecked_transaction()?;
        let mut outcome = insert_one(&transaction, &workspace.id, workspace.review_mode, new)?;
        transaction.commit()?;
        // After the commit, never inside the transaction: the embedder is a network call,
        // and a write lock held across one is a write lock held for as long as someone
        // else's model takes to load. A failure here is a warning on a write that already
        // succeeded — the memory is stored and findable lexically, and `reindex --embed`
        // exists to come back for it.
        if let Err(error) = self.embed_ids(std::slice::from_ref(&outcome.id)) {
            outcome.warnings.push(format!(
                "stored, but not embedded: {error}. `kiomon reindex --embed` retries it."
            ));
        }
        Ok(outcome)
    }

    /// Save many memories, in one transaction, through the same insert path as one.
    ///
    /// Returns how many rows were **written**: a memory whose content already exists
    /// comes back as a duplicate and is not counted. The caller knows the difference
    /// between "a hundred thousand memories" and "a hundred thousand attempts".
    ///
    /// One transaction because this is a bulk write by definition — the corpus generator
    /// and, later, `import` — and because a per-row transaction would make the harness
    /// measure the ruler instead of the code. The insert itself is [`insert_one`], so a
    /// generated memory is validated, deduped and gated exactly like a typed one.
    ///
    /// # Errors
    ///
    /// Returns whatever [`insert_one`] returns for the first row it refuses.
    pub(crate) fn insert_many(&self, news: &[NewMemory]) -> Result<usize> {
        let mut resolved: Option<(String, ReviewMode)> = None;
        let transaction = self.conn().unchecked_transaction()?;
        let mut written = 0;
        for new in news {
            // Resolving a workspace is a query, and a bulk write has one workspace in it
            // almost every time — so remember the last answer and ask again only when the
            // question changes.
            if resolved
                .as_ref()
                .is_none_or(|(id, _)| id != &new.workspace_id)
            {
                let workspace = self.resolve_workspace(&new.workspace_id)?;
                resolved = Some((workspace.id, workspace.review_mode));
            }
            let (workspace_id, review_mode) = resolved
                .as_ref()
                .expect("the workspace was resolved immediately above");
            if !insert_one(&transaction, workspace_id, *review_mode, new)?.duplicate {
                written += 1;
            }
        }
        transaction.commit()?;
        Ok(written)
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

        // Checked on the row that would result rather than on the patch: a body that was
        // already over the limit — imported by an older build, or written before the limit
        // existed — must not become un-editable, and `--title` alone must not slip past a
        // body check nobody ran.
        check_text_limits(
            patch.title.as_deref().unwrap_or(&existing.title),
            patch.excerpt.as_deref().unwrap_or(&existing.excerpt),
            patch.body.as_deref().unwrap_or(&existing.body),
        )?;

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

        // A vector is a function of the text it was made from. An update that touched
        // title, body or excerpt without re-embedding leaves a row that semantic search
        // can only find by the words it used to have — worse than a row with no vector,
        // because nothing reports it and the stale one still wins.
        let text_moved = patch.title.is_some() || patch.body.is_some() || patch.excerpt.is_some();
        if text_moved && changed > 0 {
            if let Err(error) = self.reembed_ids(std::slice::from_ref(&existing.id)) {
                warnings.push(format!("updated, but not re-embedded: {error}"));
            }
        }

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
            "SELECT kind, status, pinned, count(*) FROM memories
              WHERE 1 = 1{scope_clause}{kinds_clause}
              GROUP BY kind, status, pinned"
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

    /// One number out of one statement. Test-facing rather than public API: every
    /// caller outside the crate has a reason-bearing method instead, and a raw counting
    /// hole is how a caller ends up writing a query that reads every body in the corpus.
    pub(crate) fn count(&self, sql: &str, bindings: &[Sql]) -> Result<i64> {
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

    /// Embed these ids, skipping the ones that already have a vector for this model.
    ///
    /// The one write path for vectors, shared by `add`, `update`, `review approve` and
    /// `reindex --embed`, so that four callers cannot disagree about the text rule, the
    /// model key, or the batching.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when the embedder is configured and fails, and
    /// [`Error::Database`] when a row named here cannot be read.
    pub fn embed_ids(&self, ids: &[String]) -> Result<Vec<EmbedState>> {
        self.embed_ids_inner(ids, false)
    }

    /// Embed these ids, replacing any vector they already have.
    ///
    /// For the one caller whose text *moved*: an `update` that rewrote a title or a body
    /// leaves a vector that describes text the memory no longer has, and a stale vector is
    /// worse than a missing one — nothing reports it, and a query for the old words keeps
    /// finding the row.
    ///
    /// # Errors
    ///
    /// As [`Database::embed_ids`].
    pub fn reembed_ids(&self, ids: &[String]) -> Result<Vec<EmbedState>> {
        self.embed_ids_inner(ids, true)
    }

    /// The shared body of [`Database::embed_ids`] and [`Database::reembed_ids`].
    fn embed_ids_inner(&self, ids: &[String], force: bool) -> Result<Vec<EmbedState>> {
        let Some(embedder) = self.embedder() else {
            return Ok(ids.iter().map(|_| EmbedState::NoEmbedder).collect());
        };
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        // One statement for the whole list, not one per id: a reindex page is five hundred
        // ids, and five hundred round trips through SQLite is a scan of the corpus's index
        // five hundred times over. The `IN` list is bounded by the batch size the caller
        // chose.
        let mut states = vec![EmbedState::Present; ids.len()];
        let present: Vec<String> = if force {
            Vec::new()
        } else {
            self.embedded_subset(ids, embedder.model())?
        };
        let mut missing: Vec<(usize, String)> = Vec::new();
        for (index, id) in ids.iter().enumerate() {
            if !present.iter().any(|already| already == id) {
                missing.push((index, id.clone()));
            }
        }
        if missing.is_empty() {
            return Ok(states);
        }

        let mut texts = Vec::with_capacity(missing.len());
        for (_, id) in &missing {
            let memory = self.get(id)?;
            texts.push(crate::embed::memory_text(
                &memory.title,
                &memory.body,
                &memory.excerpt,
            ));
        }

        let vectors = embedder.embed_documents(&texts)?;
        debug_assert_eq!(vectors.len(), missing.len());

        let now = crate::db::now(self.conn())?;
        let transaction = self.conn().unchecked_transaction()?;
        {
            // `INSERT OR REPLACE` rather than `INSERT`: re-embedding a row that already
            // has a vector under this model (a model switch back, a forced reindex) is
            // the normal case, and the primary key is `(mem_id, model)`.
            let mut statement = transaction.prepare(
                "INSERT OR REPLACE INTO memory_embeddings (mem_id, model, dim, vec, created_at) \
                 VALUES ((SELECT mem_id FROM memories WHERE id = ?1), ?2, ?3, ?4, ?5)",
            )?;
            for ((index, _), vector) in missing.iter().zip(vectors) {
                statement.execute(params![
                    ids[*index],
                    embedder.model(),
                    i64::try_from(vector.len()).unwrap_or(i64::MAX),
                    crate::vector::encode(&vector),
                    now,
                ])?;
                states[*index] = EmbedState::Written;
            }
        }
        transaction.commit()?;
        Ok(states)
    }

    /// Embed every memory in scope that has no vector for this model.
    ///
    /// Paged by primary key, one page at a time, each page committed before the next is
    /// read: an interrupted run keeps the pages it finished, and running it again resumes
    /// at the first row that still has no vector — because "which rows are missing" is
    /// the query, and it is the only cursor this needs.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when there is no embedder (a reindex with nothing to
    /// embed with is a usage error rather than a no-op), and [`Error::Database`] when a
    /// page cannot be read.
    pub fn reindex_embeddings(
        &self,
        scope: &Scope,
        limit: Option<u32>,
        dry_run: bool,
    ) -> Result<EmbedReport> {
        let embedder = self.embedder().ok_or_else(|| {
            Error::Config(
                "`reindex --embed` needs an embedder, and none is configured: add an \
                 [embedder] block to the config file. Without one there is nothing to \
                 write vectors with, and a reindex that reported success without writing \
                 any would be worse than this message."
                    .to_owned(),
            )
        })?;
        let mut report = EmbedReport::empty(embedder.model());
        report.dry_run = dry_run;

        let page = limit.unwrap_or(500).max(1);
        let mut cursor: i64 = 0;
        loop {
            let ids = self.unembedded_ids(embedder.model(), scope, cursor, page)?;
            let Some(last) = ids.last().map(|(mem_id, _)| *mem_id) else {
                break;
            };
            report.requested += ids.len();
            cursor = last;
            if dry_run {
                // Counted, not embedded: the point of a dry run is to answer "how much
                // work is this" without spending a model's time or a user's tokens.
                report.embedded += ids.len();
                if ids.len() < page as usize {
                    break;
                }
                continue;
            }
            let names: Vec<String> = ids.iter().map(|(_, id)| id.clone()).collect();
            for (_, id) in &ids {
                let memory = self.get(id)?;
                let text = crate::embed::memory_text(&memory.title, &memory.body, &memory.excerpt);
                if crate::embed::Embedder::truncates(&text) {
                    report.truncated += 1;
                }
            }
            match self.embed_ids(&names) {
                Ok(states) => {
                    report.embedded += states
                        .iter()
                        .filter(|state| **state == EmbedState::Written)
                        .count();
                    report.present += states
                        .iter()
                        .filter(|state| **state == EmbedState::Present)
                        .count();
                }
                Err(error) => {
                    // Reported and skipped: one page a model refused is not a reason to
                    // abandon the pages after it, and the count at the end says what
                    // actually landed rather than what was attempted.
                    report.errors.push(error.to_string());
                }
            }
            if ids.len() < page as usize {
                break;
            }
        }
        Ok(report)
    }

    /// Which of `ids` already have a vector for `model`, in one statement.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the query fails.
    fn embedded_subset(&self, ids: &[String], model: &str) -> Result<Vec<String>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; ids.len()].join(", ");
        let sql = format!(
            "SELECT memories.id \
               FROM memories \
               JOIN memory_embeddings ON memory_embeddings.mem_id = memories.mem_id \
              WHERE memory_embeddings.model = ?1 AND memories.id IN ({placeholders})"
        );
        let mut values: Vec<Sql> = vec![Sql::Text(model.to_owned())];
        values.extend(ids.iter().map(|id| Sql::Text(id.clone())));
        let mut statement = self.conn().prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(values.iter()), |row| {
            row.get::<_, String>(0)
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Whether this memory already has a vector for `model`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the query fails.
    pub fn has_embedding(&self, id: &str, model: &str) -> Result<bool> {
        let count: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM memory_embeddings \
               JOIN memories ON memories.mem_id = memory_embeddings.mem_id \
              WHERE memories.id = ?1 AND memory_embeddings.model = ?2",
            params![id, model],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Active memories after `cursor` that have no vector for `model`.
    ///
    /// A page of `(mem_id, id)`: `mem_id` because that is what the cursor walks, `id`
    /// because that is what a caller names. `NOT EXISTS` rather than a `LEFT JOIN ... IS
    /// NULL`, which is the same answer for a fraction of the planning and reads as what it
    /// means.
    fn unembedded_ids(
        &self,
        model: &str,
        scope: &Scope,
        cursor: i64,
        limit: u32,
    ) -> Result<Vec<(i64, String)>> {
        let mut sql = String::from(
            "SELECT memories.mem_id, memories.id \
               FROM memories \
              WHERE memories.status = 'active' \
                AND memories.mem_id > ? \
                AND NOT EXISTS ( \
                    SELECT 1 FROM memory_embeddings \
                     WHERE memory_embeddings.mem_id = memories.mem_id \
                       AND memory_embeddings.model = ?)",
        );
        let mut values: Vec<Sql> = vec![Sql::Integer(cursor), Sql::Text(model.to_owned())];
        let (scope_clause, scope_values) = scope_clause(Some("memories."), scope);
        sql.push_str(&scope_clause);
        values.extend(scope_values);
        sql.push_str(" ORDER BY memories.mem_id LIMIT ?");
        values.push(Sql::Integer(i64::from(limit)));

        let mut statement = self.conn().prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(values.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// One page of an export, oldest id first.
    ///
    /// Keyset pagination on `id`, not `LIMIT/OFFSET`: the ULID sorts chronologically, so
    /// `id > cursor` is both the order and the cursor, and page five thousand costs what
    /// page one costs. An export of a 100 000-memory brain is 100 000 rows, and the
    /// difference between keyset and offset on that is the difference between streaming
    /// and quadratic.
    ///
    /// The whole row comes back, `pending_derived_from` included, so the caller does not
    /// have to go back for it per draft — one query per page is the budget, and a
    /// per-row lookup for the sake of a column nobody usually has would break it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if a page cannot be read.
    pub fn export_page(
        &self,
        scope: &Scope,
        status: Option<Status>,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<ExportRecord>> {
        // `fmt::Write` so a clause is appended with `write!` rather than a throwaway
        // `format!` per filter — the same choice `list` makes, for the same reason.
        use std::fmt::Write as _;

        let mut sql =
            format!("SELECT {MEMORY_COLUMNS}, pending_derived_from FROM memories WHERE 1 = 1");
        let mut values: Vec<Sql> = Vec::new();
        let (clause, scope_values) = scope_clause(None, scope);
        sql.push_str(&clause);
        values.extend(scope_values);
        if let Some(status) = status {
            // The `as_str()` enum literal is the one interpolation this file allows: it
            // cannot be user input, and a bound parameter here would stop SQLite using
            // the status half of `idx_mem_ws_status_created` for the filter.
            let _ = write!(sql, " AND status = '{}'", status.as_str());
        }
        if let Some(cursor) = after {
            sql.push_str(" AND id > ?");
            values.push(Sql::Text(cursor.to_owned()));
        }
        sql.push_str(" ORDER BY id ASC LIMIT ?");
        values.push(Sql::Integer(i64::from(limit)));

        let mut statement = self.conn().prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(values.iter()), |row| {
            let memory = row_to_memory(row)?;
            let pending: Option<String> = row.get(MEMORY_COLUMN_COUNT)?;
            Ok(ExportRecord::from_memory(&memory, pending.as_deref()))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Write a page of imported rows, in one transaction.
    ///
    /// Three outcomes per row, in this order: the id is already here, the *content* is
    /// already here under another id, or the row is written. The order is what makes
    /// re-importing an export a no-op — a second `import` of the same file finds every id
    /// present and writes nothing — and the content check is what keeps two exports of one
    /// brain (one from before a memory was re-saved under a new id) from producing a
    /// duplicate.
    ///
    /// `dry_run` runs the whole thing in a transaction and rolls it back, so the counts a
    /// dry run reports are the counts the real run would produce, including duplicates
    /// *inside* the file being imported. Nothing else can tell that story without a second
    /// implementation of the same rules, and a second implementation is where the two
    /// would disagree.
    ///
    /// A workspace the page mentions is created if it is missing. An export carries the
    /// workspace ids it was written from, and refusing to restore one because the target
    /// has not heard of it would make the format a fair-weather one.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if a statement fails, and [`Error::Invalid`] if an id
    /// cannot be generated. Rows are validated before they arrive, so a data problem is a
    /// caller bug rather than something this reports per row.
    pub fn import_batch(&self, rows: &[ImportRow], dry_run: bool) -> Result<ImportBatch> {
        let transaction = self.conn().unchecked_transaction()?;
        let mut batch = ImportBatch {
            outcomes: Vec::with_capacity(rows.len()),
            created_workspaces: Vec::new(),
            committed: false,
        };

        for row in rows {
            let created_here = ensure_workspace_for(&transaction, &row.workspace_id)?;
            if created_here && !batch.created_workspaces.contains(&row.workspace_id) {
                batch.created_workspaces.push(row.workspace_id.clone());
            }

            let id = match &row.id {
                Some(id) => id.clone(),
                None => id::new_memory_id()?,
            };
            let state = if memory_exists(&transaction, &id)? {
                ImportState::Exists
            } else if find_duplicate_near(
                &transaction,
                &row.workspace_id,
                &row.content_hash,
                &row.as_new(),
            )
            .is_some()
            {
                ImportState::Duplicate
            } else {
                insert_imported(&transaction, &id, row)?;
                ImportState::Written
            };
            batch.outcomes.push(ImportOutcome { id, state });
        }

        if dry_run {
            // Rolled back rather than never written: the counts above had to be computed
            // by doing the work, and undoing it is the only way to report them without
            // keeping a second copy of the rules.
            transaction.rollback()?;
        } else {
            transaction.commit()?;
            batch.committed = true;
        }
        Ok(batch)
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

/// Save one memory, against whatever transaction the caller is holding.
///
/// The single insert path. [`Database::add`] wraps it in a transaction of its own, the
/// corpus generator wraps a hundred thousand of them in one, and any future bulk import
/// does the same — so validation, dedupe, the write gate and the activation-time
/// provenance all happen identically however the memory got here. A second insert path
/// is how a corpus stops being made of the same memories a user would make.
///
/// # Errors
///
/// Returns [`Error::Invalid`] for an empty title, a confidence or importance outside
/// `0.0..=1.0`, or more than [`MAX_TAGS`] tags. Returns [`Error::Database`] if the insert
/// fails.
/// Insert one imported row, with the id and the lifecycle it came with.
///
/// Separate from [`insert_one`] rather than a flag on it because they answer different
/// questions. `insert_one` is a *write*: it generates an id, runs the write gate, decides
/// the status, and refuses a duplicate. An import is a *restore*: the id, the status, the
/// counts and the timestamps are the file's, and the job is to reproduce them exactly —
/// a restored brain whose drafts had activated, or whose `created_at` was today, would
/// have lost the thing the file was for.
///
/// What it does not skip is validation: tags are normalised, the envelope is checked, and
/// the text limits apply, all before the row gets here.
fn insert_imported(
    transaction: &rusqlite::Transaction<'_>,
    id: &str,
    row: &ImportRow,
) -> Result<()> {
    transaction.execute(
        INSERT_IMPORTED_SQL,
        params![
            id,
            row.workspace_id,
            row.kind,
            row.title,
            row.excerpt,
            row.body,
            encode_tags(&row.tags),
            metadata::tags_text(&row.tags),
            row.memory_metadata.to_string(),
            row.source,
            row.source_url,
            row.confidence,
            row.importance,
            row.strength,
            row.corroboration_count,
            row.retrieval_count,
            row.last_retrieved_at,
            row.feedback_score,
            row.feedback_count,
            row.pinned,
            row.status,
            row.expires_at,
            row.content_hash,
            row.pending_derived_from,
            row.created_at,
            row.updated_at,
        ],
    )?;
    Ok(())
}

/// Whether this id is already in the table.
///
/// Checked before the insert rather than caught after it: a `UNIQUE` violation aborts the
/// transaction, and one line that duplicates a row should cost that line and not the
/// ninety-nine beside it.
fn memory_exists(transaction: &rusqlite::Transaction<'_>, id: &str) -> Result<bool> {
    let count: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM memories WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Create a workspace an import names, and say whether this call is what created it.
///
/// The name and the id are the same string: an export carries ids, and inventing a display
/// name for one — title-casing it, splitting the hyphens — would be this module guessing at
/// something a user typed once. `init --workspace` is where names come from.
fn ensure_workspace_for(
    transaction: &rusqlite::Transaction<'_>,
    workspace_id: &str,
) -> Result<bool> {
    let count: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM workspaces WHERE id = ?1",
        params![workspace_id],
        |row| row.get(0),
    )?;
    if count > 0 {
        return Ok(false);
    }
    crate::db::ensure_workspace_on(
        transaction,
        workspace_id,
        workspace_id,
        ReviewMode::Autonomous,
    )?;
    Ok(true)
}

fn insert_one(
    transaction: &rusqlite::Transaction<'_>,
    workspace_id: &str,
    review_mode: ReviewMode,
    new: &NewMemory,
) -> Result<AddOutcome> {
    let title = new.title.trim();
    if title.is_empty() {
        return Err(Error::Invalid(
            "a memory needs a title; pass --title, or --from-file to take one from a file"
                .to_owned(),
        ));
    }
    check_unit_range("--confidence", new.confidence)?;
    check_unit_range("--importance", Some(new.importance))?;
    check_text_limits(title, &new.excerpt, &new.body)?;
    if new.source.trim().is_empty() {
        return Err(Error::Invalid(
            "--source needs a value; omit it to record a `user_saved` memory".to_owned(),
        ));
    }

    let mut warnings = Vec::new();
    let tags = metadata::normalize_tags(&new.tags).map_err(Error::Invalid)?;
    let validated = metadata::validate(new.kind, new.memory_metadata.as_ref());
    warnings.extend(validated.warning);

    let content_hash = hash::content_hash(new.kind, title, &new.body);

    if !new.force {
        if let Some(existing) = find_duplicate_near(transaction, workspace_id, &content_hash, new) {
            return Ok(AddOutcome {
                id: existing.id,
                workspace_id: existing.workspace_id,
                status: existing.status,
                duplicate: true,
                warnings,
            });
        }
    }

    let status = gate::decide(&new.source, new.confidence, review_mode).status();
    let new_id = id::new_memory_id()?;
    let pending_provenance = if status == Status::Active || new.derived_from.is_empty() {
        None
    } else {
        serde_json::to_string(&new.derived_from).ok()
    };

    transaction.execute(
        INSERT_MEMORY_SQL,
        params![
            new_id,
            workspace_id,
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
            // A draft's provenance has nowhere to go yet: an edge is readable, and
            // `relate` on the parent would show the draft's title — the inbox leaking
            // through the graph. So the ids wait on the row and `approve` wires them.
            pending_provenance,
        ],
    )?;

    // Provenance is wired at activation, never for a draft: a `derived_from` edge from a
    // memory nobody has approved would point at something retrieval cannot return.
    if status == Status::Active && !new.derived_from.is_empty() {
        warnings.extend(link_derived_from(transaction, &new_id, &new.derived_from)?);
    }

    Ok(AddOutcome {
        id: new_id,
        workspace_id: workspace_id.to_owned(),
        status,
        duplicate: false,
        warnings,
    })
}

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
pub(crate) fn link_derived_from(
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
        feedback_count: row.get("feedback_count")?,
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

    // -----------------------------------------------------------------------
    // Export paging and the import batch (WP6)
    // -----------------------------------------------------------------------

    #[test]
    fn a_page_is_the_size_it_was_asked_for_and_the_cursor_continues() {
        // The property the export's memory bound rests on. Asserted at the store level
        // because from the command line a page is invisible: the walk looks the same
        // whether it pages or reads everything, until the brain is too big to read.
        let db = testing::db();
        for index in 0..7 {
            add_semantic(&db, &format!("Note {index}"), "body");
        }

        let scope = Scope::One("default".to_owned());
        let first = db.export_page(&scope, None, None, 3).unwrap();
        assert_eq!(first.len(), 3);
        let mut seen: Vec<String> = first.iter().filter_map(|row| row.id.clone()).collect();

        let second = db
            .export_page(&scope, None, seen.last().map(String::as_str), 3)
            .unwrap();
        assert_eq!(second.len(), 3);
        seen.extend(second.iter().filter_map(|row| row.id.clone()));

        let third = db
            .export_page(&scope, None, seen.last().map(String::as_str), 3)
            .unwrap();
        assert_eq!(
            third.len(),
            1,
            "the last page is short, and that ends the walk"
        );
        seen.extend(third.iter().filter_map(|row| row.id.clone()));

        let mut sorted = seen.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 7, "every row came back exactly once");
        assert_eq!(seen, sorted, "the walk is in id order, which is time order");
    }

    #[test]
    fn an_export_page_can_be_asked_for_one_status_or_for_every_one() {
        let db = testing::db();
        add_semantic(&db, "Active one", "body");
        let archived = add_semantic(&db, "Archived one", "body");
        assert_eq!(
            db.archive(std::slice::from_ref(&archived)).unwrap().moved,
            vec![archived.clone()]
        );

        let scope = Scope::One("default".to_owned());
        let active = db
            .export_page(&scope, Some(Status::Active), None, 10)
            .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].title, "Active one");

        let every = db.export_page(&scope, None, None, 10).unwrap();
        assert_eq!(every.len(), 2, "no filter means every status");
    }

    #[test]
    fn an_export_page_carries_a_drafts_staged_provenance() {
        // The one column that is not part of the memory's own shape: a draft's
        // `--derived-from` waits on the row until approval, and an export that dropped it
        // would lose the provenance a restore is for.
        let db = testing::db();
        let parent = add_semantic(&db, "Parent", "body");
        db.add(
            &NewMemory::new(MemoryKind::Semantic, "A draft")
                .workspace("default")
                .body("body")
                .source("agent_reflection")
                .confidence(0.1)
                .derived_from(vec![parent.clone()]),
        )
        .unwrap();

        let page = db
            .export_page(&Scope::One("default".to_owned()), None, None, 10)
            .unwrap();
        let draft = page
            .iter()
            .find(|row| row.title == "A draft")
            .expect("the draft is in the export when the status filter allows it");
        assert_eq!(draft.derived_from, vec![parent]);
        assert_eq!(draft.status, Some(Status::PendingApproval));
    }

    #[test]
    fn an_import_writes_restores_and_declines_in_that_order() {
        let db = testing::db();
        let existing = add_semantic(&db, "Already here", "body");

        let rows = vec![
            row_of("k_01M56XYP4ATEW6PQP8MJ6Y2JQ7", "Brand new", "body one"),
            row_of(&existing, "Renamed but same id", "body two"),
            row_of("k_01M56XYP4ATEW6PQP8MJ6Y2JQ8", "Already here", "body"),
        ];
        let batch = db.import_batch(&rows, false).unwrap();
        assert!(batch.committed);
        assert_eq!(batch.written(), 1);
        assert_eq!(
            batch
                .outcomes
                .iter()
                .filter(|outcome| outcome.state == ImportState::Exists)
                .count(),
            1,
            "the id is already here: {batch:?}"
        );
        assert_eq!(
            batch
                .outcomes
                .iter()
                .filter(|outcome| outcome.state == ImportState::Duplicate)
                .count(),
            1,
            "the content is already here under another id: {batch:?}"
        );

        // The restored row keeps the file's id and content.
        let restored = db.get("k_01M56XYP4ATEW6PQP8MJ6Y2JQ7").unwrap();
        assert_eq!(restored.body, "body one");
        let untouched = db.get(&existing).unwrap();
        assert_eq!(
            untouched.title, "Already here",
            "a line that names an existing id changes nothing"
        );
    }

    #[test]
    fn a_dry_run_batch_reports_what_it_would_do_and_writes_nothing() {
        let db = testing::db();
        let rows = vec![
            row_of("k_01M56XYP4ATEW6PQP8MJ6Y2JQ7", "One", "body"),
            row_of("k_01M56XYP4ATEW6PQP8MJ6Y2JQ7", "One", "body"),
        ];
        let batch = db.import_batch(&rows, true).unwrap();
        assert!(!batch.committed);
        assert_eq!(batch.written(), 1, "the second line duplicates the first");
        assert_eq!(batch.skipped(), 1);
        assert!(
            db.get("k_01M56XYP4ATEW6PQP8MJ6Y2JQ7").is_err(),
            "a dry run must not write, not even the rows it counted"
        );
    }

    #[test]
    fn an_import_creates_the_workspace_the_file_names() {
        let db = testing::db();
        let mut row = row_of("k_01M56XYP4ATEW6PQP8MJ6Y2JQ7", "From elsewhere", "body");
        row.workspace_id = "work-stuff".to_owned();
        let batch = db.import_batch(&[row], false).unwrap();
        assert_eq!(batch.created_workspaces, vec!["work-stuff"]);
        let stored = db.get("k_01M56XYP4ATEW6PQP8MJ6Y2JQ7").unwrap();
        assert_eq!(stored.workspace_id, "work-stuff");
    }

    /// A validated import row, built the way the importer builds one.
    fn row_of(id: &str, title: &str, body: &str) -> ImportRow {
        let mut record: ExportRecord =
            serde_json::from_str(r#"{"kind":"semantic","title":"placeholder"}"#)
                .expect("the placeholder record");
        record.id = Some(id.to_owned());
        record.title = title.to_owned();
        record.body = body.to_owned();
        record.into_row(None).expect("a valid row")
    }

    #[test]
    fn a_write_over_a_text_limit_is_refused_and_the_row_below_it_is_not() {
        // One rule for every door: `add`, `update` and `import` share this check, so a
        // memory the file may hold and the CLI may not is not a state this engine has.
        let db = testing::db();
        let long_title = "t".repeat(MAX_TITLE + 1);
        let error = db
            .add(&NewMemory::new(MemoryKind::Semantic, &long_title).body("body"))
            .unwrap_err()
            .to_string();
        assert!(error.contains(&MAX_TITLE.to_string()), "{error}");

        let huge = "b".repeat(MAX_BODY_BYTES + 1);
        let error = db
            .add(&NewMemory::new(MemoryKind::Semantic, "Huge").body(huge))
            .unwrap_err()
            .to_string();
        assert!(error.contains(&MAX_BODY_BYTES.to_string()), "{error}");

        let id = add_semantic(&db, "At the limit", "body");
        let error = db
            .update(
                &id,
                &MemoryPatch {
                    title: Some("x".repeat(MAX_TITLE + 1)),
                    ..MemoryPatch::default()
                },
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains(&MAX_TITLE.to_string()), "{error}");
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
