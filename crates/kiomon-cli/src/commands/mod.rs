// SPDX-License-Identifier: Apache-2.0

//! One module per command family, and the dispatch table.
//!
//! A command takes `&Cli` because `--json`, `--quiet` and `--db` are global, returns
//! [`ExitCode`], and never prints: output goes through [`crate::out`], which is the only
//! module that knows the contract.
//!
//! # Why commands return an exit code rather than `()`
//!
//! `doctor` finds problems and still has a report to print, and `forget` can be told
//! about five ids of which none existed. Both are *successful runs with an unsuccessful
//! conclusion*, and the contract has codes for them. A command that had to signal that
//! by raising an error would print a second document on the JSON path — breaking the
//! one-document rule — so the conclusion is a return value instead.

pub mod bench;
pub mod lifecycle;
pub mod portability;
pub mod read;
pub mod setup;
pub mod write;

use kiomon_core::config;
use kiomon_core::{Database, Error, ExitCode, Result, Scope};

use crate::cli::{Cli, Command};

/// Dispatch a parsed command line.
///
/// # Errors
///
/// Whatever the command raises. [`ExitCode`] is for conclusions that are not errors.
pub fn run(cli: &Cli) -> Result<ExitCode> {
    match &cli.command {
        Command::Init(args) => setup::init(cli, args),
        Command::Doctor(args) => setup::doctor(cli, args),
        Command::Stats(args) => setup::stats(cli, args),
        Command::Bench(args) => bench::bench(cli, args),

        Command::Add(args) => write::add(cli, args),
        Command::Update(args) => write::update(cli, args),
        Command::Import(args) => write::import(cli, args),
        Command::Link(args) => write::link(cli, args),
        Command::Unlink(args) => write::unlink(cli, args),
        Command::Relate(args) => write::relate(cli, args),

        Command::Search(args) => read::search(cli, args),
        Command::Retrieve(args) => read::retrieve(cli, args),
        Command::Get(args) => read::get(cli, args),
        Command::List(args) => read::list(cli, args),

        Command::Review(args) => lifecycle::review(cli, args),
        Command::Outcome(args) => lifecycle::outcome(cli, args),
        Command::Decay(args) => lifecycle::decay(cli, args),
        Command::Pin(args) => lifecycle::pin(cli, args),
        Command::Unpin(args) => lifecycle::unpin(cli, args),
        Command::Archive(args) => lifecycle::archive(cli, args),
        Command::Restore(args) => lifecycle::restore(cli, args),
        Command::Rate(args) => lifecycle::rate(cli, args),
        Command::Reindex(args) => lifecycle::reindex(cli, args),
        Command::Forget(args) => lifecycle::forget(cli, args),

        Command::Export(args) => portability::export(cli, args),
    }
}

/// What a command that its work package has not written yet returns.
///
/// The work-package number is in the message on purpose: this project is being built in
/// public against `plans/001-local-v1-core.md`, and "not implemented yet (work package
/// 4)" is a more useful thing to read than "not implemented". The variant disappears by
/// WP8.
pub(crate) fn pending(command: &'static str, wp: u8) -> Result<ExitCode> {
    Err(Error::Unimplemented { command, wp })
}

/// Refuse a command this release deliberately left out (§9-D9).
///
/// The message is different from [`pending`]'s on purpose. "Work package 5" describes a
/// package in a plan that is being executed, which a caller reasonably reads as "coming in
/// this release"; `link` and `relate` are not, and an agent that waits for them waits for
/// nothing. Whoever needs the graph is told that it is the next sprint's work, not that it
/// is on its way.
pub(crate) fn deferred(command: &'static str) -> Result<ExitCode> {
    Err(Error::Deferred { command })
}

/// Run the day's decay, if it is due, and report it on stderr.
///
/// Called at the end of `add`, and deliberately **not** at the end of `search`, which is a
/// correction to §5.4 made after measuring it. The nudge costs about 48 ms of scattered
/// index writes on a 10 000-memory corpus (see `DECAY_NUDGE_ROWS`), and the cost is not
/// proportional to the rows: it is the pages a write touches on a cold cache. Forty-eight
/// milliseconds of hidden write is a thing to bolt onto a command that was already going
/// to write, and a bad thing to bolt onto a read whose whole promise is that it answers a
/// question. WP3's second finding asked exactly this question — whether a read nothing
/// acts on should move a memory's decay clock — and the answer is no.
///
/// A failure is a warning and never an error. A write that could not round the
/// maintenance corner still has to store what it was given, and failing a memory save
/// because a *separate* background task went wrong would be the tool blaming the user for
/// its own bookkeeping.
///
/// The report goes to stderr rather than into the response: what the command was asked
/// did not change, and `--json` describes the answer to the question, not the housework
/// that happened beside it. When `serve` (WP9) exists the nudge belongs on a timer in that
/// process, where there is no user waiting for anything.
pub(crate) fn decay_if_due(db: &Database, scope: &Scope, quiet: bool) {
    match db.decay_opportunistic(scope) {
        Ok(Some(outcome)) if !outcome.is_noop() => {
            crate::out::warn(
                quiet,
                &format!(
                    "aged the brain while it was here: {} memories decayed, {} archived \
                     (`kiomon decay --dry-run` for the detail)",
                    outcome.decayed, outcome.archived
                ),
            );
        }
        Ok(_) => {}
        Err(err) => crate::out::warn(quiet, &format!("decay did not run: {err}")),
    }
}

/// Open the database this invocation should use, with the embedder the config names.
///
/// The config file is read here and nowhere else. It is one small file, and a cached copy
/// would be a second object whose lifetime has to be argued about — for a process that
/// lives for one command, reading it again costs less than the argument.
///
/// # Errors
///
/// Returns [`Error::Config`] when no path can be resolved, when the config file does not
/// parse, when its `[embedder]` block is incomplete, and whatever [`Database::open`]
/// raises.
pub(crate) fn open(cli: &Cli) -> Result<Database> {
    let config = config::Config::load(cli.config.as_deref())?;
    let embedder = config.embedder()?.map(std::sync::Arc::new);
    Ok(Database::open(config.resolve_db_path(cli.db.as_deref())?)?.with_embedder(embedder))
}

/// The workspace a command should use when it was not told one.
///
/// `$KIOMON_WORKSPACE`, then the config file's `default_workspace`, then `default` — see
/// [`config::default_workspace`] for why that order.
///
/// # Errors
///
/// Returns [`Error::Config`] when the config file does not parse. A command that cannot
/// read the config cannot know which workspace it was meant to use, and guessing is how a
/// memory lands somewhere its owner will never look.
pub(crate) fn default_workspace(cli: &Cli) -> Result<String> {
    let config = config::Config::load(cli.config.as_deref())?;
    Ok(config::default_workspace(&config))
}

/// The scope a read should cover: `'*'` for everything, otherwise exactly one workspace.
///
/// `None` means the configured default — **never** every workspace. `$KIOMON_WORKSPACE`
/// is the only way to change what absence means, and it still names one workspace.
///
/// The requested value may be an id or a name, so it is resolved here rather than passed
/// through: the store filters on `workspace_id`, and handing it a display name would
/// silently return nothing.
///
/// # Errors
///
/// Returns [`Error::NotFound`] when the named workspace does not exist.
pub(crate) fn read_scope(db: &Database, cli: &Cli, workspace: Option<&str>) -> Result<Scope> {
    match workspace.map(str::trim) {
        Some("*") => Ok(Scope::All),
        other => Ok(Scope::One(write_workspace(db, cli, other)?)),
    }
}

/// The one workspace a write should land in.
///
/// `'*'` is rejected rather than resolved: there is no sensible workspace to write to
/// when every workspace was named, and silently picking one would put a memory somewhere
/// the user did not ask for.
///
/// # Errors
///
/// Returns [`Error::Invalid`] for `'*'`, and [`Error::NotFound`] for an unknown name.
pub(crate) fn write_workspace(db: &Database, cli: &Cli, workspace: Option<&str>) -> Result<String> {
    let requested = match workspace.map(str::trim) {
        Some("*") => {
            return Err(Error::Invalid(
                "a write needs one workspace; `--workspace '*'` is for reads. Omit the flag \
                 to use the default workspace."
                    .to_owned(),
            ));
        }
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => default_workspace(cli)?,
    };
    Ok(db.resolve_workspace(&requested)?.id)
}
