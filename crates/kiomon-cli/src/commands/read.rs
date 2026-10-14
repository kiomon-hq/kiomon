// SPDX-License-Identifier: Apache-2.0

//! Reading: `search`, `get`, `list`.
//!
//! `list` is deliberately not a search: it is unranked, it does not touch the retrieval
//! counters, and its ordering is a fact about the rows rather than an opinion about the
//! query. `search` — the ranked path, and the one the whole performance budget exists for
//! — arrives with WP2.

use kiomon_core::{ExitCode, ListQuery, ListSort, MemoryKind, MemorySummaryJson, Result, Status};

use super::{open, pending, read_scope};
use crate::cli::{Cli, GetArgs, ListArgs, SearchArgs};
use crate::out;

/// `kiomon search`
///
/// # Errors
///
/// Always [`Error::Unimplemented`], until WP2.
pub fn search(_cli: &Cli, _args: &SearchArgs) -> Result<ExitCode> {
    pending("search", 2)
}

/// `kiomon get`
///
/// # Errors
///
/// Returns [`Error::NotFound`] for an unknown id, never an empty success — an agent that
/// cannot tell "absent" from "empty" will conclude a memory is gone when it is not.
pub fn get(cli: &Cli, args: &GetArgs) -> Result<ExitCode> {
    let db = open(cli)?;
    let memory = db.get(&args.id)?;

    if args.relations {
        // Reading the graph is WP5. Answering with an empty list instead would be a lie
        // an agent cannot detect: "no relations" and "relations are not implemented" are
        // different facts, and only one of them is true.
        return pending("get --relations", 5);
    }

    let width = if cli.full {
        usize::MAX
    } else {
        out::DEFAULT_WIDTH
    };
    out::emit(cli.json, memory, |memory| {
        println!("{}", out::render_memory(memory, width));
    })?;
    Ok(ExitCode::Success)
}

/// `kiomon list`
///
/// # Errors
///
/// Returns [`Error::NotFound`] for an unknown workspace, and whatever the query raises.
pub fn list(cli: &Cli, args: &ListArgs) -> Result<ExitCode> {
    let db = open(cli)?;
    let scope = read_scope(&db, args.workspace.as_deref())?;

    // Absent `--status` means `active`, never "everything": a bare `list` must not mix a
    // user's review inbox into their library. `--status any` is how you ask for both.
    let status = match args.status {
        None => Some(Status::Active),
        Some(arg) => Option::<Status>::from(arg),
    };

    let query = ListQuery {
        scope,
        kinds: args.kind.iter().copied().map(MemoryKind::from).collect(),
        status,
        tags: args.tag.clone(),
        limit: u32::from(args.limit),
        sort: if args.recent {
            ListSort::Recent
        } else {
            ListSort::Strength
        },
    };

    let rows = db.list(&query)?;
    let width = if cli.full {
        usize::MAX
    } else {
        out::DEFAULT_WIDTH
    };

    if cli.json {
        let summaries: Vec<MemorySummaryJson<'_>> =
            rows.iter().map(MemorySummaryJson::from).collect();
        out::emit(true, summaries, |_| {})?;
    } else if rows.is_empty() {
        // Said on stderr, not stdout: an empty result is a successful answer, and a
        // caller piping stdout into a parser must not receive a sentence.
        out::warn(cli.quiet, "nothing matched.");
    } else {
        let summaries: Vec<MemorySummaryJson<'_>> =
            rows.iter().map(MemorySummaryJson::from).collect();
        out::emit(false, summaries, |summaries| {
            for summary in summaries {
                println!("{}", out::summary_line(summary, width));
            }
        })?;
    }

    Ok(ExitCode::Success)
}
