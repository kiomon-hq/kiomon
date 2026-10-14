// SPDX-License-Identifier: Apache-2.0

//! One module per command family, and the dispatch table.
//!
//! Every command in `plans/001-local-v1-core.md` §6.1 is declared from WP0 and
//! returns [`kiomon_core::Error::Unimplemented`] until its work package lands. That
//! is deliberate: the parser, the help text and the contract can be reviewed as a
//! whole once, and finishing a package is then a change of one function body rather
//! than a change to the surface.
//!
//! A command's function takes `&Cli` because `--json`, `--quiet` and `--db` are
//! global. It returns [`Result`] and never prints: output goes through
//! [`crate::out`], which is the only module that knows the contract.

pub mod lifecycle;
pub mod portability;
pub mod read;
pub mod setup;
pub mod write;

use kiomon_core::{Error, Result};

use crate::cli::{Cli, Command};

/// Dispatch a parsed command line.
pub fn run(cli: &Cli) -> Result<()> {
    match &cli.command {
        Command::Init(args) => setup::init(cli, args),
        Command::Doctor(args) => setup::doctor(cli, args),
        Command::Stats(args) => setup::stats(cli, args),

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

/// What an unimplemented command returns.
///
/// The work-package number is in the message on purpose: this project is being built
/// in public against `plans/001-local-v1-core.md`, and "not implemented yet (work
/// package 4)" is a more useful thing for a user to read than "not implemented". The
/// whole variant disappears by WP8.
pub(crate) fn pending<T>(command: &'static str, wp: u8) -> Result<T> {
    Err(Error::Unimplemented { command, wp })
}
