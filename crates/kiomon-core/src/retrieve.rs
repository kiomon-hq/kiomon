// SPDX-License-Identifier: Apache-2.0

//! `retrieve`: the context packet (WP5.5).
//!
//! `search` answers "what matches"; `retrieve` answers "what should I use, given what I am
//! about to do". The caller declares an intent and gets a decision-ready packet: the
//! applicable facts, **one** best procedure, the relevant episodes, evidence pointers, and
//! warnings about what not to trust (expired, low confidence, superseded, contradictory).
//!
//! Ported from `platform/worker/src/lib/retrieve.ts` and the route schema in
//! `platform/worker/src/routes/retrieve.ts`, because this packet is the payload the hosted
//! `/api/*` routes serve and the published Python and TypeScript SDKs already parse. Field
//! names, the intent vocabulary, the applicability rule, the ranking formula, the warning
//! strings and the summary sentence are therefore **the wire's**, not this crate's taste.
//! `docs/CONTRACT.md` documents them; §9-D8 of the plan records why the local engine
//! follows them rather than inventing a nicer shape.
//!
//! Three properties are worth stating because the rest follows from them:
//!
//! - **No model, no network, no provider.** Everything here is derived from stored rows:
//!   the ranking is arithmetic over the candidate order, and every warning is a fact
//!   already in the database (an `expires_at` that has passed, a `confidence` below the
//!   threshold, an edge somebody wrote).
//! - **Bounded statements, whatever the corpus.** One search, one clock read, two edge
//!   seeks, one retrieval record. A packet that cost a query per hit would make `search`'s
//!   26 ms look cheap, and the corpus this has to stay usable on is the ten-thousand-memory
//!   one the performance budget is measured on.
//! - **The candidates arrive as whole rows.** The hosted implementation loads an envelope
//!   for its candidate ids because its search returns projections; local [`crate::search`]
//!   hydrates the full memory, so confidence, metadata and `expires_at` come with the hit
//!   and there is no envelope statement to write. That is a local advantage rather than a
//!   port decision, and it is why `retrieve` costs about what `search` costs.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use rusqlite::params_from_iter;
use serde::Serialize;
use serde_json::Value;

use crate::db::Database;
use crate::error::Result;
use crate::metadata;
use crate::model::{
    DEFAULT_SNIPPET_TOKENS, Legs, Memory, MemoryKind, Scope, SearchHit, SearchQuery, SearchSort,
};

/// The packet's default size. Six items is what the hosted handler defaults to, and the
/// number an agent's context budget was sized around.
pub const DEFAULT_PACKET_ITEMS: usize = 6;

/// The most items a packet may hold. Twenty is the hosted route's ceiling and the Python
/// SDK's `MAX_RETRIEVAL_LIMIT`; a packet is a briefing, not a page of results.
pub const MAX_PACKET_ITEMS: usize = 20;

/// How many candidates are fetched per item asked for.
///
/// Re-ranking needs a wider pool than it returns — applicability and kind priority both
/// reorder within it — and this is the hosted service's factor. Too small a pool makes the
/// intent weights decorative.
const CANDIDATE_FACTOR: usize = 4;

/// The floor under the candidate pool, so a two-item packet still re-ranks something.
const MIN_CANDIDATES: usize = 15;

/// What a confidence is worth when a writer claimed none.
///
/// The hosted envelope defaults to this, which is why an unclaimed confidence is *not*
/// warned about: `0.5 < 0.5` is false, and "nobody said" is not the same fact as "they said
/// it was shaky".
const DEFAULT_CONFIDENCE: f64 = 0.5;

/// Below this, a stored confidence is worth a warning. Matches the hosted service.
const LOW_CONFIDENCE: f64 = 0.5;

/// The longest query the route accepts, in characters. Enforced here so a query the hosted
/// API would reject with a 400 cannot quietly succeed locally and then fail the day the
/// same call is pointed at the hosted brain — or at `serve` (WP9), which answers that route.
pub const MAX_QUERY_CHARS: usize = 1000;

/// The most constraints the route accepts. Applicability is a fraction over constraint
/// tokens, so an unbounded list would let one long sentence drown out every candidate.
pub const MAX_CONSTRAINTS: usize = 16;

/// The most entities the route accepts. Carried for wire fidelity only — see
/// [`RetrieveQuery::entities`].
pub const MAX_ENTITIES: usize = 32;

/// The ranking weights, from the hosted service.
///
/// Position dominates on purpose: a candidate the legs ranked fourth is more likely to be
/// the right answer than one they ranked twentieth, and no amount of applicability
/// arithmetic should invert that on its own. Applicability can move a row past its
/// neighbours, not past the list.
const REL_WEIGHT: f64 = 0.5;
const APPLICABILITY_WEIGHT: f64 = 0.3;
const KIND_PRIORITY_WEIGHT: f64 = 0.2;

/// What a contradiction costs a candidate, and what being expired costs it.
///
/// Penalties rather than exclusions: a contradicted memory is exactly the one a caller
/// needs to see in order to resolve the disagreement, and an expired one is still evidence
/// about what was believed. Both are demoted, and both are warned about.
const CONFLICT_PENALTY: f64 = 0.5;
const STALE_PENALTY: f64 = 0.3;

/// The note on a `contradicts` pair, in the hosted wording.
const CONFLICT_NOTE: &str = "conflicting claims — both shown for verification";

/// What a condition says when the memory offers no better answer.
const FALLBACK_CONDITION: &str = "partial constraint match";

/// The stop words the applicability matcher ignores.
///
/// Ported verbatim from the hosted implementation, including the peculiarity that the list
/// is only consulted for tokens of two characters or more — every entry is three or more
/// characters, so the filter is what keeps `a`, `an` and `of` out.
const APPLICABILITY_STOP_WORDS: &[&str] = &[
    "a", "an", "the", "is", "are", "was", "were", "be", "been", "of", "to", "in", "on", "for",
    "and", "or", "but", "with", "that", "this", "it", "as", "at", "by", "from", "so", "can",
    "should", "must", "will", "would",
];

/// What the caller is trying to do. **Five, and these five**: the hosted route's `z.enum`,
/// the MCP tool's schema, and the Python SDK's `RetrievalIntent` all agree, and the plan's
/// earlier `understand|execute|recall` was a misremembering (§8's WP5.5 record). An SDK
/// that sends `verify` must not be answered with a usage error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Intent {
    /// Carry out an action or a routine.
    Execute,
    /// Decompose a task.
    Plan,
    /// Answer a question about the user.
    Answer,
    /// Check a factual claim.
    Verify,
    /// Recover from a failure.
    Debug,
}

impl Intent {
    /// Every intent, in the order the hosted route lists them.
    pub const ALL: [Self; 5] = [
        Self::Execute,
        Self::Plan,
        Self::Answer,
        Self::Verify,
        Self::Debug,
    ];

    /// The wire value. The vocabulary is the contract's, so it is spelled once, here.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Execute => "execute",
            Self::Plan => "plan",
            Self::Answer => "answer",
            Self::Verify => "verify",
            Self::Debug => "debug",
        }
    }

    /// Which kind of memory this intent wants first, and how the rest rank behind it.
    ///
    /// Transcribed from `INTENT_KIND_PRIORITY` in the hosted `lib/retrieve.ts`. It is the
    /// whole of what "intent-aware" means here: no model reads the intent, one table does.
    #[must_use]
    pub const fn kind_priority(self) -> [MemoryKind; 4] {
        match self {
            Self::Execute => [
                MemoryKind::Procedural,
                MemoryKind::Semantic,
                MemoryKind::Episodic,
                MemoryKind::Reference,
            ],
            Self::Plan => [
                MemoryKind::Procedural,
                MemoryKind::Episodic,
                MemoryKind::Semantic,
                MemoryKind::Reference,
            ],
            Self::Answer => [
                MemoryKind::Semantic,
                MemoryKind::Episodic,
                MemoryKind::Reference,
                MemoryKind::Procedural,
            ],
            Self::Verify => [
                MemoryKind::Reference,
                MemoryKind::Semantic,
                MemoryKind::Episodic,
                MemoryKind::Procedural,
            ],
            Self::Debug => [
                MemoryKind::Episodic,
                MemoryKind::Semantic,
                MemoryKind::Reference,
                MemoryKind::Procedural,
            ],
        }
    }
}

impl std::str::FromStr for Intent {
    type Err = String;

    /// Parsing is by wire value, so `--intent execute` and a JSON `"execute"` can never
    /// mean different things.
    fn from_str(text: &str) -> std::result::Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|intent| intent.as_str() == text)
            .ok_or_else(|| {
                let all: Vec<&str> = Self::ALL.iter().map(|intent| intent.as_str()).collect();
                format!("`{text}` is not an intent (one of: {})", all.join(", "))
            })
    }
}

/// How risky the caller believes the task is.
///
/// **Carried, never consulted.** The hosted route validates `risk_level`, passes it to the
/// service, and the service does not read it; this port keeps the field so the wire type is
/// complete and so WP9 has somewhere to put it, and reads it nowhere. Exposing it as a CLI
/// flag would teach a user that it changes the packet, which today would be a lie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskLevel {
    /// A misstep is cheap.
    Low,
    /// A misstep is expensive.
    Medium,
    /// A misstep is dangerous.
    High,
}

impl RiskLevel {
    /// The wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// How well a candidate fits the caller's stated constraints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applicability {
    /// At least half the constraint tokens appear in the candidate.
    Applicable,
    /// Some of them do.
    Conditional,
    /// None — or the caller stated no constraints at all. A packet cannot know what the
    /// task is when it was not told, and inventing relevance from nothing would be the
    /// first place this engine started guessing.
    Unknown,
}

impl Applicability {
    /// The wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Applicable => "applicable",
            Self::Conditional => "conditional",
            Self::Unknown => "unknown",
        }
    }

    /// What this is worth in the ranking.
    #[must_use]
    const fn score(self) -> f64 {
        match self {
            Self::Applicable => 1.0,
            Self::Conditional => 0.5,
            Self::Unknown => 0.0,
        }
    }
}

/// A `retrieve` request.
///
/// `entities` and `risk_level` exist because the route accepts them. **Neither is read**:
/// the hosted service's `retrieve()` never consults them either — they are validated,
/// passed along, and dropped — so reading them locally would make the same call rank
/// differently against localhost than against the hosted brain, which is the one thing
/// WP5.5 must not do. They are here so WP9's route can accept a well-formed request
/// without a rewrite.
#[derive(Debug, Clone)]
pub struct RetrieveQuery {
    /// What the caller is about to do.
    pub intent: Intent,
    /// The query, verbatim. Sanitising is [`crate::search`]'s job, not the caller's.
    pub text: String,
    /// Which workspaces to cover.
    pub scope: Scope,
    /// Restrict the candidate pool to these kinds. Empty means every kind — and an intent
    /// that cannot be served is not the same as an empty pool: asking for `reference`
    /// memories under `execute` returns references, ranked last for that intent, rather
    /// than nothing.
    pub kinds: Vec<MemoryKind>,
    /// Hints about the task, used only to judge applicability. Not stored and not sent
    /// anywhere: the caller's task state stays the caller's, which is the hosted product's
    /// rule and this one's.
    pub constraints: Vec<String>,
    /// Accepted for wire fidelity. Never read — see the type's own note.
    pub entities: Vec<String>,
    /// Accepted for wire fidelity. Never read — see the type's own note.
    pub risk_level: Option<RiskLevel>,
    /// How many items the packet may hold, 1–[`MAX_PACKET_ITEMS`]. Clamped rather than
    /// refused, because the hosted route clamps at its ceiling and rejects only a value
    /// outside `1..=20`, which the CLI already rejects as a usage error.
    pub limit: usize,
    /// Whether the items injected into the packet count as retrievals against their decay
    /// clock. Default on for the same reason `search` is: a memory that was handed to an
    /// agent *is* a memory that was used.
    pub record: bool,
    /// Which legs the underlying search may use.
    ///
    /// `Auto` — both, when an embedder is configured — is what a caller wants almost
    /// always, and it is why this was not a parameter until WP7: with one leg there was
    /// nothing to choose. `Fts` is how a packet is still built on a machine whose embedder
    /// is down *and* how the benchmark measures the packet's own cost against the lexical
    /// search under it.
    pub legs: Legs,
}

impl Default for RetrieveQuery {
    fn default() -> Self {
        Self {
            intent: Intent::Answer,
            text: String::new(),
            scope: Scope::One(crate::model::DEFAULT_WORKSPACE.to_owned()),
            kinds: Vec::new(),
            constraints: Vec::new(),
            entities: Vec::new(),
            risk_level: None,
            limit: DEFAULT_PACKET_ITEMS,
            record: true,
            legs: Legs::Auto,
        }
    }
}

/// One memory inside a packet, with the claim it contributes and where it came from.
///
/// The field names are the wire's — `memory_id`, not `id` — because a packet item is not a
/// memory and the two are read by different code. `condition` is present **only** for a
/// [`Applicability::Conditional`] item and `source_ref` only when one was stored: the
/// absent-key shape is the hosted JSON's, asserted below, because a `null` where the wire
/// has no key is what an SDK's strict decode would find.
#[derive(Debug, Clone, Serialize)]
pub struct PacketItem {
    /// The memory's id.
    pub memory_id: String,
    /// The wire value of its kind.
    pub kind: &'static str,
    /// The actionable line: a claim, a routine's steps, an episode's outcome, or the
    /// title.
    pub claim: String,
    /// The writer's confidence, or `0.5` when none was claimed.
    pub confidence: f64,
    /// Where it came from: `user_saved`, `agent_reflection`, `import`, `connector_sync`.
    pub authority: String,
    /// How well it fits the caller's constraints.
    pub applicability: &'static str,
    /// Why it is only conditionally applicable. Absent unless `applicability` is
    /// `conditional`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// A pointer to what the memory was derived from. Absent when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
}

/// A pointer to the evidence behind a packet, rather than the evidence's text.
///
/// A `reference` memory is the user's library: what belongs in a prompt is where to look,
/// not the file.
#[derive(Debug, Clone, Serialize)]
pub struct EvidenceRef {
    /// The memory's id.
    pub memory_id: String,
    /// The URL it was saved from, when there was one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<String>,
    /// The excerpt, which is where the useful locator (a page, a section) usually lives.
    /// Absent rather than empty when the memory has no excerpt: an empty string is not a
    /// span.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_span: Option<String>,
}

/// Two memories that disagree, both of which are in the packet.
///
/// Surfaced, never resolved: which one is right is the caller's call, and this engine has
/// no basis to make it. Empty when nothing contradicts anything — which is most corpora,
/// since a contradiction is a claim somebody made and not a similarity a ranker inferred —
/// and never empty merely because the check is missing: a failed read is reported in
/// `warnings` instead.
#[derive(Debug, Clone, Serialize)]
pub struct ConflictNote {
    /// One side.
    pub memory_id_a: String,
    /// The other.
    pub memory_id_b: String,
    /// The human-readable note, in the hosted wording.
    pub note: &'static str,
}

/// The packet itself.
#[derive(Debug, Clone, Serialize)]
pub struct ContextPacket {
    /// One sentence about what was matched, in the hosted wording.
    pub summary: String,
    /// Applicable facts, in rank order.
    pub facts: Vec<PacketItem>,
    /// **The** best procedure, at most one: two competing routines in one packet is a
    /// decision the caller cannot make better than this engine can, so the runner-up is
    /// reported in `retrieved_ids` instead of competing for the same slot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub procedure: Option<PacketItem>,
    /// Relevant moments, in rank order.
    pub episodes: Vec<PacketItem>,
    /// Where to look, for the memories that are pointers rather than claims.
    pub evidence: Vec<EvidenceRef>,
    /// Instructions to honour: expired, low confidence, superseded, contradictory.
    pub warnings: Vec<String>,
    /// Active disagreements among the items.
    pub conflicts: Vec<ConflictNote>,
    /// Every memory injected into the packet — which is exactly what `outcome` is fed, so
    /// that credit lands on what was used rather than on what was fetched.
    pub retrieved_ids: Vec<String>,
}

impl ContextPacket {
    /// The packet that says nothing matched.
    ///
    /// `procedure` is absent rather than null, and so is every other optional field: this
    /// is the shape a caller sees whenever a query has no answer, so it is the shape most
    /// worth getting byte-exact.
    #[must_use]
    pub fn empty(intent: Intent) -> Self {
        Self {
            summary: format!("No memories matched intent \"{}\".", intent.as_str()),
            facts: Vec::new(),
            procedure: None,
            episodes: Vec::new(),
            evidence: Vec::new(),
            warnings: Vec::new(),
            conflicts: Vec::new(),
            retrieved_ids: Vec::new(),
        }
    }

    /// Every item across the two buckets and the procedure slot. For rendering; the
    /// packet's own order is `facts`, `procedure`, `episodes`.
    #[must_use]
    pub fn items(&self) -> Vec<&PacketItem> {
        let mut items: Vec<&PacketItem> = self.facts.iter().collect();
        items.extend(self.procedure.iter());
        items.extend(self.episodes.iter());
        items
    }
}

/// What the typed edges among a candidate window say.
///
/// A struct rather than a triple because the third field is not a result: a failed read is
/// reported *inside* a successful packet, and a caller that had to destructure a tuple to
/// notice would be one refactor away from dropping it.
struct EdgeSignals {
    /// Target id -> the source that superseded it.
    superseded_by: BTreeMap<String, String>,
    /// Source id -> the targets that contradict it.
    contradicts: BTreeMap<String, Vec<String>>,
    /// What to tell the caller when the read could not run.
    warning: Option<String>,
}

/// A candidate after scoring: the row, its claim, and why it moved.
struct Scored {
    memory: Memory,
    claim: String,
    applicability: Applicability,
    stale: bool,
    score: f64,
}

impl Database {
    /// Build the packet for `query`.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Invalid`] for a query or constraint list longer than the hosted
    /// route accepts, [`crate::Error::NotFound`] for an unknown workspace when the scope is
    /// one workspace, and whatever the candidate search raises.
    pub fn retrieve(&self, query: &RetrieveQuery) -> Result<ContextPacket> {
        let characters = query.text.chars().count();
        if characters > MAX_QUERY_CHARS {
            return Err(crate::Error::Invalid(format!(
                "the query is {characters} characters; the retrieval route accepts {MAX_QUERY_CHARS}"
            )));
        }
        if query.constraints.len() > MAX_CONSTRAINTS {
            return Err(crate::Error::Invalid(format!(
                "{} constraints given; the retrieval route accepts {MAX_CONSTRAINTS}",
                query.constraints.len()
            )));
        }
        let limit = query.limit.clamp(1, MAX_PACKET_ITEMS);

        // One search for the whole pool. `record: false` is the only place in this module
        // where something is deliberately *not* done: the retrieval record belongs to the
        // ids that were injected into the packet, not to the wider window fetched to rank
        // them, and recording both would count one use twice. The record happens once, at
        // the end, over `retrieved_ids`.
        let candidates = self.search(&SearchQuery {
            text: query.text.clone(),
            scope: query.scope.clone(),
            kinds: query.kinds.clone(),
            limit: u32::try_from((limit * CANDIDATE_FACTOR).max(MIN_CANDIDATES))
                .unwrap_or(u32::MAX),
            snippet_tokens: DEFAULT_SNIPPET_TOKENS,
            // Relevance, always: the ranking below is a function of the candidate order, so
            // that order is the one the legs produced and cannot be the caller's choice.
            // Pinned memories still lead it, which is this contract's business, not the
            // hosted one's.
            sort: SearchSort::Relevance,
            legs: query.legs,
            record: false,
        })?;

        // A packet built from one leg when two were asked for is worth saying out loud.
        // The `search` verb puts it on stderr and in `degraded`; a packet has no such
        // field on the wire, so it becomes a warning — which is where everything a caller
        // cannot otherwise see about this packet already goes.
        let degraded = candidates.degraded.then(|| {
            format!(
                "this packet was built from the full-text leg alone ({}), so a memory that                  only meaning would have found is not in it",
                candidates.reason
            )
        });

        if candidates.hits.is_empty() {
            let mut packet = ContextPacket::empty(query.intent);
            packet.warnings.extend(degraded);
            return Ok(packet);
        }

        let now = crate::db::now(self.conn())?;
        let edges = self.typed_edges(&candidates.hits);

        let priority = query.intent.kind_priority();
        let mut scored: Vec<Scored> = candidates
            .hits
            .iter()
            .enumerate()
            .map(|(index, hit)| {
                let memory = &hit.memory;
                let text = candidate_text(memory, &hit.snippet);
                let applicability = applicability_of(&query.constraints, &text);
                let stale = is_stale(
                    memory.expires_at.as_deref(),
                    meta_str(&memory.memory_metadata, metadata::KEY_VALID_UNTIL),
                    &now,
                );

                // Position in the fused list, 0-based in all but name: the first candidate
                // scores 1/1 and the eleventh 1/11, which is what lets the pool's order
                // survive the re-ranking instead of being replaced by it.
                #[allow(clippy::cast_precision_loss)] // a candidate window, never near 2^53
                let position = index as f64;
                let rel = 1.0 / (1.0 + position);
                let kind_priority = priority
                    .iter()
                    .position(|kind| *kind == memory.kind)
                    .map_or(0.0, |index| {
                        #[allow(clippy::cast_precision_loss)] // four kinds
                        let rank = index as f64;
                        #[allow(clippy::cast_precision_loss)]
                        let total = priority.len() as f64;
                        (total - rank) / total
                    });

                let conflicted = edges.superseded_by.contains_key(&memory.id)
                    || in_conflict(&edges.contradicts, &memory.id);
                let score = REL_WEIGHT * rel
                    + APPLICABILITY_WEIGHT * applicability.score()
                    + KIND_PRIORITY_WEIGHT * kind_priority
                    - if conflicted { CONFLICT_PENALTY } else { 0.0 }
                    - if stale { STALE_PENALTY } else { 0.0 };

                Scored {
                    memory: memory.clone(),
                    claim: claim_for(memory, &hit.snippet),
                    applicability,
                    stale,
                    score,
                }
            })
            .collect();

        // A total order over `f64`, so the sort is stable and deterministic: two candidates
        // with the same score keep the order the legs put them in rather than an order that
        // depends on the allocator.
        scored.sort_by(|a, b| b.score.total_cmp(&a.score));
        scored.truncate(limit);

        let packet = build_packet(&scored, query.intent, &edges, degraded);

        if query.record && !packet.retrieved_ids.is_empty() {
            self.record_retrieval(&packet.retrieved_ids)?;
        }
        Ok(packet)
    }

    /// The `supersedes` and `contradicts` edges among a candidate window.
    ///
    /// Two statements rather than one `(source_id IN … OR target_id IN …)`: an `OR` across
    /// two columns cannot be served by either index, so the single-statement form degrades
    /// to a scan of every edge in the database. Each half is a seek on `idx_link_source` or
    /// `idx_link_target`. That is the hosted implementation's reasoning, measured there and
    /// kept here.
    ///
    /// **Fail-open, but not silently.** A typed-edge read is an enrichment: it decides
    /// whether a warning is printed, not what the packet contains, so a database failure
    /// here must not fail a retrieval. It must not be invisible either — `conflicts: []`
    /// because nothing contradicts anything and `conflicts: []` because the read failed are
    /// different facts, so the failure goes in the packet's warnings. Since WP5 the first
    /// one is reachable: `link --relation contradicts` writes the edge this reads.
    fn typed_edges(&self, hits: &[SearchHit]) -> EdgeSignals {
        let mut superseded_by = BTreeMap::new();
        let mut contradicts: BTreeMap<String, Vec<String>> = BTreeMap::new();

        let ids: Vec<&str> = hits.iter().map(|hit| hit.memory.id.as_str()).collect();
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let mut edges = Vec::new();
        let mut failure = None;

        for column in ["source_id", "target_id"] {
            // The column name is one of two literals chosen here, never user input; the
            // bound parameters are the ids. See the contract's note on interpolation.
            let sql = format!(
                "SELECT source_id, target_id, relation FROM memory_links \
                 WHERE valid_to IS NULL AND relation IN ('supersedes', 'contradicts') \
                 AND {column} IN ({placeholders})"
            );
            let read = || -> Result<Vec<(String, String, String)>> {
                let mut statement = self.conn().prepare(&sql)?;
                let rows = statement.query_map(params_from_iter(ids.iter()), |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                Ok(out)
            };
            match read() {
                Ok(rows) => edges.extend(rows),
                Err(error) => {
                    failure = Some(format!(
                        "the conflict check could not run ({error}), so this packet reports no \
                         conflicts because it could not look rather than because there are none."
                    ));
                }
            }
        }

        for (source, target, relation) in edges {
            if relation == "supersedes" {
                superseded_by.insert(target, source);
            } else {
                // Push in place: the set-and-spread form rebuilds the vector per edge, so a
                // source with k contradicting targets costs O(k²).
                contradicts.entry(source).or_default().push(target);
            }
        }

        EdgeSignals {
            superseded_by,
            contradicts,
            warning: failure,
        }
    }
}

/// Whether a memory is either end of a `contradicts` edge.
///
/// Computed once per candidate rather than re-derived inside the note loop, where the
/// hosted version allocated a fresh array of arrays per row.
fn in_conflict(contradicts: &BTreeMap<String, Vec<String>>, id: &str) -> bool {
    contradicts.contains_key(id)
        || contradicts
            .values()
            .any(|targets| targets.iter().any(|target| target == id))
}

/// Rank the candidates into buckets, and derive every warning from what is stored.
fn build_packet(
    scored: &[Scored],
    intent: Intent,
    edges: &EdgeSignals,
    search_warning: Option<String>,
) -> ContextPacket {
    let mut facts = Vec::new();
    let mut episodes = Vec::new();
    let mut evidence = Vec::new();
    let mut procedure: Option<PacketItem> = None;
    let mut warnings: Vec<String> = edges.warning.iter().cloned().collect();
    // The search-level warning goes first: it is about the whole packet, and everything
    // after it is about individual items.
    warnings.extend(search_warning);
    let mut conflicts = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();

    for candidate in scored {
        let memory = &candidate.memory;
        // A condition is a *stored* reason, not a guess: the scope a routine applies in, or
        // the trigger that arms it, or an honest admission that only part of the
        // constraints matched.
        let condition = (candidate.applicability == Applicability::Conditional).then(|| {
            meta_str(&memory.memory_metadata, metadata::KEY_SCOPE)
                .or_else(|| meta_str(&memory.memory_metadata, metadata::KEY_TRIGGER))
                .unwrap_or(FALLBACK_CONDITION)
                .to_owned()
        });
        let item = PacketItem {
            memory_id: memory.id.clone(),
            kind: memory.kind.as_str(),
            claim: candidate.claim.clone(),
            confidence: memory.confidence.unwrap_or(DEFAULT_CONFIDENCE),
            authority: memory.source.clone(),
            applicability: candidate.applicability.as_str(),
            condition,
            source_ref: source_ref_for(memory),
        };

        match memory.kind {
            MemoryKind::Semantic => facts.push(item),
            MemoryKind::Episodic => episodes.push(item),
            MemoryKind::Procedural => {
                // At most one: the first in rank order wins the slot, and the rest are
                // still in `retrieved_ids`.
                if procedure.is_none() {
                    procedure = Some(item);
                }
            }
            MemoryKind::Reference => evidence.push(EvidenceRef {
                memory_id: item.memory_id.clone(),
                source_uri: memory.source_url.clone(),
                source_span: (!memory.excerpt.is_empty()).then(|| memory.excerpt.clone()),
            }),
        }

        // Warnings, from stored signals only — no model, and nothing inferred.
        if candidate.stale {
            let when = memory.expires_at.clone().or_else(|| {
                meta_str(&memory.memory_metadata, metadata::KEY_VALID_UNTIL).map(str::to_owned)
            });
            warnings.push(match when {
                Some(when) => format!("\"{}\" is expired (valid until {when}).", memory.title),
                None => format!("\"{}\" is expired.", memory.title),
            });
        }
        let confidence = memory.confidence.unwrap_or(DEFAULT_CONFIDENCE);
        if confidence < LOW_CONFIDENCE {
            warnings.push(format!(
                "\"{}\" has low confidence ({confidence:.2}).",
                memory.title
            ));
        }
        if let Some(winner) = edges.superseded_by.get(&memory.id) {
            warnings.push(format!(
                "\"{}\" is superseded by {winner}; prefer the newer memory.",
                memory.title
            ));
        }

        // Contradictions are surfaced, not auto-resolved. The pair is reported once: both
        // ends are usually in the packet, and the same disagreement twice is noise.
        for other in edges.contradicts.get(&memory.id).into_iter().flatten() {
            let key = if memory.id < *other {
                (memory.id.clone(), other.clone())
            } else {
                (other.clone(), memory.id.clone())
            };
            if !seen.insert(key) {
                continue;
            }
            conflicts.push(ConflictNote {
                memory_id_a: memory.id.clone(),
                memory_id_b: other.clone(),
                note: CONFLICT_NOTE,
            });
            warnings.push(format!(
                "Conflicting memories: \"{}\" vs \"{other}\".",
                memory.title
            ));
        }
    }

    let summary = summary_for(intent, facts.len(), procedure.is_some(), episodes.len());
    let retrieved_ids = scored
        .iter()
        .map(|candidate| candidate.memory.id.clone())
        .collect();

    ContextPacket {
        summary,
        facts,
        procedure,
        episodes,
        evidence,
        warnings,
        conflicts,
        retrieved_ids,
    }
}

/// The searchable surface text used for applicability matching.
///
/// The same surface the hosted implementation builds: title, snippet, excerpt, tags, and
/// the envelope's own words. It includes the matched snippet on purpose — a constraint that
/// appears in the passage which matched is evidence about the passage, not about the query.
fn candidate_text(memory: &Memory, snippet: &str) -> String {
    let mut parts: Vec<String> = vec![
        memory.title.clone(),
        strip_marks(snippet),
        memory.excerpt.clone(),
        memory.tags.join(" "),
    ];
    let envelope = &memory.memory_metadata;
    parts.extend(meta_list(envelope, metadata::KEY_CLAIMS));
    parts.extend(meta_list(envelope, metadata::KEY_STEPS));
    parts.extend(meta_list(envelope, metadata::KEY_ENTITIES));
    for key in [
        metadata::KEY_TRIGGER,
        metadata::KEY_OUTCOME,
        metadata::KEY_SCOPE,
    ] {
        if let Some(text) = meta_str(envelope, key) {
            parts.push(text.to_owned());
        }
    }
    parts
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// How well a candidate matches the constraints the caller stated.
///
/// No constraints means `unknown`: this engine cannot observe the task, and it will not
/// pretend that a query is one.
#[must_use]
pub fn applicability_of(constraints: &[String], candidate_text: &str) -> Applicability {
    if constraints.is_empty() {
        return Applicability::Unknown;
    }
    let wanted: BTreeSet<String> = constraints
        .iter()
        .flat_map(|constraint| tokenize(constraint))
        .collect();
    if wanted.is_empty() {
        return Applicability::Unknown;
    }
    let present = tokenize(candidate_text);
    let shared = wanted
        .iter()
        .filter(|token| present.contains(*token))
        .count();
    #[allow(clippy::cast_precision_loss)] // token counts, nowhere near 2^53
    let ratio = shared as f64 / wanted.len() as f64;
    if ratio >= 0.5 {
        Applicability::Applicable
    } else if shared > 0 {
        Applicability::Conditional
    } else {
        Applicability::Unknown
    }
}

/// The tokens applicability matching compares.
///
/// Ported verbatim from the hosted `RetrieveService.tokenize`: lowercase ASCII
/// alphanumerics, two characters or more, stop words dropped. **This is a Latin-only
/// rule**, so a constraint written in Japanese matches nothing and leaves every item
/// `unknown`. The hosted product has the same hole, and closing it locally would make the
/// same call rank differently against localhost than against `kiomon.com` — the failure
/// WP5.5 exists to prevent. The right fix is the trigram tokenizer (see the plan's D5 note),
/// applied in both places.
#[must_use]
pub fn tokenize(text: &str) -> BTreeSet<String> {
    static STOP: OnceLock<BTreeSet<&'static str>> = OnceLock::new();
    let stop = STOP.get_or_init(|| APPLICABILITY_STOP_WORDS.iter().copied().collect());

    let mut tokens = BTreeSet::new();
    let mut current = String::new();
    for character in text.chars() {
        if character.is_ascii_alphanumeric() {
            current.push(character.to_ascii_lowercase());
        } else if current.len() >= 2 && !stop.contains(current.as_str()) {
            tokens.insert(std::mem::take(&mut current));
        } else {
            current.clear();
        }
    }
    if current.len() >= 2 && !stop.contains(current.as_str()) {
        tokens.insert(current);
    }
    tokens
}

/// Whether a memory should be presented as no longer believed.
///
/// Two independent stored deadlines, and either one is enough: `expires_at`, which
/// `kiomon outcome outdated` sets and decay acts on, and the envelope's own `valid_until`,
/// which a writer chose.
///
/// The comparison is **lexicographic**, because this crate has no date library by design
/// (SQLite is the clock). That is exactly right for the timestamps this engine writes
/// (`%Y-%m-%dT%H:%M:%SZ`, so string order is time order) and for a date-only `valid_until`,
/// and it is off by the offset for an explicit `+02:00`. A value that does not even look
/// like a timestamp is treated as no deadline at all, which is what the hosted
/// `Date.parse` returning `NaN` amounts to.
#[must_use]
pub fn is_stale(expires_at: Option<&str>, valid_until: Option<&str>, now: &str) -> bool {
    [expires_at, valid_until]
        .into_iter()
        .flatten()
        .any(|deadline| looks_like_a_timestamp(deadline) && deadline < now)
}

/// Whether a string could be compared against a timestamp at all.
fn looks_like_a_timestamp(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 8
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
}

/// The actionable line for an item.
///
/// A claim if the memory made one, a routine's steps, an episode's outcome, and the title
/// otherwise. This is a *summary of what is already stored*, never an inference about it:
/// the field a caller reads first is the field the writer filled in.
#[must_use]
pub fn claim_for(memory: &Memory, snippet: &str) -> String {
    let envelope = &memory.memory_metadata;
    match memory.kind {
        MemoryKind::Semantic => {
            if let Some(claim) = meta_list(envelope, metadata::KEY_CLAIMS).next() {
                return claim;
            }
        }
        MemoryKind::Procedural => {
            let steps: Vec<String> = meta_list(envelope, metadata::KEY_STEPS).collect();
            if !steps.is_empty() {
                return steps.join(" → ");
            }
            if let Some(trigger) = meta_str(envelope, metadata::KEY_TRIGGER) {
                return format!("When: {trigger}");
            }
        }
        MemoryKind::Episodic => {
            if let Some(outcome) = meta_str(envelope, metadata::KEY_OUTCOME) {
                return outcome.to_owned();
            }
        }
        MemoryKind::Reference => {}
    }
    if memory.title.is_empty() {
        strip_marks(snippet).trim().to_owned()
    } else {
        memory.title.clone()
    }
}

/// Where an item came from, when the memory points somewhere.
#[must_use]
pub fn source_ref_for(memory: &Memory) -> Option<String> {
    if memory.kind == MemoryKind::Semantic
        && let Some(reference) =
            meta_list(&memory.memory_metadata, metadata::KEY_SOURCE_REFS).next()
    {
        return Some(reference);
    }
    memory.source_url.clone()
}

/// The summary sentence, in the hosted wording.
///
/// The hosted version appends ` Graph activation added N linked candidates.` when its graph
/// leg contributed. The local packet does not walk the graph — its graph *leg* is v1.1
/// (§4.3) even though the graph itself landed in WP5 — so this cannot say that, and a
/// summary claiming a contribution the packet did not have would be worse than a shorter
/// sentence. The graph's contribution to a local packet is the `conflicts` and supersession
/// warnings, which are reported where they belong rather than in the count.
#[must_use]
pub fn summary_for(intent: Intent, facts: usize, procedure: bool, episodes: usize) -> String {
    let fact_word = if facts == 1 { "fact" } else { "facts" };
    let episode_word = if episodes == 1 { "episode" } else { "episodes" };
    format!(
        "{facts} {fact_word}, {} procedure, {episodes} {episode_word} matched intent \"{}\".",
        usize::from(procedure),
        intent.as_str()
    )
}

/// Strip the highlight markers from a snippet.
///
/// Snippets carry `**` around matched terms for the terminal's benefit (`docs/CONTRACT.md`),
/// and a claim is prose, so the markers come off before it is put in a packet. The hosted
/// version strips `<mark>`; the two are the same idea in different rendering layers.
#[must_use]
fn strip_marks(text: &str) -> String {
    text.replace("**", "")
}

/// A string from an envelope, if the envelope has one.
///
/// Envelopes are validated on the way in, but they are JSON in a column anybody can edit,
/// so a read that assumed its types would be a panic waiting for a hand-edited database.
fn meta_str<'a>(envelope: &'a Value, key: &str) -> Option<&'a str> {
    envelope.get(key)?.as_str()
}

/// The string items of an array in an envelope, skipping anything that is not a string.
fn meta_list<'a>(envelope: &'a Value, key: &str) -> impl Iterator<Item = String> + 'a {
    envelope
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NewMemory, Relation};
    use crate::testing;
    use serde_json::json;

    fn query(intent: Intent, text: &str) -> RetrieveQuery {
        RetrieveQuery {
            intent,
            text: text.to_owned(),
            ..RetrieveQuery::default()
        }
    }

    /// Four memories of one kind each, all matching "deploy".
    fn four_kinds(db: &Database) -> [String; 4] {
        let semantic = db
            .add(
                &NewMemory::new(
                    MemoryKind::Semantic,
                    "Staging deploys need a manual migration",
                )
                .body("The migrate step does not run automatically on deploy to staging.")
                .confidence(0.9)
                .metadata(Some(
                    json!({"claims": ["migrations are manual on staging"]}),
                )),
            )
            .expect("the semantic memory must be written")
            .id;
        let procedural = db
            .add(
                &NewMemory::new(MemoryKind::Procedural, "How to deploy to staging")
                    .body("Run the deploy script and then the migration by hand.")
                    .metadata(Some(json!({
                        "trigger": "a deploy is scheduled",
                        "steps": ["run the deploy script", "run the migration"]
                    }))),
            )
            .expect("the routine must be written")
            .id;
        let episodic = db
            .add(
                &NewMemory::new(MemoryKind::Episodic, "The deploy that broke staging")
                    .body("We deployed on a Friday and staging lost its schema.")
                    .metadata(Some(json!({"outcome": "staging was down for an hour"}))),
            )
            .expect("the episode must be written")
            .id;
        let reference = db
            .add(
                &NewMemory::new(MemoryKind::Reference, "Staging deploy runbook")
                    .body("The deploy runbook, page 12.")
                    .excerpt("page 12")
                    .source_url("https://example.com/runbook"),
            )
            .expect("the reference must be written")
            .id;
        [semantic, procedural, episodic, reference]
    }

    #[test]
    fn the_five_intents_are_the_ones_the_route_validates() {
        let wire: Vec<&str> = Intent::ALL.iter().map(|intent| intent.as_str()).collect();
        assert_eq!(wire, ["execute", "plan", "answer", "verify", "debug"]);
        for intent in Intent::ALL {
            assert_eq!(
                intent.as_str().parse::<Intent>(),
                Ok(intent),
                "an intent must parse from its own wire value"
            );
        }
        let error = "understand".parse::<Intent>().expect_err("not an intent");
        assert!(error.contains("execute"), "{error}");
    }

    #[test]
    fn the_kind_priority_table_is_the_hosted_one() {
        // Every kind exactly once per intent: the weights divide by the list's length, so a
        // kind missing from it would silently score zero for components it should lead.
        for intent in Intent::ALL {
            let priority = intent.kind_priority();
            let mut kinds: Vec<&str> = priority.iter().map(|kind| kind.as_str()).collect();
            kinds.sort_unstable();
            assert_eq!(kinds, ["episodic", "procedural", "reference", "semantic"]);
        }
        assert_eq!(
            Intent::Execute.kind_priority()[0],
            MemoryKind::Procedural,
            "execute wants the routine first"
        );
        assert_eq!(
            Intent::Debug.kind_priority()[0],
            MemoryKind::Episodic,
            "debug wants what happened first"
        );
        assert_eq!(
            Intent::Verify.kind_priority()[0],
            MemoryKind::Reference,
            "verify wants the source first"
        );
    }

    #[test]
    fn a_pool_that_fits_the_packet_injects_the_same_memories_under_any_intent() {
        // The plan's acceptance criterion for this work package said "one query under two
        // intents returns the same underlying ids in a different shape". Half of that is
        // true and half is not, and the difference only shows when the packet has to leave
        // something out — which the next test does. With one memory per kind and room for
        // all of them, intent changes the summary and nothing else: every kind has its own
        // bucket, so nothing inside the packet can move.
        //
        // That is a property, not a shortcoming: a caller with a small brain gets every
        // relevant memory whatever it declares, and the intent matters exactly when a
        // choice has to be made.
        let db = testing::db();
        let ids = four_kinds(&db);

        let execute = db
            .retrieve(&query(Intent::Execute, "deploy staging"))
            .expect("a packet");
        let answer = db
            .retrieve(&query(Intent::Answer, "deploy staging"))
            .expect("a packet");

        let mut first: Vec<&str> = execute.retrieved_ids.iter().map(String::as_str).collect();
        let mut second: Vec<&str> = answer.retrieved_ids.iter().map(String::as_str).collect();
        first.sort_unstable();
        second.sort_unstable();
        assert_eq!(
            first, second,
            "nothing was cut, so both intents inject the same memories"
        );
        assert_eq!(execute.retrieved_ids.len(), ids.len());
        assert_eq!(
            execute
                .procedure
                .as_ref()
                .map(|item| item.memory_id.as_str()),
            Some(ids[1].as_str()),
            "execute must lead with the routine"
        );
        assert_eq!(
            answer
                .procedure
                .as_ref()
                .map(|item| item.memory_id.as_str()),
            Some(ids[1].as_str()),
            "answer still injects the routine — it just ranks it last"
        );
        assert_ne!(
            execute.summary, answer.summary,
            "the summary names the intent, so two intents cannot produce one sentence"
        );
        assert_eq!(
            execute.summary,
            "1 fact, 1 procedure, 1 episode matched intent \"execute\"."
        );
        assert_eq!(
            answer.summary,
            "1 fact, 1 procedure, 1 episode matched intent \"answer\"."
        );
    }

    #[test]
    fn an_intent_decides_which_memories_fit_when_the_pool_is_wider_than_the_packet() {
        // This is what intent-aware retrieval *does*: it changes the choice, not the
        // presentation. Twenty facts and one routine, a packet of eighteen — so the
        // routine's fate depends entirely on how much the intent values a routine.
        //
        // The construction is deliberately lopsided: the facts carry the query in their
        // titles, the routine only in its body, and bm25 weights a title over a body. So the
        // routine is the last candidate whatever SQLite does with ties, which is what makes
        // this an assertion rather than a coin flip. Given that, the arithmetic is forced:
        // three positions down the list the position weight is already small, and a
        // kind-priority swing of 0.2 is larger than the whole spread at the tail.
        let db = testing::db();
        let routine = db
            .add(
                &NewMemory::new(MemoryKind::Procedural, "How to ship a release")
                    .body("deploy staging, deploy staging, deploy staging"),
            )
            .expect("the routine must be written")
            .id;
        let mut facts = Vec::new();
        for index in 0..20 {
            facts.push(testing::add(
                &db,
                MemoryKind::Semantic,
                &format!("Deploy staging note {index}"),
                "deploy staging",
            ));
        }

        let execute = db
            .retrieve(&RetrieveQuery {
                limit: 18,
                ..query(Intent::Execute, "deploy staging")
            })
            .expect("a packet");
        let answer = db
            .retrieve(&RetrieveQuery {
                limit: 18,
                ..query(Intent::Answer, "deploy staging")
            })
            .expect("a packet");

        assert_eq!(execute.retrieved_ids.len(), 18);
        assert_eq!(answer.retrieved_ids.len(), 18);
        assert!(
            execute.retrieved_ids.contains(&routine),
            "execute values a routine above eighteen facts"
        );
        assert!(
            !answer.retrieved_ids.contains(&routine),
            "answer does not, and there is no consolation slot either"
        );
        assert!(execute.procedure.is_some());
        assert!(answer.procedure.is_none());
        assert_ne!(
            execute.retrieved_ids, answer.retrieved_ids,
            "the same query over the same corpus must still produce two packets"
        );
        let cut: Vec<String> = answer
            .retrieved_ids
            .iter()
            .filter(|id| !execute.retrieved_ids.contains(id))
            .cloned()
            .collect();
        assert_eq!(cut.len(), 1, "exactly one fact made room for the routine");
        assert!(
            facts.contains(&cut[0]),
            "and it was a fact, not the routine"
        );
    }

    #[test]
    fn every_kind_lands_in_its_own_bucket_and_evidence_keeps_its_locator() {
        let db = testing::db();
        let ids = four_kinds(&db);
        let packet = db
            .retrieve(&query(Intent::Answer, "deploy staging"))
            .expect("a packet");

        assert_eq!(packet.facts[0].memory_id, ids[0]);
        assert_eq!(packet.facts[0].claim, "migrations are manual on staging");
        assert_eq!(
            packet.facts[0].confidence.to_bits(),
            0.9_f64.to_bits(),
            "the stored confidence, bit for bit: it came out of a column, not a sum"
        );
        assert_eq!(packet.facts[0].authority, "user_saved");
        assert_eq!(packet.episodes[0].memory_id, ids[2]);
        assert_eq!(packet.episodes[0].claim, "staging was down for an hour");
        assert_eq!(packet.evidence[0].memory_id, ids[3]);
        assert_eq!(
            packet.evidence[0].source_uri.as_deref(),
            Some("https://example.com/runbook")
        );
        assert_eq!(
            packet.evidence[0].source_span.as_deref(),
            Some("page 12"),
            "the excerpt is the span, because that is where a locator lives"
        );
        assert_eq!(packet.facts[0].kind, "semantic");
        assert_eq!(packet.facts[0].applicability, "unknown");
        assert!(
            packet.conflicts.is_empty(),
            "no edge in this fixture contradicts anything"
        );
        assert!(packet.warnings.is_empty(), "nothing here is dubious");
    }

    #[test]
    fn the_claim_is_the_most_specific_line_the_writer_stored() {
        let db = testing::db();
        let routine = testing::add_and_get(
            &db,
            MemoryKind::Procedural,
            "Deploy staging",
            "deploy staging steps",
        );
        let mut with_steps = routine.clone();
        with_steps.memory_metadata =
            json!({"steps": ["run the script", "migrate"], "trigger": "a deploy"});
        assert_eq!(
            claim_for(&with_steps, ""),
            "run the script → migrate",
            "steps are the routine, so they are the claim"
        );

        let mut with_trigger = routine.clone();
        with_trigger.memory_metadata = json!({"trigger": "a deploy is scheduled"});
        assert_eq!(claim_for(&with_trigger, ""), "When: a deploy is scheduled");

        let mut bare = routine.clone();
        bare.memory_metadata = json!({});
        assert_eq!(
            claim_for(&bare, ""),
            "Deploy staging",
            "with nothing stored, the title is the best available summary"
        );

        let mut broken = routine;
        broken.memory_metadata = json!({"steps": "run the script"});
        assert_eq!(
            claim_for(&broken, ""),
            "Deploy staging",
            "a steps value that is not a list is ignored, not rendered as one string"
        );
    }

    #[test]
    fn a_snippet_with_highlight_markers_yields_a_claim_without_them() {
        let db = testing::db();
        let id = testing::add(
            &db,
            MemoryKind::Reference,
            "Deploy runbook",
            "the deploy runbook",
        );
        let mut memory = db.get(&id).expect("the memory");
        assert_eq!(
            candidate_text(&memory, "the **deploy** runbook"),
            "Deploy runbook the deploy runbook",
            "title plus the matched passage, whitespace collapsed, markers gone"
        );

        // The writer refuses an empty title, so the fallback is only reachable in a
        // database somebody edited by hand — which is exactly the read that must survive.
        memory.title = String::new();
        assert_eq!(
            claim_for(&memory, "the **deploy** runbook"),
            "the deploy runbook",
            "markers are for the terminal, not for a claim"
        );
    }

    #[test]
    fn only_the_top_procedure_takes_the_slot_but_both_are_injected() {
        let db = testing::db();
        let first = testing::add(
            &db,
            MemoryKind::Procedural,
            "Deploy staging",
            "deploy staging",
        );
        let second = testing::add(
            &db,
            MemoryKind::Procedural,
            "Deploy staging again",
            "deploy staging",
        );
        let packet = db
            .retrieve(&query(Intent::Execute, "deploy staging"))
            .expect("a packet");

        let slot = packet
            .procedure
            .as_ref()
            .expect("one procedure")
            .memory_id
            .clone();
        assert!(
            packet.retrieved_ids.contains(&first) && packet.retrieved_ids.contains(&second),
            "the runner-up is still injected: dropping it would lose the second-best routine"
        );
        assert!(
            slot == first || slot == second,
            "the slot holds one of the two, and the packet names which"
        );
        assert_eq!(packet.items().len(), 1, "one item took the procedure slot");
    }

    #[test]
    fn applicability_needs_half_the_constraints_and_says_nothing_when_told_nothing() {
        let text = "staging deploys need a manual migration";
        assert_eq!(
            applicability_of(&[], text),
            Applicability::Unknown,
            "no constraints means no opinion"
        );
        assert_eq!(
            applicability_of(&["the".to_owned()], text),
            Applicability::Unknown,
            "a constraint of stop words only carries no tokens"
        );
        assert_eq!(
            applicability_of(&["staging migrations".to_owned()], text),
            Applicability::Applicable,
            "half the constraint tokens is the inclusive boundary: >= 0.5 is applicable"
        );
        assert_eq!(
            applicability_of(&["staging kubernetes tuesday".to_owned()], text),
            Applicability::Conditional,
            "a third is below the boundary and above nothing"
        );
        assert_eq!(
            applicability_of(&["kubernetes tuesday".to_owned()], text),
            Applicability::Unknown,
            "no shared tokens is unknown, not a weak yes"
        );
    }

    #[test]
    fn tokenizing_matches_the_hosted_rule_exactly() {
        assert_eq!(
            tokenize("The staging deploy!"),
            ["deploy", "staging"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            "single characters and stop words are dropped, case is folded"
        );
        assert_eq!(
            tokenize("a_b-c"),
            ["a", "b", "c"]
                .into_iter()
                .filter(|token| token.len() >= 2)
                .map(str::to_owned)
                .collect::<BTreeSet<String>>(),
            "punctuation splits tokens, and one-character pieces are dropped"
        );
        assert!(
            tokenize("日本語のメモ").is_empty(),
            "the hosted rule is ASCII-only, and so is this port — see the note on `tokenize`"
        );
    }

    #[test]
    fn a_conditional_item_carries_a_stored_reason_for_its_condition() {
        let db = testing::db();
        db.add(
            &NewMemory::new(MemoryKind::Procedural, "Deploy staging")
                .body("deploy staging from the runbook")
                .metadata(Some(json!({"scope": "only when the schema changed"}))),
        )
        .expect("the routine must be written");

        let conditional = db
            .retrieve(&RetrieveQuery {
                constraints: vec!["staging kubernetes tuesday".to_owned()],
                ..query(Intent::Execute, "deploy staging")
            })
            .expect("a packet");
        let item = conditional.procedure.expect("a procedure");
        assert_eq!(item.applicability, "conditional");
        assert_eq!(
            item.condition.as_deref(),
            Some("only when the schema changed")
        );

        let unconstrained = db
            .retrieve(&query(Intent::Execute, "deploy staging"))
            .expect("a packet");
        let item = unconstrained.procedure.expect("a procedure");
        assert_eq!(item.applicability, "unknown");
        assert_eq!(
            item.condition, None,
            "a condition without a condition is a claim the packet cannot support"
        );
    }

    #[test]
    fn an_expired_memory_is_warned_about_demoted_and_still_returned() {
        let db = testing::db();
        let fresh = testing::add(
            &db,
            MemoryKind::Semantic,
            "Deploy staging",
            "deploy staging",
        );
        let stale = testing::add(
            &db,
            MemoryKind::Semantic,
            "Deploy staging",
            "deploy staging",
        );
        // One second past, so the comparison is against a real deadline rather than a
        // string that never looked like one.
        db.conn()
            .execute(
                "UPDATE memories SET expires_at = '2001-01-01T00:00:00Z' WHERE id = ?1",
                [&stale],
            )
            .expect("the deadline must be settable");

        let packet = db
            .retrieve(&query(Intent::Answer, "deploy staging"))
            .expect("a packet");
        assert_eq!(
            packet.facts.first().map(|item| item.memory_id.as_str()),
            Some(fresh.as_str()),
            "a live memory outranks an expired one that otherwise ties"
        );
        assert!(
            packet.retrieved_ids.contains(&stale),
            "expired is demoted, never dropped: it is evidence about what was believed"
        );
        assert!(
            packet
                .warnings
                .iter()
                .any(|warning| warning.contains("is expired (valid until 2001-01-01T00:00:00Z)")),
            "{:?}",
            packet.warnings
        );
    }

    #[test]
    fn a_valid_until_the_writer_chose_is_enough_to_call_a_memory_expired() {
        let db = testing::db();
        db.add(
            &NewMemory::new(MemoryKind::Semantic, "Deploy staging")
                .body("deploy staging")
                .metadata(Some(json!({"valid_until": "2001-01-01"}))),
        )
        .expect("the memory must be written");
        let packet = db
            .retrieve(&query(Intent::Answer, "deploy staging"))
            .expect("a packet");
        assert!(
            packet
                .warnings
                .iter()
                .any(|warning| warning.contains("is expired")),
            "{:?}",
            packet.warnings
        );
    }

    #[test]
    fn a_deadline_that_is_not_a_timestamp_is_not_a_deadline() {
        // The hosted version parses and gets `NaN`; this one refuses to compare a string
        // that could not be a date. Both mean "not stale", and neither may panic.
        assert!(!is_stale(Some("whenever"), None, "2026-10-17T00:00:00Z"));
        assert!(!is_stale(Some(""), None, "2026-10-17T00:00:00Z"));
        assert!(!is_stale(None, None, "2026-10-17T00:00:00Z"));
        assert!(!is_stale(
            Some("2026-10-17T00:00:00Z"),
            None,
            "2026-10-17T00:00:00Z"
        ));
        assert!(is_stale(
            Some("2026-10-16T23:59:59Z"),
            None,
            "2026-10-17T00:00:00Z"
        ));
        assert!(
            is_stale(None, Some("2026-01-01"), "2026-10-17T00:00:00Z"),
            "a date-only value orders correctly against a full timestamp"
        );
        assert!(
            !is_stale(None, Some("2026-01"), "2026-10-17T00:00:00Z"),
            "a value too short to compare is treated as no deadline rather than guessed at, \
             which is what the hosted `Date.parse` returning NaN amounts to"
        );
    }

    #[test]
    fn low_confidence_is_warned_about_and_an_unclaimed_one_is_not() {
        let db = testing::db();
        let unsure = db
            .add(
                &NewMemory::new(MemoryKind::Semantic, "Deploy staging is manual")
                    .body("deploy staging")
                    .confidence(0.2),
            )
            .expect("the memory must be written")
            .id;
        let unclaimed = testing::add(
            &db,
            MemoryKind::Semantic,
            "Deploy staging",
            "deploy staging",
        );
        let packet = db
            .retrieve(&query(Intent::Answer, "deploy staging"))
            .expect("a packet");

        let items = packet.items();
        let unsure_item = items
            .iter()
            .find(|item| item.memory_id == unsure)
            .expect("the unsure memory is in the packet");
        assert_eq!(
            unsure_item.confidence.to_bits(),
            0.2_f64.to_bits(),
            "the stored 0.2, unrounded: a packet reports what the column holds"
        );
        let unclaimed_item = items
            .iter()
            .find(|item| item.memory_id == unclaimed)
            .expect("the unclaimed memory is in the packet");
        assert_eq!(
            unclaimed_item.confidence.to_bits(),
            DEFAULT_CONFIDENCE.to_bits(),
            "no claim means the default, which is what the wire does"
        );
        assert!(
            packet
                .warnings
                .iter()
                .any(|warning| warning.contains("has low confidence (0.20)")),
            "{:?}",
            packet.warnings
        );
        assert_eq!(
            packet
                .warnings
                .iter()
                .filter(|warning| warning.contains("low confidence"))
                .count(),
            1,
            "0.5 is not below 0.5, so exactly one of the two is warned about: {:?}",
            packet.warnings
        );
    }

    #[test]
    fn a_pending_memory_never_reaches_a_packet() {
        let db = testing::db();
        let live = testing::add(
            &db,
            MemoryKind::Semantic,
            "Deploy staging",
            "deploy staging",
        );
        let draft = db
            .add(
                &NewMemory::new(MemoryKind::Semantic, "Deploy staging by hand")
                    .body("deploy staging the long way")
                    .source("agent_reflection")
                    .confidence(0.4),
            )
            .expect("the draft must be written")
            .id;
        assert_eq!(
            db.get(&draft).expect("the draft").status,
            crate::model::Status::PendingApproval
        );

        let packet = db
            .retrieve(&query(Intent::Answer, "deploy staging"))
            .expect("a packet");
        assert!(packet.retrieved_ids.contains(&live));
        assert!(
            !packet.retrieved_ids.contains(&draft),
            "a packet is a retrieval, and a draft is not retrievable"
        );
        for item in packet.items() {
            assert_ne!(item.memory_id, draft);
        }
    }

    #[test]
    fn the_packet_records_the_ids_it_injected_and_not_the_window_it_ranked() {
        // The window is `limit × 4` at least, so this is the difference between one use
        // being counted once and a memory being credited for simply being considered.
        let db = testing::db();
        let ids = four_kinds(&db);
        let packet = db
            .retrieve(&RetrieveQuery {
                limit: 2,
                ..query(Intent::Answer, "deploy staging")
            })
            .expect("a packet");
        assert_eq!(packet.retrieved_ids.len(), 2, "the page is two items");

        for id in &ids {
            let memory = db.get(id).expect("the memory");
            let expected = i64::from(packet.retrieved_ids.contains(id));
            assert_eq!(
                memory.retrieval_count, expected,
                "{} was injected {} time(s), so its counter must say so",
                memory.id, expected
            );
            assert_eq!(
                memory.last_retrieved_at.is_some(),
                packet.retrieved_ids.contains(id)
            );
        }
    }

    #[test]
    fn a_packet_can_be_built_without_touching_a_decay_clock() {
        let db = testing::db();
        let ids = four_kinds(&db);
        db.retrieve(&RetrieveQuery {
            record: false,
            ..query(Intent::Answer, "deploy staging")
        })
        .expect("a packet");
        for id in &ids {
            assert_eq!(db.get(id).expect("the memory").retrieval_count, 0);
        }
    }

    #[test]
    fn an_empty_corpus_says_so_and_omits_the_fields_it_has_nothing_for() {
        let db = testing::db();
        let packet = db
            .retrieve(&query(Intent::Plan, "nothing was ever written here"))
            .expect("a packet");
        assert_eq!(
            packet.summary, "No memories matched intent \"plan\".",
            "the empty summary names the intent, so a caller can tell whose packet it is"
        );
        assert_eq!(packet.retrieved_ids.len(), 0);
        let json = serde_json::to_value(&packet).expect("the packet must serialize");
        assert!(
            json.get("procedure").is_none(),
            "the wire omits an absent procedure rather than sending null: {json}"
        );
    }

    #[test]
    fn the_json_has_the_wire_shape_and_no_nulls_where_the_wire_has_no_keys() {
        let db = testing::db();
        four_kinds(&db);
        let packet = db
            .retrieve(&query(Intent::Execute, "deploy staging"))
            .expect("a packet");
        let json = serde_json::to_value(&packet).expect("the packet must serialize");

        let mut keys: Vec<&str> = json
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "conflicts",
                "episodes",
                "evidence",
                "facts",
                "procedure",
                "retrieved_ids",
                "summary",
                "warnings",
            ],
            "the packet's own keys are the Python SDK's ContextPacket"
        );

        let item = json["facts"][0].as_object().expect("a packet item");
        let mut item_keys: Vec<&str> = item.keys().map(String::as_str).collect();
        item_keys.sort_unstable();
        assert_eq!(
            item_keys,
            [
                "applicability",
                "authority",
                "claim",
                "confidence",
                "kind",
                "memory_id",
            ],
            "no `condition` and no `source_ref` when there is nothing to put in them: a \
             null where the wire has no key is a decode error waiting for a strict client"
        );
    }

    #[test]
    fn a_query_longer_than_the_route_accepts_is_refused_by_name() {
        let db = testing::db();
        let error = db
            .retrieve(&query(Intent::Answer, &"x".repeat(MAX_QUERY_CHARS + 1)))
            .expect_err("the route would reject this too");
        assert!(
            matches!(error, crate::Error::Invalid(_)),
            "a request the hosted route 400s must not be a 200 here: {error}"
        );
        assert!(error.to_string().contains("1000"), "{error}");
    }

    #[test]
    fn too_many_constraints_are_refused_rather_than_ignored() {
        let db = testing::db();
        let constraints = vec!["a hint".to_owned(); MAX_CONSTRAINTS + 1];
        let error = db
            .retrieve(&RetrieveQuery {
                constraints,
                ..query(Intent::Answer, "deploy staging")
            })
            .expect_err("the route caps constraints at 16");
        assert!(matches!(error, crate::Error::Invalid(_)), "{error}");
    }

    #[test]
    fn the_entities_and_risk_level_a_caller_sends_change_nothing() {
        // Not decoration: these two fields are part of the request the SDKs send, and the
        // hosted service ignores them. Reading them here would make localhost disagree with
        // kiomon.com about the same call, so the test pins the disagreement to zero.
        let db = testing::db();
        four_kinds(&db);
        let plain = db
            .retrieve(&query(Intent::Answer, "deploy staging"))
            .expect("a packet");
        let decorated = db
            .retrieve(&RetrieveQuery {
                entities: vec!["staging".to_owned(), "deploy".to_owned()],
                risk_level: Some(RiskLevel::High),
                ..query(Intent::Answer, "deploy staging")
            })
            .expect("a packet");
        assert_eq!(
            serde_json::to_string(&plain).expect("json"),
            serde_json::to_string(&decorated).expect("json"),
            "a field the hosted service does not read cannot change a packet here"
        );
    }

    #[test]
    fn the_summary_counts_what_the_buckets_hold_singular_and_plural() {
        assert_eq!(
            summary_for(Intent::Answer, 0, false, 0),
            "0 facts, 0 procedure, 0 episodes matched intent \"answer\"."
        );
        assert_eq!(
            summary_for(Intent::Verify, 1, true, 1),
            "1 fact, 1 procedure, 1 episode matched intent \"verify\"."
        );
        assert_eq!(
            summary_for(Intent::Debug, 2, false, 3),
            "2 facts, 0 procedure, 3 episodes matched intent \"debug\"."
        );
    }

    #[test]
    fn the_applicability_surface_is_the_memory_plus_the_passage_that_matched() {
        // The surface is title, snippet, excerpt, tags and the envelope — the *passage* that
        // matched, not the whole body. Two consequences, and both are the hosted behaviour:
        // a constraint found in the matched passage counts, and one that lives in an
        // unmatched part of the body does not.
        let db = testing::db();
        let id = db
            .add(
                &NewMemory::new(MemoryKind::Semantic, "Deploy staging")
                    .body("the kubernetes cluster needs a manual migration before deploy")
                    .confidence(0.8),
            )
            .expect("the memory must be written")
            .id;

        let matched = db
            .retrieve(&RetrieveQuery {
                constraints: vec!["kubernetes migration".to_owned()],
                ..query(Intent::Answer, "kubernetes migration")
            })
            .expect("a packet");
        assert_eq!(matched.facts[0].memory_id, id);
        assert_eq!(
            matched.facts[0].applicability, "applicable",
            "the query matched the passage the constraint names"
        );

        let unmatched = db
            .retrieve(&RetrieveQuery {
                constraints: vec!["kubernetes migration".to_owned()],
                ..query(Intent::Answer, "deploy staging")
            })
            .expect("a packet");
        assert_eq!(
            unmatched.facts[0].applicability, "unknown",
            "the same constraint and the same memory, ranked on a different passage"
        );
    }

    #[test]
    fn an_unknown_workspace_is_an_empty_packet_exactly_as_it_is_for_search() {
        // Workspace resolution belongs to the caller: the CLI resolves `--workspace` before
        // it gets here and turns an unknown one into exit 2, and WP9's route will do the
        // same with a 404. The core mirrors `search`, where an unknown workspace in the
        // scope is a filter that matches nothing rather than an error — one rule for two
        // verbs, so a caller cannot be right about one and wrong about the other.
        let db = testing::db();
        testing::add(
            &db,
            MemoryKind::Semantic,
            "Deploy staging",
            "deploy staging",
        );
        let packet = db
            .retrieve(&RetrieveQuery {
                scope: Scope::One("no-such-workspace".to_owned()),
                ..query(Intent::Answer, "deploy staging")
            })
            .expect("a packet");
        assert_eq!(packet.retrieved_ids.len(), 0);
        assert_eq!(
            packet.summary, "No memories matched intent \"answer\".",
            "the empty summary a caller sees for an empty workspace, too"
        );
    }

    // -----------------------------------------------------------------------
    // The graph in a packet (WP5)
    // -----------------------------------------------------------------------

    /// Two memories on one subject, so both land in one packet.
    fn a_pair(db: &Database) -> (String, String) {
        let older = db
            .add(
                &NewMemory::new(MemoryKind::Semantic, "staging deploy migrations are manual")
                    .body("A deploy to staging needs a manual migration.")
                    .confidence(0.9),
            )
            .expect("the older memory must be written")
            .id;
        let newer = db
            .add(
                &NewMemory::new(
                    MemoryKind::Semantic,
                    "staging deploy migrations are automatic",
                )
                .body("The deploy to staging runs its migration automatically now.")
                .confidence(0.9),
            )
            .expect("the newer memory must be written")
            .id;
        (older, newer)
    }

    #[test]
    fn a_contradiction_written_by_hand_becomes_a_packet_conflict() {
        // The packet's conflict read was ported in WP5.5 when nothing could write the edge
        // it reads, so `conflicts: []` was the only reachable answer. This is the test that
        // makes the other branch real: `link --relation contradicts` is the writer.
        let db = testing::db();
        let (older, newer) = a_pair(&db);
        assert!(
            db.retrieve(&query(Intent::Verify, "staging deploy migrations"))
                .expect("a packet")
                .conflicts
                .is_empty(),
            "no edge, no conflict"
        );

        db.link(&crate::model::NewLink {
            source_id: newer.clone(),
            target_id: older.clone(),
            relation: Relation::Contradicts,
            score: None,
        })
        .expect("the fixture contradiction must be written");

        let packet = db
            .retrieve(&query(Intent::Verify, "staging deploy migrations"))
            .expect("a packet");
        assert_eq!(packet.conflicts.len(), 1, "the pair is reported once");
        let note = &packet.conflicts[0];
        assert!(
            [&note.memory_id_a, &note.memory_id_b].contains(&&older)
                && [&note.memory_id_a, &note.memory_id_b].contains(&&newer),
            "both ends are named: {note:?}"
        );
        assert_eq!(note.note, CONFLICT_NOTE);
        assert!(
            packet
                .warnings
                .iter()
                .any(|warning| warning.contains("Conflicting memories")),
            "a caller that reads only the warnings still learns about it: {:?}",
            packet.warnings
        );
        assert!(
            packet.retrieved_ids.contains(&older) && packet.retrieved_ids.contains(&newer),
            "a conflict is surfaced, never auto-resolved by dropping a side"
        );
    }

    #[test]
    fn a_superseded_memory_is_warned_about_and_still_returned() {
        // WP5 extended the relation vocabulary for exactly this: the packet reads
        // `supersedes`, and before migration 0004 nothing could write one.
        let db = testing::db();
        let (older, newer) = a_pair(&db);
        db.link(&crate::model::NewLink {
            source_id: newer.clone(),
            target_id: older.clone(),
            relation: Relation::Supersedes,
            score: None,
        })
        .expect("the fixture supersession must be written");

        let packet = db
            .retrieve(&query(Intent::Execute, "staging deploy migrations"))
            .expect("a packet");
        assert!(
            packet.warnings.iter().any(|warning| {
                warning.contains("is superseded by") && warning.contains(&newer)
            }),
            "the warning names the memory that replaced it: {:?}",
            packet.warnings
        );
        assert!(
            packet.retrieved_ids.contains(&older),
            "demoted, never dropped: the caller can still see what was replaced"
        );
        assert!(packet.retrieved_ids.contains(&newer));
    }

    #[test]
    fn a_closed_conflict_is_not_reported() {
        // `valid_to` is how a settled argument stays settled, and the packet's read is
        // `WHERE valid_to IS NULL`. Nothing writes the column yet, so the fixture closes
        // the edge with SQL — the test is about the reader honouring the column.
        let db = testing::db();
        let (older, newer) = a_pair(&db);
        db.link(&crate::model::NewLink {
            source_id: newer.clone(),
            target_id: older.clone(),
            relation: Relation::Contradicts,
            score: None,
        })
        .expect("the fixture contradiction must be written");
        db.conn()
            .execute(
                "UPDATE memory_links SET valid_to = '2026-01-01T00:00:00Z'",
                [],
            )
            .expect("the fixture must close the edge");

        let packet = db
            .retrieve(&query(Intent::Verify, "staging deploy migrations"))
            .expect("a packet");
        assert!(
            packet.conflicts.is_empty(),
            "a resolved contradiction is not a conflict"
        );
        assert!(
            !packet
                .warnings
                .iter()
                .any(|warning| warning.contains("Conflicting")),
            "{:?}",
            packet.warnings
        );
    }
}
