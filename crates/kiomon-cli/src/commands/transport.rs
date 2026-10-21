// SPDX-License-Identifier: Apache-2.0

//! `serve` and `mcp`: the two transports, as commands.
//!
//! Both are thin on purpose. What they decide is where the database comes from and whether the
//! process is quiet; everything about *what* a request means is in [`crate::service`], and
//! everything about *how* it is framed is in [`crate::http`] and [`crate::mcp`]. That split is
//! the whole reason the two transports cannot drift: the only code they share is the layer that
//! answers questions, and neither of them owns an answer.

use crate::cli::{Cli, McpArgs, ServeArgs};
use crate::http::{self, ServeOptions};
use crate::service::Service;
use crate::{mcp, out};

/// `kiomon serve`
///
/// # Errors
///
/// [`kiomon_core::Error::Config`] when no database path can be resolved or, with
/// `--require-token`, the token file cannot be written; and [`kiomon_core::Error::Io`] when the
/// port cannot be bound.
pub fn serve(cli: &Cli, args: &ServeArgs) -> kiomon_core::Result<kiomon_core::ExitCode> {
    let service = Service::new(cli.db.clone(), cli.config.clone());
    // One line on stderr, before the handshake, because "which brain am I serving" is the
    // question a user asks when two terminals are open and only one of them is answering. It
    // names the database rather than the directory: `--db` is the flag that gets left off.
    if !cli.quiet {
        let db = kiomon_core::config::resolve_db_path(cli.db.as_deref())?;
        out::warn(false, &format!("serving {}", db.display()));
    }
    http::run(
        &service,
        &ServeOptions {
            port: args.port,
            require_token: args.require_token,
            once: args.once,
        },
        cli.json,
        cli.quiet,
    )
}

/// `kiomon mcp`
///
/// # Errors
///
/// [`kiomon_core::Error::Io`] when stdin or stdout cannot be read or written.
pub fn mcp(cli: &Cli, _args: &McpArgs) -> kiomon_core::Result<kiomon_core::ExitCode> {
    let service = Service::new(cli.db.clone(), cli.config.clone());
    mcp::serve(&service, cli.quiet)
}
