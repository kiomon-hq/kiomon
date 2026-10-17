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
    Assessment, ContextPacket, DecayOutcome, EmbedReport, Error, ExitCode, LegsRun, Memory,
    MemorySummaryJson, OutcomeReport, PacketItem, RatingOutcome, Result, SearchAttempt,
    SearchHitJson, SearchOutcome, Skipped,
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
    /// Which legs answered. An object rather than a word: "lexical only", "vector only"
    /// and "both" are three states, and the door reports them this way (`{fts, vector}`)
    /// so that `serve` (WP9) has nothing to translate.
    pub legs: LegsRun,
    /// The model the query vector came from, when the vector leg ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_model: Option<&'a str>,
    /// See [`SearchOutcome::degraded`].
    pub degraded: bool,
    /// Why: empty for a normal answer, otherwise the name of the reduced path. See
    /// [`SearchOutcome::reason`].
    pub reason: &'static str,
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
            vector_model: outcome.vector_model.as_deref(),
            degraded: outcome.degraded,
            reason: outcome.reason,
            recorded: outcome.recorded,
        }
    }
}

/// Render a `reindex --embed` report for a human.
///
/// The two numbers that matter are the ones a user can act on: how many memories were
/// embedded, and how many are still missing one — the latter being the same number the
/// next run will find.
#[must_use]
pub fn reindex_line(report: &EmbedReport) -> String {
    let mut parts: Vec<String> = vec![format!("model {}", report.model)];
    if report.dry_run {
        parts.push(format!("{} would be embedded", report.embedded));
    } else {
        parts.push(format!("{} embedded", report.embedded));
        if report.present > 0 {
            parts.push(format!("{} already had a vector", report.present));
        }
    }
    if report.truncated > 0 {
        parts.push(format!(
            "{} truncated to the model's window",
            report.truncated
        ));
    }
    if !report.errors.is_empty() {
        parts.push(format!("{} failed", report.errors.len()));
    }
    parts.join("  ·  ")
}

/// The `data` block of a `review list` response.
///
/// `count` is the whole inbox in scope and `items` is the page, because an agent
/// answering "is there anything to review" wants the first and a human wants the second.
/// Reporting only the page would make an inbox of five look the same as one of five
/// thousand.
#[derive(Debug, Serialize)]
pub struct PendingResponse<'a> {
    /// Everything waiting in scope.
    pub count: i64,
    /// The page, newest first.
    pub items: Vec<MemorySummaryJson<'a>>,
}

impl<'a> PendingResponse<'a> {
    /// Shape a page of drafts for the wire.
    #[must_use]
    pub fn new(page: &'a [Memory], count: i64) -> Self {
        Self {
            count,
            items: page.iter().map(MemorySummaryJson::from).collect(),
        }
    }
}

/// One draft, for `review list`.
///
/// The summary line plus the confidence, which is the one number a reviewer needs and
/// the only thing that distinguishes two drafts of the same shape. Printed as `0.40` and
/// not `40%`: the whole system says confidence in tenths of one, and a second unit for
/// the same quantity is how a reader ends up comparing a percentage to a decimal.
#[must_use]
pub fn review_line(memory: &MemorySummaryJson<'_>, width: usize) -> String {
    let mut line = summary_line(memory, width);
    let mut marks: Vec<String> = Vec::new();
    if let Some(confidence) = memory.confidence {
        marks.push(format!("conf {confidence:.2}"));
    }
    marks.push(format!("source {}", memory.source_label));
    line.push_str("  · ");
    line.push_str(&marks.join(" · "));
    line
}

/// One skipped id, for the warning stream.
///
/// Every verb that can leave an id alone reports it in the same shape, so a script
/// reading warnings does not have to learn a different sentence per verb.
#[must_use]
pub fn skip_line(verb: &str, skipped: &Skipped) -> String {
    match skipped.status {
        Some(status) => format!(
            "{verb}: `{}` is {}, so nothing happened to it ({})",
            skipped.id,
            status.as_str(),
            skipped.reason.as_str()
        ),
        None => format!("{verb}: `{}` — {}", skipped.id, skipped.reason.as_str()),
    }
}

/// What `outcome` did, in one line.
#[must_use]
pub fn outcome_line(report: &OutcomeReport) -> String {
    if report.applied.is_empty() {
        return format!(
            "Credited nothing: none of the {} memories could be reached.",
            report.skipped.len()
        );
    }
    // The breakdown is worth a line because one `outcome` call is usually several
    // different assessments, and "3 credited" hides which three.
    let mut counts: Vec<(Assessment, usize)> = Vec::new();
    for applied in &report.applied {
        match counts
            .iter_mut()
            .find(|(kind, _)| *kind == applied.assessment)
        {
            Some((_, count)) => *count += 1,
            None => counts.push((applied.assessment, 1)),
        }
    }
    let breakdown: Vec<String> = counts
        .iter()
        .map(|(assessment, count)| format!("{count} {}", assessment.as_str()))
        .collect();
    let skipped = if report.skipped.is_empty() {
        String::new()
    } else {
        format!(" {} skipped.", report.skipped.len())
    };
    format!(
        "Recorded `{}` for {} memor{}: {}.{skipped}",
        report.outcome.as_str(),
        report.applied.len(),
        if report.applied.len() == 1 {
            "y"
        } else {
            "ies"
        },
        breakdown.join(", ")
    )
}

/// What `rate` did, in one line.
///
/// The last sentence is not padding: `rate` looking like it moved a memory's ranking is
/// the single most likely thing to be assumed about it, and it does not.
#[must_use]
pub fn rating_line(outcome: &RatingOutcome) -> String {
    format!(
        "Rated {} {} · feedback score {:.2} over {} rating{}. Strength is decay's, and this \
         did not touch it.",
        outcome.id,
        outcome.rating.as_str(),
        outcome.feedback_score,
        outcome.feedback_count,
        if outcome.feedback_count == 1 { "" } else { "s" }
    )
}

/// The decay table: what a pass did, or would do.
///
/// The `--dry-run` header says "would" and includes the word *nothing*, because the one
/// thing a reader has to come away with is that the numbers are a forecast.
#[must_use]
pub fn decay_table(outcome: &DecayOutcome, width: usize) -> String {
    // `fmt::Write` is what lets a line be appended with `write!` instead of
    // `push_str(&format!(..))`, which would allocate a throwaway string per line.
    use std::fmt::Write as _;

    let mut text = String::new();
    if outcome.dry_run {
        let _ = writeln!(
            text,
            "Would decay {} of {} scanned memories and archive {}; this is a dry run and \
             nothing was written.",
            outcome.decayed, outcome.scanned, outcome.archived
        );
    } else {
        let _ = writeln!(
            text,
            "Decayed {} of {} scanned memories; archived {}.",
            outcome.decayed, outcome.scanned, outcome.archived
        );
    }
    if outcome.is_noop() {
        text.push_str("Nothing was old enough to move.\n");
        return text;
    }
    for row in &outcome.details {
        let marked = if row.archived {
            if row.expired {
                "  archived (past its deadline)"
            } else {
                "  archived (below the floor)"
            }
        } else {
            ""
        };
        let _ = writeln!(
            text,
            "  {}  {:<10}  {:.0}d  {:.3} → {:.3}{marked}",
            truncate(&row.id, 28),
            truncate(row.kind, width),
            row.age_days,
            row.before,
            row.after
        );
    }
    if outcome.truncated {
        // Both counts are row counts, and the report cannot hold more rows than a
        // process can address, so this conversion is exact in every case that can occur.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let more = usize::try_from(outcome.decayed + outcome.archived)
            .unwrap_or(usize::MAX)
            .saturating_sub(outcome.details.len());
        let _ = writeln!(
            text,
            "  … and {more} more, which `--json` counts but does not list."
        );
    }
    text
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

/// The packet, for a person.
///
/// The summary leads because it is the packet's own account of itself, and an account a
/// caller cannot see is one they cannot check. Sections follow in the order a reader needs
/// them, and a section with nothing in it is **omitted** rather than printed empty: "no
/// warnings" and "the warning check did not run" look the same in a list of nothing, which
/// is exactly why the packet puts the second one *in* the warnings when it happens.
#[must_use]
pub fn render_packet(packet: &ContextPacket, width: usize) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(out, "{}", truncate(&packet.summary, width));

    let mut section = |heading: &str, lines: &[String]| {
        if lines.is_empty() {
            return;
        }
        let _ = writeln!(out, "\n{heading}");
        for line in lines {
            let _ = writeln!(out, "{line}");
        }
    };

    section(
        "FACTS",
        &packet
            .facts
            .iter()
            .map(|item| packet_item(item, width))
            .collect::<Vec<_>>(),
    );
    section(
        "PROCEDURE",
        &packet
            .procedure
            .iter()
            .map(|item| packet_item(item, width))
            .collect::<Vec<_>>(),
    );
    section(
        "EPISODES",
        &packet
            .episodes
            .iter()
            .map(|item| packet_item(item, width))
            .collect::<Vec<_>>(),
    );
    section(
        "EVIDENCE",
        &packet
            .evidence
            .iter()
            .map(|reference| {
                let mut line = format!("- {}", reference.memory_id);
                if let Some(uri) = &reference.source_uri {
                    let _ = write!(line, "  {}", truncate(uri, width));
                }
                if let Some(span) = &reference.source_span {
                    let _ = write!(line, "  · {}", truncate(span, width));
                }
                line
            })
            .collect::<Vec<_>>(),
    );
    section(
        "WARNINGS",
        &packet
            .warnings
            .iter()
            .map(|warning| format!("! {}", truncate(warning, width)))
            .collect::<Vec<_>>(),
    );
    section(
        "CONFLICTS",
        &packet
            .conflicts
            .iter()
            .map(|conflict| {
                format!(
                    "! {} vs {} — {}",
                    conflict.memory_id_a, conflict.memory_id_b, conflict.note
                )
            })
            .collect::<Vec<_>>(),
    );
    out
}

/// One packet item: the claim, then the facts about it that a caller has to weigh.
///
/// The claim is first and is the only part that is worth truncating, because it is what the
/// caller asked for. `unknown` applicability is the default and is **not** printed: a line
/// that says "unknown" on every item teaches a reader to skip the line that sometimes
/// carries the reason an item is only conditionally true.
fn packet_item(item: &PacketItem, width: usize) -> String {
    use std::fmt::Write as _;

    let mut line = format!("{}  {}", item.memory_id, truncate(&item.claim, width));
    let _ = write!(
        line,
        "\n     {}  ·  {}  ·  confidence {:.2}",
        item.kind, item.authority, item.confidence
    );
    if item.applicability != "unknown" {
        let _ = write!(line, "  ·  {}", item.applicability);
    }
    if let Some(condition) = &item.condition {
        let _ = write!(line, " ({})", truncate(condition, width));
    }
    if let Some(reference) = &item.source_ref {
        let _ = write!(line, "  ·  from {}", truncate(reference, width));
    }
    line
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

// ---------------------------------------------------------------------------
// bench
// ---------------------------------------------------------------------------

/// One command's samples, in milliseconds.
///
/// The samples are kept rather than reduced to a mean, because a mean of a bimodal
/// distribution is a number that describes no run that ever happened. Every statistic the
/// report prints is derived from the same list, and the list's worst member is printed
/// too.
#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    /// Each cold-process sample, in milliseconds.
    pub samples: Vec<f64>,
}

impl Sample {
    /// The median, by nearest rank.
    #[must_use]
    pub fn p50(&self) -> f64 {
        self.rank(0.5)
    }

    /// The 95th percentile, by nearest rank.
    ///
    /// Nearest rank rather than an interpolated quantile: with fifteen samples an
    /// interpolated p95 is a number between the largest two and describes neither, and
    /// this project would rather report a sample that happened.
    #[must_use]
    pub fn p95(&self) -> f64 {
        self.rank(0.95)
    }

    /// The smallest sample, or zero if there are none.
    #[must_use]
    pub fn min(&self) -> f64 {
        self.samples.iter().copied().fold(f64::INFINITY, f64::min)
    }

    /// The largest sample, or zero if there are none.
    #[must_use]
    pub fn max(&self) -> f64 {
        self.samples
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max)
    }

    /// How far the samples spread, as a fraction of the median.
    ///
    /// A command whose worst sample is twice its median is one whose median nobody should
    /// plan against, and this is how the report says so instead of averaging it away.
    /// (The plan's *reproducibility* criterion is about run-to-run agreement and needs
    /// `--repeat`; this is the within-run version, which is the one a single run can see.)
    #[must_use]
    pub fn spread(&self) -> f64 {
        let median = self.p50();
        if median <= 0.0 || self.samples.is_empty() {
            return 0.0;
        }
        (self.max() - self.min()) / median
    }

    fn rank(&self, percentile: f64) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_by(f64::total_cmp);
        // A sample count, so the conversions are exact for any run a person would sit
        // through; the lint is about the general case and this is not it.
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let index = ((sorted.len() as f64 * percentile).ceil() as usize)
            .saturating_sub(1)
            .min(sorted.len() - 1);
        sorted[index]
    }
}

impl From<Vec<f64>> for Sample {
    fn from(samples: Vec<f64>) -> Self {
        Self { samples }
    }
}

/// One row of the report: a command, and what it cost.
///
/// The raw samples are kept and pooled across `--repeat` passes rather than reduced
/// pass-by-pass, because the host this was developed on moves by more than the plan's
/// whole reproducibility budget in the time it takes to run the suite twice. Sixty
/// samples pooled are a better estimate than three medians averaged, and the per-pass
/// medians are kept beside them so a reader can see the drift rather than be told about
/// it.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    /// What was run, in the report's own vocabulary.
    pub label: String,
    /// The query, for the rows that are searches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// How many rows the query matched, for the rows that are searches.
    pub matches: i64,
    /// How many samples one pass took.
    pub runs: usize,
    /// How many passes were pooled.
    pub passes: usize,
    /// The median of every sample, spawn included.
    pub total_p50_ms: f64,
    /// The 95th percentile, spawn included.
    pub total_p95_ms: f64,
    /// The median of `total − floor`, sampled in the same pass.
    pub work_p50_ms: f64,
    /// The 95th percentile of `total − floor`, sampled in the same pass.
    pub work_p95_ms: f64,
    /// The fastest sample, spawn included.
    pub min_ms: f64,
    /// The slowest sample, spawn included.
    pub max_ms: f64,
    /// `(max − min) / median`, on the raw totals.
    pub spread: f64,
    /// The work median from each pass, when there was more than one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pass_work_p50_ms: Vec<f64>,
    /// Whether the passes agreed within the plan's fifteen percent. `None` for one pass,
    /// because one pass cannot disagree with itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reproducible: Option<bool>,
    /// Every sample, kept for the merge and never serialized.
    #[serde(skip)]
    pub total_ms: Vec<f64>,
    /// The spawn floor sampled alongside each of them.
    #[serde(skip)]
    pub floor_ms: Vec<f64>,
}

impl Row {
    /// An empty row, ready for samples.
    ///
    /// Every statistic is zero until [`Row::summarise`] has run, and `summarise` is called
    /// once, after all the samples are in — never once per sample. Two places computing the
    /// same median is two places to disagree about it.
    #[must_use]
    pub fn empty(label: String, query: Option<String>, matches: i64) -> Self {
        Self {
            label,
            query,
            matches,
            runs: 0,
            passes: 1,
            total_p50_ms: 0.0,
            total_p95_ms: 0.0,
            work_p50_ms: 0.0,
            work_p95_ms: 0.0,
            min_ms: 0.0,
            max_ms: 0.0,
            spread: 0.0,
            pass_work_p50_ms: Vec::new(),
            reproducible: None,
            total_ms: Vec::new(),
            floor_ms: Vec::new(),
        }
    }

    /// The work samples: what each sample cost with the floor it was measured against
    /// subtracted, paired by position.
    ///
    /// Paired by position rather than by mean because the floor and the command move
    /// together: a sample taken while the machine was busy should be compared with a
    /// floor taken while it was busy, not with the quiet average. Where the floor has
    /// fewer samples than the row — it is sampled once per pass and a row is sampled
    /// `--runs` times per pass — the floor samples are reused, which is right, because
    /// each pass has one floor and every row in that pass shares it.
    #[must_use]
    pub fn work_samples(&self) -> Vec<f64> {
        if self.floor_ms.is_empty() {
            return self.total_ms.clone();
        }
        self.total_ms
            .iter()
            .enumerate()
            .map(|(index, total)| (total - self.floor_ms[index % self.floor_ms.len()]).max(0.0))
            .collect()
    }

    /// Fill in every statistic from the raw samples.
    pub(crate) fn summarise(&mut self) {
        let total = Sample::from(self.total_ms.clone());
        let work = Sample::from(self.work_samples());
        self.total_p50_ms = round(total.p50());
        self.total_p95_ms = round(total.p95());
        self.work_p50_ms = round(work.p50());
        self.work_p95_ms = round(work.p95());
        self.min_ms = round(total.min());
        self.max_ms = round(total.max());
        self.spread = round(total.spread());
    }
}

/// Merge `--repeat` passes into one report.
///
/// The rows are the same commands in the same order every pass, so they are matched by
/// label rather than by position: a pass that measured one row fewer would otherwise
/// shift every number in the report onto the wrong command, silently.
pub(crate) fn merge_passes(passes: &[Vec<Row>]) -> (Vec<Row>, Vec<GateResult>) {
    let Some(mut merged) = passes.first().cloned() else {
        return (Vec::new(), Vec::new());
    };
    for row in &mut merged {
        row.passes = passes.len();
        row.total_ms.clear();
        row.floor_ms.clear();
        row.pass_work_p50_ms = passes
            .iter()
            .filter_map(|pass| {
                pass.iter()
                    .find(|candidate| candidate.label == row.label)
                    .map(|candidate| round(Sample::from(candidate.work_samples()).p50()))
            })
            .collect();
        for pass in passes {
            if let Some(candidate) = pass.iter().find(|candidate| candidate.label == row.label) {
                row.total_ms.extend(&candidate.total_ms);
                // The floor is the same list for every row in a pass, and pooling it once
                // per row keeps the positional pairing intact.
                row.floor_ms.extend(&candidate.floor_ms);
            }
        }
        let lowest = row
            .pass_work_p50_ms
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min);
        let highest = row
            .pass_work_p50_ms
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max);
        row.reproducible = Some(lowest > 0.0 && highest / lowest <= 1.15);
        row.summarise();
    }
    let gates = evaluate(&merged);
    (merged, gates)
}

/// Which percentile a gate reads.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Percentile {
    /// The median.
    P50,
    /// The 95th.
    P95,
}

/// One line of the plan's §7 budget, as code.
///
/// Absolute rather than a ratio against the last run, and the plan says why: a ratio gate
/// on a shared runner is a flaky-test generator. Absolute gates are noisy too — which is
/// why they read `work`, and why the report prints the spread next to them — but a gate
/// that occasionally fails loudly is worth more than one that never fails at all.
pub(crate) struct Gate {
    /// The row it reads.
    pub(crate) label: &'static str,
    /// Which percentile of that row.
    pub(crate) percentile: Percentile,
    /// The most it may be, in milliseconds.
    pub(crate) limit_ms: f64,
}

/// The gates, exactly as the plan states them.
///
/// The hybrid-search gate is **absent rather than passing**: there is no embedder until
/// WP7, and a gate that cannot fail is not a gate. `bench --gate` says so out loud rather
/// than leaving the reader to notice the omission.
pub(crate) const GATES: &[Gate] = &[
    Gate {
        label: "open",
        percentile: Percentile::P50,
        limit_ms: 5.0,
    },
    Gate {
        label: "add",
        percentile: Percentile::P95,
        limit_ms: 40.0,
    },
    Gate {
        label: "common (fts)",
        percentile: Percentile::P95,
        limit_ms: 60.0,
    },
    // §7's hybrid budget, and the number this work package is answerable for: one query
    // embedding, a brute-force scan of every vector in scope, and a fusion of two legs on
    // ten thousand memories. The ledger is in the plan's WP7 record; the short version is
    // that the scan is 15 MB of blobs and a few million multiply-adds, which is tens of
    // milliseconds and not hundreds.
    Gate {
        label: "common (hybrid)",
        percentile: Percentile::P95,
        limit_ms: 250.0,
    },
    // The packet, on the same query as `search common` and with its retrieval record off,
    // so this row is comparable with that one. The limit is the search budget rather than a
    // budget of its own: what WP5.5 claims is that a packet costs what its search costs
    // plus a clock read, two indexed seeks and one record, and a gate that allowed more
    // would not be testing that claim.
    Gate {
        label: "retrieve (no record)",
        percentile: Percentile::P95,
        limit_ms: 45.0,
    },
];

/// Evaluate the plan's gates against the rows.
fn evaluate(rows: &[Row]) -> Vec<GateResult> {
    GATES
        .iter()
        .map(|gate| {
            let row = rows.iter().find(|row| row.label == gate.label);
            let measured = row.map(|row| match gate.percentile {
                Percentile::P50 => row.work_p50_ms,
                Percentile::P95 => row.work_p95_ms,
            });
            GateResult {
                label: gate.label,
                percentile: match gate.percentile {
                    Percentile::P50 => "p50",
                    Percentile::P95 => "p95",
                },
                limit_ms: gate.limit_ms,
                measured_ms: measured,
                passed: measured.is_some_and(|value| value <= gate.limit_ms),
            }
        })
        .collect()
}

/// One line of the plan's budget, and what this run measured against it.
#[derive(Debug, Clone, Serialize)]
pub struct GateResult {
    /// The row it reads.
    pub label: &'static str,
    /// Which percentile of that row.
    pub percentile: &'static str,
    /// The most it may be.
    pub limit_ms: f64,
    /// What it was, or `None` if the row is missing.
    pub measured_ms: Option<f64>,
    /// Whether it was met.
    pub passed: bool,
}

impl GateResult {
    /// The gate as a line of the human report.
    #[must_use]
    pub fn line(&self) -> String {
        let measured = self.measured_ms.map_or_else(
            || "not measured".to_owned(),
            |value| format!("{value:.2} ms"),
        );
        format!(
            "  {} {} {} ≤ {:.2} ms",
            if self.passed { "PASS" } else { "FAIL" },
            self.label,
            measured,
            self.limit_ms
        )
    }
}

/// One corpus, fully measured.
#[derive(Debug, Clone, Serialize)]
pub struct BenchReport {
    /// What was measured: its size, its shape, its path, and how many rows each probe
    /// query matches.
    pub corpus: kiomon_core::bench::CorpusReport,
    /// The spawn floor every `work` figure was derived from.
    pub spawn_floor: Sample,
    /// The rows.
    pub rows: Vec<Row>,
    /// The plan's gates.
    pub gates: Vec<GateResult>,
    /// This process's peak resident set, where the platform reports one.
    pub peak_rss_kb: Option<u64>,
    /// What the plan asks of this build that this build cannot answer yet.
    pub rejected: Vec<&'static str>,
}

/// The `data` block of a `bench` response.
#[derive(Debug, Clone, Serialize)]
pub struct BenchResponse {
    /// The directory the corpora are in, which is on some volume or other — and which
    /// volume it is happens to be one of the largest variables in the whole report.
    pub corpus_dir: String,
    /// The operating system and architecture, because a spawn floor is a property of one.
    pub os: String,
    /// The processor count, which decides how much of this runs while other things do.
    pub cpus: usize,
    /// The size of the binary that was measured.
    pub binary_bytes: Option<u64>,
    /// Every corpus, largest last.
    pub reports: Vec<BenchReport>,
    /// Whether every gate was met.
    pub gates_passed: bool,
}

impl BenchResponse {
    pub(crate) fn new(
        reports: Vec<BenchReport>,
        dir: &std::path::Path,
        binary_bytes: Option<u64>,
    ) -> Self {
        let gates_passed = reports
            .iter()
            .all(|report| report.gates.iter().all(|gate| gate.passed));
        Self {
            corpus_dir: dir.display().to_string(),
            os: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
            cpus: std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get),
            binary_bytes,
            reports,
            gates_passed,
        }
    }
}

/// What `bench --open` reports.
#[derive(Debug, Clone, Serialize)]
pub struct OpenProbe {
    /// The database that was opened.
    pub path: String,
    /// Its schema version, read from the file header.
    pub schema_version: u32,
    /// Peak resident set, where the platform reports one.
    pub peak_rss_kb: Option<u64>,
}

/// The human report.
///
/// A table rather than prose, with the spawn floor and the corpus's shape *above* the
/// numbers they qualify. A benchmark's two most important numbers are what it measured and
/// what it subtracted, and a reader who has to scroll to find them will not.
///
/// Assembled with `writeln!` into a `String` whose writer cannot fail, so every `Result` is
/// discarded on purpose rather than handled.
#[must_use]
pub fn render_bench(response: &BenchResponse) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "kiomon bench — {} on {} ({} cpus), binary {}",
        env!("CARGO_PKG_VERSION"),
        response.os,
        response.cpus,
        response
            .binary_bytes
            .map_or_else(|| "unknown".to_owned(), kib)
    );
    let _ = writeln!(out, "corpus volume: {}\n", response.corpus_dir);
    for report in &response.reports {
        out.push_str(&render_report(report));
    }
    let _ = writeln!(
        out,
        "gates (the plan's budget, read against the column that is work and not wall time)"
    );
    for report in &response.reports {
        let _ = writeln!(out, "  at {} memories", report.corpus.size);
        for gate in &report.gates {
            let _ = writeln!(out, "{}", gate.line());
        }
    }
    let _ = writeln!(out, "\nnot measured by this build");
    for line in response
        .reports
        .first()
        .map_or(&[][..], |report| &report.rejected)
    {
        let _ = writeln!(out, "  {line}");
    }
    let _ = writeln!(
        out,
        "\n{}\n\nThe gates read `work`: `total` minus the {:.2} ms this platform spends spawning a \
         process at all. `total` is what a user pays; `work` is what the code is answerable for.",
        if response.gates_passed {
            "every gate the plan defines for this build was met"
        } else {
            "a gate was missed; CI fails on this, and so should a release"
        },
        response
            .reports
            .last()
            .map_or(0.0, |report| report.spawn_floor.p50()),
    );
    out
}

/// One corpus's table.
fn render_report(report: &BenchReport) -> String {
    use std::fmt::Write as _;
    let corpus = &report.corpus;
    let mut out = String::new();
    let _ = writeln!(out, "{} memories", corpus.size);
    let _ = writeln!(
        out,
        "  corpus: {:.1} MiB on disk, {:.2} KiB mean body, built in {:.1} s{}",
        mib(corpus.disk_bytes),
        mean_kib(corpus.mean_body_bytes()),
        corpus.seconds,
        if corpus.reused { " (reused)" } else { "" }
    );
    out.push_str("  shape:  ");
    for class in &corpus.classes {
        let _ = write!(
            out,
            "{} {} ({:.0}% asked, {:.0}% built, {:.0} B mean)   ",
            class.class.as_str(),
            class.memories,
            class.target_percent,
            class.actual_percent(corpus.size as u64),
            class
                .mean_bytes()
                .map_or(0.0, |mean| mean_kib(mean) * 1024.0),
        );
    }
    let _ = writeln!(
        out,
        "\n  spawn floor: {:.2} ms p50, {:.2} ms p95",
        report.spawn_floor.p50(),
        report.spawn_floor.p95()
    );
    let _ = writeln!(out, "  what a query had to rank");
    for probe in &corpus.probes {
        let _ = writeln!(
            out,
            "    {:<9} {:>7} rows   {:>8}   {}",
            probe.name, probe.matches, probe.planted, probe.query
        );
    }
    let _ = writeln!(
        out,
        "  command                          work p50  work p95   total p50     min     max   spread"
    );
    for row in &report.rows {
        let _ = writeln!(
            out,
            "  {:<30} {:>8.2} {:>9.2} {:>11.2} {:>7.2} {:>7.2} {:>7.0}%{}",
            row.label,
            row.work_p50_ms,
            row.work_p95_ms,
            row.total_p50_ms,
            row.min_ms,
            row.max_ms,
            row.spread * 100.0,
            render_passes(row),
        );
    }
    let _ = writeln!(
        out,
        "  peak resident set: {}",
        report
            .peak_rss_kb
            .map_or_else(|| "not reported on this platform".to_owned(), kib)
    );
    out.push('\n');
    out
}

/// How the `--repeat` passes agreed, or nothing when there was only one.
///
/// The pass medians are printed and not just the verdict, because "not reproducible"
/// without the numbers behind it is an accusation the reader cannot check — and because the
/// *shape* of the drift is the diagnosis: three medians climbing together is a machine
/// heating up, while 21, 26, 24 is a machine with something else running on it.
fn render_passes(row: &Row) -> String {
    if row.pass_work_p50_ms.len() < 2 {
        return String::new();
    }
    let medians = row
        .pass_work_p50_ms
        .iter()
        .map(|value| format!("{value:.1}"))
        .collect::<Vec<_>>()
        .join(" / ");
    format!(
        "  passes {medians}{}",
        if row.reproducible == Some(false) {
            " — over 15% apart"
        } else {
            ""
        }
    )
}

/// A byte count as kibibytes or mebibytes, whichever reads better.
fn kib(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", mib(bytes))
    } else {
        format!("{:.0} KiB", mean_kib(bytes))
    }
}

/// Byte counts and body sizes, as floats.
///
/// A corpus is nowhere near 2^53 bytes and a body is nowhere near it either, so the
/// conversions are exact for every number this file will ever format. The lint is about the
/// general case, and this is not the general case.
#[allow(clippy::cast_precision_loss)]
fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Bytes as kibibytes.
#[allow(clippy::cast_precision_loss)]
fn mean_kib(bytes: u64) -> f64 {
    bytes as f64 / 1024.0
}

/// Two decimal places.
///
/// The timer is better than that and the machine is not: run-to-run variance here is
/// percent-level, so a third decimal is a digit nobody measured. Rounding is how the
/// report avoids implying precision it does not have.
fn round(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
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
