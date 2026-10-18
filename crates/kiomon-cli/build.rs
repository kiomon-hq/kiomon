// SPDX-License-Identifier: Apache-2.0

//! Stamps the binary with the commit it was built from.
//!
//! A release number says *which release*; it does not say which commit inside it, and the
//! difference is the whole of a bug report: `0.1.0` covers every build from the tag until
//! the next one, including the one a user made from a dirty tree. `kiomon --version`
//! therefore reports `0.1.0 (git 6cb7aed1a2b3)`, and the bench report carries the same
//! string, so a performance number in a CI artifact can be traced to the commit that
//! produced it.
//!
//! Three regressions this file has to be careful about, because each is invisible until
//! it matters:
//!
//! - **A build with no repository must still compile.** `cargo install kiomon-cli` builds
//!   from the published tarball, which has no `.git`; `cargo package --verify` is the same
//!   situation. The stamp is `unknown` there, not a build failure.
//! - **The stamp has to be re-run when the commit changes.** A build script's output is
//!   cached against the files it watched, and a commit does not touch any file in this
//!   crate. What changes is `.git/HEAD`, the branch it points at, and the index — so those
//!   are what is watched, and the branch ref is resolved rather than guessed.
//! - **A dirty tree is not a release.** Untracked files are ignored on purpose (a `target/`
//!   directory or a scratch script is not part of the build), but tracked modifications
//!   suffix the stamp with `-dirty`, so no binary can claim to be the tag it was not
//!   built from.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=KIOMON_GIT_SHA");

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    watch(&root);

    // The override wins wherever it is set, including over dirty detection: CI stamps the
    // tag's sha into a build from the tag, and a reproducible build needs the number it
    // was told rather than the one this machine happens to have.
    let stamp = match std::env::var("KIOMON_GIT_SHA") {
        Ok(sha) if !sha.trim().is_empty() => sha.trim().to_owned(),
        _ => match git(&root, &["rev-parse", "--short=12", "HEAD"]) {
            Some(sha) => {
                if is_dirty(&root) {
                    format!("{sha}-dirty")
                } else {
                    sha
                }
            }
            None => "unknown".to_owned(),
        },
    };

    println!("cargo:rustc-env=KIOMON_GIT_SHA={stamp}");
}

/// Tell cargo which files decide the stamp.
///
/// `rerun-if-changed` on the repository directory would be enough to notice a checkout and
/// not enough to notice a commit, because a commit rewrites the branch file and leaves the
/// directory's own timestamp alone. So the paths are listed one by one, and the branch is
/// the one `HEAD` names — a detached `HEAD` has no branch file, which is why the resolved
/// path is pushed rather than assumed.
fn watch(root: &Path) {
    let git = root.join(".git");
    // A `.git` *file* means a worktree or a submodule: the real repository is elsewhere.
    // `git rev-parse` still answers, and watching the pointer is cheaper than resolving it.
    for path in ["HEAD", "index", "packed-refs"] {
        println!("cargo:rerun-if-changed={}", git.join(path).display());
    }

    let Ok(head) = std::fs::read_to_string(git.join("HEAD")) else {
        return;
    };
    if let Some(reference) = head.strip_prefix("ref: ") {
        let branch = git.join(reference.trim());
        // Canonicalised where possible, because cargo compares paths textually and a
        // relative `./refs/heads/main` would watch nothing; kept as written when the file
        // does not exist yet, which is the state right after `git init`.
        let path: PathBuf = branch.canonicalize().unwrap_or(branch);
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

/// Whether a tracked file differs from `HEAD`.
///
/// `diff-index` rather than `status`, because `status` also reports untracked files and
/// those are not part of a build: a `target/` directory would mark every binary dirty.
fn is_dirty(root: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff-index", "--quiet", "HEAD", "--"])
        .status()
        .is_ok_and(|status| !status.success())
}

/// One `git` invocation's first line of stdout, or `None` when git is missing, the
/// directory is not a repository, or the command failed.
fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_owned())
}
