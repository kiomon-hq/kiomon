# Agent Guidelines — Kiomon Local

Instructions for coding agents working in this repository. Human contributors should
read [CONTRIBUTING.md](CONTRIBUTING.md) instead; it is the same rules with more prose.

## What this repository is

The open, local option of the hosted Kiomon product: one Rust binary, one SQLite
file, no network, no LLM, no account. The scope is argued in
[`plans/001-local-v1-core.md`](plans/001-local-v1-core.md) §1; the cuts in §10 are
deliberate and are not backlog items to be picked up opportunistically.

## Non-negotiable rules

1. **`kiomon-core` never prints and never exits.** No `println!`, `eprintln!`,
   `process::exit`, `std::env::args`, or `clap` in `crates/kiomon-core`. It returns
   values and `Result`s. `crates/kiomon-cli` is the only place that touches stdout,
   stderr, or the exit code.
2. **No network call on a default path.** The only permitted outbound request is the
   embedder the user explicitly configured, on the path they explicitly asked for.
3. **No async runtime.** Every v1 operation is local I/O bound by one SQLite file.
4. **Do not port `platform/` blindly.** The hosted product's shape is an artifact of
   being a multi-tenant, multi-service pipeline. `plans/001-local-v1-core.md`
   Appendix A lists what is ported verbatim (formulas, thresholds, vocabulary) and
   what is not. A change that mirrors `platform/` needs a reason that survives the
   local constraints.
5. **Comment the why.** Not what the code does — the constraint that forced the
   shape, or the failure the code prevents. A comment that restates the next line
   will be removed.
6. **`docs/CONTRACT.md` is versioned.** Adding a field is fine. Changing or removing
   one, or changing an exit code, is a breaking change to every agent integration
   and needs a version bump.
7. **Any claim in `README.md` is measured or is labelled a target.** The "three
   numbers" are targets until WP3's bench harness produces numbers with a corpus and
   a machine named.

## Building and checking

From the repository root, and these are exactly what CI runs:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

`cargo clippy --all-targets -- -D warnings` must be green before you call any work
done. Workspace lints are set in `Cargo.toml` and `pedantic` is on.

## Where things live

| Path | What belongs there |
|---|---|
| `crates/kiomon-core/` | All behaviour. Library only. |
| `crates/kiomon-core/migrations/` | Numbered `NNNN_slug.sql`, forward-only, never edited after landing. |
| `crates/kiomon-cli/` | Argument parsing, output shaping, exit codes. Thin. |
| `benches/` | The ruler. The performance gates in `plans/001-local-v1-core.md` §7. |
| `docs/CONTRACT.md` | The agent-facing CLI contract. |
| `plans/` | The argument for a change, written before it. |

## Working through the plan

`plans/001-local-v1-core.md` §8 is the work order. Each package has acceptance
criteria that a reviewer checks — they are not a description of the work, they are
the evidence for it.

- Do one package at a time and leave the repository green.
- A package's acceptance criteria are the definition of done, not a suggestion. If
  one cannot be met, say so and why; do not quietly redefine it.
- Update the plan when a package's own census contradicts it. That is expected — the
  hosted repository's plan index records three corrections from one such pass — and
  the plan is more valuable corrected than defended.
- Do not start WP4 before WP3. The ruler has to exist before there is anything worth
  measuring.
