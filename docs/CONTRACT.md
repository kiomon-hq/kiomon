# The `kiomon` CLI contract

<!-- SPDX-License-Identifier: Apache-2.0 -->

**Contract version: 1**

This is what an integration is written against. It is versioned separately from the
crate: adding a field is not a change, but changing or removing one, or changing an
exit code, is — and it needs a version bump and a note in the pull request.

Every `--json` response carries `meta.contract_version`. A caller should refuse a
version it does not know rather than mis-parse a version it does.

## Streams

| Stream | Carries |
|---|---|
| **stdout** | The answer, and nothing else. Human-readable by default; exactly one JSON document with `--json`. |
| **stderr** | Progress, warnings, and errors. Never parsed. |

`kiomon … | jq` always works. This is a rule, not a convention: `kiomon-core` has no
`print!` macro in it at all, so there is no code that could break it by accident.

Colors are never emitted. clap is built without its `color` feature, so escape
sequences cannot appear even on a terminal.

## Exit codes

| Code | Meaning | What a caller should do |
|---|---|---|
| `0` | Success. | Continue. Includes a search that matched nothing, and an `add` that found an identical memory — neither is a failure. |
| `1` | Internal failure: I/O, a corrupt database, a bug. | Report it. Retrying the same command is unlikely to help. |
| `2` | Not found, or nothing to do: the memory, workspace or link does not exist — or a verb named ids that are not in a state where it means anything (`pin` on a pinned memory, `restore` on an active one, `unlink` on an edge that was never there). | The thing is gone, or already the way you wanted it. Either way, retrying changes nothing. |
| `3` | Invalid arguments: unknown flag, conflicting options, a bad value, a missing required one. | Fix the command line. Retrying is pointless. |
| `4` | The request was valid and the environment cannot serve it: no embedder configured for `--legs vector`, an unwritable database directory, an unparseable config file. | Fix the environment or the configuration. |

`1` is also what `bench --gate` returns when a performance gate is missed: the run
succeeded and the answer is no.

`2` and `1` are deliberately distinct. An agent that receives `2` can conclude a
memory no longer exists; an agent that receives `1` should try again later.

`3` is also what a request this release cannot honour looks like when the *shape* of the
request is wrong for it: `relate --depth 2` is a malformed request here and not a missing
capability, so it fails the way a bad flag fails. A capability that is missing and asked for
by name — `search --legs graph` — is `1` with a message that says what is missing.

`4` is for a request whose *shape* was valid and whose environment cannot serve it:
today that is `--legs vector` with no embedder configured, and `reindex --embed` for the
same reason. The message names what to do instead, since an exit code that is a wall is
not an answer. `--legs vector` is never answered from the lexical leg — the platform
learned that the expensive way, and returning something other than what was asked for is
undetectable by the caller.

> clap's own exit code for a usage error is also `2`. The binary catches parse
> failures and remaps them to `3` for exactly this reason, and
> `crates/kiomon-cli/tests/cli.rs` asserts it.

## The `--json` envelope

Success:

```json
{"ok":true,"data":…,"meta":{"contract_version":"1","version":"0.1.0"}}
```

Failure:

```json
{"ok":false,"error":{"code":"not_found","message":"no memory with id `k_01JC9W5K2Q`"},"meta":{"contract_version":"1","version":"0.1.0"}}
```

`error.code` is one of `not_found`, `invalid_args`, `config`, `internal`,
`unimplemented` — and it agrees with the exit code.

Exactly one line, newline-terminated, always on stdout — including on failure, so a
caller can parse the error instead of guessing from stderr.

## Global flags

| Flag | Meaning |
|---|---|
| `--db <PATH>` | The database to use. Beats `$KIOMON_DB`, which beats the platform default path. |
| `--json` | Emit one JSON document on stdout. |
| `-q`, `--quiet` | Suppress non-error diagnostics on stderr. Never suppresses an error. |
| `--config <PATH>` | The config file to read. Beats `$KIOMON_CONFIG`, which beats the platform default path. |
| `--full` | Print whole field values instead of truncating them to fit a line. |

All five are accepted before *or* after the subcommand.

**Human output is bounded** so a four-kilobyte body cannot become a four-kilobyte line in
an agent's context window: `list` truncates each title to 120 characters unless `--full`
is passed, and `get` truncates the body the same way. The `--json` path is never
truncated — a machine asked for the field and gets the field.

**Environment.** `$KIOMON_DB` overrides the default database path, `$KIOMON_WORKSPACE`
overrides what an absent `--workspace` means — still exactly one workspace, never all of
them — and `$KIOMON_CONFIG` names the config file. `$KIOMON_EMBEDDER_API_KEY` supplies the
bearer token for an embedder that wants one, and is the right place for it: a secret in a
config file is a secret in every backup and every screenshot.

## The config file

TOML, at `--config <PATH>`, else `$KIOMON_CONFIG`, else `<data dir>/config.toml`
(`%APPDATA%\kiomon\config.toml` on Windows, `~/Library/Application Support/kiomon/` on
macOS, `$XDG_DATA_HOME/kiomon/` on Linux, `~/.kiomon/config.toml` as the fallback). Two
keys, both optional:

```toml
# What a command without --workspace uses. $KIOMON_WORKSPACE beats this.
default_workspace = "work"

[embedder]
# ollama (default) | openai | stub
provider = "ollama"
# Defaults to http://127.0.0.1:11434 for ollama; `openai` has no default.
url = "http://127.0.0.1:11434"
model = "nomic-embed-text"
# Optional. Refuses a model that returns a different width, which is what a wrong model
# name otherwise looks like: a corpus that mixes two spaces and gets quietly worse.
dim = 768
# Optional. Texts per HTTP call, and the seconds one call may take.
batch = 64
timeout_secs = 30
# Optional. `nomic-embed-text` needs these and nothing tells you; most models need none.
document_prefix = "search_document: "
query_prefix = "search_query: "
```

A **missing** file is not an error: a brain with no config is a full-text brain, and every
command works. A file that exists and does not parse **is** an error, and names the file and
the line — silently ignoring a typo in `[embedder]` would look exactly like a broken
embedder. An unknown key is an error too, for the same reason.

**Plain HTTP only.** `url` must start with `http://`. Every server this is built for —
Ollama, LM Studio, `llama.cpp`, vLLM, text-embeddings-inference — listens on plain HTTP on
localhost, and the default build has no TLS: `ureq`'s rustls feature costs about twenty
transitive crates against a budget of sixty (`plans/001-local-v1-core.md` §7, Appendix B),
and about half a megabyte of binary. An `https://` URL is refused by name, with the
workaround, rather than failing somewhere inside a handshake.

**`provider = "stub"`** is not a search feature: its vectors are a hashed bag of words, so
two texts that share words are near and two that mean the same thing are not. It exists for
tests and for `kiomon bench`, and `doctor` says so in the one place a user looks.

## `--workspace` semantics

The single most consequential default in the tool:

| Value | Meaning |
|---|---|
| absent | The **default workspace** — `$KIOMON_WORKSPACE` if set, else `default`. Never every workspace. |
| `<name>` | Exactly that workspace. |
| `'*'` | **Explicitly** every workspace. Honoured by `search`, `list`, `stats`, `export`, and `decay --all-workspaces`. |

Absence never widens scope. An agent that forgets the flag gets less, not more.

## Vocabulary

**Kinds** — `reference` (Files) · `semantic` (Facts) · `procedural` (Routines) ·
`episodic` (Moments). These strings are the wire values; the parenthesised names are
display labels and are never accepted as input.

**Statuses** — `active` · `pending_approval` · `archived`.

**Relations** — `derived_from` · `supports` · `contradicts` · `supersedes` ·
`related_to` · `precedes` · `follows` · `part_of`. All are directed except `related_to`,
which is symmetric and therefore stored once. `supersedes` and `contradicts` are the two a
packet reads: they become a warning and a demotion in `retrieve` (see
[the graph](#the-graph)).

**Legs** (for `--legs`) — `auto` · `fts` · `vector` · `graph`. The hosted product's
spellings are accepted as aliases: `hybrid` and `both` mean `auto`, `lexical` and `fulltext`
mean `fts`, `semantic` means `vector`. `graph` is in the vocabulary and has no leg behind it:
the graph itself works — `relate` walks it and the packet reads its typed edges — but a walk
*fused into a search's ranking* is v1.1 (plan §4.3). Asking for it is an error that says so,
which is the only answer that does not describe work that did not happen.

**Intents** (for `retrieve`) — `execute` · `plan` · `answer` · `verify` · `debug`.
They are the hosted route's vocabulary, the MCP tool's, and the Python SDK's
`RetrievalIntent`, in that order. Nothing else is accepted: an intent this engine does not
know is a usage error, not a default.

**Assessments** (for `outcome`) — `helpful` · `unused` · `misleading` · `outdated`.
An omitted memory is untouched; one reported as `unused` is credited with a *zero*, which
pulls its running feedback score toward zero and moves nothing else. See
[what a rating and an outcome move](#what-a-rating-and-an-outcome-actually-move).

**Sources** — `user_saved` · `agent_reflection` · `import` · `connector_sync`.
Display labels: saved · reflection · synced.

## Commands

Every command is listed in `kiomon --help`. This section records what each one
*promises*, which is the part `--help` cannot say.

### Setup and health

| Command | Promise |
|---|---|
| `init` | Creates the database, applies the schema, and ensures the default workspace exists. **Idempotent**: running it twice is not an error, and it never resets an existing workspace's review mode. `--workspace <NAME>` also creates that workspace; its id is the name slugged (`Client: Acme` → `client-acme`). |
| `doctor` | Reports the resolved path, the schema version, and the result of SQLite's `integrity_check`, `foreign_key_check` and FTS `integrity-check`, plus whether the full-text index agrees with the active memories. **Exit 0 when healthy, 1 when not** — and the report is printed either way. `--paths-only` skips the checks. Note that the checks are O(corpus): around 300 ms for a 43 MiB brain. |
| `stats` | Counts by kind and status, in one grouped query. `--kind` filters every number it reports, so the parts still sum to the total. |
| `reindex --embed` | Embeds every active memory that has no vector for the configured model, one page at a time, each page committed before the next is read — so an interrupted run keeps what it finished and running it again resumes. `--dry-run` counts without embedding, which is what to run first on a large brain. Exit `4` with no embedder configured: a reindex that wrote nothing and reported success would be worse than the message. |

### Writing

| Command | Promise |
|---|---|
| `add` | One transaction. Either the memory exists and is indexed, or it was a duplicate and the existing id is returned with `duplicate: true` and exit `0`, or nothing was written. An identical memory is never stored twice unless `--force`. |
| `update` | Re-indexes when a field retrieval can see — title, excerpt, body, tags — changed. A no-op update is not an error. |
| `import` | Restores a JSONL export, a markdown file, a directory of either, or stdin. **Ids, status and timestamps are the file's** — this is a restore, not a re-typing — and a line whose id already exists changes nothing. Streams, and reports a malformed line on stderr with its number and the fragment it could not read, then keeps going. Exit `0` if anything landed, `3` if *nothing* could be read (see [portability](#portability)). |
| `link` | Writes one typed edge. **Idempotent and exit `0` either way**: linking twice is not an error, and `created` says which happened, so a batch can be replayed. A symmetric edge (`related_to`) is canonically ordered, so `link A B` and `link B A` are the same row. Refuses a self-edge (exit `3`) and an edge to a memory that does not exist or is waiting in the review inbox (exit `2`). |
| `unlink` | Removes the edge between two memories, in either direction, or only edges of one `--relation`. **Exit `2` when there was nothing to remove** — "already unlinked" and "successfully unlinked" are the same state and different facts. |
| `relate` | The one-hop neighbourhood: for each edge, the relation, which way it points, the score and origin, and the neighbour's id, kind, title, excerpt and status. `--relation` filters; `--depth 2` is refused rather than answered with one hop. A missing id is exit `2`, never an empty list. |

**The write gate.** An `add` is activated immediately when the source is
high-authority (`user_saved`, `import`) or when `--confidence` is at least `0.75`.
Otherwise it lands in the review inbox as `pending_approval`, and **a pending memory
has no full-text index row, no links and no embedding** — it cannot be found by any
read path until it is approved. That is the whole point of the inbox.

**Validation is fail-open for metadata and fail-closed for kind.** A malformed
`--metadata` object degrades to `{}` with a warning on stderr and the memory is still
stored. An unknown `--kind` is rejected with exit `3`. Text *limits* are fail-closed and
shared by every door — title 300 characters, excerpt 600, body 1 MiB — so `add`,
`update`, `review approve` and `import` cannot disagree about what a memory may hold.

### Reading

| Command | Promise |
|---|---|
| `search` | Ranked. Reports which rung of the ladder answered, which legs ran, and how many rows were counted as retrievals. Never answers a vector-only request from the lexical leg: `--legs vector` with no embedder is exit `4`, not a lexical answer. |
| `get` | One memory with its body and metadata. A missing id is exit `2`, never an empty success. `--relations` adds one query and wraps the answer as `{memory, relations}` — the neighbourhood `relate` returns, beside the memory instead of instead of it. |
| `list` | **Unranked.** Ordered by strength by default, or by recency with `--recent`. `list` is not a search, and the two do not compete. An absent `--status` means `active`, so a bare `list` never mixes the review inbox into a library; `--status any` is how you ask for both. |

**Pinning and ordering.** A pinned memory sorts first under the default (strength)
ordering, because a pinned memory the user cannot find is a bug report. It does **not**
sort first under `--recent`: "newest first" with pinned rows hoisted to the top is not
newest first. That is a change from what an earlier draft of this document implied.

**The retrieval ladder.** A query is tried three ways, in order, and the first attempt
that returns anything wins:

| Rung | The query becomes |
|---|---|
| 1 | `"the whole query"` — the words as a phrase, in order |
| 2 | `"t1"* AND "t2"* …` — every term, prefix-matched |
| 3 | `"t1"* OR "t2"* …` — any term, prefix-matched |

Every term is **quoted**, so no input is FTS5 syntax: `NEAR`, `*`, `-`, `(`, `)` and
`"` in a query are text and not operators. Quoting is also why accents and CJK work —
the hosted product's equivalent strips everything outside `[A-Za-z0-9_]`, which deletes
a Japanese query outright and truncates `café` to `caf`.

A query that cannot match — `!!!`, or pure punctuation — is not an error. It returns
zero results at exit `0`, and `data.attempt` says which of the three facts it was:

```json
"attempt": {"outcome": "nothing_asked"}
"attempt": {"outcome": "exhausted", "query": "\"zzz\"* OR \"qqq\"*"}
"attempt": {"outcome": "matched", "rung": 2, "query": "\"deploy\"* AND \"fail\"*"}
```

`nothing_asked` is an empty query — no question was sent. `exhausted` is a corpus with
no answer, and it carries the last and most forgiving expression that was tried, which
is the diagnostic for "why did that not match". The distinction matters because the two
call for different next moves.

**Ranking.** Reciprocal rank fusion, `k = 60`, `score = Σ 1/(k + rank + 1)`, the same
formula the hosted product uses. **`score` is only comparable within one response**: it
is a function of position and not of similarity, so it is not a threshold, and the same
memory can score differently in two searches. It is omitted from human output for that
reason — the position already says it — and present in `--json` for callers that rank
or deduplicate across a batch.

**The window.** A search fetches the best `3 × limit` matches and cuts the page from
that window after ordering. Two consequences, both real and both documented rather than
discovered: a pinned memory ranked just outside the page can be hoisted onto it, and
`--sort recent` means "the newest of the top `3 × limit` matches", not "the newest
match in the corpus". `list` has no such rule — it is unranked and reads the corpus.

**Pinning.** A pinned result sorts first under `relevance` and `strength`, and **not**
under `recent`. "Newest first" with pinned rows hoisted to the top is not newest first.
This is the same rule `list` follows and for the same reason.

**Recording.** A search counts as a retrieval against every result it returns, unless
`--no-record` is passed. That counter is not telemetry: `retrieval_count` and
`last_retrieved_at` are the stretch factor on a memory's decay clock, so a memory that
keeps being found is a memory that keeps mattering. `--json` reports how many rows moved
in `data.recorded`, so a caller can tell a search that counted from one that did not.

**Snippets.** A snippet comes from the full-text index, with `**` around the matched
terms, and is **empty when the match was not in the body** — in the title, the tags or
the excerpt, all of which are already fields of the row. An empty snippet is honest; a
body opening that happens to sit under a title match reads as the place it matched.
`--snippet-len` sets how many full-text tokens of context it carries, and `--full` is
still what lets a long one through untruncated.

#### What the legs promise

| Leg | What it runs | What it costs |
|---|---|---|
| `fts` | The query ladder over FTS5, one statement per rung. Never touches an embedder, so it is the mode that cannot be slow because of the network. | Measured in §8 WP2 of the plan: 26 ms of work at ten thousand memories. |
| `vector` | One embed call for the query, then a brute-force cosine scan of every stored vector in scope. | Measured in §8 WP7: 63 ms of work at ten thousand memories (384 dimensions), most of it reading 15 MB of blobs. |
| `auto` (default) | Both, when an embedder is configured; the lexical leg alone when it is not. The two windows are fused with reciprocal rank, the platform's own rule: `1/(60+rank)` per leg, summed. | The sum of what ran. |

**A request for one leg is answered by that leg.** `--legs vector` with no embedder
configured, or with an embedder that does not answer, is exit `4` — never a lexical answer
the caller did not ask for and cannot detect. `--legs fts` never embeds anything, including
the query.

**`auto` degrades visibly, and only when something failed.** Three states, and the JSON says
which:

| `legs` | `degraded` | `reason` | What happened |
|---|---|---|---|
| `{"fts": true, "vector": false}` | `false` | `no_embedder` | No embedder is configured. This is the documented default, not a failure. |
| `{"fts": true, "vector": true}` | `false` | `""` | Both legs answered. |
| `{"fts": true, "vector": false}` | `true` | `no_query_vector` | An embedder is configured and the query could not be embedded. The answer is lexical, and stderr says so. |
| `{"fts": true, "vector": false}` | `true` | `vector_leg_failed` | The query embedded and the scan failed. |
| `{"fts": false, "vector": true}` | `false` | `vector_only` | A vector-only request, answered with the vector leg. |

`no_query_vector` and `vector_only` are the hosted door's own words, so a dashboard reading
both products has one vocabulary. `no_embedder` is local: a local brain with no model is a
full-text brain rather than a broken one, and calling that degraded would make the default a
warning.

**The vector leg has no similarity threshold, and one floor.** Candidates are ranked by
cosine similarity and cut to the window; a candidate whose similarity is `0.0` or less is not
a candidate at all, because a vector that points nowhere in common with the query carries no
information. There is deliberately no tuned floor like `0.3`: every real embedding model is
anisotropic, so a constant that separates signal from noise in one model throws away half the
corpus in another. A caller that wants a floor has `similarity` on every row it came from.

**The `similarity` field is not a score.** `score` is the fused rank — comparable only within
one response. `similarity` is a cosine in the model's own space, present only on rows the
vector leg ranked, and useful for one thing: telling a lexical hit from a semantic one.

### Retrieving

| Command | Promise |
|---|---|
| `retrieve <query>` | The packet: what to use for what you are about to do, rather than what matches. `--intent` decides which kind of memory leads and which memories fit when there are more candidates than the packet holds; `--constraint` (repeatable) says what the task is about, and is the only signal applicability is judged on. Nothing about the task is stored, and `retrieved_ids` is exactly what `outcome` wants fed back. |

**The intent is a re-ranking, not a filter.** Every intent retrieves the same candidates
for the same query; what changes is the weight of a kind and therefore, when the packet is
smaller than the pool, which candidates are in it. A packet built from a corpus that fits
the page is the same packet under any intent, apart from its summary.

**One procedure, and the facts it does not have room for.** A packet holds at most one
`procedure`: two competing routines side by side is a decision the caller cannot make
better than the engine can. The runner-up is still in `retrieved_ids`, and so is every
other memory that was injected and not bucketed.

**Nothing in a packet is inferred.** Claims come from the envelope the writer filled in,
confidence is the number the writer claimed (or the `0.5` default — which is *not* a low
confidence and is never warned about), and every warning is a fact already in a column:
expired, superseded, contradicted, or below the confidence threshold. There is no model on
this path, and there is nothing to configure.

**An expired memory is demoted, never dropped.** It is evidence about what was believed,
and the conflict it may be part of is exactly what a caller needs to see. `conflicts` is
empty when nothing contradicts anything — `link --relation contradicts` is what makes it
non-empty — and if the conflict check itself cannot run, that fact is reported in
`warnings` rather than left to look like an absence. The two are different facts, which is
why the second one is written down where a caller will read it.

### Lifecycle

| Command | Promise |
|---|---|
| `review list` | Lists what is waiting — newest first, with the confidence its writer claimed and where it came from. `count` is the whole inbox in scope; `items` is the page. |
| `review approve` | Activates a draft: status, index row and the provenance that was waiting with it, in one transaction. A draft is unreachable by `search`, `list` and `get` until this runs, and reachable immediately after. |
| `review reject` | **Archives** the draft. Nothing is deleted: rejecting is a decision made without much information, and `restore` undoes it. |
| `rate` | `+1` / `-1` per memory, folded into a running average. |
| `outcome` | The agent-facing credit call: one task outcome, up to 50 memories, each with an assessment. Recorded before it is applied, so every number that moved can be explained afterwards. |
| `decay` | `strength = importance · 0.5^(age_days / effective_half_life)`, half-lives `episodic 30d`, `procedural 90d`, `semantic 180d`, stretched by `min(1 + 0.1·retrievals, 2)`; below `0.05` archives, and so does a passed `expires_at`. `reference` never decays — it is the user's library, not a claim that gets less true; `pinned` is exempt. `--dry-run` scans everything and writes nothing. |
| `pin` / `unpin` / `archive` / `restore` | Reversible. Nothing here destroys anything. `archive` removes the index row in the same statement, so "hidden" and "unsearchable" are one fact. |
| `forget` | The only destructive verb. Deletes the row and everything keyed to it in one transaction. **Refuses without `--yes`, and never prompts** — a prompt is unusable from a script or an agent. |

**Nothing in the lifecycle is a hard failure for a missing id.** `approve`, `reject`,
`pin`, `unpin`, `archive` and `restore` report every id they left alone, with a reason
(`not found`, `wrong status`, `already there`, `outside the workspace`, `duplicate`), and
act on the rest. The exit code is `2` only when **nothing** moved: approving four drafts
of which one was already approved must not undo the other three, and a script that asked
for five things and got none of them wants to hear about it.

#### What a rating and an outcome actually move

This is the part of the lifecycle most likely to be assumed wrong, including by the first
draft of this project's own plan. Ratings and outcomes move **`confidence`**,
**`feedback_score`** and **`expires_at`**. They never touch `strength`: decay owns it and
recomputes it from `importance` on every pass, so a multiplier written by a rating would
survive exactly until the next `decay`.

| Assessment | `confidence` (from `c`, default `0.5` when nothing was claimed) | `expires_at` | feedback average | corroboration |
|---|---|---|---|---|
| `helpful` | `c + (1 - c) · 0.1` — diminishing, never certain | — | `+1` | `+1` |
| `unused` | **untouched** — an absence of information must not invent a claim | — | `0` | — |
| `misleading` | `max(0.05, c · 0.85)` | — | `-1` | — |
| `outdated` | `max(0.05, c · 0.9)` | **now** | `-1` | `+1` |

`rate` is the same running average without an assessment: `score = (score · n + rating) /
(n + 1)`, rounded to two places on every step, with `n = feedback_count`. Rounding each
step gives the average a fixed point — once a step would move it by less than half a
hundredth, it does nothing — which is the hosted product's arithmetic and is kept.

Both numbers are in every `get` and `rate` response, and **both are reported as-is**: a
score of `-0.33` over three ratings is not a percentage and is not normalised.

#### The review inbox

A draft is written by `add` when the write gate holds it back: `--source agent_reflection`
with `--confidence` below `0.75`, or any write into a workspace whose `review_mode` is
`review`. A draft has **no index row**, so it is invisible to `search` by construction
rather than by a filter somebody has to remember — and `kiomon doctor` would report the
index as *short* if a draft ever leaked into it.

Two things a draft keeps while it waits: its `derived_from` ids, which `add` does not wire
as edges (an edge from a memory retrieval cannot return would leak the draft's title
through `relate`), and its place in the inbox. `approve` wires the edges and clears the
staging column in the same transaction; `reject` discards them.

#### Decay in the background

`decay` is a command, not a daemon. `add` also nudges it — at most once a day, at most a
hundred rows, resuming from where the last nudge stopped and wrapping at the end of the
corpus — so that a brain nobody maintains still ages. `search` deliberately does **not**:
a read whose promise is that it answers a question should not spend fifty milliseconds
writing index pages, which is what the nudge costs on a ten-thousand-memory corpus.

The nudge is not the mechanism and cannot keep up with a large corpus; `kiomon decay` is,
and `kiomon doctor` reports how long it has been (and says `never run` when it has never
run). If the nudge changed anything it is reported on stderr, and `decay --dry-run` is how
you see what a full pass would do.

#### Semantic search, end to end

1. Put an `[embedder]` block in the config file (§[the config file](#the-config-file)).
   `kiomon doctor` prints the path it would read, the provider it found, the URL it will
   call, and how much of the corpus has a vector for the model in use.
2. New memories are embedded as they are written — `add`, `update`, and `review approve`
   (which repairs a draft whose embed failed at write time). **A failed embed never fails a
   write**: the memory is stored, found lexically, and the warning on stderr names
   `kiomon reindex --embed`.
3. Memories written *before* the embedder existed have no vector, and only
   `kiomon reindex --embed` can give them one. This is deliberate — embedding is work, and a
   sweep nobody asked for competes with the command the user is waiting on — and `doctor`
   reports the gap as `embedded/active` so it cannot be forgotten.
4. **One model at a time.** Vectors from two models are not comparable, and a mixed corpus
   does not error: it quietly gets worse. Every vector is keyed by model name, a search only
   ever reads the configured model's rows, and `reindex --embed` deletes any other model's
   rows as it goes. Switching models therefore costs a full re-embed, which is the honest
   price of a corpus that only ever contains one space.

### The graph

A **link** is a row in `memory_links`: a source, a target, a relation, who wrote it
(`origin`), how sure they were (`score`), and when. `relate` and `get --relations` read
them; `link` and `unlink` write them; `forget` removes every edge that touches a memory,
by foreign-key cascade. `add --ref` writes `derived_from`, and the auto-link pass writes
`related_to` (below).

**Symmetric relations are stored once.** `related_to` has no direction, so an edge is
written with the smaller id — the older memory, since ids sort by creation time — as its
source. `link A B` and `link B A` are the same row, and `relate` reads both columns, so the
canonical order is invisible to a caller. A directed relation is stored exactly as given.

**An edge is never written for a memory the inbox is holding.** A draft has no full-text
row, no embedding, and no links, in either direction: `link` refuses it (exit `2`, naming
the inbox), the auto-link pass only ever sees active memories, and `relate` filters a
pending neighbour out even if a row exists for it. The rule is what makes "a draft cannot be
found by any read path" a property of the engine rather than of every writer remembering.

**Auto-linking happens at activation.** When a memory is activated — by `add` or by
`review approve` — it is linked to at most **five** of its closest neighbours as
`related_to`, in the same transaction, with a score. The pass reads the title and the first
kilobyte of the body, takes their first twelve distinct terms, and ranks the corpus by
`bm25` with the same field weights `search` uses; a neighbour must keep at least **half** the
best match's score to be linked. The floor is relative because `bm25` is not comparable
across corpora — an absolute one would link everything in a small brain and nothing in a
large one. Three things follow from the rules rather than from taste: no self-edges, nothing
outside the memory's own workspace (an edge is a read path, and a workspace is a boundary),
and nothing at all for a memory whose words appear nowhere else.

Auto-linking is **not** re-run by `update`, is **not** run by `import` (a restore writes the
rows a file carries, and the file carries no edges), and is **not** run by the benchmark's
bulk corpus load. A brain restored from an export therefore has memories and no graph.

**A resume of `relate`'s fields**, because a caller cannot see them from `--help`:

| Field | Meaning |
|---|---|
| `memory_id`, `title`, `excerpt`, `kind`, `status` | The neighbour. An archived neighbour is shown with `archived`; a pending one is not shown. |
| `relation` | The edge's type. |
| `direction` | `out` when the memory asked about is the edge's source, `in` when the neighbour is. **`null` for `related_to`**, where the stored direction is a canonicalisation and not a fact. |
| `score` | The edge's similarity, when it has one. Auto-linked edges do. |
| `origin` | `manual` (`link`), `auto` (the pass), `agent` (`add --ref`). |
| `created_at` | When the edge was written. |

**What the packet does with the graph.** `retrieve` reads `contradicts` and `supersedes`
edges among its candidates: a contradiction becomes a `conflicts` entry naming both ids plus
a warning, and a supersession demotes the replaced memory and warns
`"…" is superseded by <id>; prefer the newer memory.` — demoted, never dropped, because a
caller that cannot see the replaced memory cannot see why it was replaced. Edges with a
`valid_to` are resolved and are not reported; nothing writes `valid_to` yet, so a
contradiction a caller wants to retract is retracted with `unlink`.

### Portability

| Command | Promise |
|---|---|
| `export` | Streams one memory per line, in `id` order, a page of five hundred at a time. The records are the **stored rows** — no derived labels, no nesting — and their field names are the memory object's own. `--format markdown` writes one document per memory into `--output <DIR>` instead, and refuses to write to stdout because a concatenation of documents is not markdown. An absent `--status` means `active`, so an export is a library's export and not the review inbox's; `--status any` is everything. |
| `import` | Reads what `export` wrote, from a file, a directory (walked, sorted, `*.jsonl`/`*.ndjson`/`*.json` and `*.md`), or `-` for stdin. Every line is validated and reported individually; `--dry-run` prints the counts and keeps nothing. |

**A line is the row.** Every field of the memory object from
[the search response](#a-search-response) except the derived labels, plus `workspace_id`,
the counters, `pending_derived_from` as `derived_from`, and `status`. A field that is
absent takes its column default; `kind` and `title` must be present, and a line without
them is an error naming the missing field.

```json
{"id":"k_01JC9W5K2Q…","workspace_id":"default","kind":"semantic","title":"Staging deploys need a manual migration","excerpt":"","body":"Run migrations before the deploy.","tags":["deploy"],"memory_metadata":{"domain":"infra"},"source":"user_saved","source_url":null,"confidence":0.9,"importance":0.5,"strength":0.84,"corroboration_count":1,"retrieval_count":3,"last_retrieved_at":"2026-10-14T09:12:00Z","feedback_score":0.0,"feedback_count":0,"pinned":false,"status":"active","expires_at":null,"created_at":"2026-10-14T09:12:00Z","updated_at":"2026-10-14T09:12:00Z"}
```

**Three things are deliberately not in an export**, and a caller that wants a *backup*
has to know all three:

- **Vectors.** A 384-dimensional vector is about 4 KB of base64, is valid only for the
  model that made it, and can be regenerated from the text. `import` embeds as it writes
  when an embedder is configured, and `kiomon reindex --embed` is what to run when it is
  not — a restored brain with no model finds nothing by meaning until then.
- **Edges.** A restore gives back every memory and none of their relationships, and
  `import` does not auto-link what it writes — a bulk restore is not an activation. So a
  brain that had a graph comes back without one, and the export says so (`edges_included:
  false`). This is a real gap and not a design statement: the format grows an edges
  section when the graph is worth carrying in a file, and the plan records it as a
  follow-up. A *draft's* staged provenance is a column rather than an edge, so it does
  travel, as `derived_from`.
- **Workspaces.** A record names its workspace id; a restored brain whose workspaces did
  not exist gets them created, in the default (autonomous) review mode, because the
  review mode is a property of the workspace row and not of a memory.

**The verdict on each line**, in the order the checks run: the id is already here
(`exists`), the *content* is already here under another id (`duplicate`), or the row is
written. That order is what makes re-importing the same file a no-op, and the content
check is what keeps two exports of one brain from doubling it.

**A malformed line is a message, not a failure.** `path:line: message` on stderr, with
the fragment that could not be read, and the run continues. Exit `3` only when *nothing*
could be read: a script has to be able to tell "five of a hundred landed" from "none
did", and only the summary can tell it the first.

**Markdown is a view, not a format.** `--format markdown` writes frontmatter with eleven
keys — `id`, `kind`, `title`, `workspace`, `status`, `tags`, `source`, `url`,
`confidence`, `importance`, `created` — and the body below it. Importing a note with no
frontmatter gives it a `reference` memory whose title is its first `# heading` (or its
filename), because that is what a directory of notes looks like. A value that would read
back as a list, a number or a boolean is quoted; everything else is bare.

**Limits, and why import has them.** A memory's title is at most **300 characters**, its
excerpt **600**, and its body **1 MiB** — on every door, `add` and `update` included, so
a memory the file may hold and the CLI may not is not a state this engine has. A line
longer than 2 MiB is refused and drained to its newline, so one absurd line cannot become
a flood of them.

### Measurement

| Command | Promise |
|---|---|
| `bench` | Measures **this build on this machine**, at corpus sizes you choose, against the performance budget in `plans/001-local-v1-core.md` §7. `--gate` applies the budget and exits `1` if any part of it is missed. It is the only command that exists for this project's benefit rather than a user's, and it ships so that the numbers in the README are ones you can falsify rather than ones you have to believe. |

**What a number means.** Every row is reported twice:

| Column | What it is |
|---|---|
| `total` | Wall clock for a cold process: spawn, open, work, exit. What a user pays. |
| `work` | `total` minus the spawn floor, measured moments before it. What the code is answerable for. |

The budget reads `work`, because the floor is a property of the operating system: about
9 ms on Windows against about 1 ms on Linux. A `open ≤ 5 ms` gate read on `total` is
unreachable on Windows before any code runs, and a gate read on `total` across two
platforms is comparing two operating systems.

**The corpus.** Generated, deterministic from a seed, and *described in the report*
rather than assumed: how many bodies are short, medium and long, and how many rows each
probe query actually matched. A query that matches everything measures a scan, and a
benchmark that does not say how many rows it ranked is a benchmark that cannot be
compared with another. Three probe terms are planted at known frequencies — one in seven,
one in forty, one in two hundred and fifty — plus a phrase, a Japanese term, an accented
term, and one term that is in nothing at all.

**The corpus's path is a measurement parameter.** `--corpus` defaults to the system temp
directory, and the report prints where it went, because a database's write cost can differ
by a factor of forty between two disks holding the same file. Two machines being compared
should be compared on the same kind of volume, and the report is how you can tell whether
they were.

**`--repeat`.** One pass is always run and discarded before the measured ones, which is
what makes every figure a warm-cache figure: building the corpus writes hundreds of
megabytes that the kernel is still flushing when the first sample would otherwise run.
`--repeat N` then runs the suite N times and reports each pass's median beside the verdict,
so a reader can see whether the drift is a machine heating up or something else running
on it.

**Exit code `1` on a missed gate**, which is the same code `doctor` uses for "the run
finished and the answer is no". CI fails on it.

**`--open`** opens the database and reports its path, its schema version and the process's
peak resident set where the platform reports one, then exits. It is the probe the harness
times to measure opening on its own, and it is public because a reader who wants to know
what opening their own brain costs should not have to read the harness to find out.

## Output shapes

### A search result

`search` and `list` return the same row; a search result is a list row with `score`,
`snippet` and sometimes `similarity` added, and the two are the same struct so they cannot
drift apart.

```json
{
  "id": "k_01JC9W5K2Q",
  "workspace_id": "default",
  "kind": "semantic",
  "kind_label": "Facts",
  "title": "Staging deploys need a manual migration",
  "excerpt": "…",
  "tags": ["deploy", "staging"],
  "source": "user_saved",
  "source_label": "saved",
  "source_url": null,
  "confidence": 0.9,
  "strength": 0.84,
  "pinned": false,
  "status": "active",
  "created_at": "2026-10-14T09:12:00Z",

  "snippet": "Run **migrations** before the deploy; the pipeline does not do it.",
  "score": 0.016393442,
  "similarity": 0.71
}
```

`similarity` is present only on a row the vector leg ranked, and only when the vector leg
ran. A row that only the lexical leg found has no cosine because nothing compared one.

`score` is the fused rank and is **relevance, not trust**. A hit with a `confidence`
carries the writer's estimate of how reliable it is; a hit without one (an imported
file) makes no such claim. Ranked order is not a statement about truth, and an agent
should hedge on a low-confidence hit rather than treat it as settled.

A `status` of anything but `active` can only appear on a `list`: a pending or archived
memory is not in the full-text index at all, and the leg filters for `active` as well,
so two independent mechanisms have to fail before a draft leaks into a search.

### A search response

`search`'s `data` is an object where `list`'s is an array, because a search has
something to say about how it answered and a list does not.

```json
{
  "ok": true,
  "data": {
    "results": [ …search result rows… ],
    "query": "why did the deploy fail",
    "attempt": {"outcome": "matched", "rung": 3, "query": "\"why\"* OR \"did\"* OR …"},
    "requested_legs": "auto",
    "legs": {"fts": true, "vector": true},
    "vector_model": "nomic-embed-text",
    "degraded": false,
    "reason": "",
    "recorded": 10
  },
  "meta": {"contract_version": "1", "version": "0.1.0"}
}
```

`requested_legs` and `legs` are both reported so a caller can tell what it asked for from
what ran. `legs` is an **object**, the hosted door's shape (`{fts, vector}`), because a single
string cannot tell "lexical only" from "both" apart without inventing a spelling — and this
field was a string before WP7, when there was one leg.

`vector_model` is present exactly when the vector leg ran, and `reason` is empty for a normal
answer and otherwise names the reduced path. The five states are the table under
[what the legs promise](#what-the-legs-promise); `no_embedder` is the one that is not a
degradation.

Each result row carries `similarity` **only** when the vector leg ranked it: see
[the vector leg's rules](#what-the-legs-promise) for why it is not a score.

### A listed memory

`list` returns the same shape as a search result, without `score` and without `snippet`:

```json
{
  "id": "k_01JC9W5K2Q",
  "workspace_id": "default",
  "kind": "semantic",
  "kind_label": "Facts",
  "title": "Staging deploys need a manual migration",
  "excerpt": "",
  "tags": ["deploy", "staging"],
  "source": "user_saved",
  "source_label": "saved",
  "source_url": null,
  "confidence": 0.9,
  "strength": 1.0,
  "pinned": false,
  "status": "active",
  "created_at": "2026-10-14T09:12:00Z"
}
```

The body and the metadata envelope are **not** in a list row — a caller browsing does not
need them, and an agent paying for tokens certainly does not. `get` is how you ask for the
whole memory.

### A context packet

`retrieve` returns this as `data`, byte-shaped like the hosted `/api/retrieve` response,
because the published Python and TypeScript SDKs already parse it:

```json
{
  "summary": "1 fact, 1 procedure, 1 episode matched intent \"execute\".",
  "facts": [
    {
      "memory_id": "k_01JC9W5K2Q",
      "kind": "semantic",
      "claim": "migrations are manual on staging",
      "confidence": 0.9,
      "authority": "user_saved",
      "applicability": "applicable"
    }
  ],
  "procedure": {
    "memory_id": "k_01JC9W5K3R",
    "kind": "procedural",
    "claim": "run the deploy script → run the migration",
    "confidence": 0.5,
    "authority": "user_saved",
    "applicability": "conditional",
    "condition": "only when the schema changed"
  },
  "episodes": [],
  "evidence": [
    {
      "memory_id": "k_01JC9W5K4S",
      "source_uri": "https://example.com/runbook",
      "source_span": "page 12"
    }
  ],
  "warnings": [],
  "conflicts": [],
  "retrieved_ids": ["k_01JC9W5K2Q", "k_01JC9W5K3R", "k_01JC9W5K4S"]
}
```

`procedure` is present only when a routine is in the packet, `condition` only on a
conditionally applicable item, and `source_ref` only when the memory points somewhere — the
key is **absent**, not `null`, which is what the wire does and what a strict decode
expects.

`claim` is the most specific line the writer stored: a semantic memory's first claim, a
routine's steps joined with `→`, an episode's outcome, and the title otherwise. `authority`
is the memory's `source`, verbatim. `applicability` is `applicable` when at least half of
the constraint tokens appear in the memory or the passage that matched, `conditional` when
some do, and `unknown` when none do or when no constraints were given — a packet does not
guess what a task is about.

### An added memory

```json
{"id":"k_01JC9W5K2Q","workspace_id":"default","status":"active","duplicate":false,"warnings":[]}
```

`duplicate: true` means nothing was written and `id` is the memory that already existed.
It is a success, and it is the answer to "my agent saved this twice". **The status travels
with it** because a duplicate that is `pending_approval` or `archived` is a duplicate the
caller has to act on, and a reply saying only `duplicate: true` leaves it with no lever.

`warnings` carries anything the write discarded — metadata of the wrong shape, a
`derived_from` id that does not exist — and the same lines are printed on stderr.

### What `add` does with a bad envelope

Metadata that does not typecheck **degrades to `{}` and the memory is still written**,
with the reason in `warnings`. Losing an agent's careful `entities` list is annoying;
losing the memory it was attached to, because the list was a string instead of an array,
would be much worse. `--metadata` and the individual `--entities`/`--claims`/`--domain`
flags are mutually exclusive: two ways to say the same thing, never both.

### An error

See [the envelope](#the-json-envelope). `error.message` is for a human; branch on
`error.code` and the exit code, never on the message text.

## Stability

| | |
|---|---|
| Adding a field to `data` | Not a breaking change. |
| Removing or renaming a field | Breaking. Needs a version bump. |
| Adding a command or a flag | Not breaking. |
| Changing an exit code, or a `error.code` value | Breaking. |
| Changing output on the *human* path | Not breaking, but say so in the pull request anyway — people grep it. |

## What is not in version 1

`bench` is a developer tool that ships. It measures this machine, not a promise: the
numbers in the README are one laptop's, they are labelled as such, and `kiomon bench` is
how you get yours.

- `kiomon mcp` (stdio MCP server) and `kiomon serve` (loopback HTTP), together in v1.1.
  `mcp` wraps *this* contract, and the tool names will be the command names. `serve`
  answers the hosted `/api/*` routes instead, so the published Python and TypeScript SDKs
  work unchanged against a local brain — see `plans/001-local-v1-core.md` §9-D8 and
  Appendix C. Routes this engine cannot answer (`revert`, briefings) return a typed
  `unsupported_locally` error rather than an empty success.
- **A graph leg inside a search.** The graph is here — see [the graph](#the-graph) — and
  `search --legs graph` refuses with a message that says so. What is v1.1 is the walk as a
  *ranking* leg: a PageRank over the seed set, fused as a third RRF list (§4.3 of the plan).
  A one-hop read and a ranked walk are different operations, and this contract will not
  answer the second with the first.
- **Edges in an export.** `edges_included: false`, because a restored brain has no graph.
  `link` is how one is put back, by hand, until the format carries edges.
- URL fetching, PDF parsing, connectors, and every LLM-backed feature. See
  `plans/001-local-v1-core.md` §10 for the trigger that would change each one.
