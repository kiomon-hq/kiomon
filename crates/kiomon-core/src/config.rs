// SPDX-License-Identifier: Apache-2.0

//! Where the database lives, and what it is called.
//!
//! Resolution is a fixed order — flag, then environment, then platform convention —
//! and there is no config file yet. That is deliberate: a config file with one key in
//! it is a parser to maintain and a second place for the answer to be wrong, and
//! nothing in WP1–WP6 needs one. The embedder (WP7) is the first thing that genuinely
//! does, and it brings a real TOML dependency with it rather than a hand-rolled
//! subset that would become a bug.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// The environment variable that overrides the database path.
pub const ENV_DB: &str = "KIOMON_DB";

/// The environment variable that overrides the default workspace.
pub const ENV_WORKSPACE: &str = "KIOMON_WORKSPACE";

/// The directory name used under the platform's data directory.
const APP_DIR: &str = "kiomon";

/// The database file name.
const DB_FILE: &str = "brain.db";

/// The fallback location, `~/.kiomon/brain.db`.
const LEGACY_DIR: &str = ".kiomon";

/// Resolve the database path: flag, then `$KIOMON_DB`, then the platform data
/// directory, then `~/.kiomon`.
///
/// # Errors
///
/// Returns [`Error::Config`] when no home directory can be determined and no explicit
/// path was given — which is the one case where there is nothing to fall back to.
pub fn resolve_db_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    if let Some(value) = std::env::var_os(ENV_DB) {
        if !value.is_empty() {
            return Ok(PathBuf::from(value));
        }
    }
    if let Some(dir) = platform_data_dir() {
        return Ok(dir.join(DB_FILE));
    }
    let home = home_dir().ok_or_else(|| {
        Error::Config(format!(
            "could not work out where to put the database: no home directory, and \
             {ENV_DB} is not set. Pass --db <PATH> to say where it goes."
        ))
    })?;
    Ok(home.join(LEGACY_DIR).join(DB_FILE))
}

/// The default workspace, unless the environment overrides it.
///
/// `$KIOMON_WORKSPACE` exists because a user with several workspaces wants a shell or
/// an agent session to stay in one of them without repeating `--workspace` on every
/// command. Absence of `--workspace` still means exactly one workspace — this one.
#[must_use]
pub fn default_workspace() -> String {
    std::env::var(ENV_WORKSPACE)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| crate::model::DEFAULT_WORKSPACE.to_owned())
}

/// The platform's per-user data directory for this application.
#[must_use]
fn platform_data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA").map(|base| PathBuf::from(base).join(APP_DIR))
    }
    #[cfg(target_os = "macos")]
    {
        home_dir().map(|home| {
            home.join("Library")
                .join("Application Support")
                .join(APP_DIR)
        })
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
            if !xdg.is_empty() {
                return Some(PathBuf::from(xdg).join(APP_DIR));
            }
        }
        home_dir().map(|home| home.join(".local").join("share").join(APP_DIR))
    }
}

/// The user's home directory, from the environment.
///
/// `std::env::home_dir` is not used: it was deprecated for years over its Windows
/// behaviour and the replacement has a minimum version this crate will not raise for
/// one path join. The two variables below are what every other tool reads.
#[must_use]
fn home_dir() -> Option<PathBuf> {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

/// Turn a workspace name into an id.
///
/// Lowercased, with every run of non-alphanumeric characters replaced by a single
/// hyphen, and trimmed of hyphens. `init --workspace "Work Stuff"` creates `work-stuff`,
/// and a name that is already a slug is its own id.
///
/// An id is what `--workspace` addresses and what a row's `workspace_id` holds, so it
/// has to be stable and free of whitespace. Deriving it rather than asking for it means
/// a user never has to think about the difference.
///
/// # Errors
///
/// Returns [`Error::Invalid`] when the name has no alphanumeric characters at all, so
/// no id can be derived from it.
pub fn workspace_id_from_name(name: &str) -> Result<String> {
    let mut out = String::with_capacity(name.len());
    let mut pending_hyphen = false;
    for ch in name.trim().chars() {
        if ch.is_alphanumeric() {
            if pending_hyphen && !out.is_empty() {
                out.push('-');
            }
            pending_hyphen = false;
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
        } else {
            pending_hyphen = true;
        }
    }
    if out.is_empty() {
        return Err(Error::Invalid(format!(
            "`{name}` has no letters or digits, so no workspace id can be made from it"
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_path_wins_over_everything() {
        let explicit = PathBuf::from("/tmp/explicit.db");
        assert_eq!(resolve_db_path(Some(&explicit)).unwrap(), explicit);
    }

    #[test]
    fn workspace_ids_are_slugs() {
        assert_eq!(workspace_id_from_name("Work Stuff").unwrap(), "work-stuff");
        assert_eq!(workspace_id_from_name("  Spaces  ").unwrap(), "spaces");
        assert_eq!(
            workspace_id_from_name("already-a-slug").unwrap(),
            "already-a-slug"
        );
        assert_eq!(
            workspace_id_from_name("Client: Acme / Q4").unwrap(),
            "client-acme-q4"
        );
        assert_eq!(workspace_id_from_name("wörk").unwrap(), "wörk");
    }

    #[test]
    fn a_name_with_nothing_in_it_is_rejected() {
        for bad in ["", "   ", "---", "!!!"] {
            let err = workspace_id_from_name(bad).unwrap_err();
            assert_eq!(err.exit_code(), crate::ExitCode::InvalidArgs, "{bad}");
        }
    }
}
