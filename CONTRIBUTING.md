# Contributing to Kiomon Local

Thanks for taking the time to contribute.

This repository is a Rust CLI and a Rust library. It is small on purpose, and the
reason it is small is a performance claim that has to be measured, not asserted.

## Before you start

- **Bug reports and feature requests** — open an issue using a template. The
  templates ask for the detail that makes a report actionable.
- **Security vulnerabilities** — do **not** open a public issue. Follow
  [SECURITY.md](SECURITY.md).
- **Large changes** — open an issue first. The scope in
  [`plans/001-local-v1-core.md`](plans/001-local-v1-core.md) §1 and §10 is
  deliberate: features are cut to protect the latency budget, and a change that
  adds a dependency, an async runtime, or a network call on a default path needs to
  argue against the budget rather than around it.

## Making a change

1. Fork the repository, branch from `main`.
2. Make the change, with tests. A behaviour change without a test will be asked
   for one.
3. Run the same checks CI runs — from the repository root:

   ```sh
   cargo fmt --check
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```

4. Open a pull request against `main` with a short description of the problem and
   the approach. Link the issue it closes, if there is one.

`main` is protected: CI green and a review before merge. Rebase rather than
merging `main` into your branch.

## Conventions

**Everything is in English.** Code, comments, commit messages, issues, pull requests.

**Comments explain why, not what.** The code says what it does. A comment earns its
place by recording the reason: the constraint that forced the shape, the tempting
alternative that was wrong, the failure it prevents. This repository's comments
carry more weight than usual because it has a sibling product with different
constraints, and the next reader will otherwise "fix" a deliberate local decision
back into the hosted shape.

**The CLI contract is a contract.** [`docs/CONTRACT.md`](docs/CONTRACT.md) is what
agent integrations are written against. Adding a field is fine; changing or removing
one, or changing an exit code, is a breaking change and needs a version bump and a
note in the pull request.

**`kiomon-core` never prints and never exits.** No `println!`, no `eprintln!`, no
`process::exit`, no `clap`. Progress, warnings and diagnostics are returned to the
caller or surfaced as errors; `kiomon-cli` is the only place that writes to stdout,
stderr, or the exit code. That rule is what makes `kiomon mcp` (v1.1) and a future
`kiomon serve` one `main.rs` each instead of a refactor.

**Dependencies are a budget.** The default build has a target of fewer than 60
direct and transitive crates. A new dependency is a pull-request discussion, and
`cargo deny` will not let a licence through that is not on the allow list.

**Test names state the behaviour**, not the function. If a test cannot be named
without "and", it is two tests.

### Style

| | |
|---|---|
| Formatting | `rustfmt` (see `rustfmt.toml`; `max_width = 100`) |
| Linting | `cargo clippy --all-targets -- -D warnings`, workspace lints in `Cargo.toml` |
| Minimal version | `rust-version` in `Cargo.toml`; CI checks it |
| Tests | unit tests beside the code, integration tests in `crates/*/tests/` |

## Commit messages

A short imperative subject line, then a blank line and a body explaining the change
and why it is needed.

```text
Index a memory only when it activates

`add` inserted the FTS row in the same transaction as the memory, which put
pending_approval drafts into every search the user ran. A review inbox that
leaks into retrieval is worse than no review inbox, so indexing now happens in
`approve` and the pending path writes the row without a trigger firing.
```

## Releasing

A release is a tag and nothing else: `git tag -a v0.1.0 -m "0.1.0" && git push origin
v0.1.0` runs `.github/workflows/release.yml`, which verifies the tagged commit (including
the performance budget), builds five targets on native runners, checksums the archives,
publishes the GitHub Release, and publishes `kiomon-core` and then `kiomon-cli`.

Four things to check before tagging, all of which the workflow checks again:

1. `CHANGELOG.md` has a `## [x.y.z]` section. The workflow extracts it as the release body.
2. `kiomon --version` reports `x.y.z (git <sha>)` — the version *and* the commit, because a
   bug report needs both. The build stamp comes from `crates/kiomon-cli/build.rs`.
3. `cargo package -p kiomon-core --allow-dirty --no-verify` succeeds, and
   `cargo package -p kiomon-cli --list --allow-dirty` lists what you expect. The CLI cannot
   be packaged before its dependency is on crates.io, so its full package is verified by the
   release workflow and not before the first release.
4. The installers still work against a real archive. Both halves of that are local:

```sh
cargo build --release
version=0.1.0
mkdir -p "dist/download/v$version"
bash scripts/package.sh --version "$version" --target x86_64-unknown-linux-gnu   --binary target/release/kiomon --out dist
mv dist/kiomon-*.tar.gz "dist/download/v$version/"
(cd "dist/download/v$version" && sha256sum kiomon-* | sort -k2 > SHA256SUMS)

KIOMON_RELEASES_URL="file://$PWD/dist" bash install.sh --version "v$version" --dry-run
```

Then run it without `--dry-run` and check `--version` on what it installed. `curl` reads a
`file://` URL like any other, so this exercises the real download, the real checksum
comparison and the real extraction — everything except GitHub's redirect to its CDN.

`KIOMON_RELEASES_URL` is read by `install.sh` for exactly this reason: a mirror, an
air-gapped install, and a test of the installer itself are the same feature. `install.ps1`
takes `-ReleasesUrl` and `-Dir`.

## Licensing

By contributing you agree that your contribution is licensed under the repository's
licence, **Apache-2.0**. There is no separate contributor licence agreement and no
copyright assignment.

Files carry an SPDX identifier on the first line:

```rust
// SPDX-License-Identifier: Apache-2.0
```

Do not add `Copyright (c) … All rights reserved.` headers — that is the hosted
product's convention and it is wrong for this repository.
