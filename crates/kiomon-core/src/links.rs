// SPDX-License-Identifier: Apache-2.0

//! The knowledge graph: typed edges, the auto-link pass, and the one-hop read.
//!
//! # What an edge is
//!
//! A row in `memory_links`: a source, a target, a relation, and three pieces of
//! provenance — who wrote it (`origin`), how sure it was (`score`), and when (`created_at`).
//! The schema was in `0001_init.sql` from WP1 and nothing wrote a `related_to` until WP5;
//! `derived_from` was the one relation with a writer, because `add --ref` needs somewhere
//! to put a citation.
//!
//! # The two rules that make the graph readable
//!
//! **Symmetric relations are stored once.** `A related_to B` and `B related_to A` are the
//! same statement, and the table's `UNIQUE (source_id, target_id, relation)` cannot prevent
//! both rows existing — the constraint does not know that the reverse pair is the same
//! edge. So a symmetric edge is canonicalised before it is written: the lexicographically
//! smaller id is the source. ULIDs sort by creation time, so the rule reads as "the older
//! memory is the source", and it makes the write idempotent from either end. `relate` reads
//! both columns, so the canonicalisation is invisible to a caller and only visible in the
//! stored row.
//!
//! **A draft has no edges, in either direction.** The review inbox's promise is that a
//! memory waiting for approval cannot be found by *any* read path, and an edge is a read
//! path: `relate` on the parent would print the draft's title. So writes refuse an edge that
//! touches a draft, and the read filters `pending_approval` out as well — the filter is what
//! makes the promise structural rather than a property of every writer remembering.
//!
//! # Auto-linking
//!
//! §5.2 of the plan: take the new memory's title and the first kilobyte of its body, run the
//! lexical ladder, and link the closest neighbours. No model, one query, and it runs inside
//! the write's transaction so a failure is the write's failure and not a second thing that
//! can go wrong later.
//!
//! Three bounds keep it from being a way to grow a graph nobody asked for: [`AUTO_LINK_TOP_N`]
//! neighbours at most, [`AUTO_LINK_MIN_SCORE`] the minimum share of the best match's score,
//! and the memory's own workspace — a graph edge across workspaces would be a leak of a
//! title into a scope the caller did not ask about.

use rusqlite::{Connection, OptionalExtension, params};

use crate::db::Database;
use crate::db::now;
use crate::error::{Error, Result};
use crate::model::{
    EdgeDirection, LinkOutcome, Neighbour, NewLink, Relation, Scope, SearchQuery, Status,
    UnlinkOutcome,
};
use crate::search::{Ladder, climb};

/// How many neighbours an auto-link pass writes at most.
///
/// Five, from the plan's §5.2. The number is a product decision rather than a technical
/// one: a graph where every memory links to twenty others is a graph whose edges mean
/// nothing, and `relate` on a memory with five good neighbours is more useful than one
/// with fifty plausible ones.
pub const AUTO_LINK_TOP_N: u32 = 5;

/// How many candidates the auto-link ladder is allowed to rank before the top five are
/// chosen from it.
///
/// More than five because the score floor rejects some — a ladder that only ever looked at
/// five rows could return nothing while the sixth row was an excellent neighbour.
pub const AUTO_LINK_CANDIDATES: u32 = 20;

/// How much of the best match's relevance a neighbour must keep to be linked.
///
/// The score is normalised against the best candidate in the same rung, so this is a
/// *relative* floor: at least half as good as the closest match. A relative floor is the
/// only kind that needs no tuning, because `bm25` is not comparable across corpora — an
/// absolute one would link everything in a small brain with distinctive words and nothing
/// in a large one with common ones.
pub const AUTO_LINK_MIN_SCORE: f64 = 0.5;

/// How much of the body an auto-link pass reads.
///
/// A kilobyte, from §5.2. The body can be a megabyte, and ranking a whole document against
/// the index on every write would make `add` cost grow with the size of what it stored
/// rather than with the corpus.
pub const AUTO_LINK_BODY_BYTES: usize = 1024;

/// How many terms of that text become the query.
///
/// The plan's §5.2 asked for the title plus a kilobyte of body and estimated the pass at
/// about a millisecond. Measured, it was two orders of magnitude out: a kilobyte is around
/// a hundred and fifty words, rung 3 of the ladder is an `OR` across all of them, and FTS5
/// has to score and sort every row in the corpus that shares *any* of them — 60 ms per
/// write on a 1 000-memory corpus, which would have made the `add` budget unreachable and
/// the corpus generator unusable. So the text is still the source, and the first twelve
/// distinct terms of it are the query: the title leads (it is the most distinctive and it
/// comes first), the opening of the body fills the rest, and `bm25` over twelve terms is
/// still a similarity signal — beyond a dozen, a bag-of-words score saturates into "how
/// many of these common words does the row have".
pub const AUTO_LINK_MAX_TERMS: usize = 12;

impl Database {
    /// Write one typed edge.
    ///
    /// Idempotent: linking twice is not an error and reports `created: false`. A symmetric
    /// relation is canonicalised, so both orders are the same row — see the module comment.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a self-edge, a score outside `0.0..=1.0`, or a blank id.
    /// [`Error::NotFound`] when either id does not exist, or exists but is waiting in the
    /// review inbox: an edge to a draft is an edge to something no read path may show, and
    /// saying so is better than writing a row that half the reads must then hide.
    pub fn link(&self, new: &NewLink) -> Result<LinkOutcome> {
        if let Some(score) = new.score
            && !(0.0..=1.0).contains(&score)
        {
            return Err(Error::Invalid(format!(
                "`--score` is {score}; an edge's similarity is between 0.0 and 1.0"
            )));
        }
        // Checked before the ids are trimmed and looked up, because it is a property of the
        // request: "link this to itself" is malformed whatever the memory is.
        if new.source_id.trim() == new.target_id.trim() {
            return Err(Error::Invalid(format!(
                "a memory cannot be linked to itself (`{}`)",
                new.source_id.trim()
            )));
        }
        let (source, target) = canonical_pair(&new.source_id, &new.target_id, new.relation);

        let transaction = self.conn().unchecked_transaction()?;
        // Both ends are checked before the insert, in the same transaction, so a failure
        // leaves no half-written edge behind.
        for id in [&source, &target] {
            require_linkable(&transaction, id)?;
        }
        // `ON CONFLICT DO NOTHING` and not `INSERT OR IGNORE`: `IGNORE` swallows *every*
        // constraint failure, and this statement found that out the hard way — a relation
        // the CHECK constraint did not list was reported as "already there" instead of as
        // an error. The conflict clause ignores a duplicate edge and lets a schema
        // violation surface.
        let created = transaction.execute(
            "INSERT INTO memory_links
                (source_id, target_id, relation, directed, score, origin, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'manual', strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
             ON CONFLICT DO NOTHING",
            params![
                source,
                target,
                new.relation.as_str(),
                i64::from(new.relation.is_directed()),
                new.score,
            ],
        )?;
        transaction.commit()?;
        Ok(LinkOutcome {
            source_id: source,
            target_id: target,
            relation: new.relation,
            created: created == 1,
        })
    }

    /// Remove edges between two memories.
    ///
    /// What is removed is the pair, in either direction: an undirected edge is stored once,
    /// and a caller that asks to unlink two memories means the relationship and not the
    /// row. `relation` narrows it to one kind of relationship, which is how a caller
    /// retracts a claim (`contradicts`) while keeping the citation (`derived_from`).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a blank id, [`Error::Database`] if a statement fails.
    pub fn unlink(
        &self,
        source: &str,
        target: &str,
        relation: Option<Relation>,
    ) -> Result<UnlinkOutcome> {
        for id in [source, target] {
            if id.trim().is_empty() {
                return Err(Error::Invalid(
                    "an unlink needs two real ids; one of them is blank".to_owned(),
                ));
            }
        }
        let transaction = self.conn().unchecked_transaction()?;
        let mut removed = 0;
        // Both orders, because `source`/`target` at the command line are two memories and
        // not two ends of a row. For a directed relation this deletes both directions if
        // both exist, which is what "unlink these two, by this relation" asks for.
        for (from, to) in [(source, target), (target, source)] {
            removed += match relation {
                Some(relation) => transaction.execute(
                    "DELETE FROM memory_links
                      WHERE source_id = ?1 AND target_id = ?2 AND relation = ?3",
                    params![from, to, relation.as_str()],
                )?,
                None => transaction.execute(
                    "DELETE FROM memory_links WHERE source_id = ?1 AND target_id = ?2",
                    params![from, to],
                )?,
            };
        }
        transaction.commit()?;
        Ok(UnlinkOutcome { removed })
    }

    /// The one-hop neighbourhood of a memory.
    ///
    /// Two statements rather than one `(source_id = ? OR target_id = ?)`: that `OR` spans
    /// two columns, so neither index can serve it and the query degrades to a scan of the
    /// whole edge table. Each half below is a seek on `idx_link_source` or `idx_link_target`
    /// — the same reasoning, and the same shape, as the packet's typed-edge read.
    ///
    /// `depth` is checked and refused above one rather than quietly ignored: a walk that
    /// returns the first hop of a request for three is a caller's worth case, and the plan
    /// specifies a one-hop read.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown id — a memory with no neighbours and a memory that
    /// does not exist are different answers, and the second is exit `2`.
    /// [`Error::Invalid`] for a `depth` this release cannot walk.
    pub fn relations(&self, id: &str, relations: &[Relation], depth: u8) -> Result<Vec<Neighbour>> {
        if depth != 1 {
            return Err(Error::Invalid(format!(
                "`--depth {depth}`: this release walks one hop. A multi-hop walk is a path \
                 search over the graph, and it is not in v1"
            )));
        }
        // The row is looked up rather than joined, so that "no such memory" is reported as
        // such even when it has no edges at all — a join would return nothing either way.
        let known: Option<Status> = self
            .conn()
            .query_row("SELECT status FROM memories WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        if known.is_none() {
            return Err(Error::NotFound(format!("no memory with id `{id}`")));
        }

        let filter = relation_filter(relations);
        let mut found: Vec<Neighbour> = Vec::new();
        for (column, neighbour, direction) in [
            ("source_id", "target_id", EdgeDirection::Out),
            ("target_id", "source_id", EdgeDirection::In),
        ] {
            // The two interpolated identifiers are literals from the array above and the
            // filter is built from `Relation::as_str`; the ids are bound. The store's
            // module comment states the rule this follows.
            let sql = format!(
                "SELECT l.relation, l.directed, l.score, l.origin, l.created_at,
                        m.id, m.kind, m.title, m.excerpt, m.status
                   FROM memory_links l
                   JOIN memories m ON m.id = l.{neighbour}
                  WHERE l.{column} = ?1
                    AND l.valid_to IS NULL
                    AND m.status <> 'pending_approval'{filter}"
            );
            let mut statement = self.conn().prepare(&sql)?;
            let rows = statement.query_map([id], |row| {
                // A relation this build does not know is a row somebody edited by hand, and
                // it fails the read rather than being skipped: a graph that silently omits
                // the edges it cannot parse is a graph a caller cannot trust to be complete.
                let relation: Relation =
                    row.get::<_, String>(0)?.parse().map_err(|error: Error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?;
                // The stored direction is only a fact for a directed relation. For
                // `related_to` it is a canonicalisation, and reporting it would tell the
                // caller that the older memory "is related out to" the newer one.
                let directed: i64 = row.get(1)?;
                Ok(Neighbour {
                    memory_id: row.get(5)?,
                    relation,
                    direction: (directed == 1).then_some(direction),
                    score: row.get(2)?,
                    origin: row.get(3)?,
                    created_at: row.get(4)?,
                    kind: row.get(6)?,
                    title: row.get(7)?,
                    excerpt: row.get(8)?,
                    status: row.get(9)?,
                })
            })?;
            for row in rows {
                found.push(row?);
            }
        }

        // Sorted here rather than in SQL: the two statements each come back in rowid order,
        // and `rowid` is the table's business, not the caller's. Grouped by relation so a
        // caller reading the human output sees the strong claims before the associations.
        found.sort_by(|a, b| {
            a.relation
                .as_str()
                .cmp(b.relation.as_str())
                .then_with(|| a.memory_id.cmp(&b.memory_id))
                .then_with(|| {
                    a.direction
                        .map(EdgeDirection::as_str)
                        .cmp(&b.direction.map(EdgeDirection::as_str))
                })
        });
        Ok(found)
    }
}

/// Wire `derived_from` edges for ids that exist.
///
/// Returns a warning for each id that does not, rather than failing: an agent citing a
/// memory that has since been forgotten should still get its new memory stored, and a
/// write that fails because of an earlier deletion is a worse outcome than a missing edge
/// with a note about it.
pub(crate) fn link_derived_from(
    conn: &Connection,
    source_id: &str,
    targets: &[String],
) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    let mut insert = conn.prepare(
        "INSERT INTO memory_links
            (source_id, target_id, relation, directed, origin, created_at)
         VALUES (?1, ?2, ?3, 1, 'agent', strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
         ON CONFLICT DO NOTHING",
    )?;
    let mut exists =
        conn.prepare("SELECT 1 FROM memories WHERE id = ?1 AND status <> 'pending_approval'")?;
    for target in targets {
        let trimmed = target.trim();
        if trimmed.is_empty() || trimmed == source_id {
            continue;
        }
        let found: Option<i64> = exists.query_row([trimmed], |row| row.get(0)).ok();
        if found.is_none() {
            warnings.push(format!(
                "no linkable memory with id `{trimmed}` to derive from; skipping that link"
            ));
            continue;
        }
        insert.execute(params![source_id, trimmed, Relation::DerivedFrom])?;
    }
    Ok(warnings)
}

/// Link a memory to its closest lexical neighbours, as `related_to` edges.
///
/// Returns how many edges were written. A call that writes none is normal: a memory whose
/// words appear nowhere else has no neighbours, and a corpus that is one memory has no
/// graph.
///
/// Failures are the caller's business and not this function's. It runs inside the write's
/// transaction, so an error here is the error of the write it belongs to, and the caller
/// reports it as a warning rather than a failed save — see `store::insert_one`.
///
/// # Errors
///
/// Returns [`Error::Database`] if the ladder's query or an insert fails.
pub(crate) fn auto_link(
    conn: &Connection,
    id: &str,
    workspace_id: &str,
    title: &str,
    body: &str,
) -> Result<usize> {
    let text = neighbour_text(title, body);
    let Some(ladder) = Ladder::new(&text) else {
        return Ok(0);
    };
    let query = SearchQuery {
        text: text.clone(),
        scope: Scope::One(workspace_id.to_owned()),
        kinds: Vec::new(),
        ..SearchQuery::default()
    };

    // The exclusion is the whole reason `climb` takes one: this memory is the closest match
    // to its own words, so without it a slot of the five-edge budget would be spent on a
    // self-edge that the write path refuses anyway.
    let (_, ranked) = climb(conn, &query, &ladder, AUTO_LINK_CANDIDATES, Some(id))?;
    let Some(best) = ranked.first() else {
        return Ok(0);
    };
    // `bm25` is negative and best-first. A non-negative best means the index found the
    // terms but scored them at zero, which leaves nothing to be a *relative* fraction of;
    // writing the top five anyway would be inventing a similarity the ranking did not
    // express.
    if best.score >= 0.0 {
        return Ok(0);
    }

    let written_at = now(conn)?;
    let mut insert = conn.prepare(
        "INSERT INTO memory_links
            (source_id, target_id, relation, directed, score, origin, created_at)
         VALUES (?1, ?2, 'related_to', 0, ?3, 'auto', ?4)
         ON CONFLICT DO NOTHING",
    )?;
    let mut written = 0;
    let mut taken = 0;
    for candidate in &ranked {
        #[allow(clippy::cast_precision_loss)] // a candidate window, never near 2^53
        let normalised = candidate.score / best.score;
        if normalised < AUTO_LINK_MIN_SCORE {
            continue;
        }
        let (source, target) = canonical_pair(id, &candidate.id, Relation::RelatedTo);
        written += insert.execute(params![source, target, normalised, written_at])?;
        taken += 1;
        if taken >= AUTO_LINK_TOP_N as usize {
            break;
        }
    }
    Ok(written)
}

/// The text an auto-link pass ranks: the title, and the head of the body, cut to the first
/// [`AUTO_LINK_MAX_TERMS`] distinct terms.
///
/// The excerpt is deliberately not in it. It is usually a sentence from the body, so
/// including it would weight that sentence twice — and the ranking this feeds already
/// weights the title above the body, which is the part of §5.2 that matters.
fn neighbour_text(title: &str, body: &str) -> String {
    let head = head_chars(body, AUTO_LINK_BODY_BYTES);
    let mut seen: Vec<String> = Vec::with_capacity(AUTO_LINK_MAX_TERMS);
    for word in title.split_whitespace().chain(head.split_whitespace()) {
        let folded = word.to_lowercase();
        if seen.contains(&folded) {
            continue;
        }
        seen.push(folded);
        if seen.len() == AUTO_LINK_MAX_TERMS {
            break;
        }
    }
    seen.join(" ")
}

/// The first `max` bytes of `text`, cut at a character boundary.
///
/// `&text[..max]` panics when `max` lands inside a multi-byte character, and a memory whose
/// kilobyte boundary falls in a Japanese word is a memory an auto-link pass must not panic
/// on. Bytes are the budget because the index limit is a byte budget; the boundary is
/// respected because the alternative is a crash on exactly the corpus this project cares
/// about working.
fn head_chars(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The canonical `(source, target)` for an edge.
///
/// Directed relations are stored as given; symmetric ones are stored with the smaller id
/// first, so that both directions of the same statement are one row. See the module comment.
fn canonical_pair(source: &str, target: &str, relation: Relation) -> (String, String) {
    if relation.is_directed() || source <= target {
        (source.to_owned(), target.to_owned())
    } else {
        (target.to_owned(), source.to_owned())
    }
}

/// `AND l.relation IN (…)`, from the caller's filter.
///
/// Empty when nothing was asked for, so the unfiltered read is one statement shape rather
/// than two. The values are [`Relation::as_str`] literals with no user input near them —
/// the same exception the store's `kind_filter` documents.
fn relation_filter(relations: &[Relation]) -> String {
    if relations.is_empty() {
        return String::new();
    }
    let list: Vec<&str> = relations.iter().map(|relation| relation.as_str()).collect();
    format!(" AND l.relation IN ('{}')", list.join("', '"))
}

/// Refuse an id that cannot be an end of an edge.
///
/// Missing and draft are both exit `2` and are not the same message: one says the memory is
/// gone, the other says it is waiting — and the second is the review inbox's promise, which
/// an edge would break.
fn require_linkable(conn: &Connection, id: &str) -> Result<()> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        return Err(Error::Invalid(
            "a link needs two real ids; one of them is blank".to_owned(),
        ));
    }
    let status: Option<Status> = conn
        .query_row(
            "SELECT status FROM memories WHERE id = ?1",
            [trimmed],
            |row| row.get(0),
        )
        .optional()?;
    match status {
        None => Err(Error::NotFound(format!("no memory with id `{trimmed}`"))),
        Some(Status::PendingApproval) => Err(Error::NotFound(format!(
            "`{trimmed}` is waiting in the review inbox, so it is not in the graph: approve \
             it first, and `approve` writes the edges it was staged with"
        ))),
        Some(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    //! The graph's own rules: what an edge means, what the auto-link pass may write, and
    //! what a one-hop read is allowed to show.
    //!
    //! Fixtures go through the public write path wherever possible, so a test that asserts
    //! an edge exists is also a test that the writer under it wrote one. The exceptions are
    //! marked: a draft neighbour and a closed edge are states no verb can produce, and a
    //! test that wants to prove the *reader* hides them has to reach past the writers with
    //! raw SQL — which is exactly the point of such a test.

    use super::*;
    use crate::model::{MemoryKind, NewMemory, Relation};
    use crate::testing;

    fn link_to(db: &Database, source: &str, target: &str, relation: Relation) -> LinkOutcome {
        db.link(&NewLink {
            source_id: source.to_owned(),
            target_id: target.to_owned(),
            relation,
            score: None,
        })
        .expect("the fixture link must succeed")
    }

    fn related(db: &Database, id: &str) -> Vec<Neighbour> {
        db.relations(id, &[], 1)
            .expect("the fixture neighbourhood must read")
    }

    fn edge_count(db: &Database) -> i64 {
        db.conn()
            .query_row("SELECT count(*) FROM memory_links", [], |row| row.get(0))
            .expect("the edge count must read")
    }

    #[test]
    fn a_directed_edge_is_stored_as_given_and_read_from_both_ends() {
        let db = testing::db();
        let derived = testing::add(&db, MemoryKind::Semantic, "derived", "a");
        let origin = testing::add(&db, MemoryKind::Semantic, "origin", "b");

        let outcome = link_to(&db, &derived, &origin, Relation::DerivedFrom);
        assert!(outcome.created);
        assert_eq!(
            (outcome.source_id.as_str(), outcome.target_id.as_str()),
            (derived.as_str(), origin.as_str()),
            "a directed edge is stored the way it was given"
        );

        let from_source = related(&db, &derived);
        assert_eq!(from_source.len(), 1);
        assert_eq!(from_source[0].memory_id, origin);
        assert_eq!(from_source[0].direction, Some(EdgeDirection::Out));

        let from_target = related(&db, &origin);
        assert_eq!(from_target.len(), 1);
        assert_eq!(from_target[0].memory_id, derived);
        assert_eq!(from_target[0].direction, Some(EdgeDirection::In));
    }

    #[test]
    fn a_symmetric_edge_is_one_row_whichever_end_it_is_written_from() {
        let db = testing::db();
        let first = testing::add(&db, MemoryKind::Semantic, "first", "a");
        let second = testing::add(&db, MemoryKind::Semantic, "second", "b");

        // Written from the later memory towards the earlier one, which is the order the
        // canonicalisation has to reverse.
        let outcome = link_to(&db, &second, &first, Relation::RelatedTo);
        assert_eq!(
            (outcome.source_id.as_str(), outcome.target_id.as_str()),
            (first.as_str(), second.as_str()),
            "the smaller id — the older memory — is the stored source"
        );

        // The other order is the same edge, not a second one.
        let again = link_to(&db, &first, &second, Relation::RelatedTo);
        assert!(!again.created, "the reverse pair is the same statement");
        assert_eq!(edge_count(&db), 1);

        // And both ends see it, with no direction, because there is not one.
        for id in [&first, &second] {
            let neighbours = related(&db, id);
            assert_eq!(neighbours.len(), 1, "{id}");
            assert_eq!(neighbours[0].direction, None);
        }
    }

    #[test]
    fn a_self_edge_is_refused_rather_than_stored() {
        let db = testing::db();
        let id = testing::add(&db, MemoryKind::Semantic, "alone", "a");

        let error = db
            .link(&NewLink {
                source_id: id.clone(),
                target_id: id,
                relation: Relation::RelatedTo,
                score: None,
            })
            .expect_err("a memory is not its own neighbour");
        assert!(matches!(error, Error::Invalid(_)), "{error}");
        assert_eq!(edge_count(&db), 0, "nothing was written");
    }

    #[test]
    fn an_edge_to_a_missing_memory_is_refused_and_writes_nothing() {
        let db = testing::db();
        let id = testing::add(&db, MemoryKind::Semantic, "here", "a");

        let error = db
            .link(&NewLink {
                source_id: id,
                target_id: "k_01ZZZZZZZZZZZZZZZZZZZZZZZZ".to_owned(),
                relation: Relation::Supports,
                score: None,
            })
            .expect_err("there is nothing there to link to");
        assert!(matches!(error, Error::NotFound(_)), "{error}");
        assert_eq!(edge_count(&db), 0, "the other end was not half-written");
    }

    #[test]
    fn an_edge_touching_the_review_inbox_is_refused() {
        let db = testing::db();
        let active = testing::add(&db, MemoryKind::Semantic, "approved", "a");
        let draft = db
            .add(
                &NewMemory::new(MemoryKind::Semantic, "a draft")
                    .body("not yet")
                    .source("agent_reflection")
                    .confidence(0.4),
            )
            .expect("a low-confidence write is stored as a draft")
            .id;
        assert_eq!(
            db.get(&draft).expect("readable by id").status,
            Status::PendingApproval
        );

        // Either end, because either one would leak the draft's title through `relate`.
        for (source, target) in [(&active, &draft), (&draft, &active)] {
            let error = db
                .link(&NewLink {
                    source_id: source.clone(),
                    target_id: target.clone(),
                    relation: Relation::RelatedTo,
                    score: None,
                })
                .expect_err("the inbox is not in the graph");
            assert!(matches!(error, Error::NotFound(_)), "{error}");
            assert!(
                error.to_string().contains("review inbox"),
                "the message says which kind of missing this is: {error}"
            );
        }
        assert_eq!(edge_count(&db), 0);
    }

    #[test]
    fn a_score_outside_the_unit_range_is_refused() {
        let db = testing::db();
        let a = testing::add(&db, MemoryKind::Semantic, "a", "a");
        let b = testing::add(&db, MemoryKind::Semantic, "b", "b");
        let error = db
            .link(&NewLink {
                source_id: a,
                target_id: b,
                relation: Relation::RelatedTo,
                score: Some(1.5),
            })
            .expect_err("similarity is a fraction");
        assert!(matches!(error, Error::Invalid(_)), "{error}");
    }

    #[test]
    fn a_relation_this_build_cannot_write_is_a_schema_error_and_not_a_silent_skip() {
        // `INSERT OR IGNORE` swallowed this once: the CHECK constraint did not list
        // `supersedes`, so the insert was reported as "already there". The constraint is
        // the schema's business, and a relation the schema does not accept must fail
        // loudly — which is why the writers use `ON CONFLICT DO NOTHING` and why this test
        // writes past the enum with raw SQL.
        let db = testing::db();
        let a = testing::add(&db, MemoryKind::Semantic, "a", "a");
        let b = testing::add(&db, MemoryKind::Semantic, "b", "b");
        let error = db
            .conn()
            .execute(
                "INSERT INTO memory_links
                    (source_id, target_id, relation, directed, origin, created_at)
                 VALUES (?1, ?2, 'resembles', 1, 'manual', '2026-01-01T00:00:00Z')",
                params![a, b],
            )
            .expect_err("an unknown relation violates the CHECK constraint");
        assert!(error.to_string().contains("CHECK"), "{error}");

        // And the one WP5 added is accepted now, which is what migration 0004 is for.
        assert!(link_to(&db, &a, &b, Relation::Supersedes).created);
    }

    #[test]
    fn auto_linking_writes_at_most_five_edges_and_never_a_self_edge() {
        let db = testing::db();
        // One topic, ten memories: every one of them is a plausible neighbour of the ninth.
        let first = testing::add(
            &db,
            MemoryKind::Semantic,
            "the deploy pipeline runs migrations before it ships",
            "The deploy pipeline runs migrations before it ships, so a deploy cannot break \
             the schema.",
        );
        for index in 1..10 {
            testing::add(
                &db,
                MemoryKind::Semantic,
                &format!("deploy note {index} about migrations and the pipeline"),
                "The deploy pipeline runs migrations before it ships.",
            );
        }

        let neighbours = related(&db, &first);
        assert!(!neighbours.is_empty(), "a shared topic is a neighbour");
        assert!(
            neighbours.len() <= AUTO_LINK_TOP_N as usize,
            "the cap is the cap: {} edges",
            neighbours.len()
        );
        assert!(
            neighbours.iter().all(|n| n.memory_id != first),
            "no self-edge"
        );
        assert!(
            neighbours.iter().all(|n| n.relation == Relation::RelatedTo
                && n.origin == "auto"
                && n.direction.is_none()
                && n.score.is_some_and(|score| score > 0.0)),
            "auto-edges are symmetric, scored, and attributed: {neighbours:?}"
        );

        // The cap holds on the writing end too: ten candidates, five rows.
        assert!(
            edge_count(&db) <= 5 * 10,
            "{} edges for 10 memories",
            edge_count(&db)
        );
    }

    #[test]
    fn auto_linking_cannot_see_out_of_its_own_workspace() {
        let db = testing::db();
        db.create_workspace("other", crate::model::ReviewMode::Autonomous)
            .expect("a second workspace must be creatable");
        let mine = testing::add(
            &db,
            MemoryKind::Semantic,
            "the deploy pipeline runs migrations",
            "Nothing else shares these words.",
        );
        db.add(
            &NewMemory::new(MemoryKind::Semantic, "the deploy pipeline runs migrations")
                .body("Identical words in another workspace.")
                .workspace("other"),
        )
        .expect("a write in the second workspace must succeed");

        assert!(
            related(&db, &mine).is_empty(),
            "a workspace is a boundary, and a graph edge across it would be a leak"
        );
    }

    #[test]
    fn auto_linking_leaves_a_memory_with_no_neighbours_alone() {
        let db = testing::db();
        // Vocabulary nothing else uses, which is what the corpus generator promises of its
        // planted terms and what a real brain mostly contains.
        let lonely = testing::add(
            &db,
            MemoryKind::Semantic,
            "zqxwv kfmpb vrytlc",
            "gpfn ydwq zmbx ktlv nsch",
        );
        assert_eq!(related(&db, &lonely).len(), 0);
        assert_eq!(edge_count(&db), 0);
    }

    #[test]
    fn an_import_restores_rows_without_inventing_edges() {
        // An import is a restore, not an activation: the file it reads has no edges in it
        // (the format says so), and a bulk load that auto-linked every row would invent a
        // graph the file did not carry.
        let db = testing::db();
        let rows: Vec<crate::import::ImportRow> = (0..3)
            .map(|index| {
                // Built the way the importer builds one: through a JSON record, so the
                // fixture cannot describe a row the parser would refuse.
                let mut record: crate::import::ExportRecord = serde_json::from_str(
                    r#"{"kind":"semantic","title":"placeholder","workspace_id":"default"}"#,
                )
                .expect("the placeholder record");
                record.title = format!("the deploy pipeline runs migrations {index}");
                record.body = "The deploy pipeline runs migrations before it ships.".to_owned();
                record.into_row(None).expect("a valid row")
            })
            .collect();
        db.import_batch(&rows, false)
            .expect("the import must write");
        assert_eq!(edge_count(&db), 0, "an import writes no edges");
    }

    #[test]
    fn unlink_removes_the_pair_from_either_end() {
        let db = testing::db();
        let a = testing::add(&db, MemoryKind::Semantic, "a", "a");
        let b = testing::add(&db, MemoryKind::Semantic, "b", "b");
        link_to(&db, &a, &b, Relation::RelatedTo);
        assert_eq!(edge_count(&db), 1);

        // Asked for the other way round, which is how a caller that read the pair off
        // `relate` would ask.
        let outcome = db.unlink(&b, &a, None).expect("the delete must run");
        assert_eq!(outcome.removed, 1);
        assert_eq!(edge_count(&db), 0);

        // And nothing is a report, not an error: the CLI maps this to exit 2.
        assert_eq!(
            db.unlink(&a, &b, None)
                .expect("a second delete must run")
                .removed,
            0
        );
    }

    #[test]
    fn unlink_narrows_to_a_relation_when_asked() {
        let db = testing::db();
        let a = testing::add(&db, MemoryKind::Semantic, "a", "a");
        let b = testing::add(&db, MemoryKind::Semantic, "b", "b");
        link_to(&db, &a, &b, Relation::Contradicts);
        link_to(&db, &a, &b, Relation::DerivedFrom);
        assert_eq!(edge_count(&db), 2);

        let outcome = db
            .unlink(&a, &b, Some(Relation::Contradicts))
            .expect("the delete must run");
        assert_eq!(
            outcome.removed, 1,
            "`contradicts` went, the citation stayed"
        );
        assert_eq!(
            related(&db, &a)
                .iter()
                .map(|n| n.relation)
                .collect::<Vec<_>>(),
            vec![Relation::DerivedFrom]
        );
    }

    #[test]
    fn unlink_without_a_relation_takes_both_rows_of_the_same_pair() {
        let db = testing::db();
        let a = testing::add(&db, MemoryKind::Semantic, "a", "a");
        let b = testing::add(&db, MemoryKind::Semantic, "b", "b");
        link_to(&db, &a, &b, Relation::Supports);
        link_to(&db, &b, &a, Relation::Supports);

        let outcome = db.unlink(&a, &b, None).expect("the delete must run");
        assert_eq!(outcome.removed, 2);
        assert_eq!(edge_count(&db), 0);
    }

    #[test]
    fn relate_returns_the_neighbour_with_its_title_and_excerpt() {
        let db = testing::db();
        // Disjoint vocabulary on purpose: the auto-link pass would otherwise add a
        // `related_to` edge between two memories that mention the same words, and this
        // test is about the one edge it writes itself.
        let a = testing::add(&db, MemoryKind::Semantic, "alpha beta", "gamma");
        let neighbour = db
            .add(
                &NewMemory::new(MemoryKind::Procedural, "Delta epsilon")
                    .body("zeta")
                    .excerpt("run the pipeline first"),
            )
            .expect("the fixture write must succeed")
            .id;
        link_to(&db, &a, &neighbour, Relation::Precedes);

        let neighbours = related(&db, &a);
        assert_eq!(neighbours.len(), 1);
        assert_eq!(neighbours[0].title, "Delta epsilon");
        assert_eq!(neighbours[0].excerpt, "run the pipeline first");
        assert_eq!(neighbours[0].kind, MemoryKind::Procedural);
        assert_eq!(neighbours[0].status, Status::Active);
        assert_eq!(neighbours[0].relation, Relation::Precedes);
        assert_eq!(neighbours[0].origin, "manual");
        assert_ne!(neighbours[0].created_at.len(), 0);
    }

    #[test]
    fn relate_walks_one_hop_and_says_so_when_asked_for_more() {
        let db = testing::db();
        let a = testing::add(&db, MemoryKind::Semantic, "a", "a");
        let b = testing::add(&db, MemoryKind::Semantic, "b", "b");
        let c = testing::add(&db, MemoryKind::Semantic, "c", "c");
        link_to(&db, &a, &b, Relation::Supports);
        link_to(&db, &b, &c, Relation::Supports);

        let first = related(&db, &a);
        assert_eq!(first.len(), 1);
        assert_eq!(
            first[0].memory_id, b,
            "the second hop is not in a one-hop read"
        );

        let error = db
            .relations(&a, &[], 2)
            .expect_err("a two-hop walk is not in this release");
        assert!(matches!(error, Error::Invalid(_)), "{error}");
        assert!(error.to_string().contains("one hop"), "{error}");
    }

    #[test]
    fn relate_filters_by_relation() {
        let db = testing::db();
        let a = testing::add(&db, MemoryKind::Semantic, "a", "a");
        let b = testing::add(&db, MemoryKind::Semantic, "b", "b");
        let c = testing::add(&db, MemoryKind::Semantic, "c", "c");
        link_to(&db, &a, &b, Relation::Supports);
        link_to(&db, &a, &c, Relation::Contradicts);

        let only = db
            .relations(&a, &[Relation::Contradicts], 1)
            .expect("a filtered read must run");
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].memory_id, c);

        let both = db
            .relations(&a, &[Relation::Supports, Relation::Contradicts], 1)
            .expect("a filtered read must run");
        assert_eq!(both.len(), 2);
    }

    #[test]
    fn relate_on_an_unknown_id_is_not_found_rather_than_empty() {
        let db = testing::db();
        let error = db
            .relations("k_01ZZZZZZZZZZZZZZZZZZZZZZZZ", &[], 1)
            .expect_err("an unknown memory is not an empty neighbourhood");
        assert!(matches!(error, Error::NotFound(_)), "{error}");
    }

    #[test]
    fn a_draft_neighbour_is_hidden_even_when_the_edge_exists() {
        // Defence in depth. No writer can create this state — `link` refuses a draft and
        // the auto-link pass only ever sees active rows — so the fixture makes it with raw
        // SQL. Without the read filter, this row would print the inbox's title.
        let db = testing::db();
        let active = testing::add(&db, MemoryKind::Semantic, "approved", "a");
        let draft = db
            .add(
                &NewMemory::new(MemoryKind::Semantic, "a draft nobody approved")
                    .body("not yet")
                    .source("agent_reflection")
                    .confidence(0.4),
            )
            .expect("the draft must be stored")
            .id;
        db.conn()
            .execute(
                "INSERT INTO memory_links
                    (source_id, target_id, relation, directed, origin, created_at)
                 VALUES (?1, ?2, 'related_to', 0, 'manual', '2026-01-01T00:00:00Z')",
                params![active, draft],
            )
            .expect("the edge cannot be written through the API, so it is written around it");

        assert!(
            related(&db, &active).is_empty(),
            "a pending memory is not in any read path, and `relate` is one"
        );
    }

    #[test]
    fn a_closed_edge_is_not_a_neighbour() {
        // `valid_to` is what "this was resolved" looks like, and nothing writes it yet; the
        // read filters it anyway, so that the first writer of a resolution cannot make a
        // settled contradiction reappear through `relate`.
        let db = testing::db();
        let a = testing::add(&db, MemoryKind::Semantic, "a", "a");
        let b = testing::add(&db, MemoryKind::Semantic, "b", "b");
        link_to(&db, &a, &b, Relation::Contradicts);
        assert_eq!(related(&db, &a).len(), 1);

        db.conn()
            .execute(
                "UPDATE memory_links SET valid_to = '2026-01-01T00:00:00Z'",
                [],
            )
            .expect("the fixture must close the edge");
        assert_eq!(related(&db, &a).len(), 0);
    }

    #[test]
    fn an_archived_neighbour_is_shown_with_the_status_that_says_so() {
        let db = testing::db();
        let a = testing::add(&db, MemoryKind::Semantic, "a", "a");
        let gone = testing::add(&db, MemoryKind::Semantic, "b", "b");
        link_to(&db, &a, &gone, Relation::RelatedTo);
        db.archive(std::slice::from_ref(&gone))
            .expect("the fixture must archive");

        let neighbours = related(&db, &a);
        assert_eq!(
            neighbours.len(),
            1,
            "an archived memory is not hidden, it is labelled"
        );
        assert_eq!(neighbours[0].status, Status::Archived);
    }

    #[test]
    fn forgetting_a_memory_takes_its_edges_with_it() {
        let db = testing::db();
        let a = testing::add(&db, MemoryKind::Semantic, "a", "a");
        let b = testing::add(&db, MemoryKind::Semantic, "b", "b");
        link_to(&db, &a, &b, Relation::RelatedTo);
        assert_eq!(edge_count(&db), 1);

        db.forget(std::slice::from_ref(&b))
            .expect("the purge must run");
        assert_eq!(edge_count(&db), 0, "the cascade is the database's job");
        assert_eq!(related(&db, &a).len(), 0);
    }

    #[test]
    fn the_canonical_order_is_the_older_memory() {
        // ULIDs sort by creation time, so "smaller id first" is "the memory that was here
        // first", which is a rule a reader can state. The three cases below pin the rule
        // rather than the sort.
        assert_eq!(
            canonical_pair("k_02", "k_01", Relation::RelatedTo),
            ("k_01".to_owned(), "k_02".to_owned())
        );
        assert_eq!(
            canonical_pair("k_01", "k_02", Relation::RelatedTo),
            ("k_01".to_owned(), "k_02".to_owned())
        );
        // A directed relation is stored as given, canonicalisation or not.
        assert_eq!(
            canonical_pair("k_02", "k_01", Relation::Precedes),
            ("k_02".to_owned(), "k_01".to_owned())
        );
    }

    #[test]
    fn the_auto_link_text_is_the_title_then_the_body_head_and_only_twelve_terms() {
        // Twelve distinct words: five from the title, then seven from the body.
        let title = "the deploy pipeline runs migrations";
        let body = (0..40)
            .map(|index| format!("word{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        let text = neighbour_text(title, &body);
        let terms: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(terms.len(), AUTO_LINK_MAX_TERMS);
        assert_eq!(
            &terms[..5],
            &["the", "deploy", "pipeline", "runs", "migrations"]
        );
        assert_eq!(terms[5], "word0");
        assert_eq!(terms[11], "word6", "the body fills the rest of the budget");

        // A megabyte of one repeated word is one term, not a megabyte of query. The body
        // is cut at a byte budget first, so this is about the dedupe.
        let repeated = neighbour_text("aa aa aa", "aa bb aa bb cc cc");
        assert_eq!(repeated, "aa bb cc");
    }

    #[test]
    fn a_multibyte_body_head_cannot_split_a_character() {
        // A kilobyte of Japanese is around three hundred characters; cutting at a byte
        // offset would panic, and this is the corpus the project claims to support.
        let body = "日本語のメモリ".repeat(200);
        let head = head_chars(&body, AUTO_LINK_BODY_BYTES);
        assert!(body.starts_with(head));
        assert!(head.is_char_boundary(head.len()));
        assert!(head.len() <= AUTO_LINK_BODY_BYTES);
    }
}
