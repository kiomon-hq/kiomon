// SPDX-License-Identifier: Apache-2.0

//! Getting the brain out: `export`.
//!
//! `export` and `import` exist so the brain is a file you own rather than a database a
//! tool keeps. It is also the migration path: the hosted product's documents and the
//! local store share the memory vocabulary, so a JSONL export is readable by both.
//!
//! When it lands, the stream is line-oriented and written as it is read, never collected
//! into a `Vec` first: a large brain must export without needing its own size in memory,
//! which is what §8 WP6's acceptance criterion asserts.

use kiomon_core::{ExitCode, Result};

use super::pending;
use crate::cli::{Cli, ExportArgs};

/// `kiomon export`
///
/// # Errors
///
/// Always [`kiomon_core::Error::Unimplemented`], until WP6.
pub fn export(_cli: &Cli, _args: &ExportArgs) -> Result<ExitCode> {
    pending("export", 6)
}
