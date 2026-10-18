# Changelog

<!-- SPDX-License-Identifier: Apache-2.0 -->

Every release that changes what a caller can rely on gets an entry here. The format is
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/)'s, and the versions are
[semver](https://semver.org/): the JSON contract has its own number (see
`docs/CONTRACT.md`), and it only changes when a field is renamed, removed, or re-typed.

## [Unreleased]

## [0.1.0] — 2026-10-18

The first release. One binary, one SQLite file, no account, no network, no daemon.

### Added

- **The store.** `init`, `doctor`, `stats`. Schema v3, migrations applied in order, a
  database that is a single file you can copy, `sqlite3` into, and back up.
- **Writing.** `add` (with `--ref`, `--metadata`, `--entities`/`--claims`/`--domain`,
  `--tag`, `--url`, `--confidence`, `--source`, `--force`, and a body from a flag, a file,
  or stdin), `update`. Content-hash deduplication, kind-aware validation, and a review gate
  that keeps an unapproved memory out of every read path.
- **Reading.** `search` (ranked, three-rung ladder over quoted queries, `--kind`, `--sort`,
  `--limit`, `--snippet-len`, `--legs`, `--no-record`), `list` (unranked, `--tag`,
  `--status`, `--recent`), `get`. `--full` is global: it prints whole field values instead
  of truncating them to fit a line.
- **The context packet.** `retrieve` — a search's better half: `--intent
  execute|plan|answer|verify|debug`, `--constraint`, and one packet with facts, an
  ordered procedure, episodes, evidence, warnings and the ids that produced it.
- **Hybrid retrieval.** A vector leg beside the lexical one, fused with reciprocal rank
  fusion. Bring your own embedder: Ollama, any OpenAI-compatible endpoint, or a
  deterministic stub for tests. `reindex --embed` to catch up a brain.
- **The lifecycle.** `review` (an inbox with `pending_approval`), `rate`, `outcome`,
  `decay` (`--dry-run`), `pin`, `archive`, `restore`, `forget`.
- **Portability.** `export` to JSONL or a directory of markdown, `import` from either or
  from stdin, with ids, statuses and timestamps preserved.
- **Measurement.** `bench` — generated corpora, cold-process timings, the plan's budget
  as gates, and a `--json` report that says which build produced it.
- `install.sh` and `install.ps1`: one command from a release archive, checksum verified.

### Not in this release

- **A direct way to set an expiry.** `expires_at` exists, `decay` acts on it, the packet
  warns about it, and a rating, an outcome or an import can write it — but `add --ttl` does
  not exist, so a user cannot say "this is true for a week" at the moment they save it. The
  flag belongs with the hosted write shape rather than invented beside it; it is recorded in
  the plan as a follow-up.
- **The graph.** `link`, `unlink`, `relate` and `get --relations` exit `1` and say they
  are deferred to the next sprint. Nothing can write a relation, so nothing reads one.
- **A server.** `kiomon serve` (loopback HTTP matching the hosted `/api/*` contract) and
  `kiomon mcp` (stdio) are v1.1. Today every command is a process.
- **Bundled embeddings.** The vector leg needs a model you already run; `native-embed`
  (an in-process ONNX model) is a reserved feature, not an implemented one.
- **TLS.** The embedder speaks plain HTTP only — local runtimes on localhost. A hosted
  endpoint needs a build with the TLS stack, which the dependency budget does not carry
  by default.

[Unreleased]: https://github.com/kiomon-hq/kiomon/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/kiomon-hq/kiomon/releases/tag/v0.1.0
