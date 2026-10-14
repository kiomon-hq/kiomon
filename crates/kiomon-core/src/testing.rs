// SPDX-License-Identifier: Apache-2.0

//! Fixtures shared by the crate's own tests.
//!
//! Compiled only for `cargo test`. It is a module rather than a dev-dependency because
//! the fixtures need the private surface — a test that builds a memory through the
//! public API can only ever test the happy path the public API already allows.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::db::Database;
use crate::error::Result;
use crate::model::{MemoryKind, NewMemory};

/// A directory that a test can write a real database into, and that is unique per call.
///
/// `:memory:` cannot be used for anything that reopens the file — WAL, migrations, and
/// the whole point of `Database::open` — so those tests need a real path. The name
/// carries the process id so two test binaries in the same run cannot collide, and a
/// counter so two tests in the same binary cannot either.
#[must_use]
pub(crate) fn tempdir() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("kiomon-test-{}-{unique}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the test directory must be creatable");
    dir
}

/// A database in memory, with the default workspace already in it.
#[must_use]
pub(crate) fn db() -> Database {
    Database::open_in_memory().expect("an in-memory database must open")
}

/// Add a memory, expecting success. Returns its id.
///
/// # Panics
///
/// Panics if the write fails, which in a test is the assertion.
pub(crate) fn add(db: &Database, kind: MemoryKind, title: &str, body: &str) -> String {
    db.add(&NewMemory::new(kind, title).body(body))
        .expect("the fixture write must succeed")
        .id
}

/// Add a memory and return its full record.
///
/// # Panics
///
/// As [`add`].
pub(crate) fn add_and_get(
    db: &Database,
    kind: MemoryKind,
    title: &str,
    body: &str,
) -> crate::model::Memory {
    let id = add(db, kind, title, body);
    db.get(&id).expect("the fixture memory must be readable")
}

/// The number of rows in the full-text index.
///
/// Counts the index itself, not a scan of `memories`: the whole point of several tests
/// is that the two differ while a draft is pending.
///
/// # Errors
///
/// Returns the database error if the count cannot be read.
pub(crate) fn fts_rows(db: &Database) -> Result<i64> {
    db.indexed_count()
}
