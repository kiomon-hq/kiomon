// SPDX-License-Identifier: Apache-2.0

//! End-to-end assertions on `kiomon serve`: the HTTP surface, through the binary.
//!
//! What is tested here is what only a process and a socket can show: the handshake line a
//! supervisor parses, the two refusals that keep browsers out (and the third that `--require-token`
//! adds on request), the status and code of every route in Appendix C, and the one promise the
//! whole work package rests on — that the HTTP answer and the CLI answer come from the same read
//! of the same file.
//!
//! The client is hand-written over `std::net::TcpStream` rather than pulled in as a dependency:
//! a server whose tests need an HTTP client crate to check one response is a server that has
//! made testing it somebody's dependency decision, and this workspace has one dependency
//! argument at a time.

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, ErrorKind, Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A temporary directory that removes itself when the test ends.
struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "kiomon-serve-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("a sandbox directory must be creatable");
        Self { dir }
    }

    fn db(&self) -> PathBuf {
        self.dir.join("brain.db")
    }

    fn db_arg(&self) -> String {
        self.db().display().to_string()
    }

    fn token(&self) -> String {
        std::fs::read_to_string(self.dir.join("serve.token"))
            .expect("the token file must exist once the server is up")
            .trim()
            .to_owned()
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
    let mut command = kiomon();
    command
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("the binary must be runnable")
}

fn json(args: &[&str]) -> serde_json::Value {
    let out = run(args);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let value: serde_json::Value = serde_json::from_str(text.trim())
        .unwrap_or_else(|err| panic!("stdout was not JSON ({err}): {text:?}"));
    assert_eq!(value["ok"], serde_json::json!(true), "failed: {value}");
    value
}

/// A server process, killed when the test ends.
///
/// The port is read from the handshake line rather than chosen by the test: `--port 0` is what
/// the installer and the release workflow use, so the line itself has to be the interface.
struct Server {
    child: Child,
    url: String,
    /// Present only when the server was started with `--require-token`.
    token: Option<String>,
}

impl Server {
    /// The default server: loopback only, no credential.
    fn start(sandbox: &Sandbox) -> Self {
        Self::spawn(sandbox, false)
    }

    /// The opt-in server: `--require-token`, for a shared machine.
    fn start_requiring_token(sandbox: &Sandbox) -> Self {
        Self::spawn(sandbox, true)
    }

    fn spawn(sandbox: &Sandbox, require_token: bool) -> Self {
        let mut command = kiomon();
        command.args([
            "--db",
            &sandbox.db_arg(),
            "serve",
            "--port",
            "0",
            "--json",
            "-q",
        ]);
        if require_token {
            command.arg("--require-token");
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the server must start");
        let mut handshake = String::new();
        BufReader::new(child.stdout.as_mut().expect("stdout"))
            .read_line(&mut handshake)
            .expect("the handshake line must arrive");
        let value: serde_json::Value = serde_json::from_str(handshake.trim())
            .unwrap_or_else(|err| panic!("the handshake was not JSON ({err}): {handshake:?}"));
        assert_eq!(value["listening"], serde_json::json!(true), "{value}");
        let url = value["url"].as_str().expect("a url").to_owned();
        // The handshake says whether a credential is expected; the test reads it rather than
        // assuming, which is the same discipline a client should follow.
        let expected = value["auth"].as_str().expect("an auth mode");
        assert_eq!(expected == "token", require_token, "{value}");
        let token = require_token.then(|| sandbox.token());
        Self { child, url, token }
    }

    fn authority(&self) -> String {
        self.url.trim_start_matches("http://").to_owned()
    }

    /// One request with whatever credential this server expects, returning status and body.
    fn call(&self, method: &str, path: &str, body: Option<&str>) -> Reply {
        match &self.token {
            Some(token) => self.request(method, path, body, &[("X-API-Key", token.as_str())]),
            None => self.request(method, path, body, &[]),
        }
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        headers: &[(&str, &str)],
    ) -> Reply {
        let mut head = format!("{method} {path} HTTP/1.1\r\n");
        // The default header set is what a well-behaved client sends; a test that supplies its
        // own `Host` is testing the rebinding refusal, and two `Host` headers would make it test
        // whichever one the parser happened to keep first.
        if !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("host"))
        {
            // `write!` rather than `push_str(&format!(..))`: the latter allocates a second string
            // per header line, and the lint that says so is right.
            write!(head, "Host: {}\r\n", self.authority()).expect("a String write cannot fail");
        }
        for (name, value) in headers {
            write!(head, "{name}: {value}\r\n").expect("a String write cannot fail");
        }
        if let Some(body) = body {
            write!(
                head,
                "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .expect("a String write cannot fail");
            head.push_str(body);
        } else {
            head.push_str("\r\n");
        }
        let mut stream = TcpStream::connect(self.authority()).expect("the port must accept");
        stream
            .write_all(head.as_bytes())
            .expect("the request must be writable");
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .expect("the response must be readable");
        let text = String::from_utf8_lossy(&raw).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
        let status: u16 = head
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("no status line in {head:?}"));
        Reply {
            status,
            body: body.to_owned(),
            raw: text,
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

struct Reply {
    status: u16,
    body: String,
    raw: String,
}

impl Reply {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|err| panic!("body was not JSON ({err}): {:?}", self.body))
    }

    fn code(&self) -> String {
        self.json()["code"].as_str().unwrap_or_default().to_owned()
    }
}

/// A brain with two memories that share vocabulary, so auto-linking has a neighbour to draw.
fn seed(sandbox: &Sandbox) -> (String, String) {
    let db = sandbox.db_arg();
    let first = add(
        sandbox,
        &db,
        "semantic",
        "Deploys go through the release workflow",
    );
    let second = add(sandbox, &db, "procedural", "How to deploy a release");
    (first, second)
}

fn add(sandbox: &Sandbox, db: &str, kind: &str, title: &str) -> String {
    let _ = sandbox;
    let value = json(&[
        "add",
        "--db",
        db,
        "--json",
        "--kind",
        kind,
        "--title",
        title,
        "--body",
        "Always \
         tag first, then let the workflow verify the tagged commit.",
        "--confidence",
        "0.9",
    ]);
    value["data"]["id"].as_str().expect("an id").to_owned()
}

fn path_of(url: &str) -> &str {
    url.split_once("://").map_or(url, |pair| pair.1)
}

// ---------------------------------------------------------------------------
// The handshake
// ---------------------------------------------------------------------------

#[test]
fn the_handshake_is_one_line_on_stdout_and_says_no_credential_is_expected() {
    // A supervisor learns the port from this line, and nothing else in the process writes to
    // stdout before it. Both facts are asserted because a stray `println!` on a startup path is
    // how a parseable handshake stops being parseable. `auth` is always present, so a client can
    // branch on it rather than on whether `token_file` happens to exist.
    let sandbox = Sandbox::new("handshake");
    let mut child = kiomon()
        .args([
            "--db",
            &sandbox.db_arg(),
            "serve",
            "--port",
            "0",
            "--json",
            "-q",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the server must start");
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().expect("stdout"))
        .read_line(&mut line)
        .expect("a handshake");
    child.kill().ok();
    child.wait().ok();

    let value: serde_json::Value = serde_json::from_str(line.trim()).expect("one JSON line");
    assert_eq!(value["listening"], serde_json::json!(true));
    assert!(value["port"].as_u64().expect("a port") > 0);
    assert!(
        value["url"]
            .as_str()
            .expect("a url")
            .starts_with("http://127.0.0.1:")
    );
    assert_eq!(value["auth"], serde_json::json!("none"));
    assert!(
        value.get("token_file").is_none(),
        "a server with no auth must not advertise a token file: {value}"
    );
    assert!(
        !sandbox.db().with_file_name("serve.token").exists(),
        "the default run must not create a token file at all"
    );
}

#[test]
fn the_handshake_names_the_token_file_when_a_token_is_demanded() {
    // The opt-in mode, and the only mode where a token file exists. The value is printed for a
    // human on the non-JSON path; the handshake carries the path, never the secret, because a
    // handshake is the line that ends up in a supervisor's log.
    let sandbox = Sandbox::new("handshake-token");
    let mut child = kiomon()
        .args([
            "--db",
            &sandbox.db_arg(),
            "serve",
            "--port",
            "0",
            "--json",
            "-q",
            "--require-token",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the server must start");
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().expect("stdout"))
        .read_line(&mut line)
        .expect("a handshake");
    child.kill().ok();
    child.wait().ok();

    let value: serde_json::Value = serde_json::from_str(line.trim()).expect("one JSON line");
    assert_eq!(value["auth"], serde_json::json!("token"));
    assert!(value["token_created"].as_bool().expect("a flag"));
    let path = value["token_file"].as_str().expect("a path");
    assert!(Path::new(path).exists());
    assert!(
        !line.contains(&sandbox.token()),
        "the handshake leaked the token"
    );
}

#[test]
fn a_second_start_reuses_the_token_instead_of_writing_a_new_one() {
    // A token that changed on every restart would be a token every client has to be
    // reconfigured with, which is the opposite of what a credential is for.
    let sandbox = Sandbox::new("token-reuse");
    let first = Server::start_requiring_token(&sandbox);
    let token = first.token.clone();
    drop(first);

    let second = Server::start_requiring_token(&sandbox);
    assert_eq!(second.token, token, "the token must survive a restart");
}

#[test]
fn a_port_already_in_use_is_an_error_that_names_it() {
    let sandbox = Sandbox::new("port-busy");
    let server = Server::start(&sandbox);
    let port = server.url.rsplit(':').next().expect("a port").to_owned();

    // The human path, because that is where the message goes to stderr: `--json` reports a
    // failure on stdout as an envelope, and this assertion is about the sentence a person reads.
    let out = run(&["--db", &sandbox.db_arg(), "serve", "--port", &port]);
    assert_eq!(out.status.code(), Some(1), "a bind failure is exit 1");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.contains(&port),
        "the message must name the port: {stderr}"
    );
    // And nothing was printed to stdout, so a supervisor that reads the handshake sees an
    // error rather than a url that does not answer.
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "");

    // On the JSON path the failure is one envelope on stdout, which is the contract every
    // command keeps — and it still names the port.
    let out = run(&[
        "--db",
        &sandbox.db_arg(),
        "serve",
        "--port",
        &port,
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(1));
    let envelope = String::from_utf8_lossy(&out.stdout).into_owned();
    let value: serde_json::Value =
        serde_json::from_str(envelope.trim()).unwrap_or_else(|err| panic!("{err}: {envelope:?}"));
    assert_eq!(value["ok"], serde_json::json!(false));
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains(&port)
    );
}

#[test]
fn once_answers_one_request_and_exits_zero() {
    // The smoke test the release workflow runs: it proves the shipped binary can bind, answer
    // and stop without a supervisor.
    let sandbox = Sandbox::new("once");
    let mut child = kiomon()
        .args([
            "--db",
            &sandbox.db_arg(),
            "serve",
            "--port",
            "0",
            "--once",
            "--json",
            "-q",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the server must start");
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().expect("stdout"))
        .read_line(&mut line)
        .expect("a handshake");
    let value: serde_json::Value = serde_json::from_str(line.trim()).expect("one JSON line");
    let authority = value["url"]
        .as_str()
        .expect("a url")
        .trim_start_matches("http://")
        .to_owned();
    let mut stream = TcpStream::connect(&authority).expect("the port must accept");
    stream
        .write_all(format!("GET /health HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes())
        .expect("writable");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("readable");
    assert!(raw.starts_with("HTTP/1.1 200 OK"), "{raw}");

    let status = child.wait().expect("the process must exit by itself");
    assert_eq!(status.code(), Some(0));
}

// ---------------------------------------------------------------------------
// The three refusals
// ---------------------------------------------------------------------------

#[test]
fn a_request_without_the_token_is_refused_before_it_can_learn_which_routes_exist() {
    // A known path and an unknown one answer identically, which is what stops an unauthorised
    // client from mapping the surface by comparing `401` with `404`.
    let sandbox = Sandbox::new("no-token");
    let server = Server::start_requiring_token(&sandbox);

    let known = server.request("GET", "/api/workspaces", None, &[]);
    let unknown = server.request("GET", "/api/nothing", None, &[]);
    assert_eq!(known.status, 401);
    assert_eq!(unknown.status, 401);
    assert_eq!(known.code(), "unauthorized");
    assert_eq!(unknown.code(), "unauthorized");

    let wrong = server.request("GET", "/api/workspaces", None, &[("X-API-Key", "nope")]);
    assert_eq!(wrong.status, 401, "{}", wrong.raw);
}

#[test]
fn the_token_is_accepted_in_either_header() {
    let sandbox = Sandbox::new("two-headers");
    let server = Server::start_requiring_token(&sandbox);
    let token = server.token.clone().expect("token mode has a token");
    let bearer = format!("Bearer {token}");

    assert_eq!(
        server
            .request("GET", "/health", None, &[("X-API-Key", token.as_str())])
            .status,
        200
    );
    assert_eq!(
        server
            .request(
                "GET",
                "/health",
                None,
                &[("Authorization", bearer.as_str())]
            )
            .status,
        200
    );
}

#[test]
fn a_foreign_origin_is_refused_and_a_loopback_one_is_not() {
    // The rule a page in a browser cannot get around: its `Origin` is not forgeable.
    let sandbox = Sandbox::new("origin");
    let server = Server::start(&sandbox);

    let foreign = server.request(
        "GET",
        "/api/workspaces",
        None,
        &[("Origin", "https://evil.example")],
    );
    assert_eq!(foreign.status, 403, "{}", foreign.raw);
    assert_eq!(foreign.code(), "forbidden");

    let local = server.request(
        "GET",
        "/api/workspaces",
        None,
        &[("Origin", "http://localhost:5173")],
    );
    assert_eq!(local.status, 200, "{}", local.raw);

    // Absent is "not a browser", and every SDK in the world omits it.
    let absent = server.call("GET", "/api/workspaces", None);
    assert_eq!(absent.status, 200);
}

#[test]
fn a_host_header_that_is_not_a_loopback_name_is_refused() {
    // DNS rebinding: an attacker's domain resolves to 127.0.0.1, the browser sends the
    // attacker's own `Host`, and everything except this check looks local.
    let sandbox = Sandbox::new("host");
    let server = Server::start(&sandbox);
    let reply = server.request(
        "GET",
        "/api/workspaces",
        None,
        &[("Host", "brain.evil.example")],
    );
    assert_eq!(reply.status, 403, "{}", reply.raw);
    assert!(
        reply.json()["error"]
            .as_str()
            .unwrap_or_default()
            .contains("loopback"),
        "{}",
        reply.body
    );
}

// ---------------------------------------------------------------------------
// Every route in Appendix C
// ---------------------------------------------------------------------------

#[test]
fn every_appendix_c_route_answers_or_says_unsupported_locally() {
    // The acceptance criterion, route by route. A route that fell out of the table would be a
    // `404` that a client reads as "nothing there", which is the failure mode Appendix C exists
    // to prevent — so the list is asserted here and again in `http.rs`'s unit test.
    let sandbox = Sandbox::new("routes");
    let (first, second) = seed(&sandbox);
    let server = Server::start(&sandbox);

    let answered: [(&str, &str, Option<&str>); 13] = [
        ("GET", "/api/search?q=deploy&limit=3", None),
        ("GET", "/api/search?q=deploy&legs=hybrid", None),
        ("GET", &format!("/api/documents/{first}"), None),
        ("GET", &format!("/api/documents/{first}/links"), None),
        ("GET", &format!("/api/graph?center_id={first}"), None),
        ("GET", "/api/workspaces", None),
        ("GET", "/api/workspaces/default/context?topic=deploy", None),
        ("GET", "/api/memories?status=pending_approval", None),
        (
            "POST",
            "/api/retrieve",
            Some(r#"{"intent":"execute","query":"deploy"}"#),
        ),
        ("POST", "/api/documents/{id}/rate", Some(r#"{"rating":1}"#)),
        ("PATCH", "/api/documents/{id}", Some(r#"{"pinned":1}"#)),
        ("POST", "/api/documents/{id}/archive", None),
        ("DELETE", "/api/documents/{id}/links/{other}", None),
    ];
    for (method, target, body) in answered {
        let target = target.replace("{id}", &second).replace("{other}", &first);
        let target = target.as_str();
        let reply = server.call(method, target, body);
        assert!(
            (200..300).contains(&reply.status),
            "{method} {target} answered {} {}",
            reply.status,
            reply.body
        );
    }

    let unsupported: [(&str, &str); 3] = [
        ("POST", "/api/memories/{id}/revert"),
        ("GET", "/api/workspaces/default/briefings/latest"),
        ("GET", "/api/graph?center_id={id}&depth=2"),
    ];
    for (method, target) in unsupported {
        let target = target.replace("{id}", &first);
        let reply = server.call(method, &target, None);
        assert_eq!(reply.status, 501, "{method} {target}: {}", reply.body);
        assert_eq!(
            reply.code(),
            "unsupported_locally",
            "{method} {target}: {}",
            reply.body
        );
        assert!(
            !reply.body.contains("[]"),
            "an unsupported route must not answer an empty shape: {}",
            reply.body
        );
    }
}

#[test]
fn an_unknown_route_is_a_404_and_a_known_path_with_the_wrong_method_is_a_405() {
    // The difference matters: one means check the URL, the other means check the verb.
    let sandbox = Sandbox::new("404-405");
    let (first, _) = seed(&sandbox);
    let server = Server::start(&sandbox);

    let unknown = server.call("GET", "/api/nonsense", None);
    assert_eq!(unknown.status, 404);
    assert_eq!(unknown.code(), "not_found");

    let wrong = server.call("PUT", "/api/retrieve", Some("{}"));
    assert_eq!(wrong.status, 405, "{}", wrong.raw);
    assert!(wrong.body.contains("PUT"), "{}", wrong.body);

    // And the three verbs that share `/api/documents/{id}` all route.
    let document = format!("/api/documents/{first}");
    assert_eq!(server.call("GET", &document, None).status, 200);
    assert_eq!(
        server
            .call("PATCH", &document, Some(r#"{"pinned":1}"#))
            .status,
        200
    );
    assert_eq!(
        server.call("DELETE", &document, None).status,
        200,
        "delete routes through the same pattern"
    );
}

#[test]
fn head_answers_like_get_without_a_body() {
    let sandbox = Sandbox::new("head");
    let server = Server::start(&sandbox);
    let reply = server.call("HEAD", "/health", None);
    assert_eq!(reply.status, 200);
    assert!(reply.body.is_empty(), "a HEAD body: {:?}", reply.body);
    assert!(
        reply.raw.contains("Content-Length:"),
        "a HEAD response describes the body it omits: {}",
        reply.raw
    );
}

// ---------------------------------------------------------------------------
// One verb layer
// ---------------------------------------------------------------------------

#[test]
fn search_over_http_returns_the_ids_the_cli_returns() {
    // The claim the work package makes: two transports, one answer. If this diverges, the
    // local brain answers differently depending on which door a caller used.
    let sandbox = Sandbox::new("same-search");
    seed(&sandbox);
    let server = Server::start(&sandbox);

    let over_http = server.call("GET", "/api/search?q=deploy&limit=5", None);
    assert_eq!(over_http.status, 200, "{}", over_http.body);
    let http_ids: Vec<String> = over_http.json()["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|row| row["id"].as_str().expect("an id").to_owned())
        .collect();

    let cli = json(&[
        "search",
        "--db",
        &sandbox.db_arg(),
        "--json",
        "--limit",
        "5",
        "deploy",
    ]);
    let cli_ids: Vec<String> = cli["data"]["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|row| row["id"].as_str().expect("an id").to_owned())
        .collect();

    assert!(!cli_ids.is_empty(), "the fixture must match something");
    assert_eq!(http_ids, cli_ids);
}

#[test]
fn the_packet_over_http_is_the_packet_the_cli_builds() {
    // `retrieve` returns the packet verbatim, so the two must be equal field for field. The CLI
    // nests it in an envelope; the route does not, because the hosted route does not.
    let sandbox = Sandbox::new("same-packet");
    seed(&sandbox);
    let server = Server::start(&sandbox);

    let over_http = server.call(
        "POST",
        "/api/retrieve",
        Some(r#"{"intent":"execute","query":"deploy a release","limit":5}"#),
    );
    assert_eq!(over_http.status, 200, "{}", over_http.body);

    let cli = json(&[
        "retrieve",
        "--db",
        &sandbox.db_arg(),
        "--json",
        "--intent",
        "execute",
        "--limit",
        "5",
        "--no-record",
        "deploy a release",
    ]);
    let from_cli = cli["data"].clone();

    // `retrieved_ids` is where the two can differ: the HTTP route records the read and the CLI
    // was told not to, so the field's *content* is compared only for the items.
    let http = over_http.json();
    assert_eq!(http["summary"], from_cli["summary"]);
    assert_eq!(http["facts"], from_cli["facts"]);
    assert_eq!(http["warnings"], from_cli["warnings"]);
    assert_eq!(http["conflicts"], from_cli["conflicts"]);
}

#[test]
fn a_patch_over_http_is_visible_to_the_cli() {
    // The other direction of the same claim: a write through one door is a write the other
    // door reads.
    let sandbox = Sandbox::new("write-through");
    let (first, _) = seed(&sandbox);
    let server = Server::start(&sandbox);

    let patched = server.call(
        "PATCH",
        &format!("/api/documents/{first}"),
        Some(r#"{"title":"Renamed over HTTP","pinned":1}"#),
    );
    assert_eq!(patched.status, 200, "{}", patched.body);

    let read = json(&["get", "--db", &sandbox.db_arg(), "--json", &first]);
    assert_eq!(read["data"]["title"], "Renamed over HTTP");
    assert_eq!(read["data"]["pinned"], serde_json::json!(true));
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

#[test]
fn a_chunked_body_is_refused_by_name_rather_than_mis_parsed() {
    let sandbox = Sandbox::new("chunked");
    seed(&sandbox);
    let server = Server::start(&sandbox);

    let head = format!(
        "POST /api/retrieve HTTP/1.1\r\nHost: {}\r\nTransfer-Encoding: \
         chunked\r\nContent-Type: application/json\r\n\r\n",
        server.authority()
    );
    let mut stream = TcpStream::connect(server.authority()).expect("connect");
    stream.write_all(head.as_bytes()).expect("writable");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("readable");
    assert!(raw.starts_with("HTTP/1.1 411"), "{raw}");
    assert!(raw.contains("Content-Length"), "{raw}");
}

#[test]
fn an_absurd_header_block_is_refused_instead_of_allocating() {
    let sandbox = Sandbox::new("header-cap");
    let server = Server::start(&sandbox);
    let mut head = format!("GET /health HTTP/1.1\r\nHost: {}\r\n", server.authority());
    for index in 0..2000 {
        write!(head, "X-Filler-{index}: {}\r\n", "x".repeat(64))
            .expect("a String write cannot fail");
    }
    head.push_str("\r\n");
    let mut stream = TcpStream::connect(server.authority()).expect("connect");
    // The server answers and closes before reading all of it, so the write may fail with a
    // broken pipe — that is the refusal working, not the test failing.
    stream.write_all(head.as_bytes()).ok();
    // macOS answers the pending bytes with a reset rather than a close, so the read itself
    // reports `ECONNRESET`. A reset *is* the refusal: what this test is about is that the
    // server answered and hung up instead of buffering 128 KiB, not the polite way it did it.
    let mut raw = String::new();
    match stream.read_to_string(&mut raw) {
        Ok(_) => assert!(
            raw.starts_with("HTTP/1.1 431") || raw.starts_with("HTTP/1.1 400"),
            "{raw}"
        ),
        Err(error) => assert_eq!(
            error.kind(),
            ErrorKind::ConnectionReset,
            "the only failure a refusal can cause on the read side is a reset: {error}"
        ),
    }
}

#[test]
fn a_malformed_json_body_is_a_400_and_the_server_still_answers_the_next_request() {
    let sandbox = Sandbox::new("bad-json");
    seed(&sandbox);
    let server = Server::start(&sandbox);

    let broken = server.call("POST", "/api/retrieve", Some("{not json"));
    assert_eq!(broken.status, 400, "{}", broken.body);
    assert_eq!(broken.code(), "validation_failed");

    let unknown_field = server.call(
        "POST",
        "/api/retrieve",
        Some(r#"{"intent":"plan","query":"x","surprise":1}"#),
    );
    assert_eq!(unknown_field.status, 400);
    assert!(
        unknown_field.body.contains("surprise"),
        "{}",
        unknown_field.body
    );

    let fine = server.call(
        "POST",
        "/api/retrieve",
        Some(r#"{"intent":"plan","query":"deploy"}"#),
    );
    assert_eq!(fine.status, 200, "the server must survive a bad frame");
}

#[test]
fn a_legs_value_the_hosted_product_accepts_is_accepted_here() {
    // `hybrid` is the hosted spelling of this engine's `auto`. A caller reading `legs` off a
    // response asks for it back by the name it saw.
    let sandbox = Sandbox::new("legs");
    seed(&sandbox);
    let server = Server::start(&sandbox);
    for legs in ["hybrid", "fts", "vector", "auto"] {
        let reply = server.call("GET", &format!("/api/search?q=deploy&legs={legs}"), None);
        // `vector` is refused *only* because no embedder is configured, and that is a `503`:
        // the request was fine and the environment cannot serve it.
        assert!(
            reply.status == 200 || reply.status == 503,
            "legs={legs} answered {} {}",
            reply.status,
            reply.body
        );
    }
    let graph = server.call("GET", "/api/search?q=deploy&legs=graph", None);
    assert_eq!(graph.status, 501, "{}", graph.body);
    let nonsense = server.call("GET", "/api/search?q=deploy&legs=sideways", None);
    assert_eq!(nonsense.status, 400, "{}", nonsense.body);
}

#[test]
fn the_error_body_carries_a_code_a_client_can_branch_on() {
    // The hosted contract's rule: `error` is prose, `code` is stable. A client that pattern
    // matched the prose would break on the next message edit.
    let sandbox = Sandbox::new("codes");
    let (first, _) = seed(&sandbox);
    let server = Server::start(&sandbox);

    for (method, path, body, code) in [
        (
            "GET",
            "/api/documents/k_01NOTHERE".to_owned(),
            None,
            "not_found",
        ),
        (
            "POST",
            "/api/retrieve".to_owned(),
            Some(r#"{"intent":"nope","query":"x"}"#),
            "validation_failed",
        ),
        ("GET", "/api/graph".to_owned(), None, "bad_request"),
        (
            "POST",
            format!("/api/documents/{first}/rate"),
            Some(r#"{"rating":3}"#),
            "bad_request",
        ),
    ] {
        let reply = server.call(method, &path, body);
        assert_eq!(reply.code(), code, "{method} {path}: {}", reply.body);
        assert!(
            reply.json()["error"]
                .as_str()
                .is_some_and(|text| !text.is_empty()),
            "every refusal needs prose: {}",
            reply.body
        );
    }
}

// ---------------------------------------------------------------------------
// The wire the published SDKs declare
// ---------------------------------------------------------------------------

/// The fields the two published SDKs declare *required* on a document.
///
/// Transcribed from `SDK/kiomon-py/src/kiomon/types.py` (`MemoryDocument`) and
/// `SDK/kiomon-ts/sdk/src/types.ts`. Both files mark every one of these as non-optional, so a
/// missing field is not a stylistic difference — it is a `KeyError` on the Python side and a
/// `TypeError` in a `strict` TypeScript build.
const DOCUMENT_REQUIRED: [&str; 24] = [
    "id",
    "source_url",
    "title",
    "author",
    "excerpt",
    "tags",
    "user_notes",
    "body",
    "pinned",
    "created_at",
    "workspace_id",
    "source",
    "kind",
    "confidence",
    "corroboration_count",
    "importance",
    "strength",
    "retrieval_count",
    "last_retrieved_at",
    "feedback_score",
    "feedback_count",
    "scope",
    "status",
    "expires_at",
];

/// The same for a search hit (`SearchHit` in both SDKs).
const HIT_REQUIRED: [&str; 7] = [
    "id",
    "title",
    "source_url",
    "excerpt",
    "snippet",
    "tags",
    "created_at",
];

#[test]
fn a_document_carries_the_fields_the_sdks_declare_required() {
    let sandbox = Sandbox::new("wire-document");
    let (first, _) = seed(&sandbox);
    let server = Server::start(&sandbox);

    let reply = server.call("GET", &format!("/api/documents/{first}"), None);
    assert_eq!(reply.status, 200, "{}", reply.body);
    let document = reply.json();
    for field in DOCUMENT_REQUIRED {
        assert!(
            document.get(field).is_some(),
            "the document is missing `{field}`, which both SDKs declare required: {document}"
        );
    }
    // `tags` is a JSON-encoded *string* on the hosted wire — the TypeScript SDK says so in a
    // comment on the interface, and the Python SDK types it `str | None`. Serving an array
    // breaks `json.loads(doc["tags"])`, which is how the field is documented to be read.
    let tags = document["tags"]
        .as_str()
        .unwrap_or_else(|| panic!("`tags` must be a JSON string, got {}", document["tags"]));
    let decoded: serde_json::Value = serde_json::from_str(tags).expect("tags must decode");
    assert_eq!(decoded, serde_json::json!([]), "no tags were given");
    // `pinned` is the hosted row's integer, and the `PATCH {"pinned": 1}` the SDK sends follows
    // from it; a boolean here would round-trip as `true` where a client compares against `1`.
    assert_eq!(document["pinned"], serde_json::json!(0), "{document}");
    assert_eq!(document["scope"], serde_json::json!("workspace"));
    assert_eq!(document["source_url"], serde_json::json!(""));
}

#[test]
fn a_search_hit_carries_the_fields_the_sdks_declare_required() {
    let sandbox = Sandbox::new("wire-hit");
    seed(&sandbox);
    let server = Server::start(&sandbox);

    // A tag, so the encoding is checked against something that is not the empty list.
    let tagged = json(&[
        "add",
        "--db",
        &sandbox.db_arg(),
        "--json",
        "--kind",
        "semantic",
        "--title",
        "Tagged for the wire",
        "--body",
        "A body so the search finds it.",
        "--tag",
        "deploy",
        "--tag",
        "staging",
    ]);
    assert_eq!(tagged["ok"], serde_json::json!(true), "{tagged}");

    let reply = server.call("GET", "/api/search?q=deploy&limit=10", None);
    assert_eq!(reply.status, 200, "{}", reply.body);
    let body = reply.json();
    assert_eq!(body["query"], serde_json::json!("deploy"));
    let results = body["results"].as_array().expect("a results array");
    assert!(!results.is_empty(), "the seeded rows must be found: {body}");
    for hit in results {
        for field in HIT_REQUIRED {
            assert!(
                hit.get(field).is_some(),
                "the hit is missing `{field}`, which both SDKs declare required: {hit}"
            );
        }
        assert!(
            hit["tags"].is_string(),
            "`tags` must be a JSON string on every hit: {hit}"
        );
        assert!(
            hit["source_url"].is_string(),
            "`source_url` is NOT NULL on the hosted wire: {hit}"
        );
        assert!(hit["rank"].is_number(), "`rank` is the fused score: {hit}");
        assert!(hit["snippet"].is_string(), "`snippet` is required: {hit}");
    }
    // And one row's tags decode to what was written, not to something else.
    let with_tags = results
        .iter()
        .find(|hit| hit["title"] == serde_json::json!("Tagged for the wire"))
        .expect("the tagged row must be in the page");
    let decoded: Vec<String> =
        serde_json::from_str(with_tags["tags"].as_str().expect("a JSON string of tags"))
            .expect("tags must decode to a list");
    assert_eq!(decoded, vec!["deploy".to_owned(), "staging".to_owned()]);
}

#[test]
fn a_workspace_carries_the_user_id_and_slug_the_sdks_require() {
    let sandbox = Sandbox::new("wire-workspace");
    seed(&sandbox);
    let server = Server::start(&sandbox);

    let reply = server.call("GET", "/api/workspaces", None);
    assert_eq!(reply.status, 200, "{}", reply.body);
    let body = reply.json();
    let workspaces = body["workspaces"].as_array().expect("a workspaces array");
    assert_eq!(workspaces.len(), 1, "{body}");
    let workspace = &workspaces[0];
    // `user_id` and `slug` are required in both SDKs' `Workspace`, and a local brain has one
    // owner: the constant is `"local"`, and the slug is the id (there is no separate naming
    // step here). A missing field is a `KeyError` in the Python client's `pick_workspace`.
    assert_eq!(
        workspace["user_id"],
        serde_json::json!("local"),
        "{workspace}"
    );
    assert!(workspace["slug"].is_string(), "{workspace}");
    assert_eq!(workspace["documents"], serde_json::json!(2), "{workspace}");
    assert_eq!(workspace["members"], serde_json::json!(1), "{workspace}");
}

#[test]
fn a_pin_written_as_the_integer_the_sdk_sends_reads_back_as_that_integer() {
    // `manage_memory(action="pin")` sends `PATCH {"pinned": 1}` — an integer, because the hosted
    // row's `pinned` is one. The route has to accept it, and the document it comes back on has
    // to report it in the same type, or a client that compares `doc["pinned"] == 1` fails.
    let sandbox = Sandbox::new("wire-pin");
    let (first, _) = seed(&sandbox);
    let server = Server::start(&sandbox);

    let reply = server.call(
        "PATCH",
        &format!("/api/documents/{first}"),
        Some(r#"{"pinned":1}"#),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["updated"], serde_json::json!(true));

    let after = server.call("GET", &format!("/api/documents/{first}"), None);
    assert_eq!(
        after.json()["pinned"],
        serde_json::json!(1),
        "{}",
        after.body
    );

    // And the refusal the SDK never sends, but a hand-written client might: 2 is not a pin.
    let refused = server.call(
        "PATCH",
        &format!("/api/documents/{first}"),
        Some(r#"{"pinned":2}"#),
    );
    assert_eq!(refused.status, 400, "{}", refused.body);
}

#[test]
fn a_draft_may_declare_the_entities_claims_and_domain_the_sdks_send() {
    // Both published SDKs document these three as the way to skip a paid extraction pass, and
    // both send them. The local request struct was `deny_unknown_fields` without them, so the
    // write path answered `400` for a body the hosted product accepts — found by the first
    // `draft_memory` call in `examples/python/quickstart.py`, not by the conformance harness,
    // which only made read calls until this test's fix widened it.
    //
    // Asserted both ways: the write is accepted *and* the envelope is stored. Silently dropping
    // the fields would be a quieter version of the same bug.
    let sandbox = Sandbox::new("wire-draft-metadata");
    let server = Server::start(&sandbox);

    let reply = server.call(
        "POST",
        "/api/memories/batch-draft",
        Some(
            r#"{"workspace_id":"default","learnings":[{
                "kind":"semantic",
                "title":"A reflection that names its evidence",
                "body":"The worker retries twice before dead-lettering.",
                "confidence":0.9,
                "entities":["worker"],
                "claims":["a failed job retries twice"],
                "domain":"infra"
            }]}"#,
        ),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    let body = reply.json();
    let memory_id = body["memories"][0]["id"]
        .as_str()
        .expect("the accepted draft must come back with its id")
        .to_owned();

    let document = server.call("GET", &format!("/api/documents/{memory_id}"), None);
    assert_eq!(document.status, 200, "{}", document.body);
    let document = document.json();
    assert_eq!(
        document["memory_metadata"]["entities"],
        serde_json::json!(["worker"]),
        "the entities a draft declared were not stored: {}",
        document["memory_metadata"]
    );
    assert_eq!(
        document["memory_metadata"]["domain"],
        serde_json::json!("infra")
    );
}

#[test]
fn the_hosted_link_retraction_is_idempotent_rather_than_a_conflict() {
    // `DELETE /api/documents/{id}/links/{targetId}` is in the hosted surface, so it is here:
    // the SDK's own escape hatch reaches it, and a `501` would be a hole for no gain. Two
    // differences from the local `unlink` verb, both asserted: it names no relation, and a
    // delete that matched nothing is `200 {deleted: false}` — the state the caller asked for —
    // rather than the `409` a POST that failed to move something earns.
    let sandbox = Sandbox::new("wire-unlink");
    let (first, second) = seed(&sandbox);
    let server = Server::start(&sandbox);

    let orphan = json(&[
        "add",
        "--db",
        &sandbox.db_arg(),
        "--json",
        "--kind",
        "semantic",
        "--title",
        "A memory with no edges",
        "--body",
        "Nothing points at this.",
    ]);
    let lonely = orphan["data"]["id"].as_str().expect("an id").to_owned();

    let linked = server.call(
        "POST",
        &format!("/api/documents/{first}/link"),
        Some(&format!(r#"{{"target_id":"{second}"}}"#)),
    );
    assert_eq!(linked.status, 200, "{}", linked.body);

    let removed = server.call(
        "DELETE",
        &format!("/api/documents/{first}/links/{second}"),
        None,
    );
    assert_eq!(removed.status, 200, "{}", removed.body);
    assert_eq!(removed.json()["deleted"], serde_json::json!(true));

    let again = server.call(
        "DELETE",
        &format!("/api/documents/{first}/links/{second}"),
        None,
    );
    assert_eq!(again.status, 200, "{}", again.body);
    assert_eq!(again.json()["deleted"], serde_json::json!(false));

    let unrelated = server.call(
        "DELETE",
        &format!("/api/documents/{first}/links/{lonely}"),
        None,
    );
    assert_eq!(unrelated.status, 200, "{}", unrelated.body);
    assert_eq!(unrelated.json()["deleted"], serde_json::json!(false));

    // The edge really is gone, not just reported gone.
    let links = server.call("GET", &format!("/api/documents/{first}/links"), None);
    let rows = links.json();
    let rows = rows.as_array().expect("an array of edges");
    assert!(
        rows.iter()
            .all(|row| row["target_id"] != serde_json::json!(second)),
        "the edge survived the delete: {rows:?}"
    );
}

#[test]
fn path_of_strips_the_scheme_so_a_test_can_pretend_to_be_a_client() {
    // Guards the helper the fixtures use: a `url` with the scheme left on it would make every
    // `TcpStream::connect` call in this file fail for a reason unrelated to the server.
    assert_eq!(
        path_of("http://127.0.0.1:1234/api/x"),
        "127.0.0.1:1234/api/x"
    );
}
