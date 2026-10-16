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
| `2` | Not found, or nothing to do: the memory, workspace, or relation does not exist — or a lifecycle verb named ids that are not in a state where it means anything (`pin` on a pinned memory, `restore` on an active one). | The thing is gone, or already the way you wanted it. Either way, retrying changes nothing. |
| `3` | Invalid arguments: unknown flag, conflicting options, a bad value, a missing required one. | Fix the command line. Retrying is pointless. |
| `4` | The request was valid and the environment cannot serve it: no embedder configured for `--legs vector`, an unwritable database directory, an unparseable config file. | Fix the environment or the configuration. |

`1` is also what `bench --gate` returns when a performance gate is missed: the run
succeeded and the answer is no.

`2` and `1` are deliberately distinct. An agent that receives `2` can conclude a
memory no longer exists; an agent that receives `1` should try again later.

`4` is for a request whose *shape* was valid. Today the only thing that raises it is
`--legs vector`, because this build has no embedder: the message names what to do
instead, since an exit code that is a wall is not an answer. `--legs vector` is never
answered from the lexical leg — the platform learned that the expensive way, and
returning something other than what was asked for is undetectable by the caller.

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
| `--full` | Print whole field values instead of truncating them to fit a line. |

All four are accepted before *or* after the subcommand.

**Human output is bounded** so a four-kilobyte body cannot become a four-kilobyte line in
an agent's context window: `list` truncates each title to 120 characters unless `--full`
is passed, and `get` truncates the body the same way. The `--json` path is never
truncated — a machine asked for the field and gets the field.

**Environment.** `$KIOMON_DB` overrides the default database path and `$KIOMON_WORKSPACE`
overrides what an absent `--workspace` means — still exactly one workspace, never all of
them. There is no config file yet; the embedder (v1.1) is the first thing that needs one.

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

**Relations** — `derived_from` · `supports` · `contradicts` · `related_to` ·
`precedes` · `follows` · `part_of`. All are directed except `related_to`, which is
symmetric and therefore stored once.

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

### Writing

| Command | Promise |
|---|---|
| `add` | One transaction. Either the memory exists and is indexed, or it was a duplicate and the existing id is returned with `duplicate: true` and exit `0`, or nothing was written. An identical memory is never stored twice unless `--force`. |
| `update` | Re-indexes when a field retrieval can see — title, excerpt, body, tags — changed. A no-op update is not an error. |
| `import` | Streams. A malformed line is reported on stderr with its number and skipped; valid lines are still imported. Exit is `1` if any line failed, `0` if none did. |
| `link` / `unlink` | Idempotent. Linking twice is not an error. |
| `relate` | Returns 1-hop neighbours by default. Depth is bounded. |

**The write gate.** An `add` is activated immediately when the source is
high-authority (`user_saved`, `import`) or when `--confidence` is at least `0.75`.
Otherwise it lands in the review inbox as `pending_approval`, and **a pending memory
has no full-text index row, no links and no embedding** — it cannot be found by any
read path until it is approved. That is the whole point of the inbox.

**Validation is fail-open for metadata and fail-closed for kind.** A malformed
`--metadata` object degrades to `{}` with a warning on stderr and the memory is still
stored. An unknown `--kind` is rejected with exit `3`.

### Reading

| Command | Promise |
|---|---|
| `search` | Ranked. Reports which rung of the ladder answered, which legs ran, and how many rows were counted as retrievals. Never answers a vector-only request from the lexical leg: `--legs vector` with no embedder is exit `4`, not a lexical answer. |
| `get` | One memory with its body and metadata. A missing id is exit `2`, never an empty success. `--relations` adds one query. |
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

### Portability

| Command | Promise |
|---|---|
| `export` | Streams JSONL (or markdown) as it reads, so a large brain exports without loading into memory. Exported ids are stable and are preserved by `import`. |

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

`search` and `list` return the same row; a search result is a list row with `score` and
`snippet` added, and the two are the same struct so they cannot drift apart.

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
  "score": 0.016393442
}
```

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
    "legs": "fts",
    "degraded": false,
    "recorded": 10
  },
  "meta": {"contract_version": "1", "version": "0.1.0"}
}
```

`requested_legs` and `legs` are both reported so a caller can tell what it asked for
from what ran. With no embedder configured, `--legs auto` runs the lexical leg — that
is the documented default and `degraded` stays `false`, because falling back to the only
leg there is was not a degradation. `degraded` exists for the vector leg's reduced scan
and is a constant `false` until there is an embedder.

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
- URL fetching, PDF parsing, connectors, and every LLM-backed feature. See
  `plans/001-local-v1-core.md` §10 for the trigger that would change each one.
