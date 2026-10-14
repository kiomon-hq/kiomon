# Kiomon Local

**A persistent memory for your coding agent, in one binary and one SQLite file.**

`kiomon` gives an agent somewhere to put what it learns and a way to get it back in
a few milliseconds — no account, no API key, no network, no server.

```sh
kiomon add --kind semantic --title "Staging deploys need a manual migration" \
           --body "Run migrations before the deploy; the pipeline does not do it." \
           --tag deploy --tag staging --domain infra

kiomon search "why did the deploy fail"
```

```text
1. Staging deploys need a manual migration                        [semantic]  0.0317
   Run migrations before the deploy; the pipeline does not do it.
   tags: deploy, staging · id k_01JC9W5K2Q
```

That is the whole product: **save once, query forever**, locally.

---

## Status

**Pre-alpha.** The plan is written and decided
([`plans/001-local-v1-core.md`](plans/001-local-v1-core.md)); the work packages in
§8 are being executed in order. Nothing below is shipped yet.

If you want the finished thing today, the hosted product is at
[kiomon.com](https://kiomon.com). This repository is the open option, and it
deliberately does far less.

## The numbers this project is trying to earn

A local memory system is only worth using if it is not the slow part of the loop.
These are **targets, not measurements** — the bench harness (WP3) is what turns
them into claims, and the README will carry measured numbers with the corpus and
the machine named. Until then, treat them as the budget the design is protecting.

| | Target, 10k memories, process spawn included |
|---|---|
| `kiomon search` | p50 ≤ 25 ms · p95 ≤ 60 ms |
| `kiomon add` | p50 ≤ 15 ms · p95 ≤ 40 ms |
| Resident memory | ≤ 25 MB |
| Stripped binary | ≤ 6 MB |

## What it does

| | |
|---|---|
| **Four memory kinds** | `reference` · `semantic` · `procedural` · `episodic` — Files, Facts, Routines, Moments — each with the metadata that makes it retrievable (domain, entities, claims, source refs, timestamps, triggers). |
| **Hybrid retrieval** | SQLite FTS5 `bm25()` fused with an optional vector leg by reciprocal rank (`k = 60`), on the same formula the hosted product uses. |
| **Lifecycle** | confidence, importance, strength, pin, archive, rating, and honest decay — unpinned memories weaken on a per-kind half-life, so the brain forgets what it never uses. |
| **Review inbox** | agent writes below the confidence threshold land pending until approved, and **pending memories are invisible to search** — a review inbox that leaks into retrieval is worse than none. |
| **Knowledge graph** | typed edges (`derived_from`, `supports`, `contradicts`, `related_to`, `precedes`, `follows`, `part_of`) plus a cheap auto-link pass on every write. |
| **Feedback** | `rate` and `outcome` credit move a memory's strength, so the corpus gets sharper instead of just bigger. |
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

Not yet installable — WP1 lands the first runnable commands. When it does:

```sh
kiomon init
kiomon doctor          # integrity, schema version, config
kiomon --help
```

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
| Database | `$KIOMON_DB`, else the platform data directory, else `~/.kiomon/brain.db` |
| Config | `~/.kiomon/config.toml` |
| Override once | `--db <path>` |

Flags beat the environment, the environment beats the config file.

## Contributing

Issues and pull requests are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) and
the [Code of Conduct](CODE_OF_CONDUCT.md). Report security issues privately per
[SECURITY.md](SECURITY.md), never in a public issue.

## Licence

[Apache-2.0](LICENSE). The hosted Kiomon product is separate and is not covered by
this licence.
