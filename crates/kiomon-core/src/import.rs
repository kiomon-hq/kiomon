// SPDX-License-Identifier: Apache-2.0

//! Getting the brain out, and back in: the JSONL export, the markdown view, and the
//! importer that reads both.
//!
//! `export` and `import` exist so the brain is a file you own rather than a database a
//! tool keeps. It is also the migration path: the hosted product's documents and the
//! local store share the memory vocabulary, so a JSONL export is readable by both.
//!
//! # The line is the row
//!
//! A JSONL line is the stored memory, field for field and by the contract's names — no
//! envelope, no nesting, no derived labels. Labels are a *rendering* of `kind` and
//! `source`, and an export that carried them would be a file with two answers to one
//! question.
//!
//! The consequence is that this module is the only place that writes those field names,
//! and it does so once: [`ExportRecord`] serialises for `export` and deserialises for
//! `import`, so the two directions cannot drift. Every field except `kind` and `title` is
//! optional on the way in, because a line written by hand is a legitimate input — and a
//! line missing a title is not.
//!
//! # What is deliberately not in an export
//!
//! **Vectors.** A 384-dimensional vector is about 4 KB of base64 — more than the mean
//! memory — and it is only valid for the model that produced it, on a machine that may
//! not have that model. Text in, vector out: `kiomon reindex --embed` regenerates them,
//! and `import` re-embeds as it writes when an embedder is configured.
//!
//! **Edges.** The graph exists since WP5 and the format still carries none of it: a
//! restore brings back every memory and no relationships, and `import` does not auto-link
//! what it writes, because a bulk restore is not an activation. `export`'s summary says
//! `edges_included: false` so a caller learns that before the restore rather than after
//! it. This is the one absence in the file that is a real loss rather than a saving — a
//! vector is derived data that `reindex --embed` rebuilds, and an edge is a fact somebody
//! recorded — and it is recorded as the format's next piece of work. A draft's *pending*
//! provenance (`--derived-from` before approval) is a column rather than an edge, so it
//! does travel, as `derived_from`.
//!
//! # Two limits that are not the database's
//!
//! [`MAX_BODY_BYTES`], [`MAX_TITLE`] and [`MAX_EXCERPT`] are enforced on every write,
//! not only on import — see the plan's WP6 record for why one rule beats two. A line that
//! breaks one is a line-level error: the rest of the file still imports.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config;
use crate::error::{Error, Result};
use crate::hash;
use crate::metadata;
use crate::model::{MAX_BODY_BYTES, MAX_EXCERPT, MAX_TITLE, Memory, MemoryKind, Status};

/// One memory, as it goes into a JSONL export and comes back out of one.
///
/// Field order here is the field order in the file, and it matches the store's column
/// list, so a diff between two exports of one brain is a diff of the rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportRecord {
    /// `k_<ulid>`. Absent on a hand-written line, which gets a fresh id on import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The workspace's id. `--workspace` on `import` overrides it for every line.
    #[serde(default = "default_workspace")]
    pub workspace_id: String,
    /// The taxonomy facet.
    pub kind: MemoryKind,
    /// Short, searchable heading.
    pub title: String,
    /// The one-line subheading.
    #[serde(default)]
    pub excerpt: String,
    /// The full content.
    #[serde(default)]
    pub body: String,
    /// Tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// The per-kind envelope.
    #[serde(default = "empty_object")]
    pub memory_metadata: Value,
    /// Provenance: `user_saved`, `agent_reflection`, `import`, `connector_sync`.
    #[serde(default = "default_source")]
    pub source: String,
    /// Where the content came from.
    #[serde(default)]
    pub source_url: Option<String>,
    /// The writer's reliability estimate.
    #[serde(default)]
    pub confidence: Option<f64>,
    /// How much it matters. The ceiling decay falls away from.
    #[serde(default)]
    pub importance: Option<f64>,
    /// Decay's output, carried so a restored brain does not relive six months of decay.
    #[serde(default)]
    pub strength: Option<f64>,
    /// How many independent writes have asserted the same thing.
    #[serde(default)]
    pub corroboration_count: Option<i64>,
    /// How many retrievals returned it.
    #[serde(default)]
    pub retrieval_count: Option<i64>,
    /// The last retrieval, RFC 3339 UTC.
    #[serde(default)]
    pub last_retrieved_at: Option<String>,
    /// Accumulated rating credit.
    #[serde(default)]
    pub feedback_score: Option<f64>,
    /// How many ratings that average is over.
    #[serde(default)]
    pub feedback_count: Option<i64>,
    /// Immune to decay, first in every ordering.
    #[serde(default)]
    pub pinned: bool,
    /// Where it is in its lifecycle. Absent means `active`.
    #[serde(default)]
    pub status: Option<Status>,
    /// When it stops being retrievable.
    #[serde(default)]
    pub expires_at: Option<String>,
    /// Creation, RFC 3339 UTC. Absent means now.
    #[serde(default)]
    pub created_at: Option<String>,
    /// Last modification. Absent means now.
    #[serde(default)]
    pub updated_at: Option<String>,
    /// A draft's provenance, waiting to become an edge on approval.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derived_from: Vec<String>,
}

fn default_workspace() -> String {
    crate::model::DEFAULT_WORKSPACE.to_owned()
}

fn default_source() -> String {
    "user_saved".to_owned()
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

impl ExportRecord {
    /// The export line for one stored memory.
    #[must_use]
    pub fn from_memory(memory: &Memory, pending_derived_from: Option<&str>) -> Self {
        Self {
            id: Some(memory.id.clone()),
            workspace_id: memory.workspace_id.clone(),
            kind: memory.kind,
            title: memory.title.clone(),
            excerpt: memory.excerpt.clone(),
            body: memory.body.clone(),
            tags: memory.tags.clone(),
            memory_metadata: memory.memory_metadata.clone(),
            source: memory.source.clone(),
            source_url: memory.source_url.clone(),
            confidence: memory.confidence,
            importance: Some(memory.importance),
            strength: Some(memory.strength),
            corroboration_count: Some(memory.corroboration_count),
            retrieval_count: Some(memory.retrieval_count),
            last_retrieved_at: memory.last_retrieved_at.clone(),
            feedback_score: Some(memory.feedback_score),
            feedback_count: Some(memory.feedback_count),
            pinned: memory.pinned,
            status: Some(memory.status),
            expires_at: memory.expires_at.clone(),
            created_at: Some(memory.created_at.clone()),
            updated_at: Some(memory.updated_at.clone()),
            derived_from: pending_derived_from
                .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
                .unwrap_or_default(),
        }
    }

    /// One JSONL line, without its newline.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Internal`] if the record cannot be serialised, which would be a
    /// bug in the record type rather than anything a user did.
    pub fn to_jsonl(&self) -> Result<String> {
        serde_json::to_string(self)
            .map_err(|err| Error::Internal(format!("could not encode a memory as JSONL: {err}")))
    }

    /// Check the line and turn it into what the store writes.
    ///
    /// `workspace` is `import --workspace`: an override for every line, which is how a
    /// brain moves from one workspace into another without the file being rewritten.
    ///
    /// # Errors
    ///
    /// Returns a message naming the field and the limit. It is a `String` rather than an
    /// [`Error`] because it belongs to one line of one file, and the caller's job is to
    /// report it and read the next line — not to abort a hundred thousand of them for
    /// one bad row.
    pub fn into_row(self, workspace: Option<&str>) -> std::result::Result<ImportRow, String> {
        // Before the destructure: `check` borrows the whole record, and destructuring by
        // value is what makes the rest of this function readable.
        self.check()?;
        let ExportRecord {
            id,
            workspace_id,
            kind,
            title,
            excerpt,
            body,
            tags,
            memory_metadata,
            source,
            source_url,
            confidence,
            importance,
            strength,
            corroboration_count,
            retrieval_count,
            last_retrieved_at,
            feedback_score,
            feedback_count,
            pinned,
            status,
            expires_at,
            created_at,
            updated_at,
            derived_from,
        } = self;

        let mut warnings = Vec::new();

        let id = match id {
            Some(raw) => {
                crate::id::decode(&raw).map_err(|err| err.to_string())?;
                Some(raw)
            }
            None => None,
        };

        // The override is a *name*, so it goes through the same derivation `init` uses;
        // the record's own value is an id from a previous export and passes through
        // unchanged when it is already one.
        let requested = match workspace {
            Some(over) => over.to_owned(),
            None => workspace_id,
        };
        let workspace_id =
            config::workspace_id_from_name(&requested).map_err(|err| err.to_string())?;

        let title = title.trim().to_owned();
        let source = source.trim().to_owned();

        // Tag normalisation is the store's own: the same trimming, deduplication and
        // twenty-tag cap a typed `add` gets, so an import cannot smuggle in a shape the
        // rest of the engine has never had to handle.
        let tags = metadata::normalize_tags(&tags)?;
        let validated = metadata::validate(kind, Some(&memory_metadata));
        if let Some(warning) = validated.warning {
            warnings.push(warning);
        }

        let status = status.unwrap_or(Status::Active);
        // A draft's provenance waits on the row until `review approve` wires it. An
        // active memory's edges are not exported at all (see the module note), so
        // carrying the ids for one would be a promise this format does not keep.
        let pending_derived_from = if derived_from.is_empty() {
            None
        } else if status == Status::Active {
            warnings.push(
                "`derived_from` was ignored: this memory is active, and an active memory's \
                 edges are not part of the export format"
                    .to_owned(),
            );
            None
        } else {
            serde_json::to_string(&derived_from).ok()
        };

        let content_hash = hash::content_hash(kind, &title, &body);

        Ok(ImportRow {
            id,
            workspace_id,
            kind,
            title,
            excerpt,
            body,
            tags,
            memory_metadata: validated.value,
            source,
            source_url,
            confidence,
            importance: importance.unwrap_or(0.5),
            strength: strength.unwrap_or(1.0),
            corroboration_count: corroboration_count.unwrap_or(0),
            retrieval_count: retrieval_count.unwrap_or(0),
            last_retrieved_at,
            feedback_score: feedback_score.unwrap_or(0.0),
            feedback_count: feedback_count.unwrap_or(0),
            pinned,
            status,
            expires_at,
            created_at,
            updated_at,
            pending_derived_from,
            content_hash,
            warnings,
        })
    }
}

impl ExportRecord {
    /// Every field-level refusal, in one pass, before anything is built.
    ///
    /// Whether `kind` and `title` are *present* is serde's business, and a missing field
    /// is reported by the parser with the line it was on. What is left is everything a
    /// hostile file can get wrong while still being valid JSON: a limit, a range, a
    /// count, or a timestamp that would sort wrong.
    fn check(&self) -> std::result::Result<(), String> {
        let ExportRecord {
            title,
            excerpt,
            body,
            source,
            confidence,
            importance,
            strength,
            corroboration_count,
            retrieval_count,
            last_retrieved_at,
            feedback_score,
            feedback_count,
            expires_at,
            created_at,
            updated_at,
            ..
        } = self;
        let title = title.trim().to_owned();
        if title.is_empty() {
            return Err("`title` is empty; a memory needs one".to_owned());
        }
        if title.chars().count() > MAX_TITLE {
            return Err(format!(
                "`title` is {} characters, over the {MAX_TITLE}-character limit",
                title.chars().count()
            ));
        }
        if excerpt.chars().count() > MAX_EXCERPT {
            return Err(format!(
                "`excerpt` is {} characters, over the {MAX_EXCERPT}-character limit",
                excerpt.chars().count()
            ));
        }
        if body.len() > MAX_BODY_BYTES {
            return Err(format!(
                "`body` is {} bytes, over the {MAX_BODY_BYTES}-byte limit",
                body.len()
            ));
        }
        if source.trim().is_empty() {
            return Err("`source` is empty; omit it to record a `user_saved` memory".to_owned());
        }

        check_unit("`confidence`", *confidence)?;
        check_unit("`importance`", *importance)?;
        if strength.is_some_and(|value| !(value.is_finite() && value >= 0.0)) {
            return Err("`strength` must be zero or more".to_owned());
        }
        for (name, count) in [
            ("`corroboration_count`", corroboration_count),
            ("`retrieval_count`", retrieval_count),
            ("`feedback_count`", feedback_count),
        ] {
            if count.is_some_and(|value| value < 0) {
                return Err(format!("{name} must be zero or more"));
            }
        }
        if feedback_score.is_some_and(|value| !value.is_finite()) {
            return Err("`feedback_score` must be a finite number".to_owned());
        }

        for (name, value) in [
            ("`created_at`", &created_at),
            ("`updated_at`", &updated_at),
            ("`last_retrieved_at`", &last_retrieved_at),
            ("`expires_at`", &expires_at),
        ] {
            if let Some(raw) = value {
                if !is_timestamp(raw) {
                    return Err(format!(
                        "{name} is `{raw}`; timestamps are \
                         `YYYY-MM-DDTHH:MM:SSZ` (RFC 3339, UTC, seconds)"
                    ));
                }
            }
        }

        Ok(())
    }
}

fn check_unit(name: &str, value: Option<f64>) -> std::result::Result<(), String> {
    match value {
        Some(value) if !(value.is_finite() && (0.0..=1.0).contains(&value)) => {
            Err(format!("{name} is {value}; it must be between 0.0 and 1.0"))
        }
        _ => Ok(()),
    }
}

/// Whether a string is the timestamp shape every date column holds.
///
/// Checked rather than parsed: this crate has no date library, and the shape is the whole
/// requirement — fixed width, UTC, seconds — because it is what makes `expires_at` and the
/// decay window comparable as strings in SQL. A *plausible* wrong date (February 31st)
/// passes, and nothing downstream cares: no code here ever interprets a timestamp's
/// meaning, only its order.
#[must_use]
pub fn is_timestamp(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return false;
    }
    bytes
        .iter()
        .enumerate()
        .all(|(index, byte)| matches!(index, 4 | 7 | 10 | 13 | 16 | 19) || byte.is_ascii_digit())
}

/// A validated line, ready for the store.
///
/// The fields are the store's columns; the type exists so validation happens once, before
/// a transaction is open, and so a batch cannot half-write a row that failed a check.
#[derive(Debug, Clone)]
pub struct ImportRow {
    /// The id to keep, or `None` for a new one.
    pub id: Option<String>,
    /// The workspace, as an id.
    pub workspace_id: String,
    /// The taxonomy facet.
    pub kind: MemoryKind,
    /// Short, searchable heading.
    pub title: String,
    /// The one-line subheading.
    pub excerpt: String,
    /// The full content.
    pub body: String,
    /// Normalised tags.
    pub tags: Vec<String>,
    /// The validated envelope.
    pub memory_metadata: Value,
    /// Provenance.
    pub source: String,
    /// Where the content came from.
    pub source_url: Option<String>,
    /// The writer's reliability estimate.
    pub confidence: Option<f64>,
    /// How much it matters.
    pub importance: f64,
    /// Decay's output at export time.
    pub strength: f64,
    /// Independent writes that asserted the same thing.
    pub corroboration_count: i64,
    /// Retrievals.
    pub retrieval_count: i64,
    /// The last retrieval.
    pub last_retrieved_at: Option<String>,
    /// Accumulated rating credit.
    pub feedback_score: f64,
    /// How many ratings that average is over.
    pub feedback_count: i64,
    /// Immune to decay.
    pub pinned: bool,
    /// The lifecycle state.
    pub status: Status,
    /// When it stops being retrievable.
    pub expires_at: Option<String>,
    /// Creation, or `None` for now.
    pub created_at: Option<String>,
    /// Last modification, or `None` for now.
    pub updated_at: Option<String>,
    /// A draft's staged provenance, as JSON.
    pub pending_derived_from: Option<String>,
    /// The idempotence key, computed the same way `add` computes it.
    pub content_hash: String,
    /// Anything the line got away with.
    pub warnings: Vec<String>,
}

impl ImportRow {
    /// The new memory this row would produce, for the content dedupe check.
    ///
    /// Built through [`crate::model::NewMemory`] so an import is deduped against the
    /// same normalisation a typed write is: importing an export of a brain into itself
    /// is a no-op rather than a second copy under a second id.
    #[must_use]
    pub fn as_new(&self) -> crate::model::NewMemory {
        crate::model::NewMemory {
            workspace_id: self.workspace_id.clone(),
            kind: self.kind,
            title: self.title.clone(),
            excerpt: self.excerpt.clone(),
            body: self.body.clone(),
            tags: self.tags.clone(),
            memory_metadata: Some(self.memory_metadata.clone()),
            source: self.source.clone(),
            source_url: self.source_url.clone(),
            confidence: self.confidence,
            importance: self.importance,
            derived_from: Vec::new(),
            force: false,
        }
    }
}

/// What happened to one line of an import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportState {
    /// The row was written (or would be, on a dry run).
    Written,
    /// A memory with this id is already here, and this line changed nothing.
    Exists,
    /// A memory with this content is already here, under another id.
    Duplicate,
}

impl ImportState {
    /// The line's one-word verdict, for a summary and for `--json`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Written => "written",
            Self::Exists => "exists",
            Self::Duplicate => "duplicate",
        }
    }
}

/// One line's outcome, with the id it ended up with.
///
/// The id matters for a line that came in without one: it is the only way a caller learns
/// what the memory is called now, and a summary that said "imported 12" without saying
/// which twelve would make the command useless to an agent scripting a migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportOutcome {
    /// The id the store holds this memory under.
    pub id: String,
    /// What happened to it.
    pub state: ImportState,
}

/// What a page of imports did.
#[derive(Debug, Clone, Default)]
pub struct ImportBatch {
    /// One entry per line, in the order the lines arrived.
    pub outcomes: Vec<ImportOutcome>,
    /// Workspaces this page created, because the file named them and the store had not.
    pub created_workspaces: Vec<String>,
    /// Whether anything was actually kept. False on a dry run, where the transaction was
    /// rolled back after the counts were read.
    pub committed: bool,
}

impl ImportBatch {
    /// How many rows were (or would be) written.
    #[must_use]
    pub fn written(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| outcome.state == ImportState::Written)
            .count()
    }

    /// The ids a caller should embed, in the order they were written.
    #[must_use]
    pub fn written_ids(&self) -> Vec<String> {
        self.outcomes
            .iter()
            .filter(|outcome| outcome.state == ImportState::Written)
            .map(|outcome| outcome.id.clone())
            .collect()
    }

    /// How many lines were already here, by id or by content.
    #[must_use]
    pub fn skipped(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| outcome.state != ImportState::Written)
            .count()
    }

    /// Fold another page's counts into this one.
    pub fn absorb(&mut self, other: Self) {
        self.outcomes.extend(other.outcomes);
        for workspace in other.created_workspaces {
            if !self.created_workspaces.contains(&workspace) {
                self.created_workspaces.push(workspace);
            }
        }
        self.committed |= other.committed;
    }
}

// ---------------------------------------------------------------------------
// Markdown
// ---------------------------------------------------------------------------

/// The frontmatter a markdown export writes, and the only keys it reads back.
///
/// A subset on purpose: markdown is the human view, and a format that could express every
/// column would be a database with `---` around it. What is missing here is what a reader
/// does not need to *read* the corpus — counters, the content hash, the feedback score.
const MARKDOWN_KEYS: [&str; 11] = [
    "id",
    "kind",
    "title",
    "workspace",
    "status",
    "tags",
    "source",
    "url",
    "confidence",
    "importance",
    "created",
];

/// One memory as a markdown document with frontmatter.
///
/// Written as `title: value` where the value is bare unless reading it back would be
/// ambiguous — see [`scalar`]. Frontmatter is borrowed from static-site conventions
/// because every editor and every agent already renders it, and because a note with a
/// `## ` heading does not need to escape anything.
#[must_use]
pub fn to_markdown(memory: &Memory) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(memory.body.len() + 512);
    out.push_str("---\n");
    let _ = writeln_key(&mut out, "id", &memory.id);
    let _ = writeln_key(&mut out, "kind", memory.kind.as_str());
    let _ = writeln_key(&mut out, "title", &memory.title);
    let _ = writeln_key(&mut out, "workspace", &memory.workspace_id);
    let _ = writeln_key(&mut out, "status", memory.status.as_str());
    if !memory.tags.is_empty() {
        let items: Vec<String> = memory.tags.iter().map(|tag| scalar(tag)).collect();
        // Written directly rather than through `writeln_key`: the list is already the
        // JSON-ish form, and `scalar` would quote the brackets that make it a list.
        let _ = writeln!(out, "tags: [{}]", items.join(", "));
    }
    let _ = writeln_key(&mut out, "source", &memory.source);
    if let Some(url) = &memory.source_url {
        let _ = writeln_key(&mut out, "url", url);
    }
    // Bare, unlike every string-valued key: these two *are* numbers, the reader parses
    // them as numbers, and quoting them would make a hand-edit look like a typo.
    if let Some(confidence) = memory.confidence {
        let _ = writeln!(out, "confidence: {confidence}");
    }
    let _ = writeln!(out, "importance: {}", memory.importance);
    let _ = writeln_key(&mut out, "created", &memory.created_at);
    out.push_str("---\n\n");
    if !memory.excerpt.trim().is_empty() {
        out.push_str(memory.excerpt.trim());
        out.push_str("\n\n");
    }
    out.push_str(&memory.body);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// `key: value`, with the value quoted only when reading it back would be ambiguous.
fn writeln_key(out: &mut String, key: &str, value: &str) -> std::fmt::Result {
    use std::fmt::Write as _;
    writeln!(out, "{key}: {}", scalar(value))
}

/// Render a frontmatter value so [`from_markdown`] reads back exactly what was written.
///
/// A bare value is what a human writes, so bare is the default. It becomes a JSON string
/// when the bare form would be read as something else: a list, an object, a number, a
/// boolean, `null`, or an empty string. Frontmatter is not YAML here — it is `key: JSON
/// scalar`, which is a grammar a hand-edited note and a generated file can both obey.
fn scalar(value: &str) -> String {
    let ambiguous = value.is_empty()
        || value
            .chars()
            .next()
            .is_some_and(|first| matches!(first, '[' | '{' | '"' | '\'' | '-' | '#'))
        || matches!(value, "true" | "false" | "null")
        || value.contains(": ")
        || value.ends_with(':')
        // A *whole* number, not merely something that starts with a digit:
        // `2026-10-18T07:17:25Z` is a timestamp and reads back as one, while `123` would
        // read as a number in any other frontmatter parser and is quoted to say it is the
        // string it is.
        || value.parse::<f64>().is_ok();
    if ambiguous {
        // `serde_json` rather than manual escaping: one escaper in the program, and it is
        // the one that is already trusted with every other string it writes.
        serde_json::to_string(value).unwrap_or_else(|_| format!("\"{value}\""))
    } else {
        value.to_owned()
    }
}

/// Split a document into its frontmatter fields and the rest of it.
///
/// Returns the fields, the body, and whether there was frontmatter at all — the third is
/// what tells a note somebody wrote from a document this tool exported, and therefore
/// what the memory's `source` should be.
///
/// Only the keys in [`MARKDOWN_KEYS`] are read. A note with a `date:` or a `cssclass:`
/// key from whichever editor wrote it imports cleanly and drops what a memory has no
/// column for, rather than refusing over metadata nobody asked for.
///
/// # Errors
///
/// Returns a message when the frontmatter opens and never closes: guessing where it
/// ended would put half a document's metadata into the body.
fn split_frontmatter(
    text: &str,
) -> std::result::Result<(BTreeMap<String, String>, String, bool), String> {
    let normalized = text.replace("\r\n", "\n");
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    let mut rest = normalized.as_str();
    let mut had_frontmatter = false;

    if let Some(after) = normalized.strip_prefix("---\n") {
        let mut offset = 4;
        let mut closed = false;
        for line in after.split_inclusive('\n') {
            offset += line.len();
            let trimmed = line.trim_end_matches('\n').trim_end();
            if trimmed == "---" {
                closed = true;
                break;
            }
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = trimmed.split_once(':') {
                let key = key.trim().to_ascii_lowercase();
                if MARKDOWN_KEYS.contains(&key.as_str()) {
                    fields.insert(key, value.trim().to_owned());
                }
            }
        }
        if !closed {
            return Err("the frontmatter opens with `---` and never closes".to_owned());
        }
        had_frontmatter = true;
        rest = normalized.get(offset..).unwrap_or("");
    }
    Ok((fields, rest.to_owned(), had_frontmatter))
}

/// Read the frontmatter's own values onto the record.
///
/// A key that is present but unreadable is an error rather than a warning: the user wrote
/// `confidence: high`, and importing the memory without the confidence they asked for
/// would be a silent difference between the file and what is stored.
///
/// # Errors
///
/// Returns a message naming the key.
fn apply_fields(
    record: &mut ExportRecord,
    fields: &BTreeMap<String, String>,
) -> std::result::Result<(), String> {
    if let Some(raw) = fields.get("id") {
        record.id = Some(text_value(raw)?);
    }
    if let Some(raw) = fields.get("kind") {
        record.kind = text_value(raw)?
            .parse()
            .map_err(|err: Error| err.to_string())?;
    }
    if let Some(raw) = fields.get("workspace") {
        record.workspace_id = text_value(raw)?;
    }
    if let Some(raw) = fields.get("status") {
        record.status = Some(
            text_value(raw)?
                .parse()
                .map_err(|err: Error| err.to_string())?,
        );
    }
    if let Some(raw) = fields.get("tags") {
        record.tags = list_value(raw);
    }
    if let Some(raw) = fields.get("source") {
        record.source = text_value(raw)?;
    }
    if let Some(raw) = fields.get("url") {
        record.source_url = Some(text_value(raw)?);
    }
    if let Some(raw) = fields.get("confidence") {
        record.confidence = Some(number_value("confidence", raw)?);
    }
    if let Some(raw) = fields.get("importance") {
        record.importance = Some(number_value("importance", raw)?);
    }
    if let Some(raw) = fields.get("created") {
        record.created_at = Some(text_value(raw)?);
    }
    Ok(())
}

/// A memory from a markdown document.
///
/// `stem` is the file's name without its extension, used as the title when the document
/// has neither frontmatter nor a heading — which is what a directory of notes looks like.
///
/// # Errors
///
/// Returns a message when the frontmatter is unterminated or a known key has a value that
/// cannot be read as what it claims to be.
pub fn from_markdown(text: &str, stem: &str) -> std::result::Result<ExportRecord, String> {
    let (fields, rest, had_frontmatter) = split_frontmatter(text)?;
    // A heading is the title if the frontmatter did not give one, and it is consumed:
    // leaving `# Staging deploys` in the body would put the title in the text twice, and
    // the second copy is what a search snippet would quote.
    let mut body = rest.trim_start_matches('\n');
    let mut heading: Option<String> = None;
    if let Some(stripped) = body.strip_prefix("# ") {
        let (line, remainder) = stripped.split_once('\n').unwrap_or((stripped, ""));
        heading = Some(line.trim().to_owned());
        body = remainder.trim_start_matches('\n');
    }

    let title = match fields.get("title") {
        Some(raw) => text_value(raw)?,
        None => heading.unwrap_or_else(|| stem.to_owned()),
    };
    if title.is_empty() {
        return Err(
            "no title: add `title:` to the frontmatter, a `# heading`, or name the file".to_owned(),
        );
    }

    let mut record = ExportRecord {
        id: None,
        workspace_id: default_workspace(),
        kind: MemoryKind::Reference,
        title,
        excerpt: String::new(),
        body: body.trim_end().to_owned(),
        tags: Vec::new(),
        memory_metadata: empty_object(),
        source: "import".to_owned(),
        source_url: None,
        confidence: None,
        importance: None,
        strength: None,
        corroboration_count: None,
        retrieval_count: None,
        last_retrieved_at: None,
        feedback_score: None,
        feedback_count: None,
        pinned: false,
        status: None,
        expires_at: None,
        created_at: None,
        updated_at: None,
        derived_from: Vec::new(),
    };

    apply_fields(&mut record, &fields)?;
    // A note with no frontmatter was not produced by `export`, so it is not a memory that
    // was saved from the web or from an agent: `import` is the honest provenance, and it
    // is what tells a later reader (and `doctor`) where the row came from.
    if !had_frontmatter {
        "import".clone_into(&mut record.source);
    }

    Ok(record)
}

fn text_value(raw: &str) -> std::result::Result<String, String> {
    let raw = raw.trim();
    if raw.starts_with('"') {
        return serde_json::from_str::<String>(raw)
            .map_err(|_| format!("`{raw}` is not a quoted string"));
    }
    if let Some(stripped) = raw.strip_prefix('\'') {
        if let Some(inner) = stripped.strip_suffix('\'') {
            return Ok(inner.to_owned());
        }
    }
    Ok(raw.to_owned())
}

fn number_value(key: &str, raw: &str) -> std::result::Result<f64, String> {
    text_value(raw)?
        .parse()
        .map_err(|_| format!("`{key}` is `{raw}`, which is not a number"))
}

fn list_value(raw: &str) -> Vec<String> {
    let raw = raw.trim();
    if raw.starts_with('[') {
        // The generated form, and the one a JSON-minded hand writes. The fallback below is
        // for the other hand: `[deploy, staging]`, which is what a bracket looks like to
        // somebody who has read three lines of YAML in their life. Readable, never
        // ambiguous — a tag containing a comma or a bracket is not a thing this format
        // supports, and saying so would cost more than it explains.
        if let Ok(items) = serde_json::from_str::<Vec<String>>(raw) {
            return items;
        }
        let inner = raw
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .unwrap_or(raw);
        return split_list(inner);
    }
    split_list(raw)
}

fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|item| item.trim().trim_matches('"').trim().to_owned())
        .filter(|item| !item.is_empty())
        .collect()
}

/// A filename for one memory: `k_<ulid>-a-short-slug.md`.
///
/// The id leads so the directory sorts chronologically and so two memories with the same
/// title cannot collide. The slug is cosmetic — the reader's eye, and the window title.
#[must_use]
pub fn markdown_filename(memory: &Memory) -> String {
    let mut slug = String::with_capacity(60);
    for ch in memory.title.chars() {
        if slug.chars().count() >= 60 {
            break;
        }
        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                slug.push(lower);
            }
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        format!("{}.md", memory.id)
    } else {
        format!("{}-{slug}.md", memory.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DEFAULT_WORKSPACE, ListQuery, NewMemory};

    fn memory() -> Memory {
        let db = crate::db::Database::open_in_memory().unwrap();
        db.add(
            &NewMemory::new(
                MemoryKind::Semantic,
                "Staging deploys need a manual migration",
            )
            .body("Run migrations before the deploy.")
            .tags(vec!["deploy".to_owned()]),
        )
        .unwrap();
        db.get(&db.list(&ListQuery::default()).unwrap()[0].id)
            .unwrap()
    }

    #[test]
    fn a_line_that_round_trips_through_json_keeps_every_field_that_matters() {
        let memory = memory();
        let line = ExportRecord::from_memory(&memory, None).to_jsonl().unwrap();
        let parsed: ExportRecord = serde_json::from_str(&line).unwrap();
        let row = parsed.into_row(None).unwrap();
        assert_eq!(row.title, memory.title);
        assert_eq!(row.kind, memory.kind);
        assert_eq!(row.tags, memory.tags);
        assert_eq!(row.confidence, memory.confidence);
        assert!((row.importance - memory.importance).abs() < f64::EPSILON);
        assert_eq!(row.created_at.as_deref(), Some(memory.created_at.as_str()));
        assert_eq!(row.status, memory.status);
        assert_eq!(row.workspace_id, DEFAULT_WORKSPACE);
    }

    #[test]
    fn a_hand_written_line_needs_a_kind_and_a_title_and_nothing_else() {
        let record: ExportRecord =
            serde_json::from_str(r#"{"kind":"semantic","title":"Bare"}"#).unwrap();
        let row = record.into_row(None).unwrap();
        assert_eq!(row.title, "Bare");
        assert!(row.id.is_none());
        assert_eq!(row.source, "user_saved");
        assert!((row.importance - 0.5).abs() < f64::EPSILON);
        assert!((row.strength - 1.0).abs() < f64::EPSILON);
        assert_eq!(row.status, Status::Active);
    }

    #[test]
    fn a_line_without_a_title_is_refused_by_name() {
        let error = serde_json::from_str::<ExportRecord>(r#"{"kind":"semantic"}"#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("title"), "{error}");
    }

    #[test]
    fn a_wrong_type_is_refused_rather_than_coerced() {
        // The message names the *type* and the column, not the key: serde has no path
        // tracking here. The line number the caller prefixes is what makes it actionable,
        // and `import` prints the fragment it could not read for exactly that reason.
        let error = serde_json::from_str::<ExportRecord>(
            r#"{"kind":"semantic","title":"t","confidence":"high"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("expected f64"), "{error}");
    }

    #[test]
    fn limits_name_the_field_and_the_number() {
        let huge = "x".repeat(MAX_BODY_BYTES + 1);
        let line = format!(r#"{{"kind":"semantic","title":"t","body":"{huge}"}}"#);
        let record: ExportRecord = serde_json::from_str(&line).unwrap();
        let error = record.into_row(None).unwrap_err();
        assert!(error.contains("`body`"), "{error}");
        assert!(error.contains(&MAX_BODY_BYTES.to_string()), "{error}");

        let record: ExportRecord = serde_json::from_str(&format!(
            r#"{{"kind":"semantic","title":"{}"}}"#,
            "t".repeat(MAX_TITLE + 1)
        ))
        .unwrap();
        assert!(record.into_row(None).unwrap_err().contains("`title`"));
    }

    #[test]
    fn an_unknown_kind_is_refused_by_serde_and_an_unknown_status_too() {
        assert!(serde_json::from_str::<ExportRecord>(r#"{"kind":"vibes","title":"t"}"#).is_err());
        assert!(
            serde_json::from_str::<ExportRecord>(
                r#"{"kind":"semantic","title":"t","status":"deleted"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn a_malformed_timestamp_is_refused_because_it_would_sort_wrong() {
        let record: ExportRecord =
            serde_json::from_str(r#"{"kind":"semantic","title":"t","created_at":"yesterday"}"#)
                .unwrap();
        let error = record.into_row(None).unwrap_err();
        assert!(error.contains("`created_at`"), "{error}");
        assert!(is_timestamp("2026-10-14T09:12:00Z"));
        assert!(!is_timestamp("2026-10-14 09:12:00Z"));
        assert!(!is_timestamp("2026-10-14T09:12:00+02:00"));
    }

    #[test]
    fn a_workspace_override_repackages_without_touching_the_file() {
        let memory = memory();
        let record = ExportRecord::from_memory(&memory, None);
        let row = record.into_row(Some("Work Stuff")).unwrap();
        assert_eq!(row.workspace_id, "work-stuff");
    }

    #[test]
    fn an_id_that_is_not_a_memory_id_is_refused() {
        let record: ExportRecord =
            serde_json::from_str(r#"{"kind":"semantic","title":"t","id":"../../etc/passwd"}"#)
                .unwrap();
        assert!(record.into_row(None).is_err());
    }

    #[test]
    fn a_draft_keeps_its_staged_provenance_and_an_active_memory_says_why_not() {
        let record: ExportRecord = serde_json::from_str(
            r#"{"kind":"semantic","title":"t","status":"pending_approval","derived_from":["k_a","k_b"]}"#,
        )
        .unwrap();
        let row = record.into_row(None).unwrap();
        assert_eq!(
            row.pending_derived_from.as_deref(),
            Some(r#"["k_a","k_b"]"#)
        );

        let record: ExportRecord =
            serde_json::from_str(r#"{"kind":"semantic","title":"t","derived_from":["k_a"]}"#)
                .unwrap();
        let row = record.into_row(None).unwrap();
        assert!(row.pending_derived_from.is_none());
        assert!(row.warnings.iter().any(|w| w.contains("derived_from")));
    }

    #[test]
    fn markdown_round_trips_the_fields_it_claims_to() {
        let memory = memory();
        let document = to_markdown(&memory);
        let record = from_markdown(&document, "stem").unwrap();
        let row = record.into_row(None).unwrap();
        assert_eq!(row.title, memory.title);
        assert_eq!(row.kind, memory.kind);
        assert_eq!(row.tags, memory.tags);
        assert_eq!(row.body, memory.body);
        assert_eq!(row.id.as_deref(), Some(memory.id.as_str()));
        assert!((row.importance - memory.importance).abs() < f64::EPSILON);
        assert_eq!(row.created_at.as_deref(), Some(memory.created_at.as_str()));
    }

    #[test]
    fn a_plain_note_gets_its_title_from_the_heading_and_its_source_from_the_importer() {
        let record = from_markdown("# Deploys\n\nThe pipeline is broken.\n", "note").unwrap();
        assert_eq!(record.title, "Deploys");
        assert_eq!(record.body, "The pipeline is broken.");
        assert_eq!(record.source, "import");
        assert_eq!(record.kind, MemoryKind::Reference);
    }

    #[test]
    fn a_note_with_no_heading_takes_its_filename() {
        let record = from_markdown("just text", "my-note").unwrap();
        assert_eq!(record.title, "my-note");
        assert_eq!(record.body, "just text");
    }

    #[test]
    fn an_unterminated_frontmatter_is_refused_rather_than_swallowing_the_body() {
        let error = from_markdown("---\ntitle: x\n\nbody", "stem").unwrap_err();
        assert!(error.contains("never closes"), "{error}");
    }

    #[test]
    fn a_value_that_would_read_back_as_something_else_is_quoted() {
        for tricky in [
            "[not, a, list]",
            "{obj}",
            "123",
            "true",
            "- dash",
            "",
            ": colon",
        ] {
            assert_eq!(
                scalar(tricky),
                serde_json::to_string(tricky).unwrap(),
                "{tricky}"
            );
        }
        assert_eq!(
            scalar("Deploys need a migration"),
            "Deploys need a migration"
        );
        assert_eq!(scalar("Staging: the story"), "\"Staging: the story\"");
        assert_eq!(scalar("2026-10-18T07:17:25Z"), "2026-10-18T07:17:25Z");
    }

    #[test]
    fn the_filename_leads_with_the_id_and_survives_a_hostile_title() {
        let mut memory = memory();
        memory.title = "../../etc/passwd".to_owned();
        let name = markdown_filename(&memory);
        assert!(name.starts_with(&memory.id), "{name}");
        assert!(!name.contains('/'), "{name}");
        assert!(!name.contains('\\'), "{name}");
        assert_eq!(
            std::path::Path::new(&name)
                .extension()
                .and_then(|ext| ext.to_str()),
            Some("md")
        );

        memory.title = "日本語".to_owned();
        assert!(markdown_filename(&memory).contains('日'));
    }

    #[test]
    fn a_crlf_document_reads_the_same_as_an_lf_one() {
        let lf = to_markdown(&memory());
        let crlf = lf.replace('\n', "\r\n");
        let a = from_markdown(&lf, "stem").unwrap();
        let b = from_markdown(&crlf, "stem").unwrap();
        assert_eq!(a.title, b.title);
        assert_eq!(a.body, b.body);
    }

    #[test]
    fn tags_written_by_hand_as_a_list_or_a_comma_list_both_read() {
        let record = from_markdown("---\ntitle: t\ntags: deploy, staging\n---\n", "s").unwrap();
        assert_eq!(record.tags, vec!["deploy".to_owned(), "staging".to_owned()]);
        let record = from_markdown("---\ntitle: t\ntags: [\"a b\", c]\n---\n", "s").unwrap();
        assert_eq!(record.tags, vec!["a b".to_owned(), "c".to_owned()]);
    }
}
