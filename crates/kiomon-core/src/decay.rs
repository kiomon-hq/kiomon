// SPDX-License-Identifier: Apache-2.0

//! Decay: the forgetting policy.
//!
//! Ported verbatim from the hosted product's `jobs/decay-memories.ts`, because the
//! formula is the product: a memory weakens as a function of how long ago it was created
//! and how often it has been retrieved, and a memory that falls below the floor — or
//! past the deadline an `outdated` assessment gave it — is archived rather than deleted.
//!
//! ```text
//! strength            = importance * 0.5 ^ (age_days / effective_half_life)
//! effective_half_life = base_half_life[kind] * min(1 + 0.1 * retrieval_count, 2.0)
//! base_half_life      = { episodic: 30, procedural: 90, semantic: 180 }   // reference: never
//! strength < 0.05     →  archived at exactly 0.05
//! pinned = 1          →  skipped
//! ```
//!
//! Four decisions worth naming, because each is a behaviour a reader will otherwise
//! assume is a bug:
//!
//! * **Age is measured from `created_at`**, not from the last write or the last
//!   retrieval. Editing a memory does not make it newer — the claim is as old as the
//!   claim — and use is already represented, by the retrieval stretch on the half-life.
//! * **`reference` memories never decay.** They are the user's library: a file the user
//!   saved is not a claim that gets less true. They are not scanned at all, which is why
//!   the half-life is an `Option` rather than a zero.
//! * **The clock is SQLite's.** `age_days` comes out of the query as
//!   `julianday('now') - julianday(created_at)`, so this crate still has no datetime
//!   dependency and there is still exactly one clock in the system.
//! * **Archived, never purged.** `forget` is the only destructive verb.
//!
//! `decay` is a command, not a daemon (§5.4), so it also runs *opportunistically*: at
//! most once a day, at most one page, on the writes and reads that were going to happen
//! anyway. A brain nobody maintains still ages correctly, and the cost lands in the
//! background of a command the user asked for rather than in the foreground of one they
//! did not.

use rusqlite::types::Value as Sql;
use serde::Serialize;

use crate::db::Database;
use crate::error::Result;
use crate::model::{MemoryKind, Scope};

/// Base half-life per kind, in days. The floor a memory's usefulness is halved at.
pub const HALF_LIFE_EPISODIC_DAYS: f64 = 30.0;
/// See [`HALF_LIFE_EPISODIC_DAYS`].
pub const HALF_LIFE_PROCEDURAL_DAYS: f64 = 90.0;
/// See [`HALF_LIFE_EPISODIC_DAYS`].
pub const HALF_LIFE_SEMANTIC_DAYS: f64 = 180.0;

/// Strength below this is archived.
pub const STRENGTH_FLOOR: f64 = 0.05;

/// Each retrieval stretches the half-life by this fraction.
pub const STRETCH_PER_RETRIEVAL: f64 = 0.1;

/// The most a memory's half-life can be stretched, however often it is retrieved.
///
/// A cap rather than no cap so that a memory cannot become immortal by being popular:
/// decay has to still be a thing that happens, or the corpus only ever grows.
pub const MAX_STRETCH_FACTOR: f64 = 2.0;

/// Rows per page of the decay scan.
///
/// Paginated rather than one statement over the whole corpus: the scan visits every
/// active row in scope, and holding a hundred thousand of them in memory to update them
/// one at a time is the shape that makes a maintenance command the largest allocation in
/// the process.
pub const DECAY_PAGE_SIZE: u32 = 500;

/// Rows an *opportunistic* pass may touch, which is not [`DECAY_PAGE_SIZE`] and is
/// smaller for a measured reason.
///
/// Five hundred rows as the first write of the day measured **62 ms** of burst on a
/// 10 000-memory corpus (87.4 ms against 25.6 ms for the same `add` with nothing to do,
/// medians of twenty samples each, same binary and same corpus) — five times an `add`.
/// A hundred rows measured 48 ms, which is the number that decided the design: the cost
/// is **not proportional to the rows**. It is dominated by the pages a write touches on a
/// cold cache, scattered as they are across a 200 MB file by the two indexes that hold
/// `strength`, so shrinking the page buys almost nothing and a page this size is the
/// honest floor.
///
/// The nudge is not the mechanism, and this is the constant that says so: a hundred rows
/// a day cannot keep a ten-thousand-memory corpus current, and it is not supposed to.
/// `kiomon decay` is the mechanism and `doctor` reports how long it has been. What the
/// nudge does do is walk the corpus: it resumes from [`META_DECAY_CURSOR`], so over weeks
/// it reaches every memory in turn rather than rewriting the same hundred rows forever —
/// which is what the first version of this did, and the measurement is what caught it.
pub const DECAY_NUDGE_ROWS: u32 = 100;

/// How long between opportunistic runs.
///
/// A float because it is compared against a `julianday` difference, which is days;
/// declaring it in hours keeps the number a reader recognises — "once a day" — without an
/// integer-to-float cast at the comparison.
pub const DECAY_INTERVAL_HOURS: f64 = 24.0;

/// The most rows a report keeps for display.
///
/// The counts are always exact; the detail list is for a human reading a `--dry-run` or
/// a caller rendering a table, and neither needs a hundred thousand rows to be told what
/// happened. `truncated` says so rather than quietly reporting a subset as the whole.
pub const DETAIL_LIMIT: usize = 200;

/// The `meta` key holding the last opportunistic run.
pub const META_LAST_DECAY: &str = "last_decay_at";

/// The `meta` key holding where the opportunistic walk got to.
///
/// Without it every nudge starts at the beginning of the corpus and rewrites the same
/// hundred rows for as long as they stay active — which, for a hundred rows that are not
/// weak enough to archive, is forever. The cursor is what makes a hundred rows a day a
/// *walk* rather than a loop, and an empty value means "start again at the beginning",
/// which is what a completed lap resets to.
pub const META_DECAY_CURSOR: &str = "decay_cursor";

/// The base half-life of a kind, or `None` for a kind that does not decay.
#[must_use]
pub fn half_life_days(kind: MemoryKind) -> Option<f64> {
    match kind {
        MemoryKind::Episodic => Some(HALF_LIFE_EPISODIC_DAYS),
        MemoryKind::Procedural => Some(HALF_LIFE_PROCEDURAL_DAYS),
        MemoryKind::Semantic => Some(HALF_LIFE_SEMANTIC_DAYS),
        // Not "infinite": a kind that does not decay should not be in the scan at all.
        MemoryKind::Reference => None,
    }
}

/// The half-life after the retrieval stretch, in days.
///
/// `None` for a kind that never decays, so a caller cannot accidentally decay a
/// reference memory by treating its half-life as some very large number.
#[must_use]
pub fn effective_half_life_days(kind: MemoryKind, retrieval_count: i64) -> Option<f64> {
    let base = half_life_days(kind)?;
    // Exact for any count a brain could hold: an i64 above 2^53 is not a number of
    // retrievals, and the `.max(0)` above has already ruled out the sign. The lint is
    // about the general case and this is not it.
    #[allow(clippy::cast_precision_loss)]
    let retrievals = retrieval_count.max(0) as f64;
    let stretch = (1.0 + retrievals * STRETCH_PER_RETRIEVAL).min(MAX_STRETCH_FACTOR);
    Some(base * stretch)
}

/// The formula itself, with the half-life already resolved.
///
/// At `age_days = 0` this is `importance`; at one half-life it is half of it; at two, a
/// quarter. Panics are impossible: `half_life_days` divides rather than takes a root, and
/// with `age_days >= 0` the exponent is never negative.
#[must_use]
pub fn decayed_strength(importance: f64, age_days: f64, half_life_days: f64) -> f64 {
    importance * 0.5_f64.powf(age_days / half_life_days)
}

/// One row's decision, for the report.
#[derive(Debug, Clone, Serialize)]
pub struct DecayRow {
    /// The memory.
    pub id: String,
    /// The kind, which is what the half-life came from.
    pub kind: &'static str,
    /// How long ago it was created, in days.
    pub age_days: f64,
    /// Its strength before this pass.
    pub before: f64,
    /// Its strength after, or the floor when it was archived.
    pub after: f64,
    /// Whether it was archived by this pass.
    pub archived: bool,
    /// Whether it was archived because `expires_at` had passed rather than for weakness.
    pub expired: bool,
}

/// What one `decay` pass did.
#[derive(Debug, Clone, Serialize)]
pub struct DecayOutcome {
    /// Rows the scan visited.
    pub scanned: i64,
    /// Rows whose strength moved.
    pub decayed: i64,
    /// Rows archived — weak, or past their deadline.
    pub archived: i64,
    /// Whether anything was written. A dry run reports `true`, and writes nothing.
    pub dry_run: bool,
    /// The first [`DETAIL_LIMIT`] rows, for a human or a table.
    pub details: Vec<DecayRow>,
    /// Whether [`DecayOutcome::details`] is a subset of what happened.
    pub truncated: bool,
}

impl DecayOutcome {
    /// Whether the pass changed nothing at all.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.decayed == 0 && self.archived == 0
    }

    fn record(&mut self, row: DecayRow) {
        if self.details.len() < DETAIL_LIMIT {
            self.details.push(row);
        } else {
            self.truncated = true;
        }
    }
}

/// One row of the scan, as read.
struct Candidate {
    id: String,
    kind: MemoryKind,
    importance: f64,
    retrieval_count: i64,
    strength: f64,
    age_days: f64,
    expired: bool,
}

impl Database {
    /// Age the brain: apply decay, and archive what falls out of it.
    ///
    /// Pages forward by id, which is safe against the archiving below: the pass changes
    /// `status`, never `id`, so a row cannot be visited twice or skipped by the cursor
    /// moving under it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if a statement fails.
    pub fn decay(&self, scope: &Scope, dry_run: bool) -> Result<DecayOutcome> {
        let mut outcome = DecayOutcome {
            scanned: 0,
            decayed: 0,
            archived: 0,
            dry_run,
            details: Vec::new(),
            truncated: false,
        };
        // The clock is read once per pass rather than per row, so every row in one pass
        // is aged against the same instant. `age_days` in the query is still relative to
        // SQLite's `now` per statement, which is the same clock and within microseconds.
        let mut cursor = String::new();
        loop {
            let page = self.decay_page(scope, &cursor, DECAY_PAGE_SIZE)?;
            if page.is_empty() {
                break;
            }
            cursor.clone_from(&page[page.len() - 1].id);
            let full = page.len() == DECAY_PAGE_SIZE as usize;

            let transaction = if dry_run {
                None
            } else {
                Some(self.conn().unchecked_transaction()?)
            };

            for candidate in &page {
                outcome.scanned += 1;
                let next = effective_half_life_days(candidate.kind, candidate.retrieval_count);
                let weakness = next.map_or(f64::INFINITY, |half_life| {
                    decayed_strength(candidate.importance, candidate.age_days, half_life)
                });
                let archive = candidate.expired || weakness < STRENGTH_FLOOR;

                if archive {
                    outcome.archived += 1;
                    if let Some(transaction) = &transaction {
                        transaction.execute(
                            "UPDATE memories
                                SET status = 'archived',
                                    strength = ?2,
                                    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                              WHERE id = ?1",
                            rusqlite::params![candidate.id, STRENGTH_FLOOR],
                        )?;
                    }
                    outcome.record(DecayRow {
                        id: candidate.id.clone(),
                        kind: candidate.kind.as_str(),
                        age_days: candidate.age_days,
                        before: candidate.strength,
                        after: STRENGTH_FLOOR,
                        archived: true,
                        expired: candidate.expired,
                    });
                    continue;
                }

                // A live row whose strength did not move is not rewritten. On a corpus
                // that is mostly young memories this is most of the scan, and an UPDATE
                // per row would make the pass cost the corpus size rather than the
                // number of rows that actually aged.
                if (weakness - candidate.strength).abs() < STRENGTH_EPSILON {
                    continue;
                }
                outcome.decayed += 1;
                if let Some(transaction) = &transaction {
                    transaction.execute(
                        "UPDATE memories
                            SET strength = ?2,
                                updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                          WHERE id = ?1",
                        rusqlite::params![candidate.id, weakness],
                    )?;
                }
                outcome.record(DecayRow {
                    id: candidate.id.clone(),
                    kind: candidate.kind.as_str(),
                    age_days: candidate.age_days,
                    before: candidate.strength,
                    after: weakness,
                    archived: false,
                    expired: false,
                });
            }

            if let Some(transaction) = transaction {
                transaction.commit()?;
            }
            if !full {
                break;
            }
        }

        if !dry_run {
            self.set_last_decay()?;
        }
        Ok(outcome)
    }

    /// One page of the scan: everything that decays, in scope.
    ///
    /// `reference` is excluded in SQL as well as in Rust. The formula would refuse to
    /// touch them either way, and excluding them in the query is what keeps the pass's
    /// cost proportional to the part of the corpus that can age.
    fn decay_page(&self, scope: &Scope, cursor: &str, limit: u32) -> Result<Vec<Candidate>> {
        let mut sql = String::from(
            "SELECT id, kind, importance, retrieval_count, strength,
                    (julianday('now') - julianday(created_at)) AS age_days,
                    (expires_at IS NOT NULL AND julianday(expires_at) < julianday('now'))
                      AS expired
               FROM memories
              WHERE status = 'active'
                AND pinned = 0
                AND kind != 'reference'
                AND id > ?",
        );
        let mut values = vec![Sql::Text(cursor.to_owned())];
        if let Some(workspace) = scope.one() {
            sql.push_str(" AND workspace_id = ?");
            values.push(Sql::Text(workspace.to_owned()));
        }
        sql.push_str(" ORDER BY id LIMIT ?");
        values.push(Sql::Integer(i64::from(limit)));

        let mut statement = self.conn().prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(values.iter()), |row| {
            Ok(Candidate {
                id: row.get(0)?,
                kind: row.get(1)?,
                importance: row.get(2)?,
                retrieval_count: row.get(3)?,
                strength: row.get(4)?,
                age_days: row.get(5)?,
                expired: row.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Run one page of decay, at most once a day, and report what it found.
    ///
    /// Called by `add` and `search` so that a brain nobody explicitly maintains still
    /// ages. Returns `None` when it was not due, which is the common case and the cheap
    /// one: a single indexed lookup in `meta`.
    ///
    /// Never fails a caller: the commands that call this treat an error as a warning,
    /// because a read that could not round the maintenance corner is still a read that
    /// has to answer.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if a statement fails.
    pub fn decay_opportunistic(&self, scope: &Scope) -> Result<Option<DecayOutcome>> {
        if !self.decay_is_due() {
            return Ok(None);
        }
        let cursor = self.decay_cursor();
        let mut outcome = DecayOutcome {
            scanned: 0,
            decayed: 0,
            archived: 0,
            dry_run: false,
            details: Vec::new(),
            truncated: false,
        };
        let page = self.decay_page(scope, &cursor, DECAY_NUDGE_ROWS)?;
        let transaction = self.conn().unchecked_transaction()?;
        for candidate in &page {
            outcome.scanned += 1;
            let next = effective_half_life_days(candidate.kind, candidate.retrieval_count);
            let weakness = next.map_or(f64::INFINITY, |half_life| {
                decayed_strength(candidate.importance, candidate.age_days, half_life)
            });
            if candidate.expired || weakness < STRENGTH_FLOOR {
                outcome.archived += 1;
                transaction.execute(
                    "UPDATE memories
                        SET status = 'archived',
                            strength = ?2,
                            updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                      WHERE id = ?1",
                    rusqlite::params![candidate.id, STRENGTH_FLOOR],
                )?;
                continue;
            }
            if (weakness - candidate.strength).abs() < STRENGTH_EPSILON {
                continue;
            }
            outcome.decayed += 1;
            transaction.execute(
                "UPDATE memories
                    SET strength = ?2,
                        updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                  WHERE id = ?1",
                rusqlite::params![candidate.id, weakness],
            )?;
        }
        // The marker is written even when the page held nothing: "checked recently" is
        // what stops the next command from checking again. The cursor moves to the last
        // row this pass looked at, or back to the beginning when the page was short —
        // which is the last page of a lap. A pass that processed nothing at all keeps the
        // cursor where it was, so an empty `cursor` after a lap stays empty.
        let next_cursor = match page.last() {
            Some(last) if page.len() == DECAY_NUDGE_ROWS as usize => last.id.clone(),
            Some(_) => String::new(),
            None => cursor,
        };
        Self::set_meta(&transaction, META_DECAY_CURSOR, &next_cursor)?;
        Self::set_last_decay_in(&transaction)?;
        transaction.commit()?;
        Ok(Some(outcome))
    }

    /// Whether an opportunistic run is due.
    ///
    /// A missing marker means due, which is what makes a brain that has never decayed
    /// decay on its first command. A row that cannot be read is treated as due as well:
    /// the cost is one page of work, and the alternative — never decaying again because
    /// one `meta` row is malformed — is silent and permanent.
    ///
    /// No `Result`: there is no failure here that should stop the caller, since the only
    /// two outcomes are "run a page" and "do nothing", and both are fine.
    fn decay_is_due(&self) -> bool {
        let elapsed: Option<f64> = self
            .conn()
            .query_row(
                "SELECT julianday('now') - julianday(value) FROM meta WHERE key = ?1",
                [META_LAST_DECAY],
                |row| row.get(0),
            )
            .ok();
        elapsed.is_none_or(|days| days * 24.0 >= DECAY_INTERVAL_HOURS)
    }

    /// When decay last ran opportunistically, RFC 3339 UTC.
    ///
    /// Read by `doctor`; the column it comes from is the one thing in the schema that is
    /// neither content nor derived from content.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the read fails.
    pub fn last_decay(&self) -> Result<Option<String>> {
        let value = self
            .conn()
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [META_LAST_DECAY],
                |row| row.get(0),
            )
            .ok();
        Ok(value)
    }

    /// How long ago decay last ran, in days.
    ///
    /// Computed by SQLite for the same reason `age_days` is: one clock, and no date
    /// arithmetic in this crate. `None` means it has never run, which is not the same as
    /// "a long time ago" — a brand-new brain and an abandoned one both have no timestamp,
    /// and `doctor` says different things about them by looking at the corpus size.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the read fails.
    pub fn decay_age_days(&self) -> Result<Option<f64>> {
        let age: Option<f64> = self
            .conn()
            .query_row(
                "SELECT julianday('now') - julianday(value) FROM meta WHERE key = ?1",
                [META_LAST_DECAY],
                |row| row.get(0),
            )
            .ok();
        Ok(age)
    }

    fn set_last_decay(&self) -> Result<()> {
        Self::set_last_decay_in(self.conn())
    }

    /// Where the opportunistic walk resumes.
    ///
    /// An empty string means the beginning, which `id > ''` matches for every row — the
    /// only value that needs no special case on either side.
    ///
    /// No `Result`: a `meta` row that cannot be read is a corpus to start walking from
    /// the beginning, which is a decision this function can make without its caller.
    fn decay_cursor(&self) -> String {
        let value: Option<String> = self
            .conn()
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [META_DECAY_CURSOR],
                |row| row.get(0),
            )
            .ok();
        value.unwrap_or_default()
    }

    /// An associated function rather than a method: it is called both with the database's
    /// own connection, outside a transaction, and with a transaction's, which is not a
    /// `&self` and does not need one.
    fn set_last_decay_in(conn: &rusqlite::Connection) -> Result<()> {
        conn.execute(
            "INSERT INTO meta (key, value)
             VALUES (?1, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [META_LAST_DECAY],
        )?;
        Ok(())
    }

    /// Write one `meta` row, against any connection.
    fn set_meta(conn: &rusqlite::Connection, key: &str, value: &str) -> Result<()> {
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }
}

/// How much a strength has to move before the pass writes it.
///
/// Floating-point equality is not the test here: the formula is recomputed from
/// `created_at` on every pass, so a row nobody has touched since the last pass still has
/// a strength that would move in the twelfth decimal place. With an epsilon of `1e-9`
/// the check would therefore never fire and every pass would rewrite the whole corpus --
/// which is the cost this exists to avoid.
///
/// A fiftieth of the floor is the threshold because a change smaller than that cannot
/// change a decision: the only decisions strength feeds are "below the floor" and "where
/// does this sort", and `5e-4` is well under the gap between two rows anyone would
/// notice swapping places.
const STRENGTH_EPSILON: f64 = STRENGTH_FLOOR / 100.0;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NewMemory, Status};
    use crate::testing;

    /// A memory whose `created_at` is `days` in the past.
    fn aged(db: &Database, kind: MemoryKind, title: &str, days: f64, importance: f64) -> String {
        let id = db
            .add(
                &NewMemory::new(kind, title)
                    .body("body")
                    .importance(importance),
            )
            .expect("the fixture write must succeed")
            .id;
        db.conn()
            .execute(
                "UPDATE memories SET created_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?2)
                  WHERE id = ?1",
                rusqlite::params![id, format!("-{days} days")],
            )
            .expect("the fixture timestamp must move");
        id
    }

    #[test]
    fn the_formula_halves_at_every_half_life() {
        // Hand-computed, at age 0, 1 and 2 half-lives, for all three decaying kinds.
        for (kind, half_life) in [
            (MemoryKind::Episodic, 30.0),
            (MemoryKind::Procedural, 90.0),
            (MemoryKind::Semantic, 180.0),
        ] {
            let resolved = effective_half_life_days(kind, 0).unwrap();
            assert!((resolved - half_life).abs() < f64::EPSILON, "{kind:?}");
            assert!((decayed_strength(0.8, 0.0, resolved) - 0.8).abs() < 1e-12);
            assert!((decayed_strength(0.8, half_life, resolved) - 0.4).abs() < 1e-12);
            assert!((decayed_strength(0.8, half_life * 2.0, resolved) - 0.2).abs() < 1e-12);
        }
    }

    #[test]
    fn retrieval_stretches_the_half_life_up_to_the_cap() {
        // Ten retrievals is +100%, the cap; the eleventh must not add anything.
        assert!((effective_half_life_days(MemoryKind::Episodic, 0).unwrap() - 30.0).abs() < 1e-12);
        assert!((effective_half_life_days(MemoryKind::Episodic, 5).unwrap() - 45.0).abs() < 1e-12);
        assert!((effective_half_life_days(MemoryKind::Episodic, 10).unwrap() - 60.0).abs() < 1e-12);
        assert!(
            (effective_half_life_days(MemoryKind::Episodic, 400).unwrap() - 60.0).abs() < 1e-12
        );
        // A negative count is not a thing, and must not shorten a life.
        assert!((effective_half_life_days(MemoryKind::Episodic, -3).unwrap() - 30.0).abs() < 1e-12);
    }

    #[test]
    fn a_reference_memory_has_no_half_life_at_all() {
        assert_eq!(half_life_days(MemoryKind::Reference), None);
        assert_eq!(effective_half_life_days(MemoryKind::Reference, 0), None);
    }

    #[test]
    fn decay_weakens_a_row_by_the_formula() {
        let db = testing::db();
        let id = aged(&db, MemoryKind::Semantic, "Six months old", 180.0, 0.8);
        let outcome = db.decay(&Scope::All, false).unwrap();
        assert_eq!(outcome.scanned, 1);
        assert_eq!(outcome.decayed, 1);
        assert_eq!(outcome.archived, 0);
        let stored = db.get(&id).unwrap().strength;
        // The row's `created_at` was set from SQLite's own `now` minus days, so the age
        // the pass computes is 180 days plus the microseconds between the two statements.
        assert!((stored - 0.4).abs() < 0.01, "expected ~0.4, got {stored}");
        assert_eq!(db.get(&id).unwrap().status, Status::Active);
    }

    #[test]
    fn a_weak_memory_is_archived_at_exactly_the_floor() {
        let db = testing::db();
        // 0.5 importance at five semantic half-lives is 0.0156, below the 0.05 floor.
        // At three half-lives it would be 0.0625, which is above the floor -- worth
        // stating, because "an old memory is archived" is wrong: it takes a memory that
        // was unimportant *and* old.
        let id = aged(&db, MemoryKind::Semantic, "Ancient", 900.0, 0.5);
        let outcome = db.decay(&Scope::All, false).unwrap();
        assert_eq!(outcome.archived, 1);
        assert_eq!(outcome.decayed, 0);
        let stored = db.get(&id).unwrap();
        assert_eq!(stored.status, Status::Archived);
        assert!(
            (stored.strength - STRENGTH_FLOOR).abs() < f64::EPSILON,
            "an archived row keeps the floor, not its computed value: {}",
            stored.strength
        );
        assert_eq!(
            testing::fts_rows(&db).unwrap(),
            0,
            "archiving must remove the index row"
        );
    }

    #[test]
    fn a_retrieved_memory_outlives_the_same_memory_unretrieved() {
        // The whole point of the retrieve/decay loop: use is what keeps a memory alive.
        let db = testing::db();
        let used = aged(&db, MemoryKind::Episodic, "Used often", 30.0, 0.5);
        let ignored = aged(&db, MemoryKind::Episodic, "Never used", 30.0, 0.5);
        db.conn()
            .execute(
                "UPDATE memories SET retrieval_count = 10 WHERE id = ?1",
                [&used],
            )
            .unwrap();

        db.decay(&Scope::All, false).unwrap();
        let used_strength = db.get(&used).unwrap().strength;
        let ignored_strength = db.get(&ignored).unwrap().strength;
        // At one base half-life the ignored row is at exactly one half: 0.5 * 0.5 = 0.25.
        // The used row's half-life is doubled, so the same 30 days is *half* a half-life:
        // 0.5 * 0.5^0.5 = 0.354.
        assert!(
            (ignored_strength - 0.25).abs() < 0.001,
            "expected 0.25, got {ignored_strength}"
        );
        assert!(
            (used_strength - 0.3536).abs() < 0.001,
            "expected 0.354, got {used_strength}"
        );
    }

    #[test]
    fn a_pinned_memory_is_never_scanned_and_never_archived() {
        let db = testing::db();
        let id = aged(&db, MemoryKind::Semantic, "Pinned and ancient", 4000.0, 0.1);
        db.set_pinned(std::slice::from_ref(&id), true).unwrap();
        let outcome = db.decay(&Scope::All, false).unwrap();
        assert_eq!(outcome.scanned, 0, "a pinned row is not even read");
        assert_eq!(db.get(&id).unwrap().status, Status::Active);
    }

    #[test]
    fn a_reference_memory_is_never_scanned_and_never_archived() {
        let db = testing::db();
        let id = aged(&db, MemoryKind::Reference, "A file", 4000.0, 0.01);
        let outcome = db.decay(&Scope::All, false).unwrap();
        assert_eq!(outcome.scanned, 0);
        assert_eq!(db.get(&id).unwrap().status, Status::Active);
    }

    #[test]
    fn a_passed_deadline_archives_even_a_strong_memory() {
        // `expires_at` is what an `outdated` assessment sets. A fact the agent reported
        // as stale is not kept because it is important.
        let db = testing::db();
        let id = testing::add(&db, MemoryKind::Semantic, "Expired fact", "body");
        db.record_outcome(
            crate::lifecycle::TaskOutcome::Success,
            &[(id.clone(), crate::lifecycle::Assessment::Outdated)],
            &Scope::All,
        )
        .unwrap();
        let outcome = db.decay(&Scope::All, false).unwrap();
        assert_eq!(outcome.archived, 1);
        assert!(outcome.details[0].expired, "the reason is reported");
        assert_eq!(db.get(&id).unwrap().status, Status::Archived);
    }

    #[test]
    fn a_deadline_in_the_future_archives_nothing() {
        let db = testing::db();
        let id = testing::add(&db, MemoryKind::Semantic, "Still true", "body");
        db.conn()
            .execute(
                "UPDATE memories SET expires_at = '2099-01-01T00:00:00Z' WHERE id = ?1",
                [&id],
            )
            .unwrap();
        let outcome = db.decay(&Scope::All, false).unwrap();
        assert_eq!(outcome.archived, 0);
        assert_eq!(db.get(&id).unwrap().status, Status::Active);
    }

    #[test]
    fn a_dry_run_reports_the_truth_and_writes_nothing() {
        let db = testing::db();
        let old = aged(&db, MemoryKind::Semantic, "Ancient", 4000.0, 0.1);
        let young = aged(&db, MemoryKind::Semantic, "Fresh", 1.0, 0.9);
        let before_old = db.get(&old).unwrap();
        let before_young = db.get(&young).unwrap();

        let outcome = db.decay(&Scope::All, true).unwrap();
        assert!(outcome.dry_run);
        assert_eq!(outcome.scanned, 2);
        assert_eq!(outcome.archived, 1);
        assert_eq!(outcome.decayed, 1);
        assert_eq!(outcome.details.len(), 2);

        assert_eq!(db.get(&old).unwrap(), before_old, "nothing may move");
        assert_eq!(db.get(&young).unwrap(), before_young);
        assert_eq!(
            db.last_decay().unwrap(),
            None,
            "a dry run must not claim to have run"
        );
    }

    #[test]
    fn a_young_corpus_is_scanned_and_left_alone() {
        let db = testing::db();
        let id = aged(&db, MemoryKind::Semantic, "An hour old", 1.0 / 24.0, 1.0);
        let before = db.get(&id).unwrap().strength;
        let outcome = db.decay(&Scope::All, false).unwrap();
        assert_eq!(outcome.scanned, 1);
        assert_eq!(outcome.decayed, 0, "0.996 is not a change worth a write");
        assert!((db.get(&id).unwrap().strength - before).abs() < f64::EPSILON);
    }

    #[test]
    fn decay_respects_the_scope_it_is_given() {
        let db = testing::db();
        db.create_workspace("Other", crate::model::ReviewMode::Autonomous)
            .unwrap();
        let here = aged(&db, MemoryKind::Semantic, "Here", 4000.0, 0.1);
        let there = db
            .add(
                &NewMemory::new(MemoryKind::Semantic, "There")
                    .body("body")
                    .importance(0.1)
                    .workspace("other"),
            )
            .unwrap()
            .id;
        db.conn()
            .execute(
                "UPDATE memories SET created_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-4000 days')
                  WHERE id = ?1",
                [&there],
            )
            .unwrap();

        let outcome = db.decay(&Scope::One("default".to_owned()), false).unwrap();
        assert_eq!(outcome.scanned, 1);
        assert_eq!(db.get(&there).unwrap().status, Status::Active);
        assert_eq!(db.get(&here).unwrap().status, Status::Archived);
    }

    #[test]
    fn a_pass_larger_than_one_page_visits_everything_exactly_once() {
        let db = testing::db();
        for index in 0..(DECAY_PAGE_SIZE + 7) {
            aged(
                &db,
                MemoryKind::Semantic,
                &format!("Row {index}"),
                4000.0,
                0.1,
            );
        }
        let outcome = db.decay(&Scope::All, false).unwrap();
        assert_eq!(outcome.scanned, i64::from(DECAY_PAGE_SIZE) + 7);
        assert_eq!(outcome.archived, i64::from(DECAY_PAGE_SIZE) + 7);
        assert_eq!(
            db.count("SELECT count(*) FROM memories WHERE status = 'active'", &[])
                .unwrap(),
            0
        );
    }

    #[test]
    fn the_detail_list_is_bounded_and_says_that_it_is() {
        let db = testing::db();
        for index in 0..(DETAIL_LIMIT + 5) {
            aged(
                &db,
                MemoryKind::Semantic,
                &format!("Row {index}"),
                4000.0,
                0.1,
            );
        }
        let outcome = db.decay(&Scope::All, true).unwrap();
        assert_eq!(outcome.details.len(), DETAIL_LIMIT);
        assert!(outcome.truncated);
        assert_eq!(
            usize::try_from(outcome.archived).unwrap(),
            DETAIL_LIMIT + 5,
            "the count is exact even when the detail list is not"
        );
    }

    #[test]
    fn the_opportunistic_run_happens_once_a_day_and_says_so() {
        let db = testing::db();
        aged(&db, MemoryKind::Semantic, "Ancient", 4000.0, 0.1);

        let first = db
            .decay_opportunistic(&Scope::All)
            .unwrap()
            .expect("a brain that has never decayed must decay");
        assert_eq!(first.archived, 1);
        assert!(db.last_decay().unwrap().is_some());

        let second = db.decay_opportunistic(&Scope::All).unwrap();
        assert!(second.is_none(), "the second call in the same day is free");

        // Age the marker, and the next call is due again.
        db.conn()
            .execute(
                "UPDATE meta SET value = strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-25 hours')
                  WHERE key = ?1",
                [META_LAST_DECAY],
            )
            .unwrap();
        assert!(db.decay_opportunistic(&Scope::All).unwrap().is_some());
    }

    #[test]
    fn the_opportunistic_run_processes_at_most_one_nudge() {
        // The point of the limit: a maintenance pass bolted onto a search must not be
        // able to grow with the size of the corpus, and must not be able to cost a burst
        // the size of the one that was measured at 62 ms.
        let db = testing::db();
        for index in 0..(DECAY_NUDGE_ROWS + 50) {
            aged(
                &db,
                MemoryKind::Semantic,
                &format!("Row {index}"),
                4000.0,
                0.1,
            );
        }
        let outcome = db.decay_opportunistic(&Scope::All).unwrap().unwrap();
        assert_eq!(outcome.scanned, i64::from(DECAY_NUDGE_ROWS));
        let remaining = db
            .count("SELECT count(*) FROM memories WHERE status = 'active'", &[])
            .unwrap();
        assert_eq!(remaining, 50, "the rest is for the explicit command");
    }

    #[test]
    fn the_nudge_walks_the_corpus_rather_than_rewriting_the_same_rows() {
        // The defect the first version had: a page from the beginning of the id order is
        // the same page every day, so a hundred rows that never fall below the floor are
        // the only hundred rows the nudge ever touches.
        let db = testing::db();
        // Fresh rows, so nothing is archived and the same page would be selected forever
        // if the cursor did not move.
        for index in 0..(DECAY_NUDGE_ROWS + 40) {
            aged(
                &db,
                MemoryKind::Semantic,
                &format!("Row {index:04}"),
                1.0,
                0.9,
            );
        }

        let first = db.decay_opportunistic(&Scope::All).unwrap().unwrap();
        assert_eq!(first.scanned, i64::from(DECAY_NUDGE_ROWS));
        // Clear only the timestamp: the cursor is what is under test.
        db.conn()
            .execute("DELETE FROM meta WHERE key = ?1", [META_LAST_DECAY])
            .unwrap();
        let second = db.decay_opportunistic(&Scope::All).unwrap().unwrap();
        assert_eq!(
            second.scanned, 40,
            "the second pass resumes after the first"
        );
        let first_ids: Vec<&str> = first.details.iter().map(|row| row.id.as_str()).collect();
        let second_ids: Vec<&str> = second.details.iter().map(|row| row.id.as_str()).collect();
        assert!(
            first_ids.iter().all(|id| !second_ids.contains(id)),
            "the two pages must not overlap, or the walk rewrote the same rows"
        );

        // A short page ends the lap, so the next pass starts again at the beginning.
        db.conn()
            .execute("DELETE FROM meta WHERE key = ?1", [META_LAST_DECAY])
            .unwrap();
        let third = db.decay_opportunistic(&Scope::All).unwrap().unwrap();
        assert_eq!(third.scanned, i64::from(DECAY_NUDGE_ROWS), "a new lap");
    }

    #[test]
    fn the_explicit_command_finishes_what_the_opportunistic_pass_started() {
        let db = testing::db();
        for index in 0..(DECAY_NUDGE_ROWS + 50) {
            aged(
                &db,
                MemoryKind::Semantic,
                &format!("Row {index}"),
                4000.0,
                0.1,
            );
        }
        db.decay_opportunistic(&Scope::All).unwrap();
        let outcome = db.decay(&Scope::All, false).unwrap();
        assert_eq!(outcome.archived, 50);
        assert_eq!(
            db.count("SELECT count(*) FROM memories WHERE status = 'active'", &[])
                .unwrap(),
            0
        );
    }

    #[test]
    fn a_row_with_no_kind_to_decay_is_left_out_of_the_scan_entirely() {
        let db = testing::db();
        aged(&db, MemoryKind::Reference, "A file", 4000.0, 0.01);
        aged(&db, MemoryKind::Semantic, "A claim", 1.0, 0.9);
        let outcome = db.decay(&Scope::All, false).unwrap();
        assert_eq!(outcome.scanned, 1, "only the semantic row is a candidate");
    }
}
