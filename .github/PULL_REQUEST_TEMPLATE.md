<!--
  Keep this short. A pull request a reviewer has to scroll is a pull request a reviewer
  skims, and this repository's changes are argued in the body or not at all.
-->

## What changes, and why

<!--
  The problem, then the approach. If this touches a deliberate cut in
  plans/001-local-v1-core.md, say which argument you are engaging with.
-->

Closes #

## Checks

- [ ] `cargo test` passes, and a behaviour change has a test that fails without it
- [ ] `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` pass
- [ ] New comments explain *why*; fallible functions have `/// # Errors`; getters are `#[must_use]`
- [ ] No `unwrap` outside tests and fixtures (and there only with an `expect` that says what held)
- [ ] If this is a CLI change: `docs/CONTRACT.md` is updated, and the pull request says whether it is breaking

## If it changes behaviour

- [ ] `CHANGELOG.md` has an entry under `## [Unreleased]`
- [ ] Nothing on a default path makes a network call, adds an async runtime, or grows the binary past the budget (`cargo bench` in CI reports the size)

## Licensing

- [ ] My contribution is licensed under Apache-2.0 (see [CONTRIBUTING.md](../CONTRIBUTING.md#licensing)), and files I added carry `SPDX-License-Identifier: Apache-2.0` on the first line
