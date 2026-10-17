// SPDX-License-Identifier: Apache-2.0

//! The measurement corpus.
//!
//! This module exists for one reason: a performance number without a corpus is a
//! number without a meaning. The platform learned it first (*"a corpus + metric harness
//! first (the ruler)"*) and WP1 learned it again the hard way — a `list` ordering that
//! looked fine on thirty short notes was 30 ms slow on five thousand long ones, and a
//! `stats` query that looked fine was reading 42 MB of bodies to answer a question about
//! counts. Neither defect was visible until the corpus had the right *shape*.
//!
//! So the shape is the product here, and it is reported rather than assumed:
//!
//! - **Body size** follows the plan's mix — 60 % short (≤ 500 B), 30 % medium (≤ 8 KiB),
//!   10 % long (≤ 100 KiB) — and what a run actually built is reported alongside what it
//!   was asked for.
//! - **Vocabulary** is Zipf-distributed over four thousand generated words, because a
//!   corpus where every term matches every document cannot tell a ranking from a scan.
//!   WP2 measured that mistake: its first generator drew every body from twenty-nine
//!   words, so `deploy` matched all ten thousand memories.
//! - **Planted terms** with known document frequencies, and the *realised* count for each
//!   is measured from the index rather than computed from the recipe. A query is only
//!   worth timing if you can say how many rows it had to rank.
//!
//! The text is generated, not collected, and that is the weakest thing about it: a
//! generated corpus cannot model stemming, phrase breaks in real prose, or the
//! vocabulary a developer actually writes down. It can model size, distribution and
//! selectivity, which is what the budget is about.

use std::path::PathBuf;
use std::time::Instant;

use serde::Serialize;

use crate::db::Database;
use crate::error::{Error, Result};
use crate::model::{MemoryKind, NewMemory};

/// The largest body in the short class, in bytes.
pub const SHORT_MAX_BYTES: u64 = 500;
/// The largest body in the medium class, in bytes.
pub const MEDIUM_MAX_BYTES: u64 = 8 * 1024;
/// The largest body in the long class, in bytes.
pub const LONG_MAX_BYTES: u64 = 100 * 1024;

/// The seed that gives the corpus these numbers reproduce.
///
/// Fixed rather than random on purpose: two runs of two different builds have to be
/// measuring the same bytes, or a difference between them is a difference in the input.
pub const DEFAULT_SEED: u64 = 0x6B69_6F6D_6F6E_0001;

/// How many generated words the filler vocabulary holds.
///
/// Four thousand is where a Zipf distribution stops being dominated by its head: the
/// commonest word lands in most bodies, the ten-thousandth lands in a handful, and the
/// long tail is what makes `bm25()` work for its living.
const VOCABULARY: usize = 4_000;

/// How many words a planted term needs before it is worth timing.
///
/// The point of a planted term is to have a *known* selectivity. `deploy` in one body in
/// seven is the realistic case — the word everybody asks about — and `timeout` in one in
/// two hundred and fifty is the case where the index has to earn its keep.
pub struct Planted {
    /// Shown in the harness output, so a reader knows what was measured.
    pub name: &'static str,
    /// The word or phrase itself, written into the bodies it belongs to.
    pub text: &'static str,
    /// One body in this many gets it. Zero means never, which is the exhaustion probe.
    pub one_in: u64,
    /// What the title does with it.
    pub title: TitleStyle,
}

/// Where a planted term goes in the title.
///
/// A field rather than a rule, because "which column the term lands in" is exactly what
/// the query is measuring: `deploy` in a title and `deploy` in a body are the same match
/// with different `bm25()` weights, and the harness should be able to say which one it
/// built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleStyle {
    /// Not in the title at all.
    None,
    /// Appended to a generated title.
    Append,
    /// Replaces the generated title, for a term that is a whole title on its own.
    Replace,
}

/// The terms the corpus guarantees, and the frequency it guarantees them at.
///
/// Orthogonal names so a probe can be timed without another probe's rows: `matches` is
/// measured from the index, but the *intent* has to be separatable or the numbers are
/// entangled.
pub const PLANTED: &[Planted] = &[
    Planted {
        name: "common",
        text: "deploy",
        one_in: 7,
        title: TitleStyle::Append,
    },
    Planted {
        name: "uncommon",
        text: "flaky",
        one_in: 40,
        title: TitleStyle::Append,
    },
    Planted {
        name: "rare",
        text: "timeout",
        one_in: 250,
        title: TitleStyle::Append,
    },
    Planted {
        name: "phrase",
        text: "manual migration before the deploy",
        one_in: 500,
        title: TitleStyle::None,
    },
    Planted {
        name: "cjk",
        text: "日本語",
        one_in: 1_000,
        title: TitleStyle::Replace,
    },
    Planted {
        name: "accented",
        text: "café",
        one_in: 333,
        title: TitleStyle::Append,
    },
    Planted {
        name: "absent",
        text: "zzzquux",
        one_in: 0,
        title: TitleStyle::None,
    },
];

impl Planted {
    /// The query a probe runs for this term.
    ///
    /// The phrase probe asks for its phrase *as a phrase*, which is what makes it a test
    /// of the ladder's first rung rather than of the third: the words are individually
    /// common in generated text and only the phrase itself is rare.
    #[must_use]
    pub fn query(&self) -> &'static str {
        self.text
    }

    /// The frequency this term was planted at, as a sentence for the harness to print.
    #[must_use]
    pub fn frequency(&self) -> String {
        match self.one_in {
            0 => "never".to_owned(),
            1 => "every memory".to_owned(),
            n => format!("1 in {n}"),
        }
    }
}

/// The share of bodies each size class gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BodyMix {
    /// Percent of memories with a body of at most [`SHORT_MAX_BYTES`].
    pub short: u32,
    /// Percent with a body of at most [`MEDIUM_MAX_BYTES`].
    pub medium: u32,
    /// Percent with a body of at most [`LONG_MAX_BYTES`].
    pub long: u32,
}

impl BodyMix {
    /// The plan's mix: mostly notes, some articles, a few large things.
    pub const PLAN: Self = Self {
        short: 60,
        medium: 30,
        long: 10,
    };

    /// Whether the three classes add up.
    #[must_use]
    pub const fn is_whole(&self) -> bool {
        self.short + self.medium + self.long == 100
    }
}

impl Default for BodyMix {
    fn default() -> Self {
        Self::PLAN
    }
}

/// Which of the three size classes a body was generated for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BodyClass {
    /// At most [`SHORT_MAX_BYTES`]. A note.
    Short,
    /// At most [`MEDIUM_MAX_BYTES`]. An article.
    Medium,
    /// At most [`LONG_MAX_BYTES`]. A document.
    Long,
}

impl BodyClass {
    /// All three, in ascending order, so a report reads as a size distribution.
    pub const ALL: [Self; 3] = [Self::Short, Self::Medium, Self::Long];

    /// The name the report uses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Short => "short",
            Self::Medium => "medium",
            Self::Long => "long",
        }
    }

    /// The largest body this class allows.
    #[must_use]
    pub const fn cap_bytes(self) -> u64 {
        match self {
            Self::Short => SHORT_MAX_BYTES,
            Self::Medium => MEDIUM_MAX_BYTES,
            Self::Long => LONG_MAX_BYTES,
        }
    }

    /// The smallest body this class produces.
    ///
    /// The floor is a fifth of the cap rather than zero: a "long" body of ten bytes
    /// would satisfy the cap and measure nothing. Uniform between the two means the
    /// class's mean is honest rather than tuned.
    #[must_use]
    pub const fn floor_bytes(self) -> u64 {
        self.cap_bytes() / 5
    }

    /// The share this class is given under `mix`.
    #[must_use]
    pub const fn share(self, mix: &BodyMix) -> u32 {
        match self {
            Self::Short => mix.short,
            Self::Medium => mix.medium,
            Self::Long => mix.long,
        }
    }
}

/// What corpus to build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusSpec {
    /// How many memories.
    pub size: usize,
    /// The PRNG seed. The same seed builds the same bytes.
    pub seed: u64,
    /// The target size distribution.
    pub mix: BodyMix,
}

impl CorpusSpec {
    /// A corpus of `size` memories under the plan's mix and [`DEFAULT_SEED`].
    #[must_use]
    pub fn new(size: usize) -> Self {
        Self {
            size,
            seed: DEFAULT_SEED,
            mix: BodyMix::PLAN,
        }
    }

    /// Sort by class, using the mix's boundaries rather than a random draw.
    ///
    /// Deterministic and exact: with 60/30/10 and a thousand memories the corpus gets
    /// exactly six hundred short bodies. A random draw would leave the realised mix a
    /// few percent off the target and make two runs of different sizes incomparable,
    /// which is the one thing a benchmark corpus must not be.
    #[must_use]
    fn class_of(&self, index: usize) -> BodyClass {
        let position = (index * 100) / self.size.max(1);
        if position < self.mix.short as usize {
            BodyClass::Short
        } else if position < (self.mix.short + self.mix.medium) as usize {
            BodyClass::Medium
        } else {
            BodyClass::Long
        }
    }
}

/// One size class's share of a built corpus, as built.
#[derive(Debug, Clone, Serialize)]
pub struct ClassReport {
    /// Which class.
    pub class: BodyClass,
    /// The share the spec asked for.
    pub target_percent: u32,
    /// How many memories actually got it.
    pub memories: u64,
    /// Their total body size.
    pub bytes: u64,
}

impl ClassReport {
    /// The mean body size in this class, or `None` if it is empty.
    #[must_use]
    pub fn mean_bytes(&self) -> Option<u64> {
        self.bytes.checked_div(self.memories)
    }

    /// The share of the corpus this class actually got, in percent.
    #[must_use]
    pub fn actual_percent(&self, total: u64) -> f64 {
        if total == 0 {
            return 0.0;
        }
        // Percentages of a corpus a machine can hold, so the conversion is exact well
        // beyond the two decimal places the report prints.
        #[allow(clippy::cast_precision_loss)]
        {
            (self.memories as f64) * 100.0 / (total as f64)
        }
    }
}

/// A planted query and how many rows it really matches.
#[derive(Debug, Clone, Serialize)]
pub struct Probe {
    /// Which planted term this is.
    pub name: &'static str,
    /// The query as a user would type it.
    pub query: &'static str,
    /// The frequency the corpus was meant to plant it at.
    pub planted: String,
    /// Rows in the index that match. Measured from the index, not from the recipe.
    pub matches: i64,
}

/// What a corpus build did.
#[derive(Debug, Clone, Serialize)]
pub struct CorpusReport {
    /// The database it was written to.
    pub path: PathBuf,
    /// How many memories were asked for.
    pub size: usize,
    /// The seed.
    pub seed: u64,
    /// The target mix.
    pub mix: BodyMix,
    /// The size distribution as built.
    pub classes: Vec<ClassReport>,
    /// Every body's total size.
    pub body_bytes: u64,
    /// What the database, its WAL and its shared-memory file occupy on disk.
    pub disk_bytes: u64,
    /// How long the build took.
    pub seconds: f64,
    /// Whether rows already existed and were kept.
    pub reused: bool,
    /// Ids worth passing to `get` — one per size class, so a probe can pick its cost.
    pub sample_ids: Vec<String>,
    /// How many memories the vector leg could see, which is the number that decides
    /// whether a hybrid row is measuring the leg or measuring nothing.
    ///
    /// Counted from the table rather than assumed: a corpus is reused across runs, and one
    /// built before an embedder was configured has no vectors at all. The harness fills
    /// the gap by running `reindex --embed` — the command a user would run — and records
    /// what it wrote here.
    pub vectors: usize,
    /// Match counts for the planted terms.
    pub probes: Vec<Probe>,
}

impl CorpusReport {
    /// The mean body size across the corpus.
    #[must_use]
    pub fn mean_body_bytes(&self) -> u64 {
        self.body_bytes
            .checked_div(u64::try_from(self.size).unwrap_or(1))
            .unwrap_or(0)
    }
}

/// Build `spec` into `db`, or measure the corpus already in it.
///
/// Idempotent by size: a database already holding `spec.size` memories is measured and
/// left alone, which is what makes the second run of the harness fast and what makes two
/// runs comparable. `rebuild` forces the regeneration.
///
/// # Errors
///
/// Returns [`Error::Database`] if the existing corpus cannot be counted or a row cannot
/// be written, and [`Error::Invalid`] if the mix does not add up.
pub fn generate(db: &Database, spec: &CorpusSpec, rebuild: bool) -> Result<CorpusReport> {
    if !spec.mix.is_whole() {
        return Err(Error::Invalid(
            "the body-size mix must add up to 100 percent".to_owned(),
        ));
    }

    let existing = corpus_size(db)?;
    let asked = u64::try_from(spec.size).unwrap_or(u64::MAX);
    let reused = !rebuild && existing == asked;
    if !reused {
        if existing != 0 {
            clear(db)?;
        }
        let started = Instant::now();
        fill(db, spec)?;
        let seconds = started.elapsed().as_secs_f64();
        return report(db, spec, seconds, false);
    }
    report(db, spec, 0.0, true)
}

/// How many memories are in the database, whatever their status or workspace.
///
/// # Errors
///
/// Returns [`Error::Database`] if the count fails.
pub fn corpus_size(db: &Database) -> Result<u64> {
    let count: i64 = db
        .conn()
        .query_row("SELECT count(*) FROM memories", [], |row| row.get(0))?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Empty the memory tables, leaving the schema and the default workspace.
fn clear(db: &Database) -> Result<()> {
    // The cascades take the plain-text index, the links, the embeddings and the
    // outcomes with it, so there is nothing to remember to clear.
    db.conn().execute_batch("DELETE FROM memories;")?;
    Ok(())
}

/// How many memories are generated and written before the next batch.
///
/// A hundred thousand long bodies is eight hundred megabytes of text, and holding all of
/// it in a `Vec` before writing any of it would make the corpus generator the largest
/// thing in the process by an order of magnitude — which is a problem when the report is
/// about to print a peak resident set. Four thousand bodies is about thirty megabytes,
/// which is enough that the per-batch transaction is not the cost.
const BATCH: usize = 4_096;

/// Write `spec.size` memories, a batch at a time.
///
/// One transaction per batch rather than one per memory, because this is setup and not
/// measurement: the harness times `kiomon add` against this corpus afterwards, and a build
/// that took an hour would make the ruler too expensive to pick up.
fn fill(db: &Database, spec: &CorpusSpec) -> Result<()> {
    let mut prng = Prng::new(spec.seed);
    let vocabulary = Vocabulary::new();
    let mut written = 0;
    let mut index = 0;
    while index < spec.size {
        let end = (index + BATCH).min(spec.size);
        let batch = (index..end)
            .map(|position| memory_for(spec, position, &mut prng, &vocabulary))
            .collect::<Vec<_>>();
        written += db.insert_many(&batch)?;
        index = end;
    }
    if written != spec.size {
        // Every generated body carries its own vocabulary that cannot produce a planted
        // term by accident, and a title with the position in it, so a short write means
        // the corpus is not the corpus the report describes.
        return Err(Error::Invalid(format!(
            "the corpus asked for {} memories and {} were written; the generator produced \
             duplicates",
            spec.size, written
        )));
    }
    Ok(())
}

/// Measure what is in the database.
fn report(db: &Database, spec: &CorpusSpec, seconds: f64, reused: bool) -> Result<CorpusReport> {
    // One scan, six aggregates. `length(body)` has to read every body, so asking each
    // class its own question would read the whole corpus three times — which is the WP1
    // lesson about `stats`, arriving again in the harness that exists to remember it.
    // Bands rather than nested caps, so each body is counted once and the sums are
    // partitions rather than subtractions to get wrong.
    //
    // The caps are interpolated as literals, which is the one exception the store already
    // documents: they are compile-time constants with no user input anywhere near them,
    // and binding them would mean narrowing a `u64` that is not allowed to be narrowed.
    let sql = format!(
        "SELECT sum(length(body) <= {SHORT_MAX_BYTES}),
                sum(CASE WHEN length(body) <= {SHORT_MAX_BYTES} THEN length(body) ELSE 0 END),
                sum(length(body) > {SHORT_MAX_BYTES} AND length(body) <= {MEDIUM_MAX_BYTES}),
                sum(CASE WHEN length(body) > {SHORT_MAX_BYTES} AND length(body) <= {MEDIUM_MAX_BYTES}
                         THEN length(body) ELSE 0 END),
                sum(length(body) > {MEDIUM_MAX_BYTES}),
                sum(CASE WHEN length(body) > {MEDIUM_MAX_BYTES} THEN length(body) ELSE 0 END)
           FROM memories"
    );
    let (short, short_bytes, medium, medium_bytes, long, long_bytes) =
        db.conn().query_row(&sql, [], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;

    let mut classes = Vec::with_capacity(3);
    for (class, memories, bytes) in [
        (BodyClass::Short, short, short_bytes),
        (BodyClass::Medium, medium, medium_bytes),
        (BodyClass::Long, long, long_bytes),
    ] {
        classes.push(ClassReport {
            class,
            target_percent: class.share(&spec.mix),
            memories: u64::try_from(memories.max(0)).unwrap_or(0),
            bytes: u64::try_from(bytes.max(0)).unwrap_or(0),
        });
    }
    let body_bytes = classes.iter().map(|class| class.bytes).sum();
    let sample_ids = db.conn().query_row(
        "SELECT id FROM memories ORDER BY rowid LIMIT 1",
        [],
        |row| row.get::<_, String>(0),
    );
    let sample_ids = match sample_ids {
        Ok(id) => vec![id],
        Err(rusqlite::Error::QueryReturnedNoRows) => Vec::new(),
        Err(error) => return Err(error.into()),
    };

    // Checkpoint before measuring the file: an un-checkpointed WAL is most of the
    // corpus, and a report that says 40 MB when the corpus is 800 MB is worse than no
    // report at all.
    db.conn()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;

    Ok(CorpusReport {
        path: db.path().to_path_buf(),
        size: spec.size,
        seed: spec.seed,
        mix: spec.mix,
        classes,
        body_bytes,
        disk_bytes: disk_usage(db.path()),
        seconds,
        reused,
        sample_ids,
        vectors: db
            .conn()
            .query_row("SELECT COUNT(*) FROM memory_embeddings", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_or(0, |count| usize::try_from(count).unwrap_or(0)),
        probes: probes(db)?,
    })
}

/// The bytes a database occupies: the file, its write-ahead log and its shared memory.
///
/// All three, because SQLite's on-disk cost is all three, and a WAL left large by a bulk
/// write is a real 40 MB that a user's disk has to hold.
#[must_use]
pub fn disk_usage(path: &std::path::Path) -> u64 {
    let mut total = 0;
    let mut name = path.as_os_str().to_os_string();
    for suffix in ["", "-wal", "-shm"] {
        name.clear();
        name.push(path.as_os_str());
        name.push(suffix);
        if let Ok(metadata) = std::fs::metadata(&name) {
            total += metadata.len();
        }
    }
    total
}

/// How many rows each planted term matches in this database.
///
/// # Errors
///
/// Returns [`Error::Database`] if the index cannot be queried.
pub fn probes(db: &Database) -> Result<Vec<Probe>> {
    let mut statement = db
        .conn()
        .prepare("SELECT count(*) FROM memory_fts WHERE memory_fts MATCH ?1")?;
    let mut out = Vec::with_capacity(PLANTED.len());
    for planted in PLANTED {
        // Every probe is asked as a quoted prefix, which is the ladder's second rung and
        // the expression that matches anything the term could match. A narrower query
        // would report a selectivity the timed query does not have.
        let query = format!("\"{}\"*", planted.text.replace('"', "\"\""));
        let matches: i64 = statement.query_row([&query], |row| row.get(0))?;
        out.push(Probe {
            name: planted.name,
            query: planted.query(),
            planted: planted.frequency(),
            matches,
        });
    }
    Ok(out)
}

/// One memory, as the corpus describes it.
fn memory_for(
    spec: &CorpusSpec,
    index: usize,
    prng: &mut Prng,
    vocabulary: &Vocabulary,
) -> NewMemory {
    let class = spec.class_of(index);
    let position = u64::try_from(index).unwrap_or(0);
    let planted = |planted: &Planted| planted.one_in != 0 && position % planted.one_in == 0;

    let mut title = vocabulary.phrase(prng, 3 + (index % 5));
    let mut body = String::new();

    // Planted terms go in first, and the filler never writes one. That is what makes the
    // match count of a probe a fact about the corpus rather than a coincidence of the
    // generator: `timeout` is in one body in two hundred and fifty because it was put
    // there, not because a random word happened to be spelled that way. There is a test.
    for term in PLANTED {
        if !planted(term) {
            continue;
        }
        match term.title {
            TitleStyle::Append => {
                title.push(' ');
                title.push_str(term.text);
            }
            TitleStyle::Replace => title = format!("{}のメモ {index}", term.text),
            TitleStyle::None => {}
        }
        body.push_str(term.text);
        body.push_str(". ");
    }

    // A sentence at a time, stopping before the cap rather than after it: the target is
    // where to stop, and the cap is what a body is not allowed to exceed. Checking only
    // the target would let the last sentence push a "long" body past 100 KiB, which is
    // the one number in the mix that has to be exact.
    let target = prng.between(class.floor_bytes(), class.cap_bytes());
    while (body.len() as u64) < target {
        let sentence = vocabulary.sentence(prng);
        if body.len() as u64 + sentence.len() as u64 > class.cap_bytes() {
            break;
        }
        body.push_str(&sentence);
    }

    let kind = match index % 10 {
        0..=2 => MemoryKind::Reference,
        3..=6 => MemoryKind::Semantic,
        7..=8 => MemoryKind::Procedural,
        _ => MemoryKind::Episodic,
    };

    NewMemory::new(kind, title)
        .body(body)
        .excerpt(vocabulary.phrase(prng, 8))
        .tags(Vocabulary::tags(prng, index))
}

// ---------------------------------------------------------------------------
// Text generation
// ---------------------------------------------------------------------------

/// A word list with a Zipf distribution over it.
struct Vocabulary {
    words: Vec<String>,
    /// Cumulative Zipf weights, so sampling is a walk and a comparison.
    cumulative: Vec<f64>,
    total: f64,
}

impl Vocabulary {
    fn new() -> Self {
        let mut prng = Prng::new(DEFAULT_SEED ^ 0x5EED_5EED);
        let mut words = Vec::with_capacity(VOCABULARY);
        while words.len() < VOCABULARY {
            let length = 1 + prng.below(2);
            let word = prng.syllables(length);
            if word.len() >= 4 && !PLANTED.iter().any(|planted| planted.text.contains(&word)) {
                words.push(word);
            }
        }
        // Zipf, at the exponent English sits near. The point is the *shape*: a handful of
        // words appearing everywhere and a long tail appearing almost nowhere, which is
        // what makes bm25's inverse document frequency do real work.
        let mut cumulative = Vec::with_capacity(VOCABULARY);
        let mut running = 0.0;
        for rank in 1..=VOCABULARY {
            #[allow(clippy::cast_precision_loss)]
            let weight = 1.0 / (rank as f64).powf(1.07);
            running += weight;
            cumulative.push(running);
        }
        Self {
            words,
            cumulative,
            total: running,
        }
    }

    fn word(&self, prng: &mut Prng) -> &str {
        let target = prng.unit() * self.total;
        let index = self
            .cumulative
            .partition_point(|weight| *weight < target)
            .min(self.words.len() - 1);
        &self.words[index]
    }

    /// One sentence of six to eighteen words, ending in a full stop.
    ///
    /// Sentences rather than a soup of words because the tokenizer is what is being
    /// measured: `unicode61` splits on punctuation, and a corpus without any would stop
    /// exercising the part of the tokenizer that has to decide what a word is.
    fn sentence(&self, prng: &mut Prng) -> String {
        let mut out = String::new();
        let words = 6 + prng.below(13);
        for position in 0..words {
            if position > 0 {
                out.push(' ');
            }
            out.push_str(self.word(prng));
        }
        out.push_str(
            ".

",
        );
        out
    }

    fn phrase(&self, prng: &mut Prng, words: usize) -> String {
        let mut out = String::new();
        for position in 0..words {
            if position > 0 {
                out.push(' ');
            }
            out.push_str(self.word(prng));
        }
        out
    }

    /// Zero to three tags, drawn from a small pool so that tag filters have something to
    /// match and duplicates are common — which is what a real tag vocabulary looks like.
    ///
    /// No tag is a planted term. A `deploy` tag would put the word in a memory that the
    /// recipe says does not have it, and the probe would then be reporting the tag
    /// generator's frequency rather than the one it planted. There is a test.
    fn tags(prng: &mut Prng, index: usize) -> Vec<String> {
        const POOL: [&str; 20] = [
            "infra", "release", "testing", "sqlite", "rust", "api", "cache", "perf", "notes",
            "todo", "design", "bug", "docs", "review", "ci", "windows", "linux", "macos", "db",
            "fts",
        ];
        let count = prng.below(4);
        let mut tags = Vec::with_capacity(count);
        for offset in 0..count {
            let tag = POOL[(index / 3 + offset * 7 + prng.below(5)) % POOL.len()];
            let owned = tag.to_owned();
            if !tags.contains(&owned) {
                tags.push(owned);
            }
        }
        tags
    }
}

/// `splitmix64`, which is eight lines and no dependency.
///
/// Not cryptographic and not trying to be: the corpus needs to be *reproducible*, so a
/// seed produces the same bytes on every platform and every run. A hash-shaped generator
/// is what makes that safe to rely on, and it is the same reasoning that has this project
/// hand-rolling its ULIDs.
struct Prng(u64);

impl Prng {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number below `bound`. Biased by at most one part in two to the sixty-fourth,
    /// which for picking word counts is not a bias at all.
    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        usize::try_from(self.next() % u64::try_from(bound).unwrap_or(u64::MAX)).unwrap_or(0)
    }

    /// A number in `low..=high`, for the one draw whose units are bytes.
    fn between(&mut self, low: u64, high: u64) -> u64 {
        let span = usize::try_from(high.saturating_sub(low)).unwrap_or(usize::MAX);
        low + u64::try_from(self.below(span)).unwrap_or(0)
    }

    /// A number in `0.0..1.0`.
    fn unit(&mut self) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        let value = (self.next() >> 11) as f64;
        // 2^53 as a float literal: the shifts above keep the value below it, so the
        // conversion is exact and the divisor never needs to be cast.
        value / 9_007_199_254_740_992.0
    }

    /// A pronounceable pseudo-word: syllables of a consonant, a vowel and an optional
    /// consonant.
    ///
    /// Pronounceable rather than random, because the FTS5 tokenizer sees letter runs and
    /// a word of random glyphs would tokenize unlike anything a person writes. No
    /// consonant cluster can form, so no generated word can be mistaken for English and,
    /// in particular, none can collide with a planted term — there is a test.
    fn syllables(&mut self, count: usize) -> String {
        const ONSETS: [&str; 13] = [
            "b", "d", "f", "g", "k", "l", "m", "n", "p", "r", "s", "t", "v",
        ];
        const VOWELS: [&str; 5] = ["a", "e", "i", "o", "u"];
        const CODAS: [&str; 6] = ["n", "r", "l", "s", "k", "t"];
        let mut out = String::with_capacity(count * 3);
        for _ in 0..count {
            out.push_str(ONSETS[self.below(ONSETS.len())]);
            out.push_str(VOWELS[self.below(VOWELS.len())]);
            if self.below(2) == 0 {
                out.push_str(CODAS[self.below(CODAS.len())]);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DEFAULT_WORKSPACE;

    fn built(size: usize) -> (Database, CorpusReport) {
        let db = Database::open_in_memory().expect("an in-memory database must open");
        let spec = CorpusSpec::new(size);
        let report = generate(&db, &spec, false).expect("generation must succeed");
        (db, report)
    }

    fn matches(report: &CorpusReport, name: &str) -> i64 {
        report
            .probes
            .iter()
            .find(|probe| probe.name == name)
            .expect("the probe must exist")
            .matches
    }

    #[test]
    fn nothing_the_generator_writes_on_its_own_can_be_mistaken_for_a_planted_term() {
        // A generated body or tag containing `deploy` would make a probe report a
        // frequency nobody planted, and the harness would be timing a query it had not
        // designed — the WP2 mistake, which was a corpus where every term matched
        // everything.
        let vocabulary = Vocabulary::new();
        for word in &vocabulary.words {
            for planted in PLANTED {
                assert!(
                    !planted.text.contains(word.as_str()),
                    "generated word {word:?} appears inside planted term {:?}",
                    planted.text
                );
            }
        }
        let mut prng = Prng::new(7);
        for _ in 0..2_000 {
            for tag in Vocabulary::tags(&mut prng, 3) {
                for planted in PLANTED {
                    assert!(
                        !planted.text.contains(tag.as_str()),
                        "tag {tag:?} appears inside planted term {:?}",
                        planted.text
                    );
                }
            }
        }
    }

    #[test]
    fn the_same_seed_builds_the_same_corpus() {
        let (_, first) = built(200);
        let (_, second) = built(200);
        assert_eq!(first.body_bytes, second.body_bytes);
        let shape = |report: &CorpusReport| {
            report
                .classes
                .iter()
                .map(|class| (class.memories, class.bytes))
                .collect::<Vec<_>>()
        };
        assert_eq!(shape(&first), shape(&second));
        // And the probes agree, which is the part that would drift silently: a different
        // word order is invisible in a total and very visible in a match count.
        let probes = |report: &CorpusReport| {
            report
                .probes
                .iter()
                .map(|probe| probe.matches)
                .collect::<Vec<_>>()
        };
        assert_eq!(probes(&first), probes(&second));
    }

    #[test]
    fn the_realised_mix_is_the_mix_that_was_asked_for() {
        let (db, report) = built(1_000);
        assert_eq!(
            report
                .classes
                .iter()
                .map(|class| (class.class, class.memories))
                .collect::<Vec<_>>(),
            vec![
                (BodyClass::Short, 600),
                (BodyClass::Medium, 300),
                (BodyClass::Long, 100),
            ],
        );
        assert_eq!(
            report.classes.iter().map(|c| c.memories).sum::<u64>(),
            1_000
        );
        assert_eq!(report.size, 1_000);
        assert_eq!(
            db.indexed_count().expect("the index must be countable"),
            1_000,
            "every generated memory must be findable"
        );
    }

    #[test]
    fn the_classes_partition_the_corpus_and_no_body_exceeds_its_cap() {
        let (db, report) = built(300);
        let mut floor = 0;
        for class in BodyClass::ALL {
            // A band, not a cap: if a body sits in two bands then the report counts it
            // twice, and the size distribution it prints is not the one it built.
            let band: i64 = db
                .conn()
                .query_row(
                    &format!(
                        "SELECT count(*) FROM memories
                          WHERE length(body) > {floor} AND length(body) <= {}",
                        class.cap_bytes()
                    ),
                    [],
                    |row| row.get(0),
                )
                .expect("the count must run");
            let reported = report
                .classes
                .iter()
                .find(|entry| entry.class == class)
                .expect("every class is reported")
                .memories;
            assert_eq!(
                u64::try_from(band).expect("a count is positive"),
                reported,
                "the {} band holds a different number of bodies than the report says",
                class.as_str()
            );
            floor = class.cap_bytes();
        }
        let longest: i64 = db
            .conn()
            .query_row("SELECT max(length(body)) FROM memories", [], |row| {
                row.get(0)
            })
            .expect("the max must run");
        let longest = u64::try_from(longest).expect("a length is positive");
        assert!(
            longest <= LONG_MAX_BYTES,
            "the longest body was {longest} bytes against a cap of {LONG_MAX_BYTES}"
        );
        let shortest: i64 = db
            .conn()
            .query_row("SELECT min(length(body)) FROM memories", [], |row| {
                row.get(0)
            })
            .expect("the min must run");
        assert!(shortest > 0, "a body of nothing measures nothing");
    }

    #[test]
    fn every_planted_term_is_in_exactly_the_memories_the_recipe_says_it_is() {
        let (_, report) = built(10_000);
        // The planted share, exactly. `deploy` is the exception, and it is a real
        // overlap rather than slack in the test: the phrase probe's planted text
        // *contains* the word, so a phrase memory matches `deploy` as well. One phrase
        // memory in seven is also a `deploy` memory, because 500 and 7 are coprime.
        // Memory zero is planted too, so the count of multiples of `one_in` below `size`
        // is `(size - 1) / one_in + 1` and not `size / one_in` — which is off by one, and
        // off by one in the direction that would have made this test pass on a corpus
        // whose recipe was wrong.
        let planted = |one_in: u64| i64::try_from((10_000 - 1) / one_in + 1).unwrap_or(0);
        let phrase = planted(500);
        let overlap =
            i64::try_from((0..phrase).filter(|step| step * 500 % 7 == 0).count()).unwrap_or(0);
        assert_eq!(matches(&report, "common"), planted(7) + phrase - overlap);
        assert_eq!(matches(&report, "uncommon"), planted(40));
        assert_eq!(matches(&report, "rare"), planted(250));
        assert_eq!(matches(&report, "phrase"), phrase);
        assert_eq!(matches(&report, "cjk"), planted(1_000));
        // `café` folds to `cafe` under `remove_diacritics`, and the probe asks for it as
        // a prefix, so a memory containing plain `cafe` would match too. Nothing the
        // generator writes on its own can be spelled that way.
        assert_eq!(matches(&report, "accented"), planted(333));
        assert_eq!(
            matches(&report, "absent"),
            0,
            "a planted absence that matches something is not an absence"
        );
    }

    #[test]
    fn the_phrase_probe_finds_only_its_phrase() {
        // The phrase is the ladder's first rung, and the reason this corpus can test it
        // is that the filler vocabulary is not English: `manual`, `migration` and
        // `deploy` appear together only where the phrase was planted.
        let (db, report) = built(2_000);
        let loose: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM memory_fts WHERE memory_fts MATCH ?1",
                [r#""manual"* AND "migration"* OR "deploy"*"#],
                |row| row.get(0),
            )
            .expect("the count must run");
        assert_eq!(
            loose,
            matches(&report, "common"),
            "the phrase's words must match exactly the memories `deploy` matches, or the \
             phrase probe is not measuring the phrase"
        );
        assert_eq!(matches(&report, "phrase"), 4);
    }

    #[test]
    fn generation_is_reused_rather_than_repeated() {
        let db = Database::open_in_memory().expect("an in-memory database must open");
        let spec = CorpusSpec::new(100);
        let first = generate(&db, &spec, false).expect("generation must succeed");
        assert!(!first.reused);
        let second = generate(&db, &spec, false).expect("the second call must succeed");
        assert!(second.reused, "the same size must not be rebuilt");
        assert_eq!(
            corpus_size(&db).expect("the corpus must be countable"),
            100,
            "reuse must not append"
        );
        let third = generate(&db, &spec, true).expect("a rebuild must succeed");
        assert!(!third.reused);
        assert_eq!(
            corpus_size(&db).expect("the corpus must be countable"),
            100,
            "a rebuild must replace rather than add"
        );
    }

    #[test]
    fn a_mix_that_does_not_add_up_is_refused() {
        let db = Database::open_in_memory().expect("an in-memory database must open");
        let spec = CorpusSpec {
            size: 10,
            seed: DEFAULT_SEED,
            mix: BodyMix {
                short: 60,
                medium: 30,
                long: 30,
            },
        };
        assert!(generate(&db, &spec, false).is_err());
    }

    #[test]
    fn every_sample_id_is_a_real_memory_in_the_default_workspace() {
        let (db, report) = built(20);
        assert!(!report.sample_ids.is_empty());
        for id in &report.sample_ids {
            assert_eq!(
                db.get(id).expect("the sampled id must exist").workspace_id,
                DEFAULT_WORKSPACE
            );
        }
    }

    #[test]
    fn the_prng_is_a_function_of_its_seed_only() {
        let mut first = Prng::new(DEFAULT_SEED);
        let mut second = Prng::new(DEFAULT_SEED);
        for _ in 0..64 {
            assert_eq!(first.next(), second.next());
        }
    }

    #[test]
    fn the_word_distribution_is_skewed_like_a_language() {
        // A uniform vocabulary would make `bm25()`'s inverse document frequency a
        // constant and the ranking no better than a token match. The head has to be
        // heavy and the tail long, or the corpus flatters the code.
        let vocabulary = Vocabulary::new();
        let mut prng = Prng::new(1);
        let mut counts = vec![0usize; vocabulary.words.len()];
        for _ in 0..10_000 {
            let word = vocabulary.word(&mut prng);
            let index = vocabulary
                .words
                .iter()
                .position(|candidate| candidate == word)
                .expect("a drawn word is in the vocabulary");
            counts[index] += 1;
        }
        assert!(
            counts[0] > 500,
            "the commonest word appeared {} times in 10 000",
            counts[0]
        );
        let never = counts.iter().filter(|count| **count == 0).count();
        assert!(
            never > 1_000,
            "only {never} words went unused, so the tail is not long"
        );
    }
}
