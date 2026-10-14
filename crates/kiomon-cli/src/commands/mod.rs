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

/// Open the database this invocation should use.
///
/// # Errors
///
/// Returns [`Error::Config`] when no path can be resolved, and whatever
/// [`Database::open`] raises.
pub(crate) fn open(cli: &Cli) -> Result<Database> {
    Database::open(config::resolve_db_path(cli.db.as_deref())?)
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
pub(crate) fn read_scope(db: &Database, workspace: Option<&str>) -> Result<Scope> {
    match workspace.map(str::trim) {
        Some("*") => Ok(Scope::All),
        other => Ok(Scope::One(write_workspace(db, other)?)),
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
pub(crate) fn write_workspace(db: &Database, workspace: Option<&str>) -> Result<String> {
    let requested = match workspace.map(str::trim) {
        Some("*") => {
            return Err(Error::Invalid(
                "a write needs one workspace; `--workspace '*'` is for reads. Omit the flag \
                 to use the default workspace."
                    .to_owned(),
            ));
        }
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => config::default_workspace(),
    };
    Ok(db.resolve_workspace(&requested)?.id)
}
