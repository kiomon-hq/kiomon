// SPDX-License-Identifier: Apache-2.0

//! Everything that writes to a stream, and the only module that decides an exit code.
//!
//! Three rules, all asserted in `tests/cli.rs`:
//!
//! - **stdout carries the answer and nothing else.** Progress, warnings and diagnostics
//!   go to stderr, so `kiomon search … | jq` is always safe. This is why the library
//!   never prints: the decision about which stream a message belongs on is made here,
//!   once, where the contract is.
//! - **A parse failure exits 3, not clap's 2.** The contract spends 2 on "not found"
//!   (see [`kiomon_core::ExitCode`]), and letting clap's own default through would make
//!   an unknown flag indistinguishable from a missing memory.
//! - **Human output is bounded.** A four-kilobyte body must not become a four-kilobyte
//!   line in an agent's context window, so every field is truncated to
//!   [`DEFAULT_WIDTH`] characters unless `--full` was asked for.

use serde::Serialize;

use kiomon_core::{
    Error, ExitCode, Memory, MemorySummaryJson, Result, SearchAttempt, SearchHitJson, SearchOutcome,
};

/// How wide a field may be on the human path before it is truncated.
pub const DEFAULT_WIDTH: usize = 120;

/// The `meta` block on every `--json` response.
///
/// An integration reads [`Meta::contract_version`] before anything else, so it can
/// refuse a shape it does not understand rather than mis-parse one it does.
#[derive(Debug, Serialize)]
pub struct Meta {
    /// The JSON contract version — `kiomon_core::CONTRACT_VERSION`.
    pub contract_version: &'static str,
    /// The version of this build.
    pub version: &'static str,
}

impl Meta {
    /// The `meta` block for this build.
    #[must_use]
    pub const fn current() -> Self {
        Self {
            contract_version: kiomon_core::CONTRACT_VERSION,
            version: kiomon_core::VERSION,
        }
    }
}

/// The success envelope: `{"ok":true,"data":…,"meta":…}`.
#[derive(Debug, Serialize)]
pub struct Success<T> {
    /// Always `true`. Present so a caller can branch on one field.
    pub ok: bool,
    /// The command's result.
    pub data: T,
    /// Version information.
    pub meta: Meta,
}

impl<T> Success<T> {
    /// Wrap a value in the success envelope.
    #[must_use]
    pub const fn new(data: T) -> Self {
        Self {
            ok: true,
            data,
            meta: Meta::current(),
        }
    }
}

/// The failure envelope: `{"ok":false,"error":{…},"meta":…}`.
#[derive(Debug, Serialize)]
struct Failure {
    ok: bool,
    error: FailureBody,
    meta: Meta,
}

#[derive(Debug, Serialize)]
struct FailureBody {
    code: &'static str,
    message: String,
}

/// Report a `clap` parse failure on the right stream, and return its exit code.
///
/// `--help` and `--version` are clap's two *successful* exits, and they are the cases
/// where it writes to stdout. Everything else is the user having got something wrong,
/// which the contract puts at 3.
pub fn parse_failure(err: &clap::Error) -> std::process::ExitCode {
    let code = if err.use_stderr() {
        ExitCode::InvalidArgs
    } else {
        ExitCode::Success
    };
    // `print()` picks the stream itself — stdout for help and version, stderr for
    // everything else — which is the split this function's exit code encodes.
    let _ = err.print();
    code.into()
}

/// Print a failure and return its exit code.
///
/// `--quiet` never suppresses an error: the flag means "stop telling me things I already
/// know", and a failure is not one of those.
pub fn report(json: bool, err: &Error) -> std::process::ExitCode {
    if json {
        let body = Failure {
            ok: false,
            error: FailureBody {
                code: err.code(),
                message: err.to_string(),
            },
            meta: Meta::current(),
        };
        match serde_json::to_string(&body) {
            Ok(line) => println!("{line}"),
            // Unreachable in practice, and worth handling anyway: falling back to the
            // human form still tells the user what went wrong, whereas a panic here
            // would hide the original error behind a serialization one.
            Err(_) => eprintln!("kiomon: {err}"),
        }
    } else {
        eprintln!("kiomon: {err}");
    }
    err.exit_code().into()
}

/// Emit a successful result: JSON when asked, otherwise through `human`.
///
/// # Errors
///
/// Returns [`Error::Internal`] if `data` cannot be serialized. That is a bug in the
/// response type rather than a user error, so it is reported as one.
pub fn emit<T, F>(json: bool, data: T, human: F) -> Result<()>
where
    T: Serialize,
    F: FnOnce(&T),
{
    if json {
        let line = serde_json::to_string(&Success::new(data)).map_err(|err| {
            Error::Internal(format!("could not encode the response as JSON: {err}"))
        })?;
        println!("{line}");
    } else {
        human(&data);
    }
    Ok(())
}

/// The `data` block of a `search` response.
///
/// An object rather than the bare array `list` returns, and the asymmetry is the point:
/// a search has opinions about how it answered — which rung of the ladder, which legs,
/// whether it recorded the read — and a caller that cannot see them cannot tell a query
/// that found nothing from a query that was never asked.
#[derive(Debug, Serialize)]
pub struct SearchResponse<'a> {
    /// The page, in the order it is meant to be read.
    pub results: Vec<SearchHitJson<'a>>,
    /// The query as it was given, before any of it became an FTS5 expression. Echoed so a
    /// caller parsing a batch of responses does not have to pair them up itself.
    pub query: &'a str,
    /// Which rung answered, or why none did.
    pub attempt: &'a SearchAttempt,
    /// What `--legs` asked for.
    pub requested_legs: &'static str,
    /// What actually ran.
    pub legs: &'static str,
    /// See [`SearchOutcome::degraded`].
    pub degraded: bool,
    /// How many rows had their retrieval counter moved.
    pub recorded: usize,
}

impl<'a> SearchResponse<'a> {
    /// Shape a search outcome for the wire.
    #[must_use]
    pub fn new(outcome: &'a SearchOutcome, query: &'a str, requested_legs: &'static str) -> Self {
        Self {
            results: outcome.hits.iter().map(SearchHitJson::from).collect(),
            query,
            attempt: &outcome.attempt,
            requested_legs,
            legs: outcome.legs,
            degraded: outcome.degraded,
            recorded: outcome.recorded,
        }
    }
}

/// A warning for the user, on stderr. Suppressed by `--quiet`, because it is not a
/// failure — it is something the command did differently from what was asked.
pub fn warn(quiet: bool, message: &str) {
    if !quiet {
        eprintln!("kiomon: {message}");
    }
}

/// Cut a string to `width` characters, marking that it was cut.
///
/// Counts characters rather than bytes, because slicing a UTF-8 string at a byte offset
/// panics and an agent writing about a Turkish city should not be able to crash `list`.
#[must_use]
pub fn truncate(text: &str, width: usize) -> String {
    if width == 0 || text.chars().count() <= width {
        return text.to_owned();
    }
    let head: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{head}…")
}

/// One memory, one line, for `list` and (later) `search`.
///
/// The line leads with the id because an agent's next move is almost always
/// `kiomon get <id>`, and the suffix carries only the things that change what that move
/// is: a status that is not `active`, a pin, tags.
#[must_use]
pub fn summary_line(memory: &MemorySummaryJson<'_>, width: usize) -> String {
    let mut line = format!(
        "{}  {:<8}  {}",
        memory.id,
        memory.kind_label,
        truncate(memory.title, width)
    );
    let mut marks: Vec<String> = Vec::new();
    if memory.status != "active" {
        marks.push(memory.status.to_owned());
    }
    if memory.pinned {
        marks.push("pinned".to_owned());
    }
    if !memory.tags.is_empty() {
        marks.push(memory.tags.join(", "));
    }
    if !marks.is_empty() {
        line.push_str("  · ");
        line.push_str(&marks.join(" · "));
    }
    line
}

/// One ranked result: the summary line, and under it the match in context.
///
/// The score is deliberately **not** printed. It is a reciprocal-rank sum, so it is a
/// function of position rather than of similarity: with one leg it is `1/61` for the
/// first row and `1/64` for the fourth, and showing those to a person invites them to read
/// meaning into three decimal places that are not there. The position already says it.
/// `--json` carries the score, and `docs/CONTRACT.md` says what it is and is not. The
/// hosted product's tool handler makes the same call for the same reason.
#[must_use]
pub fn search_line(number: usize, hit: &SearchHitJson<'_>, width: usize) -> String {
    let mut out = format!("{number}. {}", summary_line(&hit.summary, width));
    if !hit.snippet.is_empty() {
        out.push_str("\n   ");
        out.push_str(&truncate(hit.snippet, width));
    }
    out
}

/// The full record, for `get`.
#[must_use]
pub fn render_memory(memory: &Memory, width: usize) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(out, "# {}", memory.title);
    let _ = writeln!(
        out,
        "{}  ·  {}  ·  {}  ·  workspace {}",
        memory.id,
        memory.kind_label(),
        memory.status,
        memory.workspace_id
    );

    let mut facts: Vec<String> = vec![
        format!("created {}", memory.created_at),
        format!("strength {:.2}", memory.strength),
    ];
    if let Some(confidence) = memory.confidence {
        facts.push(format!("confidence {confidence:.2}"));
    }
    if memory.pinned {
        facts.push("pinned".to_owned());
    }
    if memory.retrieval_count > 0 {
        facts.push(format!("retrieved {}x", memory.retrieval_count));
    }
    let _ = writeln!(out, "{}", facts.join("  ·  "));

    let mut provenance: Vec<String> = vec![memory.source_label().to_owned()];
    if let Some(url) = &memory.source_url {
        provenance.push(url.clone());
    }
    if !memory.tags.is_empty() {
        provenance.push(format!("tags: {}", memory.tags.join(", ")));
    }
    let _ = writeln!(out, "{}", provenance.join("  ·  "));

    if !memory
        .memory_metadata
        .as_object()
        .is_none_or(serde_json::Map::is_empty)
    {
        let _ = writeln!(
            out,
            "metadata: {}",
            truncate(&memory.memory_metadata.to_string(), width)
        );
    }

    if !memory.excerpt.is_empty() {
        let _ = writeln!(out, "\n{}", truncate(&memory.excerpt, width));
    }
    if !memory.body.is_empty() {
        let _ = writeln!(out, "\n{}", truncate(&memory.body, width));
    }
    out.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_success_envelope_has_the_documented_shape() {
        let json = serde_json::to_value(Success::new(json!({"id": "k_01"}))).unwrap();
        assert_eq!(json["ok"], serde_json::json!(true));
        assert_eq!(json["data"]["id"], serde_json::json!("k_01"));
        assert_eq!(
            json["meta"]["contract_version"],
            serde_json::json!(kiomon_core::CONTRACT_VERSION)
        );
        assert!(json["meta"]["version"].is_string());
    }

    #[test]
    fn the_failure_envelope_has_the_documented_shape() {
        let body = Failure {
            ok: false,
            error: FailureBody {
                code: "not_found",
                message: "no memory with id `k_01`".into(),
            },
            meta: Meta::current(),
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["ok"], serde_json::json!(false));
        assert_eq!(json["error"]["code"], serde_json::json!("not_found"));
        assert!(json["error"]["message"].is_string());
        assert!(json["meta"]["contract_version"].is_string());
    }

    #[test]
    fn truncation_counts_characters_and_says_when_it_cut() {
        assert_eq!(truncate("short", 20), "short");
        assert_eq!(truncate("exactlyten", 10), "exactlyten");
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
        // A multi-byte string must not panic, which is the whole reason this counts
        // characters rather than bytes.
        assert_eq!(truncate("İstanbul İstanbul", 5), "İsta…");
        assert_eq!(truncate("anything", 0), "anything");
    }
}
