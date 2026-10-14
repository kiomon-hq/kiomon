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
//! | [`store`] | add, get, update, list, forget, stats |
//! | [`gate`] | which writes activate and which wait for review |
//! | [`config`] | where the database lives |
//! | `search` / `fuse` | the retrieval ladder and rank fusion *(WP2)* |
//! | `lifecycle` / `decay` | the review inbox, feedback, and forgetting *(WP4)* |
//! | `links` | typed edges and the auto-link pass *(WP5)* |
//! | `import` | JSONL and markdown in and out *(WP6)* |
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

pub mod config;
pub mod db;
pub mod error;
pub mod gate;
pub mod hash;
pub mod id;
pub mod metadata;
pub mod model;
pub mod store;

#[cfg(test)]
pub(crate) mod testing;

pub use db::{Database, Integrity};
pub use error::{Error, ExitCode, Result};
pub use gate::{AUTOPROMOTE_THRESHOLD, Verdict};
pub use model::{
    AddOutcome, ForgetOutcome, KindCount, ListQuery, ListSort, MAX_TAGS, Memory, MemoryJson,
    MemoryKind, MemoryPatch, MemorySummaryJson, NewMemory, Relation, ReviewMode, Scope, Stats,
    Status, StatusCount, UpdateOutcome, Workspace, source_label,
};

/// The version of the CLI and JSON contract this build speaks.
///
/// It is emitted as `meta.contract_version` on every `--json` response and asserted by
/// integrations before they parse anything else, so it changes only when the shape of a
/// response does — not when a field is added.
pub const CONTRACT_VERSION: &str = "1";

/// The version of this crate, from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
