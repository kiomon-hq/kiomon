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
    let sandbox = Sandbox::new();
    let out = run(&["search", "--db", &sandbox.db_arg(), "anything"]);
    assert_eq!(code(&out), 1);
    let text = stderr(&out);
    assert!(text.contains("search"), "{text}");
    assert!(text.contains("work package 2"), "{text}");
}

#[test]
fn the_json_failure_envelope_is_exactly_one_document_on_stdout() {
    let sandbox = Sandbox::new();
    let value = json(&["search", "--db", &sandbox.db_arg(), "--json", "anything"]);
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
    let out = run(&["--quiet", "search", "--db", &sandbox.db_arg(), "anything"]);
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
    let out = run(&["search", "--db", &sandbox.db_arg(), "anything", "--json"]);
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
