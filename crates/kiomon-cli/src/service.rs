// SPDX-License-Identifier: Apache-2.0

//! The verb layer: one set of operations, two transports.
//!
//! `serve` (HTTP) and `mcp` (JSON-RPC over stdio) are the same brain answering the same
//! questions through different pipes, so the questions are written down once — here — and
//! each transport's job is framing. Anything that decides *what happens* stays in
//! `kiomon-core`; anything that decides *how it looks on a wire* is here.
//!
//! # Why the field names are the hosted product's
//!
//! A published SDK parses these bodies. `kiomon-py` and `kiomon-ts` speak
//! `https://api.kiomon.com`, and `baseUrl` is what points them at a local brain — so the
//! local answer to `GET /api/graph` has to be the hosted answer, `center_id` and all. The
//! CLI is free to spell things its own way (`relate`, `--workspace '*'`); this module is
//! not, because a caller that works against the hosted product must not have to learn a
//! second dialect to work offline. Where this engine has more to say than the hosted one,
//! the extra fields are additive and named — never a renamed hosted field.
//!
//! # Why a request opens its own connection
//!
//! The CLI is a process per command, and `serve` is a thread per request over a
//! [`Database`] that is not `Sync`: one connection shared between threads would need a
//! mutex, and a mutex around the read path serialises the one thing worth parallelising. So
//! each request opens the file exactly as a fresh `kiomon` process does — pragmas and a
//! version check — against a connection nothing else can contend for. There is no shared
//! mutable state in this type at all, which is what makes the accept loop trivially safe.

use std::path::{Path, PathBuf};

use kiomon_core::lifecycle::{PendingQuery, Rating, Skipped, TaskOutcome};
use kiomon_core::{
    Assessment, Database, Error, ForgetOutcome, IdsOutcome, Intent, Legs, ListQuery, ListSort,
    Memory, MemoryKind, MemoryPatch, MemorySummaryJson, NewLink, NewMemory, Relation,
    RetrieveQuery, ReviewOutcome, RiskLevel, Scope, SearchQuery, SearchSort, Status, UpdateOutcome,
};
use serde::Deserialize;

use crate::commands::{open_paths, read_scope, write_workspace};

/// The error code every route this engine cannot answer returns.
///
/// Spelled once because it is in `docs/CONTRACT.md`, and because the alternative — a `501`
/// whose body said something subtly different per route — would make it impossible for a
/// client to branch on "this engine cannot do that" against "that failed".
pub const UNSUPPORTED_LOCALLY: &str = "unsupported_locally";

/// A refusal, in the three pieces both transports need.
///
/// HTTP needs a status, a code and a message; MCP needs a code and a message and has no
/// status at all. The status is carried anyway so `POST /api/retrieve` can answer `400` for
/// a bad intent and `503` for a dead embedder instead of flattening both into "no" — the
/// hosted routes make that distinction and a client is entitled to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    status: u16,
    code: &'static str,
    message: String,
}

impl Fault {
    /// A refusal with an explicit status.
    #[must_use]
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    /// A `501` for a route this engine deliberately does not answer.
    ///
    /// Never the same thing as a `404`: "there is no such route" and "this engine cannot
    /// answer that route" send a caller in opposite directions, and the hosted contract
    /// already distinguishes absent from not-implemented.
    #[must_use]
    pub fn unsupported(route: &str, reason: &str) -> Self {
        Self::new(
            501,
            UNSUPPORTED_LOCALLY,
            format!("{route} is not available locally: {reason}"),
        )
    }

    /// The HTTP status.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// The stable machine-readable code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    /// The human-readable explanation.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The error body, in the shape every hosted route uses: `error` for prose, `code` for
    /// a value to branch on.
    #[must_use]
    pub fn body(&self) -> serde_json::Value {
        serde_json::json!({ "error": self.message, "code": self.code })
    }
}

impl From<Error> for Fault {
    fn from(err: Error) -> Self {
        // "Not in this release" is `501` and nothing else. The CLI maps it to exit code 1 —
        // correctly, because a process has no vocabulary for "not implemented" beyond failure —
        // but over HTTP the hosted contract has a status for exactly this, and a `500` would tell
        // an SDK to retry a request that will never succeed.
        if matches!(err, Error::Deferred { .. } | Error::Unimplemented { .. }) {
            return Self::new(501, UNSUPPORTED_LOCALLY, err.to_string());
        }
        // The exit code is the CLI's contract and already the decided mapping, so the
        // status is derived from it rather than re-decided here: a second `match` over the
        // same variants is a second place for them to disagree.
        let status = match err.exit_code() {
            kiomon_core::ExitCode::NotFound => 404,
            kiomon_core::ExitCode::InvalidArgs => 400,
            // "The environment cannot serve this" — no embedder, an unparseable config.
            // The request was fine and retrying after a fix works, which is what 503 says.
            kiomon_core::ExitCode::Config => 503,
            kiomon_core::ExitCode::Success | kiomon_core::ExitCode::Internal => 500,
        };
        Self::new(status, err.code(), err.to_string())
    }
}

/// What one verb produced, or why it did not.
pub type Reply = Result<serde_json::Value, Fault>;

/// The brain, as the transports see it.
///
/// Two paths and no connection: a request needs "where is the file" and "which config", and
/// everything else is decided per request by the same helpers the CLI uses.
#[derive(Debug, Clone)]
pub struct Service {
    db: Option<PathBuf>,
    config: Option<PathBuf>,
}

impl Service {
    /// A service over the database and config the command line named.
    #[must_use]
    pub fn new(db: Option<PathBuf>, config: Option<PathBuf>) -> Self {
        Self { db, config }
    }

    /// The database path as it was given, for a startup log line.
    #[must_use]
    pub fn db_path(&self) -> Option<&Path> {
        self.db.as_deref()
    }

    fn open(&self) -> Result<Database, Fault> {
        Ok(open_paths(self.db.as_deref(), self.config.as_deref())?)
    }

    /// The scope a read covers: `'*'` for every workspace, otherwise exactly one.
    ///
    /// `None` means the configured default and never "all", which is the rule the CLI
    /// keeps and the reason `--workspace '*'` exists: absence and widening are different
    /// statements, and a wire that conflated them would be a data-leak-shaped bug in a
    /// brain with two workspaces.
    fn scope(&self, db: &Database, workspace: Option<&str>) -> Result<Scope, Fault> {
        Ok(read_scope(db, self.config.as_deref(), workspace)?)
    }

    fn workspace(&self, db: &Database, workspace: Option<&str>) -> Result<String, Fault> {
        Ok(write_workspace(db, self.config.as_deref(), workspace)?)
    }

    /// `GET /api/search?q&limit&workspace_id&kind&legs`
    ///
    /// # Errors
    ///
    /// Returns a [`Fault`] for an unknown workspace or kind, for `legs=graph` (which is
    /// `501`: the leg is v1.1, the graph is not), and for an embedder the request needed
    /// and could not reach.
    pub fn search(&self, request: &SearchRequest) -> Reply {
        let db = self.open()?;
        let scope = self.scope(&db, request.workspace_id.as_deref())?;
        let legs = parse_legs(request.legs.as_deref())?;

        let outcome = db.search(&SearchQuery {
            text: request.q.clone(),
            scope,
            kinds: request.kinds()?,
            limit: request.limit(),
            snippet_tokens: kiomon_core::DEFAULT_SNIPPET_TOKENS,
            sort: SearchSort::default(),
            legs,
            record: true,
        })?;

        // The hosted route answers `{results, query}`; this adds what a local caller
        // cannot otherwise see — which rung answered, which legs ran, whether one
        // degraded. Additive, never a rename: a hosted SDK reads `results` and `query` and
        // ignores the rest, while an agent asking "why was this empty" reads the same body.
        let mut body = to_value(&crate::out::SearchResponse::new(
            &outcome,
            &request.q,
            legs.as_str(),
        ))?;
        if outcome.degraded {
            // The warning the CLI writes to stderr is a field here. Over HTTP there is no
            // stderr belonging to this request, and a degraded answer delivered silently
            // would let a caller conclude the vector leg found nothing rather than that it
            // did not run.
            if let Some(object) = body.as_object_mut() {
                object.insert(
                    "warning".to_owned(),
                    serde_json::Value::String(format!(
                        "the vector leg did not run ({}), so these results are full text only",
                        outcome.reason
                    )),
                );
            }
        }
        Ok(body)
    }

    /// `POST /api/retrieve` — the context packet, verbatim.
    ///
    /// The packet's own JSON *is* the hosted wire payload (WP5.5 ported it from there), so
    /// this route does not reshape it: a field added to the packet appears here, which is
    /// the property that keeps the two from drifting.
    ///
    /// # Errors
    ///
    /// Returns a [`Fault`] for an unknown workspace or kind, and whatever the candidate
    /// search raises.
    pub fn retrieve(&self, request: &RetrieveRequest) -> Reply {
        let db = self.open()?;
        let scope = self.scope(&db, request.workspace_id.as_deref())?;
        let packet = db.retrieve(&RetrieveQuery {
            intent: request.intent()?,
            text: request.query.clone(),
            scope,
            kinds: request.kinds()?,
            constraints: request.constraints.clone(),
            entities: request.entities.clone(),
            risk_level: request.risk_level.as_deref().and_then(parse_risk),
            limit: usize::from(request.limit.unwrap_or(6).clamp(1, 20)),
            record: true,
            legs: parse_legs(request.legs.as_deref())?,
        })?;
        to_value(&packet)
    }

    /// `GET /api/documents/{id}`
    ///
    /// # Errors
    ///
    /// Returns a `404` [`Fault`] when the id is unknown.
    pub fn get_memory(&self, id: &str) -> Reply {
        let db = self.open()?;
        to_value(&db.get(id)?)
    }

    /// `GET /api/documents/{id}/links`
    ///
    /// The hosted route answers one direction with four fields (`link_id`, `relation`,
    /// `target_id`, `title`, `excerpt`). This answers both directions with the same four —
    /// `target_id` still means "the other end", so a hosted reader pairing this with
    /// `get_memory` keeps working — plus the ones a local caller cannot get elsewhere.
    /// `relate` already reads both ends, and a route that silently showed half of what its
    /// own CLI shows would be exactly the drift this module exists to prevent.
    ///
    /// # Errors
    ///
    /// Returns a `404` [`Fault`] when the id is unknown.
    pub fn links(&self, id: &str) -> Reply {
        let db = self.open()?;
        let neighbours = db.relations(id, &[], 1)?;
        // The centre's id is what tells an incoming edge apart from an outgoing one, so the
        // row can be written from the perspective of the memory that was asked about.
        let mut rows = Vec::with_capacity(neighbours.len());
        for neighbour in &neighbours {
            rows.push(serde_json::json!({
                "target_id": neighbour.memory_id,
                "relation": neighbour.relation.as_str(),
                "title": neighbour.title,
                "excerpt": neighbour.excerpt,
                "kind": neighbour.kind.as_str(),
                "direction": neighbour.direction.map(kiomon_core::EdgeDirection::as_str),
                "score": neighbour.score,
                "origin": neighbour.origin,
                "status": neighbour.status.as_str(),
                "created_at": neighbour.created_at,
            }));
        }
        Ok(serde_json::Value::Array(rows))
    }

    /// `POST /api/documents/{id}/link` — write one typed edge.
    ///
    /// # Errors
    ///
    /// Returns a [`Fault`] when either end is unknown (`404`), when either end is waiting
    /// in the review inbox (`404` — the inbox is not addressable, in either direction), and
    /// for an unknown relation name (`400`).
    pub fn link(&self, id: &str, body: &LinkRequest) -> Reply {
        let db = self.open()?;
        let outcome = db.link(&NewLink {
            source_id: id.to_owned(),
            target_id: body.target_id.clone(),
            relation: parse_relation(body.relation.as_deref())?,
            score: None,
        })?;
        Ok(serde_json::json!({
            "linked": true,
            "source_id": outcome.source_id,
            "target_id": outcome.target_id,
            "relation": outcome.relation.as_str(),
            "created": outcome.created,
        }))
    }

    /// `POST /api/documents/{id}/unlink` — remove one typed edge.
    ///
    /// Not a hosted route: the hosted product retracts an edge with `DELETE
    /// /api/documents/:id/links/:targetId`, which cannot carry a relation name and so
    /// cannot say *which* of two edges between the same pair to remove. The local engine
    /// can, so the local route is a body.
    ///
    /// # Errors
    ///
    /// Returns a `409` [`Fault`] when there was no such edge, and `400` for an unknown
    /// relation.
    pub fn unlink(&self, id: &str, body: &LinkRequest) -> Reply {
        let db = self.open()?;
        let relation = body
            .relation
            .as_deref()
            .map(|name| parse_relation(Some(name)));
        let relation = match relation {
            Some(parsed) => Some(parsed?),
            None => None,
        };
        let outcome = db.unlink(id, &body.target_id, relation)?;
        if outcome.removed == 0 {
            return Err(Fault::new(
                409,
                "conflict",
                format!(
                    "no edge between {id} and {} to remove (try --relation to name which one)",
                    body.target_id
                ),
            ));
        }
        Ok(serde_json::json!({
            "unlinked": true,
            "removed": outcome.removed,
        }))
    }

    /// `PATCH /api/documents/{id}` — triage fields, tags, the body, and pinning.
    ///
    /// # Errors
    ///
    /// Returns a [`Fault`] for an unknown id (`404`), an empty patch (`400`), a bad value
    /// (`400`), a `workspace_id` (which is `501`: a memory's workspace is a read boundary,
    /// and no verb here moves one), and `409` when nothing moved.
    pub fn patch(&self, id: &str, body: &PatchRequest) -> Reply {
        if body.is_empty() {
            // The hosted route answers `200 {updated: true}` to an empty patch, reporting
            // success for a request that changed nothing. A caller reading that believes it
            // wrote something.
            return Err(Fault::new(
                400,
                "bad_request",
                "the patch is empty, so nothing would change. Send at least one field.",
            ));
        }
        if body.workspace_id.is_some() {
            return Err(Fault::unsupported(
                "moving a memory between workspaces",
                "a workspace is a read boundary in this engine and no verb crosses one, so a \
                 patch that names one is refused rather than half-applied. Write the memory \
                 into the other workspace instead.",
            ));
        }
        let db = self.open()?;
        let ids = [id.to_owned()];
        // Two statements, because pinning is not a column this engine's update can set: it
        // is a state transition with its own meaning (`set_pinned` refuses a memory that
        // is not active, where `update` would happily set a flag on an archived row).
        let mut moved = false;
        if let Some(pinned) = body.pinned {
            let flag = match pinned {
                0 => false,
                1 => true,
                other => {
                    return Err(Fault::new(
                        400,
                        "bad_request",
                        format!("`pinned` must be 0 or 1, got {other}"),
                    ));
                }
            };
            moved = !db.set_pinned(&ids, flag)?.moved.is_empty();
        }
        if body.has_fields() {
            let outcome: UpdateOutcome = db.update(id, &body.to_patch()?)?;
            moved = moved || outcome.changed > 0;
            if !moved {
                return Err(Fault::new(
                    409,
                    "conflict",
                    format!("nothing changed for {id}"),
                ));
            }
            return Ok(serde_json::json!({
                "updated": true,
                "id": outcome.id,
                "changed": outcome.changed,
                "warnings": outcome.warnings,
            }));
        }
        if !moved {
            return Err(Fault::new(
                409,
                "conflict",
                format!("{id} is already pinned that way, so nothing moved"),
            ));
        }
        Ok(serde_json::json!({ "updated": true, "id": id }))
    }

    /// `POST /api/documents/{id}/{action}` — the lifecycle verbs.
    ///
    /// # Errors
    ///
    /// Returns a [`Fault`] for an unknown id (`404`), for `revert` (`501` — no revision
    /// history), for an unfinished action name (`400`), and `409` when nothing moved.
    pub fn manage(&self, id: &str, action: &str) -> Reply {
        let ids = [id.to_owned()];
        match action {
            "archive" => self.ids_action("archive", |db| db.archive(&ids)),
            "restore" => self.ids_action("restore", |db| db.restore(&ids)),
            "forget" => {
                let db = self.open()?;
                let outcome: ForgetOutcome = db.forget(&ids)?;
                if outcome.deleted.is_empty() {
                    // `missing` is already the ids that were not there, so it is the detail the
                    // message needs rather than something to wrap around the id a second time.
                    return Err(nothing_moved(action, &outcome.missing));
                }
                Ok(serde_json::json!({ "forgotten": true, "id": id }))
            }
            "approve" => self.review_action("approved", action, |db| db.approve(&ids)),
            "reject" => self.review_action("rejected", action, |db| db.reject(&ids)),
            "revert" => Err(Fault::unsupported(
                "revert",
                "this engine keeps no revision history, so there is nothing to revert to. The \
                 hosted product records a decision per write; here a memory's `updated_at` is \
                 the only record that it changed.",
            )),
            other => Err(Fault::new(
                400,
                "bad_request",
                format!(
                    "unknown action `{other}`. Expected one of: archive, restore, forget, \
                     approve, reject (revert is not available locally)."
                ),
            )),
        }
    }

    /// One of the verbs that reports the ids that moved.
    fn ids_action<F>(&self, action: &str, run: F) -> Reply
    where
        F: FnOnce(&Database) -> kiomon_core::Result<IdsOutcome>,
    {
        let db = self.open()?;
        let outcome = run(&db)?;
        if outcome.moved.is_empty() {
            return Err(nothing_moved(action, &skip_text(&outcome.skipped)));
        }
        Ok(serde_json::json!({
            format!("{action}d"): true,
            "id": outcome.moved.first(),
            "moved": outcome.moved,
        }))
    }

    /// One of the review verbs, whose word for success is not the action's name.
    fn review_action<F>(&self, key: &'static str, action: &str, run: F) -> Reply
    where
        F: FnOnce(&Database) -> kiomon_core::Result<ReviewOutcome>,
    {
        let db = self.open()?;
        let outcome = run(&db)?;
        if outcome.moved.is_empty() {
            return Err(nothing_moved(action, &skip_text(&outcome.skipped)));
        }
        let id = outcome.moved.first().cloned();
        Ok(serde_json::json!({ key: true, "id": id, "moved": outcome.moved }))
    }

    /// `POST /api/documents/{id}/rate`
    ///
    /// # Errors
    ///
    /// Returns a [`Fault`] for an unknown id (`404`) and for a rating that is not `1` or
    /// `-1` (`400`).
    pub fn rate(&self, id: &str, body: &RateRequest) -> Reply {
        let rating = match body.rating {
            1 => Rating::Up,
            -1 => Rating::Down,
            other => {
                return Err(Fault::new(
                    400,
                    "bad_request",
                    format!("rating must be 1 or -1, got {other}"),
                ));
            }
        };
        let db = self.open()?;
        let outcome = db.rate(id, rating)?;
        Ok(serde_json::json!({
            "rated": true,
            "id": outcome.id,
            "rating": outcome.rating.value(),
            "feedback_score": outcome.feedback_score,
            "feedback_count": outcome.feedback_count,
        }))
    }

    /// `GET /api/graph?center_id&relation&depth`
    ///
    /// One hop, in the hosted response shape. `depth: 2` is `501` rather than answered with
    /// one hop: a caller that asked for two and got one cannot tell a thin graph from a
    /// shallow answer. `depth` is echoed in the body so a caller can see what it got.
    ///
    /// The hosted query hides `origin = 'auto'` edges. This does not, deliberately: a
    /// hosted auto-link is a background scan that writes thousands of similarity edges, so
    /// the view filters them; here the only edges most brains have are the ≤5 per
    /// activation that `add` wrote, and hiding them would make the route disagree with
    /// `relate`, which reads the same rows. `origin` travels on every edge, so a caller that
    /// wants the hosted view filters on it.
    ///
    /// # Errors
    ///
    /// Returns a [`Fault`] for a missing or unknown `center_id` (`400`/`404`), an unknown
    /// relation (`400`), and `depth` above 1 (`501`).
    pub fn graph(&self, query: &GraphRequest) -> Reply {
        let Some(center) = query
            .center_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return Err(Fault::new(
                400,
                "bad_request",
                "`center_id` is required locally: a whole-graph overview needs the \
                 most-connected-nodes seed the hosted product computes, and this engine has no \
                 such ranking. Ask for one memory's neighbourhood.",
            ));
        };
        let depth = query.depth.unwrap_or(1);
        if depth > 1 {
            return Err(Fault::unsupported(
                "depth > 1",
                "this engine reads one hop. Neighbours-of-neighbours is a path search rather \
                 than a neighbourhood, and answering it with one hop would be undetectable by \
                 the caller.",
            ));
        }
        let filter = match query.relation.as_deref().map(str::trim) {
            None | Some("" | "all") => Vec::new(),
            Some(name) => vec![parse_relation(Some(name))?],
        };
        let db = self.open()?;
        let memory = db.get(center)?;
        let neighbours = db.relations(center, &filter, 1)?;

        let mut nodes = vec![node_of(&memory, neighbours.len())];
        let mut edges = Vec::with_capacity(neighbours.len());
        for neighbour in &neighbours {
            // A directed edge read from the target's side points the other way, so the
            // drawing needs the direction rather than the pair.
            let (source, target) = match neighbour.direction {
                Some(kiomon_core::EdgeDirection::In) => {
                    (neighbour.memory_id.clone(), memory.id.clone())
                }
                _ => (memory.id.clone(), neighbour.memory_id.clone()),
            };
            nodes.push(serde_json::json!({
                "id": neighbour.memory_id,
                "title": neighbour.title,
                "kind": neighbour.kind.as_str(),
                "excerpt": neighbour.excerpt,
                "status": neighbour.status.as_str(),
            }));
            edges.push(serde_json::json!({
                "source_id": source,
                "target_id": target,
                "relation": neighbour.relation.as_str(),
                "score": neighbour.score,
                "origin": neighbour.origin,
            }));
        }
        Ok(serde_json::json!({
            "nodes": nodes,
            "edges": edges,
            // The hosted field for collapsed `memory -> entity -> memory` links. This engine
            // has no entity index, so it is always empty — present because a client reads
            // it, and empty because that is the truth.
            "entityLinks": [],
            "totalNodes": nodes.len(),
            "hasMore": false,
            "depth": depth,
        }))
    }

    /// `GET /api/workspaces`
    ///
    /// # Errors
    ///
    /// Returns a [`Fault`] when the database cannot be read.
    pub fn workspaces(&self) -> Reply {
        let db = self.open()?;
        let workspaces: Vec<serde_json::Value> = db
            .workspaces()?
            .iter()
            .enumerate()
            .map(|(index, workspace)| {
                serde_json::json!({
                    "id": workspace.id,
                    "name": workspace.name,
                    "review_mode": workspace.review_mode.as_str(),
                    "created_at": workspace.created_at,
                    // The hosted route marks the first row of a creation-ordered list as the
                    // default. There is no per-user default column here — the default is a
                    // client setting (`--workspace`, `KIOMON_WORKSPACE`, the config file) —
                    // so the same convention is used, which is what a workspace picker sorts
                    // on.
                    "is_default": index == 0,
                })
            })
            .collect();
        Ok(serde_json::json!({ "workspaces": workspaces }))
    }

    /// `GET /api/workspaces/{id}/context?topic`
    ///
    /// The hosted composition: pinned memories, the strongest procedures, a few semantic
    /// rows for the topic, and per-kind counts. Every piece is a read this engine already
    /// has, which is why it is answerable rather than `501` — §10's absence is *briefings*,
    /// which need a model to write prose, not a context assembly.
    ///
    /// # Errors
    ///
    /// Returns a `404` [`Fault`] for an unknown workspace, and whatever the reads raise.
    pub fn context(&self, workspace: &str, topic: Option<&str>) -> Reply {
        let db = self.open()?;
        let resolved = db.resolve_workspace(workspace)?;
        let scope = Scope::One(resolved.id.clone());

        let listed = |kinds: Vec<MemoryKind>, limit: u32| -> Result<Vec<Memory>, Fault> {
            Ok(db.list(&ListQuery {
                scope: scope.clone(),
                kinds,
                status: Some(Status::Active),
                tags: Vec::new(),
                limit,
                sort: ListSort::Strength,
            })?)
        };

        // Pinned first, because the hosted query orders pinned rows by creation and this
        // engine's `ListSort::Strength` puts them first anyway — the filter is what makes
        // the list *only* pinned, and ten is enough to fill a context card.
        let pinned: Vec<Memory> = listed(Vec::new(), 10)?
            .into_iter()
            .filter(|memory| memory.pinned)
            .collect();
        let procedures = listed(vec![MemoryKind::Procedural], 10)?;
        let semantic = match topic.map(str::trim).filter(|topic| !topic.is_empty()) {
            Some(text) => db
                .search(&SearchQuery {
                    text: text.to_owned(),
                    scope: scope.clone(),
                    kinds: vec![MemoryKind::Semantic],
                    limit: 5,
                    snippet_tokens: kiomon_core::DEFAULT_SNIPPET_TOKENS,
                    sort: SearchSort::default(),
                    legs: Legs::Auto,
                    record: false,
                })?
                .hits
                .iter()
                .map(|hit| hit.memory.clone())
                .collect(),
            None => listed(vec![MemoryKind::Semantic], 5)?,
        };

        let stats = db.stats(&scope, &[])?;
        let pending = db.pending_count(&scope)?;
        let mut counts = serde_json::json!({
            "total": stats.total,
            "reference": 0,
            "semantic": 0,
            "procedural": 0,
            "episodic": 0,
            "pinned": stats.pinned,
            "pending_approval": pending,
        });
        if let Some(object) = counts.as_object_mut() {
            for count in &stats.by_kind {
                object.insert(count.kind.to_owned(), serde_json::json!(count.count));
            }
        }
        Ok(serde_json::json!({
            "workspace": {
                "id": resolved.id,
                "name": resolved.name,
                "review_mode": resolved.review_mode.as_str(),
            },
            "pinned": summaries(&pinned)?,
            "procedural_rules": summaries(&procedures)?,
            "topic_semantic": summaries(&semantic)?,
            "summary": counts,
            "has_pending_review": pending > 0,
        }))
    }

    /// `POST /api/memories/batch-draft` — several learnings, one write each.
    ///
    /// The hosted route runs a DISCARD/MERGE pre-pass and an autonomous promote pass before
    /// answering. Locally the same two decisions already belong to the store: an identical
    /// body is the dedupe index's business (`duplicate`), and whether a write activates is
    /// the workspace's `review_mode` (`gate.rs`). So the answer is per learning, in the
    /// hosted envelope, with the local verdicts beside it.
    ///
    /// # Errors
    ///
    /// Returns a [`Fault`] for an unknown workspace or kind (`404`/`400`), and for a
    /// learning that breaks one of the shared write limits — a `400` naming the field,
    /// rather than a partial write nobody can account for.
    pub fn batch_draft(&self, request: &BatchDraftRequest) -> Reply {
        let db = self.open()?;
        let workspace = self.workspace(&db, request.workspace_id.as_deref())?;
        let mut promoted = Vec::new();
        let mut pending = Vec::new();
        let mut duplicates = Vec::new();
        let mut memories = Vec::new();

        for learning in &request.learnings {
            let kind = parse_kind(&learning.kind)?;
            let mut new = NewMemory::new(kind, learning.title.clone())
                .workspace(workspace.clone())
                .body(learning.body.clone())
                .excerpt(learning.excerpt.clone().unwrap_or_default())
                .tags(learning.tags.clone())
                .source("agent_reflection".to_owned())
                .derived_from(learning.source_refs.clone())
                .confidence(learning.confidence.unwrap_or(0.5));
            new = new.force(false);
            let outcome = db.add(&new)?;
            if outcome.duplicate {
                duplicates.push(outcome.id.clone());
            }
            if outcome.status == Status::Active {
                promoted.push(outcome.id.clone());
            } else {
                pending.push(outcome.id.clone());
            }
            memories.push(serde_json::json!({
                "id": outcome.id,
                "title": learning.title,
                "kind": kind.as_str(),
                "status": outcome.status.as_str(),
            }));
        }

        Ok(serde_json::json!({
            "drafted": memories.len(),
            // The two hosted counters this engine has no pre-pass for. Reported as zero
            // rather than omitted: a client reads them as numbers, and an absent field
            // would make it print "undefined".
            "discarded": [],
            "merged": 0,
            "promoted": promoted,
            "pending": pending,
            "duplicates": duplicates,
            "memories": memories,
            "workspace_id": workspace,
            // The hosted field a client follows to the review inbox. There is no dashboard
            // to open locally, so it names the command instead of a URL.
            "review_inbox_url": if pending.is_empty() {
                None
            } else {
                Some("kiomon review list".to_owned())
            },
        }))
    }

    /// `POST /api/memories/record-outcome` — credit attribution.
    ///
    /// The hosted answer is `{updated, skipped}` with `skipped` as bare ids, and that shape
    /// is what an SDK destructures. The local detail — why an id was skipped: unknown,
    /// already archived, outside the scope — travels beside it in `skipped_detail` rather
    /// than replacing it.
    ///
    /// # Errors
    ///
    /// Returns a [`Fault`] for an unknown outcome or assessment (`400`), for more items
    /// than one call may assess (`400`), and for an unknown workspace (`404`).
    pub fn record_outcome(&self, request: &RecordOutcomeRequest) -> Reply {
        let db = self.open()?;
        let scope = self.scope(&db, request.workspace_id.as_deref())?;
        if request.items.is_empty() {
            // The hosted schema requires at least one item. Accepting an empty list would
            // report a successful task end that credited nothing, which is the one thing a
            // caller cannot tell apart from a bug in its own bookkeeping.
            return Err(Fault::new(
                400,
                "validation_failed",
                "`items` must hold at least one assessed memory",
            ));
        }
        let mut items = Vec::with_capacity(request.items.len());
        for item in &request.items {
            items.push((item.memory_id.clone(), parse_assessment(&item.assessment)?));
        }
        let report = db.record_outcome(request.task_outcome()?, &items, &scope)?;
        let skipped: Vec<&str> = report.skipped.iter().map(|skip| skip.id.as_str()).collect();
        Ok(serde_json::json!({
            "updated": report.applied,
            "skipped": skipped,
            "skipped_detail": report.skipped,
            "outcome": report.outcome.as_str(),
        }))
    }

    /// `GET /api/memories?status=` — the review inbox.
    ///
    /// # Errors
    ///
    /// Returns a [`Fault`] for an unknown status (`400`) and an unknown workspace (`404`).
    pub fn memories(&self, workspace: Option<&str>, status: Option<&str>) -> Reply {
        let db = self.open()?;
        let scope = self.scope(&db, workspace)?;
        let status = match status.map(str::trim).filter(|name| !name.is_empty()) {
            None | Some("pending_approval") => Status::PendingApproval,
            Some(other) => parse_status(other)?,
        };
        // The inbox is a queue with a writer waiting at the end of it, so it is read
        // shallowly; a status listing is a browse, and browses are the ones that page.
        let rows = if status == Status::PendingApproval {
            db.list_pending(&PendingQuery {
                scope,
                limit: 20,
                ..PendingQuery::default()
            })?
        } else {
            db.list(&ListQuery {
                scope,
                kinds: Vec::new(),
                status: Some(status),
                tags: Vec::new(),
                limit: 50,
                sort: ListSort::Recent,
            })?
        };
        Ok(serde_json::json!({
            "memories": summaries(&rows)?,
            "status": status.as_str(),
            "count": rows.len(),
        }))
    }
}

/// The `409` for a verb that was valid and moved nothing.
fn nothing_moved(action: &str, detail: &[String]) -> Fault {
    let suffix = detail
        .first()
        .map(|first| format!(": {first}"))
        .unwrap_or_default();
    Fault::new(
        409,
        "conflict",
        format!("`{action}` changed nothing{suffix}"),
    )
}

/// A neighbour's skip list, as one line per id and one reason.
fn skip_text(skipped: &[Skipped]) -> Vec<String> {
    skipped
        .iter()
        .map(|skip| format!("{} ({})", skip.id, skip.reason.as_str()))
        .collect()
}

/// A centre node, in the hosted graph shape.
///
/// `link_count` is the number of edges this read actually returned, not a stored counter: the
/// hosted product maintains one on the row, and this engine has no such column — inventing a
/// number from `corroboration_count` would be a different quantity under a familiar name.
fn node_of(memory: &Memory, link_count: usize) -> serde_json::Value {
    serde_json::json!({
        "id": memory.id,
        "title": memory.title,
        "kind": memory.kind.as_str(),
        "tags": memory.tags,
        "source": memory.source,
        "source_url": memory.source_url,
        "pinned": memory.pinned,
        "status": memory.status.as_str(),
        "link_count": link_count,
    })
}

/// A page of summaries, as JSON.
fn summaries(memories: &[Memory]) -> Result<serde_json::Value, Fault> {
    let rows: Vec<serde_json::Value> = memories
        .iter()
        .map(|memory| to_value(&MemorySummaryJson::from(memory)))
        .collect::<Result<Vec<_>, Fault>>()?;
    Ok(serde_json::Value::Array(rows))
}

/// Encode a value for a wire, naming the field that could not be encoded.
fn to_value<T: serde::Serialize>(value: &T) -> Result<serde_json::Value, Fault> {
    serde_json::to_value(value).map_err(|err| {
        // A serialisation failure here is a bug in this crate rather than anything the
        // caller did, so it says so: `500` and a message that names the error, because a
        // silently empty body would look like an answer.
        Fault::new(
            500,
            "internal",
            format!("could not encode the response: {err}"),
        )
    })
}

// ── request bodies ──────────────────────────────────────────────────────────
//
// Deserialised straight from the wire, so the validation a hosted route gets from zod
// happens once per field: `deny_unknown_fields` where a typo would otherwise be silently
// ignored, and `Option` everywhere the hosted schema is optional.

/// `GET /api/search` — a query string, so every field is optional and arrives as text.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchRequest {
    /// The query.
    #[serde(default)]
    pub q: String,
    /// How many rows.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Workspace id or name; `'*'` for every workspace.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// One kind, or a comma-separated list.
    #[serde(default)]
    pub kind: Option<String>,
    /// `fts`, `vector`, `hybrid` (the hosted spelling of `auto`), or `graph`.
    #[serde(default)]
    pub legs: Option<String>,
}

impl SearchRequest {
    /// The page size, clamped to the contract's ceiling.
    #[must_use]
    pub fn limit(&self) -> u32 {
        self.limit.unwrap_or(20).clamp(1, 200)
    }

    /// The kinds filter, from one value or a comma-separated list.
    ///
    /// # Errors
    ///
    /// Returns a `400` [`Fault`] naming an unknown kind.
    pub fn kinds(&self) -> Result<Vec<MemoryKind>, Fault> {
        parse_kinds(self.kind.as_deref())
    }
}

/// `POST /api/retrieve` — the hosted schema, field for field.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetrieveRequest {
    /// `execute`, `plan`, `answer`, `verify` or `debug`.
    #[serde(default)]
    pub intent: String,
    /// The query.
    #[serde(default)]
    pub query: String,
    /// Entity names, carried for wire fidelity and never read.
    #[serde(default)]
    pub entities: Vec<String>,
    /// Current-state hints, used only to judge applicability.
    #[serde(default)]
    pub constraints: Vec<String>,
    /// Kinds to draw from.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// The workspace.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// `low`, `medium` or `high`, carried for wire fidelity and never read.
    #[serde(default)]
    pub risk_level: Option<String>,
    /// Packet size, 1–20.
    #[serde(default)]
    pub limit: Option<u8>,
    /// Which legs the candidate search may use. Not in the hosted schema, and accepted so a
    /// caller that read `legs` off a search response can pin it — additive, so a hosted
    /// client that never sends it is unaffected.
    #[serde(default)]
    pub legs: Option<String>,
}

impl RetrieveRequest {
    /// The intent, parsed.
    ///
    /// # Errors
    ///
    /// Returns a `400` [`Fault`] naming the vocabulary for an unknown or missing intent.
    pub fn intent(&self) -> Result<Intent, Fault> {
        parse_intent(&self.intent)
    }

    /// The kinds, parsed from the hosted vocabulary.
    ///
    /// # Errors
    ///
    /// Returns a `400` [`Fault`] naming the first kind that did not parse.
    pub fn kinds(&self) -> Result<Vec<MemoryKind>, Fault> {
        self.kinds.iter().map(|name| parse_kind(name)).collect()
    }
}

/// `POST /api/documents/{id}/link` and `/unlink`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkRequest {
    /// The other end.
    #[serde(default)]
    pub target_id: String,
    /// `related_to` when absent.
    #[serde(default)]
    pub relation: Option<String>,
}

/// `PATCH /api/documents/{id}`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchRequest {
    /// `1` pins, `0` unpins.
    #[serde(default)]
    pub pinned: Option<i64>,
    /// A new title.
    #[serde(default)]
    pub title: Option<String>,
    /// A new excerpt.
    #[serde(default)]
    pub excerpt: Option<String>,
    /// Replacement tags.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// A new body.
    #[serde(default)]
    pub body: Option<String>,
    /// A new kind.
    #[serde(default)]
    pub kind: Option<String>,
    /// A new workspace. Refused — see [`Service::patch`].
    #[serde(default)]
    pub workspace_id: Option<String>,
}

impl PatchRequest {
    /// Whether the patch asks for anything at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pinned.is_none() && !self.has_fields() && self.workspace_id.is_none()
    }

    /// Whether the patch asks for anything the store's `update` can change.
    #[must_use]
    pub fn has_fields(&self) -> bool {
        self.title.is_some()
            || self.excerpt.is_some()
            || self.tags.is_some()
            || self.body.is_some()
            || self.kind.is_some()
    }

    /// Build the store's patch.
    ///
    /// # Errors
    ///
    /// Returns a `400` [`Fault`] for an unknown kind.
    pub fn to_patch(&self) -> Result<MemoryPatch, Fault> {
        let mut patch = MemoryPatch {
            title: self.title.clone(),
            excerpt: self.excerpt.clone(),
            body: self.body.clone(),
            tags: self.tags.clone(),
            ..MemoryPatch::default()
        };
        if let Some(kind) = &self.kind {
            patch.kind = Some(parse_kind(kind)?);
        }
        Ok(patch)
    }
}

/// `POST /api/documents/{id}/rate`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateRequest {
    /// `1` or `-1`.
    #[serde(default)]
    pub rating: i64,
}

/// `GET /api/graph`
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GraphRequest {
    /// The memory at the centre.
    #[serde(default)]
    pub center_id: Option<String>,
    /// One relation to follow.
    #[serde(default)]
    pub relation: Option<String>,
    /// Hops. 1 only.
    #[serde(default)]
    pub depth: Option<u8>,
}

/// One learning in a batch draft.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Learning {
    /// The kind.
    #[serde(default)]
    pub kind: String,
    /// The title.
    #[serde(default)]
    pub title: String,
    /// The body.
    #[serde(default)]
    pub body: String,
    /// The card subheading.
    #[serde(default)]
    pub excerpt: Option<String>,
    /// The writer's confidence.
    #[serde(default)]
    pub confidence: Option<f64>,
    /// Tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Ids this learning was reasoned from, wired as `derived_from` edges.
    #[serde(default)]
    pub source_refs: Vec<String>,
}

/// `POST /api/memories/batch-draft`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchDraftRequest {
    /// The workspace.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// The learnings.
    #[serde(default)]
    pub learnings: Vec<Learning>,
}

/// One assessed memory in an outcome report.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutcomeItem {
    /// The memory.
    #[serde(default)]
    pub memory_id: String,
    /// `helpful`, `unused`, `misleading` or `outdated`.
    #[serde(default)]
    pub assessment: String,
}

/// `POST /api/memories/record-outcome`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordOutcomeRequest {
    /// `success` or `failure`.
    #[serde(default)]
    pub outcome: String,
    /// The assessed memories.
    #[serde(default)]
    pub items: Vec<OutcomeItem>,
    /// The workspace the assessment covers.
    #[serde(default)]
    pub workspace_id: Option<String>,
}

impl RecordOutcomeRequest {
    /// The session outcome, parsed.
    ///
    /// # Errors
    ///
    /// Returns a `400` [`Fault`] for an unknown outcome word.
    pub fn task_outcome(&self) -> Result<TaskOutcome, Fault> {
        match self.outcome.as_str() {
            "success" => Ok(TaskOutcome::Success),
            "failure" => Ok(TaskOutcome::Failure),
            other => Err(Fault::new(
                400,
                "validation_failed",
                format!("`outcome` must be `success` or `failure`, got `{other}`"),
            )),
        }
    }
}

// ── vocabulary ─────────────────────────────────────────────────────────────
//
// The hosted product's spellings live here, once, so the HTTP route, the MCP tool and the
// error message that lists the vocabulary cannot disagree about it.

/// Parse the hosted `legs` vocabulary.
///
/// [`Legs::parse`] already knows the aliases (`hybrid` is the hosted word for this engine's
/// `auto`, and `both`, `lexical`, `fulltext` and `semantic` are the rest), so this only adds
/// the refusal. `graph` parses here and fails at the search, because the leg is v1.1 and a
/// `400` calling it a typo would be wrong about a word the hosted product accepts.
///
/// # Errors
///
/// Returns a `400` [`Fault`] naming the vocabulary for anything else.
pub fn parse_legs(raw: Option<&str>) -> Result<Legs, Fault> {
    let text = raw.unwrap_or("");
    // Absent is the default leg, not an error: a request that says nothing about legs gets the
    // hybrid search, which is what the hosted route does with `legs=` unset and what the CLI's
    // `--legs auto` means. `Legs::parse` deliberately has no empty-string arm — a CLI flag is
    // always *some* value — so the wire's absence is handled here, once.
    if text.trim().is_empty() {
        return Ok(Legs::Auto);
    }
    Legs::parse(text).ok_or_else(|| {
        Fault::new(
            400,
            "bad_request",
            format!("unknown legs `{text}`. Expected one of: fts, vector, hybrid, graph."),
        )
    })
}

/// Parse one kind by its wire name.
///
/// # Errors
///
/// Returns a `400` [`Fault`] naming the vocabulary for anything outside it.
pub fn parse_kind(raw: &str) -> Result<MemoryKind, Fault> {
    raw.parse::<MemoryKind>()
        .map_err(|err| Fault::new(400, "validation_failed", err.to_string()))
}

/// Parse a comma-separated kind list, or nothing at all.
///
/// # Errors
///
/// Returns a `400` [`Fault`] naming the kind that did not parse.
pub fn parse_kinds(raw: Option<&str>) -> Result<Vec<MemoryKind>, Fault> {
    raw.unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(parse_kind)
        .collect()
}

/// Parse a relation by its wire name, defaulting to `related_to`.
///
/// # Errors
///
/// Returns a `400` [`Fault`] naming the vocabulary for anything else.
pub fn parse_relation(raw: Option<&str>) -> Result<Relation, Fault> {
    let name = raw.map_or("", str::trim);
    if name.is_empty() {
        return Ok(Relation::RelatedTo);
    }
    name.parse::<Relation>()
        .map_err(|err| Fault::new(400, "validation_failed", err.to_string()))
}

/// Parse a status by its wire name.
///
/// # Errors
///
/// Returns a `400` [`Fault`] naming the vocabulary for anything else.
pub fn parse_status(raw: &str) -> Result<Status, Fault> {
    raw.parse::<Status>()
        .map_err(|err| Fault::new(400, "bad_request", err.to_string()))
}

/// Parse an intent by its wire name.
///
/// The hosted route rejects an unknown intent with a `400` before any work happens, and this
/// is the same decision: an intent is how the packet is ordered, so guessing one would
/// silently answer a different question.
///
/// # Errors
///
/// Returns a `400` [`Fault`] naming the five intents.
pub fn parse_intent(raw: &str) -> Result<Intent, Fault> {
    Intent::ALL
        .iter()
        .find(|intent| intent.as_str() == raw.trim().to_ascii_lowercase())
        .copied()
        .ok_or_else(|| {
            Fault::new(
                400,
                "validation_failed",
                format!(
                    "`intent` must be one of: {} (got `{raw}`)",
                    Intent::ALL
                        .iter()
                        .map(|intent| intent.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
        })
}

/// Parse an assessment by its wire name.
///
/// # Errors
///
/// Returns a `400` [`Fault`] naming the four assessments.
pub fn parse_assessment(raw: &str) -> Result<Assessment, Fault> {
    Assessment::ALL
        .iter()
        .find(|assessment| assessment.as_str() == raw.trim().to_ascii_lowercase())
        .copied()
        .ok_or_else(|| {
            Fault::new(
                400,
                "validation_failed",
                format!(
                    "`{raw}` is not an assessment. Expected one of: {}.",
                    Assessment::ALL
                        .iter()
                        .map(|assessment| assessment.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
        })
}

/// Parse a risk level, if it is one the contract knows.
///
/// Returns `None` for anything else rather than failing: the field is never read, and
/// refusing a request over a hint the engine ignores would be strictness about nothing.
#[must_use]
pub fn parse_risk(raw: &str) -> Option<RiskLevel> {
    // Matched rather than iterated over an `ALL` array, because this is the only place the
    // list is needed and adding a constant to the library for one caller is the kind of
    // widening that outlives its reason.
    match raw.trim().to_ascii_lowercase().as_str() {
        "low" => Some(RiskLevel::Low),
        "medium" => Some(RiskLevel::Medium),
        "high" => Some(RiskLevel::High),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_that_says_nothing_about_legs_gets_the_default_leg() {
        // The wire's absence is not the CLI's absence: `--legs` always has a value because clap
        // supplies one, while a query string simply omits the parameter. Getting this wrong is a
        // `400` on the most common search in the product.
        assert_eq!(parse_legs(None).expect("default"), Legs::Auto);
        assert_eq!(parse_legs(Some("")).expect("default"), Legs::Auto);
        assert_eq!(
            parse_legs(Some("hybrid")).expect("hosted spelling"),
            Legs::Auto
        );
        assert_eq!(
            parse_legs(Some("H Y B R I D".replace(' ', "").as_str())).expect("case"),
            Legs::Auto
        );
        assert_eq!(parse_legs(Some("fts")).expect("lexical"), Legs::Fts);
        assert_eq!(parse_legs(Some("lexical")).expect("alias"), Legs::Fts);
        assert_eq!(parse_legs(Some("vector")).expect("semantic"), Legs::Vector);
        // `graph` parses here and fails at the search, because the hosted product accepts the word
        // and the *leg* is what is missing.
        assert_eq!(parse_legs(Some("graph")).expect("graph"), Legs::Graph);
        let refused = parse_legs(Some("sideways")).expect_err("unknown");
        assert_eq!(refused.status(), 400);
        assert!(refused.message().contains("fts, vector, hybrid, graph"));
    }

    #[test]
    fn an_edge_with_no_relation_named_is_a_related_to_edge() {
        // `related_to` is the default in the schema, in the auto-link pass and here: a caller that
        // does not care which kind of edge it draws gets the one kind that has no direction.
        assert_eq!(parse_relation(None).expect("default"), Relation::RelatedTo);
        assert_eq!(
            parse_relation(Some("")).expect("default"),
            Relation::RelatedTo
        );
        assert_eq!(
            parse_relation(Some("supersedes")).expect("named"),
            Relation::Supersedes
        );
        // The wire spelling is the primary one; the kebab form is the CLI's.
        assert!(parse_relation(Some("derived-from")).is_err());
        let refused = parse_relation(Some("nonsense")).expect_err("unknown");
        assert_eq!(refused.status(), 400);
        assert!(
            refused.message().contains("contradicts"),
            "{}",
            refused.message()
        );
    }

    #[test]
    fn a_kind_list_parses_commas_and_the_hosted_vocabulary() {
        assert!(parse_kinds(None).expect("none").is_empty());
        assert_eq!(
            parse_kinds(Some("semantic, procedural")).expect("two"),
            vec![MemoryKind::Semantic, MemoryKind::Procedural]
        );
        let refused = parse_kinds(Some("semantic,everything")).expect_err("unknown");
        assert_eq!(refused.status(), 400);
        assert!(refused.message().contains("everything"));
    }

    #[test]
    fn a_not_in_this_release_error_is_a_501_and_not_a_500() {
        // The CLI maps `Deferred` to exit code 1, which is right for a process. Over HTTP the
        // hosted contract has a status for it, and a `500` would tell an SDK to retry forever.
        let fault = Fault::from(Error::Deferred {
            command: "search --legs graph",
            reason: "the leg is v1.1",
        });
        assert_eq!(fault.status(), 501);
        assert_eq!(fault.code(), UNSUPPORTED_LOCALLY);
    }

    #[test]
    fn the_engine_errors_map_to_the_statuses_a_client_branches_on() {
        let cases = [
            (Error::NotFound("k_01".into()), 404, "not_found"),
            (Error::Invalid("bad".into()), 400, "invalid_args"),
            // "The environment cannot serve this" is a `503`: retrying after a fix works.
            (Error::Config("no embedder".into()), 503, "config"),
            (Error::Internal("boom".into()), 500, "internal"),
            (Error::Database("locked".into()), 500, "internal"),
        ];
        for (error, status, code) in cases {
            let fault = Fault::from(error);
            assert_eq!(fault.status(), status);
            assert_eq!(fault.code(), code);
        }
    }

    #[test]
    fn every_refusal_carries_prose_and_a_code() {
        // The hosted contract's two-field error body. A client that pattern-matched the prose
        // would break on the next message edit, which is why the code is separate and asserted.
        let fault = Fault::new(400, "bad_request", "say something useful");
        let body = fault.body();
        assert_eq!(body["code"], "bad_request");
        assert_eq!(body["error"], "say something useful");
    }

    #[test]
    fn an_unsupported_route_names_itself_and_the_reason() {
        let fault = Fault::unsupported("POST /api/memories/{id}/revert", "no revision history");
        assert_eq!(fault.status(), 501);
        assert!(fault.message().contains("POST /api/memories/{id}/revert"));
        assert!(fault.message().contains("no revision history"));
    }

    #[test]
    fn a_patch_with_only_unknown_fields_is_empty_and_a_patch_with_pinned_is_not() {
        let empty = PatchRequest::default();
        assert!(empty.is_empty());
        assert!(!empty.has_fields());

        let pinned = PatchRequest {
            pinned: Some(1),
            ..PatchRequest::default()
        };
        assert!(!pinned.is_empty(), "pinning is a change");
        assert!(!pinned.has_fields(), "but it is not an `update`");

        let titled = PatchRequest {
            title: Some("t".to_owned()),
            ..PatchRequest::default()
        };
        assert!(titled.has_fields());
        assert_eq!(
            titled.to_patch().expect("a patch").title.as_deref(),
            Some("t")
        );
    }

    #[test]
    fn an_unknown_outcome_word_is_refused_by_name() {
        let request = RecordOutcomeRequest {
            outcome: "sideways".to_owned(),
            ..RecordOutcomeRequest::default()
        };
        let refused = request.task_outcome().expect_err("unknown");
        assert!(refused.message().contains("success"));
    }

    #[test]
    fn an_assessment_is_one_of_the_four_the_contract_names() {
        for (name, expected) in [
            ("helpful", Assessment::Helpful),
            ("unused", Assessment::Unused),
            ("misleading", Assessment::Misleading),
            ("outdated", Assessment::Outdated),
        ] {
            assert_eq!(parse_assessment(name).expect(name), expected);
        }
        let refused = parse_assessment("meh").expect_err("unknown");
        assert_eq!(refused.status(), 400);
        assert!(
            refused
                .message()
                .contains("helpful, unused, misleading, outdated")
        );
    }
}
