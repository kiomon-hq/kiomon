// SPDX-License-Identifier: Apache-2.0

//! In-process microbenchmarks: the inside of a command, not the command.
//!
//! `kiomon bench` measures a cold process end to end, which is what a user pays and what
//! the plan's budget is written in. This measures the *library* instead — the same
//! operations with the process, the spawn and the CLI left out — so a regression can be
//! attributed to a query rather than to a machine that got slower.
//!
//! # Why there is no Criterion here
//!
//! The plan names Criterion. It was measured before it was used, and it lost:
//!
//! - It would add **twenty-two crates** to the build — `regex`, `syn`, `ciborium`,
//!   `tinytemplate`, `walkdir` and the rest — for a project whose whole shipped dependency
//!   list is thirty-three. Criterion's own `cargo tree` is longer than the product's.
//! - `cargo test` compiles `benches/` targets, so those twenty-two would be compiled and
//!   linted on **three operating systems** in CI for benchmarks that CI does not run.
//! - Its statistical machinery answers a question this project is not asking. Criterion is
//!   built to detect a 2 % change in a microsecond-scale function; the plan's gates are
//!   cold-process budgets at tens of milliseconds, and every one of them is enforced
//!   end to end by `kiomon bench --gate`.
//!
//! What is here instead is sixty lines of nearest-rank percentiles and a warm loop. It has
//! no outlier detection and no confidence intervals, and the numbers it prints are the ones
//! it took. Where the difference matters — deciding whether a change is real — the answer
//! is to run `kiomon bench` three times, which is what the plan's reproducibility criterion
//! asks for anyway.
//!
//! Run it with `cargo bench -p kiomon-core`, or point it at a corpus built by
//! `kiomon bench`: `cargo bench -p kiomon-core -- "$TMPDIR/kiomon-bench/brain-10000.db"`.

use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

use kiomon_core::bench::{CorpusSpec, corpus_size, generate};
use kiomon_core::{Database, Legs, ListQuery, NewMemory, SearchQuery};

/// How many memories an in-memory corpus holds when none is named.
///
/// Five thousand, which is where WP1's defects appeared and where two of them were found
/// only because the bodies were kilobytes rather than bytes.
const DEFAULT_CORPUS: usize = 5_000;

/// How many times each operation runs. Enough for a median that is not an anecdote, small
/// enough that the whole file finishes in seconds.
const ITERATIONS: usize = 200;

fn main() {
    // `cargo bench` passes its own flags (`--bench`, and anything after `--`) to the target,
    // so the first argument that is not a flag is the corpus path.
    let requested = std::env::args().skip(1).find(|arg| !arg.starts_with('-'));
    let (db, label) = if let Some(path) = requested {
        let path = PathBuf::from(path);
        let db = Database::open(&path).expect("the corpus must open");
        let size = corpus_size(&db).expect("the corpus must be countable");
        (db, format!("{size} memories from {}", path.display()))
    } else {
        let db = Database::open_in_memory().expect("an in-memory database must open");
        generate(&db, &CorpusSpec::new(DEFAULT_CORPUS), false).expect("the corpus must build");
        (
            db,
            format!("{DEFAULT_CORPUS} generated memories, in memory"),
        )
    };

    println!("kiomon-core in-process bench — {label}\n");
    println!("  an in-process number is not a command's cost: it leaves out the process, the");
    println!("  spawn and the CLI, which together are most of what `kiomon bench` measures.\n");

    let searchable = SearchQuery {
        text: "deploy".to_owned(),
        legs: Legs::Fts,
        record: false,
        ..SearchQuery::default()
    };
    let id = db
        .list(&ListQuery {
            limit: 1,
            ..ListQuery::default()
        })
        .expect("the list must run")
        .first()
        .map(|memory| memory.id.clone())
        .unwrap_or_default();

    report("list (20)", ITERATIONS, || {
        black_box(db.list(&ListQuery::default()).expect("the list must run"));
    });
    if id.is_empty() {
        // An empty database has nothing to get, and a benchmark that panics on one is a
        // benchmark that cannot be pointed at a fresh file to check it is empty.
        println!("  {:<32} skipped: the database has no memories", "get");
    } else {
        report("get", ITERATIONS, || {
            black_box(db.get(&id).expect("the memory must exist"));
        });
    }
    // `--no-record`, so this is the query and not the query plus a write. The difference
    // between the two is a number worth knowing on its own, and `kiomon bench` reports it
    // end to end.
    report("search (rank and cut)", ITERATIONS, || {
        black_box(db.search(&searchable).expect("the search must run"));
    });
    report("search + retrievals recorded", ITERATIONS, || {
        black_box(
            db.search(&SearchQuery {
                record: true,
                ..searchable.clone()
            })
            .expect("the search must run"),
        );
    });
    report("stats", ITERATIONS, || {
        black_box(
            db.stats(&kiomon_core::Scope::All, &[])
                .expect("the counts must run"),
        );
    });
    report("add", ITERATIONS, || {
        let memory = NewMemory::new(kiomon_core::MemoryKind::Semantic, "an in-process add")
            .body("A body of a realistic length for a note, written by the benchmark.");
        black_box(db.add(&memory).expect("the add must run"));
    });
    // `fuse` is pure, so it can be measured without a database at all — and it is the one
    // operation whose cost is a function of the page size rather than of the corpus.
    let ranks: Vec<Vec<i64>> = (0..2)
        .map(|leg| (0..100_i64).map(|rank| rank * 3 + leg).collect())
        .collect();
    report("fuse (two legs, 100 each)", ITERATIONS * 20, || {
        black_box(kiomon_core::fuse(&ranks, 10));
    });
}

/// Time `operation` `iterations` times and print the median, the 95th percentile and the
/// fastest.
///
/// The fastest is printed beside the median because a microbenchmark's minimum is the
/// closest thing it has to "what this costs when the machine is not in the way", and a
/// median alone hides whether the tail is one interruption or a hundred.
fn report(label: &str, iterations: usize, mut operation: impl FnMut()) {
    // Warm, so the first allocation, the first statement compiled and the first page
    // faulted in are not the measurement.
    for _ in 0..iterations.min(20) {
        operation();
    }
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        operation();
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let (median, p95) = (
        samples[samples.len() / 2],
        samples[((samples.len() as f64 * 0.95).ceil() as usize)
            .saturating_sub(1)
            .min(samples.len() - 1)],
    );
    println!(
        "  {label:<32} p50 {:>8.3} ms   p95 {:>8.3} ms   min {:>8.3} ms   n {iterations}",
        median, p95, samples[0]
    );
}
