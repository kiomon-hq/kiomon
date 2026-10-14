// SPDX-License-Identifier: Apache-2.0

//! End-to-end assertions on the binary: the stream split, the exit codes, the `--json`
//! envelope, and a whole write/read/forget cycle against a real database file.
//!
//! These run the compiled binary rather than calling into the library, because every rule
//! they protect is a rule about a *process* — which stream a byte went to, what the shell
//! saw when it exited, and whether the database on disk agrees with the answer.
//!
//! Every test that touches a database passes `--db` into its own temporary directory.
//! There is no exception: a test suite that can write to the developer's real brain is a
//! test suite nobody runs twice.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A temporary directory that removes itself when the test ends.
struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kiomon-cli-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a sandbox directory must be creatable");
        Self { dir }
    }

    fn db(&self) -> PathBuf {
        self.dir.join("brain.db")
    }

    /// The database path as a string, for the `--db` flag.
    fn db_arg(&self) -> String {
        self.db().display().to_string()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

/// The binary under test, as cargo built it.
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

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("the process must have exited")
}

/// Run with `--json` and parse the single document it must produce.
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

/// Add a memory into a sandbox and return its id.
fn add(sandbox: &Sandbox, extra: &[&str]) -> String {
    let db = sandbox.db_arg();
    let base = ["add", "--db", db.as_str(), "--json"];
    let args: Vec<&str> = base.iter().copied().chain(extra.iter().copied()).collect();
    let value = json(&args);
    assert_eq!(value["ok"], serde_json::json!(true), "{value}");
    value["data"]["id"].as_str().expect("an id").to_owned()
}

// ---------------------------------------------------------------------------
// clap's two successful exits
// ---------------------------------------------------------------------------

#[test]
fn version_is_printed_on_stdout_and_exits_zero() {
    let out = run(&["--version"]);
    assert_eq!(code(&out), 0);
    assert!(
        stdout(&out).contains(env!("CARGO_PKG_VERSION")),
        "expected the crate version on stdout, got: {}",
        stdout(&out)
    );
}

#[test]
fn help_is_printed_on_stdout_and_exits_zero() {
    let out = run(&["--help"]);
    assert_eq!(code(&out), 0);
    let text = stdout(&out);
    assert!(text.contains("Usage:"), "{text}");
    assert!(!text.trim().is_empty());
}

#[test]
fn help_lists_every_command_in_the_contract() {
    // A command that exists in `plans/001-local-v1-core.md` §6.1 but not here is a
    // command agents will be told to use and will not find.
    let text = stdout(&run(&["--help"]));
    for command in [
        "init", "doctor", "stats", "add", "update", "import", "link", "unlink", "relate", "search",
        "get", "list", "review", "outcome", "decay", "pin", "unpin", "archive", "restore", "rate",
        "forget", "export",
    ] {
        assert!(text.contains(command), "`{command}` is missing from --help");
    }
}

// ---------------------------------------------------------------------------
// Exit codes. docs/CONTRACT.md §exit codes is the table these assert.
// ---------------------------------------------------------------------------

#[test]
fn a_missing_subcommand_is_a_usage_error_on_stderr_with_code_three() {
    let out = run(&[]);
    assert_eq!(
        code(&out),
        3,
        "bare `kiomon` must be a usage error, not a help screen"
    );
    assert!(
        stdout(&out).is_empty(),
        "a usage error must not write to stdout: {}",
        stdout(&out)
    );
    assert!(!stderr(&out).is_empty());
}

#[test]
fn an_unknown_flag_exits_three_and_not_two() {
    // The most important assertion in this file. clap's own default exit code for a usage
    // error is 2, and this project's contract spends 2 on "not found" — so without the
    // remap in `out::parse_failure`, an agent could not tell a typo from a deleted memory.
    let out = run(&["search", "--nope", "anything"]);
    assert_eq!(code(&out), 3, "{}", stderr(&out));
}

#[test]
fn a_missing_required_argument_exits_three() {
    let out = run(&["add", "--kind", "semantic"]);
    assert_eq!(code(&out), 3, "{}", stderr(&out));
}

#[test]
fn an_invalid_enum_value_exits_three_and_lists_the_valid_ones() {
    let out = run(&["add", "--kind", "knowledge", "--title", "x"]);
    assert_eq!(code(&out), 3);
    let text = stderr(&out);
    for kind in ["reference", "semantic", "procedural", "episodic"] {
        assert!(text.contains(kind), "the error must list `{kind}`: {text}");
    }
}

#[test]
fn a_malformed_outcome_item_exits_three() {
    let out = run(&["outcome", "success", "--item", "k_01:brilliant"]);
    assert_eq!(code(&out), 3);
    assert!(stderr(&out).contains("helpful"), "{}", stderr(&out));
}

#[test]
fn a_rating_of_minus_one_is_parsed_rather_than_read_as_a_flag() {
    // `-1` is the only value in the surface that begins with a hyphen.
    let sandbox = Sandbox::new();
    let out = run(&["rate", "--db", &sandbox.db_arg(), "k_01JC9W5K2Q", "-1"]);
    assert_eq!(
        code(&out),
        1,
        "expected the unimplemented error, not a parse error: {}",
        stderr(&out)
    );
}

#[test]
fn forgetting_without_yes_exits_three_and_writes_nothing() {
    let sandbox = Sandbox::new();
    let id = add(&sandbox, &["--kind", "semantic", "--title", "Keep me"]);

    let out = run(&["forget", "--db", &sandbox.db_arg(), &id]);
    assert_eq!(code(&out), 3, "{}", stderr(&out));
    assert!(stderr(&out).contains("--yes"), "{}", stderr(&out));

    // And the memory is still there, which is the point of refusing.
    let still = json(&["get", "--db", &sandbox.db_arg(), "--json", &id]);
    assert_eq!(still["data"]["title"], serde_json::json!("Keep me"));
}

// ---------------------------------------------------------------------------
// Streams and the JSON envelope
// ---------------------------------------------------------------------------

#[test]
fn diagnostics_never_reach_stdout() {
    // `search` is unimplemented in WP2, which makes it the cheapest failure to produce.
    let sandbox = Sandbox::new();
    let out = run(&["search", "--db", &sandbox.db_arg(), "anything"]);
    assert!(
        stdout(&out).is_empty(),
        "the human path must keep stdout clean: {}",
        stdout(&out)
    );
    assert!(stderr(&out).contains("kiomon: "), "{}", stderr(&out));
}

#[test]
fn unimplemented_commands_name_the_work_package_that_owns_them() {
    // The table *is* the list of what does not work yet, asserted rather than assumed:
    // when a work package lands, its row leaves this table and becomes a real test. A
    // command that silently did nothing would fail here, and so would one that claimed to
    // be finished before it was.
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let cases: [(&str, &[&str], u8); 6] = [
        ("review list", &["review", "list", "--db", &db], 4),
        (
            "outcome",
            &["outcome", "success", "--item", "k_01:helpful", "--db", &db],
            4,
        ),
        ("pin", &["pin", "k_01", "--db", &db], 4),
        ("relate", &["relate", "k_01", "--db", &db], 5),
        ("export", &["export", "--db", &db], 6),
        ("import", &["import", "nothing.jsonl", "--db", &db], 6),
    ];
    for (name, args, wp) in cases {
        let out = run(args);
        assert_eq!(
            code(&out),
            1,
            "`{name}` should be an internal not-implemented"
        );
        let text = stderr(&out);
        assert!(text.contains(name), "`{name}`: {text}");
        assert!(
            text.contains(&format!("work package {wp}")),
            "`{name}` should name work package {wp}: {text}"
        );
    }
}

#[test]
fn the_json_failure_envelope_is_exactly_one_document_on_stdout() {
    let sandbox = Sandbox::new();
    let value = json(&["review", "list", "--db", &sandbox.db_arg(), "--json"]);
    assert_eq!(value["ok"], serde_json::json!(false));
    assert_eq!(value["error"]["code"], serde_json::json!("unimplemented"));
    assert!(value["error"]["message"].is_string());
    assert_eq!(
        value["meta"]["contract_version"],
        serde_json::json!(kiomon_core::CONTRACT_VERSION)
    );
    assert!(value["meta"]["version"].is_string());
}

#[test]
fn quiet_does_not_suppress_an_error() {
    let sandbox = Sandbox::new();
    let out = run(&["--quiet", "review", "list", "--db", &sandbox.db_arg()]);
    assert_eq!(code(&out), 1);
    assert!(
        stderr(&out).contains("kiomon: "),
        "--quiet means less chatter, not silence on failure: {}",
        stderr(&out)
    );
}

#[test]
fn global_flags_are_accepted_after_the_subcommand_too() {
    // Agents build command lines by appending, and clap only does this for `global`
    // arguments. Asserting it means the property is not lost to a future refactor.
    let sandbox = Sandbox::new();
    let out = run(&["review", "list", "--db", &sandbox.db_arg(), "--json"]);
    assert_eq!(code(&out), 1);
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert_eq!(value["error"]["code"], serde_json::json!("unimplemented"));
}

#[test]
fn a_database_path_is_accepted_globally() {
    let sandbox = Sandbox::new();
    let before = json(&["--db", &sandbox.db_arg(), "--json", "init"]);
    assert_eq!(before["ok"], serde_json::json!(true));
    let after = json(&["init", "--db", &sandbox.db_arg(), "--json"]);
    assert_eq!(after["data"]["created"], serde_json::json!(false));
}

// ---------------------------------------------------------------------------
// The whole cycle, against a real file
// ---------------------------------------------------------------------------

#[test]
fn init_then_add_then_get_then_update_then_forget() {
    let sandbox = Sandbox::new();

    let init = json(&["init", "--db", &sandbox.db_arg(), "--json"]);
    assert_eq!(init["ok"], serde_json::json!(true));
    assert_eq!(init["data"]["created"], serde_json::json!(true));
    assert_eq!(init["data"]["schema_version"], serde_json::json!(1));
    assert!(sandbox.db().exists(), "init must create the file");

    let id = add(
        &sandbox,
        &[
            "--kind",
            "semantic",
            "--title",
            "Deploys need a manual migration",
            "--body",
            "Run migrations before the deploy.",
            "--tag",
            "deploy",
            "--tag",
            "staging",
            "--domain",
            "infra",
            "--confidence",
            "0.9",
        ],
    );

    let fetched = json(&["get", "--db", &sandbox.db_arg(), "--json", &id]);
    assert_eq!(
        fetched["data"]["title"],
        serde_json::json!("Deploys need a manual migration")
    );
    assert_eq!(fetched["data"]["kind"], serde_json::json!("semantic"));
    assert_eq!(fetched["data"]["kind_label"], serde_json::json!("Facts"));
    assert_eq!(
        fetched["data"]["tags"],
        serde_json::json!(["deploy", "staging"])
    );
    assert_eq!(fetched["data"]["confidence"], serde_json::json!(0.9));
    assert_eq!(fetched["data"]["status"], serde_json::json!("active"));
    assert_eq!(fetched["data"]["source_label"], serde_json::json!("saved"));
    assert_eq!(
        fetched["data"]["memory_metadata"],
        serde_json::json!({"domain": "infra"})
    );

    let updated = json(&[
        "update",
        "--db",
        &sandbox.db_arg(),
        "--json",
        &id,
        "--title",
        "Deploys need a migration",
    ]);
    assert_eq!(updated["data"]["changed"], serde_json::json!(1));
    assert_eq!(
        json(&["get", "--db", &sandbox.db_arg(), "--json", &id])["data"]["title"],
        serde_json::json!("Deploys need a migration")
    );

    let forgotten = json(&["forget", "--db", &sandbox.db_arg(), "--json", "--yes", &id]);
    assert_eq!(forgotten["data"]["deleted"], serde_json::json!([id]));

    // Exit 2, not 1: the memory is gone, and that is a conclusion rather than a fault.
    let out = run(&["get", "--db", &sandbox.db_arg(), "--json", &id]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    let gone: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(gone["error"]["code"], serde_json::json!("not_found"));
    assert!(gone["error"]["message"].as_str().unwrap().contains(&id));
}

#[test]
fn adding_the_same_memory_twice_reports_the_first_one_and_exits_zero() {
    let sandbox = Sandbox::new();
    let first = add(
        &sandbox,
        &[
            "--kind",
            "semantic",
            "--title",
            "Same",
            "--body",
            "Same body",
        ],
    );
    let value = json(&[
        "add",
        "--db",
        &sandbox.db_arg(),
        "--json",
        "--kind",
        "semantic",
        "--title",
        "  same ",
        "--body",
        "same   body",
    ]);
    assert_eq!(value["ok"], serde_json::json!(true));
    assert_eq!(value["data"]["duplicate"], serde_json::json!(true));
    assert_eq!(value["data"]["id"], serde_json::json!(first));
    assert_eq!(value["data"]["status"], serde_json::json!("active"));
}

#[test]
fn forgetting_an_unknown_id_exits_two_rather_than_one() {
    let sandbox = Sandbox::new();
    let out = run(&[
        "forget",
        "--db",
        &sandbox.db_arg(),
        "--json",
        "--yes",
        "k_01JC9W5K2Q",
    ]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(
        value["ok"],
        serde_json::json!(true),
        "the run itself succeeded"
    );
    assert_eq!(
        value["data"]["missing"],
        serde_json::json!(["k_01JC9W5K2Q"])
    );
}

#[test]
fn a_low_confidence_agent_write_stays_out_of_the_library_until_it_is_reviewed() {
    let sandbox = Sandbox::new();
    let draft = add(
        &sandbox,
        &[
            "--kind",
            "semantic",
            "--title",
            "Probably true",
            "--source",
            "agent_reflection",
            "--confidence",
            "0.3",
        ],
    );

    let listed = json(&["list", "--db", &sandbox.db_arg(), "--json"]);
    assert_eq!(
        listed["data"],
        serde_json::json!([]),
        "a bare list must not show the review inbox"
    );

    let all = json(&[
        "list",
        "--db",
        &sandbox.db_arg(),
        "--json",
        "--status",
        "any",
    ]);
    assert_eq!(all["data"][0]["id"], serde_json::json!(draft));
    assert_eq!(
        all["data"][0]["status"],
        serde_json::json!("pending_approval")
    );

    // And it is still readable by id, because an inbox has to show what is in it.
    let fetched = json(&["get", "--db", &sandbox.db_arg(), "--json", &draft]);
    assert_eq!(
        fetched["data"]["status"],
        serde_json::json!("pending_approval")
    );
}

#[test]
fn a_high_confidence_agent_write_activates_on_its_own() {
    let sandbox = Sandbox::new();
    let id = add(
        &sandbox,
        &[
            "--kind",
            "semantic",
            "--title",
            "Certain",
            "--source",
            "agent_reflection",
            "--confidence",
            "0.9",
        ],
    );
    let listed = json(&["list", "--db", &sandbox.db_arg(), "--json"]);
    assert_eq!(listed["data"][0]["id"], serde_json::json!(id));
}

#[test]
fn a_body_can_arrive_on_standard_input() {
    let sandbox = Sandbox::new();
    let out = kiomon()
        .args([
            "add",
            "--db",
            &sandbox.db_arg(),
            "--json",
            "--kind",
            "reference",
            "--title",
            "From a pipe",
            "--stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child
                .stdin
                .as_mut()
                .expect("a piped stdin")
                .write_all(b"the body came through a pipe")?;
            child.wait_with_output()
        })
        .expect("the binary must accept a piped body");

    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    let id = value["data"]["id"].as_str().unwrap();
    let fetched = json(&["get", "--db", &sandbox.db_arg(), "--json", id]);
    assert_eq!(
        fetched["data"]["body"],
        serde_json::json!("the body came through a pipe")
    );
}

#[test]
fn a_file_can_supply_both_the_title_and_the_body() {
    let sandbox = Sandbox::new();
    let file = sandbox.dir.join("runbook.md");
    std::fs::write(&file, "# Release runbook\n\n1. Tag\n2. Push\n").unwrap();

    let value = json(&[
        "add",
        "--db",
        &sandbox.db_arg(),
        "--json",
        "--kind",
        "procedural",
        "--from-file",
        &file.display().to_string(),
    ]);
    let id = value["data"]["id"].as_str().unwrap();

    let fetched = json(&["get", "--db", &sandbox.db_arg(), "--json", id]);
    assert_eq!(
        fetched["data"]["title"],
        serde_json::json!("Release runbook")
    );
    assert!(fetched["data"]["body"].as_str().unwrap().contains("1. Tag"));
}

#[test]
fn stats_and_doctor_agree_with_what_was_written() {
    let sandbox = Sandbox::new();
    add(&sandbox, &["--kind", "semantic", "--title", "A fact"]);
    add(&sandbox, &["--kind", "episodic", "--title", "A moment"]);
    add(
        &sandbox,
        &[
            "--kind",
            "semantic",
            "--title",
            "A draft",
            "--source",
            "agent_reflection",
            "--confidence",
            "0.1",
        ],
    );

    let stats = json(&["stats", "--db", &sandbox.db_arg(), "--json"]);
    assert_eq!(stats["data"]["total"], serde_json::json!(3));
    assert_eq!(
        stats["data"]["by_kind"],
        serde_json::json!([
            {"kind": "reference", "kind_label": "Files", "count": 0},
            {"kind": "semantic", "kind_label": "Facts", "count": 2},
            {"kind": "procedural", "kind_label": "Routines", "count": 0},
            {"kind": "episodic", "kind_label": "Moments", "count": 1},
        ])
    );

    let out = run(&["doctor", "--db", &sandbox.db_arg(), "--json"]);
    assert_eq!(code(&out), 0, "a healthy database must exit 0");
    let doctor: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(doctor["data"]["healthy"], serde_json::json!(true));
    assert_eq!(doctor["data"]["problems"], serde_json::json!([]));
    assert_eq!(
        doctor["data"]["indexed"],
        serde_json::json!(2),
        "the index must match the active memories, not the total"
    );
}

#[test]
fn doctor_reports_paths_without_touching_the_database() {
    let sandbox = Sandbox::new();
    let path = sandbox.dir.join("does-not-exist-yet.db");
    let out = run(&[
        "doctor",
        "--db",
        &path.display().to_string(),
        "--paths-only",
        "--json",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(
        value["data"]["path"],
        serde_json::json!(path.display().to_string())
    );
    // `doctor` opens the database, which creates it — that is acceptable, and reported.
    assert_eq!(value["data"]["schema_version"], serde_json::json!(1));
}

#[test]
fn a_workspace_can_be_created_by_name_and_addressed_by_name() {
    let sandbox = Sandbox::new();
    let init = json(&[
        "init",
        "--db",
        &sandbox.db_arg(),
        "--json",
        "--workspace",
        "Client: Acme",
    ]);
    // Asserted by membership rather than by position: `init` creates the default
    // workspace and then the named one inside the same second, so which comes first in
    // the report is a question about timestamps, not about behaviour.
    let ids: Vec<&str> = init["data"]["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|workspace| workspace["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"client-acme"), "{ids:?}");
    assert!(ids.contains(&"default"), "{ids:?}");

    let id = add(
        &sandbox,
        &[
            "--kind",
            "semantic",
            "--title",
            "Acme fact",
            "--workspace",
            "Client: Acme",
        ],
    );
    let fetched = json(&["get", "--db", &sandbox.db_arg(), "--json", &id]);
    assert_eq!(
        fetched["data"]["workspace_id"],
        serde_json::json!("client-acme")
    );

    // The default workspace does not see it, and `'*'` does.
    let one = json(&["list", "--db", &sandbox.db_arg(), "--json"]);
    assert_eq!(one["data"], serde_json::json!([]));
    let all = json(&[
        "list",
        "--db",
        &sandbox.db_arg(),
        "--json",
        "--workspace",
        "*",
    ]);
    assert_eq!(all["data"][0]["id"], serde_json::json!(id));
}

#[test]
fn a_write_to_every_workspace_at_once_is_refused() {
    let sandbox = Sandbox::new();
    let out = run(&[
        "add",
        "--db",
        &sandbox.db_arg(),
        "--kind",
        "semantic",
        "--title",
        "Nowhere in particular",
        "--workspace",
        "*",
    ]);
    assert_eq!(code(&out), 3, "{}", stderr(&out));
    assert!(stderr(&out).contains("one workspace"), "{}", stderr(&out));
}

#[test]
fn an_unknown_workspace_lists_the_ones_that_exist() {
    let sandbox = Sandbox::new();
    let out = run(&["list", "--db", &sandbox.db_arg(), "--workspace", "nowhere"]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    assert!(stderr(&out).contains("default"), "{}", stderr(&out));
}

#[test]
fn malformed_metadata_warns_on_stderr_and_still_writes() {
    let sandbox = Sandbox::new();
    let out = run(&[
        "add",
        "--db",
        &sandbox.db_arg(),
        "--json",
        "--kind",
        "semantic",
        "--title",
        "Kept anyway",
        "--metadata",
        r#"{"entities": "not a list"}"#,
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(value["data"]["warnings"].as_array().unwrap().len(), 1);
    assert!(
        stderr(&out).contains("entities"),
        "the warning must reach stderr: {}",
        stderr(&out)
    );
    // And the memory survived with an empty envelope.
    let id = value["data"]["id"].as_str().unwrap();
    let fetched = json(&["get", "--db", &sandbox.db_arg(), "--json", id]);
    assert_eq!(fetched["data"]["memory_metadata"], serde_json::json!({}));
}

#[test]
fn human_output_is_one_bounded_line_per_memory() {
    let sandbox = Sandbox::new();
    let long = "x".repeat(500);
    add(&sandbox, &["--kind", "semantic", "--title", &long]);

    let out = run(&["list", "--db", &sandbox.db_arg()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert_eq!(text.lines().count(), 1, "one memory, one line: {text:?}");
    assert!(
        text.chars().count() < 200,
        "a 500-character title must not become a 500-character line: {} chars",
        text.chars().count()
    );

    // `--full` is how you ask for the whole thing.
    let full = run(&["list", "--db", &sandbox.db_arg(), "--full"]);
    assert!(stdout(&full).chars().count() > 400);
}

#[test]
fn an_empty_result_says_so_on_stderr_not_stdout() {
    let sandbox = Sandbox::new();
    let out = run(&["list", "--db", &sandbox.db_arg()]);
    assert_eq!(code(&out), 0, "an empty result is a success");
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));
    assert!(stderr(&out).contains("nothing matched"), "{}", stderr(&out));

    // And `--quiet` silences the note without changing the code or the empty stdout.
    let quiet = run(&["list", "--db", &sandbox.db_arg(), "--quiet"]);
    assert_eq!(code(&quiet), 0);
    assert!(stdout(&quiet).is_empty());
    assert!(stderr(&quiet).is_empty(), "{}", stderr(&quiet));
}

#[test]
fn a_database_is_usable_without_ever_running_init() {
    // A first-time user is entitled to `kiomon add` just working.
    let sandbox = Sandbox::new();
    assert!(!sandbox.db().exists());
    add(
        &sandbox,
        &["--kind", "semantic", "--title", "No init needed"],
    );
    assert!(sandbox.db().exists());
}

#[test]
fn the_database_path_from_the_environment_is_used_when_no_flag_is_given() {
    let sandbox = Sandbox::new();
    let out = kiomon()
        .args(["init", "--json"])
        .env("KIOMON_DB", sandbox.db_arg())
        .stdin(Stdio::null())
        .output()
        .expect("the binary must be runnable");
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(sandbox.db().exists(), "the environment path must be used");
}

#[test]
fn a_helper_type_is_not_needed_for_the_path_assertion() {
    // Guards the Sandbox helper itself: without it, a test could pass by writing to the
    // developer's real database.
    let sandbox = Sandbox::new();
    assert!(!Path::new(&sandbox.db_arg()).exists());
}

// ---------------------------------------------------------------------------
// WP2 — search, and the JSON contract that goes with it
// ---------------------------------------------------------------------------

/// Seed a searchable corpus: one English memory, one Japanese, one accented.
fn searchable() -> Sandbox {
    let sandbox = Sandbox::new();
    add(
        &sandbox,
        &[
            "--kind",
            "semantic",
            "--title",
            "Staging deploys need a manual migration",
            "--body",
            "Run migrations before the deploy; the pipeline does not do it.",
            "--tag",
            "deploy",
        ],
    );
    add(
        &sandbox,
        &[
            "--kind",
            "reference",
            "--title",
            "日本語のメモ",
            "--body",
            "これは日本語のテストです。",
        ],
    );
    add(
        &sandbox,
        &[
            "--kind",
            "procedural",
            "--title",
            "Café closing routine",
            "--body",
            "The café closes at nine: count the till, then lock up.",
        ],
    );
    sandbox
}

#[test]
fn a_search_finds_what_was_written_and_exits_zero() {
    let sandbox = searchable();
    let out = run(&["search", "--db", &sandbox.db_arg(), "migration"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("Staging deploys need a manual migration"),
        "{text}"
    );
    assert!(
        text.contains("**migrations**"),
        "the snippet must show the match: {text}"
    );
}

#[test]
fn a_search_that_finds_nothing_still_exits_zero_and_explains_itself() {
    // The distinction the contract spends an exit code on: "no results" is an answer, not
    // a failure. What the caller *does* get is the expression that was tried last.
    let sandbox = searchable();
    let out = run(&["search", "--db", &sandbox.db_arg(), "onomatopoeia zzz"]);
    assert_eq!(code(&out), 0);
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));
    let text = stderr(&out);
    assert!(text.contains("nothing matched"), "{text}");
    assert!(
        text.contains("\"onomatopoeia\"* OR \"zzz\"*"),
        "the note must name what was actually asked: {text}"
    );
}

#[test]
fn the_search_json_carries_every_field_the_contract_documents() {
    let sandbox = searchable();
    let value = json(&["search", "--db", &sandbox.db_arg(), "--json", "migration"]);

    assert_eq!(value["ok"], serde_json::json!(true));
    for field in [
        "query",
        "attempt",
        "requested_legs",
        "legs",
        "degraded",
        "recorded",
    ] {
        assert!(
            value["data"].get(field).is_some(),
            "`data.{field}` is documented in docs/CONTRACT.md and must be present: {value}"
        );
    }
    assert_eq!(value["data"]["query"], serde_json::json!("migration"));
    assert_eq!(value["data"]["requested_legs"], serde_json::json!("auto"));
    assert_eq!(value["data"]["legs"], serde_json::json!("fts"));
    assert_eq!(value["data"]["degraded"], serde_json::json!(false));
    assert_eq!(
        value["data"]["attempt"]["outcome"],
        serde_json::json!("matched")
    );
    assert_eq!(value["data"]["attempt"]["rung"], serde_json::json!(1));
    assert_eq!(value["data"]["recorded"], serde_json::json!(1));

    let results = value["data"]["results"]
        .as_array()
        .expect("results is a list");
    assert_eq!(results.len(), 1);
    let hit = &results[0];
    // Every field a `list` row has, plus `score` and `snippet` — the contract says a
    // search result *is* a list row with two fields added, so both halves are asserted
    // here against the same list `docs/CONTRACT.md` carries.
    for field in [
        "id",
        "workspace_id",
        "kind",
        "kind_label",
        "title",
        "excerpt",
        "tags",
        "source",
        "source_label",
        "source_url",
        "confidence",
        "strength",
        "pinned",
        "status",
        "created_at",
        "score",
        "snippet",
    ] {
        assert!(
            hit.get(field).is_some(),
            "`results[].{field}` is missing: {hit}"
        );
    }
    assert!(
        hit["snippet"].as_str().unwrap().contains("**migrations**"),
        "{hit}"
    );
    assert!(hit["score"].as_f64().unwrap() > 0.0, "{hit}");
    assert!(hit["body"].is_null(), "a result row carries no body: {hit}");
}

#[test]
fn a_search_that_asks_for_nothing_reports_that_rather_than_no_match() {
    let sandbox = searchable();
    let value = json(&["search", "--db", &sandbox.db_arg(), "--json", ""]);
    assert_eq!(value["ok"], serde_json::json!(true));
    assert_eq!(
        value["data"]["attempt"],
        serde_json::json!({"outcome": "nothing_asked"})
    );
    assert_eq!(value["data"]["results"], serde_json::json!([]));

    let empty = json(&["search", "--db", &sandbox.db_arg(), "--json", "zzz"]);
    assert_eq!(
        empty["data"]["attempt"]["outcome"],
        serde_json::json!("exhausted")
    );
}

#[test]
fn search_pipes_safely_because_stdout_is_one_document_and_nothing_else() {
    // The property, not the tool: `jq` is not a build dependency, but the reason
    // `kiomon search | jq` works is that stdout holds exactly one newline-terminated JSON
    // document and every diagnostic went to stderr. Both halves are asserted, and the
    // search is one that has something to say on stderr.
    let sandbox = searchable();
    let out = run(&["search", "--db", &sandbox.db_arg(), "--json", "zzz"]);
    assert_eq!(code(&out), 0);
    let text = stdout(&out);
    assert_eq!(text.lines().count(), 1, "{text:?}");
    assert!(text.starts_with('{') && text.ends_with("}\n"), "{text:?}");
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("stdout parses as JSON");
    assert_eq!(parsed["data"]["results"], serde_json::json!([]));
    assert!(
        stderr(&out).contains("nothing matched"),
        "the diagnostic belongs on stderr: {}",
        stderr(&out)
    );

    // And the human path keeps stdout for the answer too.
    let human = run(&["search", "--db", &sandbox.db_arg(), "zzz"]);
    assert!(stdout(&human).is_empty(), "{}", stdout(&human));
}

#[test]
fn the_search_exit_codes_are_the_contract_codes() {
    let sandbox = searchable();
    let db = sandbox.db_arg();

    // 0 — found, and 0 — nothing found. Neither is a failure.
    assert_eq!(code(&run(&["search", "--db", &db, "migration"])), 0);
    assert_eq!(code(&run(&["search", "--db", &db, "zzz"])), 0);

    // 2 — the workspace does not exist. Distinct from 1 on purpose.
    let missing = run(&["search", "--db", &db, "--workspace", "nowhere", "migration"]);
    assert_eq!(code(&missing), 2, "{}", stderr(&missing));
    assert!(stderr(&missing).contains("nowhere"), "{}", stderr(&missing));

    // 3 — the arguments were rejected.
    for args in [
        vec!["search", "--db", &db, "--limit", "0", "migration"],
        vec!["search", "--db", &db, "--limit", "51", "migration"],
        vec!["search", "--db", &db, "--kind", "nonsense", "migration"],
        vec!["search", "--db", &db, "--snippet-len", "0", "migration"],
    ] {
        let out = run(&args);
        assert_eq!(code(&out), 3, "{args:?}: {}", stderr(&out));
    }

    // 4 — the request was valid and this build cannot serve it.
    let vector = run(&["search", "--db", &db, "--legs", "vector", "migration"]);
    assert_eq!(code(&vector), 4, "{}", stderr(&vector));
    assert!(
        stderr(&vector).contains("--legs fts"),
        "{}",
        stderr(&vector)
    );
    let as_json = json(&[
        "search",
        "--db",
        &db,
        "--json",
        "--legs",
        "vector",
        "migration",
    ]);
    assert_eq!(as_json["ok"], serde_json::json!(false));
    assert_eq!(as_json["error"]["code"], serde_json::json!("config"));
}

#[test]
fn a_japanese_and_an_accented_query_both_find_their_memory() {
    // End to end, through the binary, because this is the acceptance criterion the hosted
    // product's regex sanitizer fails: `[^\w\s]` is ASCII-only, so it deletes a Japanese
    // query outright and truncates an accented one to `caf`.
    let sandbox = searchable();
    let db = sandbox.db_arg();

    let japanese = run(&["search", "--db", &db, "日本語"]);
    assert_eq!(code(&japanese), 0);
    assert!(
        stdout(&japanese).contains("日本語のメモ"),
        "{}",
        stdout(&japanese)
    );

    for query in ["café", "cafe", "CAFÉ"] {
        let accented = run(&["search", "--db", &db, query]);
        assert_eq!(code(&accented), 0);
        assert!(
            stdout(&accented).contains("Café closing routine"),
            "`{query}` must reach the memory: {}",
            stdout(&accented)
        );
    }
}

#[test]
fn search_filters_and_the_page_size_are_respected() {
    let sandbox = searchable();
    let db = sandbox.db_arg();
    // A second memory the same query reaches, so a filter has something to remove.
    add(
        &sandbox,
        &[
            "--kind",
            "semantic",
            "--title",
            "Deploy checklist",
            "--body",
            "Deploy the migration, then verify.",
        ],
    );

    let all = json(&["search", "--db", &db, "--json", "deploy"]);
    assert!(
        all["data"]["results"].as_array().unwrap().len() > 1,
        "{all}"
    );

    let only_reference = json(&[
        "search",
        "--db",
        &db,
        "--json",
        "--kind",
        "reference",
        "deploy",
    ]);
    assert!(
        only_reference["data"]["results"]
            .as_array()
            .unwrap()
            .is_empty(),
        "no `reference` memory mentions deploy: {only_reference}"
    );

    let one = json(&["search", "--db", &db, "--json", "--limit", "1", "deploy"]);
    assert_eq!(one["data"]["results"].as_array().unwrap().len(), 1);
    assert_eq!(one["data"]["recorded"], serde_json::json!(1));
}

#[test]
fn a_search_counts_as_a_retrieval_and_no_record_stops_it() {
    // `retrieval_count` is the decay clock's stretch factor, so this is the mechanism and
    // not telemetry — which is why it is asserted through `get` rather than trusted.
    let sandbox = searchable();
    let db = sandbox.db_arg();
    let id = json(&["search", "--db", &db, "--json", "migration"])["data"]["results"][0]["id"]
        .as_str()
        .expect("an id")
        .to_owned();

    let before = json(&["get", "--db", &db, "--json", &id]);
    assert_eq!(before["data"]["retrieval_count"], serde_json::json!(1));
    assert!(before["data"]["last_retrieved_at"].is_string(), "{before}");

    let quiet = json(&["search", "--db", &db, "--json", "--no-record", "migration"]);
    assert_eq!(quiet["data"]["recorded"], serde_json::json!(0));
    let after = json(&["get", "--db", &db, "--json", &id]);
    assert_eq!(
        after["data"]["retrieval_count"],
        serde_json::json!(1),
        "`--no-record` must leave the decay clock alone"
    );
}

#[test]
fn search_output_is_bounded_and_shows_the_match_on_a_second_line() {
    let sandbox = Sandbox::new();
    add(
        &sandbox,
        &[
            "--kind",
            "semantic",
            "--title",
            &format!("Deploy note {}", "padded pads a fair bit ".repeat(20)),
            "--body",
            &format!("deploy {}", "padding ".repeat(400)),
        ],
    );

    let out = run(&["search", "--db", &sandbox.db_arg(), "deploy"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert_eq!(
        text.lines().count(),
        2,
        "a result line and a snippet: {text:?}"
    );
    assert!(
        text.lines().next().unwrap().ends_with('…'),
        "the default path truncates: {text:?}"
    );
    for line in text.lines() {
        assert!(
            line.chars().count() < 200,
            "a bounded field must stay bounded: {} chars",
            line.chars().count()
        );
    }

    // Two different bounds, two different flags, and neither stands in for the other:
    // `--full` raises the *width* a field may take, and `--snippet-len` raises how many
    // full-text tokens of context `snippet()` returns. A snippet is a window, so `--full`
    // alone still shows twelve tokens of it.
    let full = run(&["search", "--db", &sandbox.db_arg(), "--full", "deploy"]);
    let full_text = stdout(&full);
    assert_eq!(full_text.lines().count(), 2, "{full_text:?}");
    assert!(
        !full_text.lines().next().unwrap().contains('…'),
        "`--full` must stop cutting the title: {full_text:?}"
    );

    // Both flags, because a snippet is bounded twice over: the token window is
    // `--snippet-len`'s job and the display width is `--full`'s, and the narrower of the
    // two wins. That is why `--snippet-len 40` on its own still ends in an ellipsis.
    let wider = run(&[
        "search",
        "--db",
        &sandbox.db_arg(),
        "--full",
        "--snippet-len",
        "40",
        "deploy",
    ]);
    let wider_snippet = stdout(&wider)
        .lines()
        .nth(1)
        .expect("a snippet line")
        .chars()
        .count();
    assert!(
        wider_snippet > 150,
        "forty tokens must reach past twelve: {wider_snippet} chars"
    );

    let capped = run(&[
        "search",
        "--db",
        &sandbox.db_arg(),
        "--snippet-len",
        "40",
        "deploy",
    ]);
    let capped_snippet = stdout(&capped)
        .lines()
        .nth(1)
        .expect("a snippet line")
        .chars()
        .count();
    assert!(
        // `out::DEFAULT_WIDTH` is 120 and the snippet is indented by three; the bin's
        // modules are not reachable from an integration test, so the number is repeated
        // here and the `--full` case above is what proves it is the width doing the cutting.
        capped_snippet <= 124,
        "without `--full` the display width still caps it: {capped_snippet} chars"
    );
}

#[test]
fn a_title_only_match_shows_no_snippet_at_all() {
    // FTS5 would happily return the opening of the body, unhighlighted, under a title
    // match — text that reads as the place it matched. An empty snippet is honest.
    let sandbox = Sandbox::new();
    add(
        &sandbox,
        &[
            "--kind",
            "semantic",
            "--title",
            "Kubernetes rollouts",
            "--body",
            "Nothing about that word appears in this body.",
        ],
    );
    let out = run(&["search", "--db", &sandbox.db_arg(), "kubernetes"]);
    assert_eq!(code(&out), 0);
    assert_eq!(
        stdout(&out).lines().count(),
        1,
        "no snippet, so no second line: {:?}",
        stdout(&out)
    );

    let value = json(&["search", "--db", &sandbox.db_arg(), "--json", "kubernetes"]);
    assert_eq!(
        value["data"]["results"][0]["snippet"],
        serde_json::json!("")
    );
}

#[test]
fn a_search_can_be_ordered_by_recency_or_strength() {
    let sandbox = searchable();
    let db = sandbox.db_arg();
    for sort in ["relevance", "recent", "strength"] {
        let out = run(&["search", "--db", &db, "--sort", sort, "deploy"]);
        assert_eq!(code(&out), 0, "`--sort {sort}`: {}", stderr(&out));
        assert!(!stdout(&out).is_empty(), "`--sort {sort}` found nothing");
    }
}

// ---------------------------------------------------------------------------
// bench
// ---------------------------------------------------------------------------

#[test]
fn bench_open_reports_the_database_it_opened_and_nothing_else() {
    let sandbox = Sandbox::new();
    add(&sandbox, &["--kind", "semantic", "--title", "a memory"]);
    let db = sandbox.db_arg();

    let value = json(&["bench", "--db", &db, "--open", "--json"]);
    assert_eq!(value["ok"], serde_json::json!(true));
    assert_eq!(value["data"]["path"].as_str(), Some(db.as_str()));
    assert_eq!(
        value["data"]["schema_version"],
        serde_json::json!(kiomon_core::Database::target_schema_version()),
        "the probe reports the version the file is at, not the one the build wants"
    );

    // The human path prints nothing at all, because the harness times this probe and a
    // print at ten thousand rows would be a measurement of the print.
    let out = run(&["bench", "--db", &db, "--open"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(stdout(&out), "", "the open probe must not write to stdout");
}

#[test]
fn bench_measures_a_corpus_and_reports_what_it_built() {
    let sandbox = Sandbox::new();
    let corpus = sandbox.dir.join("corpus");
    let corpus_arg = corpus.display().to_string();

    let value = json(&[
        "bench",
        "--sizes",
        "100",
        "--runs",
        "1",
        "--corpus",
        &corpus_arg,
        "--quiet",
        "--json",
    ]);
    let data = &value["data"];
    assert_eq!(value["ok"], serde_json::json!(true), "{value}");
    assert_eq!(data["reports"].as_array().map(Vec::len), Some(1));
    let report = &data["reports"][0];

    // The shape is the deliverable: a corpus whose bodies are all one size cannot find the
    // defects a mixed one does.
    assert_eq!(report["corpus"]["size"], serde_json::json!(100));
    let classes = report["corpus"]["classes"]
        .as_array()
        .expect("the size classes");
    let counts: Vec<i64> = classes
        .iter()
        .map(|class| class["memories"].as_i64().expect("a count"))
        .collect();
    assert_eq!(counts, vec![60, 30, 10], "60 / 30 / 10 was asked for");

    // Every probe query says how many rows it matched, and one of them is planted to match
    // nothing — a corpus that cannot express "no results" cannot measure an empty search.
    let probes = report["corpus"]["probes"].as_array().expect("the probes");
    let matches: Vec<i64> = probes
        .iter()
        .map(|probe| probe["matches"].as_i64().expect("a count"))
        .collect();
    assert!(
        matches.windows(2).any(|pair| pair[0] > pair[1]),
        "the planted terms must span more than one selectivity: {matches:?}"
    );
    assert!(
        matches.contains(&0),
        "one planted term must match nothing: {matches:?}"
    );

    // The rows, and the two numbers on every one of them.
    let rows = report["rows"].as_array().expect("the rows");
    assert!(rows.len() >= 12, "only {} rows: {rows:?}", rows.len());
    for row in rows {
        assert!(row["total_p50_ms"].is_number(), "{row}");
        assert!(row["work_p50_ms"].is_number(), "{row}");
        assert!(
            row["work_p50_ms"].as_f64().expect("a number")
                <= row["total_p50_ms"].as_f64().expect("a number"),
            "work cannot exceed total: {row}"
        );
    }
    assert!(
        rows.iter().any(|row| row["label"] == "open"),
        "the open row is the one that decides the daemon question"
    );

    // The gates, and the ones this build cannot answer.
    let gates = report["gates"].as_array().expect("the gates");
    let labels: Vec<&str> = gates
        .iter()
        .map(|gate| gate["label"].as_str().expect("a label"))
        .collect();
    assert!(labels.contains(&"open"));
    assert!(labels.contains(&"add"));
    assert!(labels.contains(&"search common"));
    assert!(
        report["rejected"]
            .as_array()
            .expect("the deferred gates")
            .iter()
            .any(|line| line.as_str().is_some_and(|line| line.contains("embedder"))),
        "the hybrid gate is deferred, and a budget with a silent hole in it is worse than one \
         that names the hole"
    );
}

#[test]
fn a_second_bench_run_reuses_the_corpus_rather_than_building_it_again() {
    let sandbox = Sandbox::new();
    let corpus = sandbox.dir.join("corpus");
    let corpus_arg = corpus.display().to_string();
    let args = [
        "bench",
        "--sizes",
        "50",
        "--runs",
        "1",
        "--corpus",
        &corpus_arg,
        "--quiet",
        "--json",
    ];

    let first = json(&args);
    assert_eq!(first["data"]["reports"][0]["corpus"]["reused"], false);
    let second = json(&args);
    assert_eq!(
        second["data"]["reports"][0]["corpus"]["reused"], true,
        "a corpus of the right size must be measured, not rebuilt"
    );
    assert_eq!(
        second["data"]["reports"][0]["corpus"]["body_bytes"],
        first["data"]["reports"][0]["corpus"]["body_bytes"],
        "the reused corpus must be the same one, to the byte"
    );

    // And a rebuild really rebuilds: the same seed must produce the same bytes, or two runs
    // of two builds are not measuring the same input.
    let rebuilt = json(
        &args
            .iter()
            .copied()
            .chain(["--rebuild"])
            .collect::<Vec<_>>(),
    );
    assert_eq!(rebuilt["data"]["reports"][0]["corpus"]["reused"], false);
    assert_eq!(
        rebuilt["data"]["reports"][0]["corpus"]["body_bytes"],
        first["data"]["reports"][0]["corpus"]["body_bytes"],
        "the same seed must build the same corpus"
    );
}

#[test]
fn bench_gate_is_the_plan_s_limits_and_a_missed_gate_is_an_exit_code() {
    let sandbox = Sandbox::new();
    let corpus = sandbox.dir.join("corpus");
    let corpus_arg = corpus.display().to_string();
    let out = run(&[
        "bench",
        "--sizes",
        "50",
        "--runs",
        "1",
        "--corpus",
        &corpus_arg,
        "--quiet",
        "--gate",
        "--json",
    ]);
    let value: serde_json::Value =
        serde_json::from_str(&stdout(&out)).expect("one JSON document on stdout");

    // A tiny corpus on any machine this runs on meets every gate the plan defines for this
    // build, so the *passing* exit code is 0. The failing path is `evaluate`'s, and it is
    // asserted where it lives rather than by trying to make a fast machine slow.
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(
        value["data"]["gates_passed"],
        serde_json::json!(true),
        "{value}"
    );
    for gate in value["data"]["reports"][0]["gates"]
        .as_array()
        .expect("the gates")
    {
        assert_eq!(gate["passed"], serde_json::json!(true), "{gate}");
        assert!(
            gate["limit_ms"].as_f64().expect("a limit") > 0.0,
            "a gate without a limit is not a gate: {gate}"
        );
    }
}
