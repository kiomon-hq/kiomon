// SPDX-License-Identifier: Apache-2.0

//! Opening the database: the pragmas, the migrations, and the workspace resolution
//! every other module builds on.
//!
//! # The pragmas are not optional
//!
//! They are applied on **every open**, not only when the file is created. Two of them
//! (`synchronous`, `cache_size`) are per-connection and silently revert to SQLite's
//! defaults without them, and a `kiomon search` that skipped them would be several
//! times slower than the one the benchmarks measured — a difference that would show up
//! only on someone else's machine.
//!
//! # The schema version is `PRAGMA user_version`
//!
//! Not a row in `meta`. `user_version` is part of the database header and is written
//! inside the same transaction as the migration it describes, so the version and the
//! schema cannot disagree — which is exactly the failure a version row cannot rule out.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::config;
use crate::error::{Error, Result};
use crate::model::{DEFAULT_WORKSPACE, DEFAULT_WORKSPACE_NAME, ReviewMode, Workspace};

/// A migration that has shipped. `include_str!` rather than a file read at runtime:
/// the binary must not depend on its own source tree being on disk next to it.
struct Migration {
    name: &'static str,
    sql: &'static str,
}

/// Every migration, in order. Append only — a shipped file is never edited.
const MIGRATIONS: &[Migration] = &[Migration {
    name: "0001_init",
    sql: include_str!("../migrations/0001_init.sql"),
}];

/// The pragmas every connection gets.
///
/// Kept as one string so there is exactly one place to argue about them.
const PRAGMAS: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous  = NORMAL;
    PRAGMA foreign_keys = ON;
    PRAGMA busy_timeout = 5000;
    PRAGMA temp_store   = MEMORY;
    PRAGMA cache_size   = -16000;
    PRAGMA mmap_size    = 268435456;
    PRAGMA analysis_limit = 1000;
";

/// An open brain.
///
/// Holds one connection for its lifetime. Nothing in this crate opens a second one:
/// SQLite is fastest when a process keeps a connection and reuses its statement cache,
/// and the CLI's whole job is one short transaction.
pub struct Database {
    conn: Connection,
    path: PathBuf,
    in_memory: bool,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("path", &self.path)
            .field("in_memory", &self.in_memory)
            .finish_non_exhaustive()
    }
}

impl Database {
    /// Open the database at `path`, creating it and applying the schema if needed.
    ///
    /// A database that did not exist is initialised, including the default workspace —
    /// so `kiomon add` works on a machine that has never run `kiomon init`, which is
    /// the behaviour a first-time user is entitled to expect. An existing database is
    /// left alone apart from any pending migration.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the directory cannot be created, [`Error::Database`] if
    /// SQLite refuses the file, and [`Error::Config`] if it is not writable.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|err| {
                    Error::Config(format!(
                        "could not create the directory {}: {err}",
                        parent.display()
                    ))
                })?;
            }
        }

        let mut conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|err| {
            // Every branch names the path. SQLite's own message for the common failure
            // is "unable to open database file", which does not say *which* file, and
            // the path is the whole of the action a user has to take.
            match err.sqlite_error_code() {
                Some(
                    rusqlite::ErrorCode::CannotOpen
                    | rusqlite::ErrorCode::NotADatabase
                    | rusqlite::ErrorCode::PermissionDenied
                    | rusqlite::ErrorCode::ReadOnly,
                ) => Error::Config(format!(
                    "could not open {}: {err}. Pass --db with a path this process can \
                     read and write.",
                    path.display()
                )),
                _ => Error::Database(format!("could not open {}: {err}", path.display())),
            }
        })?;

        apply_pragmas(&conn)?;
        let created = migrate(&mut conn)?;
        if created {
            ensure_workspace_on(
                &conn,
                DEFAULT_WORKSPACE,
                DEFAULT_WORKSPACE_NAME,
                ReviewMode::Autonomous,
            )?;
        }

        Ok(Self {
            conn,
            path,
            in_memory: false,
        })
    }

    /// Open a fresh, empty database in memory. For tests and for `--dry-run` paths.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if SQLite refuses, which in practice means the
    /// build has no in-memory support at all.
    pub fn open_in_memory() -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        apply_pragmas(&conn)?;
        migrate(&mut conn)?;
        ensure_workspace_on(
            &conn,
            DEFAULT_WORKSPACE,
            DEFAULT_WORKSPACE_NAME,
            ReviewMode::Autonomous,
        )?;
        Ok(Self {
            conn,
            path: PathBuf::from(":memory:"),
            in_memory: true,
        })
    }

    /// Open the database the environment points at.
    ///
    /// # Errors
    ///
    /// As [`Database::open`], plus [`Error::Config`] when no path can be resolved.
    pub fn open_default() -> Result<Self> {
        Self::open(config::resolve_db_path(None)?)
    }

    /// Where this database lives.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether this database is in memory.
    #[must_use]
    pub const fn is_in_memory(&self) -> bool {
        self.in_memory
    }

    /// The schema version this file is at.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if SQLite cannot read the header.
    pub fn schema_version(&self) -> Result<u32> {
        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        // `user_version` is a 32-bit signed field that this crate only ever writes
        // with a small literal, so a value that does not fit means the file is not
        // one of ours.
        Ok(u32::try_from(version).unwrap_or(0))
    }

    /// The schema version this build expects.
    #[must_use]
    pub fn target_schema_version() -> u32 {
        // `as u32` is safe: the list is a handful of entries and a compile-time
        // constant. Written as a cast because `len()` is inherently `usize`.
        #[allow(clippy::cast_possible_truncation)]
        let version = MIGRATIONS.len() as u32;
        version
    }

    /// The names of every migration this build knows, in order.
    #[must_use]
    pub fn migrations() -> Vec<&'static str> {
        MIGRATIONS.iter().map(|migration| migration.name).collect()
    }

    /// Run SQLite's own consistency checks.
    ///
    /// Three separate checks, because they fail independently: `integrity_check` for
    /// the B-tree structures, `foreign_key_check` for the graph, and the full-text
    /// index's own `integrity-check` for the inverted index — which is the one that
    /// catches a trigger that stopped maintaining it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] only if a check could not be *run*. A check that
    /// runs and finds a problem is reported in the returned value, not as an error,
    /// because a corrupt database is still a database whose other answers are readable.
    pub fn integrity(&self) -> Result<Integrity> {
        let mut statement = self.conn.prepare("PRAGMA integrity_check")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut btree = Vec::new();
        for row in rows {
            btree.push(row?);
        }

        let mut statement = self.conn.prepare("PRAGMA foreign_key_check")?;
        let rows = statement.query_map([], |row| {
            Ok(format!(
                "{} references {}",
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?
            ))
        })?;
        let mut keys = Vec::new();
        for row in rows {
            keys.push(row?);
        }

        let fts_error = self
            .conn
            .execute(
                "INSERT INTO memory_fts(memory_fts) VALUES('integrity-check')",
                [],
            )
            .err()
            .map(|err| err.to_string());

        Ok(Integrity {
            btree,
            foreign_key_violations: keys,
            fts_error,
        })
    }

    /// Every workspace, by id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the query fails.
    pub fn workspaces(&self) -> Result<Vec<Workspace>> {
        // Ordering is by creation, and by `rowid` rather than by id as the tiebreak: the
        // timestamp has second precision, so `init --workspace x` creates two workspaces
        // inside the same second and an id tiebreak would put them in alphabetical order
        // instead of the order they were made in. `rowid` is the table's implicit
        // insertion counter and is exactly the fact wanted here.
        let mut statement = self.conn.prepare(
            "SELECT id, name, review_mode, created_at FROM workspaces \
             ORDER BY created_at, rowid",
        )?;
        let rows = statement.query_map([], row_to_workspace)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Create a workspace if it does not exist. Idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Invalid`] if `name` yields no usable id.
    pub fn create_workspace(&self, name: &str, review_mode: ReviewMode) -> Result<Workspace> {
        let id = config::workspace_id_from_name(name)?;
        ensure_workspace_on(&self.conn, &id, name.trim(), review_mode)?;
        self.workspace(&id)?.ok_or_else(|| {
            Error::Internal(format!(
                "workspace `{id}` was created but cannot be read back"
            ))
        })
    }

    /// One workspace by id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the query fails.
    pub fn workspace(&self, id: &str) -> Result<Option<Workspace>> {
        let mut statement = self
            .conn
            .prepare("SELECT id, name, review_mode, created_at FROM workspaces WHERE id = ?1")?;
        let mut rows = statement.query_map([id], row_to_workspace)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Resolve what a user typed into a workspace: an id first, then a name.
    ///
    /// The name fallback exists because `init --workspace "Work Stuff"` creates the id
    /// `work-stuff`, and a user who then types `--workspace "Work Stuff"` has made no
    /// mistake. Matching is case-insensitive on the name and exact on the id, so a
    /// workspace whose name happens to equal another's id cannot shadow it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] when nothing matches, listing what exists — a
    /// not-found on a *workspace* is nearly always a typo, and the list is the fix.
    pub fn resolve_workspace(&self, requested: &str) -> Result<Workspace> {
        let wanted = requested.trim();
        if wanted.is_empty() {
            return Err(Error::Invalid(
                "--workspace needs a name; omit the flag to use the default workspace".to_owned(),
            ));
        }
        if let Some(workspace) = self.workspace(wanted)? {
            return Ok(workspace);
        }
        let lowered = wanted.to_lowercase();
        let existing = self.workspaces()?;
        if let Some(workspace) = existing
            .iter()
            .find(|candidate| candidate.name.to_lowercase() == lowered)
        {
            return Ok(workspace.clone());
        }
        Err(Error::NotFound(format!(
            "no workspace called `{wanted}`. It has: {}",
            existing
                .iter()
                .map(|workspace| workspace.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }

    /// The connection, for the sibling modules that own their own SQL.
    pub(crate) const fn conn(&self) -> &Connection {
        &self.conn
    }
}

/// The outcome of running SQLite's consistency checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integrity {
    /// `integrity_check`'s rows. A healthy database answers with exactly `["ok"]`.
    pub btree: Vec<String>,
    /// `foreign_key_check`'s rows. A healthy database answers with nothing.
    pub foreign_key_violations: Vec<String>,
    /// The full-text index's own check. `None` means it passed.
    pub fts_error: Option<String>,
}

impl Integrity {
    /// Whether every check passed.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.btree.len() == 1
            && self.btree.first().is_some_and(|row| row == "ok")
            && self.foreign_key_violations.is_empty()
            && self.fts_error.is_none()
    }

    /// Every problem found, as lines to print.
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        for row in &self.btree {
            if row != "ok" {
                out.push(format!("integrity_check: {row}"));
            }
        }
        for row in &self.foreign_key_violations {
            out.push(format!("foreign_key_check: {row}"));
        }
        if let Some(err) = &self.fts_error {
            out.push(format!("full-text index: {err}"));
        }
        out
    }
}

fn apply_pragmas(conn: &Connection) -> Result<()> {
    conn.execute_batch(PRAGMAS)?;
    Ok(())
}

/// Apply every migration the file has not seen. Returns whether the file was created.
///
/// Each migration runs in its own transaction together with the `user_version` bump,
/// so an interrupted upgrade leaves the file at the last complete version rather than
/// half-migrated. `PRAGMA user_version` cannot take a bound parameter, hence the
/// `format!` — the value is an index into a compile-time constant list, never input.
fn migrate(conn: &mut Connection) -> Result<bool> {
    let before: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let created = before == 0;

    for (index, migration) in MIGRATIONS.iter().enumerate() {
        let version = u32::try_from(index + 1).map_err(|_| {
            Error::Internal("more migrations than the version field can hold".to_owned())
        })?;
        if i64::from(version) <= before {
            continue;
        }
        let transaction = conn.transaction()?;
        transaction.execute_batch(migration.sql).map_err(|err| {
            Error::Database(format!(
                "migration {} failed and was rolled back: {err}",
                migration.name
            ))
        })?;
        transaction.execute_batch(&format!("PRAGMA user_version = {version}"))?;
        transaction.commit()?;
    }

    Ok(created)
}

/// Insert a workspace if it is not there, leaving an existing one untouched.
fn ensure_workspace_on(
    conn: &Connection,
    id: &str,
    name: &str,
    review_mode: ReviewMode,
) -> Result<()> {
    // `INSERT OR IGNORE` rather than a read-then-write: it is one statement, and
    // "leave what is there alone" is the whole requirement. An existing workspace's
    // review mode is a user's decision, not something an open should reset.
    conn.execute(
        "INSERT OR IGNORE INTO workspaces (id, name, review_mode, created_at)
         VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))",
        rusqlite::params![id, name, review_mode.as_str()],
    )?;
    Ok(())
}

fn row_to_workspace(row: &rusqlite::Row<'_>) -> rusqlite::Result<Workspace> {
    let mode: String = row.get(2)?;
    Ok(Workspace {
        id: row.get(0)?,
        name: row.get(1)?,
        // The CHECK constraint means only the two known values can be stored, so an
        // unknown one would be a corrupted file rather than a user error. Defaulting
        // to the safer mode (everything reviewed) is better than failing a read.
        review_mode: mode.parse().unwrap_or(ReviewMode::Review),
        created_at: row.get(3)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::tempdir;

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn opening_creates_the_default_workspace() {
        let db = Database::open_in_memory().unwrap();
        let workspaces = db.workspaces().unwrap();
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].id, DEFAULT_WORKSPACE);
        assert_eq!(workspaces[0].review_mode, ReviewMode::Autonomous);
    }

    #[test]
    fn the_schema_is_at_the_version_this_build_expects() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(
            db.schema_version().unwrap(),
            Database::target_schema_version()
        );
        assert!(!Database::migrations().is_empty());
    }

    #[test]
    fn migrating_twice_changes_nothing() {
        // The applier runs on every open, so this is the property that makes a
        // `kiomon search` cheap rather than a rebuild.
        let dir = tempdir();
        let path = dir.join("brain.db");

        {
            let db = Database::open(&path).unwrap();
            assert_eq!(db.schema_version().unwrap(), 1);
            db.create_workspace("Work Stuff", ReviewMode::Review)
                .unwrap();
        }
        {
            let db = Database::open(&path).unwrap();
            assert_eq!(db.schema_version().unwrap(), 1);
            assert_eq!(db.workspaces().unwrap().len(), 2);
        }
        {
            // And a third time, because "twice" is not a proof about an idempotent
            // operation whose state is a version number.
            let db = Database::open(&path).unwrap();
            assert_eq!(db.workspaces().unwrap().len(), 2);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reopening_does_not_reset_a_review_mode() {
        let dir = tempdir();
        let path = dir.join("brain.db");
        {
            let db = Database::open(&path).unwrap();
            db.create_workspace("Reviewed", ReviewMode::Review).unwrap();
        }
        {
            let db = Database::open(&path).unwrap();
            let workspace = db.workspace("reviewed").unwrap().unwrap();
            assert_eq!(workspace.review_mode, ReviewMode::Review);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn creating_a_workspace_twice_is_not_an_error() {
        let db = Database::open_in_memory().unwrap();
        let first = db.create_workspace("Work", ReviewMode::Autonomous).unwrap();
        let second = db.create_workspace("Work", ReviewMode::Review).unwrap();
        assert_eq!(first, second, "the second call must not change the first");
        assert_eq!(db.workspaces().unwrap().len(), 2);
    }

    #[test]
    fn workspaces_come_back_in_creation_order() {
        // Both rows land in the same second, so this asserts the `rowid` tiebreak rather
        // than the timestamp: without it the order would be alphabetical, which is not a
        // fact about anything the user did.
        let db = Database::open_in_memory().unwrap();
        db.create_workspace("Zebra", ReviewMode::Autonomous)
            .unwrap();
        db.create_workspace("Apple", ReviewMode::Autonomous)
            .unwrap();
        let ids: Vec<String> = db
            .workspaces()
            .unwrap()
            .into_iter()
            .map(|workspace| workspace.id)
            .collect();
        assert_eq!(ids, vec!["default", "zebra", "apple"]);
    }

    #[test]
    fn a_workspace_resolves_by_id_then_by_name() {
        let db = Database::open_in_memory().unwrap();
        db.create_workspace("Work Stuff", ReviewMode::Autonomous)
            .unwrap();
        assert_eq!(db.resolve_workspace("work-stuff").unwrap().id, "work-stuff");
        assert_eq!(db.resolve_workspace("Work Stuff").unwrap().id, "work-stuff");
        assert_eq!(db.resolve_workspace("WORK STUFF").unwrap().id, "work-stuff");

        let err = db.resolve_workspace("nope").unwrap_err();
        assert_eq!(err.exit_code(), crate::ExitCode::NotFound);
        // The error lists what does exist, because a wrong workspace is a typo and the
        // list is the fix.
        assert!(err.to_string().contains("default"), "{err}");
    }

    #[test]
    fn an_empty_workspace_is_an_argument_error_not_a_not_found() {
        let db = Database::open_in_memory().unwrap();
        let err = db.resolve_workspace("   ").unwrap_err();
        assert_eq!(err.exit_code(), crate::ExitCode::InvalidArgs);
    }

    #[test]
    fn a_fresh_database_passes_every_integrity_check() {
        let db = Database::open_in_memory().unwrap();
        let integrity = db.integrity().unwrap();
        assert!(integrity.is_ok(), "{:?}", integrity.problems());
        assert_eq!(integrity.btree, vec!["ok".to_owned()]);
    }

    #[test]
    fn foreign_keys_are_enforced() {
        // Without `PRAGMA foreign_keys = ON` this insert would succeed and the cascade
        // in `forget` would silently do nothing.
        let db = Database::open_in_memory().unwrap();
        let result = db.conn().execute(
            "INSERT INTO memories (id, workspace_id, kind, title, created_at, updated_at)
             VALUES ('k_nope', 'no-such-workspace', 'semantic', 't', 'now', 'now')",
            [],
        );
        assert!(result.is_err(), "a dangling workspace_id must be rejected");
    }

    #[test]
    fn wal_is_enabled_and_the_cache_is_sized() {
        let dir = tempdir();
        let path = dir.join("brain.db");
        let db = Database::open(&path).unwrap();
        let journal: String = db
            .conn()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal.to_lowercase(), "wal");
        let cache: i64 = db
            .conn()
            .query_row("PRAGMA cache_size", [], |row| row.get(0))
            .unwrap();
        assert_eq!(cache, -16000);
        assert_eq!(count(db.conn(), "SELECT count(*) FROM workspaces"), 1);
        drop(db);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_fts_table_is_not_external_content() {
        // The whole design of a pending memory is "not in the index", which an external
        // content table cannot express without `integrity-check` calling it corruption.
        // Assert the design, because reverting it would be a one-line "simplification"
        // that quietly breaks the review inbox.
        let db = Database::open_in_memory().unwrap();
        let sql: String = db
            .conn()
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'memory_fts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!sql.contains("content="), "{sql}");
    }
}
