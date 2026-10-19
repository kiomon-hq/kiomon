// SPDX-License-Identifier: Apache-2.0

//! End-to-end assertions on the graph, through the binary: `link`, `unlink`, `relate`, and
//! `get --relations`.
//!
//! The library's own rules are tested in `kiomon-core/src/links.rs`. What is tested here is
//! what only a process can show: the exit codes a caller branches on, the JSON shape an
//! integration parses, and whether stdout stays clean on the human path. The two are
//! deliberately different tests — a rule that holds in the library and a promise a shell
//! scripts against are different promises.

use std::path::PathBuf;
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
        let dir =
            std::env::temp_dir().join(format!("kiomon-graph-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a sandbox directory must be creatable");
        Self { dir }
    }

    fn db(&self) -> PathBuf {
        self.dir.join("brain.db")
    }

    fn db_arg(&self) -> String {
        self.db().display().to_string()
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

fn test_stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("the process must have exited")
}

/// Run with `--json`, require success, and parse the single document it must produce.
///
/// The `ok` check matters: an error is a *valid* envelope, so a helper that only parsed
/// would let a refused command through as if it had run. It did exactly once, when
/// `forget` needed a `--yes` this test had not learned about yet.
fn json(args: &[&str]) -> serde_json::Value {
    let out = run(args);
    let text = test_stdout(&out);
    assert_eq!(
        text.lines().count(),
        1,
        "the envelope must be exactly one line, got: {text:?} (stderr: {})",
        stderr(&out)
    );
    let value: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("stdout was not JSON ({err}): {text:?}"));
    assert_eq!(
        value["ok"],
        serde_json::json!(true),
        "`kiomon {}` failed: {value}",
        args.join(" ")
    );
    value
}

/// Write a memory with the given title and body, returning its id.
fn add(sandbox: &Sandbox, kind: &str, title: &str, body: &str) -> String {
    let db = sandbox.db_arg();
    let value = json(&[
        "add",
        "--db",
        &db,
        "--json",
        "--kind",
        kind,
        "--title",
        title,
        "--body",
        body,
        "--confidence",
        "0.9",
    ]);
    value["data"]["id"].as_str().expect("an id").to_owned()
}

/// Two memories with nothing in common, so the auto-link pass adds no edges of its own and
/// a test's assertions are about the edge it writes.
fn two_unrelated(sandbox: &Sandbox) -> (String, String) {
    let first = add(
        sandbox,
        "semantic",
        "alpha beta gamma",
        "delta epsilon zeta",
    );
    let second = add(sandbox, "procedural", "kappa lambda", "mu nu xi");
    (first, second)
}

// ---------------------------------------------------------------------------
// link
// ---------------------------------------------------------------------------

#[test]
fn link_writes_an_edge_and_reports_it() {
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let (first, second) = two_unrelated(&sandbox);

    let value = json(&[
        "link",
        "--db",
        &db,
        &first,
        &second,
        "--relation",
        "supports",
        "--score",
        "0.75",
        "--json",
    ]);
    assert_eq!(value["ok"], serde_json::json!(true), "{value}");
    assert_eq!(value["data"]["created"], serde_json::json!(true));
    assert_eq!(value["data"]["relation"], "supports");
    assert_eq!(value["data"]["source_id"], serde_json::json!(first));
    assert_eq!(value["data"]["target_id"], serde_json::json!(second));

    // And the read side sees it, from the end it was not written to as well.
    let from_target = json(&["relate", "--db", &db, &second, "--json"]);
    assert_eq!(from_target["data"][0]["direction"], "in");
    assert_eq!(
        from_target["data"][0]["memory_id"],
        serde_json::json!(first)
    );
}

#[test]
fn linking_twice_is_a_success_that_says_nothing_changed() {
    // The contract's rule: idempotent, and not an error. A script replaying a batch has to
    // be able to run it twice, so `created` is a field and not an exit code.
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let (first, second) = two_unrelated(&sandbox);

    let args = ["link", "--db", &db, &first, &second, "--json"];
    let first_run = json(&args);
    assert_eq!(first_run["data"]["created"], serde_json::json!(true));

    let out = run(&args);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let second_run: serde_json::Value =
        serde_json::from_str(&test_stdout(&out)).expect("one JSON document");
    assert_eq!(
        second_run["data"]["created"],
        serde_json::json!(false),
        "the second write reports the edge was already there"
    );
}

#[test]
fn a_self_edge_is_exit_three_because_the_request_is_malformed() {
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let (first, _) = two_unrelated(&sandbox);

    let out = run(&["link", "--db", &db, &first, &first]);
    assert_eq!(code(&out), 3, "{}", stderr(&out));
    assert!(stderr(&out).contains("itself"), "{}", stderr(&out));
}

#[test]
fn an_edge_to_a_missing_memory_is_exit_two_and_names_the_id() {
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let (first, _) = two_unrelated(&sandbox);

    let out = run(&["link", "--db", &db, &first, "k_01ZZZZZZZZZZZZZZZZZZZZZZZZ"]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    assert!(stderr(&out).contains("k_01ZZZ"), "{}", stderr(&out));
}

#[test]
fn a_pending_draft_cannot_be_linked_in_either_direction() {
    // The review inbox's promise, reached through the graph: a draft has no edges, so no
    // read path can print its title before a person approved it.
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let active = add(&sandbox, "semantic", "approved memory", "body text here");
    let draft = json(&[
        "add",
        "--db",
        &db,
        "--json",
        "--kind",
        "semantic",
        "--title",
        "a draft nobody approved",
        "--source",
        "agent_reflection",
        "--confidence",
        "0.4",
    ]);
    let draft = draft["data"]["id"].as_str().expect("an id").to_owned();

    for (source, target) in [(&active, &draft), (&draft, &active)] {
        let out = run(&["link", "--db", &db, source, target]);
        assert_eq!(code(&out), 2, "{}", stderr(&out));
        assert!(
            stderr(&out).contains("review inbox"),
            "the message says which kind of missing this is: {}",
            stderr(&out)
        );
    }
}

#[test]
fn a_relation_that_is_not_offered_is_exit_three() {
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let (first, second) = two_unrelated(&sandbox);
    let out = run(&[
        "link",
        "--db",
        &db,
        &first,
        &second,
        "--relation",
        "resembles",
    ]);
    assert_eq!(code(&out), 3, "{}", stderr(&out));
    assert!(stderr(&out).contains("possible values"), "{}", stderr(&out));
}

// ---------------------------------------------------------------------------
// unlink
// ---------------------------------------------------------------------------

#[test]
fn unlink_removes_what_link_wrote_and_says_how_many() {
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let (first, second) = two_unrelated(&sandbox);
    json(&["link", "--db", &db, &first, &second, "--json"]);

    // Asked for the other way round, which is how a caller reads the pair off `relate`.
    let value = json(&["unlink", "--db", &db, &second, &first, "--json"]);
    assert_eq!(value["data"]["removed"], serde_json::json!(1));

    let out = run(&["relate", "--db", &db, &first]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("no graph neighbours"),
        "{}",
        stderr(&out)
    );
    assert!(test_stdout(&out).is_empty(), "{}", test_stdout(&out));
}

#[test]
fn unlinking_an_edge_that_was_never_there_is_exit_two() {
    // The lifecycle verbs' rule: nothing moved is exit 2. "Already unlinked" and
    // "successfully unlinked" are the same state, and a script asks which one happened.
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let (first, second) = two_unrelated(&sandbox);

    let out = run(&["unlink", "--db", &db, &first, &second]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    assert!(
        test_stdout(&out).contains("no edge"),
        "{}",
        test_stdout(&out)
    );
}

#[test]
fn unlink_narrows_to_one_relation_when_asked() {
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let (first, second) = two_unrelated(&sandbox);
    json(&[
        "link",
        "--db",
        &db,
        &first,
        &second,
        "--relation",
        "contradicts",
        "--json",
    ]);
    json(&[
        "link",
        "--db",
        &db,
        &first,
        &second,
        "--relation",
        "derived_from",
        "--json",
    ]);

    let value = json(&[
        "unlink",
        "--db",
        &db,
        &first,
        &second,
        "--relation",
        "contradicts",
        "--json",
    ]);
    assert_eq!(value["data"]["removed"], serde_json::json!(1));

    let remaining = json(&["relate", "--db", &db, &first, "--json"]);
    let relations: Vec<&str> = remaining["data"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|edge| edge["relation"].as_str().expect("a relation"))
        .collect();
    assert_eq!(relations, vec!["derived_from"]);
}

// ---------------------------------------------------------------------------
// relate
// ---------------------------------------------------------------------------

#[test]
fn relate_reports_a_neighbour_with_its_relation_and_title() {
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let (first, second) = two_unrelated(&sandbox);
    let third = add(&sandbox, "semantic", "omicron pi", "rho sigma tau");
    json(&[
        "link",
        "--db",
        &db,
        &first,
        &second,
        "--relation",
        "precedes",
        "--json",
    ]);

    let value = json(&["relate", "--db", &db, &first, "--json"]);
    let edges = value["data"].as_array().expect("a list");
    assert_eq!(edges.len(), 1, "{value}");
    let edge = &edges[0];
    assert_eq!(edge["memory_id"], serde_json::json!(second));
    assert_eq!(edge["relation"], "precedes");
    assert_eq!(edge["direction"], "out");
    assert_eq!(edge["origin"], "manual");
    assert_eq!(edge["kind"], "procedural");
    assert_eq!(edge["title"], "kappa lambda");
    assert_eq!(edge["excerpt"], "");
    assert_eq!(edge["status"], "active");
    assert!(edge["created_at"].as_str().is_some_and(|at| !at.is_empty()));

    // An empty neighbourhood is an empty list, not a missing key. A third memory with no
    // edges at all is the only honest fixture for that: `second` has this very edge.
    let lonely = json(&["relate", "--db", &db, &third, "--json"]);
    assert_eq!(lonely["data"], serde_json::json!([]));

    // The end it points at sees the same edge from the other side.
    let inbound = json(&["relate", "--db", &db, &second, "--json"]);
    assert_eq!(inbound["data"][0]["direction"], "in");
}

#[test]
fn relate_on_an_unknown_id_is_exit_two_rather_than_an_empty_list() {
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let out = run(&["relate", "--db", &db, "k_01ZZZZZZZZZZZZZZZZZZZZZZZZ"]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    assert!(stderr(&out).contains("k_01ZZZ"), "{}", stderr(&out));
}

#[test]
fn depth_beyond_one_is_refused_rather_than_answered_with_one_hop() {
    // A request for three hops answered with one is the failure mode the contract's exit
    // codes exist to prevent: the caller cannot tell a shallow answer from a thin graph.
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let (first, _) = two_unrelated(&sandbox);

    let out = run(&["relate", "--db", &db, &first, "--depth", "2"]);
    assert_eq!(code(&out), 3, "{}", stderr(&out));
    assert!(stderr(&out).contains("one hop"), "{}", stderr(&out));
}

#[test]
fn relate_filters_by_relation_on_the_command_line() {
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let (first, second) = two_unrelated(&sandbox);
    let third = add(&sandbox, "semantic", "sigma tau", "upsilon phi");
    json(&[
        "link",
        "--db",
        &db,
        &first,
        &second,
        "--relation",
        "supports",
        "--json",
    ]);
    json(&[
        "link",
        "--db",
        &db,
        &first,
        &third,
        "--relation",
        "contradicts",
        "--json",
    ]);

    let filtered = json(&[
        "relate",
        "--db",
        &db,
        &first,
        "--relation",
        "contradicts",
        "--json",
    ]);
    let edges = filtered["data"].as_array().expect("a list");
    assert_eq!(edges.len(), 1, "{filtered}");
    assert_eq!(edges[0]["memory_id"], serde_json::json!(third));
}

#[test]
fn two_related_memories_are_related_without_anybody_asking() {
    // The auto-link pass, end to end: two `add` calls that share a distinctive phrase, and
    // the graph between them exists because activation writes it (§5.2).
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let first = add(
        &sandbox,
        "semantic",
        "the staging deploy needs a manual migration",
        "The staging deploy needs a manual migration before it ships.",
    );
    let second = add(
        &sandbox,
        "semantic",
        "the staging deploy runs its migration",
        "The staging deploy runs its migration automatically now.",
    );

    let value = json(&["relate", "--db", &db, &first, "--json"]);
    let edges = value["data"].as_array().expect("a list");
    assert!(
        edges
            .iter()
            .any(|edge| edge["memory_id"] == serde_json::json!(second)
                && edge["relation"] == "related_to"
                && edge["origin"] == "auto"),
        "a shared subject is a neighbour: {value}"
    );
    assert!(
        edges.iter().all(|edge| edge["direction"].is_null()),
        "`related_to` has no direction to report: {value}"
    );
}

// ---------------------------------------------------------------------------
// get --relations
// ---------------------------------------------------------------------------

#[test]
fn get_relations_wraps_the_memory_and_its_neighbours() {
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let (first, second) = two_unrelated(&sandbox);
    json(&[
        "link",
        "--db",
        &db,
        &first,
        &second,
        "--relation",
        "part_of",
        "--json",
    ]);

    let value = json(&["get", "--db", &db, &first, "--relations", "--json"]);
    assert_eq!(value["data"]["memory"]["id"], serde_json::json!(first));
    assert_eq!(
        value["data"]["memory"]["title"], "alpha beta gamma",
        "the memory object is the one `get` always returns"
    );
    let edges = value["data"]["relations"].as_array().expect("a list");
    assert_eq!(edges.len(), 1, "{value}");
    assert_eq!(edges[0]["relation"], "part_of");
    assert_eq!(edges[0]["memory_id"], serde_json::json!(second));

    // Without the flag the shape is the memory itself, which is what every existing
    // consumer parses.
    let plain = json(&["get", "--db", &db, &first, "--json"]);
    assert_eq!(plain["data"]["id"], serde_json::json!(first));
    assert!(
        plain["data"].get("relations").is_none(),
        "the flag's shape is not added to the unasked-for response: {plain}"
    );
}

#[test]
fn get_relations_on_a_missing_id_still_reports_the_missing_id() {
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let out = run(&[
        "get",
        "--db",
        &db,
        "k_01ZZZZZZZZZZZZZZZZZZZZZZZZ",
        "--relations",
    ]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    assert!(stderr(&out).contains("no memory"), "{}", stderr(&out));
}

// ---------------------------------------------------------------------------
// The graph in the rest of the product
// ---------------------------------------------------------------------------

#[test]
fn a_contradiction_linked_at_the_command_line_reaches_the_packet() {
    // The whole point of the graph landing: `retrieve`'s conflict machinery, ported in
    // WP5.5 when nothing could write the edge it reads, now has a writer.
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let older = add(
        &sandbox,
        "semantic",
        "the staging deploy migrations are manual",
        "The staging deploy needs a manual migration.",
    );
    let newer = add(
        &sandbox,
        "semantic",
        "the staging deploy migrations are automatic",
        "The staging deploy runs its migration automatically.",
    );

    let before = json(&[
        "retrieve",
        "--db",
        &db,
        "--intent",
        "verify",
        "--json",
        "staging deploy migrations",
    ]);
    assert_eq!(before["data"]["conflicts"], serde_json::json!([]));

    json(&[
        "link",
        "--db",
        &db,
        &newer,
        &older,
        "--relation",
        "contradicts",
        "--json",
    ]);

    let after = json(&[
        "retrieve",
        "--db",
        &db,
        "--intent",
        "verify",
        "--json",
        "staging deploy migrations",
    ]);
    let conflicts = after["data"]["conflicts"].as_array().expect("a list");
    assert_eq!(conflicts.len(), 1, "{after}");
    assert!(
        after["data"]["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .any(|warning| warning
                .as_str()
                .is_some_and(|text| text.contains("Conflicting memories"))),
        "{after}"
    );
}

#[test]
fn a_supersession_linked_at_the_command_line_demotes_what_it_replaced() {
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let older = add(
        &sandbox,
        "semantic",
        "the staging deploy migrations are manual",
        "The staging deploy needs a manual migration.",
    );
    let newer = add(
        &sandbox,
        "semantic",
        "the staging deploy migrations are automatic",
        "The staging deploy runs its migration automatically.",
    );
    json(&[
        "link",
        "--db",
        &db,
        &newer,
        &older,
        "--relation",
        "supersedes",
        "--json",
    ]);

    let packet = json(&[
        "retrieve",
        "--db",
        &db,
        "--intent",
        "execute",
        "--json",
        "staging deploy migrations",
    ]);
    assert!(
        packet["data"]["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .any(|warning| warning
                .as_str()
                .is_some_and(|text| text.contains("is superseded by"))),
        "{packet}"
    );
    assert!(
        packet["data"]["retrieved_ids"]
            .as_array()
            .expect("ids")
            .contains(&serde_json::json!(older)),
        "demoted, not dropped: {packet}"
    );
}

#[test]
fn forgetting_a_neighbour_takes_the_edge_with_it() {
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let (first, second) = two_unrelated(&sandbox);
    json(&["link", "--db", &db, &first, &second, "--json"]);

    json(&["forget", "--db", &db, &second, "--yes", "--json"]);

    let value = json(&["relate", "--db", &db, &first, "--json"]);
    assert_eq!(value["data"], serde_json::json!([]));
}

#[test]
fn the_graph_leg_of_a_search_is_still_the_only_refusal_left() {
    // WP5 landed the graph; what it did not land is the *walk as a ranking leg*, which
    // §4.3 gives to v1.1. The message has to be about the leg, because a caller told "the
    // graph is not in this release" would conclude `relate` does not work, and it does.
    let sandbox = Sandbox::new();
    let db = sandbox.db_arg();
    let out = run(&["search", "--db", &db, "--legs", "graph", "anything"]);
    assert_eq!(code(&out), 1, "{}", stderr(&out));
    let text = stderr(&out);
    assert!(text.contains("graph"), "{text}");
    assert!(text.contains("leg"), "{text}");
    assert!(text.contains("v1.1"), "{text}");
}
