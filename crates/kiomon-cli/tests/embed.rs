// SPDX-License-Identifier: Apache-2.0

//! The vector leg through the binary: the config file, `--legs`, `reindex`, and `doctor`.
//!
//! What these add to `kiomon-core/tests/vector.rs` is the shell's view: the exit code a
//! script branches on, which stream a message went to, and whether the config file a user
//! actually writes is the one the program reads. Every one of those is a rule about a
//! *process*, and none of them can be asserted from inside the library.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("kiomon-embed-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a sandbox directory must be creatable");
        Self { dir }
    }

    fn db(&self) -> PathBuf {
        self.dir.join("brain.db")
    }

    fn db_arg(&self) -> String {
        self.db().display().to_string()
    }

    /// Write a config file into the sandbox and return its path.
    fn config(&self, body: &str) -> String {
        let path = self.dir.join("config.toml");
        std::fs::write(&path, body).expect("the config must be writable");
        path.display().to_string()
    }

    /// The documented minimal `[embedder]` block: Ollama, which is not running.
    fn ollama_config(&self) -> String {
        self.config("[embedder]\nprovider = \"ollama\"\nmodel = \"nomic-embed-text\"\n")
    }

    /// The stub provider: no model, no network, deterministic.
    fn stub_config(&self) -> String {
        self.config("[embedder]\nprovider = \"stub\"\n")
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

fn add(sandbox: &Sandbox, config: &str, title: &str, body: &str) {
    let db = sandbox.db_arg();
    let out = run(&[
        "--db", &db, "--config", config, "add", "--kind", "semantic", "--title", title, "--body",
        body, "--quiet",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
}

// ---------------------------------------------------------------------------
// The config file
// ---------------------------------------------------------------------------

#[test]
fn a_config_file_that_does_not_parse_is_an_error_naming_the_file_and_the_line() {
    let sandbox = Sandbox::new();
    // Two typos, both common: an unquoted value and a key that does not exist.
    let config = sandbox.config("[embedder]\nprovider = ollama\nmodel = \"m\"\n");
    let out = run(&[
        "--db",
        &sandbox.db_arg(),
        "--config",
        &config,
        "search",
        "anything",
    ]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    let message = stderr(&out);
    assert!(message.contains("config.toml"), "{message}");
    assert!(
        message.contains("line") || message.contains('1'),
        "the message has to point at the typo: {message}"
    );
}

#[test]
fn a_config_file_with_an_unknown_key_is_refused_rather_than_ignored() {
    let sandbox = Sandbox::new();
    // `model_name` instead of `model`: silently ignoring it would look exactly like a
    // broken embedder, which is the failure mode the strict parse exists to prevent.
    let config = sandbox.config("[embedder]\nprovider = \"stub\"\nmodel_name = \"stub\"\n");
    let out = run(&[
        "--db",
        &sandbox.db_arg(),
        "--config",
        &config,
        "search",
        "anything",
    ]);
    assert_eq!(code(&out), 4);
    let message = stderr(&out);
    assert!(message.contains("model_name"), "{message}");
}

#[test]
fn https_is_refused_by_name_because_this_build_speaks_plain_http() {
    let sandbox = Sandbox::new();
    let config = sandbox.config(
        "[embedder]\nprovider = \"openai\"\nurl = \"https://api.example.com/v1\"\nmodel = \"m\"\n",
    );
    let out = run(&[
        "--db",
        &sandbox.db_arg(),
        "--config",
        &config,
        "search",
        "anything",
    ]);
    assert_eq!(code(&out), 4);
    let message = stderr(&out);
    assert!(message.contains("no TLS"), "{message}");
    assert!(message.contains("http://"), "{message}");
}

#[test]
fn a_default_workspace_from_the_config_file_is_where_a_write_lands() {
    let sandbox = Sandbox::new();
    let config = sandbox.config("default_workspace = \"research\"\n");
    let db = sandbox.db_arg();
    let out = run(&[
        "--db",
        &db,
        "--config",
        &config,
        "init",
        "--workspace",
        "Research",
        "--quiet",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    let value = json(&[
        "--db", &db, "--config", &config, "add", "--kind", "semantic", "--title", "A fact",
        "--body", "b", "--json",
    ]);
    assert_eq!(value["data"]["workspace_id"], serde_json::json!("research"));
    // And the config file is not consulted when the flag says otherwise.
    let value = json(&[
        "--db",
        &db,
        "--config",
        &config,
        "add",
        "--kind",
        "semantic",
        "--title",
        "Another fact",
        "--body",
        "b",
        "--workspace",
        "default",
        "--json",
    ]);
    assert_eq!(value["data"]["workspace_id"], serde_json::json!("default"));
}

// ---------------------------------------------------------------------------
// `--legs`
// ---------------------------------------------------------------------------

#[test]
fn a_vector_only_search_with_no_embedder_exits_four_and_names_the_config_key() {
    let sandbox = Sandbox::new();
    add(
        &sandbox,
        &sandbox.stub_config(),
        "A fact",
        "the deploy worked",
    );

    // No `--config`, so no embedder: the request is valid and the environment cannot serve
    // it, which is exit 4 — never a lexical answer the caller did not ask for.
    let out = run(&[
        "--db",
        &sandbox.db_arg(),
        "search",
        "--legs",
        "vector",
        "deploy",
    ]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    let message = stderr(&out);
    assert!(message.contains("[embedder]"), "{message}");
    assert!(message.contains("--legs fts"), "{message}");
    assert!(stdout(&out).is_empty(), "nothing on stdout for a refusal");
}

#[test]
fn the_json_reports_which_legs_answered_and_which_model_the_query_vector_came_from() {
    let sandbox = Sandbox::new();
    let config = sandbox.stub_config();
    add(&sandbox, &config, "Deploy runbook", "ship it on friday");
    let db = sandbox.db_arg();

    let value = json(&[
        "--db",
        &db,
        "--config",
        &config,
        "search",
        "--json",
        "--no-record",
        "deploy ship",
    ]);
    assert_eq!(value["data"]["legs"]["fts"], serde_json::json!(true));
    assert_eq!(value["data"]["legs"]["vector"], serde_json::json!(true));
    assert_eq!(value["data"]["vector_model"], serde_json::json!("stub"));
    assert_eq!(value["data"]["degraded"], serde_json::json!(false));
    assert_eq!(value["data"]["reason"], serde_json::json!(""));
    // The row carries the similarity the vector leg gave it, beside the fused score.
    assert!(
        value["data"]["results"][0]["similarity"].is_number(),
        "{value}"
    );

    // And a lexical-only search reports one leg, no model, and no similarity on any row.
    let value = json(&[
        "--db",
        &db,
        "--config",
        &config,
        "search",
        "--json",
        "--no-record",
        "--legs",
        "fts",
        "deploy ship",
    ]);
    assert_eq!(value["data"]["legs"]["vector"], serde_json::json!(false));
    assert!(value["data"]["vector_model"].is_null(), "{value}");
    assert!(
        value["data"]["results"][0]["similarity"].is_null(),
        "{value}"
    );
}

#[test]
fn a_hybrid_search_on_a_dead_embedder_says_so_on_stderr_and_still_answers() {
    let sandbox = Sandbox::new();
    // Ollama at its default port, with nothing listening: the documented failure a user
    // actually hits, and the one that must not look like an empty library.
    let config = sandbox.ollama_config();
    // The memories are written by the same dead embedder, so the write path's own warning
    // is exercised here too.
    let db = sandbox.db_arg();
    let out = run(&[
        "--db",
        &db,
        "--config",
        &config,
        "add",
        "--kind",
        "semantic",
        "--title",
        "Deploy runbook",
        "--body",
        "ship it on friday",
    ]);
    assert_eq!(code(&out), 0, "a failed embed never fails a write");
    let warnings = stderr(&out);
    assert!(warnings.contains("not embedded"), "{warnings}");
    assert!(warnings.contains("reindex --embed"), "{warnings}");

    let out = run(&["--db", &db, "--config", &config, "search", "deploy"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stdout(&out).contains("Deploy runbook"),
        "the lexical leg's answer survives: {}",
        stdout(&out)
    );
    let warnings = stderr(&out);
    assert!(warnings.contains("vector leg did not run"), "{warnings}");
    assert!(warnings.contains("full text only"), "{warnings}");
}

#[test]
fn a_degraded_hybrid_search_reports_the_reason_in_json_and_the_leak_that_is_not_there() {
    let sandbox = Sandbox::new();
    let config = sandbox.ollama_config();
    let db = sandbox.db_arg();
    run(&[
        "--db",
        &db,
        "--config",
        &config,
        "add",
        "--kind",
        "semantic",
        "--title",
        "Deploy runbook",
        "--body",
        "ship it",
        "--quiet",
    ]);
    let value = json(&[
        "--db",
        &db,
        "--config",
        &config,
        "search",
        "--json",
        "--no-record",
        "deploy",
    ]);
    assert_eq!(value["data"]["degraded"], serde_json::json!(true));
    assert_eq!(
        value["data"]["reason"],
        serde_json::json!("no_query_vector")
    );
    assert_eq!(value["data"]["legs"]["fts"], serde_json::json!(true));
    assert_eq!(value["data"]["legs"]["vector"], serde_json::json!(false));
}

#[test]
fn the_hosted_leg_spellings_are_accepted_by_the_binary() {
    let sandbox = Sandbox::new();
    let config = sandbox.stub_config();
    add(&sandbox, &config, "Deploy runbook", "ship it");
    let db = sandbox.db_arg();
    for spelling in ["hybrid", "both", "lexical", "fulltext", "semantic", "auto"] {
        let out = run(&[
            "--db", &db, "--config", &config, "search", "--legs", spelling, "deploy",
        ]);
        assert_eq!(code(&out), 0, "`--legs {spelling}`: {}", stderr(&out));
    }
    // And one that is not in either vocabulary is a usage error, not a silent default.
    let out = run(&[
        "--db", &db, "--config", &config, "search", "--legs", "bm25", "deploy",
    ]);
    assert_eq!(code(&out), 3, "{}", stderr(&out));
}

#[test]
fn the_graph_leg_says_it_is_deferred_rather_than_searching_hybrid() {
    let sandbox = Sandbox::new();
    let config = sandbox.stub_config();
    add(&sandbox, &config, "Deploy runbook", "ship it");
    let out = run(&[
        "--db",
        &sandbox.db_arg(),
        "--config",
        &config,
        "search",
        "--legs",
        "graph",
        "deploy",
    ]);
    assert_eq!(code(&out), 1, "{}", stderr(&out));
    assert!(stderr(&out).contains("next sprint"), "{}", stderr(&out));
    assert!(stdout(&out).is_empty());
}

// ---------------------------------------------------------------------------
// `reindex`
// ---------------------------------------------------------------------------

#[test]
fn reindex_embeds_a_corpus_that_predates_the_embedder_and_reports_what_it_did() {
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let config = sandbox.stub_config();
    // Written with no embedder configured at all: the state every existing brain is in the
    // first time someone adds an `[embedder]` block.
    for index in 0..4 {
        let out = run(&[
            "--db",
            &db,
            "add",
            "--kind",
            "semantic",
            "--title",
            &format!("Memory {index}"),
            "--body",
            "the deploy was rolled out",
            "--quiet",
        ]);
        assert_eq!(code(&out), 0, "{}", stderr(&out));
    }

    let value = json(&[
        "--db", &db, "--config", &config, "reindex", "--embed", "--json",
    ]);
    assert_eq!(value["data"]["embedded"], serde_json::json!(4));
    assert_eq!(value["data"]["model"], serde_json::json!("stub"));
    assert_eq!(value["data"]["dry_run"], serde_json::json!(false));

    // The second run has nothing to do and says so, which is the idempotence a reindex has
    // to have to be safe to re-run after an interruption.
    let value = json(&[
        "--db", &db, "--config", &config, "reindex", "--embed", "--json",
    ]);
    assert_eq!(value["data"]["embedded"], serde_json::json!(0));

    // And a dry run on a corpus that is fully embedded reports zero rather than writing.
    let value = json(&[
        "--db",
        &db,
        "--config",
        &config,
        "reindex",
        "--embed",
        "--dry-run",
        "--json",
    ]);
    assert_eq!(value["data"]["dry_run"], serde_json::json!(true));
    assert_eq!(value["data"]["embedded"], serde_json::json!(0));
}

#[test]
fn reindex_without_an_embedder_exits_four_and_says_which_key_is_missing() {
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    run(&[
        "--db", &db, "add", "--kind", "semantic", "--title", "A fact", "--body", "b", "--quiet",
    ]);
    let out = run(&["--db", &db, "reindex", "--embed"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(stderr(&out).contains("[embedder]"), "{}", stderr(&out));
}

#[test]
fn reindex_without_a_target_is_a_usage_error_rather_than_a_no_op() {
    let sandbox = Sandbox::new();
    let out = run(&[
        "--db",
        &sandbox.db_arg(),
        "--config",
        &sandbox.stub_config(),
        "reindex",
    ]);
    assert_eq!(code(&out), 3, "{}", stderr(&out));
    assert!(stderr(&out).contains("--embed"), "{}", stderr(&out));
}

// ---------------------------------------------------------------------------
// `doctor`
// ---------------------------------------------------------------------------

#[test]
fn doctor_names_the_config_file_it_read_and_the_embedder_it_found() {
    let sandbox = Sandbox::new();
    let config = sandbox.stub_config();
    add(&sandbox, &config, "A fact", "the deploy worked");
    let db = sandbox.db_arg();

    let value = json(&["--db", &db, "--config", &config, "doctor", "--json"]);
    assert_eq!(
        value["data"]["embedder"]["configured"],
        serde_json::json!(true)
    );
    assert_eq!(
        value["data"]["embedder"]["provider"],
        serde_json::json!("stub")
    );
    assert_eq!(value["data"]["embedder"]["active"], serde_json::json!(1));
    assert_eq!(value["data"]["embedder"]["embedded"], serde_json::json!(1));
    let detail = value["data"]["embedder"]["detail"].as_str().unwrap();
    assert!(detail.contains("1/1"), "{detail}");
    assert!(
        detail.contains("not meaningful"),
        "a brain built with the stub has to say so where a user looks: {detail}"
    );

    let paths = json(&[
        "--db",
        &db,
        "--config",
        &config,
        "doctor",
        "--paths-only",
        "--json",
    ]);
    assert_eq!(paths["data"]["config"], serde_json::json!(config.clone()));
}

#[test]
fn doctor_without_an_embedder_says_what_to_add_and_where_the_file_is() {
    let sandbox = Sandbox::new();
    let config = sandbox.config("default_workspace = \"default\"\n");
    let value = json(&[
        "--db",
        &sandbox.db_arg(),
        "--config",
        &config,
        "doctor",
        "--json",
    ]);
    assert_eq!(
        value["data"]["embedder"]["configured"],
        serde_json::json!(false)
    );
    let detail = value["data"]["embedder"]["detail"].as_str().unwrap();
    assert!(detail.contains("[embedder]"), "{detail}");
    assert!(detail.contains("config.toml"), "{detail}");
}

#[test]
fn doctor_notices_a_corpus_that_has_no_vectors_for_the_configured_model() {
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let config = sandbox.stub_config();
    run(&[
        "--db",
        &db,
        "add",
        "--kind",
        "semantic",
        "--title",
        "Written with no embedder",
        "--body",
        "b",
        "--quiet",
    ]);

    let value = json(&["--db", &db, "--config", &config, "doctor", "--json"]);
    assert_eq!(value["data"]["embedder"]["active"], serde_json::json!(1));
    assert_eq!(value["data"]["embedder"]["embedded"], serde_json::json!(0));
    let detail = value["data"]["embedder"]["detail"].as_str().unwrap();
    assert!(
        detail.contains("reindex --embed"),
        "the gap has to come with the command that closes it: {detail}"
    );
}

// ---------------------------------------------------------------------------
// `retrieve`
// ---------------------------------------------------------------------------

#[test]
fn retrieve_can_be_built_without_touching_the_embedder() {
    let sandbox = Sandbox::new();
    let database = sandbox.db_arg();
    // A dead embedder on the default port, so `auto` would degrade: the packet has to be
    // obtainable without it.
    let config = sandbox.ollama_config();
    run(&[
        "--db",
        &database,
        "--config",
        &config,
        "add",
        "--kind",
        "semantic",
        "--title",
        "Deploy runbook",
        "--body",
        "ship it on friday",
        "--quiet",
    ]);
    let value = json(&[
        "--db", &database, "--config", &config, "retrieve", "--legs", "fts", "--json", "deploy",
    ]);
    assert_eq!(
        value["data"]["facts"][0]["claim"],
        serde_json::json!("Deploy runbook")
    );
    assert!(
        value["data"]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|warning| !warning.as_str().unwrap().contains("full-text leg alone")),
        "a lexical packet that was asked for lexically is not degraded: {value}"
    );

    // And on `auto`, the same dead embedder becomes a warning inside the packet — there is
    // no `degraded` field on the wire packet, so a warning is where a caller can see it.
    let value = json(&[
        "--db", &database, "--config", &config, "retrieve", "--json", "deploy",
    ]);
    let warnings = value["data"]["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("full-text leg alone")),
        "{value}"
    );
}

/// The `--config` flag reaches every command that reads it, not just `search`.
#[test]
fn the_config_flag_is_global() {
    let sandbox = Sandbox::new();
    let config = sandbox.stub_config();
    let db = sandbox.db_arg();
    for args in [
        vec!["stats", "--json"],
        vec!["list", "--json"],
        vec!["doctor", "--json"],
        vec!["review", "list", "--json"],
    ] {
        let mut full = vec!["--db", db.as_str(), "--config", config.as_str()];
        full.extend(args);
        let out = run(&full);
        assert_eq!(code(&out), 0, "{full:?}: {}", stderr(&out));
    }
    let _: &Path = Path::new(&db);
}
