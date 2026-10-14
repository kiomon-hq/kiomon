// SPDX-License-Identifier: Apache-2.0

//! Lexical retrieval: the query ladder, the ranked window, and the record of it.
//!
//! # The ladder
//!
//! A local user types a sentence, not a query language. The hosted product's
//! `sanitizeFtsQuery` answers that by stripping punctuation with `replace(/[^\w\s]/g,
//! "")` and joining the survivors with `AND` — which has two problems worth fixing
//! rather than porting:
//!
//! 1. **JavaScript's `\w` is ASCII.** `café` becomes `caf`, and a query for a Japanese
//!    phrase becomes the empty string. The bug is invisible in English and total
//!    elsewhere. FTS5's own string syntax is the fix: quoting a term makes it a literal,
//!    so nothing needs stripping and nothing is lost.
//! 2. **An `AND` of every word in a question is usually empty.** "why did the deploy
//!    fail" has five terms and no corpus contains all five, so the sanitizer's answer to
//!    a perfectly reasonable question is no results at all.
//!
//! So the expression is a ladder, tried from most precise to most forgiving, and the
//! first rung that returns anything wins:
//!
//! | Rung | Expression | What it means |
//! |---|---|---|
//! | 1 | `"the whole query"` | The words as a phrase, in order |
//! | 2 | `"t1"* AND "t2"*` | Every term, prefix-matched |
//! | 3 | `"t1"* OR "t2"*` | Any term, prefix-matched |
//!
//! Prefix matching is what makes the ladder worth climbing for languages without spaces:
//! `unicode61` treats a run of CJK letters as one token, so `日本語` is a *prefix* of the
//! token `日本語のメモ` and rung 1 — which asks for the whole token — finds nothing.
//!
//! # The window and the page
//!
//! A leg returns a ranked window [`WINDOW_OVERFETCH`] times wider than the page, and the
//! page is cut from that window after the ordering has been applied. Two consequences,
//! both intentional: a pinned memory at rank 25 can still reach page one, and `--sort`
//! cannot reorder a row the window never held.

use std::collections::HashMap;

use rusqlite::types::Value as Sql;

use crate::db::Database;
use crate::error::{Error, Result};
use crate::fuse;
use crate::model::SearchSort::Relevance;
use crate::model::{
    Legs, Memory, SearchAttempt, SearchHit, SearchOutcome, SearchQuery, SearchSort,
    WINDOW_OVERFETCH,
};
use crate::store::{MEMORY_COLUMNS, kind_filter, row_to_memory, scope_clause};

/// How many rungs the ladder has.
pub const LADDER_RUNGS: u8 = 3;

/// The column index `snippet()` reads from: `body`.
///
/// Not `-1`, which picks the best-matching column: that would put the title in the
/// snippet, and the title is already a field of every row. The body is the part a caller
/// has not seen.
const SNIPPET_COLUMN: u8 = 2;

/// The characters wrapped around a matched term inside a snippet, before
/// [`normalize_snippet`] turns them into `**`.
///
/// Control characters rather than `**` directly, because `**` is legal text: a body
/// containing `**` would make an unhighlighted snippet indistinguishable from a
/// highlighted one, and unhighlighted snippets are dropped, not shown.
const HIGHLIGHT_START: char = '\u{1}';
const HIGHLIGHT_END: char = '\u{2}';

/// The ellipsis FTS5 puts where it cut the text.
const SNIPPET_ELLIPSIS: &str = "…";

/// Every FTS5 expression one query is tried as, in order.
///
/// Built once per search so an empty or punctuation-only query cannot reach SQLite as a
/// syntax error: every term is quoted, and a quoted string is a literal to FTS5, which
/// means `NEAR`, `*`, `-`, `"` and `()` in a user's query are all just text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ladder {
    rungs: [String; LADDER_RUNGS as usize],
}

impl Ladder {
    /// Build the ladder for a query, or `None` when there is nothing to search for.
    ///
    /// # Why an empty query is not an error
    ///
    /// `kiomon search ""` is a caller with no question, not a malformed request. It is
    /// reported as [`SearchAttempt::NothingAsked`], which says exactly that and costs no
    /// query.
    #[must_use]
    pub fn new(query: &str) -> Option<Self> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return None;
        }
        let terms: Vec<String> = trimmed
            .split_whitespace()
            .map(|term| format!("{}*", quote(term)))
            .collect();
        Some(Self {
            rungs: [quote(trimmed), terms.join(" AND "), terms.join(" OR ")],
        })
    }

    /// The expressions, in the order they are tried.
    #[must_use]
    pub fn rungs(&self) -> &[String; LADDER_RUNGS as usize] {
        &self.rungs
    }

    /// The most forgiving rung, for reporting what was tried when nothing matched.
    #[must_use]
    pub fn last(&self) -> &str {
        &self.rungs[LADDER_RUNGS as usize - 1]
    }
}

/// Wrap `text` as an FTS5 string literal.
///
/// A double quote inside is escaped by doubling it, which is FTS5's rule and not SQL's —
/// this string is a *value* bound to a parameter, never interpolated into SQL.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// Turn a raw `snippet()` result into the value a caller sees.
///
/// Returns an empty string when the snippet carries no highlight. `snippet()` does not
/// return nothing when the match was in another column — it returns the *first* tokens of
/// the requested one, unhighlighted — and a snippet whose text merely happens to be near
/// the top of the body reads as though that is where the match is. An empty `snippet` is
/// honest; a plausible wrong one is not. The title, the excerpt and the tags are already
/// fields of the row, so nothing is actually lost when the match was in one of them.
///
/// Whitespace is collapsed for the same reason the MCP handler collapses it: a body with
/// newlines in it would otherwise put newlines in the middle of a result line.
fn normalize_snippet(raw: &str) -> String {
    if !raw.contains(HIGHLIGHT_START) {
        return String::new();
    }
    let mut flattened = String::with_capacity(raw.len());
    let mut pending_space = false;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && !flattened.is_empty() {
            flattened.push(' ');
        }
        pending_space = false;
        flattened.push(ch);
    }
    flattened.replace([HIGHLIGHT_START, HIGHLIGHT_END], "**")
}

impl Database {
    /// Search the brain.
    ///
    /// One ranked read plus, unless recording is off, one counter bump per returned row.
    /// The bump is not telemetry: `retrieval_count` and `last_retrieved_at` are the decay
    /// clock's stretch factor, so a memory that keeps being used is a memory that keeps
    /// mattering.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] for [`crate::model::Legs::Vector`], which this build
    /// cannot serve, and [`Error::Database`] if a query fails.
    pub fn search(&self, query: &SearchQuery) -> Result<SearchOutcome> {
        let legs = match query.legs {
            Legs::Auto | Legs::Fts => "fts",
            // Not a fallback. The platform learned this the expensive way: answering a
            // request for one thing with another thing returns something the caller did
            // not ask for and cannot detect. §4.2 of the plan says never silently
            // approximate, and exit code 4 exists so that "I cannot" is distinguishable
            // from "there is nothing".
            Legs::Vector => {
                return Err(Error::Config(
                    "`--legs vector` needs an embedder, and this build has none: the vector \
                     leg arrives with the embedder. Use `--legs fts`, or `--legs auto`, \
                     which is full text only until then."
                        .to_owned(),
                ));
            }
        };

        let Some(ladder) = Ladder::new(&query.text) else {
            return Ok(SearchOutcome {
                hits: Vec::new(),
                attempt: SearchAttempt::NothingAsked,
                legs,
                degraded: false,
                recorded: 0,
            });
        };

        // The window cannot be narrower than the page, and `limit` is a `u32` a caller
        // could set to zero.
        let window = query.limit.max(1).saturating_mul(WINDOW_OVERFETCH);

        // Zipped with an explicit 1-based counter rather than tracked by `enumerate`,
        // because the number is reported to a caller and read by a human: "rung 2 of 3"
        // is a thing a person can reason about, and "rung 1 of 0..=2" is not.
        let mut matched: Option<(u8, String, Vec<i64>)> = None;
        for (rung, expression) in (1..=LADDER_RUNGS).zip(ladder.rungs()) {
            let ranked = self.lexical_window(expression, query, window)?;
            if !ranked.is_empty() {
                matched = Some((rung, expression.clone(), ranked));
                break;
            }
        }

        let Some((rung, expression, ranked)) = matched else {
            return Ok(SearchOutcome {
                hits: Vec::new(),
                attempt: SearchAttempt::Exhausted {
                    query: ladder.last().to_owned(),
                },
                legs,
                degraded: false,
                recorded: 0,
            });
        };

        // Fusion is handed the window, not the page: the page is cut after ordering, and
        // a fusion that had already truncated to `limit` would make `--sort` and the pin
        // hoist no-ops. With one leg this preserves the leg's order exactly, so nothing
        // about the ranking depends on the fusion being here yet.
        let fused = fuse::fuse(&[ranked], window as usize);
        let window_rowids: Vec<i64> = fused.iter().map(|(mem_id, _)| *mem_id).collect();
        let mut by_rowid = self.memories_by_rowid(&window_rowids)?;

        // The window and the page, kept side by side: the page is what the caller gets and
        // the window is what they were cut from, and the snippets are asked for by the
        // rowid the window knows and the page does not carry.
        let mut hits: Vec<SearchHit> = Vec::with_capacity(window_rowids.len());
        let mut page_rowids: Vec<i64> = Vec::with_capacity(window_rowids.len());
        for (mem_id, score) in fused {
            // A row in the leg and not in the table means the two queries disagreed about
            // the corpus, which two statements in autocommit can do if another process
            // purged a memory between them. Skipping is right: the row is gone.
            if let Some(memory) = by_rowid.remove(&mem_id) {
                page_rowids.push(mem_id);
                hits.push(SearchHit {
                    memory,
                    score,
                    snippet: String::new(),
                });
            }
        }

        cut(&mut hits, query.sort, query.limit as usize);
        page_rowids.truncate(hits.len());

        // Snippets last, and only for the page — see `snippets` for why this is a
        // statement of its own rather than a column of the ranked one.
        let mut snippets = self.snippets(&expression, query.snippet_tokens, &page_rowids)?;
        for (index, hit) in hits.iter_mut().enumerate() {
            // `page_rowids` is in the same order as `hits` — both were built in the loop
            // above and then cut together — so the index pairs them.
            hit.snippet = page_rowids
                .get(index)
                .and_then(|rowid| snippets.remove(rowid))
                .map_or_else(String::new, |raw| normalize_snippet(&raw));
        }

        let recorded = if query.record {
            let ids: Vec<String> = hits.iter().map(|hit| hit.memory.id.clone()).collect();
            self.record_retrieval(&ids)?
        } else {
            0
        };

        Ok(SearchOutcome {
            hits,
            attempt: SearchAttempt::Matched {
                rung,
                query: expression,
            },
            legs,
            degraded: false,
            recorded,
        })
    }

    /// One leg's ranked window: the rowids of the best `limit` rows for one FTS5
    /// expression, best first.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the query fails. A malformed expression cannot
    /// reach here — every rung is built by [`Ladder`], which quotes every term.
    fn lexical_window(
        &self,
        expression: &str,
        query: &SearchQuery,
        limit: u32,
    ) -> Result<Vec<i64>> {
        // `memories.` prefixes on the filter clauses, because the index has columns named
        // `title`, `excerpt` and `body` too, and a future unqualified clause would resolve
        // to whichever table SQLite happened to prefer.
        let mut sql = String::from(
            "SELECT memory_fts.rowid \
               FROM memory_fts \
               JOIN memories ON memories.mem_id = memory_fts.rowid \
              WHERE memory_fts MATCH ? \
                AND memories.status = 'active'",
        );
        // Binding order is the text order of the placeholders, so it is: the match
        // expression, the scope, the kinds (which are literals), the limit.
        let mut values: Vec<Sql> = vec![Sql::Text(expression.to_owned())];

        let (scope, scope_values) = scope_clause(Some("memories."), &query.scope);
        sql.push_str(&scope);
        values.extend(scope_values);
        sql.push_str(&kind_filter(Some("memories."), &query.kinds));

        // `bm25` returns negative values, so ascending is best-first. The weights are
        // title 10 · excerpt 5 · body 1 · tags 8, in the index's column order — a title
        // match is worth more than a ten-body-word match, and a tag is worth nearly a
        // title because a tag is a person's own index term.
        sql.push_str(" ORDER BY bm25(memory_fts, 10.0, 5.0, 1.0, 8.0) LIMIT ?");
        values.push(Sql::Integer(i64::from(limit)));

        let mut statement = self.conn().prepare(&sql)?;
        let rows =
            statement.query_map(rusqlite::params_from_iter(values.iter()), |row| row.get(0))?;

        let mut ranked = Vec::new();
        for row in rows {
            ranked.push(row?);
        }
        Ok(ranked)
    }

    /// The match in context for the page, keyed by memory id.
    ///
    /// A statement of its own, and the measurements are the whole argument. `snippet()` is
    /// an FTS5 auxiliary function evaluated in the select list, and SQLite evaluates a
    /// select list for every row the `WHERE` clause admits — *before* the `ORDER BY` and
    /// `LIMIT` that would have thrown most of them away. Asking for a snippet in the
    /// ranked query therefore tokenises every match in the corpus to keep ten of them:
    /// measured at 47 ms against 13 ms for the same query without it, on ten thousand
    /// memories where a five-word disjunction matches nine thousand of them.
    ///
    /// Asking here instead costs about 0.8 ms per row, because a `MATCH` inside an `IN`
    /// list is re-evaluated for each rowid in it — the probe confirmed the cost is linear
    /// in the list, not fixed. So the right number of rows to ask about is the page, and
    /// not the three-times-wider window it was cut from.
    ///
    /// Keyed by `mem_id` rather than returned as a list: the order of an `IN` list is not
    /// the order of its results, and the caller has already decided the page's order.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the query fails.
    fn snippets(
        &self,
        expression: &str,
        tokens: u16,
        rowids: &[i64],
    ) -> Result<HashMap<i64, String>> {
        if rowids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = vec!["?"; rowids.len()].join(", ");
        let mut values: Vec<Sql> = vec![
            Sql::Text(SNIPPET_ELLIPSIS.to_owned()),
            Sql::Integer(i64::from(tokens)),
            Sql::Text(expression.to_owned()),
        ];
        values.extend(rowids.iter().map(|rowid| Sql::Integer(*rowid)));

        let sql = format!(
            "SELECT memory_fts.rowid, \
                    snippet(memory_fts, {SNIPPET_COLUMN}, char(1), char(2), ?, ?) \
               FROM memory_fts \
              WHERE memory_fts MATCH ? \
                AND memory_fts.rowid IN ({placeholders})"
        );
        let mut statement = self.conn().prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(values.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = HashMap::with_capacity(rowids.len());
        for row in rows {
            let (mem_id, snippet) = row?;
            out.insert(mem_id, snippet);
        }
        Ok(out)
    }

    /// Load whole memories by their integer rows, keyed by the same.
    ///
    /// `mem_id` is read positionally and everything else by name: this selects one more
    /// column than [`MEMORY_COLUMNS`] so the caller can match rows back to the leg that
    /// asked for them, and [`row_to_memory`] looks its fields up by name, so the extra
    /// column is invisible to it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the query fails.
    fn memories_by_rowid(&self, rowids: &[i64]) -> Result<HashMap<i64, Memory>> {
        if rowids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = vec!["?"; rowids.len()].join(", ");
        let sql = format!(
            "SELECT mem_id, {MEMORY_COLUMNS} FROM memories WHERE mem_id IN ({placeholders})"
        );
        let mut statement = self.conn().prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(rowids.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row_to_memory(row)?))
        })?;
        let mut out = HashMap::with_capacity(rowids.len());
        for row in rows {
            let (mem_id, memory) = row?;
            out.insert(mem_id, memory);
        }
        Ok(out)
    }
}

/// Order the window, cut the page from it, and drop the rest.
///
/// Three steps, in this order, and all three matter:
///
/// 1. Apply the requested ordering. `relevance` is already in fusion order.
/// 2. Hoist pinned rows, unless the ordering is `recent`.
/// 3. Truncate to the page size.
///
/// Cutting the page before step 2 would let hoisting promote a pinned row only if it were
/// already inside the first `limit` — which is exactly the case where hoisting is not
/// needed. That is the whole reason the leg fetches a window wider than the page:
/// [`WINDOW_OVERFETCH`].
///
/// The hoist is a stable sort on one key, so it is a partition: pinned rows in the
/// requested order, then everything else in the requested order. That is what makes the
/// result total rather than merely sorted.
///
/// `recent` is excluded for the reason `list --recent` is: "newest first" with pinned
/// rows on top is not newest first. `relevance` and `strength` are opinions about what
/// matters, and a pin is a stronger opinion than either.
fn cut(hits: &mut Vec<SearchHit>, sort: SearchSort, limit: usize) {
    match sort {
        Relevance => {}
        SearchSort::Recent => hits.sort_by(|a, b| {
            b.memory
                .created_at
                .cmp(&a.memory.created_at)
                // `created_at` has second precision, so ties are ordinary rather than
                // exceptional and an id tiebreak is what keeps the order stable.
                .then_with(|| b.memory.id.cmp(&a.memory.id))
        }),
        SearchSort::Strength => hits.sort_by(|a, b| {
            b.memory
                .strength
                .total_cmp(&a.memory.strength)
                .then_with(|| b.memory.id.cmp(&a.memory.id))
        }),
    }
    if sort != SearchSort::Recent {
        hits.sort_by_key(|hit| !hit.memory.pinned);
    }
    hits.truncate(limit);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MemoryKind, NewMemory, ReviewMode, Scope, Status};
    use crate::testing;

    /// A corpus with one English memory, one accented, one Japanese, and one whose only
    /// `deploy` is in its body — so a test can tell a title match from a body match.
    fn corpus() -> Database {
        let db = testing::db();
        testing::add(
            &db,
            MemoryKind::Semantic,
            "Staging deploys need a manual migration",
            "Run migrations before the deploy; the pipeline does not do it.",
        );
        testing::add(
            &db,
            MemoryKind::Semantic,
            "Café hours",
            "The café opens at nine and closes late.",
        );
        testing::add(
            &db,
            MemoryKind::Reference,
            "日本語のメモ",
            "これは日本語のテストです。漢字も入っています。",
        );
        db
    }

    fn search(db: &Database, text: &str) -> SearchOutcome {
        db.search(&SearchQuery {
            text: text.to_owned(),
            ..SearchQuery::default()
        })
        .expect("the fixture search must succeed")
    }

    fn titles(outcome: &SearchOutcome) -> Vec<&str> {
        outcome
            .hits
            .iter()
            .map(|hit| hit.memory.title.as_str())
            .collect()
    }

    // -----------------------------------------------------------------------
    // The ladder
    // -----------------------------------------------------------------------

    #[test]
    fn the_ladder_looks_like_the_documented_table() {
        let ladder = Ladder::new("why did the deploy fail").expect("a query with terms");
        assert_eq!(
            ladder.rungs(),
            &[
                "\"why did the deploy fail\"".to_owned(),
                "\"why\"* AND \"did\"* AND \"the\"* AND \"deploy\"* AND \"fail\"*".to_owned(),
                "\"why\"* OR \"did\"* OR \"the\"* OR \"deploy\"* OR \"fail\"*".to_owned(),
            ]
        );
    }

    #[test]
    fn a_single_term_still_climbs_from_exact_to_prefix() {
        // Rung 1 and rung 2 are not the same expression even for one word, which is the
        // case that makes the ladder more than a fallback: `"deploy"` finds the token and
        // `"deploy"*` finds every word starting with it.
        let ladder = Ladder::new("deploy").expect("a query with one term");
        assert_eq!(
            ladder.rungs(),
            &[
                "\"deploy\"".to_owned(),
                "\"deploy\"*".to_owned(),
                "\"deploy\"*".to_owned(),
            ]
        );
    }

    #[test]
    fn an_empty_or_whitespace_query_has_no_ladder() {
        assert!(Ladder::new("").is_none());
        assert!(Ladder::new("   \t\n ").is_none());
    }

    #[test]
    fn every_rung_of_every_query_is_a_valid_expression() {
        // The point of quoting is that no user input can become FTS5 syntax. Each of
        // these is a query a person could type, and each must be searched rather than
        // rejected — a syntax error out of `MATCH` would surface as a database error and
        // exit 1, which is the wrong answer to a typo.
        let db = corpus();
        for text in [
            "deploy",
            "why did the deploy fail",
            "\"quoted\"",
            "NEAR(deploy fail)",
            "deploy*",
            "c++ AND (rust OR go)",
            "-deploy",
            "col:deploy",
            "{}[]|^~",
            "!!!",
            "@#$%",
            "表 日本語",
            "café",
            "😀",
            "a\"b",
        ] {
            let outcome = db
                .search(&SearchQuery {
                    text: text.to_owned(),
                    ..SearchQuery::default()
                })
                .unwrap_or_else(|err| panic!("`{text}` must be searchable, got {err}"));
            // No assertion on the count: the assertion is that this returned at all.
            let _ = outcome.attempt;
        }
    }

    #[test]
    fn the_ladder_takes_the_first_rung_that_returns_anything() {
        // Table-driven, and the table is the acceptance criterion: each row names the
        // query and the rung the answer must come from. Between them the rows cover a
        // rung-1 hit, a rung-2 hit, a rung-3 hit, and no hit at all.
        let db = corpus();
        let cases: [(&str, Option<u8>, usize); 8] = [
            // The words are adjacent in the title, so the phrase is enough.
            ("manual migration", Some(1), 1),
            ("Café hours", Some(1), 1),
            // "deploy" is in the body and "manual" in the title: no phrase holds both, and
            // both terms are present, so the conjunction is the first rung that works.
            ("manual deploy", Some(2), 1),
            // Every word present, prefix-matched, is still one memory.
            ("deploys migration", Some(2), 1),
            // "pipeline" and "café" are in different memories, so nothing satisfies the
            // conjunction and the disjunction is what finds them.
            ("pipeline café", Some(3), 2),
            // The question the sanitizer fails: five common words, not one of which is
            // guaranteed to co-occur with the others. `the` alone reaches a second
            // memory, which is the disjunction doing exactly what it is for.
            ("why did the deploy fail", Some(3), 2),
            // Vocabulary the corpus does not have, in every rung.
            ("onomatopoeia zzz", None, 0),
            // Punctuation only. Every rung is a legal expression that matches nothing,
            // which is the difference between this and a syntax error.
            ("!!!", None, 0),
        ];
        for (text, expected_rung, expected_hits) in cases {
            let outcome = search(&db, text);
            assert_eq!(
                outcome.hits.len(),
                expected_hits,
                "`{text}` returned {:?}",
                titles(&outcome)
            );
            match (expected_rung, &outcome.attempt) {
                (Some(rung), SearchAttempt::Matched { rung: got, .. }) => assert_eq!(
                    *got, rung,
                    "`{text}` should have been answered by rung {rung}"
                ),
                (Some(rung), other) => {
                    panic!("`{text}` should have been answered by rung {rung}, got {other:?}")
                }
                (None, SearchAttempt::Exhausted { .. }) => {}
                (None, other) => panic!("`{text}` should have exhausted the ladder, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_exhausted_ladder_reports_the_last_expression_tried() {
        // The diagnostic an agent needs to answer "why did that not match": what was
        // actually asked. The last rung, because that is the most forgiving thing there
        // is and the one that had the best chance.
        let db = corpus();
        let outcome = search(&db, "onomatopoeia zzz");
        let SearchAttempt::Exhausted { query } = &outcome.attempt else {
            panic!("expected exhaustion, got {:?}", outcome.attempt);
        };
        assert_eq!(query, "\"onomatopoeia\"* OR \"zzz\"*");
    }

    #[test]
    fn an_empty_query_asks_nothing_rather_than_matching_nothing() {
        // Two different facts, and the contract distinguishes them: `nothing_asked` is a
        // caller that sent no question, `exhausted` is a corpus that had no answer.
        let db = corpus();
        for text in ["", "   ", "\n"] {
            let outcome = search(&db, text);
            assert_eq!(outcome.attempt, SearchAttempt::NothingAsked);
            assert!(outcome.hits.is_empty());
            assert_eq!(outcome.recorded, 0);
        }
    }

    // -----------------------------------------------------------------------
    // The languages the sanitizer loses
    // -----------------------------------------------------------------------

    #[test]
    fn a_japanese_query_finds_its_memory() {
        // `unicode61` treats the run of letters as one token, so the whole-title phrase
        // matches and — more usefully — a prefix of it does too. Both are asserted because
        // the prefix case is the one that needs the ladder.
        let db = corpus();
        for text in ["日本語のメモ", "日本語"] {
            let outcome = search(&db, text);
            assert_eq!(
                titles(&outcome),
                vec!["日本語のメモ"],
                "`{text}` should find the Japanese memory"
            );
        }
        // Inside the body, after a full stop, so it is its own token and a prefix of it
        // matches: the case the platform's `[^\w\s]` sanitizer deletes outright.
        assert_eq!(titles(&search(&db, "漢字")), vec!["日本語のメモ"]);
    }

    #[test]
    fn a_japanese_suffix_is_the_documented_gap() {
        // `unicode61` does not segment Japanese, so `のメモ` is the *tail* of the token
        // `日本語のメモ` and no prefix query can reach it. Pinned as a known limit rather
        // than asserted as behaviour, because the next reader will otherwise file it as a
        // bug: `plans/001-local-v1-core.md` §10 records the trigram tokenizer as the fix
        // if this ever costs somebody a memory.
        let db = corpus();
        assert!(search(&db, "のメモ").hits.is_empty());
    }

    #[test]
    fn an_accented_query_ignores_the_accent() {
        // `unicode61` removes diacritics on both sides, so `cafe` and `café` are the same
        // term. The platform's `\w` is ASCII-only, so its sanitizer turns this query into
        // `caf` and the memory becomes unreachable by its own title.
        let db = corpus();
        for text in ["café", "cafe", "CAFÉ"] {
            assert_eq!(
                titles(&search(&db, text)),
                vec!["Café hours"],
                "`{text}` should find the accented memory"
            );
        }
    }

    // -----------------------------------------------------------------------
    // The window
    // -----------------------------------------------------------------------

    #[test]
    fn the_window_is_wider_than_the_page_and_the_page_is_cut_from_it() {
        let db = testing::db();
        for index in 0..20 {
            testing::add(
                &db,
                MemoryKind::Semantic,
                &format!("Deployment note {index}"),
                "deploy deploy deploy",
            );
        }
        let outcome = db
            .search(&SearchQuery {
                text: "deploy".to_owned(),
                limit: 3,
                ..SearchQuery::default()
            })
            .unwrap();
        assert_eq!(outcome.hits.len(), 3, "the page is the limit");
    }

    #[test]
    fn a_pinned_memory_at_the_far_edge_of_the_window_reaches_the_top_of_the_page() {
        // The promise, and the argument for a window wider than the page. Twelve memories
        // match and the page is three, so the window is nine: the memory ranked *last*
        // inside that window has to be able to reach the top of the page, and it can only
        // do so because the page is cut after the hoist rather than before it.
        let db = testing::db();
        for index in 0..12 {
            testing::add(
                &db,
                MemoryKind::Semantic,
                &format!("Deployment note {index}"),
                &format!("deploy step {index} {}", "deploy ".repeat(index + 1)),
            );
        }

        // Ask for the whole ranking first, so the test pins the row the ranking actually
        // puts last rather than a row it merely hopes is last.
        let ranking = db
            .search(&SearchQuery {
                text: "deploy".to_owned(),
                limit: 12,
                record: false,
                ..SearchQuery::default()
            })
            .unwrap();
        assert_eq!(ranking.hits.len(), 12);
        let edge = ranking.hits[8].memory.id.clone();
        assert!(
            !ranking.hits[..9].iter().any(|hit| hit.memory.pinned),
            "the fixture must start unpinned or this proves nothing"
        );
        db.conn()
            .execute("UPDATE memories SET pinned = 1 WHERE id = ?1", [&edge])
            .unwrap();

        let page = db
            .search(&SearchQuery {
                text: "deploy".to_owned(),
                limit: 3,
                ..SearchQuery::default()
            })
            .unwrap();
        assert_eq!(page.hits.len(), 3, "the page is the limit, not the window");
        assert_eq!(
            page.hits[0].memory.id, edge,
            "a pin has to be able to cross the whole window"
        );
        assert!(
            !page.hits[1].memory.pinned,
            "only the pinned row was hoisted, so the rest keep the fused order"
        );
    }

    #[test]
    fn a_pin_outside_the_window_cannot_reach_the_page() {
        // The documented approximation, asserted so that it stays documented: `--sort`
        // and the pin hoist see the top `WINDOW_OVERFETCH * limit` matches and nothing
        // beyond them. That is the price of answering a page of results without scanning
        // the corpus, and the contract says so rather than leaving it to be discovered.
        let db = testing::db();
        for index in 0..20 {
            testing::add(
                &db,
                MemoryKind::Semantic,
                &format!("Deployment note {index}"),
                &format!("deploy step {index} {}", "deploy ".repeat(index + 1)),
            );
        }
        let ranking = db
            .search(&SearchQuery {
                text: "deploy".to_owned(),
                limit: 20,
                record: false,
                ..SearchQuery::default()
            })
            .unwrap();
        let outside = ranking.hits[12].memory.id.clone();
        db.conn()
            .execute("UPDATE memories SET pinned = 1 WHERE id = ?1", [&outside])
            .unwrap();

        let page = db
            .search(&SearchQuery {
                text: "deploy".to_owned(),
                limit: 3,
                ..SearchQuery::default()
            })
            .unwrap();
        assert_eq!(page.hits.len(), 3);
        assert!(
            !page.hits.iter().any(|hit| hit.memory.id == outside),
            "rank 13 is outside a window of 9, and the page is honest about that"
        );
    }

    #[test]
    fn the_recency_ordering_does_not_hoist_a_pin() {
        let db = testing::db();
        let first = testing::add(&db, MemoryKind::Semantic, "Deploy alpha", "deploy");
        db.conn()
            .execute("UPDATE memories SET pinned = 1 WHERE id = ?1", [&first])
            .unwrap();
        let second = testing::add(&db, MemoryKind::Semantic, "Deploy beta", "deploy");
        db.conn()
            .execute(
                "UPDATE memories SET created_at = '2099-01-01T00:00:00Z' WHERE id = ?1",
                [&second],
            )
            .unwrap();

        let outcome = db
            .search(&SearchQuery {
                text: "deploy".to_owned(),
                sort: SearchSort::Recent,
                ..SearchQuery::default()
            })
            .unwrap();
        assert_eq!(outcome.hits[0].memory.id, second, "newest first, unhoisted");
    }

    // -----------------------------------------------------------------------
    // The records the search keeps
    // -----------------------------------------------------------------------

    #[test]
    fn a_search_counts_as_a_retrieval_unless_it_is_told_not_to() {
        let db = corpus();
        let outcome = search(&db, "manual migration");
        assert_eq!(outcome.recorded, 1);
        let memory = db.get(&outcome.hits[0].memory.id).unwrap();
        assert_eq!(memory.retrieval_count, 1);
        assert!(
            memory.last_retrieved_at.is_some(),
            "the decay clock needs the date, not just the count"
        );

        let quiet = db
            .search(&SearchQuery {
                text: "manual migration".to_owned(),
                record: false,
                ..SearchQuery::default()
            })
            .unwrap();
        assert_eq!(quiet.recorded, 0);
        assert_eq!(
            db.get(&quiet.hits[0].memory.id).unwrap().retrieval_count,
            1,
            "`--no-record` must not move the counter"
        );
    }

    #[test]
    fn recording_a_retrieval_does_not_rebuild_the_index() {
        // The `mem_au` trigger fires on `UPDATE OF` a list of columns, and neither
        // `retrieval_count` nor `last_retrieved_at` is on it. If that ever changes, every
        // search would rewrite the full-text row for every hit — and the way to notice is
        // to assert the index row is *unchanged*, not merely still correct.
        let db = corpus();
        let before: (i64, i64) = db
            .conn()
            .query_row("SELECT count(*), max(rowid) FROM memory_fts", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        search(&db, "manual migration");
        let after: (i64, i64) = db
            .conn()
            .query_row("SELECT count(*), max(rowid) FROM memory_fts", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn a_draft_and_an_archived_memory_are_both_unsearchable() {
        // WP4 owns `review` and `archive`, but the guarantee is the schema's and it is
        // asserted here because search is the only way to observe it. Two mechanisms
        // agree: the trigger never indexed the draft, and the leg's `status = 'active'`
        // clause would filter it anyway.
        let db = testing::db();
        let active = testing::add(&db, MemoryKind::Semantic, "Visible deploy note", "deploy");
        let draft = testing::add(&db, MemoryKind::Semantic, "Draft deploy note", "deploy");
        let archived = testing::add(&db, MemoryKind::Semantic, "Archived deploy note", "deploy");
        db.conn()
            .execute(
                "UPDATE memories SET status = 'pending_approval' WHERE id = ?1",
                [&draft],
            )
            .unwrap();
        db.conn()
            .execute(
                "UPDATE memories SET status = 'archived' WHERE id = ?1",
                [&archived],
            )
            .unwrap();
        assert_eq!(testing::fts_rows(&db).unwrap(), 1);

        let outcome = search(&db, "deploy");
        assert_eq!(titles(&outcome), vec!["Visible deploy note"]);
        assert_eq!(outcome.hits[0].memory.id, active);
    }

    // -----------------------------------------------------------------------
    // Filters and scopes
    // -----------------------------------------------------------------------

    #[test]
    fn the_kind_filter_and_the_workspace_scope_both_narrow_the_window() {
        let db = testing::db();
        testing::add(&db, MemoryKind::Semantic, "Deploy facts", "deploy");
        testing::add(&db, MemoryKind::Reference, "Deploy file", "deploy");
        db.create_workspace("other", ReviewMode::Autonomous)
            .unwrap();
        db.add(
            &NewMemory::new(MemoryKind::Semantic, "Deploy elsewhere")
                .body("deploy")
                .workspace("other"),
        )
        .unwrap();

        let kinds = db
            .search(&SearchQuery {
                text: "deploy".to_owned(),
                kinds: vec![MemoryKind::Reference],
                ..SearchQuery::default()
            })
            .unwrap();
        assert_eq!(titles(&kinds), vec!["Deploy file"]);

        let scoped = db
            .search(&SearchQuery {
                text: "deploy".to_owned(),
                scope: Scope::One("other".to_owned()),
                ..SearchQuery::default()
            })
            .unwrap();
        assert_eq!(titles(&scoped), vec!["Deploy elsewhere"]);

        let everywhere = db
            .search(&SearchQuery {
                text: "deploy".to_owned(),
                scope: Scope::All,
                ..SearchQuery::default()
            })
            .unwrap();
        assert_eq!(everywhere.hits.len(), 3);
    }

    #[test]
    fn the_vector_leg_is_refused_rather_than_substituted() {
        let db = corpus();
        let error = db
            .search(&SearchQuery {
                text: "deploy".to_owned(),
                legs: Legs::Vector,
                ..SearchQuery::default()
            })
            .expect_err("no embedder, so `--legs vector` cannot be served");
        assert_eq!(error.exit_code(), crate::error::ExitCode::Config);
        // The message has to say what to do instead, or exit 4 is a wall.
        assert!(error.to_string().contains("--legs fts"), "{error}");
    }

    // -----------------------------------------------------------------------
    // Snippets
    // -----------------------------------------------------------------------

    #[test]
    fn a_snippet_marks_the_match_and_a_body_match_only() {
        let db = corpus();
        let outcome = search(&db, "migrations");
        let hit = &outcome.hits[0];
        assert_eq!(hit.memory.title, "Staging deploys need a manual migration");
        assert_eq!(
            hit.snippet,
            "Run **migrations** before the deploy; the pipeline does not do it."
        );
    }

    #[test]
    fn a_title_only_match_has_an_empty_snippet_rather_than_a_misleading_one() {
        // FTS5 returns the *first* tokens of the requested column when the match was in
        // another column — unhighlighted. Showing that would put body text under a title
        // match and read as though the body were where it matched.
        let db = corpus();
        let outcome = search(&db, "staging");
        let hit = &outcome.hits[0];
        assert_eq!(hit.memory.title, "Staging deploys need a manual migration");
        assert_eq!(hit.snippet, "");
    }

    #[test]
    fn a_multiline_body_produces_a_single_line_snippet() {
        let db = testing::db();
        testing::add(
            &db,
            MemoryKind::Procedural,
            "Deploy steps",
            "First deploy the thing.\n\n    Then\tverify   it.\n",
        );
        let outcome = search(&db, "deploy");
        let snippet = &outcome.hits[0].snippet;
        assert!(!snippet.contains('\n'), "{snippet:?}");
        assert!(!snippet.contains("  "), "{snippet:?}");
        assert_eq!(snippet, "First **deploy** the thing. Then verify it.");
    }

    #[test]
    fn the_snippet_length_flag_changes_how_much_is_shown() {
        let db = testing::db();
        testing::add(
            &db,
            MemoryKind::Semantic,
            "Deploy steps",
            "deploy one two three four five six seven eight nine ten eleven twelve",
        );
        let short = db
            .search(&SearchQuery {
                text: "deploy".to_owned(),
                snippet_tokens: 4,
                ..SearchQuery::default()
            })
            .unwrap();
        let long = db
            .search(&SearchQuery {
                text: "deploy".to_owned(),
                snippet_tokens: 20,
                ..SearchQuery::default()
            })
            .unwrap();
        assert!(
            short.hits[0].snippet.len() < long.hits[0].snippet.len(),
            "{} vs {}",
            short.hits[0].snippet,
            long.hits[0].snippet
        );
    }

    #[test]
    fn a_tag_match_shows_in_no_snippet_but_is_still_a_hit() {
        // The tags column is weighted and searchable, and a snippet is read from the body
        // only — so a tag-only match has a row, a title, tags, and no snippet. Asserted
        // because "matched but shows nothing" is otherwise reported as a bug.
        let db = testing::db();
        db.add(
            &NewMemory::new(MemoryKind::Semantic, "Unrelated heading")
                .body("nothing to see")
                .tags(vec!["kubernetes".to_owned()]),
        )
        .unwrap();
        let outcome = search(&db, "kubernetes");
        assert_eq!(outcome.hits.len(), 1);
        assert_eq!(outcome.hits[0].snippet, "");
        assert_eq!(outcome.hits[0].memory.tags, vec!["kubernetes".to_owned()]);
    }

    #[test]
    fn the_status_clause_is_not_what_keeps_a_draft_out() {
        // Proves the two mechanisms are independent rather than the same one twice: with
        // the row forced into the index by hand, the leg's `status = 'active'` clause is
        // what still hides it.
        let db = testing::db();
        let draft = testing::add(&db, MemoryKind::Semantic, "Draft deploy note", "deploy");
        db.conn()
            .execute(
                "UPDATE memories SET status = 'pending_approval' WHERE id = ?1",
                [&draft],
            )
            .unwrap();
        // Bypasses the `mem_au` trigger by writing to the index directly, which is the
        // drift `doctor` reports and the case this clause exists for.
        db.conn()
            .execute(
                "INSERT INTO memory_fts(rowid, title, excerpt, body, tags_text)
                 SELECT mem_id, title, excerpt, body, tags_text FROM memories WHERE id = ?1",
                [&draft],
            )
            .unwrap();
        assert_eq!(testing::fts_rows(&db).unwrap(), 1);
        assert!(
            search(&db, "deploy").hits.is_empty(),
            "the leg must not return a memory that is not active"
        );
    }

    #[test]
    fn status_is_a_field_the_caller_can_trust_even_though_it_is_always_active() {
        // `SearchHitJson` carries `status`, and a search can only ever produce `active` —
        // unless the trigger and the clause both fail. Pinned so the field's meaning is
        // written down rather than inferred from an absence.
        let db = corpus();
        for hit in search(&db, "deploy").hits {
            assert_eq!(hit.memory.status, Status::Active);
        }
    }
}
