// SPDX-License-Identifier: Apache-2.0

//! The `kiomon` binary.
//!
//! This file is the whole of the process's interaction with the outside world: it parses,
//! dispatches, and turns a [`kiomon_core::Error`] into a stream and an exit code.
//! Everything else is in `kiomon-core`, which is why an MCP server or a daemon can be
//! added later as another `main.rs` rather than as a refactor.
//!
//! A command returns an [`kiomon_core::ExitCode`] as well as raising errors, because some
//! runs succeed and still conclude badly — `doctor` after finding a corrupt index,
//! `forget` when none of the ids existed. Those need a code *and* their normal output, and
//! raising an error would print a second document on the `--json` path.

// `unreachable_pub` is a library lint: it exists to catch a `pub` item that no downstream
// crate can reach. This package is a binary, so there is no external surface and every
// `pub` would fire. The lint stays on in `kiomon-core`, where it means something. Cargo
// does not allow overriding one workspace lint in a member manifest alongside
// `lints.workspace = true`, so the exemption lives here.
#![allow(unreachable_pub)]

mod cli;
mod commands;
mod out;

use std::process::ExitCode;

use clap::Parser;

use cli::Cli;

/// What this build is: the release number, and the commit inside it.
///
/// The release number is a promise about the wire; the commit is a promise about the
/// binary. Both are needed to answer "is this the build with the fix in it", and neither
/// is enough alone. `build.rs` stamps [`env!("KIOMON_GIT_SHA")`] — or `unknown` when the
/// source had no repository, as a crates.io tarball does not.
///
/// It reaches three places, on purpose: `--version`, every JSON response's `meta.version`,
/// and the bench report's header. An integration that logs one response, and a CI artifact
/// that records one number, should each name the commit that produced them.
pub const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (git ",
    env!("KIOMON_GIT_SHA"),
    ")"
);

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // clap's exit code for a usage error is 2, which this project's contract already
        // spends on "not found" — see `kiomon_core::ExitCode`. Catching the error is the
        // only way the documented table stays true.
        Err(err) => return out::parse_failure(&err),
    };

    match commands::run(&cli) {
        Ok(code) => ExitCode::from(code.as_u8()),
        Err(err) => out::report(cli.json, &err),
    }
}
