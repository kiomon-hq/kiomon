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
    // `open` rather than `Database::open`, so `doctor` inspects the brain the other
    // commands use — embedder attached — and not a bare file beside it.
    let config = config::Config::load(cli.config.as_deref())?;
    let path = config.resolve_db_path(cli.db.as_deref())?;
    let db = Database::open(&path)?;
    let embedder = config.embedder()?;
    let db = db.with_embedder(embedder.map(std::sync::Arc::new));
    let schema_version = db.schema_version()?;

    if args.paths_only {
        let report = PathsReport {
            path: path.display().to_string(),
            schema_version,
            expected_schema_version: Database::target_schema_version(),
            workspace: super::default_workspace(cli)?,
            environment_db: std::env::var(config::ENV_DB).ok(),
            environment_workspace: std::env::var(config::ENV_WORKSPACE).ok(),
            config: config.source.as_ref().map_or_else(
                || "(none: defaults in use)".to_owned(),
                |path| path.display().to_string(),
            ),
        };
        out::emit(cli.json, report.clone(), |report| {
            println!("database    {}", report.path);
            println!(
                "schema      v{} this build expects v{}",
                report.schema_version, report.expected_schema_version
            );
            println!("workspace   {}", report.workspace);
            println!("config      {}", report.config);
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
    // Read before the report takes ownership of `stats`, which is the only thing that
    // knows whether a missing decay timestamp means "new brain" or "abandoned brain".
    let total_memories = stats.total;
    let last_decay = db.last_decay()?;
    let decay_age = db.decay_age_days()?;
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
        embedder: embedder_report(&db, &config)?,
        decay: DecayReport {
            // The opportunistic nudge is a hundred rows a day (see `decay.rs`), which
            // cannot keep a large corpus current. Saying so here is what makes the nudge
            // an honest one: the user finds out from `doctor` rather than from a bad
            // search result six months later.
            detail: decay_detail(decay_age, last_decay.is_none(), total_memories),
            last_run: last_decay,
            overdue_days: decay_age,
        },
    };

    out::emit(cli.json, report.clone(), render_doctor)?;

    Ok(if healthy {
        ExitCode::Success
    } else {
        ExitCode::Internal
    })
}

/// What `doctor` says about the vector leg.
///
/// # Errors
///
/// Returns whatever the coverage query raises.
fn embedder_report(db: &Database, config: &config::Config) -> Result<EmbedderReport> {
    use std::fmt::Write as _;
    let Some(configured) = db.embedder() else {
        let where_to_edit = config.source.as_ref().map_or_else(
            || "the config file (`kiomon doctor --paths-only` prints where)".to_owned(),
            |path| path.display().to_string(),
        );
        return Ok(EmbedderReport {
            configured: false,
            detail: format!(
                "not configured — search is full text only. Add an [embedder] block to \
                 {where_to_edit} for semantic search."
            ),
            provider: None,
            model: None,
            endpoint: None,
            active: None,
            embedded: None,
        });
    };
    let (active, embedded) = db.embedding_coverage(configured.model(), &Scope::All)?;
    let mut detail = format!("{} · model {}", configured.provider(), configured.model());
    if let Some(endpoint) = configured.endpoint() {
        let _ = write!(detail, " · {endpoint}");
    }
    if active > 0 {
        let _ = write!(detail, " · {embedded}/{active} memories have a vector");
        if embedded < active {
            detail.push_str(" — `kiomon reindex --embed` fills the gap");
        }
    } else {
        detail.push_str(" · nothing to embed yet");
    }
    if configured.provider() == "stub" {
        // A brain built with the stub says so, in the one place a user looks. Its vectors
        // are a hashed bag of words: two texts that share words are near, two that mean the
        // same thing are not, and calling that semantic search would be a lie this tool
        // should not tell even in a configuration it only ships for tests.
        detail.push_str(" · a stub, whose vectors are not meaningful");
    }
    Ok(EmbedderReport {
        configured: true,
        detail,
        provider: Some(configured.provider().to_owned()),
        model: Some(configured.model().to_owned()),
        endpoint: configured.endpoint(),
        active: Some(active),
        embedded: Some(embedded),
    })
}

/// The human form of a `doctor` report.
///
/// Separate from [`doctor`] because gathering and rendering are two jobs, and because the
/// report is the part a reader compares against the JSON on the line above it.
fn render_doctor(report: &DoctorReport) {
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
    println!("decay       {}", report.decay.detail);
}

/// `kiomon stats`
///
/// # Errors
///
/// Whatever opening the database or reading the counts raises.
pub fn stats(cli: &Cli, args: &StatsArgs) -> Result<ExitCode> {
    let db = open(cli)?;
    let scope = read_scope(&db, cli, args.workspace.as_deref())?;
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
    /// The config file that was read, or `(none: defaults in use)`.
    ///
    /// Printed because "the config file" is an abstraction until a user has to edit it,
    /// and the first thing they need is the path — which is a different path on every
    /// platform, and not the one they would guess on Windows.
    pub config: String,
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
    /// When decay last ran, and whether that is long enough ago to matter.
    pub decay: DecayReport,
}

/// What `doctor` can say about decay.
#[derive(Debug, Clone, Serialize)]
pub struct DecayReport {
    /// The last run, RFC 3339 UTC.
    pub last_run: Option<String>,
    /// How many days ago that was.
    pub overdue_days: Option<f64>,
    /// What a user needs to know about it.
    pub detail: String,
}

/// The sentence `doctor` prints about decay.
///
/// Thresholds rather than an exact age because the point is to answer "do I need to do
/// anything": under a week nothing has had time to weaken much, and past a month on a
/// corpus of any size the answer is yes.
fn decay_detail(age_days: Option<f64>, never: bool, memories: i64) -> String {
    match age_days {
        None if never && memories <= 1 => {
            "never run; nothing to age yet, and the first write starts the clock".to_owned()
        }
        None => "never run — `kiomon decay` is what ages a brain".to_owned(),
        Some(days) if days < 7.0 => format!("last run {days:.1} days ago; nothing to do"),
        Some(days) if days < 30.0 => format!(
            "last run {days:.1} days ago — `kiomon decay --dry-run` shows what a pass would \
             do"
        ),
        Some(days) => format!(
            "last run {days:.0} days ago — memories weaken by age, and a full pass is \
             overdue"
        ),
    }
}

/// Whether the optional vector leg can run, and how much of the corpus it can see.
///
/// The coverage pair is the point of this report. An embedder that is configured and a
/// corpus with no vectors for it is the state that looks like a working semantic search
/// and is not one — every query runs, every query returns lexical results, and nothing
/// says why. Two integers make it visible without costing the read path a count.
#[derive(Debug, Clone, Serialize)]
pub struct EmbedderReport {
    /// Whether an embedder is configured.
    pub configured: bool,
    /// What a user needs to know about it.
    pub detail: String,
    /// The provider, when one is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The model every vector is labelled with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The URL it sends to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Active memories in scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<i64>,
    /// Of those, how many have a vector for this model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedded: Option<i64>,
}
