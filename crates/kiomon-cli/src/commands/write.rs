// SPDX-License-Identifier: Apache-2.0

//! Writing: `add`, `update`, `import`, `link`, `unlink`, `relate`.
//!
//! Every one of these goes through `kiomon_core::gate`, which owns the two decisions
//! a write makes: whether the memory activates or waits in the review inbox, and
//! whether an identical memory already exists. Keeping that in one place is what stops
//! the CLI, a future MCP server, and the auto-linker from disagreeing about it.

use kiomon_core::Result;

use super::pending;
use crate::cli::{AddArgs, Cli, ImportArgs, LinkArgs, RelateArgs, UnlinkArgs, UpdateArgs};

/// Save one memory.
pub fn add(_cli: &Cli, _args: &AddArgs) -> Result<()> {
    pending("add", 1)
}

/// Change an existing memory, re-indexing it if what retrieval sees has changed.
pub fn update(_cli: &Cli, _args: &UpdateArgs) -> Result<()> {
    pending("update", 1)
}

/// Read memories in from a file, a directory, or standard input.
pub fn import(_cli: &Cli, _args: &ImportArgs) -> Result<()> {
    pending("import", 6)
}

/// Draw a typed edge between two memories.
pub fn link(_cli: &Cli, _args: &LinkArgs) -> Result<()> {
    pending("link", 5)
}

/// Remove an edge between two memories.
pub fn unlink(_cli: &Cli, _args: &UnlinkArgs) -> Result<()> {
    pending("unlink", 5)
}

/// Show a memory's neighbours in the graph.
pub fn relate(_cli: &Cli, _args: &RelateArgs) -> Result<()> {
    pending("relate", 5)
}
