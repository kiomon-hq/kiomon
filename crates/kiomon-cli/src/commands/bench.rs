// SPDX-License-Identifier: Apache-2.0

//! `bench`: the ruler.
//!
//! Every performance claim in this project is read off this command or it is not made.
//! The plan puts the ruler before the optimizations for a reason that has now been earned
//! twice: WP1 found two defects that only existed at scale, and WP2 found a 34 ms one
//! that only existed because a query was measured end to end rather than reasoned about.
//!
//! # What it measures, and what it refuses to
//!
//! - **Cold process → exit.** Every sample is a real spawn of this binary, because that
//!   is what a user pays. An in-process microbenchmark answers a different question, and
//!   `crates/kiomon-core/benches/` has those.
//! - **Work, not wall time.** Spawning a process at all costs about 9 ms on Windows and
//!   about 1 ms on Linux, so a budget compared across machines without subtracting it is
//!   measuring the operating system. The spawn floor is measured first and every row is
//!   reported twice: `total`, and `work` = total − floor. **The plan's gates read
//!   `work`**, because `open ≤ 5 ms` is unreachable on Windows on `total` alone — the
//!   process would have to spawn faster than it can spawn.
//! - **The corpus's shape**, reported before anything is timed: how many bodies are
//!   short, medium and long, and how many rows each probe query actually matches. A query
//!   that matches everything measures a scan, and WP2 shipped one of those by accident.
//! - **The volume's path**, because it is a measurement parameter. WP2's worst afternoon
//!   was a forty-fold difference in write cost between two disks holding the same file.
//!
//! # Why it ships in the product
//!
//! A benchmark nobody can run is a claim nobody can check. `kiomon bench` is in the
//! released binary so that "search is 26 ms at ten thousand memories" is a sentence a
//! reader can falsify on their own machine instead of one they have to take on faith. It
//! is the only command that exists for this project's benefit rather than a user's, and
//! `docs/CONTRACT.md` says so in those words.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use kiomon_core::bench::{CorpusReport, CorpusSpec, PLANTED};
use kiomon_core::config;
use kiomon_core::{Database, Error, ExitCode, Result};

use crate::cli::{BenchArgs, Cli};
use crate::out::{self, Sample};

/// The sizes measured when none are named.
///
/// Ten thousand is where the plan's gates are read, so it is always in the set. One
/// thousand is there so a regression can be seen to be a scaling regression rather than a
/// constant one — a number without a smaller number beside it cannot tell the difference.
const DEFAULT_SIZES: &[usize] = &[1_000, 10_000];

/// The gate the plan defines and this build cannot express.
///
/// The hybrid gate left this list in WP7: the harness configures a stub embedder, so the
/// `common (hybrid)` row measures the real code path and the gate reads it. What a stub
/// cannot measure is a *model's* latency — which is not this program's number and not
/// something CI could bound anyway.
const DEFERRED_GATE: &str =
    "search p95 (hybrid) against a real model — the harness uses a stub, so 
                              what is measured is this program's work and not the model's";

/// `kiomon bench`
///
/// # Errors
///
/// Returns [`Error::Config`] if a corpus directory cannot be created or a corpus cannot
/// be built, and [`Error::Internal`] if this binary cannot find or spawn itself — the one
/// thing the harness cannot work around, because the harness *is* the thing being
/// measured.
pub fn bench(cli: &Cli, args: &BenchArgs) -> Result<ExitCode> {
    if args.open {
        return open_probe(cli);
    }

    let exe = std::env::current_exe().map_err(|error| {
        Error::Internal(format!(
            "the harness measures this binary by running it, and cannot find it: {error}"
        ))
    })?;
    let dir = corpus_dir(args)?;
    let sizes = sizes(args)?;

    // The harness gives every command it runs one stub embedder, and the corpora are
    // built with it, for two reasons. The gate the plan sets for this work package is a
    // *hybrid* search budget, and CI has no model to embed with; and a row measured with an
    // embedder configured is the row that shows what the vector leg costs, which is the
    // number a user with Ollama actually pays. The stub is a hashed bag of words — no
    // network, microseconds per thousand texts, deterministic — so what these rows measure
    // is this program's work and not a model's, which is what the budget is about.
    let config = stub_config(&dir)?;

    let mut reports = Vec::with_capacity(sizes.len());
    for size in sizes {
        let path = dir.join(format!("brain-{size}.db"));
        if !cli.quiet {
            eprintln!(
                "kiomon bench: {size} memories in {} — building it if it is not already built",
                path.display()
            );
        }
        let db = Database::open(&path)?;
        let mut corpus = kiomon_core::bench::generate(&db, &CorpusSpec::new(size), args.rebuild)?;
        drop(db);
        // A corpus that was built before this work package exists has no vectors, and one
        // built by an earlier run of this harness in the same directory is reused rather
        // than rebuilt. Either way the vector leg only sees what is there, so the gap is
        // filled once — through the same `reindex --embed` a user would run, which is also
        // how the harness keeps that command exercised.
        corpus.vectors = embed_corpus(&exe, &config, &corpus, cli.quiet)?;
        reports.push(measure(&exe, &config, &corpus, args)?);
    }

    let passed = reports
        .iter()
        .all(|report| report.gates.iter().all(|gate| gate.passed));
    out::emit(
        cli.json,
        out::BenchResponse::new(reports, &dir, exe_bytes()),
        |response| {
            print!("{}", out::render_bench(response));
        },
    )?;

    if args.gate && !passed {
        // Exit 1 rather than a new code: `doctor` already spends 1 on "the run finished
        // and the answer is no", and CI has to be able to fail on this.
        return Ok(ExitCode::Internal);
    }
    Ok(ExitCode::Success)
}

/// Open the database, say what the platform offers about it, and exit.
///
/// Writes nothing on the human path: whatever this prints is inside the measurement when
/// the harness times it, and a print at ten thousand rows is a measurement of the print.
///
/// # Errors
///
/// Returns whatever [`Database::open`] and [`Database::schema_version`] raise.
fn open_probe(cli: &Cli) -> Result<ExitCode> {
    let db = Database::open(config::resolve_db_path(cli.db.as_deref())?)?;
    if cli.json {
        let probe = out::OpenProbe {
            path: db.path().display().to_string(),
            schema_version: db.schema_version()?,
            peak_rss_kb: peak_rss_kb(),
        };
        out::emit(true, probe, |_| {})?;
    }
    Ok(ExitCode::Success)
}

/// The stub embedder this harness gives every command it runs.
///
/// Written beside the corpora rather than in the user's data directory: the harness must
/// not change how the user's own `kiomon search` behaves, and a config file it owns is a
/// file it can rewrite when the shape of the block changes.
///
/// # Errors
///
/// Returns [`Error::Io`] when the file cannot be written.
fn stub_config(dir: &Path) -> Result<PathBuf> {
    let path = dir.join("bench-config.toml");
    // `default_workspace` is deliberately absent: a probe passes `--db` and lets the
    // workspace default to `default`, which is what the corpus was generated into.
    let text = r#"# Written by `kiomon bench`. The stub embedder has no model and no network:
# its vectors are a hashed bag of words, so what the hybrid rows measure is
# this program's work and not a model's.
[embedder]
provider = "stub"
"#;
    std::fs::write(&path, text)?;
    Ok(path)
}

/// Give the corpus vectors for the stub model, and report how many it wrote.
///
/// Through `reindex --embed`, which is the command a user runs: a harness that embedded
/// rows itself would be measuring a code path that does not exist in production, and a
/// harness that skipped the step would report a hybrid row with nothing in the vector leg.
///
/// # Errors
///
/// Returns [`Error::Internal`] when the command exits without a JSON report, which means
/// the harness cannot know what it measured.
fn embed_corpus(
    exe: &Path,
    config: &Path,
    corpus: &kiomon_core::CorpusReport,
    quiet: bool,
) -> Result<usize> {
    // `--config` is prepended by the runner, so it is not repeated here.
    let args = vec![
        "--db".to_owned(),
        corpus.path.display().to_string(),
        "reindex".to_owned(),
        "--embed".to_owned(),
        "--workspace".to_owned(),
        "*".to_owned(),
        "--json".to_owned(),
    ];
    let config = config.display().to_string();
    let (elapsed, out) = one_capturing(exe, &config, &args, true)?;
    let value: serde_json::Value = serde_json::from_str(out.trim()).map_err(|error| {
        Error::Internal(format!(
            "`kiomon reindex --embed` printed no JSON report, so the harness cannot tell \
             how many vectors the corpus has: {error}"
        ))
    })?;
    let written = value["data"]["embedded"].as_u64().unwrap_or(0);
    if !quiet && written > 0 {
        eprintln!(
            "kiomon bench: embedded {written} memories for the vector leg in {elapsed:.1} ms"
        );
    }
    Ok(corpus
        .vectors
        .saturating_add(usize::try_from(written).unwrap_or(0)))
}

/// Where the corpora live.
///
/// The temp directory by default, which is on the system volume. A corpus is a large file
/// that the benchmark then reads and writes, so putting it beside the source tree would
/// make every number a property of whichever disk the checkout happens to be on.
///
/// # Errors
///
/// Returns [`Error::Config`] if the directory cannot be created.
fn corpus_dir(args: &BenchArgs) -> Result<PathBuf> {
    let dir = args
        .corpus
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("kiomon-bench"));
    std::fs::create_dir_all(&dir).map_err(|error| {
        Error::Config(format!(
            "the corpus goes in {}, which cannot be created: {error}",
            dir.display()
        ))
    })?;
    Ok(dir)
}

/// The sizes to measure, de-duplicated and ascending.
///
/// # Errors
///
/// Returns [`Error::Invalid`] for a size of zero, which is a corpus that would measure a
/// database's response to being empty.
fn sizes(args: &BenchArgs) -> Result<Vec<usize>> {
    let mut sizes = if args.sizes.is_empty() {
        DEFAULT_SIZES.to_vec()
    } else {
        args.sizes.clone()
    };
    sizes.sort_unstable();
    sizes.dedup();
    if sizes.contains(&0) {
        return Err(Error::Invalid(
            "a corpus of zero memories would measure nothing; pass --sizes with a size".to_owned(),
        ));
    }
    Ok(sizes)
}

/// Measure one corpus, `--repeat` times.
fn measure(
    exe: &Path,
    config: &Path,
    corpus: &CorpusReport,
    args: &BenchArgs,
) -> Result<out::BenchReport> {
    // One pass is run and thrown away before the measured ones. Building the corpus
    // writes two hundred megabytes and then checkpoints the write-ahead log back into the
    // file, and the kernel does not finish flushing it for another minute — a command
    // measured inside that minute is competing with its own benchmark's setup. It showed
    // up as the `add` row's median being five times its own later passes, 17.1 ms against
    // 3.5 and 4.1, which reads as an unstable command and is an unflushed corpus.
    //
    // Discarded rather than reported, because its samples are *known* to be contaminated:
    // a number that is known to be wrong belongs in the bin and not in an average. Every
    // figure this harness publishes is therefore a warm-cache figure, which is what the
    // plan's numbers claim to be.
    let (_, _, mut written) = measure_once(exe, config, corpus, args)?;
    let mut passes = Vec::with_capacity(args.repeat as usize);
    let mut floor_samples = Vec::new();
    for _ in 0..args.repeat {
        let (rows, floor, added) = measure_once(exe, config, corpus, args)?;
        passes.push(rows);
        floor_samples.extend(floor.samples);
        written.extend(added);
    }
    // The corpus's rows are put back the way they were found, after every pass rather than
    // between them: forgetting two hundred memories is a write burst, and a burst in the
    // middle of the suite would land on whichever row happened to run next. The *file* keeps
    // the freed pages — a corpus that has been measured a few times is a few percent larger
    // than a fresh one — which is why the size check is on rows and not on bytes.
    if !written.is_empty() {
        let db = Database::open(&corpus.path)?;
        db.forget(&written)?;
        drop(db);
    }
    let floor = Sample::from(floor_samples);
    let (rows, gates) = out::merge_passes(&passes);
    Ok(out::BenchReport {
        corpus: corpus.clone(),
        spawn_floor: floor,
        rows,
        gates,
        peak_rss_kb: peak_rss_kb(),
        rejected: rejected_names(),
    })
}

/// One command to measure: what it is called, what it asks, and how to run it.
struct Probe {
    /// The row's name in the report.
    label: &'static str,
    /// The query, for a search.
    query: Option<String>,
    /// How many rows the query matches.
    matches: i64,
    /// The arguments for sample `n`, or `None` for a row that takes one sample per pass.
    ///
    /// `n` is there because some commands must not be asked the same question twice:
    /// `add` dedupes, and a deduplicated add is a lookup rather than a write.
    args: Box<dyn Fn(usize) -> Option<Vec<String>>>,
    /// Whether this row writes a memory into the corpus, whose id has to come back.
    ///
    /// The `add` row measures writing into the corpus, which means the corpus grows by one
    /// memory per sample. Nothing removes those on its own, and a corpus that drifts by two
    /// hundred rows per run stops being the corpus the report describes — and stops being
    /// reusable, because `generate` matches on size. The id is read back from `--json` and
    /// the memory is forgotten afterwards.
    ///
    /// Reading the id back costs this row a pipe and a JSON parse. That is a real cost, and
    /// it is the price of a corpus that stays the corpus; the alternative — a separate
    /// scratch database — would measure `add` against a brain that is not the one every
    /// other row is measured against.
    collect_ids: bool,
}

/// Measure one corpus, once.
///
/// # Sample-major, and why
///
/// Every row's sample *n* is taken alongside every other row's sample *n*, and the spawn
/// floor for that index is measured immediately before them. The order is the whole point:
/// the machine this was developed on moves by more than the plan's entire reproducibility
/// budget over a single suite, so a floor measured once at the beginning describes a
/// machine that may not exist by the time the last row runs. Block-major measurement — all
/// of one row, then all of the next — makes every row a statement about a different minute.
///
/// It costs one extra spawn per sample *index* rather than per sample, so seventeen rows of
/// twenty samples pay twenty spawns for their floor and not three hundred and forty.
fn probes_for(corpus: &CorpusReport) -> Vec<Probe> {
    let mut probes = write_probes(corpus);
    probes.extend(search_probes(corpus));
    probes.extend(read_probes(corpus));
    probes
}

/// The writes: opening, the first write on this volume, and a plain `add`.
fn write_probes(corpus: &CorpusReport) -> Vec<Probe> {
    let db = corpus.path.display().to_string();
    let id = corpus.sample_ids.first().cloned().unwrap_or_default();
    let mut probes = Vec::new();

    // `open` alone, which is the number that decides whether a daemon is worth having: if
    // opening the database dominates every command, then the per-process design is the
    // problem and the query is not.
    probes.push(Probe {
        label: "open",
        query: None,
        matches: 0,
        collect_ids: false,
        args: Box::new({
            let db = db.clone();
            move |_| {
                Some(vec![
                    "--db".to_owned(),
                    db.clone(),
                    "bench".to_owned(),
                    "--open".to_owned(),
                    "--json".to_owned(),
                ])
            }
        }),
    });

    // The first write to a file that has never been written, on this volume, in a fresh
    // process. One sample, because it measures the disk and a disk does the same thing
    // every time. It is here because WP2 lost an afternoon to it, and because two machines
    // being compared deserve to see it. The file is removed while the arguments are being
    // built, so the sample really is a first write.
    probes.push(Probe {
        label: "add (cold file)",
        query: None,
        matches: 0,
        // Its own file, removed before every use, so the corpus has nothing to clean.
        collect_ids: false,
        args: Box::new({
            let cold = corpus.path.with_file_name("cold-file.db");
            move |sample| {
                if sample > 0 {
                    return None;
                }
                for suffix in ["", "-wal", "-shm"] {
                    let mut name = cold.clone().into_os_string();
                    name.push(suffix);
                    let _ = std::fs::remove_file(PathBuf::from(name));
                }
                Some(vec![
                    "--db".to_owned(),
                    cold.display().to_string(),
                    "add".to_owned(),
                    "--kind".to_owned(),
                    "semantic".to_owned(),
                    "--title".to_owned(),
                    "the first write on this volume".to_owned(),
                    "--body".to_owned(),
                    "written once, into a database that did not exist a moment ago".to_owned(),
                ])
            }
        }),
    });

    // `add` with content nobody has stored, because a deduplicated add is a lookup and the
    // gate is about writing.
    probes.push(Probe {
        label: "add",
        query: None,
        matches: 0,
        collect_ids: true,
        args: Box::new({
            let db = db.clone();
            let id = id.clone();
            move |sample| {
                Some(vec![
                    "--db".to_owned(),
                    db.clone(),
                    "add".to_owned(),
                    "--json".to_owned(),
                    "--kind".to_owned(),
                    "semantic".to_owned(),
                    "--title".to_owned(),
                    format!("measured write {sample} of {id}"),
                    "--body".to_owned(),
                    "A body of a realistic length for a note, written by the harness, and never \
                     written twice because a duplicate is not a write."
                        .to_owned(),
                ])
            }
        }),
    });

    probes
}

/// One row per planted term, so each row says what its query had to rank.
fn search_probes(corpus: &CorpusReport) -> Vec<Probe> {
    let db = corpus.path.display().to_string();
    let mut probes = Vec::new();
    // Every planted term, so each row says what it had to rank. The queries span four
    // orders of magnitude of selectivity, and a ranking that is fine over four rows is not
    // automatically fine over fifteen thousand.
    for planted in PLANTED {
        let query = planted.query().to_owned();
        let matches = corpus
            .probes
            .iter()
            .find(|probe| probe.name == planted.name)
            .map_or(0, |probe| probe.matches);
        // `auto`, which with the harness's stub embedder is both legs — the mix a user
        // who configured a model actually gets. The lexical-only and vector-only twins are
        // added below, and the set is the point: a hybrid row that drifts is either one leg
        // getting slower or the fusion getting slower, and only the separate rows say which.
        probes.push(Probe {
            label: match planted.name {
                "common" => "common (hybrid)",
                "uncommon" => "uncommon",
                "rare" => "rare",
                "phrase" => "phrase",
                "cjk" => "cjk",
                "accented" => "accented",
                _ => "absent",
            },
            query: Some(query.clone()),
            matches,
            collect_ids: false,
            args: Box::new({
                let db = db.clone();
                move |_| {
                    Some(vec![
                        "--db".to_owned(),
                        db.clone(),
                        "search".to_owned(),
                        "--quiet".to_owned(),
                        query.clone(),
                    ])
                }
            }),
        });
    }

    // The same query on one leg at a time, so "the vector leg costs X" and "the lexical leg
    // costs Y" are measurements rather than arithmetic on a sum.
    let common = corpus
        .probes
        .iter()
        .find(|probe| probe.name == "common")
        .map_or(0, |probe| probe.matches);
    for (label, leg) in [("common (fts)", "fts"), ("common (vector)", "vector")] {
        probes.push(Probe {
            label,
            query: Some("deploy".to_owned()),
            matches: common,
            collect_ids: false,
            args: Box::new({
                let db = db.clone();
                move |_| {
                    Some(vec![
                        "--db".to_owned(),
                        db.clone(),
                        "search".to_owned(),
                        "--quiet".to_owned(),
                        "--legs".to_owned(),
                        leg.to_owned(),
                        "deploy".to_owned(),
                    ])
                }
            }),
        });
    }

    probes
}

/// The reads: the same query without its retrieval record, the packet built from it, a
/// full page, and the two list orderings worth watching.
fn read_probes(corpus: &CorpusReport) -> Vec<Probe> {
    let db = corpus.path.display().to_string();
    // How many rows the `deploy` rows have to rank, which is the number that decides what
    // the rest of this table means.
    let common = corpus
        .probes
        .iter()
        .find(|probe| probe.name == "common")
        .map_or(0, |probe| probe.matches);
    let mut probes = Vec::new();
    // `retrieve` rides on the same `deploy` query as the search rows on purpose: a packet
    // is a search plus a re-rank, a clock read and an edge lookup, so the number to compare
    // it with is the search that fed it and not a number of its own. Its row is the guard
    // against the packet growing a per-hit query, which would show up here as a multiple
    // rather than as a few milliseconds.
    for (label, verb, extra, query) in [
        (
            "search (no record)",
            "search",
            vec!["--no-record", "deploy"],
            true,
        ),
        (
            "search (limit 50)",
            "search",
            vec!["--no-record", "--limit", "50", "deploy"],
            true,
        ),
        // Lexical on purpose: this row is WP5.5's gate ("a packet costs what the search
        // under it costs"), and a comparison is only a comparison if both halves run the
        // same legs. `retrieve (hybrid)` is the row a configured user actually gets.
        (
            "retrieve (no record)",
            "retrieve",
            vec![
                "--no-record",
                "--legs",
                "fts",
                "--intent",
                "execute",
                "deploy",
            ],
            true,
        ),
        (
            "retrieve (hybrid)",
            "retrieve",
            vec!["--no-record", "--intent", "execute", "deploy"],
            true,
        ),
        ("list", "list", vec![], false),
        ("list --recent", "list", vec!["--recent"], false),
        ("list --full", "list", vec!["--full"], false),
    ] {
        let mut rest = vec![verb, "--quiet"];
        rest.extend(extra);
        probes.push(Probe {
            label,
            query: query.then(|| "deploy".to_owned()),
            matches: if query { common } else { 0 },
            collect_ids: false,
            args: Box::new({
                let db = db.clone();
                move |_| {
                    let mut arguments = vec!["--db".to_owned(), db.clone()];
                    arguments.extend(rest.iter().map(|arg| (*arg).to_owned()));
                    Some(arguments)
                }
            }),
        });
    }

    probes.extend(single_probes(corpus));

    probes
}

/// The rows that are one command with no query: `stats`, `get`, and the decay pass.
///
/// Split out of [`read_probes`] because they answer a different question — "what does
/// this command cost" rather than "what does this query cost over a corpus" — and
/// because a function that does two things is a function whose parts cannot be read.
fn single_probes(corpus: &CorpusReport) -> Vec<Probe> {
    let db = corpus.path.display().to_string();
    let id = corpus.sample_ids.first().cloned().unwrap_or_default();
    let mut probes = Vec::new();
    probes.push(Probe {
        label: "stats",
        query: None,
        matches: 0,
        collect_ids: false,
        args: Box::new({
            let db = db.clone();
            move |_| {
                Some(vec![
                    "--db".to_owned(),
                    db.clone(),
                    "stats".to_owned(),
                    "--quiet".to_owned(),
                ])
            }
        }),
    });
    probes.push(Probe {
        label: "get",
        query: None,
        matches: 0,
        collect_ids: false,
        args: Box::new({
            let db = db.clone();
            let id = id.clone();
            move |_| {
                Some(vec![
                    "--db".to_owned(),
                    db.clone(),
                    "get".to_owned(),
                    "--quiet".to_owned(),
                    id.clone(),
                ])
            }
        }),
    });
    // The graph read, over the corpus's own planted edges (`bench::plant_graph` writes
    // five neighbours per memory, which is the auto-link cap). One memory's neighbourhood,
    // so the row measures two index seeks and a handful of row lookups rather than a size
    // of graph — and it is on the corpus's sample id, the same one `get` reads, so the two
    // rows can be read against each other.
    probes.push(Probe {
        label: "relate",
        query: None,
        matches: 0,
        collect_ids: false,
        args: Box::new({
            let db = db.clone();
            let id = id.clone();
            move |_| {
                Some(vec![
                    "--db".to_owned(),
                    db.clone(),
                    "relate".to_owned(),
                    "--quiet".to_owned(),
                    id.clone(),
                ])
            }
        }),
    });
    // Last, because it is the most expensive row in the suite and because it is the one
    // that reads the whole corpus: running it earlier would put its page cache under
    // every row after it.
    //
    // Measured as a **dry run**, and that is a decision rather than a shortcut. A real
    // pass rewrites the strength of almost every row in the corpus — from 1.0 to
    // `importance` on the first pass — so `--repeat` would be comparing a corpus with the
    // one it become after being decayed. The read cost is what scales with the corpus,
    // and the write cost is `add`-shaped per row that actually moved, which is a property
    // of the corpus's age rather than of its size. `decay` itself is gated on correctness
    // (WP4's tests), not on a stopwatch.
    probes.push(Probe {
        label: "decay (dry run)",
        query: None,
        matches: 0,
        collect_ids: false,
        args: Box::new({
            let db = db.clone();
            move |_| {
                Some(vec![
                    "--db".to_owned(),
                    db.clone(),
                    "decay".to_owned(),
                    "--dry-run".to_owned(),
                    "--all-workspaces".to_owned(),
                    "--quiet".to_owned(),
                ])
            }
        }),
    });

    probes
}

/// Run every probe once per sample index, with the floor for that index measured first.
fn measure_once(
    exe: &Path,
    config: &Path,
    corpus: &CorpusReport,
    args: &BenchArgs,
) -> Result<(Vec<out::Row>, Sample, Vec<String>)> {
    let probes = probes_for(corpus);
    let config = config.display().to_string();
    let runs = args.runs as usize;
    let mut rows = probes
        .iter()
        .map(|probe| {
            out::Row::empty(
                display_label(probe.label),
                probe.query.clone(),
                probe.matches,
            )
        })
        .collect::<Vec<_>>();
    let mut floor_samples = Vec::with_capacity(runs);
    let mut written = Vec::new();
    for sample in 0..runs {
        // The floor for this sample index, taken moments before it. Every row's sample of
        // this index is measured against it.
        let floor = one(exe, &config, &["--version".to_owned()])?;
        floor_samples.push(floor);
        for (index, probe) in probes.iter().enumerate() {
            let Some(arguments) = (probe.args)(sample) else {
                continue;
            };
            let (elapsed, out) = one_capturing(exe, &config, &arguments, probe.collect_ids)?;
            rows[index].total_ms.push(elapsed);
            rows[index].floor_ms.push(floor);
            if probe.collect_ids {
                written.push(id_from(&out).ok_or_else(|| {
                    Error::Internal(format!(
                        "`kiomon {}` was measured writing a memory and its id could not be read back, so \
                         the corpus would keep a row nobody could remove",
                        arguments.join(" ")
                    ))
                })?);
            }
        }
    }
    for row in &mut rows {
        row.runs = row.total_ms.len();
        row.summarise();
    }
    Ok((rows, Sample::from(floor_samples), written))
}

/// The id out of an `add --json` response.
fn id_from(stdout: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    value["data"]["id"].as_str().map(str::to_owned)
}

/// The name a probe is reported under.
///
/// A search row carries the plant's name rather than the query, because that is what the
/// selectivity column beside it means: `search common` is the row that matched fifteen
/// thousand documents, and a reader comparing two machines needs to know that before
/// comparing their times.
fn display_label(label: &str) -> String {
    match label {
        "open"
        | "add"
        | "add (cold file)"
        | "stats"
        | "get"
        | "relate"
        | "decay (dry run)"
        | "retrieve (no record)"
        | "retrieve (hybrid)" => label.to_owned(),
        name if name.starts_with("list")
            || name.starts_with("search (")
            || name.ends_with("(fts)")
            || name.ends_with("(vector)")
            || name.ends_with("(hybrid)") =>
        {
            name.to_owned()
        }
        name => format!("search {name}"),
    }
}

/// One cold process, in milliseconds.
fn one(exe: &Path, config: &str, args: &[String]) -> Result<f64> {
    Ok(one_capturing(exe, config, args, false)?.0)
}

/// One cold process, in milliseconds, optionally keeping its stdout.
///
/// Nothing is captured unless a row asks for it, and that is not an optimisation: a
/// benchmark whose output goes nowhere is one fewer thing to go wrong, and the pipe read is
/// a cost the rows that ask for it really do pay.
fn one_capturing(
    exe: &Path,
    config: &str,
    args: &[String],
    capture: bool,
) -> Result<(f64, String)> {
    let started = Instant::now();
    let output = Command::new(exe)
        // The global flags first, so every row runs against the same database the same way:
        // with the harness's stub embedder configured. A row that ran without it would be a
        // row from a different program.
        .arg("--config")
        .arg(config)
        .args(args)
        // Nowhere on purpose. Piping the output would make the child block on a full
        // pipe, and reading it would make this a measurement of the harness; a benchmark
        // whose output nobody looks at is easier to trust than one that formats a table
        // and throws it away.
        .stdout(if capture {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            Error::Internal(format!(
                "the harness could not run {} {}: {error}",
                exe.display(),
                args.join(" ")
            ))
        })?;
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    if !output.status.success() {
        // A failure timed as success is the worst number in the report, because it looks
        // like the fastest one.
        return Err(Error::Internal(format!(
            "`kiomon {}` exited with {} while being measured, so the sample would have been a \
             measurement of a failure",
            args.join(" "),
            output
                .status
                .code()
                .map_or_else(|| "a signal".to_owned(), |code| code.to_string())
        )));
    }
    Ok((
        elapsed,
        String::from_utf8_lossy(&output.stdout).into_owned(),
    ))
}

/// The rows the plan asks this build for and this build cannot answer.
///
/// Reported rather than dropped, for the same reason the CLI's unimplemented commands name
/// their work package: a budget with a silent hole in it is a budget that looks better
/// than it is.
fn rejected_names() -> Vec<&'static str> {
    // WP5 removed the last row that was a *missing capability* (`relate` and the graph).
    // What is left is not a hole in the build: it is a number this program cannot own,
    // because a stub embedder has no model latency and CI has no model.
    vec![DEFERRED_GATE]
}

/// This process's peak resident set, where the platform makes it free.
///
/// Linux only, and `None` rather than a guess elsewhere: Windows needs a `psapi` call that
/// this workspace forbids (`unsafe_code = "forbid"`) and macOS a `libc` one that would
/// cost a dependency for a single number. It is in the report because the plan asks for
/// it, and because a memory budget with no memory number in it is not a budget — where
/// "this platform does not say" is a better answer than a zero.
fn peak_rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse().ok())
}

/// The size of the binary doing the measuring.
///
/// The plan budgets it, CI enforces it, and it belongs in the report because a fast
/// binary that costs six megabytes is a different trade from a slow one that costs one.
fn exe_bytes() -> Option<u64> {
    std::env::current_exe()
        .ok()
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|metadata| metadata.len())
}
