// SPDX-License-Identifier: Apache-2.0

//! End-to-end assertions on `kiomon mcp`: the JSON-RPC surface, through the binary.
//!
//! Two things are only testable across a process boundary and both are here: that stdout carries
//! *only* protocol frames (a stray `println!` in a stdio server shows up as a client that hangs,
//! not as an error), and that the tools answer with what the CLI answers — the same claim `serve`
//! makes, checked through a different pipe.

use std::io::{BufRead, BufReader, Write as _};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "kiomon-mcp-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("a sandbox directory must be creatable");
        Self { dir }
    }

    fn db_arg(&self) -> String {
        self.dir.join("brain.db").display().to_string()
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

fn cli_json(args: &[&str]) -> Value {
    let out = run(args);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let value: Value = serde_json::from_str(text.trim())
        .unwrap_or_else(|err| panic!("stdout was not JSON ({err}): {text:?}"));
    assert_eq!(value["ok"], json!(true), "failed: {value}");
    value
}

/// A live MCP session: the child process, and its two pipes.
struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Session {
    fn start(sandbox: &Sandbox) -> Self {
        let mut child = kiomon()
            .args(["--db", &sandbox.db_arg(), "mcp", "-q"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the server must start");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut session = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        let initialized = session.request(
            "initialize",
            &json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" },
            }),
        );
        assert_eq!(initialized["protocolVersion"], "2024-11-05");
        session
    }

    fn send(&mut self, frame: &Value) {
        let line = serde_json::to_string(frame).expect("a frame must serialise");
        writeln!(self.stdin, "{line}").expect("the frame must be writable");
        self.stdin.flush().expect("flush");
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .expect("a reply must arrive");
        serde_json::from_str(line.trim())
            .unwrap_or_else(|err| panic!("a reply was not JSON ({err}): {line:?}"))
    }

    fn request(&mut self, method: &str, params: &Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(
            &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params.clone() }),
        );
        let reply = self.read();
        assert_eq!(reply["id"], json!(id), "the reply id must match: {reply}");
        assert_eq!(reply["jsonrpc"], "2.0");
        if let Some(error) = reply.get("error") {
            panic!("{method} failed at the protocol level: {error}");
        }
        reply["result"].clone()
    }

    fn tool(&mut self, name: &str, arguments: &Value) -> (String, bool) {
        let result = self.request(
            "tools/call",
            &json!({ "name": name, "arguments": arguments }),
        );
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("a tool must answer text: {result}"))
            .to_owned();
        (text, result["isError"].as_bool().unwrap_or(false))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

fn seed(sandbox: &Sandbox) -> (String, String) {
    let db = sandbox.db_arg();
    let first = add(&db, "semantic", "Deploys go through the release workflow");
    let second = add(&db, "procedural", "How to deploy a release");
    (first, second)
}

fn add(db: &str, kind: &str, title: &str) -> String {
    let value = cli_json(&[
        "add",
        "--db",
        db,
        "--json",
        "--kind",
        kind,
        "--title",
        title,
        "--body",
        "Always tag first, then let the workflow verify the tagged commit.",
        "--confidence",
        "0.9",
    ]);
    value["data"]["id"].as_str().expect("an id").to_owned()
}

// ---------------------------------------------------------------------------
// The handshake and the registry
// ---------------------------------------------------------------------------

#[test]
fn initialize_reports_a_protocol_version_and_the_agent_instructions() {
    // The instructions are the transport-level copy of the agent contract. A client that does not
    // load `AGENTS.md` gets them here, and this is the only place they can arrive from.
    let sandbox = Sandbox::new("initialize");
    seed(&sandbox);
    let mut session = Session::start(&sandbox);
    let result = session.request("initialize", &json!({ "protocolVersion": "2024-11-05" }));
    assert_eq!(result["serverInfo"]["name"], "kiomon");
    let instructions = result["instructions"].as_str().expect("instructions");
    assert!(instructions.contains("retrieve_memory"), "{instructions}");
    assert!(instructions.contains("record_outcome"), "{instructions}");
}

#[test]
fn the_tool_list_holds_the_hosted_names_and_describes_every_tool() {
    // An agent's prompt is written against the hosted toolset, so the names have to match — and a
    // tool with no description or no schema is a tool a model cannot decide to call.
    let sandbox = Sandbox::new("tools");
    let mut session = Session::start(&sandbox);
    let result = session.request("tools/list", &json!({}));
    let tools = result["tools"].as_array().expect("tools");

    let names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    for hosted in [
        "retrieve_memory",
        "search_memory",
        "get_memory",
        "draft_memory",
        "record_outcome",
        "manage_memory",
        "list_workspaces",
        "get_workspace_context",
        "explore_memory_graph",
        "select_workspace",
        "reflect_session",
        "get_latest_briefing",
    ] {
        assert!(names.contains(&hosted), "{hosted} is missing");
    }
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or_default();
        assert!(
            tool["description"]
                .as_str()
                .is_some_and(|text| text.len() > 20),
            "{name} has no description"
        );
        assert_eq!(tool["inputSchema"]["type"], "object", "{name}");
    }
}

#[test]
fn a_notification_is_not_answered() {
    // `notifications/initialized` has no id and takes no reply; answering it desynchronises a
    // client that is counting replies.
    let sandbox = Sandbox::new("notification");
    let mut session = Session::start(&sandbox);
    session.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    // The next reply must belong to the next request, which is the proof that the notification
    // produced nothing: a session that answered it would shift every id by one.
    let result = session.request("ping", &json!({}));
    assert_eq!(result, json!({}));
}

#[test]
fn stdout_carries_only_protocol_frames() {
    // The failure this prevents is a client that hangs: one stray line of text on stdout and the
    // parser is reading prose as JSON. Every line the process wrote is parsed here, including the
    // ones the stderr-quiet path might otherwise hide.
    let sandbox = Sandbox::new("stdout-clean");
    seed(&sandbox);
    let mut session = Session::start(&sandbox);
    for _ in 0..3 {
        session.tool("search_memory", &json!({ "query": "deploy" }));
        session.tool("list_workspaces", &json!({}));
    }
    session.request("ping", &json!({}));
}

// ---------------------------------------------------------------------------
// The tools answer
// ---------------------------------------------------------------------------

#[test]
fn a_search_over_mcp_returns_the_ids_the_cli_returns() {
    // The same claim `serve` makes, through the other transport: one verb layer, one answer.
    let sandbox = Sandbox::new("same-search");
    seed(&sandbox);
    let mut session = Session::start(&sandbox);
    let (text, is_error) = session.tool("search_memory", &json!({ "query": "deploy", "limit": 5 }));
    assert!(!is_error, "{text}");

    let cli = cli_json(&[
        "search",
        "--db",
        &sandbox.db_arg(),
        "--json",
        "--limit",
        "5",
        "deploy",
    ]);
    let cli_ids: Vec<&str> = cli["data"]["results"]
        .as_array()
        .expect("results")
        .iter()
        .filter_map(|row| row["id"].as_str())
        .collect();
    assert!(!cli_ids.is_empty(), "the fixture must match something");
    for id in &cli_ids {
        assert!(text.contains(id), "the MCP answer omits {id}: {text}");
    }
}

#[test]
fn a_draft_over_mcp_is_a_memory_the_cli_can_read() {
    let sandbox = Sandbox::new("draft");
    seed(&sandbox);
    let mut session = Session::start(&sandbox);
    let (text, is_error) = session.tool(
        "draft_memory",
        &json!({
            "kind": "semantic",
            "title": "Staging resets nightly",
            "body": "Anything left in staging after 02:00 UTC is gone.",
            "confidence": 0.9,
        }),
    );
    assert!(!is_error, "{text}");

    // Whatever the workspace's review mode decided, the row exists and `list` sees it under
    // `--status any` — which is the check that the write really landed rather than being reported.
    let listed = cli_json(&[
        "list",
        "--db",
        &sandbox.db_arg(),
        "--json",
        "--status",
        "any",
        "--limit",
        "50",
    ]);
    let titles: Vec<&str> = listed["data"]
        .as_array()
        .expect("list returns an array of rows, not an object")
        .iter()
        .filter_map(|row| row["title"].as_str())
        .collect();
    assert!(
        titles.contains(&"Staging resets nightly"),
        "the draft is not in the brain: {titles:?}"
    );
}

#[test]
fn reflect_session_writes_every_learning() {
    let sandbox = Sandbox::new("reflect");
    seed(&sandbox);
    let mut session = Session::start(&sandbox);
    let (text, is_error) = session.tool(
        "reflect_session",
        &json!({
            "learnings": [
                { "kind": "semantic", "title": "First reflection", "body": "One." },
                { "kind": "procedural", "title": "Second reflection", "body": "Two." },
            ]
        }),
    );
    assert!(!is_error, "{text}");
    assert!(text.contains("2 drafted"), "{text}");
}

#[test]
fn record_outcome_credits_the_memory_the_packet_returned() {
    // The loop the whole product is for: retrieve, work, report. If the credit does not land, the
    // strength of a memory never reflects whether it helped.
    let sandbox = Sandbox::new("outcome");
    let (first, _) = seed(&sandbox);
    let mut session = Session::start(&sandbox);

    let (text, is_error) = session.tool(
        "record_outcome",
        &json!({
            "outcome": "success",
            "items": [{ "memory_id": first, "assessment": "helpful" }],
        }),
    );
    assert!(!is_error, "{text}");

    let memory = cli_json(&["get", "--db", &sandbox.db_arg(), "--json", &first]);
    assert!(
        memory["data"]["feedback_count"]
            .as_i64()
            .unwrap_or_default()
            >= 1,
        "the credit did not land: {}",
        memory["data"]
    );
}

#[test]
fn manage_memory_pins_and_rates_and_refuses_revert() {
    let sandbox = Sandbox::new("manage");
    let (first, _) = seed(&sandbox);
    let mut session = Session::start(&sandbox);

    let (text, is_error) = session.tool("manage_memory", &json!({ "action": "pin", "id": first }));
    assert!(!is_error, "{text}");
    let memory = cli_json(&["get", "--db", &sandbox.db_arg(), "--json", &first]);
    assert_eq!(memory["data"]["pinned"], json!(true));

    let (text, is_error) = session.tool(
        "manage_memory",
        &json!({ "action": "rate", "id": first, "value": -1 }),
    );
    assert!(!is_error, "{text}");

    let (text, is_error) =
        session.tool("manage_memory", &json!({ "action": "revert", "id": first }));
    assert!(is_error, "revert must be an error, not a success: {text}");
    assert!(text.contains("revision history"), "{text}");
}

#[test]
fn the_graph_is_reachable_over_mcp_and_a_second_hop_is_refused() {
    let sandbox = Sandbox::new("graph");
    let (first, _) = seed(&sandbox);
    let mut session = Session::start(&sandbox);

    let (text, is_error) = session.tool("get_memory_relations", &json!({ "id": first }));
    assert!(!is_error, "{text}");
    assert!(text.contains("related_to"), "{text}");

    let (text, is_error) =
        session.tool("explore_memory_graph", &json!({ "id": first, "depth": 2 }));
    assert!(
        is_error,
        "depth 2 is a path search this engine does not do: {text}"
    );
    assert!(text.contains("one hop"), "{text}");
}

#[test]
fn select_workspace_scopes_the_tools_that_follow_it() {
    // A session default is a convenience the hosted product also keeps; an explicit
    // `workspace_id` still wins, which is asserted by the second call.
    let sandbox = Sandbox::new("select");
    let db = sandbox.db_arg();
    run(&["init", "--db", &db, "--workspace", "second"]);
    let mut session = Session::start(&sandbox);

    let (text, is_error) = session.tool("select_workspace", &json!({ "workspace_id": "second" }));
    assert!(!is_error, "{text}");

    let (text, is_error) = session.tool(
        "draft_memory",
        &json!({ "kind": "semantic", "title": "In the second workspace", "body": "Body." }),
    );
    assert!(!is_error, "{text}");

    let listed = cli_json(&[
        "list",
        "--db",
        &db,
        "--json",
        "--workspace",
        "second",
        "--status",
        "any",
    ]);
    let titles: Vec<&str> = listed["data"]
        .as_array()
        .expect("list returns an array of rows, not an object")
        .iter()
        .filter_map(|row| row["title"].as_str())
        .collect();
    assert!(
        titles.contains(&"In the second workspace"),
        "the draft landed in the wrong workspace: {titles:?}"
    );

    let (_, is_error) = session.tool("select_workspace", &json!({ "workspace_id": "nope" }));
    assert!(is_error, "an unknown workspace must not be selected");
}

#[test]
fn a_briefing_says_it_is_not_available_locally_instead_of_returning_nothing() {
    // The distinction the contract insists on: an empty briefing and a missing feature look
    // identical to an agent, and one of them makes it stop asking.
    let sandbox = Sandbox::new("briefing");
    seed(&sandbox);
    let mut session = Session::start(&sandbox);
    let (text, is_error) =
        session.tool("get_latest_briefing", &json!({ "workspace_id": "default" }));
    assert!(is_error, "{text}");
    assert!(text.contains("get_workspace_context"), "{text}");
}

#[test]
fn an_unknown_tool_is_an_error_result_and_not_a_dead_session() {
    // A model calls a tool that does not exist; the session has to survive it, because a client
    // that dies here loses the conversation it was in the middle of.
    let sandbox = Sandbox::new("unknown-tool");
    seed(&sandbox);
    let mut session = Session::start(&sandbox);
    let (text, is_error) = session.tool("no_such_tool", &json!({}));
    assert!(is_error, "{text}");
    assert!(text.contains("tools/list"), "{text}");

    let (text, is_error) = session.tool("list_workspaces", &json!({}));
    assert!(!is_error, "the session must still answer: {text}");
}

#[test]
fn a_malformed_frame_is_a_protocol_error_and_the_session_continues() {
    let sandbox = Sandbox::new("bad-frame");
    let mut session = Session::start(&sandbox);
    writeln!(session.stdin, "{{not json").expect("writable");
    session.stdin.flush().expect("flush");

    let broken = session.read();
    assert_eq!(broken["error"]["code"], json!(-32700));
    assert_eq!(broken["id"], Value::Null);

    let result = session.request("ping", &json!({}));
    assert_eq!(result, json!({}));
}

#[test]
fn a_tool_call_with_a_missing_argument_names_the_argument() {
    let sandbox = Sandbox::new("missing-arg");
    seed(&sandbox);
    let mut session = Session::start(&sandbox);
    let (text, is_error) = session.tool("retrieve_memory", &json!({ "intent": "execute" }));
    assert!(is_error, "{text}");
    assert!(text.contains("query"), "{text}");
}
