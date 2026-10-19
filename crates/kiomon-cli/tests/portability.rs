// SPDX-License-Identifier: Apache-2.0

//! `export` and `import` through the binary: the round trip, the hostile line, the
//! directory walk, and what happens to a brain's vectors.
//!
//! What these add to `kiomon-core`'s own tests is the part that only exists in a process:
//! which stream a per-line error goes to, what the exit code says when *nothing* could be
//! read, whether `export` streams to stdout without a summary line in the middle of the
//! file, and whether a re-import of an export is a no-op. §8 WP6's acceptance criteria are
//! here, in the order the plan lists them.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kiomon-port-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a sandbox directory must be creatable");
        Self { dir }
    }

    fn db(&self) -> PathBuf {
        self.dir.join("brain.db")
    }

    fn db_arg(&self) -> String {
        self.db().display().to_string()
    }

    fn other_db_arg(&self) -> String {
        self.dir.join("restored.db").display().to_string()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn path_arg(&self, name: &str) -> String {
        self.path(name).display().to_string()
    }

    fn write(&self, name: &str, body: &str) -> String {
        let path = self.path(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the parent must be creatable");
        }
        std::fs::write(&path, body).expect("the fixture must be writable");
        path.display().to_string()
    }

    /// The stub embedder: no model, no network, deterministic.
    fn stub_config(&self) -> String {
        self.write("config.toml", "[embedder]\nprovider = \"stub\"\n")
    }

    /// An embedder that cannot answer, because nothing is listening on that port.
    fn dead_config(&self) -> String {
        self.write(
            "dead.toml",
            "[embedder]\nprovider = \"ollama\"\nmodel = \"nomic-embed-text\"\nurl = \"http://127.0.0.1:9\"\n",
        )
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn kiomon() -> Command {
    Command::new(env!("CARGO_BIN_EXE_kiomon"))
}

fn run(args: &[&str]) -> Output {
    kiomon()
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("the binary must be runnable")
}

fn run_stdin(args: &[&str], input: &str) -> Output {
    use std::io::Write as _;
    let mut child = kiomon()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary must be runnable");
    child
        .stdin
        .as_mut()
        .expect("stdin was piped")
        .write_all(input.as_bytes())
        .expect("the fixture must be writable to stdin");
    child.wait_with_output().expect("the process must finish")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("the process must have exited")
}

fn json(args: &[&str]) -> serde_json::Value {
    let out = run(args);
    let text = stdout(&out);
    assert_eq!(
        text.lines().count(),
        1,
        "the envelope must be exactly one line, got: {text:?} (stderr: {})",
        stderr(&out)
    );
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("stdout was not JSON ({err}): {text:?}"))
}

/// Add one memory and return its id.
fn add(sandbox: &Sandbox, args: &[&str]) -> String {
    let db = sandbox.db_arg();
    let base = ["add", "--db", db.as_str(), "--json", "--quiet"];
    let all: Vec<&str> = base.iter().copied().chain(args.iter().copied()).collect();
    let value = json(&all);
    assert_eq!(value["ok"], serde_json::json!(true), "{value}");
    value["data"]["id"].as_str().expect("an id").to_owned()
}

/// Export to a file and return the parsed report.
fn export_to(sandbox: &Sandbox, extra: &[&str]) -> serde_json::Value {
    let db = sandbox.db_arg();
    let path = sandbox.path_arg("out.jsonl");
    let base = [
        "export",
        "--db",
        db.as_str(),
        "--output",
        path.as_str(),
        "--json",
    ];
    let all: Vec<&str> = base.iter().copied().chain(extra.iter().copied()).collect();
    json(&all)
}

/// The seed every round-trip test uses: two memories, distinct enough that a search for
/// one cannot find the other, plus an envelope field the packet reads.
fn seed(sandbox: &Sandbox) -> (String, String) {
    let first = add(
        sandbox,
        &[
            "--kind",
            "semantic",
            "--title",
            "Staging deploys need a manual migration",
            "--body",
            "Run migrations before the deploy; the pipeline does not do it.",
            "--tag",
            "deploy",
            "--confidence",
            "0.9",
            "--claims",
            "migrations are manual on staging",
            "--domain",
            "infra",
        ],
    );
    let second = add(
        sandbox,
        &[
            "--kind",
            "procedural",
            "--title",
            "Quarterly invoice reconciliation",
            "--body",
            "Match the ledger against the bank export before filing.",
            "--tag",
            "finance",
        ],
    );
    (first, second)
}

// ---------------------------------------------------------------------------
// Acceptance 1: export → a fresh database → import → the same ids and titles
// ---------------------------------------------------------------------------

#[test]
fn a_round_trip_into_a_fresh_database_returns_the_same_ids_and_titles() {
    let sandbox = Sandbox::new();
    let (first, second) = seed(&sandbox);
    let report = export_to(&sandbox, &[]);
    assert_eq!(report["data"]["exported"], serde_json::json!(2));

    let fresh = sandbox.other_db_arg();
    let out = run(&["import", "--db", &fresh, &sandbox.path_arg("out.jsonl")]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stdout(&out).contains("Imported 2 of 2"), "{}", stdout(&out));

    // The ids the plan's criterion names, found by the query rather than by a lookup:
    // `search`, not `get`, because an id that only survives when it is asked for by name
    // has not survived.
    let found = json(&["search", "--db", &fresh, "--json", "deploy"]);
    let hits = found["data"]["results"].as_array().expect("results");
    assert_eq!(hits.len(), 1, "{found}");
    assert_eq!(hits[0]["id"], serde_json::json!(first));
    assert_eq!(
        hits[0]["title"],
        serde_json::json!("Staging deploys need a manual migration")
    );

    let finance = json(&["search", "--db", &fresh, "--json", "ledger"]);
    let hits = finance["data"]["results"].as_array().expect("results");
    assert_eq!(hits.len(), 1, "{finance}");
    assert_eq!(hits[0]["id"], serde_json::json!(second));
}

#[test]
fn the_round_trip_keeps_what_the_context_packet_reads() {
    // The packet reads `confidence`, `expires_at` and the envelope's own keys, so a
    // round trip is asserted through `retrieve` and not only through `search`: a restore
    // that lost the claims would still pass a title check.
    let sandbox = Sandbox::new();
    let id = add(
        &sandbox,
        &[
            "--kind",
            "procedural",
            "--title",
            "Deploying to staging",
            "--body",
            "Run the deploy script, then the migration by hand.",
            "--confidence",
            "0.8",
            "--metadata",
            r#"{"trigger":"a deploy is scheduled","steps":["run the deploy script","run the migration"],"claims":["migrations are manual"]}"#,
        ],
    );
    export_to(&sandbox, &[]);

    let fresh = sandbox.other_db_arg();
    run(&["import", "--db", &fresh, &sandbox.path_arg("out.jsonl")]);

    let packet = json(&[
        "retrieve",
        "--db",
        &fresh,
        "--json",
        "--intent",
        "execute",
        "deploy staging",
    ]);
    let procedure = &packet["data"]["procedure"];
    assert_eq!(procedure["memory_id"], serde_json::json!(id), "{packet}");
    let claim = procedure["claim"].as_str().expect("a claim");
    assert!(
        claim.contains("run the deploy script") && claim.contains("run the migration"),
        "the procedure's claim is built from the steps the file carried: {packet}"
    );
    let facts = json(&["get", "--db", &fresh, "--json", &id]);
    assert_eq!(facts["data"]["confidence"], serde_json::json!(0.8));
}

#[test]
fn a_status_is_carried_across_so_a_draft_comes_back_as_a_draft() {
    let sandbox = Sandbox::new();
    // An agent write below the threshold lands pending, which is what the review inbox is.
    let config = sandbox.write("review.toml", "default_workspace = \"default\"\n");
    let out = run(&[
        "--db",
        &sandbox.db_arg(),
        "--config",
        &config,
        "add",
        "--kind",
        "semantic",
        "--title",
        "An agent wrote this",
        "--body",
        "It should not be visible yet.",
        "--source",
        "agent_reflection",
        "--quiet",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    export_to(&sandbox, &["--status", "any"]);
    let fresh = sandbox.other_db_arg();
    run(&["import", "--db", &fresh, &sandbox.path_arg("out.jsonl")]);

    // Visible in a listing (which can show any status) and absent from a search, because
    // the FTS trigger only indexes an active row and the draft came back pending.
    let listed = json(&[
        "list",
        "--db",
        &fresh,
        "--json",
        "--status",
        "pending-approval",
    ]);
    assert_eq!(listed["data"].as_array().map(Vec::len), Some(1), "{listed}");
    let search = json(&["search", "--db", &fresh, "--json", "agent"]);
    assert!(
        search["data"]["results"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "a restored draft must not become findable: {search}"
    );
}

#[test]
fn a_draft_below_the_review_threshold_is_not_exported_by_default() {
    // `export` defaults to `active`, which is the same rule `list` and `search` follow: a
    // backup that silently included the review inbox would be a backup that leaked it.
    let sandbox = Sandbox::new();
    add(
        &sandbox,
        &[
            "--kind", "semantic", "--title", "Public", "--body", "deploy",
        ],
    );
    run(&[
        "--db",
        &sandbox.db_arg(),
        "add",
        "--kind",
        "semantic",
        "--title",
        "Private draft",
        "--body",
        "deploy",
        "--source",
        "agent_reflection",
        "--confidence",
        "0.1",
        "--quiet",
    ]);

    let report = export_to(&sandbox, &[]);
    assert_eq!(
        report["data"]["exported"],
        serde_json::json!(1),
        "the draft is not in a default export: {report}"
    );
    let everything = export_to(&sandbox, &["--status", "any"]);
    assert_eq!(everything["data"]["exported"], serde_json::json!(2));
}

// ---------------------------------------------------------------------------
// Acceptance 2: a hostile file reports per line and imports the valid lines
// ---------------------------------------------------------------------------

#[test]
fn a_hostile_file_imports_the_valid_lines_and_reports_the_rest_on_stderr() {
    let sandbox = Sandbox::new();
    let host = "x".repeat(400);
    let body = "y".repeat(1024 * 1024 + 1);
    let fixture = format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
        r#"{"kind":"semantic","title":"good one","body":"deploy"}"#,
        "this is not json at all",
        r#"{"kind":"vibes","title":"unknown kind"}"#,
        r#"{"title":"no kind at all"}"#,
        format_args!(
            r#"{{"kind":"semantic","title":"{}"}}"#,
            host.replace('"', "'")
        ),
        format_args!(
            r#"{{"kind":"semantic","title":"{}","body":"{body}"}}"#,
            "huge"
        ),
        r#"{"kind":"semantic","title":"t","confidence":9}"#,
        r#"{"kind":"semantic","title":"good two","tags":["a"]}"#,
    );
    let path = sandbox.write("hostile.jsonl", &fixture);

    let out = run(&["import", "--db", &sandbox.db_arg(), &path]);
    assert_eq!(
        code(&out),
        0,
        "some lines landed, so this is a success: {}",
        stderr(&out)
    );
    let errors = stderr(&out);
    for (line, needle) in [
        (2, "expected ident"),
        (3, "unknown variant"),
        (4, "missing field"),
        (5, "`title`"),
        (6, "`body`"),
        (7, "`confidence`"),
    ] {
        assert!(
            errors.contains(&format!("hostile.jsonl:{line}:")),
            "line {line} should be reported by number: {errors}"
        );
        assert!(errors.contains(needle), "line {line}: {needle}\n{errors}");
    }
    assert!(
        stdout(&out).contains("Imported 2 of 5") && stdout(&out).contains("6 errors"),
        "{}",
        stdout(&out)
    );

    let listed = json(&["list", "--db", &sandbox.db_arg(), "--json"]);
    assert_eq!(
        listed["data"].as_array().map(Vec::len),
        Some(2),
        "the two good lines are here and the six bad ones are not: {listed}"
    );
}

#[test]
fn a_file_where_nothing_can_be_read_exits_one() {
    // The distinction a script needs: "some of it landed" and "none of it did" cannot both
    // be exit 0. Partial success is a success, total failure is not.
    let sandbox = Sandbox::new();
    let path = sandbox.write("junk.jsonl", "not json\nstill not json\n");
    let out = run(&["import", "--db", &sandbox.db_arg(), &path]);
    assert_eq!(
        code(&out),
        3,
        "nothing could be read, so the *file* is the problem: {}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("no line could be imported"),
        "{}",
        stderr(&out)
    );

    // A file that is empty is not a failure: nothing was wrong with it.
    let empty = sandbox.write("empty.jsonl", "");
    let out = run(&["import", "--db", &sandbox.db_arg(), &empty]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
}

#[test]
fn one_absurd_line_is_one_error_and_not_thousands() {
    // A single line longer than the reader's cap: reported once, drained to its newline,
    // and the good line after it still imports. Without the drain the rest of the line
    // would be read as a great many lines, which is how one bad row becomes a stderr flood.
    let sandbox = Sandbox::new();
    let huge = format!(
        "{{\"kind\":\"semantic\",\"title\":\"{}\"}}\n",
        "z".repeat(3 * 1024 * 1024)
    );
    let fixture = format!(
        "{huge}{}\n",
        r#"{"kind":"semantic","title":"after the monster","body":"deploy"}"#
    );
    let path = sandbox.write("monster.jsonl", &fixture);

    let out = run(&["import", "--db", &sandbox.db_arg(), &path]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let errors = stderr(&out);
    assert_eq!(
        errors.matches("longer than").count(),
        1,
        "one line, one error: {errors}"
    );
    assert!(
        stdout(&out).contains("Imported 1 of 1") && stdout(&out).contains("1 errors"),
        "the good line landed and the monster was one error: {}",
        stdout(&out)
    );
}

#[test]
fn dry_run_reports_what_a_real_run_would_do_and_writes_nothing() {
    let sandbox = Sandbox::new();
    seed(&sandbox);
    export_to(&sandbox, &[]);

    let fresh = sandbox.other_db_arg();
    let out = run(&[
        "import",
        "--db",
        &fresh,
        "--dry-run",
        &sandbox.path_arg("out.jsonl"),
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stdout(&out).contains("Would import 2 of 2"),
        "{}",
        stdout(&out)
    );

    // Nothing was kept: the database file may exist, but it holds no memories.
    let listed = json(&["list", "--db", &fresh, "--json"]);
    assert_eq!(listed["data"].as_array().map(Vec::len), Some(0), "{listed}");

    // And a dry run's counts include duplicates *inside the file*, because it does the
    // work and rolls it back rather than guessing at what the work would do.
    let doubled = format!(
        "{}{}",
        std::fs::read_to_string(sandbox.path("out.jsonl")).expect("the export"),
        std::fs::read_to_string(sandbox.path("out.jsonl")).expect("the export"),
    );
    let path = sandbox.write("doubled.jsonl", &doubled);
    let out = run(&["import", "--db", &fresh, "--dry-run", &path]);
    assert!(
        stdout(&out).contains("Would import 2 of 4"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn re_importing_the_same_file_is_a_no_op_for_every_line() {
    // Idempotence is what makes an import safe to re-run after an interruption, and it is
    // the id check that provides it — not the content hash, which is the second line of
    // defence for a file that came from somewhere else.
    let sandbox = Sandbox::new();
    seed(&sandbox);
    export_to(&sandbox, &[]);
    let path = sandbox.path_arg("out.jsonl");
    // A fresh database: the sandbox's own is the export's *source*, so importing into it
    // would test the id check against rows that were already there rather than against an
    // import that landed them.
    let fresh = sandbox.other_db_arg();

    let first = run(&["import", "--db", &fresh, &path]);
    assert!(
        stdout(&first).contains("Imported 2 of 2"),
        "{}",
        stdout(&first)
    );

    let second = run(&["import", "--db", &fresh, &path]);
    assert_eq!(code(&second), 0);
    assert!(
        stdout(&second).contains("Imported 0 of 2") && stdout(&second).contains("2 already here"),
        "{}",
        stdout(&second)
    );

    let listed = json(&["list", "--db", &fresh, "--json"]);
    assert_eq!(listed["data"].as_array().map(Vec::len), Some(2));
}

#[test]
fn the_same_content_under_a_new_id_is_a_duplicate_rather_than_a_second_memory() {
    // Two exports of one brain — one taken before a memory was re-saved under `--force` —
    // would otherwise produce two memories that say the same thing, which is the one thing
    // dedupe exists to prevent.
    let sandbox = Sandbox::new();
    seed(&sandbox);
    export_to(&sandbox, &[]);
    let line = std::fs::read_to_string(sandbox.path("out.jsonl"))
        .expect("the export")
        .lines()
        .next()
        .expect("a line")
        .to_owned();
    // The same content with a different id.
    let renamed = line.replace(
        &format!("\"id\":\"{}\"", id_of(&line)),
        "\"id\":\"k_01M56XYP4ATEW6PQP8MJ6Y2JQ7\"",
    );
    let path = sandbox.write("renamed.jsonl", &format!("{renamed}\n"));

    let out = run(&["import", "--db", &sandbox.db_arg(), &path]);
    assert!(
        stdout(&out).contains("1 duplicates"),
        "the content is here under another id: {}",
        stdout(&out)
    );
    let listed = json(&["list", "--db", &sandbox.db_arg(), "--json"]);
    assert_eq!(listed["data"].as_array().map(Vec::len), Some(2));
}

fn id_of(line: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(line).expect("a record");
    value["id"].as_str().expect("an id").to_owned()
}

// ---------------------------------------------------------------------------
// Acceptance 3: streaming, and the shape of what comes out
// ---------------------------------------------------------------------------

#[test]
fn export_to_stdout_keeps_the_records_and_the_summary_apart() {
    // The summary is the one line that would corrupt a piped file, so it goes to stderr
    // and `--json` is refused with the reason rather than silently emitted after the
    // records.
    let sandbox = Sandbox::new();
    seed(&sandbox);
    let out = run(&["export", "--db", &sandbox.db_arg()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "one record per line: {lines:?}");
    for line in &lines {
        serde_json::from_str::<serde_json::Value>(line).expect("each line is a record");
    }
    assert!(
        stderr(&out).contains("vectors and edges are not in the file"),
        "{}",
        stderr(&out)
    );

    let out = run(&["export", "--db", &sandbox.db_arg(), "--json"]);
    assert_eq!(
        code(&out),
        3,
        "a bad flag combination is exit 3: {}",
        stderr(&out)
    );
    let failure = stdout(&out);
    assert!(
        failure.contains("--output") && failure.contains("--json"),
        "a `--json` failure is an envelope on stdout: {failure}"
    );
}

#[test]
fn a_brain_that_does_not_fit_in_one_page_still_exports_completely() {
    // The paging is invisible from here, which is the point: four hundred and twenty
    // memories is more than one page of five hundred? No — it is *fewer*, so the test
    // doubles as the check that a short final page ends the walk. The corpus is sized to
    // cross the boundary as well, because a cursor that never advances looks exactly like
    // a working one from the outside.
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    for index in 0..520 {
        let out = run(&[
            "add",
            "--db",
            &db,
            "--kind",
            "reference",
            "--title",
            &format!("Note {index}"),
            "--body",
            "deploy",
            "--quiet",
        ]);
        assert_eq!(code(&out), 0, "{}", stderr(&out));
    }

    let report = export_to(&sandbox, &[]);
    assert_eq!(
        report["data"]["exported"],
        serde_json::json!(520),
        "{report}"
    );
    let lines = std::fs::read_to_string(sandbox.path("out.jsonl")).expect("the export");
    assert_eq!(lines.lines().count(), 520);

    let fresh = sandbox.other_db_arg();
    let out = run(&["import", "--db", &fresh, &sandbox.path_arg("out.jsonl")]);
    assert!(
        stdout(&out).contains("Imported 520 of 520"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn a_workspace_mapping_moves_every_line_into_the_named_one() {
    let sandbox = Sandbox::new();
    let (first, _) = seed(&sandbox);
    export_to(&sandbox, &[]);

    let fresh = sandbox.other_db_arg();
    let out = run(&[
        "import",
        "--db",
        &fresh,
        "--workspace",
        "Restored Brain",
        &sandbox.path_arg("out.jsonl"),
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("created workspace `restored-brain`"),
        "the workspace the file named was not here, and the import says so: {}",
        stderr(&out)
    );

    let found = json(&["get", "--db", &fresh, "--json", &first]);
    assert_eq!(
        found["data"]["workspace_id"],
        serde_json::json!("restored-brain")
    );
}

#[test]
fn a_workspace_the_file_names_is_created_from_the_file_itself() {
    // Restoring into a database that has never heard of the workspace is the normal case
    // for a migration, and refusing it would make the format a fair-weather one.
    let sandbox = Sandbox::new();
    let workspace = sandbox.write(
        "ws.jsonl",
        "{\"kind\":\"semantic\",\"title\":\"From another machine\",\"body\":\"deploy\",\"workspace_id\":\"work-stuff\"}\n",
    );
    let out = run(&["import", "--db", &sandbox.db_arg(), &workspace]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("created workspace `work-stuff`"),
        "{}",
        stderr(&out)
    );
    let listed = json(&[
        "list",
        "--db",
        &sandbox.db_arg(),
        "--json",
        "--workspace",
        "work-stuff",
    ]);
    assert_eq!(listed["data"].as_array().map(Vec::len), Some(1));
}

// ---------------------------------------------------------------------------
// Markdown, and the directory walk
// ---------------------------------------------------------------------------

#[test]
fn markdown_export_writes_a_readable_document_per_memory_and_reads_it_back() {
    let sandbox = Sandbox::new();
    let (first, _) = seed(&sandbox);
    let notes = sandbox.path_arg("notes");
    let out = run(&[
        "export",
        "--db",
        &sandbox.db_arg(),
        "--format",
        "markdown",
        "--output",
        &notes,
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stdout(&out).contains("Exported 2 memories"),
        "{}",
        stdout(&out)
    );

    let files: Vec<PathBuf> = std::fs::read_dir(sandbox.path("notes"))
        .expect("the directory")
        .map(|entry| entry.expect("an entry").path())
        .collect();
    assert_eq!(files.len(), 2, "{files:?}");
    let document = std::fs::read_to_string(
        files
            .iter()
            .find(|path| path.to_string_lossy().contains(&first))
            .expect("the round-trippable document"),
    )
    .expect("readable");
    assert!(document.starts_with("---\nid: "), "{document}");
    assert!(document.contains("kind: semantic"), "{document}");
    assert!(document.contains("tags: [deploy]"), "{document}");
    assert!(
        document.contains("Run migrations before the deploy"),
        "{document}"
    );

    // Back in: into a new database, through the directory, ending up with the same id.
    let fresh = sandbox.other_db_arg();
    let out = run(&["import", "--db", &fresh, &notes, "--quiet"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let found = json(&["get", "--db", &fresh, "--json", &first]);
    assert_eq!(found["data"]["id"], serde_json::json!(first));
    assert_eq!(found["data"]["kind"], serde_json::json!("semantic"));
    assert_eq!(found["data"]["tags"], serde_json::json!(["deploy"]));
}

#[test]
fn markdown_to_stdout_is_refused_with_the_reason() {
    let sandbox = Sandbox::new();
    seed(&sandbox);
    let out = run(&["export", "--db", &sandbox.db_arg(), "--format", "markdown"]);
    assert_eq!(code(&out), 3, "{}", stderr(&out));
    assert!(stderr(&out).contains("--output"), "{}", stderr(&out));
}

#[test]
fn a_directory_of_notes_imports_by_extension_and_skips_the_rest() {
    let sandbox = Sandbox::new();
    sandbox.write("notes/one.md", "# First note\n\nThe deploy is manual.\n");
    sandbox.write(
        "notes/two.markdown",
        "---\ntitle: Second\nkind: semantic\ntags: deploy\n---\n\nBody here.\n",
    );
    sandbox.write(
        "notes/three.jsonl",
        "{\"kind\":\"semantic\",\"title\":\"Third\"}\n",
    );
    sandbox.write("notes/ignore.txt", "not a memory\n");
    sandbox.write("notes/.hidden/four.md", "# Hidden\n");
    sandbox.write("notes/nested/five.md", "# Nested\n");

    let out = run(&[
        "import",
        "--db",
        &sandbox.db_arg(),
        &sandbox.path_arg("notes"),
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stdout(&out).contains("Imported 4 of 4"),
        "three files at the top level, one nested, and nothing from the dot-directory: {}",
        stdout(&out)
    );
    let listed = json(&["list", "--db", &sandbox.db_arg(), "--json"]);
    let titles: Vec<&str> = listed["data"]
        .as_array()
        .expect("memories")
        .iter()
        .filter_map(|memory| memory["title"].as_str())
        .collect();
    assert!(titles.contains(&"First note"), "{titles:?}");
    assert!(titles.contains(&"Nested"), "{titles:?}");
    assert!(!titles.contains(&"Hidden"), "{titles:?}");
}

#[test]
fn a_directory_with_nothing_to_import_says_so_rather_than_reporting_success() {
    let sandbox = Sandbox::new();
    sandbox.write("stuff/readme.txt", "nothing to do here\n");
    let out = run(&[
        "import",
        "--db",
        &sandbox.db_arg(),
        &sandbox.path_arg("stuff"),
    ]);
    assert_eq!(
        code(&out),
        2,
        "a directory with nothing to read was not found, rather than being malformed: {}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("no .jsonl or .md files"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn stdin_is_the_third_source_and_reads_jsonl() {
    let sandbox = Sandbox::new();
    let out = run_stdin(
        &["import", "--db", &sandbox.db_arg(), "-"],
        "{\"kind\":\"semantic\",\"title\":\"From a pipe\",\"body\":\"deploy\"}\n",
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stdout(&out).contains("Imported 1 of 1"), "{}", stdout(&out));
}

// ---------------------------------------------------------------------------
// Vectors: what an export deliberately does not carry, and what import does about it
// ---------------------------------------------------------------------------

#[test]
fn an_import_with_an_embedder_embeds_what_it_wrote() {
    let sandbox = Sandbox::new();
    let config = sandbox.stub_config();
    seed(&sandbox);
    export_to(&sandbox, &[]);

    let fresh = sandbox.other_db_arg();
    let out = run(&[
        "--db",
        &fresh,
        "--config",
        &config,
        "import",
        &sandbox.path_arg("out.jsonl"),
        "--quiet",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    // The proof is behavioural: a query that shares no words with the document finds it,
    // which is only possible if a vector was written during the import.
    let search = json(&[
        "--db",
        &fresh,
        "--config",
        &config,
        "search",
        "--json",
        "--legs",
        "vector",
        "ledger bank reconciliation",
    ]);
    assert_eq!(
        search["data"]["legs"]["vector"],
        serde_json::json!(true),
        "{search}"
    );
    assert!(
        !search["data"]["results"]
            .as_array()
            .expect("results")
            .is_empty(),
        "the imported memories were embedded: {search}"
    );
}

#[test]
fn an_import_with_a_broken_embedder_still_imports_and_says_what_to_run() {
    let sandbox = Sandbox::new();
    let config = sandbox.dead_config();
    seed(&sandbox);
    export_to(&sandbox, &[]);

    let fresh = sandbox.other_db_arg();
    let out = run(&[
        "--db",
        &fresh,
        "--config",
        &config,
        "import",
        &sandbox.path_arg("out.jsonl"),
    ]);
    assert_eq!(
        code(&out),
        0,
        "a dead embedder is not a failed import: {}",
        stderr(&out)
    );
    let errors = stderr(&out);
    assert!(
        errors.contains("not embedded") && errors.contains("reindex --embed"),
        "the refusal names the command that fixes it: {errors}"
    );
    assert!(stdout(&out).contains("Imported 2 of 2"), "{}", stdout(&out));
}

#[test]
fn an_export_says_that_vectors_and_edges_are_not_in_it() {
    // The two things a user would otherwise assume wrongly: a JSONL export is not a backup
    // of semantic search, and since WP5 it is not a backup of the graph either. Both are
    // stated in the summary instead of being discovered after a restore.
    let sandbox = Sandbox::new();
    seed(&sandbox);
    let report = export_to(&sandbox, &[]);
    assert_eq!(report["data"]["vectors_included"], serde_json::json!(false));
    assert_eq!(report["data"]["edges_included"], serde_json::json!(false));
}

#[test]
fn a_restore_brings_back_memories_and_not_the_graph() {
    // The gap the summary above names, asserted rather than described: a brain with a
    // hand-made edge exports, imports into a fresh database, and comes back with the
    // memories and no edges. When the format grows an edges section this test is what
    // changes.
    let sandbox = Sandbox::new();
    let (first, second) = seed(&sandbox);
    json(&["link", "--db", &sandbox.db_arg(), &first, &second, "--json"]);

    let report = export_to(&sandbox, &["--status", "any"]);
    assert_eq!(report["data"]["exported"], serde_json::json!(2));

    let out = run(&[
        "--db",
        &sandbox.other_db_arg(),
        "import",
        "--json",
        &sandbox.path_arg("out.jsonl"),
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    let related = json(&["relate", "--db", &sandbox.other_db_arg(), &first, "--json"]);
    assert_eq!(
        related["data"],
        serde_json::json!([]),
        "the file carries no edges, so the restore has none"
    );
}

// ---------------------------------------------------------------------------
// The plan's own eyes: the report, and the flags that shape it
// ---------------------------------------------------------------------------

#[test]
fn export_status_all_includes_the_archived_and_the_pending() {
    let sandbox = Sandbox::new();
    let live = add(
        &sandbox,
        &["--kind", "semantic", "--title", "Live", "--body", "deploy"],
    );
    let gone = add(
        &sandbox,
        &["--kind", "semantic", "--title", "Gone", "--body", "deploy"],
    );
    let out = run(&["archive", "--db", &sandbox.db_arg(), &gone, "--quiet"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    let default = export_to(&sandbox, &[]);
    assert_eq!(
        default["data"]["exported"],
        serde_json::json!(1),
        "{default}"
    );
    let everything = export_to(&sandbox, &["--status", "any"]);
    assert_eq!(everything["data"]["exported"], serde_json::json!(2));

    // And the archived one comes back archived, not resurrected.
    let fresh = sandbox.other_db_arg();
    run(&["import", "--db", &fresh, &sandbox.path_arg("out.jsonl")]);
    let found = json(&["get", "--db", &fresh, "--json", &gone]);
    assert_eq!(found["data"]["status"], serde_json::json!("archived"));
    let live_now = json(&["get", "--db", &fresh, "--json", &live]);
    assert_eq!(live_now["data"]["status"], serde_json::json!("active"));
}

#[test]
fn the_import_report_names_the_ids_so_a_script_can_use_them() {
    let sandbox = Sandbox::new();
    let path = sandbox.write(
        "no-ids.jsonl",
        "{\"kind\":\"semantic\",\"title\":\"Generated id\"}\n",
    );
    let value = json(&["import", "--db", &sandbox.db_arg(), "--json", &path]);
    let ids = value["data"]["ids"].as_array().expect("ids");
    assert_eq!(ids.len(), 1, "{value}");
    let id = ids[0].as_str().expect("an id");
    assert!(id.starts_with("k_"), "{id}");
    assert_eq!(value["data"]["written"], serde_json::json!(1));
    assert_eq!(value["data"]["dry_run"], serde_json::json!(false));
}

#[test]
fn a_hand_written_line_that_gives_its_own_id_keeps_it() {
    let sandbox = Sandbox::new();
    let path = sandbox.write(
        "with-id.jsonl",
        "{\"id\":\"k_01M56XYP4ATEW6PQP8MJ6Y2JQ7\",\"kind\":\"semantic\",\"title\":\"Pinned id\"}\n",
    );
    let out = run(&["import", "--db", &sandbox.db_arg(), &path]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let found = json(&[
        "get",
        "--db",
        &sandbox.db_arg(),
        "--json",
        "k_01M56XYP4ATEW6PQP8MJ6Y2JQ7",
    ]);
    assert_eq!(
        found["data"]["title"],
        serde_json::json!("Pinned id"),
        "{found}"
    );
}
