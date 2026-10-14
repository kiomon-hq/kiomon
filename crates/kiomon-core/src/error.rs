// SPDX-License-Identifier: Apache-2.0

//! Errors, and the exit codes they map to.
//!
//! The exit code is part of the CLI contract (`docs/CONTRACT.md`), so the mapping
//! lives here, beside the variants, rather than in the binary. A code decided at the
//! call site is a code that eventually gets the wrong value in one place.
//!
//! `Error` deliberately carries no `anyhow` and no backtrace: this is a local tool
//! whose failures are almost always the user's argument or their filesystem, and a
//! chain of `caused by:` lines is noise in a terminal. The one place a chain earns
//! its keep is a SQLite error, which arrives in WP1 as its own variant with its own
//! message.

use std::fmt;

/// A process exit code.
///
/// These values are a **stable contract**: agent integrations branch on them, so
/// they change only with a version bump and a note in `docs/CONTRACT.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ExitCode {
    /// The command succeeded.
    ///
    /// This includes a search that matched nothing, and an `add` that found an
    /// existing identical memory. Neither is a failure, and reporting either as one
    /// teaches an agent to retry work that already happened.
    Success = 0,
    /// The command failed for a reason the user cannot fix by changing the command.
    Internal = 1,
    /// The memory, workspace, or relation asked for does not exist.
    ///
    /// Distinct from [`ExitCode::Internal`] on purpose: an agent that gets a `2` can
    /// conclude the memory is gone, while a `1` tells it to try again later. The
    /// hosted product's own agent instructions already read a 404 that way, and the
    /// distinction has to survive locally.
    NotFound = 2,
    /// The arguments were rejected: unknown flag, conflicting options, a bad
    /// `--kind`, a malformed `--item`, a missing required flag.
    ///
    /// clap's own exit code for this is also `2`, which is why `kiomon-cli` catches
    /// parse failures and remaps them — see `out::parse_failure`.
    InvalidArgs = 3,
    /// The request was valid but the environment cannot serve it: no embedder
    /// configured for `--legs vector`, an unwritable database directory, a
    /// configuration file that does not parse.
    Config = 4,
}

impl ExitCode {
    /// The process exit status as a `u8`.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

impl From<ExitCode> for std::process::ExitCode {
    fn from(code: ExitCode) -> Self {
        Self::from(code.as_u8())
    }
}

impl fmt::Display for ExitCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Success => "success",
            Self::Internal => "internal",
            Self::NotFound => "not_found",
            Self::InvalidArgs => "invalid_args",
            Self::Config => "config",
        })
    }
}

/// Everything that can go wrong in the engine.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The memory, workspace or relation asked for does not exist.
    #[error("{0}")]
    NotFound(String),

    /// The request was well-formed and is not valid.
    #[error("{0}")]
    Invalid(String),

    /// The environment cannot serve a valid request.
    #[error("{0}")]
    Config(String),

    /// A code path that a work package owns but has not written yet.
    ///
    /// This variant exists so the skeleton binary is honest rather than broken: every
    /// command in `plans/001-local-v1-core.md` §8 is declared from WP0, and each one
    /// names the package that implements it. It is gone by the end of WP8.
    #[error("`{command}` is not implemented yet (work package {wp} of plans/001-local-v1-core.md)")]
    Unimplemented {
        /// The subcommand, as typed.
        command: &'static str,
        /// The work package that implements it.
        wp: u8,
    },

    /// An internal invariant was broken. Always a bug in this crate.
    #[error("internal error: {0}")]
    Internal(String),

    /// The filesystem said no.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    /// The stable machine-readable code, used as the `error.code` field of `--json`
    /// output and asserted by tests.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "not_found",
            Self::Invalid(_) => "invalid_args",
            Self::Config(_) => "config",
            Self::Unimplemented { .. } => "unimplemented",
            Self::Internal(_) | Self::Io(_) => "internal",
        }
    }

    /// The process exit code this error maps to.
    #[must_use]
    pub const fn exit_code(&self) -> ExitCode {
        match self {
            Self::NotFound(_) => ExitCode::NotFound,
            Self::Invalid(_) => ExitCode::InvalidArgs,
            Self::Config(_) => ExitCode::Config,
            Self::Unimplemented { .. } | Self::Internal(_) | Self::Io(_) => ExitCode::Internal,
        }
    }
}

/// The crate's result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_contract_codes_are_the_documented_ones() {
        // These numbers are in docs/CONTRACT.md and in agent integrations. Asserting
        // them here means a change to the enum is a failing test rather than a silent
        // break for every caller.
        assert_eq!(ExitCode::Success.as_u8(), 0);
        assert_eq!(ExitCode::Internal.as_u8(), 1);
        assert_eq!(ExitCode::NotFound.as_u8(), 2);
        assert_eq!(ExitCode::InvalidArgs.as_u8(), 3);
        assert_eq!(ExitCode::Config.as_u8(), 4);
    }

    #[test]
    fn not_found_is_distinguishable_from_internal() {
        let missing = Error::NotFound("k_01ABC".into());
        let broken = Error::Internal("boom".into());
        assert_ne!(missing.exit_code(), broken.exit_code());
        assert_eq!(missing.exit_code(), ExitCode::NotFound);
        assert_eq!(broken.exit_code(), ExitCode::Internal);
    }

    #[test]
    fn every_error_codes_matches_its_exit_code() {
        assert_eq!(Error::NotFound(String::new()).code(), "not_found");
        assert_eq!(Error::Invalid(String::new()).code(), "invalid_args");
        assert_eq!(Error::Config(String::new()).code(), "config");
        assert_eq!(Error::Internal(String::new()).code(), "internal");
        assert_eq!(
            Error::Unimplemented {
                command: "init",
                wp: 1
            }
            .code(),
            "unimplemented"
        );
    }
}
