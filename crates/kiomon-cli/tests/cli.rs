// SPDX-License-Identifier: Apache-2.0

//! End-to-end assertions on the binary: the stream split, the exit codes, and the
//! `--json` envelope.
//!
//! These are the WP0 acceptance criteria from `plans/001-local-v1-core.md` §8, made
//! executable. They run the compiled binary rather than calling into the library,
//! because every rule they protect is a rule about a *process* — which stream a byte
//! went to, and what the shell saw when it exited.

use std::process::{Command, Output};

/// The binary under test, as cargo built it.
fn kiomon() -> Command {
    Command::new(env!("CARGO_BIN_EXE_kiomon"))
}

fn run(args: &[&str]) -> Output {
    kiomon()
        .args(args)
        .output()
        .expect("the binary must be runnable")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

// ---------------------------------------------------------------------------
// clap's two successful exits
// ---------------------------------------------------------------------------

#[test]
fn version_is_printed_on_stdout_and_exits_zero() {
    let out = run(&["--version"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        stdout(&out).contains(env!("CARGO_PKG_VERSION")),
        "expected the crate version on stdout, got: {}",
        stdout(&out)
    );
}

#[test]
fn help_is_printed_on_stdout_and_exits_zero() {
    let out = run(&["--help"]);
    assert_eq!(out.status.code(), Some(0));
    let text = stdout(&out);
    assert!(text.contains("Usage:"), "{text}");
    assert!(!text.trim().is_empty(), "help must say something");
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
        out.status.code(),
        Some(3),
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
    // The most important assertion in this file. clap's own default exit code for a
    // usage error is 2, and this project's contract spends 2 on "not found" — so
    // without the remap in `out::parse_failure`, an agent could not tell a typo from a
    // deleted memory.
    let out = run(&["search", "--nope", "anything"]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
}

#[test]
fn a_missing_required_argument_exits_three() {
    // `add` needs a title (or --from-file) and a kind.
    let out = run(&["add", "--kind", "semantic"]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
}

#[test]
fn an_invalid_enum_value_exits_three_and_lists_the_valid_ones() {
    let out = run(&["add", "--kind", "knowledge", "--title", "x"]);
    assert_eq!(out.status.code(), Some(3));
    let text = stderr(&out);
    for kind in ["reference", "semantic", "procedural", "episodic"] {
        assert!(text.contains(kind), "the error must list `{kind}`: {text}");
    }
}

#[test]
fn a_malformed_outcome_item_exits_three() {
    let out = run(&["outcome", "success", "--item", "k_01:brilliant"]);
    assert_eq!(out.status.code(), Some(3));
    assert!(stderr(&out).contains("helpful"), "{}", stderr(&out));
}

#[test]
fn a_rating_of_minus_one_is_parsed_rather_than_read_as_a_flag() {
    // `-1` is the only value in the surface that begins with a hyphen.
    let out = run(&["rate", "k_01JC9W5K2Q", "-1"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected the unimplemented error, not a parse error: {}",
        stderr(&out)
    );
}

#[test]
fn forget_without_yes_refuses_rather_than_prompting() {
    // Nothing in this tool may block on a terminal: the callers are scripts and
    // agents. Whatever `forget` does when implemented, it does not prompt.
    let out = kiomon()
        .args(["forget", "k_01JC9W5K2Q"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        !stdout(&out).is_empty() || !stderr(&out).is_empty(),
        "the command must say something rather than exit silently"
    );
    // WP1 makes this exit 3 without `--yes`; until then it is the unimplemented exit.
    // Either way it is one of the contract's codes and never a hang.
    assert!(
        matches!(out.status.code(), Some(1 | 3)),
        "unexpected exit code: {:?}",
        out.status.code()
    );
}

// ---------------------------------------------------------------------------
// Streams and the JSON envelope
// ---------------------------------------------------------------------------

#[test]
fn diagnostics_never_reach_stdout() {
    let out = run(&["init"]);
    assert!(
        stdout(&out).is_empty(),
        "the human path must keep stdout clean: {}",
        stdout(&out)
    );
    assert!(stderr(&out).contains("kiomon: "), "{}", stderr(&out));
}

#[test]
fn unimplemented_commands_name_the_work_package_that_owns_them() {
    let out = run(&["init"]);
    assert_eq!(out.status.code(), Some(1));
    let text = stderr(&out);
    assert!(text.contains("init"), "{text}");
    assert!(
        text.contains("work package 1"),
        "the message must point at the plan: {text}"
    );
}

#[test]
fn the_json_failure_envelope_is_exactly_one_document_on_stdout() {
    let out = run(&["--json", "init"]);
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert_eq!(
        text.lines().count(),
        1,
        "the envelope must be a single line: {text}"
    );
    let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
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
    let out = run(&["--quiet", "init"]);
    assert_eq!(out.status.code(), Some(1));
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
    let out = run(&["init", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert_eq!(value["error"]["code"], serde_json::json!("unimplemented"));
}

#[test]
fn a_database_path_is_accepted_globally() {
    let out = run(&["--db", "/tmp/kiomon-does-not-exist.db", "init"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "the path must parse; `init` is what is unimplemented"
    );
    let out = run(&["init", "--db", "/tmp/kiomon-does-not-exist.db"]);
    assert_eq!(out.status.code(), Some(1));
}
