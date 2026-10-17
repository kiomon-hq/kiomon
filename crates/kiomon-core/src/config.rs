// SPDX-License-Identifier: Apache-2.0

//! Where the database lives, and what it is called.
//!
//!
//! Resolution is a fixed order — flag, then environment, then the config file, then the
//! platform convention — and the order is the contract, not an accident. Each step is a
//! narrower statement than the one before it: a flag says "this file, now", an
//! environment variable says "this file, in this shell", and the config file says "this
//! file, on this machine".
//!
//! # The config file
//!
//! `$KIOMON_CONFIG`, or `--config`, or `<data dir>/config.toml` (`%APPDATA%\kiomon` on
//! Windows, `~/.kiomon` as the fallback everywhere): TOML, and it holds `default_workspace`
//! and the optional `[embedder]` block. §3.1 of the plan also assigns it `review_mode`,
//! which it does not hold — the review mode is a column on the workspace row, set by
//! `init --review`, and a second place to say it is a second place to be wrong. §8's WP7
//! record carries the contradiction.
//!
//! A missing file is not an error: a brain with no config is a full-text brain, and the
//! commands that need nothing from the file must not fail because it is absent. A file
//! that exists and does not parse *is* an error, and it names the file and the span —
//! silently ignoring a typo in `[embedder]` would look exactly like a broken embedder.
//!
//! # The one deliberate omission
//!
//! `review_mode` is not here. See above.

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

/// The default workspace: `$KIOMON_WORKSPACE`, then the config file, then `default`.
///
/// `$KIOMON_WORKSPACE` exists because a user with several workspaces wants a shell or an
/// agent session to stay in one of them without repeating `--workspace` on every command,
/// and it outranks the file because an environment variable is the more specific
/// statement. Absence of `--workspace` still means exactly one workspace — this one.
#[must_use]
pub fn default_workspace(config: &Config) -> String {
    std::env::var(ENV_WORKSPACE)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            config
                .default_workspace
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| crate::model::DEFAULT_WORKSPACE.to_owned())
}

/// The environment variable that names the config file.
pub const ENV_CONFIG: &str = "KIOMON_CONFIG";

/// The config file's name, in whichever directory it lives.
const CONFIG_FILE: &str = "config.toml";

/// What the config file said.
///
/// Every field is optional because the file is: a config that sets only
/// `default_workspace` is a complete config, and a config that sets only an embedder is a
/// complete config too.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The workspace a command uses when it is not told.
    pub default_workspace: Option<String>,
    /// The optional embedder (WP7). Absent means lexical search only.
    pub embedder: Option<crate::embed::EmbedderConfig>,
    /// The file this was read from, for messages. `None` when there was no file.
    #[serde(skip)]
    pub source: Option<PathBuf>,
}

impl Config {
    /// Read the config file: `--config`, then `$KIOMON_CONFIG`, then the data directory.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] naming the file and the parse error when a file that
    /// exists does not parse, and [`Error::Io`] when it exists and cannot be read.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = match explicit {
            Some(path) => Some(path.to_path_buf()),
            None => match std::env::var_os(ENV_CONFIG) {
                Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
                _ => Self::default_path(),
            },
        };
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            // Absent is a normal state: most brains have no embedder and no config file.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // ... unless the path was named explicitly, in which case the user is
                // asking for *that* file and a typo in it has to be a failure.
                if explicit.is_some() || std::env::var_os(ENV_CONFIG).is_some() {
                    return Err(Error::Config(format!(
                        "could not read the config file {}: {err}",
                        path.display()
                    )));
                }
                return Ok(Self::default());
            }
            Err(err) => {
                return Err(Error::Config(format!(
                    "could not read the config file {}: {err}",
                    path.display()
                )));
            }
        };
        Self::parse(&text, &path)
    }

    /// Parse config text, with the file it came from for the error message.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] with the file, the line, and the offending text.
    pub fn parse(text: &str, path: &Path) -> Result<Self> {
        let mut parsed: Self = toml::from_str(text).map_err(|err| {
            // `toml`'s own message includes the span and a caret, which is the most useful
            // part of it: a config typo is almost always one character, one line up.
            Error::Config(format!("{} is not valid TOML: {err}", path.display()))
        })?;
        parsed.source = Some(path.to_path_buf());
        Ok(parsed)
    }

    /// The file a config is read from when nothing names one.
    ///
    /// Prefers the platform data directory, because that is where the database lives and
    /// a brain is one thing; `~/.kiomon` is the fallback the database path also uses, so a
    /// user on an old path keeps working.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        if let Some(dir) = platform_data_dir() {
            let candidate = dir.join(CONFIG_FILE);
            if candidate.exists() {
                return Some(candidate);
            }
        }
        let legacy = home_dir()?.join(LEGACY_DIR).join(CONFIG_FILE);
        Some(legacy)
    }

    /// The database path this config resolves to: `explicit`, then `$KIOMON_DB`, then the
    /// platform convention.
    ///
    /// The config file holds no path of its own. A database path in a config file is a
    /// second answer to "which brain is this", and the first thing a user would do is move
    /// the file and get two brains.
    ///
    /// # Errors
    ///
    /// As [`resolve_db_path`].
    pub fn resolve_db_path(&self, explicit: Option<&Path>) -> Result<PathBuf> {
        resolve_db_path(explicit)
    }

    /// The embedder this config describes, if any.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] with the source file named when the `[embedder]` block
    /// is incomplete or contradicts itself. Building it here rather than lazily is
    /// deliberate: a broken embedder is a broken configuration, and a `search` that never
    /// touches the vector leg should still refuse to run on a config it cannot honour.
    pub fn embedder(&self) -> Result<Option<crate::embed::Embedder>> {
        let Some(config) = &self.embedder else {
            return Ok(None);
        };
        crate::embed::Embedder::build(config)
            .map(Some)
            .map_err(|err| match &self.source {
                Some(path) => Error::Config(format!("{}: {err}", path.display())),
                None => err,
            })
    }
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
