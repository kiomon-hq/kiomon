# Kiomon Local

**A persistent memory for your coding agent, in one binary and one SQLite file.**

`kiomon` gives an agent somewhere to put what it learns and a way to get it back in
milliseconds — no account, no API key, no network, no server.

```sh
kiomon add --kind semantic --title "Staging deploys need a manual migration" \
           --body "Run migrations before the deploy; the pipeline does not do it." \
           --tag deploy --tag staging --domain infra

kiomon search "why did the deploy fail"
```

```text
1. k_01JC9W5K2Q  Facts     Staging deploys need a manual migration  · deploy, staging
   Run **migrations** before the **deploy**; the pipeline does not do it.
```

That is the whole product: **save once, query forever**, locally.

---

## Status

**Pre-alpha, and it works.** `init`, `doctor`, `stats`, `add`, `update`, `get`, `list`,
`search` and `forget` all run end to end against a real SQLite file, and the core loop —
save something, find it again by a sentence — is complete. The lifecycle verbs, the graph
and the vector leg are not, and each says so along with the work package that owns it.

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
about 9 ms.

One developer machine — Intel Core i5-9300H, Windows, `--release`, warm OS cache,
**10 000 memories of about a kilobyte** (47 MiB), one in seven of them mentioning
`deploy`.

| | p50 | p95 |
|---|---|---|
| `kiomon --version` (the spawn floor) | 8.9 ms | 11.0 ms |
| `kiomon search "deploy"` | 26.0 ms | 31.8 ms |
| `kiomon search` (a whole sentence, five terms) | 30.8 ms | 35.6 ms |
| `kiomon search --limit 50` | 48.1 ms | 63.7 ms |
| `kiomon list` (20) | 12.6 ms | 16.2 ms |
| `kiomon stats` | 14.1 ms | 21.0 ms |
| `kiomon get <id>` | 12.8 ms | 15.9 ms |
| `kiomon add` | 12.5 ms | 14.8 ms |

Binary **2.9 MiB** from **33** transitive crates. The plan's budget is `search` p95 ≤ 60 ms
and `add` p50 ≤ 15 ms: both hold for the default page, and `--limit 50` crosses the gate at
p95 because the ranked window grows with the page. That is written down rather than rounded
off.

Three caveats, because a number without them is a claim:

- **There is no bench harness yet** — that is WP3, and these are one machine's numbers
  rather than a benchmark.
- **The corpus is generated, not collected.** It gives each memory its own vocabulary out
  of a three-thousand-word pool, so a query for a common term matches one memory in seven.
  The first attempt drew every body from the same twenty-nine words and measured a corpus
  in which every term matched every memory — which cannot tell a ranking from a scan. See
  the plan.
- **The volume matters more than the machine.** The same binary on the same database file
  spent 47–145 ms on its first write on one drive and 2.9–5.9 ms on another. These numbers
  are from the fast one.

## What it does

| | |
|---|---|
| **Four memory kinds** | `reference` · `semantic` · `procedural` · `episodic` — Files, Facts, Routines, Moments — each with the metadata that makes it retrievable (domain, entities, claims, source refs, timestamps, triggers). |
| **Retrieval that copes with what you actually type** | *(WP2)* A sentence is tried three ways — the phrase, then all its terms prefix-matched, then any of them — and the first that finds anything wins. Every term is quoted, so punctuation is never an error and a Japanese or accented query is never mangled. `bm25()` weights title over tags over excerpt over body, and results come back with **the match in context**. |
| **Hybrid retrieval** | *(WP7)* The same ranking fused with an optional vector leg by reciprocal rank (`k = 60`), on the formula the hosted product uses. `--legs vector` exits `4` and names what to pass instead — it is never quietly answered from the lexical leg. |
| **Lifecycle** | *(WP4)* confidence, importance, strength, pin, archive, rating, and honest decay — unpinned memories weaken on a per-kind half-life, so the brain forgets what it never uses. |
| **Review inbox** | agent writes below the confidence threshold land pending until approved, and **a pending memory is not in the index at all** — a review inbox that leaks into retrieval is worse than none. Approval itself is WP4; the guarantee is WP1's and is tested. |
| **Knowledge graph** | *(WP5)* typed edges (`derived_from`, `supports`, `contradicts`, `related_to`, `precedes`, `follows`, `part_of`) plus a cheap auto-link pass on every write. `--ref` already writes `derived_from`. |
| **Feedback** | *(WP4)* `rate` and `outcome` credit move a memory's strength, so the corpus gets sharper instead of just bigger. |
| **Workspaces** | isolation is a `WHERE` clause. `--workspace` stays where you point it; `--workspace '*'` is an explicit opt-in to every workspace. |
| **Agent-grade CLI** | every command takes `--json`, exit codes are stable and documented, and diagnostics never pollute stdout — so `kiomon search … \| jq` always works. A search reports which of its three query forms answered, and which legs ran, so an agent can see *why* it got what it got. |
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

kiomon search "why did the deploy fail"   # ranked, with the match in context
kiomon search "café" --kind semantic      # accents and CJK work
kiomon list                               # unranked: strongest first, one line each
kiomon get k_01JC9W5K2Q                   # the whole memory
kiomon doctor                             # integrity, schema version, index agreement
kiomon stats
```

Every command takes `--json`, and `--quiet` on a command that has nothing to say. A `2`
means the memory is gone, a `1` means try again, and a `4` means this build cannot serve
the request — with a message saying why. `docs/CONTRACT.md` is the whole of it.

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
