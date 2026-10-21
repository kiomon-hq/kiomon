// SPDX-License-Identifier: Apache-2.0

//! Getting the brain out and back: `export` and `import`.
//!
//! The format rules, the limits and the markdown view live in
//! [`kiomon_core::import`]; what is here is the streaming, the file walking, and the
//! reporting — which is where a portability feature actually goes wrong. Three decisions
//! are made in this module and nowhere else:
//!
//! - **`export` streams a page at a time.** The query is keyset-paginated and each page is
//!   serialised straight into a buffered writer, so the memory cost of exporting a brain is
//!   one page of rows and not the brain. §8 WP6 asserts it with a 100 MB corpus, because
//!   "streams" is otherwise a claim about code somebody did not read.
//! - **`import` reports per line and keeps going.** A line that cannot be read is a line
//!   that is skipped: `path:line: message` on stderr, the fragment when the error came from
//!   the parser, and the count at the end. Only a file where *nothing* could be read exits
//!   non-zero, because a script has to be able to tell "some of it landed" from "none of
//!   it did".
//! - **A batch is a transaction and a batch is embedded after it.** Five hundred rows at
//!   a time, so an interrupted import keeps its pages and a re-run resumes, exactly like
//!   `reindex --embed`.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use kiomon_core::import::{
    ExportRecord, ImportBatch, ImportRow, ImportState, markdown_filename, to_markdown,
};
use kiomon_core::{Error, ExitCode, Memory, Result, Scope, Status};

use super::{open, read_scope};
use crate::cli::{Cli, ExportArgs, FormatArg, ImportArgs};
use crate::out;

/// How many rows one page holds.
///
/// The same 500 the reindex walks with: a page is one transaction and one embed call, and
/// the number that matters is that it is bounded — a brain is not, and an import that read
/// its whole file before writing anything would need the brain's size in memory to restore
/// it.
const PAGE: u32 = 500;

/// The most bytes one line may be.
///
/// The body cap (`kiomon_core::MAX_BODY_BYTES`) is what a memory may hold; this is what
/// the *reader* will hold for one line, and it is larger because a line is a JSON object
/// around a body with escapes. A file with a single 2 GiB line is the case this exists
/// for: `read_until` would happily allocate it, and a limit enforced after the allocation
/// is not a limit.
const MAX_LINE_BYTES: usize = 2 * 1024 * 1024;

/// How many line errors are printed before the rest are only counted.
///
/// A file of ten thousand broken lines should not produce ten thousand lines of stderr:
/// the first few tell a user what is wrong with the file, and the count tells them how
/// much of it. Nothing is hidden — the number is in the summary, and `--json` carries it.
const MAX_REPORTED_ERRORS: usize = 100;

/// `kiomon export`
///
/// # Errors
///
/// Returns [`Error::Invalid`] for a combination of flags that cannot mean anything (JSON
/// on stdout with `--json`, markdown without a directory), [`Error::Io`] when the output
/// cannot be written, and whatever the store raises.
pub fn export(cli: &Cli, args: &ExportArgs) -> Result<ExitCode> {
    let db = open(cli)?;
    let scope = read_scope(&db, cli.config.as_deref(), args.workspace.as_deref())?;
    let status: Option<Status> = args.status.map_or(Some(Status::Active), Option::from);

    match args.format {
        FormatArg::Jsonl => export_jsonl(cli, args, &db, &scope, status),
        FormatArg::Markdown => export_markdown(cli, args, &db, &scope, status),
    }
}

fn export_jsonl(
    cli: &Cli,
    args: &ExportArgs,
    db: &kiomon_core::Database,
    scope: &Scope,
    status: Option<Status>,
) -> Result<ExitCode> {
    // The file is opened before the first row is read, so a bad path is a failure with
    // nothing written rather than a failure after an hour of work.
    let mut sink = open_sink(args.output.as_deref())?;
    if args.output.is_none() && cli.json {
        return Err(Error::Invalid(
            "`export --format jsonl` writes JSON to stdout, so --json has nothing to add \
             and would be a second document after the records. Pass --output to write the \
             file and get a summary, or drop --json and pipe the records."
                .to_owned(),
        ));
    }

    let mut exported = 0usize;
    let mut cursor: Option<String> = None;
    loop {
        let page = db.export_page(scope, status, cursor.as_deref(), PAGE)?;
        let Some(last) = page.last() else { break };
        cursor.clone_from(&last.id);
        for record in &page {
            let line = record.to_jsonl()?;
            sink.write_all(line.as_bytes())
                .and_then(|()| sink.write_all(b"\n"))
                .map_err(|err| Error::Io(std::io::Error::new(err.kind(), err.to_string())))?;
        }
        exported += page.len();
        if page.len() < PAGE as usize {
            break;
        }
    }
    sink.flush().map_err(Error::Io)?;
    drop(sink);

    let report = ExportReport {
        format: "jsonl",
        path: args
            .output
            .as_deref()
            .map(|path| path.display().to_string()),
        exported,
        vectors_included: false,
        edges_included: false,
    };
    // The records are the answer on the human path when they went to stdout, so a summary
    // there would be a line in the middle of somebody's file. It goes to stderr instead.
    if args.output.is_none() {
        out::warn(
            cli.quiet || cli.json,
            &format!(
                "exported {exported} memories to stdout; vectors and edges are not in the \
                 file — `kiomon reindex --embed` and `kiomon link` are how they come back"
            ),
        );
        return Ok(ExitCode::Success);
    }
    out::emit(cli.json, report, |report| {
        println!(
            "Exported {} memories to {}",
            report.exported,
            report.path.as_deref().unwrap_or("-")
        );
    })?;
    Ok(ExitCode::Success)
}

fn export_markdown(
    cli: &Cli,
    args: &ExportArgs,
    db: &kiomon_core::Database,
    scope: &Scope,
    status: Option<Status>,
) -> Result<ExitCode> {
    let Some(directory) = args.output.as_deref() else {
        return Err(Error::Invalid(
            "`export --format markdown` writes one document per memory, so it needs \
             --output <DIR>: a directory of files is readable, a stream of concatenated \
             documents is not."
                .to_owned(),
        ));
    };
    if directory.is_file() {
        return Err(Error::Invalid(format!(
            "--output is {} and markdown export writes many files; name a directory",
            directory.display()
        )));
    }
    std::fs::create_dir_all(directory)?;

    let mut exported = 0usize;
    let mut cursor: Option<String> = None;
    loop {
        let page = db.export_page(scope, status, cursor.as_deref(), PAGE)?;
        let Some(last) = page.last() else { break };
        cursor.clone_from(&last.id);
        for record in &page {
            let memory = record_to_memory(record);
            let path = directory.join(markdown_filename(&memory));
            let mut file = BufWriter::new(File::create(&path).map_err(|err| {
                Error::Io(std::io::Error::new(
                    err.kind(),
                    format!("could not write {}: {err}", path.display()),
                ))
            })?);
            file.write_all(to_markdown(&memory).as_bytes())
                .map_err(Error::Io)?;
            file.flush().map_err(Error::Io)?;
        }
        exported += page.len();
        if page.len() < PAGE as usize {
            break;
        }
    }

    let report = ExportReport {
        format: "markdown",
        path: Some(directory.display().to_string()),
        exported,
        vectors_included: false,
        edges_included: false,
    };
    out::emit(cli.json, report, |report| {
        println!(
            "Exported {} memories to {}",
            report.exported,
            report.path.as_deref().unwrap_or("-")
        );
    })?;
    Ok(ExitCode::Success)
}

/// The markdown writer's view of a record.
///
/// The export record is the file's shape and [`Memory`] is the renderer's, and the two are
/// deliberately not the same type: markdown shows ten of twenty-four fields, and a renderer
/// that could see the rest would eventually print one.
fn record_to_memory(record: &ExportRecord) -> Memory {
    Memory {
        id: record.id.clone().unwrap_or_default(),
        workspace_id: record.workspace_id.clone(),
        kind: record.kind,
        title: record.title.clone(),
        excerpt: record.excerpt.clone(),
        body: record.body.clone(),
        tags: record.tags.clone(),
        memory_metadata: record.memory_metadata.clone(),
        source: record.source.clone(),
        source_url: record.source_url.clone(),
        confidence: record.confidence,
        importance: record.importance.unwrap_or(0.5),
        strength: record.strength.unwrap_or(1.0),
        corroboration_count: record.corroboration_count.unwrap_or(0),
        retrieval_count: record.retrieval_count.unwrap_or(0),
        last_retrieved_at: record.last_retrieved_at.clone(),
        feedback_score: record.feedback_score.unwrap_or(0.0),
        feedback_count: record.feedback_count.unwrap_or(0),
        pinned: record.pinned,
        status: record.status.unwrap_or(Status::Active),
        expires_at: record.expires_at.clone(),
        created_at: record.created_at.clone().unwrap_or_default(),
        updated_at: record.updated_at.clone().unwrap_or_default(),
    }
}

fn open_sink(path: Option<&Path>) -> Result<Box<dyn Write>> {
    match path {
        Some(path) if path != Path::new("-") => {
            let file = File::create(path).map_err(|err| {
                Error::Io(std::io::Error::new(
                    err.kind(),
                    format!("could not write {}: {err}", path.display()),
                ))
            })?;
            Ok(Box::new(BufWriter::new(file)))
        }
        _ => Ok(Box::new(BufWriter::new(std::io::stdout()))),
    }
}

/// What an export did, for the summary and for `--json`.
#[derive(serde::Serialize)]
pub struct ExportReport {
    /// `jsonl` or `markdown`.
    pub format: &'static str,
    /// Where it went, or `None` for stdout.
    pub path: Option<String>,
    /// How many memories were written.
    pub exported: usize,
    /// Always `false`, and stated rather than implied: a caller that assumes an export is
    /// a backup should learn otherwise from the response and not from a search that cannot
    /// find anything without a model.
    pub vectors_included: bool,
    /// Always `false`, and for a different reason than the vectors: a vector is derived
    /// data that can be rebuilt, while an edge is a fact somebody recorded. The format does
    /// not carry edges yet, `import` does not auto-link what it restores, and this field is
    /// here so a caller restoring a brain learns that its graph did not come with it.
    pub edges_included: bool,
}

/// `kiomon import`
///
/// # Errors
///
/// Returns [`Error::Io`] when a file or stdin cannot be read, [`Error::Invalid`] for a
/// `--workspace` that cannot be a workspace, and whatever the store raises. A line that
/// cannot be read is **not** an error here: it is reported on stderr and counted.
pub fn import(cli: &Cli, args: &ImportArgs) -> Result<ExitCode> {
    let db = open(cli)?;
    let sources = Sources::resolve(&args.path)?;
    let mut totals = Totals {
        dry_run: args.dry_run,
        ..Totals::default()
    };

    for source in &sources {
        match source {
            Source::Jsonl(path) => {
                let reader: Box<dyn BufRead> = match path {
                    Some(path) => {
                        let file = File::open(path).map_err(|err| {
                            Error::Io(std::io::Error::new(
                                err.kind(),
                                format!("could not read {}: {err}", path.display()),
                            ))
                        })?;
                        Box::new(BufReader::new(file))
                    }
                    None => Box::new(BufReader::new(std::io::stdin())),
                };
                let label = path
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), |path| path.display().to_string());
                read_jsonl(&db, args, &label, reader, &mut totals)?;
            }
            Source::Markdown(path) => {
                let text = std::fs::read_to_string(path).map_err(|err| {
                    Error::Io(std::io::Error::new(
                        err.kind(),
                        format!("could not read {}: {err}", path.display()),
                    ))
                })?;
                let stem = path
                    .file_stem()
                    .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
                let label = path.display().to_string();
                let record = match kiomon_core::import::from_markdown(&text, &stem) {
                    Ok(record) => record,
                    Err(message) => {
                        totals.error(&label, 1, &message, None);
                        continue;
                    }
                };
                totals.records += 1;
                import_page(&db, args, &label, vec![(1, record)], &mut totals)?;
            }
        }
    }

    if totals.written() == 0 && totals.errors > 0 {
        // Partial success is a success; total failure is not. A script that imported five
        // of a hundred lines can check the count, but one that imported none and exited 0
        // could not tell that from an empty file. The summary is *not* emitted first: a
        // `--json` caller gets one document, and the failure envelope is the honest one.
        return Err(Error::Invalid(format!(
            "no line could be imported: {} line(s) reported an error",
            totals.errors
        )));
    }

    out::emit(cli.json, totals.report(), ImportReport::print_human)?;
    Ok(ExitCode::Success)
}

/// Read a JSONL stream a line at a time, importing in pages.
///
/// `BufRead::read_until` rather than `lines()`: `lines()` stops at the first line whose
/// bytes are not UTF-8, and a file with one corrupted line in it is exactly the file this
/// command exists to report about. Bytes are read, then decoded per line, so a bad line
/// costs a message and not the rest of the file.
fn read_jsonl<R: BufRead>(
    db: &kiomon_core::Database,
    args: &ImportArgs,
    label: &str,
    mut reader: R,
    totals: &mut Totals,
) -> Result<()> {
    // `(line, record)`: a page is where lines stop being lines for the *write*, but not
    // for the report. A title that is too long is a line error like any other, and losing
    // its number would send a user hunting through a file for it.
    let mut pending: Vec<(usize, ExportRecord)> = Vec::with_capacity(PAGE as usize);
    let mut buffer: Vec<u8> = Vec::with_capacity(1024);
    let mut line_number = 0usize;

    loop {
        buffer.clear();
        let read = read_line_capped(&mut reader, &mut buffer)?;
        if read == Capped::End {
            break;
        }
        line_number += 1;
        if read == Capped::TooLong {
            totals.error(
                label,
                line_number,
                &format!(
                    "this line is longer than {MAX_LINE_BYTES} bytes and was skipped; a \
                     memory is at most {} bytes of body",
                    kiomon_core::MAX_BODY_BYTES
                ),
                None,
            );
            continue;
        }
        let raw = buffer.strip_suffix(b"\n").unwrap_or(&buffer);
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        let raw = raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(raw);
        if raw.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let text = match std::str::from_utf8(raw) {
            Ok(text) => text,
            Err(err) => {
                totals.error(
                    label,
                    line_number,
                    &format!("not valid UTF-8 (byte {})", err.valid_up_to()),
                    None,
                );
                continue;
            }
        };
        let record: ExportRecord = match serde_json::from_str(text) {
            Ok(record) => record,
            Err(err) => {
                totals.error(label, line_number, &err.to_string(), Some(text));
                continue;
            }
        };
        totals.records += 1;
        pending.push((line_number, record));
        if pending.len() >= PAGE as usize {
            import_page(db, args, label, std::mem::take(&mut pending), totals)?;
        }
    }
    if !pending.is_empty() {
        import_page(db, args, label, pending, totals)?;
    }
    Ok(())
}

/// Whether one line's read finished a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capped {
    /// A line was read.
    Ok,
    /// The line was over [`MAX_LINE_BYTES`]; the rest of it was discarded.
    TooLong,
    /// Nothing was left to read.
    End,
}

/// Read one line, refusing to buffer more than [`MAX_LINE_BYTES`].
///
/// A line that does not fit is drained to its newline before it is reported, so the next
/// read starts at a line boundary — otherwise one absurd line would be reported as a great
/// many absurd lines, and the file would produce more stderr than it has bytes.
fn read_line_capped<R: BufRead>(reader: &mut R, buffer: &mut Vec<u8>) -> std::io::Result<Capped> {
    // UFCS, not `bytes.take(n)`: the method form resolves through the deref to `Self = R`
    // and tries to move `R` out of its own reference. `Take<&mut R>` is `BufRead` because
    // `&mut R` is, which is the whole reason this can stay generic over the reader.
    let mut taken = std::io::Read::take(&mut *reader, MAX_LINE_BYTES as u64 + 1);
    let read = taken.read_until(b'\n', buffer)?;
    if read == 0 {
        return Ok(Capped::End);
    }
    if read <= MAX_LINE_BYTES {
        return Ok(Capped::Ok);
    }
    let mut sink: Vec<u8> = Vec::with_capacity(64 * 1024);
    loop {
        sink.clear();
        let drained = std::io::Read::take(&mut *reader, 64 * 1024).read_until(b'\n', &mut sink)?;
        if drained == 0 || sink.ends_with(b"\n") {
            break;
        }
    }
    buffer.clear();
    Ok(Capped::TooLong)
}

/// Validate a page of records and write the ones that are valid.
fn import_page(
    db: &kiomon_core::Database,
    args: &ImportArgs,
    label: &str,
    records: Vec<(usize, ExportRecord)>,
    totals: &mut Totals,
) -> Result<()> {
    let mut rows: Vec<ImportRow> = Vec::with_capacity(records.len());
    for (line, record) in records {
        match record.into_row(args.workspace.as_deref()) {
            Ok(row) => {
                for warning in &row.warnings {
                    out::warn(false, &format!("{label}:{line}: {warning}"));
                }
                rows.push(row);
            }
            Err(message) => totals.error(label, line, &message, None),
        }
    }
    if rows.is_empty() {
        return Ok(());
    }
    let batch = db.import_batch(&rows, args.dry_run)?;
    // After the commit, never inside it, and in the same shape `add` uses: an embedder is
    // a network call, and an import that failed because somebody's model was down would
    // have lost the file it was given. The rows are on disk and findable lexically;
    // `reindex --embed` comes back for them.
    if batch.committed {
        let ids = batch.written_ids();
        if db.embedder().is_some() {
            match db.embed_ids(&ids) {
                Ok(states) => {
                    let missing = states
                        .iter()
                        .filter(|state| **state != kiomon_core::EmbedState::Written)
                        .count();
                    if missing > 0 {
                        out::warn(
                            false,
                            &format!(
                                "{missing} of {exported} imported memories were not embedded;                                  `kiomon reindex --embed` retries them",
                                exported = ids.len()
                            ),
                        );
                    }
                }
                Err(error) => out::warn(
                    false,
                    &format!(
                        "imported, but not embedded: {error}. `kiomon reindex --embed` retries it."
                    ),
                ),
            }
        }
    }
    for workspace in &batch.created_workspaces {
        out::warn(
            false,
            &format!("created workspace `{workspace}` (it was not in this database)"),
        );
    }
    for outcome in &batch.outcomes {
        if outcome.state != ImportState::Written {
            continue;
        }
        totals.ids.push(outcome.id.clone());
    }
    totals.batch.absorb(batch);
    Ok(())
}

/// Where an import's rows come from.
enum Source {
    /// A JSONL file, or stdin when `None`.
    Jsonl(Option<PathBuf>),
    /// One markdown document.
    Markdown(PathBuf),
}

/// Everything the `path` argument can name.
struct Sources;

impl Sources {
    /// Resolve the argument into a list of things to read.
    ///
    /// A directory is walked, not globbed: notes live in trees, and a user pointing at
    /// their notes directory means "all of it". Dot-entries are skipped — `.git`, `.obsidian`,
    /// `.venv` are not note collections — and the walk stops descending past
    /// [`MAX_DEPTH`] so a symlink loop is a short answer rather than a long one.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] when the path does not exist, and [`Error::Io`] when a
    /// directory cannot be read.
    fn resolve(path: &str) -> Result<Vec<Source>> {
        if path == "-" {
            return Ok(vec![Source::Jsonl(None)]);
        }
        let path = PathBuf::from(path);
        let metadata = std::fs::metadata(&path)
            .map_err(|err| Error::NotFound(format!("could not read {}: {err}", path.display())))?;
        if metadata.is_file() {
            return Ok(classify(&path).into_iter().collect());
        }
        let mut out = Vec::new();
        walk(&path, 0, &mut out)?;
        if out.is_empty() {
            return Err(Error::NotFound(format!(
                "{} holds no .jsonl or .md files",
                path.display()
            )));
        }
        Ok(out)
    }
}

/// How deep a directory walk descends.
const MAX_DEPTH: usize = 8;

fn walk(directory: &Path, depth: usize, out: &mut Vec<Source>) -> Result<()> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let entries = std::fs::read_dir(directory).map_err(|err| {
        Error::Io(std::io::Error::new(
            err.kind(),
            format!("could not read {}: {err}", directory.display()),
        ))
    })?;
    let mut children: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(Error::Io)?;
        children.push(entry.path());
    }
    // Sorted so two runs of the same import touch the file in the same order — which is
    // what makes an interrupted import resumable by looking.
    children.sort();
    for child in children {
        let name = child
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        if name.starts_with('.') {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&child) else {
            continue;
        };
        if metadata.is_dir() {
            walk(&child, depth + 1, out)?;
        } else if let Some(source) = classify(&child) {
            out.push(source);
        }
    }
    Ok(())
}

fn classify(path: &Path) -> Option<Source> {
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase());
    match extension.as_deref() {
        Some("jsonl" | "ndjson" | "json") => Some(Source::Jsonl(Some(path.to_owned()))),
        Some("md" | "markdown") => Some(Source::Markdown(path.to_owned())),
        _ => None,
    }
}

/// The running counts of an import.
#[derive(Default)]
struct Totals {
    records: usize,
    errors: usize,
    suppressed: usize,
    ids: Vec<String>,
    dry_run: bool,
    batch: ImportBatch,
}

impl Totals {
    fn written(&self) -> usize {
        self.batch.written()
    }

    fn error(&mut self, label: &str, line: usize, message: &str, fragment: Option<&str>) {
        self.errors += 1;
        if self.errors > MAX_REPORTED_ERRORS {
            self.suppressed += 1;
            return;
        }
        let where_ = if line == 0 {
            label.to_owned()
        } else {
            format!("{label}:{line}")
        };
        let fragment = fragment
            .map(|text| format!(" — {}", out::truncate(text, 120)))
            .unwrap_or_default();
        eprintln!("kiomon: {where_}: {message}{fragment}");
    }

    fn report(&self) -> ImportReport {
        ImportReport {
            records: self.records,
            written: self.written(),
            exists: self
                .batch
                .outcomes
                .iter()
                .filter(|outcome| outcome.state == ImportState::Exists)
                .count(),
            duplicates: self
                .batch
                .outcomes
                .iter()
                .filter(|outcome| outcome.state == ImportState::Duplicate)
                .count(),
            errors: self.errors,
            suppressed: self.suppressed,
            created_workspaces: self.batch.created_workspaces.clone(),
            ids: self.ids.clone(),
            dry_run: self.dry_run,
        }
    }
}

/// What an import did, for the summary and for `--json`.
#[derive(serde::Serialize)]
pub struct ImportReport {
    /// How many lines parsed and validated.
    pub records: usize,
    /// How many memories were written.
    pub written: usize,
    /// How many were already here under the same id.
    pub exists: usize,
    /// How many were already here under another id with the same content.
    pub duplicates: usize,
    /// How many lines could not be read.
    pub errors: usize,
    /// How many of those errors were only counted, past the reporting cap.
    pub suppressed: usize,
    /// Workspaces the file named that this database did not have.
    pub created_workspaces: Vec<String>,
    /// The ids that were written, in order. The one thing a caller cannot recompute.
    pub ids: Vec<String>,
    /// Whether this was a dry run: the counts are what a real run would produce, and
    /// nothing was kept.
    pub dry_run: bool,
}

impl ImportReport {
    fn print_human(&self) {
        println!(
            "{} {} of {} memories ({} already here, {} duplicates, {} errors)",
            if self.dry_run {
                "Would import"
            } else {
                "Imported"
            },
            self.written,
            self.records,
            self.exists,
            self.duplicates,
            self.errors
        );
        if self.suppressed > 0 {
            println!(
                "  {} more line errors were counted but not printed",
                self.suppressed
            );
        }
        if !self.created_workspaces.is_empty() {
            println!(
                "  created workspaces: {}",
                self.created_workspaces.join(", ")
            );
        }
    }
}
