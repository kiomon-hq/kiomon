// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the store: the WP1 acceptance criteria from
//! `plans/001-local-v1-core.md` §8, run against the public API rather than the crate's
//! internals.
//!
//! The unit tests beside each module cover behaviour. These cover the two things that
//! can only be seen from outside: that a real file survives being closed and reopened,
//! and that the full-text index is still consistent after a thousand writes and a
//! thousand purges — which is the check that catches a wrong trigger, and the reason
//! `memory_fts` is not an external-content table.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use kiomon_core::{
    Database, Error, ExitCode, ListQuery, MAX_TAGS, MemoryKind, MemoryPatch, NewMemory, ReviewMode,
    Scope, Status,
};

fn tempdir() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("kiomon-it-{}-{unique}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a test directory must be creatable");
    dir
}

/// How many memories exist, across every workspace and status.
fn memories(db: &Database) -> i64 {
    db.stats(&Scope::All, &[])
        .expect("stats must be readable")
        .total
}

/// How many rows the full-text index holds.
fn indexed(db: &Database) -> i64 {
    db.indexed_count()
        .expect("the index count must be readable")
}

#[test]
fn a_memory_survives_closing_and_reopening_the_file() {
    let dir = tempdir();
    let path = dir.join("brain.db");

    let id = {
        let db = Database::open(&path).unwrap();
        db.add(
            &NewMemory::new(MemoryKind::Semantic, "Durable")
                .body("This must still be here")
                .tags(vec!["persist".to_owned()]),
        )
        .unwrap()
        .id
    };

    {
        let db = Database::open(&path).unwrap();
        let memory = db.get(&id).unwrap();
        assert_eq!(memory.title, "Durable");
        assert_eq!(memory.body, "This must still be here");
        assert_eq!(memory.tags, vec!["persist"]);
        assert_eq!(indexed(&db), 1);
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_index_stays_consistent_after_a_thousand_writes_and_a_thousand_purges() {
    let db = Database::open_in_memory().unwrap();

    let mut ids = Vec::with_capacity(1_000);
    for index in 0..1_000 {
        let kind = match index % 4 {
            0 => MemoryKind::Semantic,
            1 => MemoryKind::Episodic,
            2 => MemoryKind::Procedural,
            _ => MemoryKind::Reference,
        };
        let outcome = db
            .add(
                &NewMemory::new(kind, format!("Memory number {index}"))
                    .body(format!(
                        "body for memory {index}, with a distinctive token tok{index}"
                    ))
                    .tags(vec![format!("group{}", index % 7)]),
            )
            .unwrap();
        assert!(!outcome.duplicate);
        ids.push(outcome.id);
    }

    assert_eq!(memories(&db), 1_000);
    assert_eq!(indexed(&db), 1_000);
    let integrity = db.integrity().unwrap();
    assert!(integrity.is_ok(), "{:?}", integrity.problems());

    // Purge in a different order than the writes, so a trigger that only works
    // sequentially cannot pass by accident.
    let purge: Vec<String> = ids.iter().step_by(3).cloned().collect();
    let outcome = db.forget(&purge).unwrap();
    assert_eq!(outcome.deleted.len(), purge.len());
    assert_eq!(outcome.missing, [] as [std::string::String; 0]);

    let remaining = 1_000 - purge.len();
    let expected = i64::try_from(remaining).unwrap();
    assert_eq!(memories(&db), expected);
    assert_eq!(
        indexed(&db),
        expected,
        "the index must track the table exactly"
    );

    let integrity = db.integrity().unwrap();
    assert!(
        integrity.is_ok(),
        "a wrong FTS trigger is what this catches: {:?}",
        integrity.problems()
    );

    // And a purge of everything leaves both empty rather than merely consistent.
    let all: Vec<String> = ids;
    db.forget(&all).unwrap();
    assert_eq!(memories(&db), 0);
    assert_eq!(indexed(&db), 0);
    assert!(db.integrity().unwrap().is_ok());
}

#[test]
fn a_pending_memory_is_never_in_the_index() {
    // The review inbox's whole guarantee, asserted at the level a caller can see.
    let db = Database::open_in_memory().unwrap();
    let draft = db
        .add(
            &NewMemory::new(MemoryKind::Semantic, "Unapproved")
                .body("a distinctive phrase that must not be findable")
                .source("agent_reflection")
                .confidence(0.2),
        )
        .unwrap();

    assert_eq!(draft.status, Status::PendingApproval);
    assert_eq!(indexed(&db), 0);
    assert_eq!(memories(&db), 1);
    assert!(db.integrity().unwrap().is_ok());

    // It is still readable by id — a review inbox has to show what is in it.
    assert_eq!(db.get(&draft.id).unwrap().title, "Unapproved");
}

#[test]
fn every_kind_round_trips_with_its_own_envelope() {
    let db = Database::open_in_memory().unwrap();
    let cases: [(MemoryKind, serde_json::Value); 4] = [
        (
            MemoryKind::Reference,
            serde_json::json!({"domain": "docs", "arbitrary": {"nested": true}}),
        ),
        (
            MemoryKind::Semantic,
            serde_json::json!({"entities": ["a"], "claims": ["b"], "source_refs": ["c"]}),
        ),
        (
            MemoryKind::Procedural,
            serde_json::json!({"trigger": "on release", "steps": ["one"], "priority": "low"}),
        ),
        (
            MemoryKind::Episodic,
            serde_json::json!({
                "occurred_at": "2026-10-14T09:00:00Z",
                "participants": ["dip"],
                "outcome": "shipped",
            }),
        ),
    ];

    for (kind, envelope) in cases {
        let outcome = db
            .add(
                &NewMemory::new(kind, format!("{kind} memory"))
                    .body("body")
                    .metadata(Some(envelope.clone())),
            )
            .unwrap();
        assert!(
            outcome.warnings.is_empty(),
            "{kind}: {:?}",
            outcome.warnings
        );
        let stored = db.get(&outcome.id).unwrap();
        assert_eq!(stored.kind, kind);
        assert_eq!(stored.memory_metadata, envelope, "{kind}");
    }
}

#[test]
fn the_contract_exit_codes_are_what_the_store_raises() {
    let db = Database::open_in_memory().unwrap();

    // 2 — not found.
    let missing = db.get("k_01JC9W5K2Q").unwrap_err();
    assert_eq!(missing.exit_code(), ExitCode::NotFound);
    assert_eq!(missing.code(), "not_found");

    // 3 — the request was wrong.
    let empty_title = db
        .add(&NewMemory::new(MemoryKind::Semantic, " "))
        .unwrap_err();
    assert_eq!(empty_title.exit_code(), ExitCode::InvalidArgs);
    assert_eq!(empty_title.code(), "invalid_args");

    let too_many_tags = db
        .add(
            &NewMemory::new(MemoryKind::Semantic, "T")
                .tags((0..=MAX_TAGS).map(|index| format!("tag{index}")).collect()),
        )
        .unwrap_err();
    assert_eq!(too_many_tags.exit_code(), ExitCode::InvalidArgs);

    // 2 — the workspace does not exist.
    let no_workspace = db
        .add(&NewMemory::new(MemoryKind::Semantic, "T").workspace("ghost"))
        .unwrap_err();
    assert_eq!(no_workspace.exit_code(), ExitCode::NotFound);
}

#[test]
fn a_review_workspace_holds_agent_writes_until_they_are_approved() {
    // Approval is WP4; what WP1 owes is that the draft is stored, unreachable, and
    // carries the status a caller can act on.
    let db = Database::open_in_memory().unwrap();
    db.create_workspace("Careful", ReviewMode::Review).unwrap();
    let outcome = db
        .add(
            &NewMemory::new(MemoryKind::Semantic, "Held")
                .body("body")
                .workspace("careful"),
        )
        .unwrap();
    assert_eq!(outcome.status, Status::PendingApproval);
    assert_eq!(indexed(&db), 0);

    let inbox = db
        .list(&ListQuery {
            scope: Scope::One("careful".to_owned()),
            status: Some(Status::PendingApproval),
            ..ListQuery::default()
        })
        .unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].id, outcome.id);
}

#[test]
fn an_edit_is_visible_to_the_next_read_and_to_dedupe() {
    let db = Database::open_in_memory().unwrap();
    let id = db
        .add(&NewMemory::new(MemoryKind::Semantic, "Before").body("old body"))
        .unwrap()
        .id;

    let outcome = db
        .update(
            &id,
            &MemoryPatch {
                title: Some("After".to_owned()),
                body: Some("new body".to_owned()),
                ..MemoryPatch::default()
            },
        )
        .unwrap();
    assert_eq!(outcome.changed, 2);

    let stored = db.get(&id).unwrap();
    assert_eq!(stored.title, "After");
    assert_eq!(stored.body, "new body");

    // The old text is now a new memory, and the new text is a duplicate.
    let old_text = db
        .add(&NewMemory::new(MemoryKind::Semantic, "Before").body("old body"))
        .unwrap();
    assert!(!old_text.duplicate);
    let new_text = db
        .add(&NewMemory::new(MemoryKind::Semantic, "After").body("new body"))
        .unwrap();
    assert!(new_text.duplicate);
    assert_eq!(new_text.id, id);
}

#[test]
fn a_workspace_can_be_reached_by_its_id_or_its_name() {
    let dir = tempdir();
    let path: &Path = &dir.join("brain.db");
    let db = Database::open(path).unwrap();
    let workspace = db
        .create_workspace("Client: Acme", ReviewMode::Autonomous)
        .unwrap();
    assert_eq!(workspace.id, "client-acme");

    for spelling in ["client-acme", "Client: Acme", "CLIENT: ACME"] {
        let found = db.resolve_workspace(spelling).unwrap();
        assert_eq!(found.id, "client-acme", "{spelling}");
    }

    let err = db.resolve_workspace("acme").unwrap_err();
    assert_eq!(err.exit_code(), ExitCode::NotFound);
    assert!(err.to_string().contains("client-acme"), "{err}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_failure_to_open_names_the_path() {
    // A directory is not a database. The point of the test is the message, because the
    // action a user has to take is in the path.
    let dir = tempdir();
    let err = Database::open(&dir).unwrap_err();
    assert!(
        matches!(err, Error::Config(_) | Error::Database(_)),
        "{err:?}"
    );
    assert!(
        err.to_string().contains(&dir.display().to_string()),
        "{err}"
    );
    std::fs::remove_dir_all(&dir).ok();
}
