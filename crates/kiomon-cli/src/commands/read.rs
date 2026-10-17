// SPDX-License-Identifier: Apache-2.0

//! Reading: `search`, `retrieve`, `get`, `list`.
//!
//! `list` is deliberately not a search: it is unranked, it does not touch the retrieval
//! counters, and its ordering is a fact about the rows rather than an opinion about the
//! query. `search` is the ranked path, and the one the whole performance budget exists
//! for. `retrieve` is `search` plus a decision: it asks the same index, then re-ranks and
//! buckets the answer by what the caller said it was about to do.

use kiomon_core::{
    ExitCode, ListQuery, ListSort, MemoryKind, MemorySummaryJson, Result, RetrieveQuery,
    SearchAttempt, SearchHitJson, SearchQuery, Status,
};

use super::{deferred, open, read_scope};
use crate::cli::{Cli, GetArgs, ListArgs, RetrieveArgs, SearchArgs};
use crate::out;

/// `kiomon search`
///
/// # Errors
///
/// Returns [`kiomon_core::Error::Config`] for `--legs vector` with no embedder configured,
/// for an embedder that does not answer, and [`kiomon_core::Error::NotFound`] for an
/// unknown workspace.
pub fn search(cli: &Cli, args: &SearchArgs) -> Result<ExitCode> {
    let db = open(cli)?;
    let scope = read_scope(&db, cli, args.workspace.as_deref())?;

    // The embedder is asked for one query vector, and it is the only call on this path
    // that can take longer than the whole rest of the command. `--legs fts` exists for
    // exactly this reason, and the config decides whether `auto` involves it at all.
    let outcome = db.search(&SearchQuery {
        text: args.query.clone(),
        scope,
        kinds: args.kind.iter().copied().map(MemoryKind::from).collect(),
        limit: u32::from(args.limit),
        snippet_tokens: args.snippet_len,
        sort: args.sort.into(),
        legs: args.legs.into(),
        record: !args.no_record,
    })?;

    let width = if cli.full {
        usize::MAX
    } else {
        out::DEFAULT_WIDTH
    };

    // The three ways a search can come back with nothing are three different facts, and
    // an agent's next move differs for each, so they are three different warnings. None of
    // them is an error: exit 0 with no results is the right answer to "is there anything
    // about X", and a non-zero code would teach an agent to retry a question that was
    // already answered.
    if outcome.hits.is_empty() {
        match &outcome.attempt {
            SearchAttempt::NothingAsked => {
                out::warn(cli.quiet, "the query was empty, so nothing was searched.");
            }
            SearchAttempt::Exhausted { query } => out::warn(
                cli.quiet,
                &format!("nothing matched, on any of the three query forms. The last was {query}"),
            ),
            // Unreachable: a rung that matched returns at least one row. A leg that
            // returned rows and a table read that could not find them would do it, which
            // is what the check in `search` is for.
            SearchAttempt::Matched { .. } => {}
        }
    }

    // A degraded answer is reported on stderr, before the results, and never folded into
    // them: the rows on stdout are what the other leg found, and a reader piping them into
    // `jq` must not have to parse prose to find out that half the search did not run.
    if outcome.degraded {
        out::warn(
            cli.quiet,
            &format!(
                "the vector leg did not run ({}), so these results are full text only.                  `kiomon doctor` checks the embedder.",
                outcome.reason
            ),
        );
    } else if outcome.reason == "no_embedder" && args.legs.as_str() != "fts" {
        // Not a degradation — nothing failed — but "why is my search not semantic" has
        // exactly one answer, and it is cheaper to say it than to be asked.
        out::warn(
            cli.quiet,
            "no embedder is configured, so this search was full text only. See the              [embedder] block in the config file (`kiomon doctor` prints its path).",
        );
    }

    if cli.json {
        let response = out::SearchResponse::new(&outcome, &args.query, args.legs.as_str());
        out::emit(true, response, |_| {})?;
    } else {
        let hits: Vec<SearchHitJson<'_>> = outcome.hits.iter().map(SearchHitJson::from).collect();
        for (index, hit) in hits.iter().enumerate() {
            // `index + 1` because a person counting results starts at one, and because
            // the MCP handler numbers them the same way — the two are meant to read alike.
            println!("{}", out::search_line(index + 1, hit, width));
        }
    }

    Ok(ExitCode::Success)
}

/// `kiomon retrieve`
///
/// # Errors
///
/// [`kiomon_core::Error::Invalid`] for a query or constraint list the hosted route would
/// reject, [`kiomon_core::Error::NotFound`] for an unknown workspace when one is named, and
/// whatever the candidate search raises.
pub fn retrieve(cli: &Cli, args: &RetrieveArgs) -> Result<ExitCode> {
    let db = open(cli)?;
    let scope = read_scope(&db, cli, args.workspace.as_deref())?;

    let packet = db.retrieve(&RetrieveQuery {
        intent: args.intent.into(),
        text: args.query.clone(),
        scope,
        kinds: args.kind.iter().copied().map(MemoryKind::from).collect(),
        constraints: args.constraint.clone(),
        limit: usize::from(args.limit),
        record: !args.no_record,
        legs: args.legs.into(),
        ..RetrieveQuery::default()
    })?;

    let width = if cli.full {
        usize::MAX
    } else {
        out::DEFAULT_WIDTH
    };
    out::emit(cli.json, packet, |packet| {
        println!("{}", out::render_packet(packet, width));
    })?;
    Ok(ExitCode::Success)
}

/// `kiomon get`
///
/// # Errors
///
/// Returns [`Error::NotFound`] for an unknown id, never an empty success — an agent that
/// cannot tell "absent" from "empty" will conclude a memory is gone when it is not.
pub fn get(cli: &Cli, args: &GetArgs) -> Result<ExitCode> {
    let db = open(cli)?;

    // Refused before the row is looked up, so that a flag this build cannot honour behaves
    // the same way whatever the id is. Checking the id first would answer "no such memory"
    // for a typo and "not implemented" for a real one, and a caller that learns different
    // things about the same flag depending on its data cannot write correct error handling.
    // It is also the cheaper order, and the answer is a lie either way: an empty relation
    // list would claim a memory has no neighbours when nobody can ask (plan §9-D9).
    if args.relations {
        return deferred("get --relations");
    }

    let memory = db.get(&args.id)?;

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
    let scope = read_scope(&db, cli, args.workspace.as_deref())?;

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
