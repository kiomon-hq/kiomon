// SPDX-License-Identifier: Apache-2.0

//! The memory lifecycle: the review inbox, feedback, decay, and the terminal verbs.
//!
//! These are the commands that make the brain *compound* rather than merely
//! accumulate, and they are the ones with the longest memory: a strength written here
//! decides what a search surfaces a month from now. The formulas are the hosted
//! product's, ported verbatim, and they are documented in
//! `plans/001-local-v1-core.md` §5.3 and §5.4. Do not tune one of them without a
//! measurement and a note in the plan — the hosted repository's own history is a
//! standing lesson that an unnamed threshold becomes a permanent argument.

use kiomon_core::Result;

use super::pending;
use crate::cli::{
    Cli, DecayArgs, ForgetArgs, IdsArgs, OutcomeArgs, RateArgs, ReviewArgs, ReviewCommand,
};

/// The review inbox: what is waiting, and what you think of it.
pub fn review(cli: &Cli, args: &ReviewArgs) -> Result<()> {
    match &args.command {
        ReviewCommand::List(list) => {
            let _ = (cli, list);
            pending("review list", 4)
        }
        ReviewCommand::Approve(ids) => {
            let _ = (cli, ids);
            pending("review approve", 4)
        }
        ReviewCommand::Reject(ids) => {
            let _ = (cli, ids);
            pending("review reject", 4)
        }
    }
}

/// Report how retrieved memories performed, so credit lands where it is due.
pub fn outcome(_cli: &Cli, _args: &OutcomeArgs) -> Result<()> {
    pending("outcome", 4)
}

/// Age memories: apply decay, expire what is due, archive what has fallen below the
/// floor.
pub fn decay(_cli: &Cli, _args: &DecayArgs) -> Result<()> {
    pending("decay", 4)
}

/// Pin memories: immune to decay, and ranked first in every ordering.
pub fn pin(_cli: &Cli, _args: &IdsArgs) -> Result<()> {
    pending("pin", 4)
}

/// Remove a pin.
pub fn unpin(_cli: &Cli, _args: &IdsArgs) -> Result<()> {
    pending("unpin", 4)
}

/// Archive memories: hidden from retrieval, restorable, never purged by decay.
pub fn archive(_cli: &Cli, _args: &IdsArgs) -> Result<()> {
    pending("archive", 4)
}

/// Bring archived memories back.
pub fn restore(_cli: &Cli, _args: &IdsArgs) -> Result<()> {
    pending("restore", 4)
}

/// Say whether a memory was useful.
pub fn rate(_cli: &Cli, _args: &RateArgs) -> Result<()> {
    pending("rate", 4)
}

/// Delete memories permanently.
///
/// The only destructive verb in the tool. Decay archives, `review reject` archives,
/// and everything else is reversible — so this one refuses without `--yes` rather than
/// prompting, because a prompt is unusable from the scripts and agents this tool
/// exists for.
pub fn forget(_cli: &Cli, _args: &ForgetArgs) -> Result<()> {
    pending("forget", 1)
}
