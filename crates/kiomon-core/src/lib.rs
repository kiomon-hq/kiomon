// SPDX-License-Identifier: Apache-2.0

//! A fast, local, SQLite-backed memory for coding agents.
//!
//! `kiomon-core` is the engine behind the [`kiomon`](https://github.com/kiomon-hq/kiomon)
//! command-line tool. It stores memories, retrieves them by fusing full-text and
//! optional vector search, and ages them with the lifecycle the hosted Kiomon product
//! uses — so a local brain and a hosted brain mean the same thing.
//!
//! # The one rule
//!
//! **This crate never prints and never exits.** It has no `println!`, no `eprintln!`,
//! no `process::exit`, and no `clap`. It returns values and [`Result`]s, and the binary
//! decides what the user sees. That rule is what makes an MCP server (v1.1) and a daemon
//! (v1.2) one `main.rs` each instead of a refactor, and it is why diagnostics travel
//! back to the caller as data rather than going straight to a terminal the library
//! cannot know exists.
//!
//! # What lives where
//!
//! The engine's modules land package by package; see
//! `plans/001-local-v1-core.md` §8 for the order.
//!
//! | Module | Owns |
//! |---|---|
//! | [`error`] | [`Error`], [`Result`], and the [`ExitCode`] each maps to |
//! | [`model`] | the domain vocabulary and the row types |
//! | [`id`] | `k_<ulid>` identifiers |
//! | [`hash`] | text normalisation and the idempotence key |
//! | [`metadata`] | the per-kind envelope, and its fail-open rule |
//! | [`db`] | opening, pragmas, migrations, integrity checks |
//! | [`store`] | add, get, update, list, forget, stats, retrieval counts |
//! | [`gate`] | which writes activate and which wait for review |
//! | [`config`] | where the database lives |
//! | [`search`] | the query ladder, the ranked window, and the record of it |
//! | [`fuse`] | reciprocal rank fusion |
//! | [`bench`] | the measurement corpus, so a number has a shape to mean |
//! | `lifecycle` / `decay` | the review inbox, feedback, and forgetting *(WP4)* |
//! | `links` | typed edges, the auto-link pass, and the one-hop read *(WP5)* |
//! | [`import`] | JSONL and markdown in and out *(WP6)* |
//! | `embed` / `vector` | the optional vector leg *(WP7)* |
//!
//! # Example
//!
//! ```
//! use kiomon_core::{Database, ListQuery, MemoryKind, NewMemory};
//!
//! let db = Database::open_in_memory()?;
//! db.add(
//!     &NewMemory::new(MemoryKind::Semantic, "Deploys need a manual migration")
//!         .body("Run migrations before the deploy; the pipeline does not do it.")
//!         .tags(vec!["deploy".to_owned()]),
//! )?;
//!
//! for memory in db.list(&ListQuery::default())? {
//!     println!("{} {} ({})", memory.id, memory.title, memory.kind_label());
//! }
//! # Ok::<(), kiomon_core::Error>(())
//! ```

#![doc(html_root_url = "https://docs.rs/kiomon-core")]

pub mod bench;
pub mod config;
pub mod db;
pub mod decay;
pub mod embed;
pub mod error;
pub mod fuse;
pub mod gate;
pub mod hash;
pub mod id;
pub mod import;
pub mod lifecycle;
pub mod links;
pub mod metadata;
pub mod model;
pub mod retrieve;
pub mod search;
pub mod store;
pub mod vector;

#[cfg(test)]
pub(crate) mod testing;

pub use bench::{BodyClass, BodyMix, CorpusReport, CorpusSpec, Probe};
pub use config::Config;
pub use db::{Database, Integrity};
pub use decay::DecayOutcome;
pub use embed::{Backend, Embedder, EmbedderConfig, Provider};
pub use error::{Error, ExitCode, Result};
pub use fuse::{RRF_K, fuse, rrf_score};
pub use gate::{AUTOPROMOTE_THRESHOLD, Verdict};
pub use import::{ExportRecord, ImportBatch, ImportOutcome, ImportRow, ImportState};
pub use lifecycle::{
    AppliedOutcome, Assessment, IdsOutcome, OutcomeReport, PendingQuery, Rating, RatingOutcome,
    ReviewOutcome, Skipped, TaskOutcome,
};
pub use links::{AUTO_LINK_BODY_BYTES, AUTO_LINK_CANDIDATES, AUTO_LINK_MIN_SCORE, AUTO_LINK_TOP_N};
pub use model::{
    AddOutcome, DEFAULT_SNIPPET_TOKENS, EdgeDirection, EmbedReport, EmbedState, ForgetOutcome,
    KindCount, Legs, LegsRun, LinkOutcome, ListQuery, ListSort, MAX_BODY_BYTES, MAX_EXCERPT,
    MAX_TAGS, MAX_TITLE, Memory, MemoryJson, MemoryKind, MemoryPatch, MemorySummaryJson, Neighbour,
    NewLink, NewMemory, Relation, ReviewMode, Scope, SearchAttempt, SearchHit, SearchHitJson,
    SearchOutcome, SearchQuery, SearchSort, Stats, Status, StatusCount, UnlinkOutcome,
    UpdateOutcome, WINDOW_OVERFETCH, Workspace, source_label,
};
pub use retrieve::{
    Applicability, ConflictNote, ContextPacket, DEFAULT_PACKET_ITEMS, EvidenceRef, Intent,
    MAX_CONSTRAINTS, MAX_ENTITIES, MAX_PACKET_ITEMS, MAX_QUERY_CHARS, PacketItem, RetrieveQuery,
    RiskLevel,
};
pub use search::{LADDER_RUNGS, Ladder};

/// The version of the CLI and JSON contract this build speaks.
///
/// It is emitted as `meta.contract_version` on every `--json` response and asserted by
/// integrations before they parse anything else, so it changes only when the shape of a
/// response does — not when a field is added.
pub const CONTRACT_VERSION: &str = "1";

/// The version of this crate, from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
