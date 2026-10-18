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

**Pre-alpha, and it works.** `init`, `doctor`, `stats`, `add`, `update`, `export`,
`import`, `get`, `list`, `search`, `retrieve`, `reindex`, `review`, `rate`, `outcome`,
`decay`, `pin`, `archive`, `restore` and `forget` all run end to end against a real SQLite
file. The core loop — save something, find it again by a sentence or by meaning, ask for
what to use next, tell it whether what you found helped, let what nobody uses fade, and get
the whole thing out as a file you own — is complete, with one deliberate hole: the graph.
`link` and `relate` are not in this release and say so along with the sprint that owns
them (§9-D9 of the plan).

The work packages in [`plans/001-local-v1-core.md`](plans/001-local-v1-core.md) §8 are
being executed in order, and the plan records where reality contradicted it rather than
quietly diverging.

If you want the finished thing today, the hosted product is at
[kiomon.com](https://kiomon.com). This repository is the open option, and it deliberately
does far less.

## The numbers

A local memory system is only worth using if it is not the slow part of the loop. These
are measured by `kiomon bench`, which ships in the binary, so you can run it against your
own machine instead of believing this one.

Read it the way the plan's §7 budget is read: **`work` is `total` minus the process-spawn
floor**, because on Windows spawning the process at all costs about 8 ms and on Linux about
1 ms. The budget is written against `work`, since a gate read on `total` compares operating
systems rather than code.

One developer machine — Intel Core i5-9300H, Windows, `--release`, warm cache — with a
**generated corpus of 10 000 memories, 197 MiB**, bodies 60 % short / 30 % medium / 10 %
long, and one memory in seven containing the word `deploy`.

| | work p50 | work p95 | total p50 |
|---|---|---|---|
| `kiomon --version` (the spawn floor, no work at all) | — | — | 8.2 ms |
| `kiomon bench --open` | 2.0 ms | 5.2 ms | 10.2 ms |
| `kiomon add` | 3.0 ms | 6.6 ms | 11.2 ms |
| `kiomon get <id>` | 2.5 ms | 6.1 ms | 10.6 ms |
| `kiomon list` (20) | 2.9 ms | 7.7 ms | 11.1 ms |
| `kiomon stats` | 4.4 ms | 8.8 ms | 12.4 ms |
| `kiomon search "timeout"` (40 rows matched) | 13.3 ms | 19.8 ms | 21.6 ms |
| `kiomon search "flaky"` (250 rows) | 15.5 ms | 21.3 ms | 23.7 ms |
| **`kiomon search "deploy"` (1 446 rows)** | **26.8 ms** | **38.9 ms** | 34.9 ms |
| `kiomon search "deploy" --no-record` | 18.0 ms | 26.3 ms | 26.4 ms |
| `kiomon search "deploy" --limit 50` | 21.9 ms | 33.1 ms | 30.5 ms |
| `kiomon search "deploy" --legs vector` (384 dims) | 61.1 ms | 98.2 ms | 72.4 ms |
| **`kiomon search "deploy"` (both legs, stub embedder)** | **81.6 ms** | **123.5 ms** | 92.0 ms |
| `kiomon retrieve "deploy"` (context packet, lexical) | 24.1 ms | 37.3 ms | 35.8 ms |

Binary **4.0 MiB** from **49** transitive crates. The plan's gates are `open` p50 ≤ 5 ms,
`add` p95 ≤ 40 ms, `search` p95 ≤ 60 ms (lexical) and ≤ 250 ms (hybrid) at ten thousand
memories: all met, with the first write to a brand-new database file the one number that is
genuinely expensive — **16.6 ms**, against 3 ms for the next write into the same file.

The two vector rows come from a later session than the rest of the table, on the same
machine — its spawn floor was 10.9 ms against 8.2 ms, which is the drift the harness prints
the floor to expose.

Both are measured with the built-in **stub** embedder — a hashed bag of words:
microseconds, deterministic, no network. That is the point of it: what those rows measure is
this program's work (one query embedding, a scan of every stored vector, one fusion) and not
a model's latency, which is a property of whatever server you point it at. At ten thousand
memories the vector leg is 61 ms of work and almost all of it is SQLite handing over 15 MB of
blobs; four million multiply-adds cost about 2 ms. It scales with the corpus, so the 250 ms
ceiling is crossed somewhere around 40 000 memories — an extrapolation from one size, and the
next measurement that matters is `kiomon bench --sizes 100000`.

Four things worth knowing before quoting any of these:

- **A search's cost is the number of rows it has to rank, and `--limit` does not bound it.**
  Forty matches cost 13 ms and 1 446 cost 27 ms, while `--limit 50` costs about the same as
  `--limit 10` — the window is what the ranking reads, not the page. At **100 000
  memories** (1.9 GiB) a common term matches 14 457 rows and takes **187 ms**. The plan's
  60 ms gate is written for ten thousand, where it holds; the number to watch as a brain
  grows is the match count, not the row count.
- **`kiomon search` records what it returned, and that is a write.** It costs 8.8 ms of the
  26.8, and that counter is what stretches a memory's decay clock. `--no-record` is there
  for callers that only want to look.
- **The corpus is generated, not collected.** It models size, distribution and
  selectivity — not English, and not the vocabulary you actually write down.
- **The volume matters more than the machine.** The same binary on the same database file
  spent 47–145 ms on its first write on one drive and 2.9–5.9 ms on another. `kiomon bench`
  prints the corpus's path for exactly this reason, and its default is the system volume.

One more caveat that is about the machine and not the code: **p95 on a general-purpose
computer measures the computer.** On this laptop a 200 ms interruption arrives roughly once
in a few hundred spawns and lands on `--version` — which opens nothing and prints one line —
as readily as on a search. That is why the gates are read at two hundred samples per row
(with ten samples of slack at the 95th percentile) and why the medians are the numbers to
compare. `kiomon bench --repeat 3` prints each pass's median beside the verdict.

## What it does

| | |
|---|---|
| **Four memory kinds** | `reference` · `semantic` · `procedural` · `episodic` — Files, Facts, Routines, Moments — each with the metadata that makes it retrievable (domain, entities, claims, source refs, timestamps, triggers). |
| **Retrieval that copes with what you actually type** | *(WP2)* A sentence is tried three ways — the phrase, then all its terms prefix-matched, then any of them — and the first that finds anything wins. Every term is quoted, so punctuation is never an error and a Japanese or accented query is never mangled. `bm25()` weights title over tags over excerpt over body, and results come back with **the match in context**. |
| **The context packet** | `retrieve` answers a different question from `search`: not *what matches* but *what should I use, given what I am about to do*. Declare one of five intents (`execute`, `plan`, `answer`, `verify`, `debug`) and get back the applicable facts, **one** best procedure, the relevant episodes, evidence pointers, and warnings about what not to trust — expired, low confidence, superseded, contradictory. `claim` is the most specific line the writer stored, never a model's summary of it, and `constraints` are the only thing applicability is judged on. It is the payload both published SDKs lead with, and on a 10 000-memory corpus it costs about what the search under it costs (18 ms of work against 17 ms, retrieval record off). |
| **Hybrid retrieval** | The same ranking fused with a vector leg by reciprocal rank (`k = 60`), on the formula the hosted product uses. **Bring your own model**: point `[embedder]` at Ollama or any OpenAI-compatible server and every write embeds itself; nothing is downloaded, no ONNX runtime is linked, and a brain with no `[embedder]` block is a full-text brain rather than a broken one. `--legs fts` never touches the network, `--legs vector` exits `4` with the config key to add rather than being quietly answered from the lexical leg, and `auto` says *why* when the vector leg did not run. `kiomon reindex --embed` backfills what predates the embedder, and `doctor` reports the coverage gap. |
| **Lifecycle** | Confidence, importance, strength, pin, archive, rating, and honest decay — unpinned memories weaken on a per-kind half-life (`episodic 30d`, `procedural 90d`, `semantic 180d`, stretched up to 2× by use), and pass below `0.05` into the archive rather than into oblivion. `reference` memories never decay: a file you saved is not a claim that gets less true. Decay is a command, not a daemon, and `add` nudges it at most once a day; `doctor` tells you when it has not run. |
| **Review inbox** | Agent writes below the confidence threshold land pending until approved, and **a pending memory is not in the index at all** — a review inbox that leaks into retrieval is worse than none. `review approve` activates and indexes it, `review reject` archives it rather than deleting it, and a draft's `--derived-from` provenance waits on the row until it is allowed to become an edge. |
| **Knowledge graph** | **Not in v1** — deferred to the next sprint (§9-D9 of the plan). The schema is here and `--ref` already writes a `derived_from` edge, but `link`, `unlink`, `relate` and the auto-link pass are not, and they say *deferred* rather than *coming in this release*: an agent that waits for a package it cannot look up waits for nothing. |
| **Feedback** | `rate` and `outcome` credit a memory's **confidence** and a running feedback score, so the corpus gets sharper instead of just bigger. `outcome` takes a task's end state and up to fifty assessments (`helpful`, `unused`, `misleading`, `outdated`), records every one of them before applying it, and expires what an agent reports as stale. **Neither verb touches `strength`** — decay owns that number, and a multiplier written by a rating would not survive one pass. |
| **Workspaces** | isolation is a `WHERE` clause. `--workspace` stays where you point it; `--workspace '*'` is an explicit opt-in to every workspace. |
| **Agent-grade CLI** | every command takes `--json`, exit codes are stable and documented, and diagnostics never pollute stdout — so `kiomon search … \| jq` always works. A search reports which of its three query forms answered, and which legs ran, so an agent can see *why* it got what it got. |
| **Measured, not asserted** | `kiomon bench` measures this build on your machine: cold-process timings per command, the corpus's shape, the process-spawn floor, and the plan's budget applied as a pass or a fail. It ships in the binary so the numbers above are falsifiable. |
| **Yours** | `export` to JSONL (one memory per line, the stored row, nothing derived) or to a directory of markdown documents, and `import` back from either — a file, a directory of notes, or stdin. **Ids, status and timestamps are the file's**, so a restore is a restore, and re-importing the same file changes nothing. Line errors are reported with their line number and the rest of the file still imports. What is *not* in an export is stated rather than implied: no vectors (they are model-specific and `import` re-embeds as it writes), no edges (there is no graph yet), and `vectors_included: false` in every summary. |

## What it deliberately is not

- **Not a cloud client.** No network call on any default path. The one optional
  exception is the embedder you configure yourself, which is off unless you set it — and
  which this build reaches over **plain HTTP only** (`http://`), because TLS in the default
  build costs about twenty transitive crates against the plan's budget of sixty. Ollama, LM
  Studio, `llama.cpp`, vLLM and text-embeddings-inference all serve plain HTTP on localhost;
  an `https://` URL is refused by name with the workaround.
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

[`docs/DESIGN.md`](docs/DESIGN.md) is the long form of why the machine is shaped this
way — the rules each decision protects, the storage layout, the two read paths, what is
deliberately absent, and the numbers that settled each argument. The scope cut, the schema,
the retrieval ladder, the decay formula, the performance budget, and what was rejected are
in **[`plans/001-local-v1-core.md`](plans/001-local-v1-core.md)**, whose work-package
records are the archaeology behind both.

## Install

One command, from a release archive, checksum verified:

```sh
# Linux and macOS
curl -fsSL https://raw.githubusercontent.com/kiomon-hq/kiomon/main/install.sh | sh

# Windows, in PowerShell
irm https://raw.githubusercontent.com/kiomon-hq/kiomon/main/install.ps1 | iex
```

Both take `--dry-run` to print what they would do first, which is what you should run
before piping anything from the internet into a shell. Prebuilt for Linux (x86-64, ARM64),
macOS (Apple silicon, Intel) and Windows (x86-64); `install.sh --target <triple>` stages a
build for another platform. From source:

```sh
cargo install --path crates/kiomon-cli        # or: cargo build --release
```

## Quickstart

```sh
kiomon init                               # creates the database and the default workspace
kiomon add --kind semantic --title "Staging deploys need a manual migration"            --body "Run migrations before the deploy; the pipeline does not do it."            --tag deploy --tag staging --domain infra

kiomon search "why did the deploy fail"   # ranked, with the match in context

# Optional: semantic search, over a model you already run.
# Put this in the config file `kiomon doctor --paths-only` prints:
#   [embedder]
#   provider = "ollama"
#   model = "nomic-embed-text"
# Then backfill what you already had, and search by meaning:
kiomon reindex --embed
kiomon search "the rollout went sideways" # finds it without sharing a word
kiomon search "café" --kind semantic      # accents and CJK work
kiomon list                               # unranked: strongest first, one line each
kiomon get k_01JC9W5K2Q                   # the whole memory
kiomon export --output brain.jsonl        # the brain as a file you own
kiomon import --db other.db brain.jsonl   # and back, ids and statuses intact
kiomon doctor                             # integrity, schema version, index agreement
kiomon stats
kiomon bench --gate                       # measure this machine against the budget
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
