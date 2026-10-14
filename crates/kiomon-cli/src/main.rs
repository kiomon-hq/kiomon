// SPDX-License-Identifier: Apache-2.0

//! The `kiomon` binary.
//!
//! This file is the whole of the process's interaction with the outside world: it
//! parses, dispatches, and turns a [`kiomon_core::Error`] into a stream and an exit
//! code. Everything else is in `kiomon-core`, which is why an MCP server or a daemon
//! can be added later as another `main.rs` rather than as a refactor.

// `unreachable_pub` is a library lint: it exists to catch a `pub` item that no
// downstream crate can reach. This package is a binary, so there is no external
// surface and every `pub` would fire. The lint stays on in `kiomon-core`, where it
// means something. Cargo does not allow overriding one workspace lint in a member
// manifest alongside `lints.workspace = true`, so the exemption lives here.
#![allow(unreachable_pub)]

mod cli;
mod commands;
mod out;

use std::process::ExitCode;

use clap::Parser;

use cli::Cli;

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // clap's exit code for a usage error is 2, which this project's contract
        // already spends on "not found" — see `kiomon_core::ExitCode`. Catching the
        // error is the only way the documented table stays true.
        Err(err) => return out::parse_failure(&err),
    };

    match commands::run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => out::report(cli.json, &err),
    }
}
