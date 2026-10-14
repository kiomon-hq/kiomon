// SPDX-License-Identifier: Apache-2.0

//! Reading: `search`, `get`, `list`.
//!
//! `search` comes before `add` in the work plan only in the sense that WP2 finishes it
//! first; the two are written together in WP1–WP2 because a store with no retrieval is
//! not worth measuring. `list` is deliberately not a search: it is unranked, so it can
//! show a workspace without pretending an order means relevance.

use kiomon_core::Result;

use super::pending;
use crate::cli::{Cli, GetArgs, ListArgs, SearchArgs};

/// Search the brain, fusing full text with meaning when an embedder is configured.
pub fn search(_cli: &Cli, _args: &SearchArgs) -> Result<()> {
    pending("search", 2)
}

/// Fetch one memory by id, with its body, metadata and optionally its neighbours.
pub fn get(_cli: &Cli, _args: &GetArgs) -> Result<()> {
    pending("get", 1)
}

/// List memories without ranking them.
pub fn list(_cli: &Cli, _args: &ListArgs) -> Result<()> {
    pending("list", 1)
}
