// SPDX-License-Identifier: Apache-2.0

//! The domain vocabulary.
//!
//! These strings are the wire values: they are what `--json` emits, what the SQL
//! `CHECK` constraints accept, and what the hosted product's MCP tools use. They are
//! therefore **not** free to change for readability — the friendly names
//! ([`MemoryKind::label`]) are the place for that, and they exist so an agent can
//! describe a memory to a person without teaching them the schema.

use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};

/// What kind of knowledge a memory holds.
///
/// The taxonomy is the one the hosted product settled on, and it is load-bearing
/// rather than cosmetic: retrieval filters on it, decay is per-kind, and the write
/// gate's kind-specific hooks key off it. See `plans/001-local-v1-core.md` §1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// Imported or saved content — a document, an article, a file. *Files.*
    ///
    /// This is also the safe default for content of unknown provenance, and the only
    /// kind that never decays.
    Reference,
    /// Knowledge, facts, and relationships. *Facts.*
    Semantic,
    /// How-to rules, triggers, and preferences. *Routines.*
    Procedural,
    /// Something that happened, with a timestamp. *Moments.*
    Episodic,
}

impl MemoryKind {
    /// Every kind, in the order the taxonomy is documented. Useful for help text and
    /// for tests that must cover all four.
    pub const ALL: [Self; 4] = [
        Self::Reference,
        Self::Semantic,
        Self::Procedural,
        Self::Episodic,
    ];

    /// The wire value — the SQL `CHECK` value, the JSON field, and the agent's word.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reference => "reference",
            Self::Semantic => "semantic",
            Self::Procedural => "procedural",
            Self::Episodic => "episodic",
        }
    }

    /// The user-facing noun, matching the hosted product's dashboard and docs
    /// verbatim so the same memory reads the same way in both surfaces.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Reference => "Files",
            Self::Semantic => "Facts",
            Self::Procedural => "Routines",
            Self::Episodic => "Moments",
        }
    }

    /// The one-line framing the write side asks a user (or an agent) to choose by.
    #[must_use]
    pub const fn prompt(self) -> &'static str {
        match self {
            Self::Reference => "Keep this",
            Self::Semantic => "Teach it",
            Self::Procedural => "Automate it",
            Self::Episodic => "Log it",
        }
    }
}

impl FromStr for MemoryKind {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "reference" => Ok(Self::Reference),
            "semantic" => Ok(Self::Semantic),
            "procedural" => Ok(Self::Procedural),
            "episodic" => Ok(Self::Episodic),
            other => Err(Error::Invalid(format!(
                "unknown memory kind `{other}`: expected one of reference, semantic, procedural, episodic"
            ))),
        }
    }
}

impl std::fmt::Display for MemoryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a memory is in its lifecycle.
///
/// The distinction between [`Status::PendingApproval`] and [`Status::Active`] is the
/// review inbox, and it is a retrieval boundary rather than a flag: a pending memory
/// has no full-text index row, no links, and no embedding, so it cannot be found by
/// any read path. A review inbox that leaks into search is worse than no review
/// inbox — see `plans/001-local-v1-core.md` §5.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Indexed, linked, retrievable. The normal state.
    Active,
    /// Waiting in the review inbox. Invisible to retrieval by construction.
    PendingApproval,
    /// Hidden from retrieval, restorable, never purged by decay.
    Archived,
}

impl Status {
    /// Every status.
    pub const ALL: [Self; 3] = [Self::Active, Self::PendingApproval, Self::Archived];

    /// The wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::PendingApproval => "pending_approval",
            Self::Archived => "archived",
        }
    }
}

impl FromStr for Status {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        for status in Self::ALL {
            if status.as_str() == s.trim().to_ascii_lowercase() {
                return Ok(status);
            }
        }
        Err(Error::Invalid(format!(
            "unknown status `{s}`: expected one of active, pending_approval, archived"
        )))
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A typed edge in the memory graph.
///
/// These are the hosted product's relation names (`platform/CONTEXT.md` →
/// Knowledge Graph) and they are the vocabulary an agent walks: `derived_from` is
/// the provenance trail, `contradicts` is the conflict flag, `precedes`/`follows`
/// order episodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    /// This memory was derived from the target. Directed; written from `source_refs`.
    DerivedFrom,
    /// This memory supports the target's claim.
    Supports,
    /// This memory conflicts with the target.
    Contradicts,
    /// Unspecified similarity. Undirected, and what auto-linking writes.
    RelatedTo,
    /// This memory happened before the target (episodic ordering).
    Precedes,
    /// This memory happened after the target (episodic ordering).
    Follows,
    /// This memory is a component of the target.
    PartOf,
}

impl Relation {
    /// Every relation.
    pub const ALL: [Self; 7] = [
        Self::DerivedFrom,
        Self::Supports,
        Self::Contradicts,
        Self::RelatedTo,
        Self::Precedes,
        Self::Follows,
        Self::PartOf,
    ];

    /// The wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DerivedFrom => "derived_from",
            Self::Supports => "supports",
            Self::Contradicts => "contradicts",
            Self::RelatedTo => "related_to",
            Self::Precedes => "precedes",
            Self::Follows => "follows",
            Self::PartOf => "part_of",
        }
    }

    /// Whether the edge has a direction.
    ///
    /// `related_to` is symmetric: `A related_to B` and `B related_to A` are the same
    /// statement, so the graph stores one row and `relate` reads both directions.
    /// Writing a directed edge for a symmetric relation is how a graph grows
    /// duplicate-looking edges that nobody can explain.
    #[must_use]
    pub const fn is_directed(self) -> bool {
        !matches!(self, Self::RelatedTo)
    }
}

impl FromStr for Relation {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let normalized = s.trim().to_ascii_lowercase();
        for relation in Self::ALL {
            if relation.as_str() == normalized {
                return Ok(relation);
            }
        }
        Err(Error::Invalid(format!(
            "unknown relation `{s}`: expected one of {}",
            Self::ALL
                .iter()
                .map(|r| r.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }
}

impl std::fmt::Display for Relation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The provenance label for a `source` value, matching the hosted product's
/// `kindLabels.ts`. Unknown sources are their own label rather than a guess.
#[must_use]
pub fn source_label(source: &str) -> &str {
    match source {
        "user_saved" => "saved",
        "connector_sync" => "synced",
        "agent_reflection" => "reflection",
        "email_inbound" => "email",
        "" => "unknown",
        other => other,
    }
}

/// The workspace a write lands in when nothing else is asked for.
///
/// `init` creates it, so it always exists on a database that has been initialised,
/// and its name — not its id — is what a user is told about.
pub const DEFAULT_WORKSPACE: &str = "default";

/// The name shown for [`DEFAULT_WORKSPACE`] on a fresh install.
pub const DEFAULT_WORKSPACE_NAME: &str = "Default";

/// The most tags one memory may carry.
///
/// The hosted product's twenty, kept for two reasons: a memory with two hundred tags
/// is a memory whose tags have stopped meaning anything, and `tags_text` is indexed,
/// so the cap is also a bound on a write's cost.
pub const MAX_TAGS: usize = 20;

/// How a workspace admits agent-authored writes.
///
/// The distinction is the review inbox's whole configuration. It lives on the
/// workspace rather than on the command line because it is a property of the corpus a
/// user is building, not of the invocation they happen to be running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewMode {
    /// Trust the write gate: a high-authority source, or an agent write at or above
    /// the confidence threshold, activates immediately.
    Autonomous,
    /// Everything agent-authored waits in the review inbox, whatever its confidence.
    Review,
}

impl ReviewMode {
    /// The wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Autonomous => "autonomous",
            Self::Review => "review",
        }
    }
}

impl FromStr for ReviewMode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "autonomous" => Ok(Self::Autonomous),
            "review" => Ok(Self::Review),
            other => Err(Error::Invalid(format!(
                "unknown review mode `{other}`: expected autonomous or review"
            ))),
        }
    }
}

impl std::fmt::Display for ReviewMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A workspace: the isolation boundary, and the review mode's home.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    /// The id every `--workspace` refers to.
    pub id: String,
    /// The label a user sees.
    pub name: String,
    /// How this workspace admits agent writes.
    pub review_mode: ReviewMode,
    /// When it was created, RFC 3339 UTC.
    pub created_at: String,
}

/// One memory, as stored.
///
/// Serialises to the contract's shape (see `docs/CONTRACT.md`), which is why
/// [`Serialize`] is implemented by hand rather than derived: the derived form would
/// carry `kind` as an enum name and omit the two display labels an agent is told to
/// use when talking to a person.
#[derive(Debug, Clone, PartialEq)]
pub struct Memory {
    /// The agent-facing id, `k_<ulid>`.
    pub id: String,
    /// The workspace it belongs to.
    pub workspace_id: String,
    /// The memory taxonomy facet.
    pub kind: MemoryKind,
    /// Short, searchable heading.
    pub title: String,
    /// The one-line card subheading. Empty rather than absent.
    pub excerpt: String,
    /// The full content.
    pub body: String,
    /// Tags, in the order they were given.
    pub tags: Vec<String>,
    /// The per-kind envelope.
    pub memory_metadata: Value,
    /// Provenance: `user_saved`, `agent_reflection`, `import`, `connector_sync`.
    pub source: String,
    /// Where the content came from, when it came from somewhere.
    pub source_url: Option<String>,
    /// The writer's estimate of reliability, 0.0–1.0, or `None` when none was claimed.
    pub confidence: Option<f64>,
    /// How much this matters, 0.0–1.0. The ceiling decay falls away from.
    pub importance: f64,
    /// Decay's own output. The only envelope field that moves without being written.
    pub strength: f64,
    /// How many independent writes have asserted the same thing.
    pub corroboration_count: i64,
    /// How many retrievals have returned it.
    pub retrieval_count: i64,
    /// The last retrieval, RFC 3339 UTC.
    pub last_retrieved_at: Option<String>,
    /// Accumulated `rate`/`outcome` credit: a running average of the ratings.
    pub feedback_score: f64,
    /// How many ratings that average is over. Zero means `feedback_score` means nothing.
    pub feedback_count: i64,
    /// Immune to decay, and first in every ordering.
    pub pinned: bool,
    /// Where it is in its lifecycle.
    pub status: Status,
    /// When it stops being retrievable, if it was given a deadline.
    pub expires_at: Option<String>,
    /// Creation, RFC 3339 UTC.
    pub created_at: String,
    /// Last modification, RFC 3339 UTC.
    pub updated_at: String,
}

impl Memory {
    /// The user-facing noun for the kind. `Facts`, not `semantic`.
    #[must_use]
    pub const fn kind_label(&self) -> &'static str {
        self.kind.label()
    }

    /// The short provenance label. `saved`, not `user_saved`.
    #[must_use]
    pub fn source_label(&self) -> &str {
        source_label(&self.source)
    }
}

/// The wire shape of a [`Memory`]. See `docs/CONTRACT.md`.
#[derive(Debug, Serialize)]
pub struct MemoryJson<'a> {
    /// See [`Memory::id`].
    pub id: &'a str,
    /// See [`Memory::workspace_id`].
    pub workspace_id: &'a str,
    /// The wire value of the kind.
    pub kind: &'a str,
    /// The user-facing noun for the kind.
    pub kind_label: &'static str,
    /// See [`Memory::title`].
    pub title: &'a str,
    /// See [`Memory::excerpt`].
    pub excerpt: &'a str,
    /// See [`Memory::body`].
    pub body: &'a str,
    /// See [`Memory::tags`].
    pub tags: &'a [String],
    /// See [`Memory::memory_metadata`].
    pub memory_metadata: &'a Value,
    /// See [`Memory::source`].
    pub source: &'a str,
    /// The short provenance label.
    pub source_label: &'a str,
    /// See [`Memory::source_url`].
    pub source_url: &'a Option<String>,
    /// See [`Memory::confidence`].
    pub confidence: Option<f64>,
    /// See [`Memory::importance`].
    pub importance: f64,
    /// See [`Memory::strength`].
    pub strength: f64,
    /// See [`Memory::corroboration_count`].
    pub corroboration_count: i64,
    /// See [`Memory::retrieval_count`].
    pub retrieval_count: i64,
    /// See [`Memory::last_retrieved_at`].
    pub last_retrieved_at: &'a Option<String>,
    /// See [`Memory::feedback_score`].
    pub feedback_score: f64,
    /// See [`Memory::feedback_count`].
    pub feedback_count: i64,
    /// See [`Memory::pinned`].
    pub pinned: bool,
    /// The wire value of the status.
    pub status: &'a str,
    /// See [`Memory::expires_at`].
    pub expires_at: &'a Option<String>,
    /// See [`Memory::created_at`].
    pub created_at: &'a str,
    /// See [`Memory::updated_at`].
    pub updated_at: &'a str,
}

impl<'a> From<&'a Memory> for MemoryJson<'a> {
    fn from(memory: &'a Memory) -> Self {
        Self {
            id: &memory.id,
            workspace_id: &memory.workspace_id,
            kind: memory.kind.as_str(),
            kind_label: memory.kind.label(),
            title: &memory.title,
            excerpt: &memory.excerpt,
            body: &memory.body,
            tags: &memory.tags,
            memory_metadata: &memory.memory_metadata,
            source: &memory.source,
            source_label: source_label(&memory.source),
            source_url: &memory.source_url,
            confidence: memory.confidence,
            importance: memory.importance,
            strength: memory.strength,
            corroboration_count: memory.corroboration_count,
            retrieval_count: memory.retrieval_count,
            last_retrieved_at: &memory.last_retrieved_at,
            feedback_score: memory.feedback_score,
            feedback_count: memory.feedback_count,
            pinned: memory.pinned,
            status: memory.status.as_str(),
            expires_at: &memory.expires_at,
            created_at: &memory.created_at,
            updated_at: &memory.updated_at,
        }
    }
}

impl Serialize for Memory {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        MemoryJson::from(self).serialize(serializer)
    }
}

/// The wire shape of a [`Memory`] **without its body or envelope**.
///
/// What a list or a search returns, because a caller that is browsing does not need a
/// hundred kilobytes of body text for every row, and an agent paying for tokens
/// certainly does not. `get` is how you ask for the whole thing.
#[derive(Debug, Serialize)]
pub struct MemorySummaryJson<'a> {
    /// See [`Memory::id`].
    pub id: &'a str,
    /// See [`Memory::workspace_id`].
    pub workspace_id: &'a str,
    /// The wire value of the kind.
    pub kind: &'a str,
    /// The user-facing noun for the kind.
    pub kind_label: &'static str,
    /// See [`Memory::title`].
    pub title: &'a str,
    /// See [`Memory::excerpt`].
    pub excerpt: &'a str,
    /// See [`Memory::tags`].
    pub tags: &'a [String],
    /// See [`Memory::source`].
    pub source: &'a str,
    /// The short provenance label.
    pub source_label: &'a str,
    /// See [`Memory::source_url`].
    pub source_url: &'a Option<String>,
    /// See [`Memory::confidence`].
    pub confidence: Option<f64>,
    /// See [`Memory::strength`].
    pub strength: f64,
    /// See [`Memory::pinned`].
    pub pinned: bool,
    /// The wire value of the status.
    pub status: &'a str,
    /// See [`Memory::created_at`].
    pub created_at: &'a str,
}

impl<'a> From<&'a Memory> for MemorySummaryJson<'a> {
    fn from(memory: &'a Memory) -> Self {
        Self {
            id: &memory.id,
            workspace_id: &memory.workspace_id,
            kind: memory.kind.as_str(),
            kind_label: memory.kind.label(),
            title: &memory.title,
            excerpt: &memory.excerpt,
            tags: &memory.tags,
            source: &memory.source,
            source_label: source_label(&memory.source),
            source_url: &memory.source_url,
            confidence: memory.confidence,
            strength: memory.strength,
            pinned: memory.pinned,
            status: memory.status.as_str(),
            created_at: &memory.created_at,
        }
    }
}

/// A memory about to be written.
///
/// A builder rather than a struct literal because every field but two has a sensible
/// default, and a literal would make every call site restate them.
#[derive(Debug, Clone, PartialEq)]
pub struct NewMemory {
    /// Where it goes.
    pub workspace_id: String,
    /// What kind of knowledge it is.
    pub kind: MemoryKind,
    /// Short, searchable heading.
    pub title: String,
    /// The one-line subheading.
    pub excerpt: String,
    /// The full content.
    pub body: String,
    /// Tags, before normalisation.
    pub tags: Vec<String>,
    /// The per-kind envelope, before validation.
    pub memory_metadata: Option<Value>,
    /// Provenance.
    pub source: String,
    /// Where the content came from.
    pub source_url: Option<String>,
    /// The writer's reliability estimate.
    pub confidence: Option<f64>,
    /// How much this matters.
    pub importance: f64,
    /// Ids this was derived from. Becomes `derived_from` edges on activation.
    pub derived_from: Vec<String>,
    /// Store it even if an identical memory already exists.
    pub force: bool,
}

impl NewMemory {
    /// A memory of `kind` with `title`, and nothing else.
    #[must_use]
    pub fn new(kind: MemoryKind, title: impl Into<String>) -> Self {
        Self {
            workspace_id: DEFAULT_WORKSPACE.to_owned(),
            kind,
            title: title.into(),
            excerpt: String::new(),
            body: String::new(),
            tags: Vec::new(),
            memory_metadata: None,
            source: "user_saved".to_owned(),
            source_url: None,
            confidence: None,
            importance: 0.5,
            derived_from: Vec::new(),
            force: false,
        }
    }

    /// Set the full content.
    #[must_use]
    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }

    /// Set the one-line subheading.
    #[must_use]
    pub fn excerpt(mut self, excerpt: impl Into<String>) -> Self {
        self.excerpt = excerpt.into();
        self
    }

    /// Set the workspace.
    #[must_use]
    pub fn workspace(mut self, workspace: impl Into<String>) -> Self {
        self.workspace_id = workspace.into();
        self
    }

    /// Set the tags, before normalisation.
    #[must_use]
    pub fn tags(mut self, tags: Vec<String>) -> Self {
        self.tags = tags;
        self
    }

    /// Set the per-kind envelope, before validation.
    #[must_use]
    pub fn metadata(mut self, metadata: Option<Value>) -> Self {
        self.memory_metadata = metadata;
        self
    }

    /// Set the provenance.
    #[must_use]
    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }

    /// Set the source URL.
    #[must_use]
    pub fn source_url(mut self, url: impl Into<String>) -> Self {
        self.source_url = Some(url.into());
        self
    }

    /// Set the reliability estimate.
    #[must_use]
    pub fn confidence(mut self, confidence: f64) -> Self {
        self.confidence = Some(confidence);
        self
    }

    /// Set how much this matters.
    #[must_use]
    pub fn importance(mut self, importance: f64) -> Self {
        self.importance = importance;
        self
    }

    /// Set the ids this was derived from.
    #[must_use]
    pub fn derived_from(mut self, ids: Vec<String>) -> Self {
        self.derived_from = ids;
        self
    }

    /// Store it even if an identical memory already exists.
    #[must_use]
    pub fn force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }
}

/// A partial change to a memory. `None` means "leave it as it is".
///
/// Every field is a double option only where clearing matters, and none are: an empty
/// string clears an excerpt, and an empty array clears tags. That keeps the type
/// readable, which matters more here than the ability to distinguish "absent" from
/// "null".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemoryPatch {
    /// A new title.
    pub title: Option<String>,
    /// A new subheading.
    pub excerpt: Option<String>,
    /// New content.
    pub body: Option<String>,
    /// Replacement tags, before normalisation.
    pub tags: Option<Vec<String>>,
    /// A new kind.
    pub kind: Option<MemoryKind>,
    /// A new reliability estimate.
    pub confidence: Option<f64>,
    /// A new importance.
    pub importance: Option<f64>,
    /// A new source URL.
    pub source_url: Option<String>,
    /// A replacement envelope, before validation.
    pub memory_metadata: Option<Value>,
}

impl MemoryPatch {
    /// Whether this patch would change anything at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.excerpt.is_none()
            && self.body.is_none()
            && self.tags.is_none()
            && self.kind.is_none()
            && self.confidence.is_none()
            && self.importance.is_none()
            && self.source_url.is_none()
            && self.memory_metadata.is_none()
    }

    /// Whether this patch touches anything retrieval can see.
    ///
    /// Must stay in step with the columns in the `mem_au` trigger's `UPDATE OF` list
    /// in `0001_init.sql`: a field that changes what is indexed and is missing from
    /// that list makes a memory unfindable after an edit.
    #[must_use]
    pub const fn touches_index(&self) -> bool {
        self.title.is_some()
            || self.excerpt.is_some()
            || self.body.is_some()
            || self.tags.is_some()
            || self.kind.is_some()
    }
}

/// Which workspaces a read covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Exactly one workspace. What every read does by default.
    One(String),
    /// Every workspace. Only ever reached by an explicit `--workspace '*'`.
    All,
}

impl Scope {
    /// The workspace id, or `None` for [`Scope::All`].
    #[must_use]
    pub fn one(&self) -> Option<&str> {
        match self {
            Self::One(id) => Some(id),
            Self::All => None,
        }
    }
}

/// How a `list` is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListSort {
    /// Strongest first, pinned ahead of everything — the decay clock's opinion of what
    /// still matters.
    #[default]
    Strength,
    /// Newest first, with no hoisting. A pinned row does not jump the queue here, because
    /// a recency view that silently reorders itself is not one.
    Recent,
}

/// Which retrieval legs a search is allowed to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Legs {
    /// Every leg the build has. Falls back to lexical alone when no embedder is
    /// configured, which is not a degradation — it is the documented default.
    #[default]
    Auto,
    /// Lexical only. Never touches an embedder, so it is the mode that cannot be slow
    /// because of the network.
    Fts,
    /// Vector only. An error, not a fallback, when there is no embedder: a request for
    /// one thing is never answered with another.
    Vector,
}

/// How search results are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchSort {
    /// The fused rank, with pinned memories hoisted. Default.
    #[default]
    Relevance,
    /// Newest first, with no hoisting — the same promise `list --recent` makes and for
    /// the same reason.
    Recent,
    /// Strongest first, with pinned hoisted.
    Strength,
}

/// The default number of full-text tokens in a snippet.
///
/// Twelve is the platform's number: long enough to show a phrase in context, short
/// enough that ten results are not a page of prose.
pub const DEFAULT_SNIPPET_TOKENS: u16 = 12;

/// How much wider than the page a ranked window is fetched.
///
/// Three, the platform's number and for the same reason: a row at fused rank 25 can
/// still reach page one of ten when the page is reordered by strength or by pin, and it
/// cannot reach a window it was never in. The consequence is documented rather than
/// hidden — `--sort` cannot see past this window, so `--sort recent` means "the newest
/// of the top 3×limit matches", not "the newest match in the corpus".
pub const WINDOW_OVERFETCH: u32 = 3;

/// What a search should return.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchQuery {
    /// The user's query, verbatim. Sanitising it is [`crate::search`]'s job, not the
    /// caller's, so a leg added later cannot forget to.
    pub text: String,
    /// Which workspaces to cover.
    pub scope: Scope,
    /// Restrict to these kinds. Empty means every kind.
    pub kinds: Vec<MemoryKind>,
    /// The most rows to return.
    pub limit: u32,
    /// How many full-text tokens of context each snippet carries.
    pub snippet_tokens: u16,
    /// The ordering.
    pub sort: SearchSort,
    /// Which legs to run.
    pub legs: Legs,
    /// Whether each returned row counts as a retrieval against its decay clock.
    /// Default on: a search that returned a memory *is* the memory being used, and the
    /// stretch factor that feeds decay is the reason this is not telemetry.
    pub record: bool,
}

impl Default for SearchQuery {
    fn default() -> Self {
        Self {
            text: String::new(),
            scope: Scope::One(DEFAULT_WORKSPACE.to_owned()),
            kinds: Vec::new(),
            limit: 10,
            snippet_tokens: DEFAULT_SNIPPET_TOKENS,
            sort: SearchSort::Relevance,
            legs: Legs::Auto,
            record: true,
        }
    }
}

/// What a search returned, and how it got there.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchOutcome {
    /// The page, already ordered.
    pub hits: Vec<SearchHit>,
    /// Which rung of the ladder answered, or why none did.
    pub attempt: SearchAttempt,
    /// The legs that ran. A `&'static str` rather than an enum because it is reported
    /// verbatim in `--json` and the vocabulary is the contract's, not Rust's.
    pub legs: &'static str,
    /// True when a leg ran in a reduced form. Always false without an embedder: `auto`
    /// falling back to lexical is the documented default, not a degradation.
    pub degraded: bool,
    /// How many rows had their retrieval counter moved. Zero under `--no-record`, and
    /// zero when nothing matched.
    pub recorded: usize,
}

/// Which rung of the query ladder answered, or why none did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SearchAttempt {
    /// There was nothing to search for: the query was empty, or held only whitespace, so
    /// no query ran. Distinct from [`Self::Exhausted`] because "you asked for nothing"
    /// and "nothing matched" call for different next moves.
    NothingAsked,
    /// Every rung was tried and every one came back empty.
    Exhausted {
        /// The last expression tried, which is the most forgiving one there is. Present
        /// so a caller with no results can see exactly what was asked for.
        query: String,
    },
    /// This rung matched, and its rows are the ones returned.
    Matched {
        /// Which rung, 1-based, counting from the most precise. See [`crate::search::Ladder`].
        rung: u8,
        /// The FTS5 expression that matched. Exposed because "why did that match" and
        /// "why did that not" are the same question, and this is the answer to both.
        query: String,
    },
}

/// One ranked row.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// The whole memory. Search needs the body anyway — for the snippet, which the
    /// index produces — and a caller that gets an id then has to ask again.
    pub memory: Memory,
    /// The reciprocal-rank fusion score. **Only comparable within one response**: it is
    /// a function of position, not of similarity, so it is not a threshold and two
    /// searches' scores do not mean the same thing.
    pub score: f32,
    /// The match in context, with `**` around the matched terms. Empty when the matched
    /// column produced nothing to show — see `search.rs`'s note on highlight markers for
    /// why an unhighlighted snippet is dropped rather than shown.
    pub snippet: String,
}

/// The wire shape of a [`SearchHit`]: a [`MemorySummaryJson`] plus the two fields only a
/// ranked result has.
///
/// Flattened rather than retyped so a search row and a list row cannot drift apart:
/// `docs/CONTRACT.md` promises that a search result *is* a list row with `score` and
/// `snippet` added, and the only way to keep that promise is to make it the same struct.
#[derive(Debug, Serialize)]
pub struct SearchHitJson<'a> {
    /// The memory, without its body or envelope.
    #[serde(flatten)]
    pub summary: MemorySummaryJson<'a>,
    /// See [`SearchHit::score`].
    pub score: f32,
    /// See [`SearchHit::snippet`].
    pub snippet: &'a str,
}

impl<'a> From<&'a SearchHit> for SearchHitJson<'a> {
    fn from(hit: &'a SearchHit) -> Self {
        Self {
            summary: MemorySummaryJson::from(&hit.memory),
            score: hit.score,
            snippet: &hit.snippet,
        }
    }
}

/// What `list` should return.
#[derive(Debug, Clone, PartialEq)]
pub struct ListQuery {
    /// Which workspaces to cover.
    pub scope: Scope,
    /// Restrict to these kinds. Empty means every kind.
    pub kinds: Vec<MemoryKind>,
    /// Restrict to one status. `None` means every status.
    pub status: Option<Status>,
    /// Require every one of these tags.
    pub tags: Vec<String>,
    /// The most rows to return.
    pub limit: u32,
    /// The ordering.
    pub sort: ListSort,
}

impl Default for ListQuery {
    fn default() -> Self {
        Self {
            scope: Scope::One(DEFAULT_WORKSPACE.to_owned()),
            kinds: Vec::new(),
            // `active` rather than `None`: a bare `list` must never show a user their
            // review inbox mixed into their library.
            status: Some(Status::Active),
            tags: Vec::new(),
            limit: 20,
            sort: ListSort::Strength,
        }
    }
}

/// What an `add` did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AddOutcome {
    /// The id of the memory that now holds the content — new, or the one that was
    /// already there.
    pub id: String,
    /// The workspace it lives in.
    pub workspace_id: String,
    /// Where it is in its lifecycle.
    pub status: Status,
    /// True when nothing was written because an identical memory already existed.
    ///
    /// This is a success, and it is the answer to "my agent saved this twice". The
    /// status travels with it because a duplicate that is `archived` or
    /// `pending_approval` is a duplicate a caller has to act on, and a reply that says
    /// only `duplicate: true` leaves it with no lever — the defect the hosted product's
    /// own plan 028 records.
    pub duplicate: bool,
    /// Anything discarded on the way in. Printed on stderr; never fatal.
    pub warnings: Vec<String>,
}

/// What an `update` did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UpdateOutcome {
    /// The memory that was changed.
    pub id: String,
    /// How many fields the statement actually set. Zero means the patch was empty.
    pub changed: usize,
    /// Anything discarded on the way in.
    pub warnings: Vec<String>,
}

/// What a `forget` did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ForgetOutcome {
    /// The ids that were deleted, with everything keyed to them.
    pub deleted: Vec<String>,
    /// The ids that did not exist. Reported rather than fatal, because deleting five
    /// memories of which one was already gone should not undo the other four.
    pub missing: Vec<String>,
}

/// How much is in the brain.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Stats {
    /// Total rows in scope, whatever their status.
    pub total: i64,
    /// Rows per kind, which is the same vocabulary `--kind` filters on.
    pub by_kind: Vec<KindCount>,
    /// Rows per status.
    pub by_status: Vec<StatusCount>,
    /// Pinned rows.
    pub pinned: i64,
    /// How many workspaces the count covers.
    pub workspaces: i64,
}

/// A count of memories of one kind.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KindCount {
    /// The wire value of the kind.
    pub kind: &'static str,
    /// The user-facing noun for the kind.
    pub kind_label: &'static str,
    /// How many.
    pub count: i64,
}

/// A count of memories in one status.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusCount {
    /// The wire value of the status.
    pub status: &'static str,
    /// How many.
    pub count: i64,
}

// ---------------------------------------------------------------------------
// SQLite conversions.
//
// `FromSql` and `ToSql` rather than parse-and-`unwrap` at every call site: the column
// values are constrained by CHECK clauses, so a value that does not parse is a
// corrupted file. Putting the conversion here means the error surfaces as a row-level
// read failure with the offending value in it, instead of a panic deep inside a mapper.
// ---------------------------------------------------------------------------

/// Parse a wire value out of a column.
fn from_sql_enum<T: FromStr>(
    value: rusqlite::types::ValueRef<'_>,
) -> rusqlite::types::FromSqlResult<T>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let text = value.as_str()?;
    text.parse::<T>()
        .map_err(|err| rusqlite::types::FromSqlError::Other(Box::new(err)))
}

macro_rules! sql_enum {
    ($type:ty) => {
        impl rusqlite::types::FromSql for $type {
            fn column_result(
                value: rusqlite::types::ValueRef<'_>,
            ) -> rusqlite::types::FromSqlResult<Self> {
                from_sql_enum::<Self>(value)
            }
        }

        impl rusqlite::ToSql for $type {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                Ok(rusqlite::types::ToSqlOutput::from(self.as_str()))
            }
        }
    };
}

sql_enum!(MemoryKind);
sql_enum!(Status);
sql_enum!(Relation);
sql_enum!(ReviewMode);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_round_trips_through_its_wire_value() {
        for kind in MemoryKind::ALL {
            assert_eq!(kind.as_str().parse::<MemoryKind>().unwrap(), kind);
            // The Display form is the wire form: `--json` and SQL must never disagree
            // with what the help text calls a value.
            assert_eq!(kind.to_string(), kind.as_str());
        }
    }

    #[test]
    fn every_status_and_relation_round_trips() {
        for status in Status::ALL {
            assert_eq!(status.as_str().parse::<Status>().unwrap(), status);
        }
        for relation in Relation::ALL {
            assert_eq!(relation.as_str().parse::<Relation>().unwrap(), relation);
        }
    }

    #[test]
    fn a_bad_kind_names_the_valid_ones() {
        let err = "knowledge".parse::<MemoryKind>().unwrap_err();
        let message = err.to_string();
        for kind in MemoryKind::ALL {
            assert!(
                message.contains(kind.as_str()),
                "the error must list `{}`: {message}",
                kind.as_str()
            );
        }
        assert_eq!(err.exit_code(), crate::ExitCode::InvalidArgs);
    }

    #[test]
    fn parsing_is_case_and_whitespace_insensitive() {
        // Agents interpolate these values from prose more often than they type them.
        assert_eq!(
            "  Semantic ".parse::<MemoryKind>().unwrap(),
            MemoryKind::Semantic
        );
    }

    #[test]
    fn only_related_to_is_undirected() {
        assert!(!Relation::RelatedTo.is_directed());
        for relation in Relation::ALL {
            if relation != Relation::RelatedTo {
                assert!(relation.is_directed(), "{relation} must be directed");
            }
        }
    }

    #[test]
    fn unknown_sources_are_their_own_label() {
        assert_eq!(source_label("user_saved"), "saved");
        assert_eq!(source_label("agent_reflection"), "reflection");
        assert_eq!(source_label(""), "unknown");
        assert_eq!(source_label("custom_ingest"), "custom_ingest");
    }
}
