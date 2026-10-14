// SPDX-License-Identifier: Apache-2.0

//! Setup and health: `init`, `doctor`, `stats`.
//!
//! `init` and `doctor` are the first two commands WP1 makes real, because every other
//! command needs a database that exists and a way to find out why one does not.

use kiomon_core::Result;

use super::pending;
use crate::cli::{Cli, DoctorArgs, InitArgs, StatsArgs};

/// Create a database, apply the schema, and ensure the default workspace exists.
pub fn init(_cli: &Cli, _args: &InitArgs) -> Result<()> {
    pending("init", 1)
}

/// Check the database, the schema and the configuration, and report what is wrong.
pub fn doctor(_cli: &Cli, _args: &DoctorArgs) -> Result<()> {
    pending("doctor", 1)
}

/// Count what is in the brain, by kind and status.
pub fn stats(_cli: &Cli, _args: &StatsArgs) -> Result<()> {
    pending("stats", 4)
}
