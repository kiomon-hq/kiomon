// SPDX-License-Identifier: Apache-2.0

//! Setup and health: `init`, `doctor`, `stats`.
//!
//! `doctor` is the most important command in the tool and the least exciting. It is the
//! answer to "why is my brain doing that", and it answers it from the three checks that
//! actually fail independently: SQLite's B-tree integrity, the foreign keys, and the
//! full-text index — which is the one that catches a trigger that stopped maintaining
//! it, and the reason `memory_fts` is not an external-content table.

use serde::Serialize;

use kiomon_core::config;
use kiomon_core::{Database, ExitCode, MemoryKind, Result, Scope, Stats, Workspace};

use super::{open, read_scope};
use crate::cli::{Cli, DoctorArgs, InitArgs, StatsArgs};
use crate::out;

/// `kiomon init`
///
/// # Errors
///
/// Whatever opening or creating a workspace raises.
pub fn init(cli: &Cli, args: &InitArgs) -> Result<ExitCode> {
    let path = config::resolve_db_path(cli.db.as_deref())?;
    let existed = path.exists();
    let db = Database::open(&path)?;

    // Creating a workspace is idempotent, so this is safe to re-run and safe to run on
    // the database the open above just created.
    for name in &args.workspace {
        db.create_workspace(name, kiomon_core::ReviewMode::Autonomous)?;
    }

    let report = InitReport {
        path: path.display().to_string(),
        created: !existed,
        schema_version: db.schema_version()?,
        workspaces: db.workspaces()?,
    };

    out::emit(cli.json, report, |report| {
        if report.created {
            println!("Created {}", report.path);
        } else {
            println!("Using {}", report.path);
        }
        println!(
            "Schema v{}, workspaces: {}",
            report.schema_version,
            names(&report.workspaces).join(", ")
        );
        if !report.created {
            println!("\nNext: kiomon add --kind semantic --title \"…\" --body \"…\"");
        }
    })?;
    Ok(ExitCode::Success)
}

/// `kiomon doctor`
///
/// # Errors
///
/// Whatever opening the database raises. Problems *found* are a report and an exit code
/// of 1, not an error — a corrupt database is still a database whose other answers are
/// readable, and a caller has more use for the report than for a message.
pub fn doctor(cli: &Cli, args: &DoctorArgs) -> Result<ExitCode> {
    let path = config::resolve_db_path(cli.db.as_deref())?;
    let db = Database::open(&path)?;
    let schema_version = db.schema_version()?;

    if args.paths_only {
        let report = PathsReport {
            path: path.display().to_string(),
            schema_version,
            expected_schema_version: Database::target_schema_version(),
            workspace: config::default_workspace(),
            environment_db: std::env::var(config::ENV_DB).ok(),
            environment_workspace: std::env::var(config::ENV_WORKSPACE).ok(),
        };
        out::emit(cli.json, report.clone(), |report| {
            println!("database    {}", report.path);
            println!(
                "schema      v{} this build expects v{}",
                report.schema_version, report.expected_schema_version
            );
            println!("workspace   {}", report.workspace);
            println!(
                "env         {}={}  {}={}",
                config::ENV_DB,
                report.environment_db.as_deref().unwrap_or("(unset)"),
                config::ENV_WORKSPACE,
                report.environment_workspace.as_deref().unwrap_or("(unset)")
            );
        })?;
        return Ok(ExitCode::Success);
    }

    let integrity = db.integrity()?;
    let stats = db.stats(&Scope::All, &[])?;
    let indexed = db.indexed_count()?;
    let workspaces = db.workspaces()?;
    let problems = integrity.problems();

    // The index should hold exactly the active rows. It cannot hold more, since it has no
    // other source, so the interesting direction is a shortfall — and a shortfall in the
    // other direction (a draft that leaked in) would show as a surplus.
    let active = stats
        .by_status
        .iter()
        .find(|entry| entry.status == "active")
        .map_or(0, |entry| entry.count);
    let index_mismatch = if indexed == active {
        None
    } else {
        Some(format!(
            "{indexed} indexed rows against {active} active memories"
        ))
    };

    let healthy = problems.is_empty() && index_mismatch.is_none();
    let report = DoctorReport {
        path: path.display().to_string(),
        schema_version,
        expected_schema_version: Database::target_schema_version(),
        healthy,
        problems,
        index_mismatch,
        indexed,
        active,
        workspaces: names(&workspaces),
        stats,
        // The embedder arrives with WP7. Reported as absent rather than omitted, because
        // "the vector leg is off" is a thing a user should be able to find out.
        embedder: EmbedderReport {
            configured: false,
            detail: "not configured (the vector leg arrives in work package 7)".to_owned(),
        },
    };

    out::emit(cli.json, report.clone(), |report| {
        println!("database    {}", report.path);
        println!(
            "schema      v{} this build expects v{}",
            report.schema_version, report.expected_schema_version
        );
        if report.problems.is_empty() {
            println!("integrity   ok");
        } else {
            println!("integrity   {} problem(s)", report.problems.len());
            for problem in &report.problems {
                println!("  {problem}");
            }
        }
        if let Some(mismatch) = &report.index_mismatch {
            println!("index       {mismatch}");
        } else {
            println!(
                "index       {} rows, matching the active memories",
                report.indexed
            );
        }
        println!("workspaces  {}", report.workspaces.join(", "));
        println!(
            "memories    {} total, {} indexed as active",
            report.stats.total, report.indexed
        );
        println!("embedder    {}", report.embedder.detail);
    })?;

    Ok(if healthy {
        ExitCode::Success
    } else {
        ExitCode::Internal
    })
}

/// `kiomon stats`
///
/// # Errors
///
/// Whatever opening the database or reading the counts raises.
pub fn stats(cli: &Cli, args: &StatsArgs) -> Result<ExitCode> {
    let db = open(cli)?;
    let scope = read_scope(&db, args.workspace.as_deref())?;
    let kinds: Vec<MemoryKind> = args.kind.iter().copied().map(MemoryKind::from).collect();
    let stats = db.stats(&scope, &kinds)?;

    let label = match &scope {
        Scope::All => "every workspace".to_owned(),
        Scope::One(id) => id.clone(),
    };

    out::emit(cli.json, stats, |stats| {
        println!("{} memories in {label}", stats.total);
        for entry in &stats.by_kind {
            println!("  {:<9} {:>8}", entry.kind_label, entry.count);
        }
        let statuses: Vec<String> = stats
            .by_status
            .iter()
            .map(|entry| format!("{} {}", entry.status, entry.count))
            .collect();
        println!("  {}", statuses.join(" · "));
        println!("  pinned {}", stats.pinned);
    })?;
    Ok(ExitCode::Success)
}

fn names(workspaces: &[Workspace]) -> Vec<String> {
    workspaces
        .iter()
        .map(|workspace| workspace.id.clone())
        .collect()
}

/// What `init` reports.
#[derive(Debug, Clone, Serialize)]
pub struct InitReport {
    /// Where the database lives.
    pub path: String,
    /// Whether this invocation created it.
    pub created: bool,
    /// The schema version it is at now.
    pub schema_version: u32,
    /// The workspaces it has.
    pub workspaces: Vec<Workspace>,
}

/// What `doctor --paths-only` reports.
#[derive(Debug, Clone, Serialize)]
pub struct PathsReport {
    /// Where the database lives.
    pub path: String,
    /// The schema version on disk.
    pub schema_version: u32,
    /// The schema version this build expects.
    pub expected_schema_version: u32,
    /// The workspace a command without `--workspace` will use.
    pub workspace: String,
    /// `$KIOMON_DB`, when it is set.
    pub environment_db: Option<String>,
    /// `$KIOMON_WORKSPACE`, when it is set.
    pub environment_workspace: Option<String>,
}

/// What `doctor` reports.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    /// Where the database lives.
    pub path: String,
    /// The schema version on disk.
    pub schema_version: u32,
    /// The schema version this build expects.
    pub expected_schema_version: u32,
    /// Whether every check passed. The exit code agrees with this field.
    pub healthy: bool,
    /// Everything SQLite's checks found, as lines.
    pub problems: Vec<String>,
    /// The full-text index disagreeing with the active rows, if it does.
    pub index_mismatch: Option<String>,
    /// Rows in the full-text index.
    pub indexed: i64,
    /// Active memories.
    pub active: i64,
    /// The workspace ids that exist.
    pub workspaces: Vec<String>,
    /// The counts.
    pub stats: Stats,
    /// Whether a vector leg is available.
    pub embedder: EmbedderReport,
}

/// Whether the optional vector leg can run.
#[derive(Debug, Clone, Serialize)]
pub struct EmbedderReport {
    /// Whether an embedder is configured.
    pub configured: bool,
    /// What a user needs to know about it.
    pub detail: String,
}
