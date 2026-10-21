// SPDX-License-Identifier: Apache-2.0

//! `kiomon serve`: loopback HTTP/1.1 over the [verb layer](crate::service).
//!
//! `std::net` and threads, no async runtime, no framework. That is not minimalism for its
//! own sake: a `hyper` or `axum` stack is thirty-odd crates and an executor, and this
//! server answers about a dozen routes for one user on one machine, where the total cost of
//! a request is dominated by `open()` and the SQLite read behind it (see the WP9 record for
//! the numbers). What the surface has to be is *correct* — one request per connection,
//! bounded buffers, timeouts, and a refusal for everything it does not implement.
//!
//! # What this is not
//!
//! Not a public server, and the code enforces that rather than documenting it: the socket
//! is bound to `127.0.0.1`, a `Host` header that is not a loopback name is refused (DNS
//! rebinding — an attacker's domain pointed at `127.0.0.1` arrives with its own `Host`), and
//! a foreign `Origin` is refused (a page in a browser cannot read this API). There is no CORS
//! header anywhere, so even a same-origin browser fetch gets an opaque response it cannot
//! read.
//!
//! # Auth
//!
//! **There is none by default.** A local brain has no accounts, no organisations and no roles —
//! a workspace is the only thing that separates one body of memory from another, and that is a
//! read boundary rather than a permission. Asking a user to manage a key for a file they own was
//! the one piece of hosted ceremony that bought them nothing, so the default is: the port is on
//! `127.0.0.1`, the `Host` and `Origin` checks below keep browsers out, and that is it.
//!
//! The SDKs still demand *a* credential because they are written for the hosted product, so
//! callers pass the placeholder `local` as the API key. The server ignores its value: there is
//! nothing here to compare it against, and checking a constant would be a key by another name.
//!
//! `--require-token` restores the old behaviour for the two cases that want it — a shared
//! machine, or a port forwarded somewhere it should not be. It creates `serve.token` beside the
//! database (mode 0600 on Unix), prints it once, and then every request must carry it in
//! `X-API-Key` or `Authorization: Bearer`. Off by default because a check every local process
//! already passes is a chore, not a boundary.

use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kiomon_core::{ExitCode, Result, config};

use crate::out;
use crate::service::{
    self, BatchDraftRequest, Fault, GraphRequest, LinkRequest, PatchRequest, RateRequest,
    RecordOutcomeRequest, RetrieveRequest, SearchRequest, Service,
};

/// The only address this server binds.
///
/// A `--bind` flag was rejected: it would exist to be typed wrong once, and the whole threat
/// model of a token file in a data directory assumes the socket is not on a network the user
/// does not control.
const LOOPBACK: &str = "127.0.0.1";

/// Request headers may not exceed this. A header block is a handful of lines; the cap is what
/// stops a client from making the server allocate until it dies.
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// Request bodies may not exceed this. The largest legitimate body is a batch draft of 25
/// memories at the 1 MiB body cap, so this is that ceiling with room for the envelope.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// How many requests may be in flight.
///
/// Each one holds a thread and its own SQLite connection, and the work behind it is a
/// millisecond-scale read on a file this machine owns. Eight is enough for an agent, an SDK's
/// test suite and a shell, and small enough that a burst of connections cannot exhaust the
/// process.
const WORKERS: usize = 8;

/// Per-socket read and write budget.
///
/// Without it, one client that opens a connection and says nothing pins a worker forever —
/// which is a denial of service that needs no attacker to trigger.
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// The name of the token file, beside the database.
const TOKEN_FILE: &str = "serve.token";

/// What `kiomon serve` was asked to do.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// The port. Zero asks the operating system to choose, and the chosen one is printed.
    pub port: u16,
    /// Demand a token on every request. Off by default; see this module's note on auth.
    pub require_token: bool,
    /// Answer one request and exit.
    pub once: bool,
}

/// The credential a request must carry, when one is demanded.
struct Auth {
    /// The token, read from or written to `path`.
    token: String,
    /// Where it lives, so the greeting and the handshake can name it.
    path: PathBuf,
    /// Whether this run created it, which decides whether the value is printed.
    created: bool,
}

/// Run the server until something kills it.
///
/// # Errors
///
/// Returns [`kiomon_core::Error::Config`] when no database path can be resolved or the token
/// file cannot be read or created, and [`kiomon_core::Error::Io`] when the port cannot be
/// bound.
pub fn run(service: &Service, options: &ServeOptions, json: bool, quiet: bool) -> Result<ExitCode> {
    let db_path = config::resolve_db_path(service.db_path())?;
    let auth = if options.require_token {
        let path = db_path.with_file_name(TOKEN_FILE);
        let (token, created) = load_or_create_token(&path)?;
        Some(Auth {
            token,
            path,
            created,
        })
    } else {
        None
    };

    let token = auth.as_ref().map(|auth| auth.token.as_str());

    let listener = TcpListener::bind((LOOPBACK, options.port)).map_err(|err| {
        kiomon_core::Error::Io(std::io::Error::new(
            err.kind(),
            format!("could not listen on {LOOPBACK}:{}: {err}", options.port),
        ))
    })?;
    let address = listener.local_addr()?;
    let url = format!("http://{LOOPBACK}:{}", address.port());

    // The handshake goes to stdout because it is the answer to the command: with `--port 0`
    // nothing else knows which port was chosen, and a supervisor needs the line it can parse.
    // It is printed once, before the first request, and never repeated.
    // `auth` is in the handshake either way: a client that parses this line needs to know
    // whether to send a credential, and a field that appears only sometimes is a field a client
    // forgets to check.
    if json {
        let mut handshake = serde_json::json!({
            "listening": true,
            "url": url,
            "port": address.port(),
            "db": db_path.display().to_string(),
            "auth": if auth.is_some() { "token" } else { "none" },
        });
        if let Some(auth) = &auth {
            handshake["token_file"] = serde_json::json!(auth.path.display().to_string());
            handshake["token_created"] = serde_json::json!(auth.created);
        }
        println!("{handshake}");
    } else {
        println!("kiomon is listening on {url}");
        println!("  brain   {}", db_path.display());
        match &auth {
            None => println!("  auth    none - loopback only (`--require-token` to demand one)"),
            Some(auth) => println!("  token   {}", auth.path.display()),
        }
        if let Some(auth) = auth.as_ref().filter(|auth| auth.created) {
            println!();
            println!("A new token was written there. Send it as `X-API-Key` on every request:");
            println!();
            println!("  {}", auth.token);
        }
    }
    let _ = std::io::stdout().flush();

    if options.once {
        // The one-request mode exists so a smoke test can start this server, make one call
        // and collect an exit code without managing a process — which is how the release
        // workflow proves the shipped binary can bind, answer and exit.
        let (stream, _) = listener.accept()?;
        handle(stream, service, token, quiet);
        return Ok(ExitCode::Success);
    }

    serve_forever(&listener, service, token, quiet)
}

/// Accept connections forever, handing each to one of [`WORKERS`] threads.
///
/// # Errors
///
/// Returns [`kiomon_core::Error::Io`] when accepting fails for a reason that is not a signal.
fn serve_forever(
    listener: &TcpListener,
    service: &Service,
    token: Option<&str>,
    quiet: bool,
) -> Result<ExitCode> {
    // The receiver is behind a mutex because `mpsc::Receiver` is not `Sync`: a worker holds
    // the lock only for the length of a `recv()`, never while it handles a request, so the
    // lock orders which worker gets which connection and nothing else.
    let (sender, receiver) = mpsc::channel::<TcpStream>();
    let receiver = Arc::new(Mutex::new(receiver));
    let live = Arc::new(AtomicUsize::new(0));

    for index in 0..WORKERS {
        let receiver = Arc::clone(&receiver);
        let token = token.map(str::to_owned);
        let live = Arc::clone(&live);
        // The service is cloned into each worker rather than shared behind a lock: it holds
        // two paths, and every request opens its own connection inside `Service`.
        let service = service.clone();
        std::thread::Builder::new()
            .name(format!("kiomon-http-{index}"))
            .spawn(move || {
                loop {
                    let stream = match receiver.lock() {
                        Ok(receiver) => receiver.recv(),
                        Err(_) => return,
                    };
                    let Ok(stream) = stream else {
                        // The sender is gone: the listener loop ended and this worker is done.
                        return;
                    };
                    live.fetch_add(1, Ordering::Relaxed);
                    handle(stream, &service, token.as_deref(), quiet);
                    live.fetch_sub(1, Ordering::Relaxed);
                }
            })?;
    }

    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                // A worker that died takes its connection with it, and `send` then fails.
                // That is a worker panic, which is a bug — but it must not take the whole
                // server down silently, so the acceptance loop keeps going and the panic
                // message has already printed itself on stderr.
                if sender.send(stream).is_err() {
                    out::warn(
                        quiet,
                        "a worker thread stopped; the server is running short",
                    );
                }
            }
            // A signal interrupts `accept`; the loop is supposed to carry on, and there is
            // nothing else to do in that arm.
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err.into()),
        }
    }
}

/// One request, one response, then close.
///
/// Errors are handled inside and never returned: a connection that fails to parse gets a
/// response if it can still be written to, and a closed socket gets nothing. There is
/// nothing above this to report to — the process serves the next connection either way.
fn handle(stream: TcpStream, service: &Service, token: Option<&str>, quiet: bool) {
    let started = Instant::now();
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let peer = stream.peer_addr().ok();

    let mut head_only = false;
    let (reply, summary) = match read_request(&stream) {
        Ok(request) => {
            head_only = request.method == "HEAD";
            let summary = format!("{} {}", request.method, request.path);
            (route(service, &request, token), summary)
        }
        Err(fault) => (
            Fault::new(fault.status(), fault.code(), fault.message().to_owned()).into(),
            "-".to_owned(),
        ),
    };

    let status = reply.fault_status();
    if let Some(peer) = peer
        && !quiet
    {
        out::warn(
            false,
            &format!(
                "serve: {summary} -> {status} in {:.1} ms ({peer})",
                started.elapsed().as_secs_f64() * 1000.0
            ),
        );
    }
    write_response(stream, status, &reply.body(), head_only);
}

/// Auth, then routing, then the handler.
fn route(service: &Service, request: &Request, token: Option<&str>) -> Response {
    // The refusals come before routing so that a refused client cannot learn which routes exist
    // by comparing `401` with `404`. `Host` and `Origin` are checked even when no token is
    // demanded: they are what keeps a web page and a DNS-rebinding attempt out, and neither of
    // those is a local process the user trusts. The token, when one is demanded, is checked
    // last because it is the cheapest.
    if !request.host_is_loopback() {
        return Fault::new(
            403,
            "forbidden",
            "this server answers only requests addressed to a loopback name. A request whose \
             Host header names another machine is a DNS-rebinding attempt or a proxy in the \
             middle, and neither is served.",
        )
        .into();
    }
    if !request.origin_is_loopback() {
        return Fault::new(
            403,
            "forbidden",
            "a browser origin other than this machine's is refused: this API is for agents on \
             the same machine, and a page must not be able to read the brain.",
        )
        .into();
    }
    if let Some(token) = token
        && !request.has_token(token)
    {
        return Fault::new(
            401,
            "unauthorized",
            "this server was started with `--require-token`. Send the token from the file \
                 beside the database as `X-API-Key` (or `Authorization: Bearer <token>`); \
                 `kiomon serve` printed its path at startup.",
        )
        .into();
    }

    // The pattern and the method are both part of a match. Filtering on the pattern and then
    // checking the method would find `GET /api/documents/:id` and refuse a `PATCH` to the same
    // path that another row answers — which is the bug this shape was written to fix: three
    // verbs share one path here, so the method has to be part of the search.
    let mut pattern_matched = false;
    for route in ROUTES.iter().filter(|route| route.matches(request)) {
        pattern_matched = true;
        if route.method.allows(&request.method) {
            return (route.handler)(service, request);
        }
    }
    if pattern_matched {
        // A wrong method on a known path is `405`, which is the difference between "you typed
        // the wrong URL" and "you used the wrong verb".
        return Fault::new(
            405,
            "bad_request",
            format!("{} does not accept {}", request.path, request.method),
        )
        .into();
    }
    Fault::new(
        404,
        "not_found",
        format!(
            "no route for {} {}. `docs/CONTRACT.md` lists the routes this engine answers.",
            request.method, request.path
        ),
    )
    .into()
}

// ── the route table ─────────────────────────────────────────────────────────

/// One route: a method, a path pattern, and what answers it.
struct Route {
    /// The method, or [`Method::Any`] for the ones that share a handler.
    method: Method,
    /// Path segments, where a segment starting with `:` is a parameter.
    pattern: &'static [&'static str],
    handler: fn(&Service, &Request) -> Response,
}

impl Route {
    /// Whether this route's pattern matches the request's path segments.
    fn matches(&self, request: &Request) -> bool {
        if self.pattern.len() != request.segments.len() {
            return false;
        }
        self.pattern
            .iter()
            .zip(&request.segments)
            .all(|(pattern, segment)| pattern.starts_with(':') || pattern == segment)
    }
}

/// The method a route is registered for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    Get,
    Post,
    Patch,
    Delete,
    /// Any method: used for the routes whose only answer is `501`, so that a client learns
    /// the route exists and is not served by *this* engine rather than that it typed the
    /// wrong verb.
    Any,
}

impl Method {
    fn allows(self, method: &str) -> bool {
        // `HEAD` is answered by every `GET` route, because RFC 9110 says a GET route's headers
        // are HEAD's answer and the body is simply omitted. A `405` would be legal and useless: a
        // client checking whether the server is up gets an error meaning "wrong verb" when the
        // verb was fine.
        matches!(self, Self::Any)
            || matches!(
                (self, method),
                (Self::Get, "GET" | "HEAD")
                    | (Self::Post, "POST")
                    | (Self::Patch, "PATCH")
                    | (Self::Delete, "DELETE")
            )
    }
}

/// Every route this engine answers or refuses by name.
///
/// A route whose answer is `unsupported_locally` lives here rather than falling off the end
/// into a `404`, which is the whole point of Appendix C: a caller must be able to tell "this
/// engine cannot do that" from "no such route", because the first means stop asking and the
/// second means check the URL.
static ROUTES: &[Route] = &[
    Route {
        method: Method::Get,
        pattern: &["api", "search"],
        handler: search,
    },
    Route {
        method: Method::Post,
        pattern: &["api", "retrieve"],
        handler: retrieve,
    },
    Route {
        method: Method::Get,
        pattern: &["api", "documents", ":id"],
        handler: get_document,
    },
    Route {
        method: Method::Patch,
        pattern: &["api", "documents", ":id"],
        handler: patch_document,
    },
    Route {
        method: Method::Delete,
        pattern: &["api", "documents", ":id"],
        handler: delete_document,
    },
    Route {
        method: Method::Get,
        pattern: &["api", "documents", ":id", "links"],
        handler: document_links,
    },
    Route {
        method: Method::Post,
        pattern: &["api", "documents", ":id", "link"],
        handler: link_document,
    },
    Route {
        method: Method::Post,
        pattern: &["api", "documents", ":id", "unlink"],
        handler: unlink_document,
    },
    Route {
        method: Method::Delete,
        pattern: &["api", "documents", ":id", "links", ":target"],
        handler: unlink_edge,
    },
    Route {
        method: Method::Post,
        pattern: &["api", "documents", ":id", "rate"],
        handler: rate_document,
    },
    Route {
        method: Method::Post,
        pattern: &["api", "documents", ":id", ":action"],
        handler: manage_document,
    },
    Route {
        method: Method::Get,
        pattern: &["api", "graph"],
        handler: graph,
    },
    Route {
        method: Method::Get,
        pattern: &["api", "memories"],
        handler: memories,
    },
    Route {
        method: Method::Post,
        pattern: &["api", "memories", "batch-draft"],
        handler: batch_draft,
    },
    Route {
        method: Method::Post,
        pattern: &["api", "memories", "record-outcome"],
        handler: record_outcome,
    },
    Route {
        method: Method::Any,
        pattern: &["api", "memories", ":id", "revert"],
        handler: revert_unsupported,
    },
    Route {
        method: Method::Get,
        pattern: &["api", "workspaces"],
        handler: workspaces,
    },
    Route {
        method: Method::Get,
        pattern: &["api", "workspaces", ":id", "context"],
        handler: workspace_context,
    },
    Route {
        method: Method::Any,
        pattern: &["api", "workspaces", ":id", "briefings", "latest"],
        handler: briefing_unsupported,
    },
    Route {
        method: Method::Get,
        pattern: &["health"],
        handler: health,
    },
];

fn search(service: &Service, request: &Request) -> Response {
    let body = SearchRequest {
        q: request.query("q").unwrap_or_default().to_owned(),
        limit: request.query("limit").and_then(|raw| raw.parse().ok()),
        workspace_id: request.query("workspace_id").map(str::to_owned),
        kind: request
            .query("kind")
            .map(str::to_owned)
            .or_else(|| request.query("kinds").map(str::to_owned)),
        legs: request.query("legs").map(str::to_owned),
    };
    service.search(&body).into()
}

fn retrieve(service: &Service, request: &Request) -> Response {
    let body: RetrieveRequest = match request.json() {
        Ok(body) => body,
        Err(fault) => return fault.into(),
    };
    service.retrieve(&body).into()
}

fn get_document(service: &Service, request: &Request) -> Response {
    service
        .get_memory(request.param("id").unwrap_or_default())
        .into()
}

fn patch_document(service: &Service, request: &Request) -> Response {
    let body: PatchRequest = match request.json() {
        Ok(body) => body,
        Err(fault) => return fault.into(),
    };
    service
        .patch(request.param("id").unwrap_or_default(), &body)
        .into()
}

fn delete_document(service: &Service, request: &Request) -> Response {
    service
        .manage(request.param("id").unwrap_or_default(), "forget")
        .map(|_| serde_json::json!({ "deleted": true }))
        .into()
}

fn document_links(service: &Service, request: &Request) -> Response {
    service
        .links(request.param("id").unwrap_or_default())
        .into()
}

fn link_document(service: &Service, request: &Request) -> Response {
    let body: LinkRequest = match request.json() {
        Ok(body) => body,
        Err(fault) => return fault.into(),
    };
    service
        .link(request.param("id").unwrap_or_default(), &body)
        .into()
}

fn unlink_document(service: &Service, request: &Request) -> Response {
    let body: LinkRequest = match request.json() {
        Ok(body) => body,
        Err(fault) => return fault.into(),
    };
    service
        .unlink(request.param("id").unwrap_or_default(), &body)
        .into()
}

fn unlink_edge(service: &Service, request: &Request) -> Response {
    service
        .unlink_edge(
            request.param("id").unwrap_or_default(),
            request.param("target").unwrap_or_default(),
            request.query("relation").filter(|name| !name.is_empty()),
        )
        .into()
}

fn rate_document(service: &Service, request: &Request) -> Response {
    let body: RateRequest = match request.json() {
        Ok(body) => body,
        Err(fault) => return fault.into(),
    };
    service
        .rate(request.param("id").unwrap_or_default(), &body)
        .into()
}

fn manage_document(service: &Service, request: &Request) -> Response {
    service
        .manage(
            request.param("id").unwrap_or_default(),
            request.param("action").unwrap_or_default(),
        )
        .into()
}

fn graph(service: &Service, request: &Request) -> Response {
    let body = GraphRequest {
        // The hosted query name is `center_id`; `id` is accepted because Appendix C of the
        // plan spelled it that way and an early reader of the contract will type it.
        center_id: request
            .query("center_id")
            .or_else(|| request.query("id"))
            .map(str::to_owned),
        relation: request.query("relation").map(str::to_owned),
        depth: request.query("depth").and_then(|raw| raw.parse().ok()),
    };
    service.graph(&body).into()
}

fn memories(service: &Service, request: &Request) -> Response {
    service
        .memories(request.query("workspace_id"), request.query("status"))
        .into()
}

fn batch_draft(service: &Service, request: &Request) -> Response {
    let body: BatchDraftRequest = match request.json() {
        Ok(body) => body,
        Err(fault) => return fault.into(),
    };
    service.batch_draft(&body).into()
}

fn record_outcome(service: &Service, request: &Request) -> Response {
    let body: RecordOutcomeRequest = match request.json() {
        Ok(body) => body,
        Err(fault) => return fault.into(),
    };
    service.record_outcome(&body).into()
}

fn workspaces(service: &Service, _request: &Request) -> Response {
    service.workspaces().into()
}

fn workspace_context(service: &Service, request: &Request) -> Response {
    service
        .context(
            request.param("id").unwrap_or_default(),
            request.query("topic"),
        )
        .into()
}

fn revert_unsupported(_service: &Service, request: &Request) -> Response {
    Fault::unsupported(
        &format!("POST {}", request.path),
        "this engine keeps no revision history, so there is nothing to revert to. The hosted \
         product records a decision per write; here a memory's `updated_at` is the only record \
         that it changed.",
    )
    .into()
}

fn briefing_unsupported(_service: &Service, request: &Request) -> Response {
    Fault::unsupported(
        &format!("GET {}", request.path),
        "a briefing is composed by a model over a window of memory, and this engine has no \
         model on any read path. `GET /api/workspaces/{id}/context` answers the same question \
         with the rows themselves.",
    )
    .into()
}

fn health(_service: &Service, _request: &Request) -> Response {
    Response::ok(serde_json::json!({
        "ok": true,
        "version": crate::VERSION,
        "routes": ROUTES.len(),
    }))
}

// ── responses ───────────────────────────────────────────────────────────────

/// A response on its way out: a status and a JSON body.
///
/// The status is the whole of the difference between an answer and a refusal — both carry a
/// JSON body in the hosted shape, and a client branches on the number, not on which Rust type
/// produced it.
struct Response {
    status: u16,
    body: serde_json::Value,
}

impl Response {
    fn ok(body: serde_json::Value) -> Self {
        Self { status: 200, body }
    }

    fn body(&self) -> serde_json::Value {
        self.body.clone()
    }

    fn fault_status(&self) -> u16 {
        self.status
    }
}

impl From<service::Reply> for Response {
    fn from(reply: service::Reply) -> Self {
        match reply {
            Ok(body) => Self::ok(body),
            Err(fault) => Self {
                status: fault.status(),
                body: fault.body(),
            },
        }
    }
}

impl From<Fault> for Response {
    fn from(fault: Fault) -> Self {
        Self {
            status: fault.status(),
            body: fault.body(),
        }
    }
}

/// The reason phrase for a status this server can produce.
///
/// Spelled out rather than looked up in a table crate: there are fewer than ten of them, and
/// a wrong reason phrase next to a right code is a lie in the only place a human reads.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

/// Write one response and close the connection.
///
/// `head_only` omits the body but keeps its `Content-Length`, which is what the protocol says: a
/// `HEAD` response describes the body a `GET` would have produced.
fn write_response(mut stream: TcpStream, status: u16, body: &serde_json::Value, head_only: bool) {
    let payload = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    let head = format!(
        "HTTP/1.1 {status} {}\r\n\
         Content-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         Cache-Control: no-store\r\n\
         \r\n",
        reason(status),
        payload.len()
    );
    // The head and the body are two writes because the head is text and the body is bytes;
    // `write_all` on each, then a shutdown, so a client reading to EOF sees the whole thing.
    let _ = stream.write_all(head.as_bytes());
    if !head_only {
        let _ = stream.write_all(&payload);
    }
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Write);
}

// ── requests ────────────────────────────────────────────────────────────────

/// A parsed request: what arrived, not what it means.
#[derive(Debug, Clone)]
struct Request {
    method: String,
    path: String,
    segments: Vec<String>,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    /// A query parameter, percent-decoded.
    fn query(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// A path parameter, by pattern name.
    fn param(&self, name: &str) -> Option<&str> {
        let route = ROUTES.iter().find(|route| route.matches(self))?;
        let wanted = format!(":{name}");
        let index = route
            .pattern
            .iter()
            .position(|segment| *segment == wanted)?;
        self.segments.get(index).map(String::as_str)
    }

    /// A header value, case-insensitively.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Whether the request named a loopback `Host`.
    ///
    /// This is the DNS-rebinding guard: a page that resolves `evil.example` to `127.0.0.1`
    /// reaches this socket with `Host: evil.example:PORT`, which is the one header the
    /// browser cannot forge.
    fn host_is_loopback(&self) -> bool {
        let Some(host) = self.header("host") else {
            // HTTP/1.1 requires the header. A missing one is not a browser, and refusing it
            // would break hand-written clients (curl always sends it; `nc` does not).
            return true;
        };
        let name = host.split(':').next().unwrap_or(host);
        matches!(name, "127.0.0.1" | "localhost" | "[::1]" | "::1")
    }

    /// Whether any `Origin` header present is this machine.
    ///
    /// Absent means "not a browser" and is allowed: an SDK, curl and an agent do not send
    /// `Origin`, and requiring it would make the server unusable by the clients it exists
    /// for. A present one has to be loopback, or a page on the internet could read the brain.
    fn origin_is_loopback(&self) -> bool {
        let Some(origin) = self.header("origin") else {
            return true;
        };
        let rest = origin.split_once("://").map_or(origin, |pair| pair.1);
        let authority = rest.split('/').next().unwrap_or(rest);
        let name = authority.rsplit_once(':').map_or(authority, |pair| pair.0);
        matches!(name, "127.0.0.1" | "localhost" | "[::1]" | "::1")
    }

    /// Whether the request carries the server's token.
    fn has_token(&self, token: &str) -> bool {
        if self.header("x-api-key").is_some_and(|value| value == token) {
            return true;
        }
        self.header("authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|value| value.trim() == token)
    }

    /// The body as JSON.
    fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, Fault> {
        if self.body.is_empty() {
            return Err(Fault::new(
                400,
                "validation_failed",
                "the request has no body; this route expects a JSON object.",
            ));
        }
        serde_json::from_slice(&self.body).map_err(|err| {
            Fault::new(
                400,
                "validation_failed",
                format!("the request body is not the expected JSON: {err}"),
            )
        })
    }
}

/// The request line and headers, as bytes, up to and including the blank line.
///
/// Bounded by [`MAX_HEADER_BYTES`], because "read until a blank line" with no cap is an
/// invitation to allocate until the process dies.
fn read_head(reader: &mut BufReader<&TcpStream>) -> Result<Vec<u8>, Fault> {
    let mut head = Vec::new();
    loop {
        let mut line = Vec::new();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|err| io_fault(&err))?;
        if read == 0 {
            return Err(Fault::new(
                400,
                "bad_request",
                "the connection closed before a complete request arrived",
            ));
        }
        head.extend_from_slice(&line);
        if head.len() > MAX_HEADER_BYTES {
            return Err(Fault::new(
                431,
                "bad_request",
                format!("the request headers exceed {MAX_HEADER_BYTES} bytes"),
            ));
        }
        if line == b"\r\n" || line == b"\n" {
            return Ok(head);
        }
    }
}

/// What the request line and headers said, before the body is read.
struct Head {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    content_length: usize,
    wants_continue: bool,
    chunked: bool,
}

/// Split the head into a method, a target and headers.
fn parse_head(head: &[u8]) -> Result<Head, Fault> {
    let text = String::from_utf8(head.to_vec()).map_err(|_| {
        Fault::new(
            400,
            "bad_request",
            "the request headers are not valid UTF-8",
        )
    })?;
    let mut lines = text.split('\n').map(|line| line.trim_end_matches('\r'));
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or_default().trim().to_owned();
    let target = parts.next().unwrap_or_default().trim().to_owned();
    if method.is_empty() || target.is_empty() || !target.starts_with('/') {
        return Err(Fault::new(
            400,
            "bad_request",
            "the request line is not `METHOD /path HTTP/1.1`",
        ));
    }

    let mut headers = Vec::new();
    let mut content_length = 0_usize;
    let mut wants_continue = false;
    let mut chunked = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(Fault::new(
                400,
                "bad_request",
                format!("the header line `{line}` has no colon"),
            ));
        };
        let name = name.trim().to_owned();
        let value = value.trim().to_owned();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().map_err(|_| {
                Fault::new(
                    400,
                    "bad_request",
                    format!("`Content-Length: {value}` is not a number"),
                )
            })?;
        }
        if name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        }
        if name.eq_ignore_ascii_case("expect")
            && value.to_ascii_lowercase().contains("100-continue")
        {
            wants_continue = true;
        }
        headers.push((name, value));
    }
    Ok(Head {
        method,
        target,
        headers,
        content_length,
        wants_continue,
        chunked,
    })
}

/// Read one request: the request line, the headers, then exactly `Content-Length` bytes.
///
/// The connection is closed after one request, so there is no keep-alive state to track and no
/// pipelining to get wrong — which is what makes a hand-written parser safe here.
fn read_request(stream: &TcpStream) -> Result<Request, Fault> {
    let mut reader = BufReader::new(stream);
    let head = parse_head(&read_head(&mut reader)?)?;

    if head.chunked {
        // Refused by name rather than mis-parsed: every client that speaks to this server
        // (curl, requests, fetch, node) sends `Content-Length` for a JSON body, and a chunked
        // reader is a state machine that would be untested code on the only path that uses it.
        return Err(Fault::new(
            411,
            "bad_request",
            "chunked request bodies are not supported; send a `Content-Length`.",
        ));
    }
    if head.content_length > MAX_BODY_BYTES {
        return Err(Fault::new(
            413,
            "payload_too_large",
            format!("the request body exceeds {MAX_BODY_BYTES} bytes"),
        ));
    }
    if head.wants_continue {
        // curl sends `Expect: 100-continue` for a body over a kilobyte and waits a second for
        // this line before sending it. Answering is two lines; not answering makes every large
        // POST a second slower with no error to show for it.
        let mut stream = stream;
        let _ = stream.write_all(
            b"HTTP/1.1 100 Continue

",
        );
        let _ = stream.flush();
    }

    let mut body = vec![0_u8; head.content_length];
    if head.content_length > 0 {
        reader.read_exact(&mut body).map_err(|err| io_fault(&err))?;
    }

    let (path, raw_query) = head.target.split_once('?').unwrap_or((&head.target, ""));
    Ok(Request {
        method: head.method,
        path: decode(path),
        segments: path
            .trim_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(decode)
            .collect(),
        query: parse_query(raw_query),
        headers: head.headers,
        body,
    })
}

/// A socket error, as a refusal rather than a panic.
fn io_fault(err: &std::io::Error) -> Fault {
    let status = match err.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => 408,
        _ => 400,
    };
    Fault::new(
        status,
        "bad_request",
        format!("the request could not be read: {err}"),
    )
}

/// Split a query string into decoded pairs.
///
/// `+` is a space because form encoding says so and because `curl --data-urlencode` and a
/// hand-typed URL both produce it; `encodeURIComponent` never emits it, so accepting it costs
/// nothing.
fn parse_query(raw: &str) -> Vec<(String, String)> {
    raw.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (
                decode(&key.replace('+', " ")),
                decode(&value.replace('+', " ")),
            )
        })
        .collect()
}

/// Percent-decode a path segment or query value.
///
/// An escape that is not two hex digits is kept literally rather than refused: a path is
/// matched against a fixed table, so a bad escape cannot become an injection — it just fails
/// to match, and the `404` names the path the client sent.
fn decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ── the token file ──────────────────────────────────────────────────────────

/// Read the token file, creating it when it is not there.
///
/// Returns the token and whether it was created, because the caller prints a new token once —
/// a token that has to be found in a file before the server has ever been used is a token
/// nobody finds.
///
/// # Errors
///
/// Returns [`kiomon_core::Error::Io`] when the file cannot be read or written, and
/// [`kiomon_core::Error::Config`] when it exists but is empty.
fn load_or_create_token(path: &Path) -> Result<(String, bool)> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let token = text.trim().to_owned();
            if token.is_empty() {
                return Err(kiomon_core::Error::Config(format!(
                    "{} is empty. Delete it and `kiomon serve` will write a new token.",
                    path.display()
                )));
            }
            Ok((token, false))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let token = new_token()?;
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            write_private(path, &token)?;
            Ok((token, true))
        }
        Err(err) => Err(kiomon_core::Error::Io(std::io::Error::new(
            err.kind(),
            format!("could not read the token file {}: {err}", path.display()),
        ))),
    }
}

/// Write a file only this user can read.
///
/// On Unix the mode is set on the open file, so there is no window in which the token exists
/// world-readable; on Windows the file inherits the data directory's ACL, which is the user's
/// profile — the same protection the database itself has.
fn write_private(path: &Path, contents: &str) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

/// Sixty-four hex characters from the operating system's random source.
///
/// `getrandom` rather than a hash of the clock and the pid: a token survives restarts, so it
/// is a long-lived credential, and the one property it must have is that it cannot be guessed
/// from anything an attacker can observe.
fn new_token() -> Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|err| kiomon_core::Error::Internal(format!("no random source: {err}")))?;
    // Built as bytes rather than with `format!("{byte:02x}")` per byte: that spelling
    // allocates a `String` per byte and appends it, which is sixty-four allocations for a
    // value that is one.
    let mut token = String::with_capacity(64);
    for byte in bytes {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        token.push(char::from(HEX[usize::from(byte >> 4)]));
        token.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, target: &str, headers: &[(&str, &str)]) -> Request {
        let (path, raw_query) = target.split_once('?').unwrap_or((target, ""));
        Request {
            method: method.to_owned(),
            path: decode(path),
            segments: path
                .trim_matches('/')
                .split('/')
                .filter(|segment| !segment.is_empty())
                .map(decode)
                .collect(),
            query: parse_query(raw_query),
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            body: Vec::new(),
        }
    }

    #[test]
    fn a_loopback_host_is_the_only_host_served() {
        // The rebinding guard: the header is the one thing a page cannot forge.
        assert!(request("GET", "/health", &[("Host", "127.0.0.1:7777")]).host_is_loopback());
        assert!(request("GET", "/health", &[("Host", "localhost:7777")]).host_is_loopback());
        // No header at all is a hand-written client, not a browser.
        assert!(request("GET", "/health", &[]).host_is_loopback());
        assert!(!request("GET", "/health", &[("Host", "evil.example:7777")]).host_is_loopback());
    }

    #[test]
    fn a_foreign_origin_is_refused_and_an_absent_one_is_not() {
        assert!(request("GET", "/health", &[]).origin_is_loopback());
        assert!(
            request("GET", "/health", &[("Origin", "http://localhost:3000")]).origin_is_loopback()
        );
        assert!(
            !request("GET", "/health", &[("Origin", "https://evil.example")]).origin_is_loopback()
        );
        // The 127.0.0.1 spelling is what a local page actually sends.
        assert!(
            request("GET", "/health", &[("Origin", "http://127.0.0.1:8080")]).origin_is_loopback()
        );
    }

    #[test]
    fn the_token_is_accepted_in_either_header_and_nothing_else_is() {
        let token = "abc123";
        assert!(request("GET", "/", &[("X-API-Key", token)]).has_token(token));
        assert!(request("GET", "/", &[("Authorization", "Bearer abc123")]).has_token(token));
        assert!(!request("GET", "/", &[("X-API-Key", "nope")]).has_token(token));
        assert!(!request("GET", "/", &[("Authorization", "Bearer nope")]).has_token(token));
        assert!(!request("GET", "/", &[]).has_token(token));
    }

    #[test]
    fn a_percent_encoded_path_segment_decodes_to_what_it_names() {
        let decoded = request("GET", "/api/documents/k_01%20AB", &[]);
        assert_eq!(decoded.segments, ["api", "documents", "k_01 AB"]);
        assert_eq!(decoded.param("id"), Some("k_01 AB"));
    }

    #[test]
    fn a_query_string_survives_plus_spaces_and_percent_escapes() {
        let parsed = request("GET", "/api/search?q=deploy+the&limit=5", &[]);
        assert_eq!(parsed.query("q"), Some("deploy the"));
        assert_eq!(parsed.query("limit"), Some("5"));
        assert_eq!(parsed.query("missing"), None);
    }

    #[test]
    fn every_route_in_appendix_c_resolves_to_an_answer() {
        // The acceptance criterion is route by route, so the list is asserted route by route:
        // a route that falls out of the table becomes a `404` that reads like an empty
        // library, and this test is what notices.
        let cases = [
            ("GET", "/api/search?q=x"),
            ("POST", "/api/retrieve"),
            ("GET", "/api/documents/k_01"),
            ("PATCH", "/api/documents/k_01"),
            ("DELETE", "/api/documents/k_01"),
            ("POST", "/api/memories/batch-draft"),
            ("POST", "/api/memories/record-outcome"),
            ("GET", "/api/graph?center_id=k_01"),
            ("POST", "/api/documents/k_01/rate"),
            ("POST", "/api/documents/k_01/archive"),
            ("POST", "/api/documents/k_01/approve"),
            ("POST", "/api/documents/k_01/reject"),
            ("GET", "/api/documents/k_01/links"),
            ("POST", "/api/documents/k_01/link"),
            ("DELETE", "/api/documents/k_01/links/k_02"),
            ("POST", "/api/documents/k_01/unlink"),
            ("GET", "/api/workspaces"),
            ("GET", "/api/workspaces/w/context?topic=x"),
            ("POST", "/api/memories/k_01/revert"),
            ("GET", "/api/workspaces/w/briefings/latest"),
        ];
        for (method, target) in cases {
            let parsed = request(method, target, &[]);
            assert!(
                ROUTES.iter().any(|route| route.matches(&parsed)),
                "{method} {target} is not routed"
            );
        }
    }

    #[test]
    fn a_known_path_with_the_wrong_method_is_not_a_404() {
        let parsed = request("PUT", "/api/retrieve", &[]);
        let route = ROUTES
            .iter()
            .find(|route| route.matches(&parsed))
            .expect("the path is routed");
        assert!(!route.method.allows(&parsed.method));
    }

    #[test]
    fn a_token_is_sixty_four_hex_characters_and_two_differ() {
        let first = new_token().expect("the operating system has a random source");
        let second = new_token().expect("the operating system has a random source");
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }
}
