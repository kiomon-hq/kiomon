// SPDX-License-Identifier: Apache-2.0

//! Writing: `add`, `update`, `import`, `link`, `unlink`, `relate`.
//!
//! The CLI's whole job here is to turn flags into a [`NewMemory`] or a [`MemoryPatch`]
//! and hand it to the store. The two decisions a write makes — whether it activates, and
//! whether an identical memory already exists — belong to `kiomon_core::gate` and the
//! store, so that the CLI, a future MCP server and the auto-linker cannot disagree about
//! them.

use std::io::Read as _;
use std::path::Path;

use kiomon_core::metadata;
use kiomon_core::{Error, ExitCode, MemoryPatch, NewMemory, Result, Scope};

use super::{decay_if_due, deferred, open, pending, write_workspace};
use crate::cli::{AddArgs, Cli, ImportArgs, LinkArgs, RelateArgs, UnlinkArgs, UpdateArgs};
use crate::out;

/// `kiomon add`
///
/// # Errors
///
/// Returns [`Error::Invalid`] for an unusable combination of inputs, [`Error::Io`] when
/// a file or stdin cannot be read, and whatever the store raises.
pub fn add(cli: &Cli, args: &AddArgs) -> Result<ExitCode> {
    let db = open(cli)?;
    let workspace = write_workspace(&db, cli, args.workspace.as_deref())?;

    let (title, body) = if let Some(path) = &args.from_file {
        let text = read_file(path)?;
        let derived = title_from_markdown(&text).or_else(|| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        });
        // An explicit --title wins over the file's own heading: the user said what they
        // wanted the memory to be called.
        let title = args.title.clone().or(derived).ok_or_else(|| {
            Error::Invalid(format!(
                "{} has no heading to take a title from; pass --title",
                path.display()
            ))
        })?;
        (title, text)
    } else {
        let title = args.title.clone().unwrap_or_default();
        (
            title,
            read_body(args.body.as_deref(), args.body_file.as_deref(), args.stdin)?,
        )
    };

    let envelope = match args.metadata.as_deref() {
        Some(raw) => Some(
            serde_json::from_str(raw)
                .map_err(|err| Error::Invalid(format!("--metadata is not valid JSON: {err}")))?,
        ),
        None => metadata::assemble(vec![
            (
                metadata::KEY_DOMAIN,
                string_or_empty(args.domain.as_deref()),
            ),
            (
                metadata::KEY_ENTITIES,
                metadata::string_list(&args.entities),
            ),
            (metadata::KEY_CLAIMS, metadata::string_list(&args.claims)),
        ]),
    };

    let mut new = NewMemory::new(args.kind.into(), title)
        .workspace(workspace.clone())
        .body(body)
        .excerpt(args.excerpt.clone().unwrap_or_default())
        .tags(args.tag.clone())
        .metadata(envelope)
        .source(args.source.clone())
        .derived_from(args.derived_from.clone())
        .force(args.force);
    if let Some(confidence) = args.confidence {
        new = new.confidence(confidence);
    }
    if let Some(url) = &args.url {
        new = new.source_url(url.clone());
    }

    let outcome = db.add(&new)?;
    for warning in &outcome.warnings {
        out::warn(cli.quiet, warning);
    }

    out::emit(cli.json, outcome, |outcome| {
        if outcome.duplicate {
            println!("{} already holds this ({}).", outcome.id, outcome.status);
        } else {
            println!(
                "{}  {}  {}",
                outcome.id, outcome.status, outcome.workspace_id
            );
        }
    })?;

    decay_if_due(&db, &Scope::One(workspace), cli.quiet);

    Ok(ExitCode::Success)
}

/// `kiomon update`
///
/// # Errors
///
/// Returns [`Error::NotFound`] for an unknown id, [`Error::Invalid`] for a bad value, and
/// [`Error::Io`] when a file or stdin cannot be read.
pub fn update(cli: &Cli, args: &UpdateArgs) -> Result<ExitCode> {
    let db = open(cli)?;
    let body = match (args.body.as_deref(), args.body_file.as_deref(), args.stdin) {
        (None, None, false) => None,
        (body, body_file, stdin) => Some(read_body(body, body_file, stdin)?),
    };

    let patch = MemoryPatch {
        title: args.title.clone(),
        excerpt: args.excerpt.clone(),
        body,
        tags: if args.tag.is_empty() {
            None
        } else {
            Some(args.tag.clone())
        },
        kind: args.kind.map(kiomon_core::MemoryKind::from),
        confidence: args.confidence,
        importance: args.importance,
        source_url: args.url.clone(),
        memory_metadata: args
            .metadata
            .as_deref()
            .map(|text| {
                serde_json::from_str(text)
                    .map_err(|err| Error::Invalid(format!("--metadata is not valid JSON: {err}")))
            })
            .transpose()?,
    };

    let outcome = db.update(&args.id, &patch)?;
    for warning in &outcome.warnings {
        out::warn(cli.quiet, warning);
    }

    out::emit(cli.json, outcome, |outcome| {
        if outcome.changed == 0 {
            println!("{}: nothing to change.", outcome.id);
        } else {
            println!("{}: {} field(s) updated.", outcome.id, outcome.changed);
        }
    })?;
    Ok(ExitCode::Success)
}

/// `kiomon import`
///
/// # Errors
///
/// Always [`Error::Unimplemented`], until WP6.
pub fn import(_cli: &Cli, _args: &ImportArgs) -> Result<ExitCode> {
    pending("import", 6)
}

/// `kiomon link`
///
/// # Errors
///
/// Always [`Error::Deferred`]: the graph left v1 (§9-D9) and is the next sprint's first
/// work package. Writing the edge anyway — the table and its constraints exist — would
/// create rows that no verb can read back, which is a worse answer than a refusal.
pub fn link(_cli: &Cli, _args: &LinkArgs) -> Result<ExitCode> {
    deferred("link")
}

/// `kiomon unlink`
///
/// # Errors
///
/// Always [`Error::Deferred`] — see [`link`].
pub fn unlink(_cli: &Cli, _args: &UnlinkArgs) -> Result<ExitCode> {
    deferred("unlink")
}

/// `kiomon relate`
///
/// # Errors
///
/// Always [`Error::Deferred`] — see [`link`].
pub fn relate(_cli: &Cli, _args: &RelateArgs) -> Result<ExitCode> {
    deferred("relate")
}

/// The content for a write, from whichever source was given.
///
/// Stdin is read only when it was asked for. Reading it otherwise would make
/// `kiomon add` hang on a terminal — and worse, would silently consume the input of a
/// shell pipeline the user did not aim at this command.
fn read_body(body: Option<&str>, body_file: Option<&Path>, stdin: bool) -> Result<String> {
    if let Some(text) = body {
        return Ok(text.to_owned());
    }
    if let Some(path) = body_file {
        return read_file(path);
    }
    if stdin {
        let mut buffer = String::new();
        std::io::stdin().read_to_string(&mut buffer)?;
        return Ok(buffer);
    }
    Ok(String::new())
}

/// Read a whole file, naming it when it fails.
fn read_file(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|err| {
        Error::Io(std::io::Error::new(
            err.kind(),
            format!("could not read {}: {err}", path.display()),
        ))
    })
}

/// The title a markdown file gives itself: its first ATX heading.
///
/// Deliberately the *first* one rather than the most prominent: a file whose first line
/// is `# Title` and whose third is `## Overview` means the title is `Title`, and scanning
/// for the shallowest heading would pick up a document that starts with `## Notes`.
///
/// Closing hashes (`# Title #`) are stripped, and a heading with no text is skipped
/// rather than becoming an empty title.
#[must_use]
fn title_from_markdown(text: &str) -> Option<String> {
    for line in text.lines().take(50) {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix('#') else {
            continue;
        };
        let heading = rest.trim_start_matches('#').trim();
        let heading = heading.trim_end_matches('#').trim();
        if !heading.is_empty() {
            return Some(heading.to_owned());
        }
    }
    None
}

fn string_or_empty(value: Option<&str>) -> serde_json::Value {
    serde_json::Value::String(value.unwrap_or_default().trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_markdown_heading_becomes_the_title() {
        assert_eq!(
            title_from_markdown("# Release checklist\n\n1. tag"),
            Some("Release checklist".to_owned())
        );
        assert_eq!(
            title_from_markdown("## Deeper\n# Shallow"),
            Some("Deeper".to_owned()),
            "the first heading wins, not the shallowest"
        );
        assert_eq!(
            title_from_markdown("# Title #\n"),
            Some("Title".to_owned()),
            "closing hashes are decoration"
        );
    }

    #[test]
    fn a_file_with_no_heading_has_no_derived_title() {
        assert_eq!(title_from_markdown("just prose\nand more"), None);
        assert_eq!(
            title_from_markdown("#\n#\n"),
            None,
            "an empty heading is not a title"
        );
        assert_eq!(title_from_markdown(""), None);
    }

    #[test]
    fn body_sources_are_used_in_the_order_the_parser_allows() {
        // The parser's ArgGroup makes these mutually exclusive, so this asserts the
        // mapping rather than the conflict: whichever one survives parsing is used.
        assert_eq!(read_body(Some("text"), None, false).unwrap(), "text");
        assert_eq!(
            read_body(None, None, false).unwrap(),
            "",
            "no source is an empty body"
        );
    }

    #[test]
    fn a_missing_file_names_the_path() {
        let err = read_file(Path::new("/no/such/kiomon/file.md")).unwrap_err();
        assert!(err.to_string().contains("file.md"), "{err}");
        assert_eq!(err.exit_code(), ExitCode::Internal);
    }
}
