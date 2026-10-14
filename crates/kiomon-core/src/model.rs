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
