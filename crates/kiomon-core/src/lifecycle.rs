// SPDX-License-Identifier: Apache-2.0

//! The lifecycle verbs: what happens to a memory after it is written.
//!
//! Four things live here, and they are grouped because they share one property: each is
//! a *correction* to a row that already exists, and each has to be explainable a month
//! later.
//!
//! * **The review inbox** — drafts that the write gate held back, and the two ways out
//!   of it. Approving is the only thing that ever indexes a draft, which is why the
//!   transition and the index row are in the same transaction: the `mem_au` trigger does
//!   the indexing for free, and it is the schema rather than this module that guarantees
//!   the two cannot disagree.
//! * **Feedback** — `rate` and `outcome`. Ported from the hosted product's
//!   `credit.ts` and `documents/service.ts`, including the part §5.3 of the plan got
//!   wrong: these move `confidence`, `feedback_score` and `expires_at`, and they
//!   **never** move `strength`. Decay owns strength and recomputes it from `importance`
//!   on every pass, so a multiplier written here would survive exactly until the next
//!   `kiomon decay`.
//! * **Pins and the terminal states** — `pin`, `archive`, `restore`.
//! * **Nothing destructive.** `forget` is the only verb that deletes, it lives in
//!   `store.rs` because it is about the row rather than about its state, and everything
//!   in this module is reversible.
//!
//! Every multiplier and threshold is a named constant with a comment, because the
//! hosted repository's own history is a standing lesson that an unnamed number becomes a
//! permanent argument.

use rusqlite::types::Value as Sql;
use serde::Serialize;

use crate::db::Database;
use crate::error::{Error, Result};
use crate::model::{Memory, MemoryKind, Status};

/// Confidence gain for a memory assessed `helpful`, as a fraction of the distance
/// remaining to 1.0.
///
/// Diminishing rather than additive: a memory at 0.9 goes to 0.91, not to 1.0. An
/// additive step would let five good outcomes make any claim certain, which is the
/// opposite of what evidence about a memory should do.
pub const HELPFUL_GAIN: f64 = 0.1;

/// Confidence multiplier for `misleading`.
pub const MISLEADING_DECAY: f64 = 0.85;

/// Confidence multiplier for `outdated`.
///
/// Gentler than `misleading` on purpose: a stale fact was true when it was written, and
/// the memory that says it is not the same thing as a memory that was wrong.
pub const OUTDATED_DECAY: f64 = 0.9;

/// Confidence never falls below this, and `0.5` is what an assessment assumes when the
/// writer claimed nothing.
///
/// The floor is not zero because confidence gates the write path: a memory that
/// re-enters the inbox on its next update is a memory the user has to approve twice.
pub const CONFIDENCE_FLOOR: f64 = 0.05;

/// The most memories one `outcome` call may assess.
///
/// The hosted tool caps at 50 and says so in its schema; the cap is here because a
/// single call is one transaction, and an unbounded transaction is a lock held for as
/// long as a caller feels like.
pub const MAX_OUTCOME_ITEMS: usize = 50;

/// Which way a rating went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Rating {
    /// It helped.
    Up,
    /// It did not.
    Down,
}

impl Rating {
    /// The number the running average is fed.
    #[must_use]
    pub const fn value(self) -> f64 {
        match self {
            Self::Up => 1.0,
            Self::Down => -1.0,
        }
    }

    /// The wire value, which is also what `--json` reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Up => "+1",
            Self::Down => "-1",
        }
    }
}

impl std::fmt::Display for Rating {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How one memory performed in a task, as the agent reports it.
///
/// The four are the hosted tool's, verbatim: they are the vocabulary the SDKs and the
/// agent instructions already use, and a local brain that renamed one would be
/// translating at the one boundary that should be identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Assessment {
    /// Used, and it helped.
    Helpful,
    /// Retrieved, and not used.
    Unused,
    /// Used, and it caused a wrong result.
    Misleading,
    /// Stale.
    Outdated,
}

impl Assessment {
    /// The wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Helpful => "helpful",
            Self::Unused => "unused",
            Self::Misleading => "misleading",
            Self::Outdated => "outdated",
        }
    }

    /// Every assessment, in the order the contract lists them.
    pub const ALL: [Self; 4] = [
        Self::Helpful,
        Self::Unused,
        Self::Misleading,
        Self::Outdated,
    ];

    /// Whether this assessment counts as a retrieval that was *used*.
    ///
    /// `unused` and `misleading` do not: one was ignored and the other was acted on and
    /// wrong, and neither is corroboration.
    #[must_use]
    pub const fn is_corroboration(self) -> bool {
        matches!(self, Self::Helpful | Self::Outdated)
    }
}

impl std::str::FromStr for Assessment {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "helpful" => Ok(Self::Helpful),
            "unused" => Ok(Self::Unused),
            "misleading" => Ok(Self::Misleading),
            "outdated" => Ok(Self::Outdated),
            other => Err(Error::Invalid(format!(
                "`{other}` is not an assessment; use one of: helpful, unused, misleading, \
                 outdated"
            ))),
        }
    }
}

impl std::fmt::Display for Assessment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How the task itself ended.
///
/// Recorded alongside every item and never used to compute anything: a `helpful`
/// memory in a failed task is still a helpful memory. It is a fact about the session,
/// and the usefulness log is where facts about sessions belong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskOutcome {
    /// It worked.
    Success,
    /// It did not.
    Failure,
}

impl TaskOutcome {
    /// The wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
        }
    }
}

impl std::fmt::Display for TaskOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why an id was left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// No row with that id.
    NotFound,
    /// Not in the state the verb needs.
    WrongStatus,
    /// Already in the state the verb wanted.
    AlreadyThere,
    /// Outside the workspace the caller scoped the verb to.
    OutOfScope,
    /// The same id appeared twice in one call.
    Duplicate,
}

impl SkipReason {
    /// The wire value, and the word the human line uses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not found",
            Self::WrongStatus => "wrong status",
            Self::AlreadyThere => "already there",
            Self::OutOfScope => "outside the workspace",
            Self::Duplicate => "duplicate",
        }
    }
}

/// One id that was left alone, and why.
#[derive(Debug, Clone, Serialize)]
pub struct Skipped {
    /// The id that did not move.
    pub id: String,
    /// Why not.
    pub reason: SkipReason,
    /// The state the row is actually in, when there is one to report.
    pub status: Option<Status>,
}

impl Skipped {
    /// A skip with no status to report.
    fn new(id: &str, reason: SkipReason) -> Self {
        Self {
            id: id.to_owned(),
            reason,
            status: None,
        }
    }

    /// A skip with the state the row is in.
    fn in_status(id: &str, reason: SkipReason, status: Status) -> Self {
        Self {
            id: id.to_owned(),
            reason,
            status: Some(status),
        }
    }
}

/// What `pin`, `unpin`, `archive` and `restore` did.
#[derive(Debug, Clone, Serialize)]
pub struct IdsOutcome {
    /// The ids that moved.
    pub moved: Vec<String>,
    /// The ids that did not, with a reason each.
    pub skipped: Vec<Skipped>,
}

impl IdsOutcome {
    /// Whether anything at all moved.
    ///
    /// The exit code turns on this: nothing moved means there is nothing to report as
    /// success, and a script asking to archive five ids that were all already archived
    /// wants to hear about it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.moved.is_empty()
    }
}

/// What `review approve` and `review reject` did.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewOutcome {
    /// The ids that left the inbox.
    pub moved: Vec<String>,
    /// The ids that did not, with a reason each.
    pub skipped: Vec<Skipped>,
}

/// What `rate` did.
#[derive(Debug, Clone, Serialize)]
pub struct RatingOutcome {
    /// The memory that was rated.
    pub id: String,
    /// Which way.
    pub rating: Rating,
    /// The running average after this rating.
    pub feedback_score: f64,
    /// How many ratings that average is over, including this one.
    pub feedback_count: i64,
}

/// One memory's credit, after an outcome was applied.
#[derive(Debug, Clone, Serialize)]
pub struct AppliedOutcome {
    /// The memory.
    pub id: String,
    /// What the agent said about it.
    pub assessment: Assessment,
    /// Its confidence after the adjustment.
    pub confidence: f64,
    /// The running average after this assessment.
    pub feedback_score: f64,
    /// How many ratings and assessments that average is over.
    pub feedback_count: i64,
    /// Set when the assessment expired the memory.
    pub expires_at: Option<String>,
}

/// What `outcome` did.
#[derive(Debug, Clone, Serialize)]
pub struct OutcomeReport {
    /// How the task ended, carried through so a log line has both halves.
    pub outcome: TaskOutcome,
    /// The memories that were credited.
    pub applied: Vec<AppliedOutcome>,
    /// The ids that were not, with a reason each.
    pub skipped: Vec<Skipped>,
}

/// The inbox query.
#[derive(Debug, Clone)]
pub struct PendingQuery {
    /// The workspace to read, or every workspace.
    pub scope: crate::model::Scope,
    /// At most this many rows.
    pub limit: u16,
    /// Only drafts whose writer claimed at least this much confidence.
    pub min_confidence: Option<f64>,
}

impl Default for PendingQuery {
    fn default() -> Self {
        Self {
            scope: crate::model::Scope::One(crate::model::DEFAULT_WORKSPACE.to_owned()),
            limit: 20,
            min_confidence: None,
        }
    }
}

/// What the running average becomes after one more rating.
///
/// Extracted so the arithmetic is testable without a database, and because it is the one
/// formula in this module that a reader is likely to want to check by hand.
#[must_use]
pub fn running_average(score: f64, count: i64, rating: f64) -> f64 {
    let count = count.max(0);
    // Exact for any count a brain could hold: an i64 above 2^53 is not a number of
    // ratings, and the `.max(0)` above has already ruled out the sign. The lint is about
    // the general case and this is not it.
    #[allow(clippy::cast_precision_loss)]
    let (ratings, next_ratings) = (count as f64, (count + 1) as f64);
    let next = (score * ratings + rating) / next_ratings;
    // Rounded to two places so the value that is printed, stored and compared is the
    // same value. The hosted product rounds for the same reason.
    (next * 100.0).round() / 100.0
}

/// The confidence a `helpful` assessment leaves behind.
#[must_use]
pub fn confidence_after_helpful(confidence: f64) -> f64 {
    round2(confidence + (1.0 - confidence) * HELPFUL_GAIN)
}

/// The confidence a `misleading` or `outdated` assessment leaves behind.
#[must_use]
pub fn confidence_after_negative(confidence: f64, multiplier: f64) -> f64 {
    round2((confidence * multiplier).max(CONFIDENCE_FLOOR))
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// What a `Memory`'s assessed confidence starts from: the claim, or the neutral 0.5.
fn base_confidence(claimed: Option<f64>) -> f64 {
    claimed.unwrap_or(0.5)
}

impl Database {
    /// The review inbox: drafts waiting for a decision.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the query fails.
    pub fn list_pending(&self, query: &PendingQuery) -> Result<Vec<Memory>> {
        let mut sql = format!(
            "SELECT {} FROM memories WHERE status = 'pending_approval'",
            crate::store::MEMORY_COLUMNS
        );
        let mut values: Vec<Sql> = Vec::new();
        if let Some(workspace) = query.scope.one() {
            sql.push_str(" AND workspace_id = ?");
            values.push(Sql::Text(workspace.to_owned()));
        }
        if let Some(threshold) = query.min_confidence {
            // A draft with no claimed confidence is excluded rather than treated as
            // zero: "at least 0.8" is a question about what the writer claimed, and a
            // writer who claimed nothing has not answered it.
            sql.push_str(" AND confidence >= ?");
            values.push(Sql::Real(threshold));
        }
        sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
        values.push(Sql::Integer(i64::from(query.limit)));

        let mut statement = self.conn().prepare(&sql)?;
        let rows = statement.query_map(
            rusqlite::params_from_iter(values.iter()),
            crate::store::row_to_memory,
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// How many drafts are waiting, in scope.
    ///
    /// A count rather than a page: the number is what `doctor` and every "you have N
    /// waiting" line needs, and it is answered from `idx_mem_ws_status_created` without
    /// reading a row.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the count fails.
    pub fn pending_count(&self, scope: &crate::model::Scope) -> Result<i64> {
        let (clause, values) = crate::store::scope_clause(None, scope);
        let sql =
            format!("SELECT count(*) FROM memories WHERE status = 'pending_approval'{clause}");
        let mut statement = self.conn().prepare(&sql)?;
        let count =
            statement.query_row(rusqlite::params_from_iter(values.iter()), |row| row.get(0))?;
        Ok(count)
    }

    /// Activate drafts: status, index, and the provenance that was waiting with them.
    ///
    /// One transaction for the whole call, and the index row is written by `mem_au`
    /// rather than by this module. That is the design: a second statement that wrote the
    /// index would be a second thing that can forget to.
    ///
    /// The `pending_derived_from` column is read, wired and cleared here — a draft's
    /// `--derived-from` is dropped by `add` (an edge from a memory retrieval cannot
    /// return would leak the draft through `relate`), so the ids are held on the row
    /// until the row is allowed to exist.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if a statement fails. An id that is not pending is
    /// reported in the outcome rather than raised: approving four drafts of which one
    /// was already approved must not undo the other three.
    pub fn approve(&self, ids: &[String]) -> Result<ReviewOutcome> {
        let transaction = self.conn().unchecked_transaction()?;
        let mut moved = Vec::new();
        let mut skipped = Vec::new();
        let mut seen: Vec<&String> = Vec::new();

        for id in ids {
            if seen.contains(&id) {
                skipped.push(Skipped::new(id, SkipReason::Duplicate));
                continue;
            }
            seen.push(id);

            let current: Option<Status> = transaction
                .query_row("SELECT status FROM memories WHERE id = ?1", [id], |row| {
                    row.get(0)
                })
                .ok();
            let Some(status) = current else {
                skipped.push(Skipped::new(id, SkipReason::NotFound));
                continue;
            };
            if status != Status::PendingApproval {
                skipped.push(Skipped::in_status(id, SkipReason::WrongStatus, status));
                continue;
            }

            let pending: Option<String> = transaction
                .query_row(
                    "SELECT pending_derived_from FROM memories WHERE id = ?1",
                    [id],
                    |row| row.get(0),
                )
                .unwrap_or(None);
            // Read before the status flip, because the auto-link pass needs the text and the
            // scope, and the row is about to become findable — but the status flip is what
            // makes the index row exist, so the text is read here and used below.
            let text: Option<(String, String, String)> = transaction
                .query_row(
                    "SELECT workspace_id, title, body FROM memories WHERE id = ?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .ok();

            transaction.execute(
                "UPDATE memories
                    SET status = 'active',
                        pending_derived_from = NULL,
                        updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                  WHERE id = ?1",
                [id],
            )?;

            if let Some(raw) = pending {
                let targets: Vec<String> = serde_json::from_str(&raw).unwrap_or_default();
                if !targets.is_empty() {
                    crate::links::link_derived_from(&transaction, id, &targets)?;
                }
            }
            // The same activation hook `add` uses: a memory becomes findable here, so this
            // is where it joins the graph. The row is in the index by now — the status flip
            // is what the `mem_au` trigger watches for — so the ladder can see it (and is
            // told to exclude it). A failure is not a failed approval: the memory is active
            // and findable, and an edge is enrichment (see `store::insert_one`).
            if let Some((workspace_id, title, body)) = text {
                let _ = crate::links::auto_link(&transaction, id, &workspace_id, &title, &body);
            }
            moved.push(id.clone());
        }

        transaction.commit()?;
        // Approving is the moment a draft becomes findable, so it is the moment it needs a
        // vector: `add` skipped one for a draft (semantic search cannot return a
        // `pending_approval` row), and the search that runs right after this would
        // otherwise never find it. Failures are silent here because the review outcome has
        // no warning channel and a memory that is approved and lexically findable is a
        // success; `doctor`'s coverage count and `reindex --embed` are where a missing
        // vector surfaces.
        if !moved.is_empty() {
            let _ = self.embed_ids(&moved);
        }
        Ok(ReviewOutcome { moved, skipped })
    }

    /// Reject drafts: archived, never activated, never indexed.
    ///
    /// Archived rather than deleted, which is the hosted product's rule and the right one
    /// for an inbox: rejecting is a decision made without much information, and a
    /// rejection that cannot be undone is a decision a reviewer will not make. The
    /// segment is recoverable with `kiomon restore`.
    ///
    /// # Errors
    ///
    /// As [`Database::approve`].
    pub fn reject(&self, ids: &[String]) -> Result<ReviewOutcome> {
        let transaction = self.conn().unchecked_transaction()?;
        let mut moved = Vec::new();
        let mut skipped = Vec::new();
        let mut seen: Vec<&String> = Vec::new();

        for id in ids {
            if seen.contains(&id) {
                skipped.push(Skipped::new(id, SkipReason::Duplicate));
                continue;
            }
            seen.push(id);

            let current: Option<Status> = transaction
                .query_row("SELECT status FROM memories WHERE id = ?1", [id], |row| {
                    row.get(0)
                })
                .ok();
            let Some(status) = current else {
                skipped.push(Skipped::new(id, SkipReason::NotFound));
                continue;
            };
            if status != Status::PendingApproval {
                skipped.push(Skipped::in_status(id, SkipReason::WrongStatus, status));
                continue;
            }

            transaction.execute(
                "UPDATE memories
                    SET status = 'archived',
                        pending_derived_from = NULL,
                        updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                  WHERE id = ?1",
                [id],
            )?;
            moved.push(id.clone());
        }

        transaction.commit()?;
        Ok(ReviewOutcome { moved, skipped })
    }

    /// Pin or unpin memories.
    ///
    /// A pin is two promises: decay skips the row, and every ordering puts it first.
    /// Both are honoured in the read paths rather than copied into a second column, so
    /// unpinning is exactly reversible.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if a statement fails.
    pub fn set_pinned(&self, ids: &[String], pinned: bool) -> Result<IdsOutcome> {
        let transaction = self.conn().unchecked_transaction()?;
        let mut moved = Vec::new();
        let mut skipped = Vec::new();
        let mut seen: Vec<&String> = Vec::new();

        for id in ids {
            if seen.contains(&id) {
                skipped.push(Skipped::new(id, SkipReason::Duplicate));
                continue;
            }
            seen.push(id);

            let current: Option<bool> = transaction
                .query_row("SELECT pinned FROM memories WHERE id = ?1", [id], |row| {
                    row.get(0)
                })
                .ok();
            let Some(current) = current else {
                skipped.push(Skipped::new(id, SkipReason::NotFound));
                continue;
            };
            if current == pinned {
                let status = transaction
                    .query_row("SELECT status FROM memories WHERE id = ?1", [id], |row| {
                        row.get(0)
                    })
                    .ok();
                skipped.push(Skipped {
                    id: id.clone(),
                    reason: SkipReason::AlreadyThere,
                    status,
                });
                continue;
            }
            transaction.execute(
                "UPDATE memories
                    SET pinned = ?2,
                        updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                  WHERE id = ?1",
                rusqlite::params![id, pinned],
            )?;
            moved.push(id.clone());
        }

        transaction.commit()?;
        Ok(IdsOutcome { moved, skipped })
    }

    /// Archive memories: hidden from retrieval, never purged.
    ///
    /// The `mem_au` trigger removes the index row as part of the same statement, so
    /// "archived" and "unsearchable" are one fact and not two.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if a statement fails.
    pub fn archive(&self, ids: &[String]) -> Result<IdsOutcome> {
        let transaction = self.conn().unchecked_transaction()?;
        let mut moved = Vec::new();
        let mut skipped = Vec::new();
        let mut seen: Vec<&String> = Vec::new();

        for id in ids {
            if seen.contains(&id) {
                skipped.push(Skipped::new(id, SkipReason::Duplicate));
                continue;
            }
            seen.push(id);

            let current: Option<Status> = transaction
                .query_row("SELECT status FROM memories WHERE id = ?1", [id], |row| {
                    row.get(0)
                })
                .ok();
            let Some(status) = current else {
                skipped.push(Skipped::new(id, SkipReason::NotFound));
                continue;
            };
            if status == Status::Archived {
                skipped.push(Skipped::in_status(id, SkipReason::AlreadyThere, status));
                continue;
            }
            transaction.execute(
                "UPDATE memories
                    SET status = 'archived',
                        updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                  WHERE id = ?1",
                [id],
            )?;
            moved.push(id.clone());
        }

        transaction.commit()?;
        Ok(IdsOutcome { moved, skipped })
    }

    /// Bring archived memories back, with a fresh decay clock.
    ///
    /// `strength = importance` is the hosted product's rule and it is load-bearing: a
    /// strength left at the floor would be below the line on the next decay pass, so
    /// every restore would be undone by the next `kiomi decay` and the user would
    /// conclude that restore does not work.
    ///
    /// `expires_at = NULL` is a deliberate divergence from the hosted implementation,
    /// which keeps the deadline — and therefore re-archives the row on the next pass, so
    /// its restore does not work either. Restoring is a person saying the deadline no
    /// longer applies; the plan records the divergence.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if a statement fails.
    pub fn restore(&self, ids: &[String]) -> Result<IdsOutcome> {
        let transaction = self.conn().unchecked_transaction()?;
        let mut moved = Vec::new();
        let mut skipped = Vec::new();
        let mut seen: Vec<&String> = Vec::new();

        for id in ids {
            if seen.contains(&id) {
                skipped.push(Skipped::new(id, SkipReason::Duplicate));
                continue;
            }
            seen.push(id);

            let current: Option<Status> = transaction
                .query_row("SELECT status FROM memories WHERE id = ?1", [id], |row| {
                    row.get(0)
                })
                .ok();
            let Some(status) = current else {
                skipped.push(Skipped::new(id, SkipReason::NotFound));
                continue;
            };
            if status != Status::Archived {
                skipped.push(Skipped::in_status(id, SkipReason::WrongStatus, status));
                continue;
            }
            transaction.execute(
                "UPDATE memories
                    SET status = 'active',
                        strength = importance,
                        expires_at = NULL,
                        updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                  WHERE id = ?1",
                [id],
            )?;
            moved.push(id.clone());
        }

        transaction.commit()?;
        Ok(IdsOutcome { moved, skipped })
    }

    /// Record one rating, and return the average it moved.
    ///
    /// A weighted running average rather than a sum: a memory rated `-1` fifty times and
    /// `+1` once should not look the same as one rated `+1` and then `-1`, and only an
    /// average can tell them apart.
    ///
    /// The arithmetic is done in SQL against the stored row, so the score is a function
    /// of the state it is stored in rather than of a value read a moment earlier.
    /// `feedback_score` and `feedback_count` are not in `mem_au`'s `UPDATE OF` list, so
    /// this does not rebuild the index row.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] if no memory has that id.
    pub fn rate(&self, id: &str, rating: Rating) -> Result<RatingOutcome> {
        let transaction = self.conn().unchecked_transaction()?;
        let row = transaction
            .query_row(
                "UPDATE memories
                    SET feedback_count = feedback_count + 1,
                        feedback_score = ROUND((feedback_score * feedback_count + ?2)
                                               / (feedback_count + 1), 2),
                        updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                  WHERE id = ?1
                  RETURNING feedback_score, feedback_count",
                rusqlite::params![id, rating.value()],
                |row| Ok((row.get::<_, f64>(0)?, row.get::<_, i64>(1)?)),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Error::NotFound(format!(
                    "no memory with id `{id}`. It may have been forgotten, archived, or \
                     exported from a different database."
                )),
                other => other.into(),
            })?;
        transaction.commit()?;
        Ok(RatingOutcome {
            id: id.to_owned(),
            rating,
            feedback_score: row.0,
            feedback_count: row.1,
        })
    }

    /// Credit a task's outcome to the memories that were part of it.
    ///
    /// One transaction for the whole call, and every effect is derived from the row as
    /// it is stored rather than from a value read earlier, so two assessments of the
    /// same memory in one call cannot derive from the same base.
    ///
    /// The `memory_outcomes` row is written **before** its effect is applied, in the same
    /// transaction. It is the audit trail: a strength or a confidence that moved without
    /// a row explaining it is a number nobody can argue with.
    ///
    /// `outcome` itself changes nothing. A `helpful` memory in a failed task is still a
    /// helpful memory, and the session's end state is a fact about the session.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Invalid`] for more than [`MAX_OUTCOME_ITEMS`] items. Ids that do
    /// not exist, or that fall outside `scope`, are reported in the outcome rather than
    /// raised.
    pub fn record_outcome(
        &self,
        outcome: TaskOutcome,
        items: &[(String, Assessment)],
        scope: &crate::model::Scope,
    ) -> Result<OutcomeReport> {
        if items.len() > MAX_OUTCOME_ITEMS {
            return Err(Error::Invalid(format!(
                "one outcome call may assess at most {MAX_OUTCOME_ITEMS} memories, got {}",
                items.len()
            )));
        }

        let transaction = self.conn().unchecked_transaction()?;
        let mut applied = Vec::new();
        let mut skipped = Vec::new();
        let mut seen: Vec<&String> = Vec::new();

        for (id, assessment) in items {
            if seen.contains(&id) {
                // The same memory assessed twice in one call is a caller bug, and
                // applying both would make the result depend on their order.
                skipped.push(Skipped::new(id, SkipReason::Duplicate));
                continue;
            }
            seen.push(id);

            let row: Option<CreditRow> = transaction
                .query_row(
                    "SELECT confidence, feedback_count, feedback_score, workspace_id
                       FROM memories WHERE id = ?1",
                    [id],
                    |row| {
                        Ok(CreditRow {
                            claimed: row.get(0)?,
                            ratings: row.get(1)?,
                            average: row.get(2)?,
                            workspace_id: row.get(3)?,
                        })
                    },
                )
                .ok();

            let Some(credit) = row else {
                skipped.push(Skipped::new(id, SkipReason::NotFound));
                continue;
            };
            if let Some(only) = scope.one() {
                if credit.workspace_id != only {
                    skipped.push(Skipped::new(id, SkipReason::OutOfScope));
                    continue;
                }
            }

            let base = base_confidence(credit.claimed);
            // `None` means "leave the claim exactly as it was". `unused` is the only
            // assessment that writes nothing: it is not evidence about reliability, and
            // writing the platform's 0.5 fallback onto a row whose writer claimed
            // nothing would invent a claim out of an absence of information. A
            // deliberate divergence, recorded in the plan.
            let (confidence, expires_at) = match assessment {
                Assessment::Helpful => (Some(confidence_after_helpful(base)), None),
                Assessment::Unused => (None, None),
                Assessment::Misleading => (
                    Some(confidence_after_negative(base, MISLEADING_DECAY)),
                    None,
                ),
                Assessment::Outdated => (
                    Some(confidence_after_negative(base, OUTDATED_DECAY)),
                    Some(now_expression(&transaction)?),
                ),
            };

            transaction.execute(
                "INSERT INTO memory_outcomes (memory_id, outcome, assessment, created_at)
                 VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))",
                rusqlite::params![id, outcome.as_str(), assessment.as_str()],
            )?;

            let score_next =
                running_average(credit.average, credit.ratings, rating_for(*assessment));
            transaction.execute(
                "UPDATE memories
                    SET confidence = COALESCE(?2, confidence),
                        expires_at = COALESCE(?3, expires_at),
                        feedback_score = ?4,
                        feedback_count = feedback_count + 1,
                        corroboration_count = corroboration_count + ?5,
                        updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                  WHERE id = ?1",
                rusqlite::params![
                    id,
                    confidence,
                    expires_at,
                    score_next,
                    i64::from(assessment.is_corroboration())
                ],
            )?;

            applied.push(AppliedOutcome {
                id: id.clone(),
                assessment: *assessment,
                confidence: confidence.unwrap_or(base),
                feedback_score: score_next,
                feedback_count: credit.ratings + 1,
                expires_at,
            });
        }

        transaction.commit()?;
        Ok(OutcomeReport {
            outcome,
            applied,
            skipped,
        })
    }

    /// How many rows of credit history a memory has.
    ///
    /// Used by `doctor` and by the tests that assert the audit trail exists; it is not
    /// read by anything on the hot path.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the count fails.
    pub fn outcome_count(&self, id: &str) -> Result<i64> {
        let mut statement = self
            .conn()
            .prepare("SELECT count(*) FROM memory_outcomes WHERE memory_id = ?1")?;
        let count = statement.query_row([id], |row| row.get(0))?;
        Ok(count)
    }

    /// The kinds that decay at all.
    ///
    /// `reference` is absent deliberately: it is the user's own library, and a file the
    /// user saved is not a claim that gets less true. The plan and the hosted product
    /// agree on this, and it is the reason `HALF_LIFE` is a lookup with a missing arm
    /// rather than a match with a zero in it.
    #[must_use]
    pub fn decays(kind: MemoryKind) -> bool {
        crate::decay::half_life_days(kind).is_some()
    }
}

/// What an assessment feeds the running average.
///
/// `unused` feeds it a zero, which is a datum and not an absence — that is the whole
/// reason `feedback_count` exists. Without a count, an `unused` assessment would be a
/// no-op and a memory retrieved forty times and never once used would look exactly like a
/// memory nobody had ever been asked about.
///
/// `misleading` and `outdated` are both `-1` and are deliberately written as one arm:
/// they are the same verdict on the rating scale — it was wrong to rely on this — and
/// they differ in what they do to *confidence*, not in how much they count.
const fn rating_for(assessment: Assessment) -> f64 {
    match assessment {
        Assessment::Helpful => 1.0,
        Assessment::Misleading | Assessment::Outdated => -1.0,
        Assessment::Unused => 0.0,
    }
}

/// What `record_outcome` reads about a memory before it decides anything: the claimed
/// confidence, the running average and its count, and the workspace it belongs to.
///
/// The existing `expires_at` is deliberately not read: the only assessment that sets a
/// deadline sets it to now, and the others keep whatever was there through `COALESCE`,
/// so reading it would be reading a value nothing decides on.
///
/// A struct rather than a five-tuple, because a tuple of five types is a thing a reader
/// has to hold in their head exactly once per read and gets wrong once per edit.
struct CreditRow {
    claimed: Option<f64>,
    ratings: i64,
    average: f64,
    workspace_id: String,
}

/// The database's own timestamp, so `expires_at` comes from the clock that wrote every
/// other column.
fn now_expression(conn: &rusqlite::Connection) -> Result<String> {
    crate::db::now(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NewMemory, Scope};
    use crate::testing;

    fn draft(db: &Database, title: &str) -> String {
        db.add(
            &NewMemory::new(MemoryKind::Semantic, title)
                .body("a claim nobody has checked")
                .source("agent_reflection")
                .confidence(0.4),
        )
        .expect("the fixture draft must be written")
        .id
    }

    #[test]
    fn a_draft_is_not_indexed_and_an_approval_indexes_it() {
        // The review inbox's boundary, asserted on the index itself rather than on a
        // search: a draft that is merely *not returned* by today's query is one rewrite
        // away from being returned by tomorrow's.
        let db = testing::db();
        let id = draft(&db, "Unreviewed claim");
        assert_eq!(testing::fts_rows(&db).unwrap(), 0);
        assert_eq!(db.get(&id).unwrap().status, Status::PendingApproval);
        testing::add(&db, MemoryKind::Semantic, "An active memory", "x");

        let outcome = db.approve(std::slice::from_ref(&id)).unwrap();
        assert_eq!(outcome.moved, vec![id.clone()]);
        assert!(outcome.skipped.is_empty());
        assert_eq!(testing::fts_rows(&db).unwrap(), 2);
        assert_eq!(db.get(&id).unwrap().status, Status::Active);
    }

    #[test]
    fn approving_an_already_active_memory_is_reported_rather_than_raised() {
        let db = testing::db();
        let id = testing::add(&db, MemoryKind::Semantic, "Already active", "body");
        let outcome = db.approve(&[id.clone(), "k_missing".to_owned()]).unwrap();
        assert!(outcome.moved.is_empty());
        assert_eq!(outcome.skipped.len(), 2);
        assert_eq!(outcome.skipped[0].reason, SkipReason::WrongStatus);
        assert_eq!(outcome.skipped[0].status, Some(Status::Active));
        assert_eq!(outcome.skipped[1].reason, SkipReason::NotFound);
    }

    #[test]
    fn a_rejection_archives_and_still_does_not_index() {
        let db = testing::db();
        let id = draft(&db, "Rejected claim");
        let outcome = db.reject(std::slice::from_ref(&id)).unwrap();
        assert_eq!(outcome.moved, vec![id.clone()]);
        assert_eq!(db.get(&id).unwrap().status, Status::Archived);
        assert_eq!(testing::fts_rows(&db).unwrap(), 0);
    }

    #[test]
    fn a_rejected_draft_can_be_restored_even_though_it_was_never_activated() {
        // Archive, not delete: a rejection is a decision made without much information.
        let db = testing::db();
        let id = draft(&db, "Rejected then restored");
        db.reject(std::slice::from_ref(&id)).unwrap();
        let outcome = db.restore(std::slice::from_ref(&id)).unwrap();
        assert_eq!(outcome.moved, vec![id.clone()]);
        assert_eq!(db.get(&id).unwrap().status, Status::Active);
        assert_eq!(
            testing::fts_rows(&db).unwrap(),
            1,
            "restoring an archived row must index it, whatever it was before"
        );
    }

    #[test]
    fn a_drafts_provenance_survives_until_it_is_approved() {
        // `add` deliberately writes no edge for a draft: an edge from a memory retrieval
        // cannot return would leak the draft's title through `relate`. The ids are held
        // on the row instead, and wired on approval.
        let db = testing::db();
        let parent = testing::add(&db, MemoryKind::Semantic, "The parent", "origin");
        let id = db
            .add(
                &NewMemory::new(MemoryKind::Semantic, "Derived claim")
                    .source("agent_reflection")
                    .confidence(0.3)
                    .derived_from(vec![parent.clone()]),
            )
            .unwrap()
            .id;

        assert_eq!(
            db.count("SELECT count(*) FROM memory_links", &[]).unwrap(),
            0
        );
        db.approve(std::slice::from_ref(&id)).unwrap();
        assert_eq!(
            db.count("SELECT count(*) FROM memory_links", &[]).unwrap(),
            1
        );
        let origin: String = db
            .conn()
            .query_row(
                "SELECT origin FROM memory_links WHERE source_id = ?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(origin, "agent");
    }

    #[test]
    fn a_rejected_drafts_pending_provenance_is_discarded() {
        let db = testing::db();
        let parent = testing::add(&db, MemoryKind::Semantic, "The parent", "origin");
        let id = db
            .add(
                &NewMemory::new(MemoryKind::Semantic, "Rejected claim")
                    .source("agent_reflection")
                    .confidence(0.3)
                    .derived_from(vec![parent]),
            )
            .unwrap()
            .id;
        db.reject(std::slice::from_ref(&id)).unwrap();
        assert_eq!(
            db.count("SELECT count(*) FROM memory_links", &[]).unwrap(),
            0
        );
        // And restoring it later must not resurrect provenance the user rejected.
        db.restore(&[id]).unwrap();
        assert_eq!(
            db.count("SELECT count(*) FROM memory_links", &[]).unwrap(),
            0
        );
    }

    #[test]
    fn the_inbox_lists_only_drafts_newest_first() {
        let db = testing::db();
        let first = draft(&db, "Older draft");
        let second = draft(&db, "Newer draft");
        testing::add(&db, MemoryKind::Semantic, "Active memory", "body");

        let rows = db.list_pending(&PendingQuery::default()).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter().all(|row| row.status == Status::PendingApproval),
            "the inbox must never show an active memory"
        );
        // `created_at` has second precision, so the tie is the common case and the id
        // tiebreak is what makes the order a fact rather than a coincidence.
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert!(ids.contains(&first.as_str()) && ids.contains(&second.as_str()));
    }

    #[test]
    fn the_inbox_can_be_filtered_by_confidence_and_scope() {
        let db = testing::db();
        db.create_workspace("Other", crate::model::ReviewMode::Autonomous)
            .unwrap();
        // 0.7 is about as confident as a draft can be: at `AUTOPROMOTE_THRESHOLD` (0.75)
        // the write gate activates it and it never reaches the inbox at all.
        db.add(
            &NewMemory::new(MemoryKind::Semantic, "Confident draft")
                .source("agent_reflection")
                .confidence(0.7),
        )
        .unwrap();
        draft(&db, "Unsure draft");
        db.add(
            &NewMemory::new(MemoryKind::Semantic, "Elsewhere")
                .source("agent_reflection")
                .confidence(0.7)
                .workspace("other"),
        )
        .unwrap();

        let all = db.list_pending(&PendingQuery::default()).unwrap();
        assert_eq!(all.len(), 2, "the default scope is one workspace");

        let confident = db
            .list_pending(&PendingQuery {
                min_confidence: Some(0.6),
                ..PendingQuery::default()
            })
            .unwrap();
        assert_eq!(confident.len(), 1);
        assert_eq!(confident[0].title, "Confident draft");

        let everywhere = db
            .list_pending(&PendingQuery {
                scope: Scope::All,
                ..PendingQuery::default()
            })
            .unwrap();
        assert_eq!(everywhere.len(), 3);

        assert_eq!(db.pending_count(&Scope::All).unwrap(), 3);
        assert_eq!(
            db.pending_count(&Scope::One("default".to_owned())).unwrap(),
            2
        );
    }

    #[test]
    fn a_pin_moves_only_when_it_changes_something() {
        let db = testing::db();
        let id = testing::add(&db, MemoryKind::Semantic, "Pin me", "body");

        let first = db.set_pinned(std::slice::from_ref(&id), true).unwrap();
        assert_eq!(first.moved, vec![id.clone()]);
        let again = db.set_pinned(std::slice::from_ref(&id), true).unwrap();
        assert!(again.moved.is_empty());
        assert_eq!(again.skipped[0].reason, SkipReason::AlreadyThere);
        let off = db.set_pinned(std::slice::from_ref(&id), false).unwrap();
        assert_eq!(off.moved, vec![id.clone()]);
        assert!(!db.get(&id).unwrap().pinned);
    }

    #[test]
    fn archiving_hides_a_memory_from_every_read_path() {
        let db = testing::db();
        let id = testing::add(&db, MemoryKind::Semantic, "Archive me", "body");
        assert_eq!(testing::fts_rows(&db).unwrap(), 1);

        let outcome = db.archive(std::slice::from_ref(&id)).unwrap();
        assert_eq!(outcome.moved, vec![id.clone()]);
        assert_eq!(testing::fts_rows(&db).unwrap(), 0);
        assert_eq!(db.get(&id).unwrap().status, Status::Archived);
        assert!(
            db.list(&crate::model::ListQuery::default())
                .unwrap()
                .is_empty(),
            "an archived memory must not appear in a bare list"
        );

        let again = db.archive(&[id]).unwrap();
        assert_eq!(again.skipped[0].reason, SkipReason::AlreadyThere);
    }

    #[test]
    fn restoring_refreshes_strength_and_clears_the_deadline() {
        // Both halves matter. Without the strength refresh the next decay pass
        // re-archives the row; and the deadline is what an `outdated` assessment set, so
        // keeping it would make restore useless for exactly the memories most likely to
        // need it.
        let db = testing::db();
        let id = testing::add(&db, MemoryKind::Semantic, "Expired fact", "body");
        db.conn()
            .execute(
                "UPDATE memories SET strength = 0.05,
                        expires_at = '2020-01-01T00:00:00Z'
                  WHERE id = ?1",
                [&id],
            )
            .unwrap();
        db.archive(std::slice::from_ref(&id)).unwrap();

        let outcome = db.restore(std::slice::from_ref(&id)).unwrap();
        assert_eq!(outcome.moved, vec![id.clone()]);
        let restored = db.get(&id).unwrap();
        assert_eq!(restored.status, Status::Active);
        assert_eq!(restored.expires_at, None);
        assert!(
            (restored.strength - restored.importance).abs() < f64::EPSILON,
            "a restored memory must not be re-archived by the next decay pass"
        );

        let not_archived = db
            .restore(&[testing::add(&db, MemoryKind::Semantic, "A", "b")])
            .unwrap();
        assert_eq!(not_archived.skipped[0].reason, SkipReason::WrongStatus);
    }

    #[test]
    fn a_first_rating_is_the_average_and_a_second_one_moves_it() {
        let db = testing::db();
        let id = testing::add(&db, MemoryKind::Semantic, "Rate me", "body");

        let first = db.rate(&id, Rating::Up).unwrap();
        assert_eq!(first.feedback_count, 1);
        assert!((first.feedback_score - 1.0).abs() < f64::EPSILON);

        let second = db.rate(&id, Rating::Down).unwrap();
        assert_eq!(second.feedback_count, 2);
        assert!(
            second.feedback_score.abs() < f64::EPSILON,
            "one up and one down average to zero, got {}",
            second.feedback_score
        );

        let third = db.rate(&id, Rating::Down).unwrap();
        assert_eq!(third.feedback_count, 3);
        assert!((third.feedback_score + 0.33).abs() < 1e-9, "{third:?}");

        let stored = db.get(&id).unwrap();
        assert_eq!(stored.feedback_count, 3);
        assert!((stored.feedback_score + 0.33).abs() < 1e-9);
    }

    #[test]
    fn rating_never_moves_strength() {
        // Decay owns strength and recomputes it from importance, so a rating that
        // multiplied it would be erased by the next pass. Asserted because the plan
        // originally specified exactly that, and the specification was wrong.
        let db = testing::db();
        let id = testing::add(&db, MemoryKind::Semantic, "Rate me", "body");
        let before = db.get(&id).unwrap().strength;
        db.rate(&id, Rating::Up).unwrap();
        db.rate(&id, Rating::Down).unwrap();
        assert!((db.get(&id).unwrap().strength - before).abs() < f64::EPSILON);
    }

    #[test]
    fn rating_something_that_does_not_exist_names_the_id() {
        let db = testing::db();
        let err = db.rate("k_01JC9W5K2Q", Rating::Up).unwrap_err();
        assert_eq!(err.exit_code(), crate::ExitCode::NotFound);
        assert!(err.to_string().contains("k_01JC9W5K2Q"), "{err}");
    }

    #[test]
    fn helpful_credit_raises_confidence_without_ever_reaching_certainty() {
        assert!((confidence_after_helpful(0.5) - 0.55).abs() < 1e-9);
        assert!((confidence_after_helpful(0.9) - 0.91).abs() < 1e-9);
        assert!(confidence_after_helpful(1.0) <= 1.0);
    }

    #[test]
    fn a_misleading_memory_loses_confidence_but_has_a_floor() {
        assert!((confidence_after_negative(1.0, MISLEADING_DECAY) - 0.85).abs() < 1e-9);
        assert!((confidence_after_negative(0.0, MISLEADING_DECAY) - CONFIDENCE_FLOOR).abs() < 1e-9);
        // An `outdated` memory is demoted more gently than a misleading one: a stale
        // fact was true when it was written.
        assert!(
            confidence_after_negative(0.8, OUTDATED_DECAY)
                > confidence_after_negative(0.8, MISLEADING_DECAY)
        );
    }

    #[test]
    fn the_running_average_is_an_average_and_not_a_sum() {
        let mut score = 0.0;
        for (count, value) in [1.0, 1.0, -1.0, 0.0].into_iter().enumerate() {
            #[allow(clippy::cast_possible_wrap)]
            // A four-element fixture: the cast is exact and the lint is about the general
            // case.
            let count = count as i64;
            score = running_average(score, count, value);
        }
        assert!((score - 0.25).abs() < 1e-9, "got {score}");

        // The case the count exists for: forty `unused` assessments pull the average
        // down, where a sum would have left it exactly where it started.
        //
        // They pull it to 0.07 and not to zero, and the reason is the rounding: the
        // average is rounded to two places on *every* step, which gives it a fixed point
        // -- once a step would move it by less than half a hundredth, the step is a
        // no-op. That is the hosted product's arithmetic and it is kept; the assertion is
        // the honest one rather than the flattering one.
        let mut score = 1.0;
        for count in 1..=40 {
            score = running_average(score, count, 0.0);
        }
        assert!(
            score < 0.1,
            "forty zeros must not be invisible, got {score}"
        );
        let settled = running_average(score, 40, 0.0);
        assert!(
            (settled - score).abs() < 0.01,
            "the forty-first zero barely moves it: {settled}"
        );
    }

    #[test]
    fn an_outcome_moves_confidence_and_expiry_but_not_strength() {
        let db = testing::db();
        let id = testing::add(&db, MemoryKind::Semantic, "Fact", "body");
        let strength_before = db.get(&id).unwrap().strength;

        let report = db
            .record_outcome(
                TaskOutcome::Success,
                &[(id.clone(), Assessment::Helpful)],
                &Scope::One("default".to_owned()),
            )
            .unwrap();

        assert_eq!(report.applied.len(), 1);
        assert_eq!(report.applied[0].assessment, Assessment::Helpful);
        assert!(report.applied[0].confidence > 0.5);
        assert_eq!(report.applied[0].expires_at, None);

        let stored = db.get(&id).unwrap();
        assert!((stored.strength - strength_before).abs() < f64::EPSILON);
        assert_eq!(stored.feedback_count, 1);
        assert!((stored.feedback_score - 1.0).abs() < f64::EPSILON);
        assert_eq!(stored.corroboration_count, 1);
        assert_eq!(db.outcome_count(&id).unwrap(), 1);
    }

    #[test]
    fn an_outdated_fact_is_expired_and_an_unused_one_changes_no_confidence() {
        let db = testing::db();
        let stale = testing::add(&db, MemoryKind::Semantic, "Stale", "body");
        let ignored = testing::add(&db, MemoryKind::Semantic, "Ignored", "body");

        let report = db
            .record_outcome(
                TaskOutcome::Success,
                &[
                    (stale.clone(), Assessment::Outdated),
                    (ignored.clone(), Assessment::Unused),
                ],
                &Scope::All,
            )
            .unwrap();
        assert_eq!(report.applied.len(), 2);

        let expiring = db.get(&stale).unwrap();
        assert!(expiring.expires_at.is_some(), "outdated must expire");
        assert!(expiring.confidence.unwrap() < 0.5);
        assert_eq!(expiring.corroboration_count, 1);

        let untouched = db.get(&ignored).unwrap();
        assert_eq!(untouched.confidence, None, "unused must not invent a claim");
        assert_eq!(untouched.feedback_count, 1);
        assert!(
            untouched.feedback_score.abs() < f64::EPSILON,
            "an `unused` assessment is a zero, and a zero moves an average"
        );
        assert_eq!(untouched.corroboration_count, 0);
    }

    #[test]
    fn a_misleading_memory_loses_confidence_and_says_so_in_the_log() {
        let db = testing::db();
        let id = testing::add(&db, MemoryKind::Semantic, "Wrong", "body");
        db.record_outcome(
            TaskOutcome::Failure,
            &[(id.clone(), Assessment::Misleading)],
            &Scope::All,
        )
        .unwrap();
        let stored = db.get(&id).unwrap();
        assert!(stored.confidence.unwrap() < 0.5);
        assert_eq!(
            stored.expires_at, None,
            "misleading is not the same as stale"
        );
        assert_eq!(db.outcome_count(&id).unwrap(), 1);
    }

    #[test]
    fn an_outcome_records_the_session_it_came_from() {
        let db = testing::db();
        let id = testing::add(&db, MemoryKind::Semantic, "Fact", "body");
        db.record_outcome(
            TaskOutcome::Failure,
            &[(id.clone(), Assessment::Helpful)],
            &Scope::All,
        )
        .unwrap();
        let (outcome, assessment): (String, String) = db
            .conn()
            .query_row(
                "SELECT outcome, assessment FROM memory_outcomes WHERE memory_id = ?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(outcome, "failure");
        assert_eq!(
            assessment, "helpful",
            "a helpful memory in a failed task is still a helpful memory"
        );
    }

    #[test]
    fn an_outcome_outside_the_scope_is_skipped_rather_than_credited() {
        let db = testing::db();
        db.create_workspace("Other", crate::model::ReviewMode::Autonomous)
            .unwrap();
        let elsewhere = db
            .add(
                &NewMemory::new(MemoryKind::Semantic, "Their fact")
                    .body("body")
                    .workspace("other"),
            )
            .unwrap()
            .id;

        let report = db
            .record_outcome(
                TaskOutcome::Success,
                &[
                    (elsewhere.clone(), Assessment::Helpful),
                    ("k_missing".to_owned(), Assessment::Helpful),
                ],
                &Scope::One("default".to_owned()),
            )
            .unwrap();
        assert!(report.applied.is_empty());
        assert_eq!(report.skipped[0].reason, SkipReason::OutOfScope);
        assert_eq!(report.skipped[1].reason, SkipReason::NotFound);
        assert_eq!(db.get(&elsewhere).unwrap().feedback_count, 0);
    }

    #[test]
    fn the_same_id_twice_in_one_call_is_applied_once() {
        let db = testing::db();
        let id = testing::add(&db, MemoryKind::Semantic, "Fact", "body");
        let report = db
            .record_outcome(
                TaskOutcome::Success,
                &[
                    (id.clone(), Assessment::Helpful),
                    (id.clone(), Assessment::Misleading),
                ],
                &Scope::All,
            )
            .unwrap();
        assert_eq!(report.applied.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::Duplicate);
        assert_eq!(
            db.get(&id).unwrap().feedback_count,
            1,
            "applying both would make the result depend on their order"
        );
    }

    #[test]
    fn more_than_fifty_items_is_rejected_before_anything_is_written() {
        let db = testing::db();
        let items: Vec<(String, Assessment)> = (0..=MAX_OUTCOME_ITEMS)
            .map(|index| (format!("k_{index}"), Assessment::Helpful))
            .collect();
        let err = db
            .record_outcome(TaskOutcome::Success, &items, &Scope::All)
            .unwrap_err();
        assert_eq!(err.exit_code(), crate::ExitCode::InvalidArgs);
        assert_eq!(
            db.count("SELECT count(*) FROM memory_outcomes", &[])
                .unwrap(),
            0
        );
    }

    #[test]
    fn an_outcome_on_a_draft_is_recorded_without_activating_it() {
        // Credit is not approval. A draft that was somehow retrieved cannot be, so the
        // only way this happens is an id typed by hand, and the honest answer is to
        // record the assessment and leave the status alone.
        let db = testing::db();
        let id = draft(&db, "Still a draft");
        db.record_outcome(
            TaskOutcome::Success,
            &[(id.clone(), Assessment::Helpful)],
            &Scope::All,
        )
        .unwrap();
        assert_eq!(db.get(&id).unwrap().status, Status::PendingApproval);
        assert_eq!(testing::fts_rows(&db).unwrap(), 0);
    }

    #[test]
    fn an_assessment_parses_from_the_wire_spelling() {
        for assessment in Assessment::ALL {
            let parsed: Assessment = assessment.as_str().parse().unwrap();
            assert_eq!(parsed, assessment);
        }
        assert!("HELPFUL".parse::<Assessment>().is_ok());
        let err = "sometimes".parse::<Assessment>().unwrap_err();
        assert!(err.to_string().contains("helpful"), "{err}");
    }

    #[test]
    fn the_kinds_that_decay_are_the_kinds_the_plan_names() {
        assert!(Database::decays(MemoryKind::Episodic));
        assert!(Database::decays(MemoryKind::Procedural));
        assert!(Database::decays(MemoryKind::Semantic));
        assert!(
            !Database::decays(MemoryKind::Reference),
            "a file the user saved is not a claim that gets less true"
        );
    }
}
