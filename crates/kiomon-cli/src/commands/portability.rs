// SPDX-License-Identifier: Apache-2.0

//! Getting the brain out: `export`.
//!
//! `export` and `import` exist so the brain is a file you own rather than a database a
//! tool keeps. It is also the migration path: the hosted product's documents and the
//! local store share the memory vocabulary, so a JSONL export is readable by both.
//!
//! The stream is line-oriented and written as it is read, not collected into a
//! `Vec` first: a 100 MB brain must export without needing 100 MB of memory, and the
//! plan's §8 WP6 acceptance criterion asserts exactly that.

use kiomon_core::Result;

use super::pending;
use crate::cli::{Cli, ExportArgs};

/// Write the brain out as JSONL or markdown.
pub fn export(_cli: &Cli, _args: &ExportArgs) -> Result<()> {
    pending("export", 6)
}
