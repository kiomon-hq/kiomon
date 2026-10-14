# Kiomon Local

**A persistent memory for your coding agent, in one binary and one SQLite file.**

`kiomon` gives an agent somewhere to put what it learns and a way to get it back in
milliseconds — no account, no API key, no network, no server.

```sh
kiomon add --kind semantic --title "Staging deploys need a manual migration" \
           --body "Run migrations before the deploy; the pipeline does not do it." \
           --tag deploy --tag staging --domain infra

kiomon list
```

```text
k_01JC9W5K2Q  Facts     Staging deploys need a manual migration  · deploy, staging
```

That is the whole product: **save once, query forever**, locally.

---

## Status

**Pre-alpha, and usable.** `init`, `doctor`, `stats`, `add`, `update`, `get`, `list` and
`forget` work end to end against a real SQLite file. `search` does not yet — it is WP2,
and until then the store is complete and the ranking is not.

The work packages in [`plans/001-local-v1-core.md`](plans/001-local-v1-core.md) §8 are
being executed in order, and the plan records where reality contradicted it rather than
quietly diverging.

If you want the finished thing today, the hosted product is at
[kiomon.com](https://kiomon.com). This repository is the open option, and it deliberately
does far less.

## The numbers

A local memory system is only worth using if it is not the slow part of the loop. These
are measured, not asserted, and they are read the same way the plan's §7 budget is read:
**subtract the process-spawn floor**, because on Windows spawning the process at all costs
8.6 ms.

One developer machine — Intel Core i5-9300H, Windows, `--release`, warm OS cache,
**5 000 memories with 5 KB bodies** (43.5 MiB).

| | p50 | p95 |
|---|---|---|
| `kiomon --version` (the spawn floor) | 8.6 ms | 11.3 ms |
| `kiomon add` | 12.5 ms | 14.8 ms |
| `kiomon get <id>` | 11.6 ms | 14.7 ms |
| `kiomon list` (20) | 14.8 ms | 25.3 ms |
| `kiomon stats` | 14.3 ms | 19.5 ms |
| `kiomon init` (re-open) | 12.1 ms | 16.3 ms |
| `kiomon list --workspace '*'` | 55.7 ms | 91.9 ms |

Binary **2.9 MiB**, **33** transitive crates, ~8.7 KiB stored per 5 KB memory. There is no
bench harness yet (WP3), these are one machine's numbers rather than a benchmark, and the
slow row is a known one rather than a surprise: `list --workspace '*'` has no index that
matches its ordering. The plan records it, and the fix it would need.

## What it does

| | |
|---|---|
| **Four memory kinds** | `reference` · `semantic` · `procedural` · `episodic` — Files, Facts, Routines, Moments — each with the metadata that makes it retrievable (domain, entities, claims, source refs, timestamps, triggers). |
| **Hybrid retrieval** | *(WP2)* SQLite FTS5 `bm25()` fused with an optional vector leg by reciprocal rank (`k = 60`), on the same formula the hosted product uses. |
| **Lifecycle** | *(WP4)* confidence, importance, strength, pin, archive, rating, and honest decay — unpinned memories weaken on a per-kind half-life, so the brain forgets what it never uses. |
| **Review inbox** | agent writes below the confidence threshold land pending until approved, and **a pending memory is not in the index at all** — a review inbox that leaks into retrieval is worse than none. Approval itself is WP4; the guarantee is WP1's and is tested. |
| **Knowledge graph** | *(WP5)* typed edges (`derived_from`, `supports`, `contradicts`, `related_to`, `precedes`, `follows`, `part_of`) plus a cheap auto-link pass on every write. `--ref` already writes `derived_from`. |
| **Feedback** | *(WP4)* `rate` and `outcome` credit move a memory's strength, so the corpus gets sharper instead of just bigger. |
| **Workspaces** | isolation is a `WHERE` clause. `--workspace` stays where you point it; `--workspace '*'` is an explicit opt-in to every workspace. |
| **Agent-grade CLI** | every command takes `--json`, exit codes are stable and documented, and diagnostics never pollute stdout — so `kiomon search | jq` always works. |
| **Yours** | `export` to JSONL and `import` back. The brain is a file you own and can read. |

## What it deliberately is not

- **Not a cloud client.** No network call on any default path. The one optional
  exception is the embedder you configure yourself, which is off unless you set it.
- **Not an LLM.** No tagging, no extraction, no summarisation, no chat. The agent
  supplies `entities`, `claims`, and `domain` when it writes — which is exactly what
  the hosted product's `draft_memory` tool asks for to skip its paid pass.
- **Not a notes app.** No editor, no reader mode, no browser extension, no dashboard.
- **Not multi-tenant.** One person, one machine, one file. There is no `user_id` and
  no sync.
- **Not a port of the hosted platform.** The hosted product is a multi-service
  pipeline with queues, a vector database, connectors, and a model provider. Almost
  none of that is needed for the part users feel.

## Design

```text
  kiomon-cli         clap parser · output shaping · exit codes
      │              (the only place that prints or exits)
      ▼
  kiomon-core        Database · Store · Search · Lifecycle · Links · Gate · Decay · Embed
      │              (no clap, no print!, no process::exit — so `kiomon mcp` is one main.rs)
      ▼
  rusqlite (bundled SQLite) ──▶  ~/.kiomon/brain.db
```

WAL journaling, `mmap`, a 16 MB page cache, prepared statements, and one
transaction per command. No async runtime: every v1 operation is local I/O bound by
a single file, and a runtime would cost binary size and cold-start variance for
nothing.

The full argument — the scope cut, the schema, the retrieval ladder, the decay
formula, the performance budget, and what was rejected — is in
**[`plans/001-local-v1-core.md`](plans/001-local-v1-core.md)**.

## Quickstart

Not yet packaged — build it from source for now (`cargo build --release`). There is no
`init` step: the first write creates the database.

```sh
kiomon add --kind semantic --title "Staging deploys need a manual migration"            --body "Run migrations before the deploy; the pipeline does not do it."            --tag deploy --tag staging --domain infra

kiomon list            # newest and strongest, one line each
kiomon get k_01JC9W5K2Q --relations
kiomon doctor          # integrity, schema version, what the index thinks it holds
kiomon stats
```

Every command takes `--json`, and `--quiet` on a command that has nothing to say.

## Building

```sh
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## Where it lives

| | |
|---|---|
| Database | `--db <PATH>`, else `$KIOMON_DB`, else the platform data directory, else `~/.kiomon/brain.db` |
| Default workspace | `$KIOMON_WORKSPACE`, else `default` |

Flags beat the environment. There is no config file yet — the embedder (v1.1) is the first
thing that needs one, and it will bring a real TOML dependency rather than a hand-rolled
subset.

## Contributing

Issues and pull requests are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) and
the [Code of Conduct](CODE_OF_CONDUCT.md). Report security issues privately per
[SECURITY.md](SECURITY.md), never in a public issue.

## Licence

[Apache-2.0](LICENSE). The hosted Kiomon product is separate and is not covered by
this licence.
