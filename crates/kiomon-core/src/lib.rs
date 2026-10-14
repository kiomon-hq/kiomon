// SPDX-License-Identifier: Apache-2.0

//! A fast, local, SQLite-backed memory for coding agents.
//!
//! `kiomon-core` is the engine behind the [`kiomon`](https://github.com/kiomon-hq/kiomon)
//! command-line tool. It stores memories, retrieves them by fusing full-text and
//! optional vector search, and ages them with the lifecycle the hosted Kiomon
//! product uses — so a local brain and a hosted brain mean the same thing.
//!
//! # The one rule
//!
//! **This crate never prints and never exits.** It has no `println!`, no `eprintln!`,
//! no `process::exit`, and no `clap`. It returns values and [`Result`]s, and the
//! binary decides what the user sees. That rule is what makes an MCP server (v1.1)
//! and a daemon (v1.2) one `main.rs` each instead of a refactor, and it is why
//! diagnostics travel back to the caller as data rather than going straight to a
//! terminal the library cannot know exists.
//!
//! # What lives where
//!
//! The engine's modules land package by package; see
//! `plans/001-local-v1-core.md` §8 for the order.
//!
//! | Module | Owns |
//! |---|---|
//! | [`error`] | [`Error`], [`Result`], and the [`ExitCode`] each maps to |
//! | [`model`] | the domain vocabulary: kinds, statuses, relations, labels |
//! | `db` | opening, PRAGMAs, migrations, transactions *(WP1)* |
//! | `store` | add, get, update, list, purge *(WP1)* |
//! | `search` / `fuse` | the retrieval ladder and rank fusion *(WP2)* |
//! | `gate` / `lifecycle` / `decay` | the write gate, review inbox, and forgetting *(WP4)* |
//! | `links` | typed edges and the auto-link pass *(WP5)* |
//! | `import` | JSONL and markdown in and out *(WP6)* |
//! | `embed` / `vector` | the optional vector leg *(WP7)* |
//!
//! # Example
//!
//! ```rust,ignore
//! // WP1 lands `Database`, `NewMemory` and `search`, and removes the `ignore`.
//! use kiomon_core::{Database, MemoryKind, NewMemory};
//!
//! let db = Database::open_default()?;
//! let saved = db.add(
//!     NewMemory::new(MemoryKind::Semantic, "Deploys need a manual migration")
//!         .body("Run migrations before the deploy; the pipeline does not do it."),
//! )?;
//! for hit in db.search("why did the deploy fail")? {
//!     println!("{} {}", hit.id, hit.title);
//! }
//! # Ok::<(), kiomon_core::Error>(())
//! ```

#![doc(html_root_url = "https://docs.rs/kiomon-core")]

pub mod error;
pub mod model;

pub use error::{Error, ExitCode, Result};
pub use model::{MemoryKind, Relation, Status, source_label};

/// The version of the CLI and JSON contract this build speaks.
///
/// It is emitted as `meta.contract_version` on every `--json` response and asserted
/// by integrations before they parse anything else, so it changes only when the
/// shape of a response does — not when a field is added.
pub const CONTRACT_VERSION: &str = "1";

/// The version of this crate, from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
