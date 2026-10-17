// SPDX-License-Identifier: Apache-2.0

//! The command-line surface.
//!
//! This file is `docs/CONTRACT.md` expressed as a parser, and the two are reviewed
//! against each other in the same diff. Where they disagree, one of them is a bug
//! and the pull request says which.
//!
//! Two rules shape everything here:
//!
//! - **A command's meaning is declared, not inferred.** Every flag that takes a
//!   constrained value uses an enum, so clap rejects a bad one with a message that
//!   lists the valid ones and an exit code of 3 — before any code runs, and before
//!   any database is opened.
//! - **`--workspace` absent is not `--workspace '*'`.** Absence means the configured
//!   default workspace. `'*'` is an explicit widening, and the distinction is tested,
//!   because a bare `--workspace` that silently searched every workspace would be a
//!   data-leak-shaped bug in a shared brain.

use std::path::PathBuf;
use std::str::FromStr;

use clap::{Args, Parser, Subcommand, ValueEnum};

use kiomon_core::{Legs, MemoryKind, Relation, SearchSort, Status};

/// A fast, local memory for your coding agent.
///
/// Kiomon keeps what an agent learns in one SQLite file and gets it back in a few
/// milliseconds — no account, no API key, no network. Every command takes `--json`
/// for machine consumption, and diagnostics never touch stdout.
#[derive(Debug, Parser)]
#[command(
    name = "kiomon",
    version,
    about = "A fast, local memory for your coding agent",
    disable_help_subcommand = true,
    propagate_version = true,
    max_term_width = 100,
    // Running `kiomon` with no subcommand is a usage error (exit 3), not a help
    // screen (exit 0). `clap`'s derive would otherwise set `arg_required_else_help`
    // for a required subcommand field, which turns the most common mistake a new
    // user makes into a success that a script cannot detect.
    subcommand_required = true,
    arg_required_else_help = false
)]
pub struct Cli {
    /// Path to the memory database. Overrides $KIOMON_DB and the config file.
    #[arg(long, global = true, value_name = "PATH")]
    pub db: Option<PathBuf>,

    /// Path to the config file: the embedder, and the default workspace.
    ///
    /// Overrides $KIOMON_CONFIG. Without either, `<data dir>/config.toml` is read when it
    /// exists, and a brain with no config file is a full-text brain rather than an error.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Emit one JSON document on stdout instead of human-readable text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Suppress non-error diagnostics on stderr.
    #[arg(long, short, global = true)]
    pub quiet: bool,

    /// Print full field values instead of truncating them to fit a line.
    #[arg(long, global = true)]
    pub full: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// Everything `kiomon` can do.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a database and a default workspace.
    Init(InitArgs),
    /// Check the database, the schema, and the configuration.
    Doctor(DoctorArgs),
    /// Show how much is in the brain, by kind and status.
    Stats(StatsArgs),

    /// Measure this build on this machine: retrieval, writes, and the plan's budget.
    Bench(BenchArgs),

    /// Save one memory.
    Add(AddArgs),
    /// Change an existing memory.
    Update(UpdateArgs),
    /// Import memories from a file or directory, or standard input.
    Import(ImportArgs),
    /// Draw a typed edge between two memories.
    Link(LinkArgs),
    /// Remove an edge between two memories.
    Unlink(UnlinkArgs),
    /// Show a memory's neighbours in the graph.
    Relate(RelateArgs),

    /// Search the brain: full text, fused with meaning when an embedder is configured.
    Search(SearchArgs),
    /// Retrieve a decision-ready packet for a task: facts, the best procedure, episodes,
    /// evidence and warnings, chosen by what you are trying to do.
    Retrieve(RetrieveArgs),
    /// Fetch one memory by id, with its body and metadata.
    Get(GetArgs),
    /// List memories without ranking them.
    List(ListArgs),

    /// Show what is waiting in the review inbox.
    Review(ReviewArgs),
    /// Report how retrieved memories actually performed, so credit lands where it is due.
    Outcome(OutcomeArgs),
    /// Age memories: apply decay, expire what is due, archive what has fallen below the floor.
    Decay(DecayArgs),

    /// Pin a memory: immune to decay, and ranked first in every ordering.
    Pin(IdsArgs),
    /// Remove a pin.
    Unpin(IdsArgs),
    /// Archive a memory: hidden from retrieval, restorable, never purged.
    Archive(IdsArgs),
    /// Bring an archived memory back.
    Restore(IdsArgs),
    /// Say whether a memory was useful. Feeds its strength and its decay clock.
    Rate(RateArgs),

    /// Rebuild an index: embed the memories that have no vector yet.
    Reindex(ReindexArgs),

    /// Permanently delete memories. The only destructive command.
    Forget(ForgetArgs),
    /// Write the brain out as JSONL or markdown.
    Export(ExportArgs),
}

// ---------------------------------------------------------------------------
// Setup and health
// ---------------------------------------------------------------------------

/// `kiomon init`
#[derive(Debug, Args)]
pub struct InitArgs {
    /// Also create this workspace. Repeating is allowed; `default` always exists.
    #[arg(long, value_name = "NAME")]
    pub workspace: Vec<String>,
}

/// `kiomon doctor`
#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Print the resolved paths and configuration without the checks.
    #[arg(long)]
    pub paths_only: bool,
}

/// `kiomon bench`
#[derive(Debug, Args)]
pub struct BenchArgs {
    /// Corpus sizes to measure, comma-separated. Each is built once and reused.
    ///
    /// One thousand and ten thousand by default. One hundred thousand is not among them
    /// because that corpus is 1.9 GiB and takes about a hundred seconds to build, and a
    /// ruler nobody picks up because it is heavy is not a ruler.
    #[arg(long, value_name = "N", value_delimiter = ',')]
    pub sizes: Vec<usize>,

    /// Where the corpora live. Defaults to `kiomon-bench` in the system temp directory,
    /// which is on the system volume rather than beside the source tree: a database's
    /// write cost can differ forty-fold between two disks, so the corpus's path is a
    /// measurement parameter rather than an implementation detail.
    #[arg(long, value_name = "DIR")]
    pub corpus: Option<PathBuf>,

    /// Build the corpus again even if a database of the right size is already there.
    #[arg(long)]
    pub rebuild: bool,

    /// Samples per command. A median and a 95th percentile need enough of them to mean
    /// anything, and every one is its own process.
    ///
    /// Fifty by default, and the number is not arbitrary. This host interrupts a spawn
    /// somewhere between once in forty and once in ninety — with a delay of five to twenty
    /// times the median, measured on `--version`, which opens no file and prints one line —
    /// and a 95th percentile over thirty samples is the 29th, so a single interruption
    /// decides the gate. At fifty the percentile has two samples of slack; at two hundred
    /// it has ten, which is what CI uses. A gate read at too few samples is a gate that
    /// fails on the weather.
    #[arg(long, value_name = "N", default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=2000))]
    pub runs: u32,

    /// Run the whole suite this many times and report how far the medians moved.
    ///
    /// The plan's reproducibility criterion as a number: three runs on one machine must
    /// agree within fifteen percent, or the harness is measuring the machine's mood
    /// rather than the code.
    #[arg(long, value_name = "N", default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=10))]
    pub repeat: u32,

    /// Apply the plan's performance budget, and exit `1` if any part of it is missed.
    #[arg(long)]
    pub gate: bool,

    /// Open the database, report what the platform offers about it, and exit.
    ///
    /// The probe the harness times to measure `open` on its own. It is public because a
    /// reader who wants to know what opening their own brain costs should not have to
    /// read the harness to find out how to ask.
    #[arg(long)]
    pub open: bool,
}

/// `kiomon stats`
#[derive(Debug, Args)]
pub struct StatsArgs {
    /// Restrict the counts to these kinds.
    #[arg(long, value_enum, value_delimiter = ',')]
    pub kind: Vec<KindArg>,

    /// Workspace to count. `*` counts every workspace.
    #[arg(long, value_name = "WORKSPACE")]
    pub workspace: Option<String>,
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// `kiomon add`
#[derive(Debug, Args)]
#[command(group(
    clap::ArgGroup::new("body_source")
        .args(["body", "body_file", "stdin", "from_file"])
        .multiple(false)
))]
pub struct AddArgs {
    /// What kind of knowledge this is.
    #[arg(long, value_enum)]
    pub kind: KindArg,

    /// Short, searchable title.
    #[arg(long, value_name = "TEXT", required_unless_present = "from_file")]
    pub title: Option<String>,

    /// Take the whole memory from this file: the title is its first heading, else its
    /// file name, and the body is the file.
    #[arg(long, value_name = "PATH")]
    pub from_file: Option<PathBuf>,

    /// The full content.
    #[arg(long, value_name = "TEXT")]
    pub body: Option<String>,

    /// Read the full content from this file.
    #[arg(long, value_name = "PATH")]
    pub body_file: Option<PathBuf>,

    /// Read the full content from standard input.
    #[arg(long)]
    pub stdin: bool,

    /// The one-line subheading shown under the title.
    #[arg(long, value_name = "TEXT")]
    pub excerpt: Option<String>,

    /// A tag. Repeatable, up to 20.
    #[arg(long, value_name = "TAG")]
    pub tag: Vec<String>,

    /// Where this came from.
    #[arg(long, value_name = "URL")]
    pub url: Option<String>,

    /// How reliable this is, 0.0–1.0. An agent reflection at 0.75 or above activates
    /// immediately; below it lands in the review inbox.
    #[arg(long, value_name = "0.0-1.0")]
    pub confidence: Option<f64>,

    /// Key entities, comma-separated. Supplying these is what skips an extraction pass.
    #[arg(long, value_delimiter = ',', value_name = "A,B")]
    pub entities: Vec<String>,

    /// Verifiable facts, comma-separated.
    #[arg(long, value_delimiter = ',', value_name = "A,B")]
    pub claims: Vec<String>,

    /// Topic label.
    #[arg(long, value_name = "TEXT")]
    pub domain: Option<String>,

    /// The whole per-kind envelope, as a JSON object. Mutually exclusive with
    /// `--entities`, `--claims` and `--domain`: one of the two ways to say the same
    /// thing, never both, because a merge rule would be a precedence puzzle nobody asked
    /// for.
    #[arg(long, value_name = "JSON", conflicts_with_all = ["entities", "claims", "domain"])]
    pub metadata: Option<String>,

    /// Id of a memory this was derived from. Repeatable; becomes a `derived_from` edge.
    #[arg(long = "ref", value_name = "ID")]
    pub derived_from: Vec<String>,

    /// Workspace to write to. Defaults to the configured workspace.
    #[arg(long, value_name = "WORKSPACE")]
    pub workspace: Option<String>,

    /// Where the memory came from: user_saved, agent_reflection, import, connector_sync.
    #[arg(long, value_name = "SOURCE", default_value = "user_saved")]
    pub source: String,

    /// Save even if an identical memory already exists.
    #[arg(long)]
    pub force: bool,
}

/// `kiomon update`
#[derive(Debug, Args)]
#[command(group(
    clap::ArgGroup::new("body_source")
        .args(["body", "body_file", "stdin"])
        .multiple(false)
))]
pub struct UpdateArgs {
    /// The memory to change.
    pub id: String,

    /// New title.
    #[arg(long, value_name = "TEXT")]
    pub title: Option<String>,

    /// New full content.
    #[arg(long, value_name = "TEXT")]
    pub body: Option<String>,

    /// New full content, from this file.
    #[arg(long, value_name = "PATH")]
    pub body_file: Option<PathBuf>,

    /// New full content, from standard input.
    #[arg(long)]
    pub stdin: bool,

    /// New subheading.
    #[arg(long, value_name = "TEXT")]
    pub excerpt: Option<String>,

    /// Replace the tags with exactly these.
    #[arg(long, value_name = "TAG")]
    pub tag: Vec<String>,

    /// The memory's kind.
    #[arg(long, value_enum)]
    pub kind: Option<KindArg>,

    /// How reliable this is, 0.0–1.0.
    #[arg(long, value_name = "0.0-1.0")]
    pub confidence: Option<f64>,

    /// How important this is, 0.0–1.0. Sets the decaying strength's ceiling.
    #[arg(long, value_name = "0.0-1.0")]
    pub importance: Option<f64>,

    /// Source URL.
    #[arg(long, value_name = "URL")]
    pub url: Option<String>,

    /// Replace the per-kind metadata with this JSON object.
    #[arg(long, value_name = "JSON")]
    pub metadata: Option<String>,
}

/// `kiomon import`
#[derive(Debug, Args)]
pub struct ImportArgs {
    /// A `.jsonl` export, a markdown file, or a directory of them. `-` reads JSONL
    /// from standard input.
    pub path: String,

    /// Workspace to import into. Defaults to the configured workspace.
    #[arg(long, value_name = "WORKSPACE")]
    pub workspace: Option<String>,

    /// Parse everything and report what would happen, writing nothing.
    #[arg(long)]
    pub dry_run: bool,
}

/// `kiomon link`
#[derive(Debug, Args)]
pub struct LinkArgs {
    /// The memory the edge starts at.
    pub source: String,
    /// The memory the edge points to.
    pub target: String,
    /// The kind of edge.
    #[arg(long, value_enum)]
    pub relation: Option<RelationArg>,
    /// Similarity, 0.0–1.0, when the edge came from a measurement.
    #[arg(long, value_name = "0.0-1.0")]
    pub score: Option<f64>,
}

/// `kiomon unlink`
#[derive(Debug, Args)]
pub struct UnlinkArgs {
    /// The memory the edge starts at.
    pub source: String,
    /// The memory the edge points to.
    pub target: String,
    /// Only remove an edge with this relation. Without it, every edge between the two
    /// is removed.
    #[arg(long, value_enum)]
    pub relation: Option<RelationArg>,
}

/// `kiomon relate`
#[derive(Debug, Args)]
pub struct RelateArgs {
    /// The memory to start from.
    pub id: String,
    /// Only follow these relations. Omit for all of them.
    #[arg(long, value_enum, value_delimiter = ',')]
    pub relation: Vec<RelationArg>,
    /// How many hops to walk. `1` is the memory's direct neighbours.
    #[arg(long, default_value_t = 1, value_name = "N")]
    pub depth: u8,
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

/// `kiomon retrieve`
#[derive(Debug, Args)]
pub struct RetrieveArgs {
    /// The query: what you are about to do, in your own words.
    pub query: String,

    /// What you are about to do. Decides which kind of memory leads, and which memories fit
    /// when there are more candidates than the packet holds.
    #[arg(long, value_enum, default_value_t = IntentArg::Answer)]
    pub intent: IntentArg,

    /// Hints about the task, used only to judge how applicable a memory is. Repeatable, up
    /// to 16. Nothing here is stored, and nothing is sent anywhere.
    #[arg(long, value_name = "HINT")]
    pub constraint: Vec<String>,

    /// Restrict the candidate pool to these kinds. Repeatable or comma-separated.
    #[arg(long, value_enum, value_delimiter = ',', value_name = "KIND")]
    pub kind: Vec<KindArg>,

    /// Workspace to retrieve from. `*` covers every workspace.
    #[arg(long, value_name = "WORKSPACE")]
    pub workspace: Option<String>,

    /// Maximum items in the packet, 1–20.
    #[arg(long, default_value_t = 6, value_parser = clap::value_parser!(u8).range(1..=20))]
    pub limit: u8,

    /// Do not count the items in the packet against their decay clock.
    #[arg(long)]
    pub no_record: bool,

    /// Which legs the search under the packet may use. `fts` builds the packet without
    /// going near the embedder, which is what to reach for when it is down.
    #[arg(long, value_enum, default_value_t = LegsArg::Auto)]
    pub legs: LegsArg,
}

/// `kiomon search`
#[derive(Debug, Args)]
pub struct SearchArgs {
    /// The query: keywords, a phrase, or a whole sentence.
    pub query: String,

    /// Restrict to these kinds. Repeatable or comma-separated.
    #[arg(long, value_enum, value_delimiter = ',', value_name = "KIND")]
    pub kind: Vec<KindArg>,

    /// Workspace to search. `*` searches every workspace.
    #[arg(long, value_name = "WORKSPACE")]
    pub workspace: Option<String>,

    /// Maximum results, 1–50.
    #[arg(long, default_value_t = 10, value_name = "N", value_parser = clap::value_parser!(u16).range(1..=50))]
    pub limit: u16,

    /// Which retrieval legs to run. `auto` uses the vector leg when an embedder is
    /// configured and falls back to `fts` when one is not.
    #[arg(long, value_enum, default_value_t = LegsArg::Auto)]
    pub legs: LegsArg,

    /// Ordering. Pinned memories sort first under `relevance` and `strength`; `recent`
    /// is honestly recent, and pinned rows do not jump it.
    #[arg(long, value_enum, default_value_t = SortArg::Relevance)]
    pub sort: SortArg,

    /// Snippet length, in full-text tokens. Longer snippets cost nothing extra — the text
    /// comes from the index — but `--full` is still what lets a long one through
    /// untruncated, because the display width caps it either way.
    #[arg(long, default_value_t = kiomon_core::DEFAULT_SNIPPET_TOKENS, value_name = "TOKENS", value_parser = clap::value_parser!(u16).range(1..=64))]
    pub snippet_len: u16,

    /// Do not count the retrieval against each result's decay clock.
    #[arg(long)]
    pub no_record: bool,
}

/// `kiomon get`
#[derive(Debug, Args)]
pub struct GetArgs {
    /// The memory id, as returned by `search` or `list`.
    pub id: String,

    /// Also show the memory's graph neighbours. Costs one extra query.
    #[arg(long)]
    pub relations: bool,
}

/// `kiomon list`
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Restrict to these kinds. Repeatable or comma-separated.
    #[arg(long, value_enum, value_delimiter = ',', value_name = "KIND")]
    pub kind: Vec<KindArg>,

    /// Restrict to one lifecycle status. Defaults to `active`.
    #[arg(long, value_enum, value_name = "STATUS")]
    pub status: Option<StatusArg>,

    /// Only memories carrying this tag. Repeatable.
    #[arg(long, value_name = "TAG")]
    pub tag: Vec<String>,

    /// Workspace to list. `*` lists every workspace.
    #[arg(long, value_name = "WORKSPACE")]
    pub workspace: Option<String>,

    /// Maximum rows.
    #[arg(long, default_value_t = 20, value_name = "N")]
    pub limit: u16,

    /// Newest first, instead of the default strength ordering.
    #[arg(long)]
    pub recent: bool,
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// `kiomon review` — the inbox that keeps unapproved agent writes out of retrieval.
#[derive(Debug, Args)]
pub struct ReviewArgs {
    #[command(subcommand)]
    pub command: ReviewCommand,
}

/// The three things you can do to the review inbox.
#[derive(Debug, Subcommand)]
pub enum ReviewCommand {
    /// Show what is waiting.
    List(ReviewListArgs),
    /// Activate drafts: index them, link them, and make them retrievable.
    Approve(IdsArgs),
    /// Archive drafts without activating them.
    Reject(IdsArgs),
}

/// `kiomon review list`
#[derive(Debug, Args)]
pub struct ReviewListArgs {
    /// Only drafts in this workspace.
    #[arg(long, value_name = "WORKSPACE")]
    pub workspace: Option<String>,
    /// Maximum rows.
    #[arg(long, default_value_t = 20, value_name = "N")]
    pub limit: u16,
    /// Only drafts at or above this confidence.
    #[arg(long, value_name = "0.0-1.0")]
    pub min_confidence: Option<f64>,
}

/// `kiomon outcome`
#[derive(Debug, Args)]
pub struct OutcomeArgs {
    /// How the task ended.
    #[arg(value_enum)]
    pub outcome: OutcomeArg,

    /// `ID:ASSESSMENT`, repeatable. Assessments: helpful, unused, misleading, outdated.
    #[arg(long = "item", value_name = "ID:ASSESSMENT", required = true)]
    pub items: Vec<OutcomeItem>,

    /// Workspace the memories belong to. Defaults to the configured workspace.
    #[arg(long, value_name = "WORKSPACE")]
    pub workspace: Option<String>,
}

/// `kiomon decay`
#[derive(Debug, Args)]
pub struct DecayArgs {
    /// Print what would change and write nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Apply decay to every workspace, not just the configured one.
    #[arg(long)]
    pub all_workspaces: bool,
}

/// `kiomon rate`
#[derive(Debug, Args)]
pub struct RateArgs {
    /// The memory to rate.
    pub id: String,
    /// `+1` if it helped, `-1` if it did not.
    ///
    /// `allow_hyphen_values` is required: without it clap reads `-1` as an unknown
    /// flag and the one value the command exists to accept is rejected as invalid
    /// syntax.
    #[arg(value_enum, allow_hyphen_values = true)]
    pub rating: RatingArg,
}

/// `kiomon reindex`
///
/// What it reindexes is decided by flags rather than by a sub-subcommand: there is one
/// index that can fall behind (`--embed`), and a `reindex embed` spelling would be a verb
/// whose only value is a word.
#[derive(Debug, Args)]
pub struct ReindexArgs {
    /// Re-embed every active memory that has no vector for the configured model.
    #[arg(long)]
    pub embed: bool,

    /// Workspace to cover. `*` covers every workspace.
    #[arg(long, value_name = "WORKSPACE")]
    pub workspace: Option<String>,

    /// Report what would be embedded, and embed nothing.
    #[arg(long)]
    pub dry_run: bool,

    /// Memories per batch. Each batch is one embed call per `batch` the embedder
    /// configures, and one transaction.
    #[arg(long, default_value_t = 500, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=10_000))]
    pub limit: u32,
}

/// `kiomon forget`
#[derive(Debug, Args)]
pub struct ForgetArgs {
    /// The memories to delete permanently.
    #[arg(required = true)]
    pub ids: Vec<String>,

    /// Confirm the deletion. Without it the command refuses and writes nothing —
    /// it never prompts, because a prompt is unusable from a script or an agent.
    #[arg(long)]
    pub yes: bool,
}

// ---------------------------------------------------------------------------
// Portability
// ---------------------------------------------------------------------------

/// `kiomon export`
#[derive(Debug, Args)]
pub struct ExportArgs {
    /// The output format.
    #[arg(long, value_enum, default_value_t = FormatArg::Jsonl)]
    pub format: FormatArg,

    /// Workspace to export. `*` exports every workspace.
    #[arg(long, value_name = "WORKSPACE")]
    pub workspace: Option<String>,

    /// Only memories with this status. Defaults to `active`.
    #[arg(long, value_enum, value_name = "STATUS")]
    pub status: Option<StatusArg>,

    /// Write here instead of standard output.
    #[arg(long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

/// Pinned, or any single memory by id. The verbs that take a list of ids share this.
#[derive(Debug, Args)]
pub struct IdsArgs {
    /// The memories to act on.
    #[arg(required = true)]
    pub ids: Vec<String>,
}

// ---------------------------------------------------------------------------
// Value enums. Each one exists so a bad value is rejected by the parser, with the
// valid ones listed, before anything is opened or written.
// ---------------------------------------------------------------------------

/// A memory kind, as a command-line value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum KindArg {
    /// Files: imported or saved content.
    Reference,
    /// Facts: knowledge, facts, relationships.
    Semantic,
    /// Routines: how-to rules, triggers, preferences.
    Procedural,
    /// Moments: something that happened, with a timestamp.
    Episodic,
}

impl From<KindArg> for MemoryKind {
    fn from(arg: KindArg) -> Self {
        match arg {
            KindArg::Reference => Self::Reference,
            KindArg::Semantic => Self::Semantic,
            KindArg::Procedural => Self::Procedural,
            KindArg::Episodic => Self::Episodic,
        }
    }
}

/// A lifecycle status, as a command-line value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum StatusArg {
    /// Indexed and retrievable.
    Active,
    /// Waiting in the review inbox.
    PendingApproval,
    /// Hidden from retrieval, restorable.
    Archived,
    /// Every status at once. Needed by `export` and by an audit; never the default.
    Any,
}

impl From<StatusArg> for Option<Status> {
    fn from(arg: StatusArg) -> Self {
        match arg {
            StatusArg::Active => Some(Status::Active),
            StatusArg::PendingApproval => Some(Status::PendingApproval),
            StatusArg::Archived => Some(Status::Archived),
            StatusArg::Any => None,
        }
    }
}

/// A graph relation, as a command-line value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RelationArg {
    /// Derived from.
    DerivedFrom,
    /// Supports.
    Supports,
    /// Contradicts.
    Contradicts,
    /// Related to. Undirected.
    RelatedTo,
    /// Happened before.
    Precedes,
    /// Happened after.
    Follows,
    /// Is a component of.
    PartOf,
}

impl From<RelationArg> for Relation {
    fn from(arg: RelationArg) -> Self {
        match arg {
            RelationArg::DerivedFrom => Self::DerivedFrom,
            RelationArg::Supports => Self::Supports,
            RelationArg::Contradicts => Self::Contradicts,
            RelationArg::RelatedTo => Self::RelatedTo,
            RelationArg::Precedes => Self::Precedes,
            RelationArg::Follows => Self::Follows,
            RelationArg::PartOf => Self::PartOf,
        }
    }
}

/// Which retrieval legs to run.
///
/// The hosted spellings are accepted as aliases — `hybrid` and `both` for `auto`,
/// `lexical` for `fts`, `semantic` for `vector` — because a caller that read `legs` off a
/// hosted response asks for it back by the name it saw, and a usage error for a word the
/// other half of this product uses would be a gratuitous difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LegsArg {
    /// Every leg this build has: full text, plus the vector leg when an embedder is
    /// configured. `--legs hybrid` and `--legs both` are the same thing.
    #[value(alias = "hybrid", alias = "both")]
    Auto,
    /// Full text only. Never touches an embedder, so it is the fast mode.
    #[value(alias = "lexical", alias = "fulltext")]
    Fts,
    /// Vector only. Fails with exit code 4 when no embedder is configured, and never
    /// answers from the lexical leg.
    #[value(alias = "semantic")]
    Vector,
    /// The graph leg, which this release does not have (§9-D9).
    Graph,
}

impl From<LegsArg> for Legs {
    fn from(arg: LegsArg) -> Self {
        match arg {
            LegsArg::Auto => Self::Auto,
            LegsArg::Fts => Self::Fts,
            LegsArg::Vector => Self::Vector,
            LegsArg::Graph => Self::Graph,
        }
    }
}

impl LegsArg {
    /// The wire value, as reported in `--json` next to the legs that actually ran.
    ///
    /// `--legs auto` resolving to `fts` is the documented default without an embedder,
    /// not a degradation — and a caller can only tell the two apart if it sees both what
    /// it asked for and what it got.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Fts => "fts",
            Self::Vector => "vector",
            Self::Graph => "graph",
        }
    }
}

/// What the caller is about to do, for `retrieve --intent`.
///
/// The five values are the wire vocabulary, spelled once in `kiomon_core::Intent`; this
/// enum exists so clap can validate the flag and print the choices. There is no `understand`
/// and no `recall`: those were in an early draft of the plan and nothing accepts them (see
/// §8's WP5.5 record).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum IntentArg {
    /// Carry out an action or a routine.
    Execute,
    /// Decompose a task.
    Plan,
    /// Answer a question about the user.
    Answer,
    /// Check a factual claim.
    Verify,
    /// Recover from a failure.
    Debug,
}

impl From<IntentArg> for kiomon_core::Intent {
    fn from(arg: IntentArg) -> Self {
        match arg {
            IntentArg::Execute => Self::Execute,
            IntentArg::Plan => Self::Plan,
            IntentArg::Answer => Self::Answer,
            IntentArg::Verify => Self::Verify,
            IntentArg::Debug => Self::Debug,
        }
    }
}

/// How to order results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SortArg {
    /// The fused rank. Pinned first.
    Relevance,
    /// Newest first.
    Recent,
    /// Strongest first — the decay clock's opinion.
    Strength,
}

impl From<AssessmentArg> for kiomon_core::lifecycle::Assessment {
    fn from(arg: AssessmentArg) -> Self {
        match arg {
            AssessmentArg::Helpful => Self::Helpful,
            AssessmentArg::Unused => Self::Unused,
            AssessmentArg::Misleading => Self::Misleading,
            AssessmentArg::Outdated => Self::Outdated,
        }
    }
}

impl From<RatingArg> for kiomon_core::lifecycle::Rating {
    fn from(arg: RatingArg) -> Self {
        match arg {
            RatingArg::Up => Self::Up,
            RatingArg::Down => Self::Down,
        }
    }
}

impl From<OutcomeArg> for kiomon_core::lifecycle::TaskOutcome {
    fn from(arg: OutcomeArg) -> Self {
        match arg {
            OutcomeArg::Success => Self::Success,
            OutcomeArg::Failure => Self::Failure,
        }
    }
}

impl From<SortArg> for SearchSort {
    fn from(arg: SortArg) -> Self {
        match arg {
            SortArg::Relevance => Self::Relevance,
            SortArg::Recent => Self::Recent,
            SortArg::Strength => Self::Strength,
        }
    }
}

/// How a task ended, for `outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutcomeArg {
    /// It worked.
    Success,
    /// It did not.
    Failure,
}

/// An export format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FormatArg {
    /// One JSON object per line. What `import` reads.
    Jsonl,
    /// One markdown file per memory, for reading.
    Markdown,
}

/// A rating: useful or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RatingArg {
    /// It helped.
    #[value(name = "+1", alias = "1", alias = "up", alias = "useful")]
    Up,
    /// It did not.
    #[value(name = "-1", alias = "down", alias = "not-useful")]
    Down,
}

/// What an agent thought of a memory it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AssessmentArg {
    /// Used, and it helped.
    Helpful,
    /// Retrieved, and not used.
    Unused,
    /// Used, and it caused a wrong result.
    Misleading,
    /// Stale or expired.
    Outdated,
}

/// One `--item` of `kiomon outcome`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeItem {
    /// The memory the agent was given.
    pub memory_id: String,
    /// How it performed.
    pub assessment: AssessmentArg,
}

impl FromStr for OutcomeItem {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Split on the last colon so an id that contains one still parses; ids are
        // generated by this tool and do not, but a hand-written import file might,
        // and silently reading the wrong half of a split is worse than accepting it.
        let (id, assessment) = s.rsplit_once(':').ok_or_else(|| {
            format!("expected ID:ASSESSMENT, got `{s}` — for example `k_01JC9W5K2Q:helpful`")
        })?;
        let memory_id = id.trim();
        if memory_id.is_empty() {
            return Err(format!("`{s}` has an empty memory id"));
        }
        let assessment = match assessment.trim().to_ascii_lowercase().as_str() {
            "helpful" => AssessmentArg::Helpful,
            "unused" => AssessmentArg::Unused,
            "misleading" => AssessmentArg::Misleading,
            "outdated" => AssessmentArg::Outdated,
            other => {
                return Err(format!(
                    "`{other}` is not an assessment — expected helpful, unused, misleading or outdated"
                ));
            }
        };
        Ok(Self {
            memory_id: memory_id.to_owned(),
            assessment,
        })
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn the_parser_is_internally_consistent() {
        // clap's own check: duplicate flags, a group naming a missing argument, a
        // subcommand that shadows another. This catches the whole class at once.
        Cli::command().debug_assert();
    }

    #[test]
    fn an_outcome_item_splits_on_the_last_colon() {
        let item: OutcomeItem = "k_01JC9W5K2Q:helpful".parse().unwrap();
        assert_eq!(item.memory_id, "k_01JC9W5K2Q");
        assert_eq!(item.assessment, AssessmentArg::Helpful);
    }

    #[test]
    fn a_malformed_outcome_item_lists_the_valid_assessments() {
        let err = "k_01JC9W5K2Q:brilliant".parse::<OutcomeItem>().unwrap_err();
        for valid in ["helpful", "unused", "misleading", "outdated"] {
            assert!(err.contains(valid), "{err}");
        }
        let err = "k_01JC9W5K2Q".parse::<OutcomeItem>().unwrap_err();
        assert!(err.contains("ID:ASSESSMENT"), "{err}");
        let err = ":helpful".parse::<OutcomeItem>().unwrap_err();
        assert!(err.contains("empty memory id"), "{err}");
    }

    #[test]
    fn every_arg_enum_maps_onto_the_domain_enum() {
        // The two vocabularies are separate types so that `kiomon-core` never depends
        // on clap. That seam is only safe if every value actually maps.
        for kind in [
            KindArg::Reference,
            KindArg::Semantic,
            KindArg::Procedural,
            KindArg::Episodic,
        ] {
            assert_eq!(
                MemoryKind::from(kind)
                    .as_str()
                    .parse::<MemoryKind>()
                    .unwrap(),
                MemoryKind::from(kind)
            );
        }
        for relation in [
            RelationArg::DerivedFrom,
            RelationArg::Supports,
            RelationArg::Contradicts,
            RelationArg::RelatedTo,
            RelationArg::Precedes,
            RelationArg::Follows,
            RelationArg::PartOf,
        ] {
            let domain = Relation::from(relation);
            assert_eq!(domain.as_str().parse::<Relation>().unwrap(), domain);
        }
        for status in [
            StatusArg::Active,
            StatusArg::PendingApproval,
            StatusArg::Archived,
        ] {
            let domain = Option::<Status>::from(status).expect("not Any");
            assert_eq!(domain.as_str().parse::<Status>().unwrap(), domain);
        }
        assert_eq!(Option::<Status>::from(StatusArg::Any), None);
    }

    #[test]
    fn a_workspace_that_was_not_asked_for_is_not_star() {
        // The single most consequential default in the parser: absence of
        // `--workspace` must never behave like `--workspace '*'`.
        let cli = Cli::try_parse_from(["kiomon", "search", "anything"]).unwrap();
        let Command::Search(args) = cli.command else {
            panic!("expected the search subcommand");
        };
        assert_eq!(args.workspace, None);
    }

    #[test]
    fn a_rating_accepts_its_hyphenated_spelling() {
        // `-1` is the one value in the whole surface that begins with a hyphen, so it
        // is the one that needs `allow_hyphen_values`; assert it, because the failure
        // is a parse error rather than a wrong number.
        let cli = Cli::try_parse_from(["kiomon", "rate", "k_01JC9W5K2Q", "-1"]).unwrap();
        let Command::Rate(args) = cli.command else {
            panic!("expected the rate subcommand");
        };
        assert_eq!(args.rating, RatingArg::Down);

        let cli = Cli::try_parse_from(["kiomon", "rate", "k_01JC9W5K2Q", "+1"]).unwrap();
        let Command::Rate(args) = cli.command else {
            panic!("expected the rate subcommand");
        };
        assert_eq!(args.rating, RatingArg::Up);
    }

    #[test]
    fn quiet_has_a_short_form_and_json_does_not() {
        // `-q` is muscle memory; `-j` is not, and a short flag nobody guesses is a
        // short flag that collides with a future one for no gain.
        let cli = Cli::try_parse_from(["kiomon", "-q", "stats"]).unwrap();
        assert!(cli.quiet);
        assert!(!cli.json);
        assert!(Cli::try_parse_from(["kiomon", "-j", "stats"]).is_err());
    }
}
