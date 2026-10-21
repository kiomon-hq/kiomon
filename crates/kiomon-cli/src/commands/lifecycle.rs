// SPDX-License-Identifier: Apache-2.0

//! The memory lifecycle: the review inbox, feedback, decay, and the terminal verbs.
//!
//! These are the commands that make the brain *compound* rather than merely accumulate,
//! and they have the longest memory: a strength written here decides what a search
//! surfaces a month from now. The formulas are the hosted product's, ported verbatim and
//! documented in `plans/001-local-v1-core.md` §5.3 and §5.4 — with two corrections the
//! port turned up, both recorded in the plan: ratings and outcomes move `confidence` and
//! a running `feedback_score` and *never* `strength` (decay recomputes strength from
//! `importance`, so a multiplier written here would not survive one pass), and an
//! `unused` assessment leaves an unclaimed confidence alone rather than inventing the
//! hosted 0.5.
//!
//! The three verbs that leave a memory alone when there is nothing to do — `pin`,
//! `archive`, `restore` — report what they skipped and why, and exit `2` when nothing
//! moved. That mirrors `forget`: a script that asked for five things and got none of them
//! wants to hear about it, and the alternative, exiting `0` for every no-op, is how a
//! typo'd id becomes a silent nothing.

use kiomon_core::lifecycle::{Assessment, PendingQuery, Rating, TaskOutcome};
use kiomon_core::{ExitCode, Result, Scope};

use super::{open, read_scope};
use crate::cli::{
    Cli, DecayArgs, ForgetArgs, IdsArgs, OutcomeArg, OutcomeArgs, RateArgs, RatingArg, ReindexArgs,
    ReviewArgs, ReviewCommand,
};
use crate::out;

/// What a verb that moves rows returns when nothing moved.
///
/// `2` is "the thing you named is not there", and `forget` already uses it for exactly
/// this. A no-op is not a success: the user named ids, and none of them were in a state
/// where the verb meant anything.
fn moved_or(code: ExitCode, moved: usize) -> ExitCode {
    if moved == 0 { ExitCode::NotFound } else { code }
}

/// `kiomon review`
///
/// # Errors
///
/// Returns [`kiomon_core::Error::NotFound`] for an unknown workspace.
pub fn review(cli: &Cli, args: &ReviewArgs) -> Result<ExitCode> {
    let db = open(cli)?;
    let width = if cli.full {
        usize::MAX
    } else {
        out::DEFAULT_WIDTH
    };

    match &args.command {
        ReviewCommand::List(list) => {
            let query = PendingQuery {
                scope: read_scope(&db, cli.config.as_deref(), list.workspace.as_deref())?,
                limit: list.limit,
                min_confidence: list.min_confidence,
            };
            let total = db.pending_count(&query.scope)?;
            let pending = db.list_pending(&query)?;
            let response = out::PendingResponse::new(&pending, total);
            out::emit(cli.json, response, |response| {
                if response.items.is_empty() {
                    println!("Nothing is waiting for review.");
                } else {
                    for (index, memory) in response.items.iter().enumerate() {
                        println!("{}. {}", index + 1, out::review_line(memory, width));
                    }
                    let shown = i64::try_from(response.items.len()).unwrap_or(i64::MAX);
                    if response.count > shown {
                        println!(
                            "{} more waiting; raise --limit to see them.",
                            response.count - shown
                        );
                    }
                }
            })?;
            Ok(ExitCode::Success)
        }
        ReviewCommand::Approve(ids) => {
            let outcome = db.approve(&ids.ids)?;
            for skip in &outcome.skipped {
                out::warn(cli.quiet, &out::skip_line("approve", skip));
            }
            let code = moved_or(ExitCode::Success, outcome.moved.len());
            out::emit(cli.json, outcome, |outcome| {
                println!(
                    "Approved {} draft{}; they are indexed and retrievable now.",
                    outcome.moved.len(),
                    if outcome.moved.len() == 1 { "" } else { "s" }
                );
            })?;
            Ok(code)
        }
        ReviewCommand::Reject(ids) => {
            let outcome = db.reject(&ids.ids)?;
            for skip in &outcome.skipped {
                out::warn(cli.quiet, &out::skip_line("reject", skip));
            }
            let code = moved_or(ExitCode::Success, outcome.moved.len());
            out::emit(cli.json, outcome, |outcome| {
                println!(
                    "Rejected {} draft{}; archived, not deleted — `kiomon restore` brings \
                     {} back.",
                    outcome.moved.len(),
                    if outcome.moved.len() == 1 { "" } else { "s" },
                    if outcome.moved.len() == 1 {
                        "it"
                    } else {
                        "them"
                    }
                );
            })?;
            Ok(code)
        }
    }
}

/// `kiomon outcome`
///
/// # Errors
///
/// Returns [`kiomon_core::Error::Invalid`] for more than fifty items, and
/// [`kiomon_core::Error::NotFound`] for an unknown workspace.
pub fn outcome(cli: &Cli, args: &OutcomeArgs) -> Result<ExitCode> {
    let db = open(cli)?;
    let scope = if args.workspace.is_some() {
        read_scope(&db, cli.config.as_deref(), args.workspace.as_deref())?
    } else {
        Scope::All
    };

    let items: Vec<(String, Assessment)> = args
        .items
        .iter()
        .map(|item| (item.memory_id.clone(), Assessment::from(item.assessment)))
        .collect();
    let outcome = match args.outcome {
        OutcomeArg::Success => TaskOutcome::Success,
        OutcomeArg::Failure => TaskOutcome::Failure,
    };

    let report = db.record_outcome(outcome, &items, &scope)?;
    for skip in &report.skipped {
        out::warn(cli.quiet, &out::skip_line("credit", skip));
    }
    let code = moved_or(ExitCode::Success, report.applied.len());
    out::emit(cli.json, report, |report| {
        println!("{}", out::outcome_line(report));
    })?;
    Ok(code)
}

/// `kiomon decay`
///
/// # Errors
///
/// Returns [`kiomon_core::Error::NotFound`] for an unknown workspace.
pub fn decay(cli: &Cli, args: &DecayArgs) -> Result<ExitCode> {
    let db = open(cli)?;
    let scope = if args.all_workspaces {
        Scope::All
    } else {
        read_scope(&db, cli.config.as_deref(), None)?
    };

    let outcome = db.decay(&scope, args.dry_run)?;
    let width = if cli.full {
        usize::MAX
    } else {
        out::DEFAULT_WIDTH
    };
    out::emit(cli.json, outcome, |outcome| {
        print!("{}", out::decay_table(outcome, width));
    })?;
    Ok(ExitCode::Success)
}

/// `kiomon pin`
///
/// # Errors
///
/// Returns [`kiomon_core::Error::Database`] if a statement fails.
pub fn pin(cli: &Cli, args: &IdsArgs) -> Result<ExitCode> {
    set_pinned(cli, args, true)
}

/// `kiomon unpin`
///
/// # Errors
///
/// As [`pin`].
pub fn unpin(cli: &Cli, args: &IdsArgs) -> Result<ExitCode> {
    set_pinned(cli, args, false)
}

fn set_pinned(cli: &Cli, args: &IdsArgs, pinned: bool) -> Result<ExitCode> {
    let db = open(cli)?;
    let outcome = db.set_pinned(&args.ids, pinned)?;
    let verb = if pinned { "pin" } else { "unpin" };
    for skip in &outcome.skipped {
        out::warn(cli.quiet, &out::skip_line(verb, skip));
    }
    let code = moved_or(ExitCode::Success, outcome.moved.len());
    out::emit(cli.json, outcome, |outcome| {
        println!(
            "{} {} memor{}.",
            if pinned { "Pinned" } else { "Unpinned" },
            outcome.moved.len(),
            if outcome.moved.len() == 1 { "y" } else { "ies" }
        );
        if pinned && !outcome.moved.is_empty() {
            println!("A pin is immune to decay and first in every ordering.");
        }
    })?;
    Ok(code)
}

/// `kiomon archive`
///
/// # Errors
///
/// Returns [`kiomon_core::Error::Database`] if a statement fails.
pub fn archive(cli: &Cli, args: &IdsArgs) -> Result<ExitCode> {
    let db = open(cli)?;
    let outcome = db.archive(&args.ids)?;
    for skip in &outcome.skipped {
        out::warn(cli.quiet, &out::skip_line("archive", skip));
    }
    let code = moved_or(ExitCode::Success, outcome.moved.len());
    out::emit(cli.json, outcome, |outcome| {
        println!(
            "Archived {} memor{}. They are hidden from retrieval and can be brought back \
             with `kiomon restore`.",
            outcome.moved.len(),
            if outcome.moved.len() == 1 { "y" } else { "ies" }
        );
    })?;
    Ok(code)
}

/// `kiomon restore`
///
/// # Errors
///
/// Returns [`kiomon_core::Error::Database`] if a statement fails.
pub fn restore(cli: &Cli, args: &IdsArgs) -> Result<ExitCode> {
    let db = open(cli)?;
    let outcome = db.restore(&args.ids)?;
    for skip in &outcome.skipped {
        out::warn(cli.quiet, &out::skip_line("restore", skip));
    }
    let code = moved_or(ExitCode::Success, outcome.moved.len());
    out::emit(cli.json, outcome, |outcome| {
        println!(
            "Restored {} memor{}. Their decay clock starts again from now.",
            outcome.moved.len(),
            if outcome.moved.len() == 1 { "y" } else { "ies" }
        );
    })?;
    Ok(code)
}

/// `kiomon rate`
///
/// # Errors
///
/// Returns [`kiomon_core::Error::NotFound`] when no memory has that id.
pub fn rate(cli: &Cli, args: &RateArgs) -> Result<ExitCode> {
    let db = open(cli)?;
    let rating = match args.rating {
        RatingArg::Up => Rating::Up,
        RatingArg::Down => Rating::Down,
    };
    let outcome = db.rate(&args.id, rating)?;
    out::emit(cli.json, outcome, |outcome| {
        println!("{}", out::rating_line(outcome));
    })?;
    Ok(ExitCode::Success)
}

/// `kiomon reindex`
///
/// The vector index is the one thing that can fall behind the rows it describes: it is
/// written by `add`, `update` and `review approve`, and a memory that arrived while the
/// embedder was down — or before it was configured at all — has no vector until something
/// goes back for it. This is that something, and it is manual on purpose: embedding costs
/// a model's time, and a sweep nobody asked for is a sweep that competes with the work the
/// user is actually waiting on.
///
/// # Errors
///
/// Returns [`kiomon_core::Error::Config`] when no embedder is configured, or `--embed` was
/// not passed. Returns [`kiomon_core::Error::NotFound`] for an unknown workspace.
pub fn reindex(cli: &Cli, args: &ReindexArgs) -> Result<ExitCode> {
    if !args.embed {
        return Err(kiomon_core::Error::Invalid(
            "`reindex` needs to be told what to reindex: `--embed` rebuilds the vector              index. The full-text index is maintained by triggers and never needs one."
                .to_owned(),
        ));
    }
    let db = open(cli)?;
    let scope = read_scope(&db, cli.config.as_deref(), args.workspace.as_deref())?;
    let report = db.reindex_embeddings(&scope, Some(args.limit), args.dry_run)?;

    for error in &report.errors {
        out::warn(cli.quiet, error);
    }

    let missing = if report.dry_run {
        report.embedded
    } else {
        // What is left, asked of the database rather than inferred from the counts: a
        // report computed from its own bookkeeping is a report that cannot disagree with
        // itself and therefore cannot be checked.
        let (active, embedded) = db.embedding_coverage(&report.model, &scope)?;
        usize::try_from((active - embedded).max(0)).unwrap_or(0)
    };

    out::emit(cli.json, report.clone(), |report| {
        println!("{}", out::reindex_line(report));
    })?;

    if !report.errors.is_empty() {
        return Ok(ExitCode::Internal);
    }
    if missing > 0 {
        out::warn(
            cli.quiet,
            &format!("{missing} memories still have no vector; run `kiomon reindex --embed` again"),
        );
    }
    Ok(ExitCode::Success)
}

/// `kiomon forget`
///
/// The only destructive verb. Decay archives, `review reject` archives, and everything
/// else is reversible — so this one refuses without `--yes` rather than prompting,
/// because a prompt is unusable from the scripts and agents this tool exists for.
///
/// # Errors
///
/// Returns [`kiomon_core::Error::Invalid`] without `--yes`. A missing id is *not* an error:
/// it is reported in the outcome, and the exit code is 2 only when none of the ids existed
/// — deleting five memories of which one was already gone must not undo the other four.
pub fn forget(cli: &Cli, args: &ForgetArgs) -> Result<ExitCode> {
    if !args.yes {
        return Err(kiomon_core::Error::Invalid(format!(
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
