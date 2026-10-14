// SPDX-License-Identifier: Apache-2.0

//! The memory lifecycle: the review inbox, feedback, decay, and the terminal verbs.
//!
//! These are the commands that make the brain *compound* rather than merely accumulate,
//! and they have the longest memory: a strength written here decides what a search
//! surfaces a month from now. The formulas are the hosted product's, ported verbatim and
//! documented in `plans/001-local-v1-core.md` §5.3 and §5.4. Do not tune one without a
//! measurement and a note in the plan — the hosted repository's own history is a standing
//! lesson that an unnamed threshold becomes a permanent argument.
//!
//! Only `forget` is implemented in WP1. It is the one verb that has to exist from the
//! start: a tool that can store a memory and cannot remove it is not one anybody should
//! put their notes in.

use kiomon_core::{Error, ExitCode, Result};

use super::{open, pending};
use crate::cli::{
    Cli, DecayArgs, ForgetArgs, IdsArgs, OutcomeArgs, RateArgs, ReviewArgs, ReviewCommand,
};
use crate::out;

/// `kiomon review`
///
/// # Errors
///
/// Always [`Error::Unimplemented`], until WP4.
pub fn review(_cli: &Cli, args: &ReviewArgs) -> Result<ExitCode> {
    match &args.command {
        ReviewCommand::List(_) => pending("review list", 4),
        ReviewCommand::Approve(_) => pending("review approve", 4),
        ReviewCommand::Reject(_) => pending("review reject", 4),
    }
}

/// `kiomon outcome`
///
/// # Errors
///
/// Always [`Error::Unimplemented`], until WP4.
pub fn outcome(_cli: &Cli, _args: &OutcomeArgs) -> Result<ExitCode> {
    pending("outcome", 4)
}

/// `kiomon decay`
///
/// # Errors
///
/// Always [`Error::Unimplemented`], until WP4.
pub fn decay(_cli: &Cli, _args: &DecayArgs) -> Result<ExitCode> {
    pending("decay", 4)
}

/// `kiomon pin`
///
/// # Errors
///
/// Always [`Error::Unimplemented`], until WP4.
pub fn pin(_cli: &Cli, _args: &IdsArgs) -> Result<ExitCode> {
    pending("pin", 4)
}

/// `kiomon unpin`
///
/// # Errors
///
/// Always [`Error::Unimplemented`], until WP4.
pub fn unpin(_cli: &Cli, _args: &IdsArgs) -> Result<ExitCode> {
    pending("unpin", 4)
}

/// `kiomon archive`
///
/// # Errors
///
/// Always [`Error::Unimplemented`], until WP4.
pub fn archive(_cli: &Cli, _args: &IdsArgs) -> Result<ExitCode> {
    pending("archive", 4)
}

/// `kiomon restore`
///
/// # Errors
///
/// Always [`Error::Unimplemented`], until WP4.
pub fn restore(_cli: &Cli, _args: &IdsArgs) -> Result<ExitCode> {
    pending("restore", 4)
}

/// `kiomon rate`
///
/// # Errors
///
/// Always [`Error::Unimplemented`], until WP4.
pub fn rate(_cli: &Cli, _args: &RateArgs) -> Result<ExitCode> {
    pending("rate", 4)
}

/// `kiomon forget`
///
/// The only destructive verb. Decay archives, `review reject` archives, and everything
/// else is reversible — so this one refuses without `--yes` rather than prompting,
/// because a prompt is unusable from the scripts and agents this tool exists for.
///
/// # Errors
///
/// Returns [`Error::Invalid`] without `--yes`. A missing id is *not* an error: it is
/// reported in the outcome, and the exit code is 2 only when none of the ids existed —
/// deleting five memories of which one was already gone must not undo the other four.
pub fn forget(cli: &Cli, args: &ForgetArgs) -> Result<ExitCode> {
    if !args.yes {
        return Err(Error::Invalid(format!(
            "`forget` deletes {} memor{} permanently and cannot be undone. Pass --yes to \
             confirm, or use `kiomon archive` to hide them reversibly.",
            args.ids.len(),
            if args.ids.len() == 1 { "y" } else { "ies" }
        )));
    }

    let db = open(cli)?;
    let outcome = db.forget(&args.ids)?;
    let code = if outcome.deleted.is_empty() {
        ExitCode::NotFound
    } else {
        ExitCode::Success
    };

    for id in &outcome.missing {
        out::warn(cli.quiet, &format!("no memory with id `{id}`"));
    }

    out::emit(cli.json, outcome, |outcome| {
        println!(
            "Deleted {} memor{}.",
            outcome.deleted.len(),
            if outcome.deleted.len() == 1 {
                "y"
            } else {
                "ies"
            }
        );
    })?;

    Ok(code)
}
