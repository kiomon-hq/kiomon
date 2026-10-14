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
| `2` | Not found: the memory, workspace, or relation does not exist. | The thing is gone. A `get_memory`-style caller may treat it as "archived or deleted". |
| `3` | Invalid arguments: unknown flag, conflicting options, a bad value, a missing required one. | Fix the command line. Retrying is pointless. |
| `4` | The request was valid and the environment cannot serve it: no embedder configured for `--legs vector`, an unwritable database directory, an unparseable config file. | Fix the environment or the configuration. |

`2` and `1` are deliberately distinct. An agent that receives `2` can conclude a
memory no longer exists; an agent that receives `1` should try again later.

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
`unused` is a no-op: an omitted memory is untouched, and so is one reported as unused.

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
| `search` | Ranked. Reports which retrieval legs ran and, when a leg degraded, says so rather than silently substituting another. Never answers a vector-only request from the lexical leg. |
| `get` | One memory with its body and metadata. A missing id is exit `2`, never an empty success. `--relations` adds one query. |
| `list` | **Unranked.** Ordered by strength by default, or by recency with `--recent`. `list` is not a search, and the two do not compete. An absent `--status` means `active`, so a bare `list` never mixes the review inbox into a library; `--status any` is how you ask for both. |

**Pinning and ordering.** A pinned memory sorts first under the default (strength)
ordering, because a pinned memory the user cannot find is a bug report. It does **not**
sort first under `--recent`: "newest first" with pinned rows hoisted to the top is not
newest first. That is a change from what an earlier draft of this document implied.

**The retrieval ladder.** A query is tried three ways and the first attempt with a
non-empty result wins: the literal phrase, then all terms prefix-matched (`AND`),
then any term prefix-matched (`OR`). The attempt that answered is reported in
`--json`, so a caller can tell "nothing matches" from "nothing matches all of it".

**Ranking.** Reciprocal rank fusion, `k = 60`, `score = Σ 1/(k + rank)`. Pinned
memories sort first under every `--sort`. A search records a retrieval against each
result it returns unless `--no-record` is passed — that counter is what stretches a
memory's decay clock, so it is mechanism rather than telemetry.

### Lifecycle

| Command | Promise |
|---|---|
| `review list` | Lists what is waiting. `review approve` activates: it writes the index row, the links and the embedding in one transaction with the status flip. |
| `outcome` | Records credit before applying it. `helpful` strengthens, `misleading` and `outdated` weaken, `outdated` also expires. Every recorded outcome is auditable. |
| `decay` | `strength = importance · 0.5^(age_days / effective_half_life)`, half-lives `episodic 30d`, `procedural 90d`, `semantic 180d`, stretched by `min(1 + 0.1·retrievals, 2)`; below `0.05` archives. `reference` never decays; `pinned` is exempt. `--dry-run` writes nothing. |
| `pin` / `unpin` / `archive` / `restore` | Reversible. Nothing here destroys anything. |
| `rate` | `+1` strengthens, `-1` weakens. |
| `forget` | The only destructive verb. Deletes the row and everything keyed to it in one transaction. **Refuses without `--yes`, and never prompts** — a prompt is unusable from a script or an agent. |

### Portability

| Command | Promise |
|---|---|
| `export` | Streams JSONL (or markdown) as it reads, so a large brain exports without loading into memory. Exported ids are stable and are preserved by `import`. |

## Output shapes

### A search result

```json
{
  "id": "k_01JC9W5K2Q",
  "kind": "semantic",
  "kind_label": "Facts",
  "title": "Staging deploys need a manual migration",
  "excerpt": "…",
  "snippet": "…run **migrations** before the deploy…",
  "tags": ["deploy", "staging"],
  "source_url": null,
  "source_label": "saved",
  "confidence": 0.9,
  "strength": 0.84,
  "pinned": false,
  "status": "active",
  "score": 0.0317,
  "created_at": "2026-10-14T09:12:00Z"
}
```

`score` is the fused rank and is **relevance, not trust**. A hit with a `confidence`
carries the writer's estimate of how reliable it is; a hit without one (an imported
file) makes no such claim. Ranked order is not a statement about truth, and an agent
should hedge on a low-confidence hit rather than treat it as settled.

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

- `kiomon mcp` (stdio MCP server) — v1.1. It wraps this contract, and the tool names
  will be the command names.
- A daemon or HTTP server — v1.2.
- URL fetching, PDF parsing, connectors, and every LLM-backed feature. See
  `plans/001-local-v1-core.md` §10 for the trigger that would change each one.
