// SPDX-License-Identifier: Apache-2.0

//! The per-kind metadata envelope.
//!
//! A memory's body is prose, and prose is a poor place to keep the things retrieval
//! wants to filter and rank on: which entities a fact is about, when an event
//! happened, what triggers a routine. That is what this envelope is for, and the
//! hosted product's `draft_memory` tool asks an agent to supply exactly these fields
//! so a paid extraction pass can be skipped. Locally, the agent *always* supplies
//! them, so there is no extraction pass at all.
//!
//! # One rule: fail open, and say so
//!
//! Metadata that does not typecheck **degrades to `{}` with a warning on stderr, and
//! the memory is still written.** Losing an agent's careful `entities` list is
//! annoying; losing the memory it was attached to because the list was a string
//! instead of an array would be much worse, and the second is what a closed failure
//! would buy. The caller prints the warning; this module never does.
//!
//! # What is deliberately not done
//!
//! **Defaults are not injected.** The hosted product's zod schemas fill absent array
//! fields with `[]`, which its TypeScript consumers rely on. Nothing here is untyped,
//! so the shape does not need pre-filling, and an envelope that stores what it was
//! given is an envelope whose diffs are readable.
//!
//! **Unknown keys pass through.** The taxonomy grows — a future kind will want a field
//! this version has never heard of — and dropping unknown keys would mean a newer
//! writer's data silently disappearing on an older reader.

use serde_json::{Map, Value};

use crate::model::MemoryKind;

/// The largest envelope that will be stored, in bytes of JSON.
///
/// An envelope is a handful of short strings and lists. Anything on the order of a
/// megabyte is a body that was put in the wrong field, and storing it would put that
/// mistake in every row a query touches rather than failing where it was made.
pub const MAX_METADATA_BYTES: usize = 64 * 1024;

/// The key that holds a semantic memory's subject matter. Part of the envelope for
/// every kind, because an agent filing a routine still wants it found by the project
/// it belongs to.
pub const KEY_DOMAIN: &str = "domain";
/// The key that holds the entities a memory is about.
pub const KEY_ENTITIES: &str = "entities";
/// The key that holds the verifiable claims a memory asserts.
pub const KEY_CLAIMS: &str = "claims";
/// The key that holds the memory ids a memory was derived from.
pub const KEY_SOURCE_REFS: &str = "source_refs";

/// The result of checking an envelope: what to store, and what to tell the user.
#[derive(Debug, Clone, PartialEq)]
pub struct Validated {
    /// The envelope to store. Never malformed; `{}` when the input was.
    pub value: Value,
    /// A warning for the caller to print on stderr, if anything was discarded.
    pub warning: Option<String>,
}

impl Validated {
    fn accepted(value: Value) -> Self {
        Self {
            value,
            warning: None,
        }
    }

    fn rejected(input: &Value, reason: &str) -> Self {
        Self {
            value: Value::Object(Map::new()),
            warning: Some(format!(
                "ignoring memory metadata ({reason}); storing an empty envelope instead. \
                 Received: {}",
                summarize(input)
            )),
        }
    }
}

/// A field's expected shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// A string.
    Text,
    /// An array of strings.
    TextList,
    /// One of a fixed set of strings.
    Choice(&'static [&'static str]),
}

/// The priority values a procedural memory accepts.
const PRIORITIES: &[&str] = &["low", "medium", "high"];

/// Fields every kind accepts, on top of its own.
const COMMON: &[(&str, Shape)] = &[
    (KEY_DOMAIN, Shape::Text),
    (KEY_ENTITIES, Shape::TextList),
    (KEY_CLAIMS, Shape::TextList),
    (KEY_SOURCE_REFS, Shape::TextList),
];

const SEMANTIC: &[(&str, Shape)] = &[("valid_from", Shape::Text), ("valid_until", Shape::Text)];

const PROCEDURAL: &[(&str, Shape)] = &[
    ("trigger", Shape::Text),
    ("steps", Shape::TextList),
    ("scope", Shape::Text),
    ("priority", Shape::Choice(PRIORITIES)),
    ("examples", Shape::TextList),
];

const EPISODIC: &[(&str, Shape)] = &[
    ("occurred_at", Shape::Text),
    ("participants", Shape::TextList),
    ("outcome", Shape::Text),
    ("location", Shape::Text),
    ("context", Shape::Text),
];

/// The kind-specific fields, on top of [`COMMON`].
#[must_use]
fn kind_fields(kind: MemoryKind) -> &'static [(&'static str, Shape)] {
    match kind {
        MemoryKind::Reference => &[],
        MemoryKind::Semantic => SEMANTIC,
        MemoryKind::Procedural => PROCEDURAL,
        MemoryKind::Episodic => EPISODIC,
    }
}

/// Every field name this version knows about, for help text and tests.
#[must_use]
pub fn known_fields(kind: MemoryKind) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = COMMON.iter().map(|(name, _)| *name).collect();
    names.extend(kind_fields(kind).iter().map(|(name, _)| *name));
    names
}

/// Check an envelope against its kind.
///
/// `None` and `null` mean "no envelope" and are not an error — they are what a plain
/// `kiomon add --title … --body …` produces.
#[must_use]
pub fn validate(kind: MemoryKind, raw: Option<&Value>) -> Validated {
    let Some(raw) = raw else {
        return Validated::accepted(Value::Object(Map::new()));
    };

    if raw.is_null() {
        return Validated::accepted(Value::Object(Map::new()));
    }

    // A scalar or an array is not an envelope. Both are what a caller produces by
    // double-encoding JSON (`--metadata '"{}"'`) or by putting an entities list where
    // the whole object belongs, and both are worth naming rather than silently
    // treating as absent.
    if !raw.is_object() {
        return Validated::rejected(
            raw,
            if raw.is_array() {
                "expected a JSON object, got an array"
            } else {
                "expected a JSON object"
            },
        );
    }

    let bytes = raw.to_string().len();
    if bytes > MAX_METADATA_BYTES {
        return Validated::rejected(
            raw,
            &format!("it is {bytes} bytes, over the {MAX_METADATA_BYTES}-byte limit"),
        );
    }

    // `raw` is an object, so this cannot be `None`.
    let Some(object) = raw.as_object() else {
        return Validated::rejected(raw, "expected a JSON object");
    };

    // Validate before copying: a wrong type anywhere means the envelope is discarded
    // whole, so there is no point building a partial one.
    for (name, shape) in COMMON.iter().chain(kind_fields(kind)) {
        let Some(found) = object.get(*name) else {
            continue;
        };
        if let Err(reason) = check(name, *shape, found) {
            return Validated::rejected(raw, &reason);
        }
    }

    Validated::accepted(Value::Object(object.clone()))
}

/// Collect the fields a caller supplied into an envelope, dropping the empty ones.
///
/// `kiomon add --entities a --entities b` and `--metadata '{"entities":["a","b"]}'`
/// must produce the same row, so both go through here and then through [`validate`].
#[must_use]
pub fn assemble(parts: Vec<(&'static str, Value)>) -> Option<Value> {
    let mut object = Map::new();
    for (key, value) in parts {
        let empty = match &value {
            Value::String(text) => text.is_empty(),
            Value::Array(items) => items.is_empty(),
            _ => false,
        };
        if empty {
            continue;
        }
        object.insert(key.to_owned(), value);
    }
    if object.is_empty() {
        None
    } else {
        Some(Value::Object(object))
    }
}

/// A string list from a repeatable, comma-separated command-line flag.
///
/// Both spellings are accepted because both are natural: `--tag a --tag b` from a
/// human, `--tag a,b` from an agent building one command line.
#[must_use]
pub fn string_list(items: &[String]) -> Value {
    let mut out: Vec<Value> = Vec::with_capacity(items.len());
    for item in items {
        for part in item.split(',') {
            let trimmed = part.trim();
            if !trimmed.is_empty() {
                out.push(Value::String(trimmed.to_owned()));
            }
        }
    }
    Value::Array(out)
}

/// Normalise a tag list: trimmed, non-empty, deduplicated case-insensitively, capped.
///
/// Tags are the one part of the envelope a user types by hand, so they get the most
/// tolerance. The cap is the hosted product's twenty, kept because a memory with two
/// hundred tags is a memory whose tags have stopped meaning anything, and because
/// `tags_text` is indexed.
///
/// # Errors
///
/// Returns the reason as a string when the list is over the cap, for the caller to
/// turn into whatever it wants. Never fails for any other reason — a duplicate tag is
/// dropped rather than reported, because it is not worth a user's attention.
pub fn normalize_tags(tags: &[String]) -> Result<Vec<String>, String> {
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<String> = Vec::new();
    for tag in tags {
        let trimmed = tag.trim();
        if trimmed.is_empty() {
            continue;
        }
        let folded = trimmed.to_lowercase();
        if seen.contains(&folded) {
            continue;
        }
        seen.push(folded);
        out.push(trimmed.to_owned());
    }
    if out.len() > crate::model::MAX_TAGS {
        return Err(format!(
            "{} tags were given but the limit is {}",
            out.len(),
            crate::model::MAX_TAGS
        ));
    }
    Ok(out)
}

/// The space-joined form of a tag list, wrapped in spaces.
///
/// This exists only so the full-text index has a column to weight. The surrounding
/// spaces are what make it *nearly* safe to match against — but not quite, which is
/// why tag filtering reads the JSON and never this. See the `memories` table's
/// comment in `0001_init.sql`.
#[must_use]
pub fn tags_text(tags: &[String]) -> String {
    if tags.is_empty() {
        return String::new();
    }
    let mut out =
        String::with_capacity(tags.iter().map(String::len).sum::<usize>() + tags.len() + 1);
    out.push(' ');
    for tag in tags {
        out.push_str(tag);
        out.push(' ');
    }
    out
}

fn check(name: &str, shape: Shape, found: &Value) -> Result<(), String> {
    let ok = match shape {
        Shape::Text => found.is_string(),
        Shape::TextList => found
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_string)),
        Shape::Choice(allowed) => found.as_str().is_some_and(|value| allowed.contains(&value)),
    };
    if ok {
        return Ok(());
    }
    Err(format!(
        "`{name}` should be {}, found {}",
        describe(shape),
        describe_value(found)
    ))
}

fn describe(shape: Shape) -> String {
    match shape {
        Shape::Text => "a string".to_owned(),
        Shape::TextList => "an array of strings".to_owned(),
        Shape::Choice(allowed) => format!("one of {}", allowed.join(", ")),
    }
}

fn describe_value(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// A short rendering of a value for a warning line. Truncated because the point of the
/// warning is to say what went wrong, not to reprint a body that was pasted into the
/// wrong field.
fn summarize(value: &Value) -> String {
    let rendered = value.to_string();
    if rendered.len() <= 200 {
        return rendered;
    }
    // `&rendered[..200]` would panic on a multi-byte boundary; `chars().take()` cannot.
    let head: String = rendered.chars().take(200).collect();
    format!("{head}… ({} bytes)", rendered.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn warning_of(validated: &Validated) -> &str {
        validated.warning.as_deref().expect("expected a warning")
    }

    #[test]
    fn no_envelope_is_an_empty_object_not_a_warning() {
        let validated = validate(MemoryKind::Semantic, None);
        assert_eq!(validated.value, json!({}));
        assert!(validated.warning.is_none());

        let validated = validate(MemoryKind::Semantic, Some(&Value::Null));
        assert_eq!(validated.value, json!({}));
        assert!(validated.warning.is_none());
    }

    #[test]
    fn a_well_formed_envelope_is_stored_unchanged() {
        let input = json!({
            "domain": "infra",
            "entities": ["staging", "migrations"],
            "claims": ["migrations are manual"],
        });
        let validated = validate(MemoryKind::Semantic, Some(&input));
        assert_eq!(validated.value, input);
        assert!(validated.warning.is_none());
    }

    #[test]
    fn absent_list_fields_are_not_injected() {
        // The hosted product's zod schemas fill these with `[]`. Nothing here needs
        // the shape pre-filled, and an envelope that stores what it was given is one
        // whose diffs can be read.
        let validated = validate(MemoryKind::Semantic, Some(&json!({"domain": "infra"})));
        assert_eq!(validated.value, json!({"domain": "infra"}));
    }

    #[test]
    fn unknown_keys_pass_through() {
        // A newer writer's field must survive an older reader, or an upgrade becomes a
        // data-loss event.
        let input = json!({"something_from_the_future": {"nested": [1, 2]}});
        let validated = validate(MemoryKind::Episodic, Some(&input));
        assert_eq!(validated.value, input);
        assert!(validated.warning.is_none());
    }

    #[test]
    fn a_common_field_is_accepted_on_every_kind() {
        // `--entities` is on `add` for all four kinds, so it has to validate for all
        // four. A kind that rejected it would make the flag a trap.
        for kind in MemoryKind::ALL {
            let input = json!({"entities": ["a"], "claims": ["b"], "domain": "c"});
            let validated = validate(kind, Some(&input));
            assert_eq!(validated.value, input, "{kind}");
            assert!(validated.warning.is_none(), "{kind}");
        }
    }

    #[test]
    fn a_wrong_type_discards_the_whole_envelope_and_names_the_field() {
        let validated = validate(
            MemoryKind::Semantic,
            Some(&json!({"domain": "infra", "entities": "staging"})),
        );
        assert_eq!(validated.value, json!({}));
        let warning = warning_of(&validated);
        assert!(warning.contains("entities"), "{warning}");
        assert!(warning.contains("array of strings"), "{warning}");
    }

    #[test]
    fn a_list_with_a_non_string_element_is_rejected() {
        let validated = validate(MemoryKind::Semantic, Some(&json!({"entities": ["a", 3]})));
        assert_eq!(validated.value, json!({}));
        assert!(warning_of(&validated).contains("entities"));
    }

    #[test]
    fn a_scalar_or_array_is_not_an_envelope() {
        let validated = validate(MemoryKind::Reference, Some(&json!("just a string")));
        assert_eq!(validated.value, json!({}));
        assert!(warning_of(&validated).contains("expected a JSON object"));

        let validated = validate(MemoryKind::Reference, Some(&json!(["a", "b"])));
        assert_eq!(validated.value, json!({}));
        assert!(warning_of(&validated).contains("got an array"));
    }

    #[test]
    fn a_choice_field_lists_its_options() {
        let validated = validate(MemoryKind::Procedural, Some(&json!({"priority": "urgent"})));
        assert_eq!(validated.value, json!({}));
        let warning = warning_of(&validated);
        for allowed in PRIORITIES {
            assert!(warning.contains(allowed), "{warning}");
        }
        let ok = validate(MemoryKind::Procedural, Some(&json!({"priority": "high"})));
        assert!(ok.warning.is_none());
    }

    #[test]
    fn the_kind_specific_fields_are_checked_where_they_belong() {
        // `steps` is a procedural field. An episodic memory carrying it is not an
        // error — it passes through as unknown — which is the passthrough rule doing
        // its job rather than a hole in the check.
        let wrong_here = validate(MemoryKind::Procedural, Some(&json!({"steps": "one"})));
        assert!(wrong_here.warning.is_some());
        let passthrough = validate(MemoryKind::Episodic, Some(&json!({"steps": "one"})));
        assert!(passthrough.warning.is_none());
    }

    #[test]
    fn an_oversized_envelope_is_discarded_with_its_size_in_the_warning() {
        let huge = "x".repeat(MAX_METADATA_BYTES + 1);
        let validated = validate(MemoryKind::Reference, Some(&json!({"domain": huge})));
        assert_eq!(validated.value, json!({}));
        let warning = warning_of(&validated);
        assert!(warning.contains("limit"), "{warning}");
    }

    #[test]
    fn a_long_warning_is_truncated_rather_than_reprinting_the_body() {
        // The value has to be a wrong *shape* to be rejected, so a big array of
        // objects is the cheapest way to produce a large rejected input.
        let items: Vec<Value> = (0..500).map(|index| json!({"n": index})).collect();
        let validated = validate(MemoryKind::Semantic, Some(&json!({"entities": items})));
        let warning = warning_of(&validated);
        assert!(warning.len() < 400, "warning is {} bytes", warning.len());
        assert!(warning.contains('…'), "{warning}");
    }

    #[test]
    fn assembling_drops_empty_parts_and_yields_nothing_when_all_are_empty() {
        assert!(assemble(vec![("domain", json!("")), ("entities", json!([]))]).is_none());
        assert_eq!(
            assemble(vec![("domain", json!("infra")), ("claims", json!([]))]),
            Some(json!({"domain": "infra"}))
        );
    }

    #[test]
    fn string_lists_accept_both_spellings_and_drop_empties() {
        let items = vec!["a,b".to_owned(), " c ".to_owned(), String::new()];
        assert_eq!(string_list(&items), json!(["a", "b", "c"]));
    }

    #[test]
    fn tags_are_normalised_and_capped() {
        let tags = [
            "deploy".to_owned(),
            "Deploy".to_owned(),
            " staging ".to_owned(),
        ];
        assert_eq!(
            normalize_tags(&tags).unwrap(),
            vec!["deploy".to_owned(), "staging".to_owned()]
        );

        let too_many: Vec<String> = (0..=crate::model::MAX_TAGS)
            .map(|index| format!("t{index}"))
            .collect();
        let err = normalize_tags(&too_many).unwrap_err();
        assert!(err.contains("limit"), "{err}");
    }

    #[test]
    fn tags_text_is_wrapped_in_spaces_and_empty_when_there_are_none() {
        assert_eq!(tags_text(&[]), "");
        assert_eq!(
            tags_text(&["deploy".to_owned(), "staging".to_owned()]),
            " deploy staging "
        );
    }

    #[test]
    fn every_known_field_is_listed_for_help_text() {
        assert_eq!(
            known_fields(MemoryKind::Reference),
            vec!["domain", "entities", "claims", "source_refs"]
        );
        assert!(known_fields(MemoryKind::Episodic).contains(&"occurred_at"));
        assert!(known_fields(MemoryKind::Procedural).contains(&"steps"));
        assert!(known_fields(MemoryKind::Semantic).contains(&"valid_until"));
    }
}
