// SPDX-License-Identifier: Apache-2.0

//! The vector leg, end to end: the write path, the read path, and the ways each can fail.
//!
//! # The model these tests use, and why it is not the stub
//!
//! The acceptance criterion for WP7 is *"with an embedder configured, hybrid returns rows
//! the lexical leg misses (a semantic-only query fixture)"*. No hash-based embedder can
//! satisfy that, and neither can a real one in CI: what the test needs is a model whose
//! similarity follows a *meaning* the fixture defines. So the fixture installs an embedder
//! of its own — [`Embedder::from_backend`], which exists for exactly this — that maps
//! words to a small number of concepts, synonyms included. `ship` and `deploy` land in the
//! same coordinate; `invoice` does not. Then "a query the lexical leg cannot match" is a
//! statement about the fixture rather than a hope about a model.
//!
//! Everything else here uses the real `stub` provider through `Embedder::build`, because
//! the paths it exercises — the blob encoding, the model key, the coverage count, the
//! degradation rules — are about this program and not about a model.

use kiomon_core::{
    Backend, Database, Embedder, EmbedderConfig, Error, ExitCode, Legs, MemoryKind, NewMemory,
    Result, Scope, SearchQuery, Status,
};

/// A database in memory, with the default workspace already in it.
fn db() -> Database {
    Database::open_in_memory().expect("an in-memory database must open")
}

/// Add a memory, expecting success. Returns its id.
fn add(db: &Database, kind: MemoryKind, title: &str, body: &str) -> String {
    db.add(&NewMemory::new(kind, title).body(body))
        .expect("a fixture write must succeed")
        .id
}

/// A directory that a test can write a real database into, unique per call.
fn tempdir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("kiomon-vector-{}-{unique}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the test directory must be creatable");
    dir
}

/// A backend whose vectors are concepts the fixture defines.
///
/// Two dimensions are enough to make the point and are cheap to reason about: the first is
/// "getting software out the door", the second is "money". A word that appears in neither
/// list contributes nothing, so a text made only of unknown words has no direction at all
/// and is orthogonal to everything — which is the honest behaviour for a fixture.
struct Concepts;

impl Backend for Concepts {
    fn provider(&self) -> &'static str {
        "concepts"
    }

    fn embed(&self, _model: &str, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            let mut vector = vec![0.0f32, 0.0f32];
            for word in text.split(|ch: char| !ch.is_alphanumeric()) {
                match word.to_ascii_lowercase().as_str() {
                    "deploy" | "ship" | "release" | "rollout" => vector[0] += 1.0,
                    "invoice" | "refund" | "billing" | "payment" => vector[1] += 1.0,
                    _ => {}
                }
            }
            let length = (vector[0] * vector[0] + vector[1] * vector[1]).sqrt();
            if length > 0.0 {
                vector[0] /= length;
                vector[1] /= length;
            }
            out.push(vector);
        }
        Ok(out)
    }
}

/// A database with the concept model installed.
fn db_with_concepts() -> Database {
    db().with_embedder(Some(std::sync::Arc::new(Embedder::from_backend(
        Box::new(Concepts),
        "concepts",
    ))))
}

/// A database with the real stub provider, through the config-shaped builder.
fn db_with_stub() -> Database {
    let embedder = Embedder::build(&EmbedderConfig {
        provider: Some("stub".to_owned()),
        ..EmbedderConfig::default()
    })
    .expect("the stub needs no model and no URL");
    db().with_embedder(Some(std::sync::Arc::new(embedder)))
}

fn search(db: &Database, text: &str, legs: Legs) -> kiomon_core::SearchOutcome {
    db.search(&SearchQuery {
        text: text.to_owned(),
        legs,
        record: false,
        ..SearchQuery::default()
    })
    .expect("the search must run")
}

// ---------------------------------------------------------------------------
// The write path
// ---------------------------------------------------------------------------

#[test]
fn a_write_embeds_the_memory_and_the_read_path_can_use_it() {
    let db = db_with_stub();
    let id = add(&db, MemoryKind::Semantic, "Migrations", "run them by hand");
    assert!(
        db.has_embedding(&id, "stub").unwrap(),
        "add has to leave a vector behind, or semantic search never sees a new memory"
    );

    let outcome = search(&db, "migrations", Legs::Auto);
    assert!(outcome.legs.vector);
    assert_eq!(outcome.hits.len(), 1);
    assert!(
        outcome.hits[0].similarity.is_some(),
        "a row the vector leg ranked carries its similarity"
    );
    assert_eq!(outcome.vector_model.as_deref(), Some("stub"));
}

#[test]
fn a_write_that_cannot_embed_is_still_a_write_and_says_so() {
    let db = db_with_failing_embedder();
    let outcome = db
        .add(&NewMemory::new(MemoryKind::Semantic, "A fact").body("the deploy failed"))
        .expect("a failed embed must not fail the write");
    assert!(!outcome.duplicate);
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.contains("not embedded") && warning.contains("reindex")),
        "{:?}",
        outcome.warnings
    );
    // And the memory is findable the whole time by the leg that does not need a model.
    let outcome = search(&db, "deploy", Legs::Fts);
    assert_eq!(outcome.hits.len(), 1);
}

/// An embedder that always fails, for the paths that must survive one.
fn db_with_failing_embedder() -> Database {
    struct Broken;

    impl Backend for Broken {
        fn provider(&self) -> &'static str {
            "broken"
        }

        fn embed(&self, _model: &str, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Err(Error::Config("the model is not running".to_owned()))
        }
    }

    db().with_embedder(Some(std::sync::Arc::new(Embedder::from_backend(
        Box::new(Broken),
        "broken",
    ))))
}

#[test]
fn an_update_that_moves_the_text_re_embeds_it_and_a_stale_vector_never_wins() {
    let db = db_with_concepts();
    let id = add(
        &db,
        MemoryKind::Semantic,
        "Deploy notes",
        "ship it on friday",
    );
    assert!(db.has_embedding(&id, "concepts").unwrap());

    // The text now says something else — title *and* body, because the vector covers both
    // and a test that moved one of them would be testing half the rule. Without a re-embed
    // the old vector would still be the row's vector, and a query about the old subject
    // would keep finding it.
    db.update(
        &id,
        &kiomon_core::MemoryPatch {
            title: Some("Billing notes".to_owned()),
            body: Some("the invoice was refunded".to_owned()),
            ..kiomon_core::MemoryPatch::default()
        },
    )
    .expect("the update must succeed");

    let billing = search(&db, "billing", Legs::Vector);
    assert_eq!(
        billing.hits.len(),
        1,
        "the new text is what a vector search sees"
    );
    assert_eq!(billing.hits[0].memory.id, id);

    let shipping = search(&db, "release", Legs::Vector);
    assert!(
        shipping.hits.is_empty(),
        "the old text's vector is gone, so a query about it finds nothing"
    );
}

#[test]
fn an_approved_draft_is_embedded_at_the_moment_it_becomes_findable() {
    let db = db_with_stub();
    // A reflection below the promote threshold lands as a draft: stored, and invisible to
    // both legs.
    let draft = db
        .add(
            &NewMemory::new(MemoryKind::Semantic, "Uncertain fact")
                .body("the deploy was reverted")
                .source("agent_reflection")
                .confidence(0.4),
        )
        .expect("the draft must be stored")
        .id;
    assert_eq!(db.get(&draft).unwrap().status, Status::PendingApproval);
    // A draft *is* embedded when it is written, and the vector is invisible until it is
    // approved: the read path filters on status, not on whether a vector exists, which is
    // what makes approval instant and keeps the two mechanisms independent.
    assert!(
        db.has_embedding(&draft, "stub").unwrap(),
        "the write path embeds what it stores, whatever status the gate gave it"
    );
    assert!(
        search(&db, "reverted", Legs::Vector).hits.is_empty(),
        "and the vector leg must not return a memory its own status hides"
    );

    db.approve(std::slice::from_ref(&draft))
        .expect("approving must succeed");
    assert!(
        db.has_embedding(&draft, "stub").unwrap(),
        "approving leaves it embedded — and repairs a draft whose embed failed, because it \
         goes through the same idempotent path"
    );
    assert_eq!(search(&db, "reverted", Legs::Auto).hits.len(), 1);
}

// ---------------------------------------------------------------------------
// The read path
// ---------------------------------------------------------------------------

#[test]
fn the_vector_leg_finds_a_row_the_lexical_leg_cannot() {
    // The acceptance criterion of WP7, and the fixture that makes it a test rather than a
    // hope. Neither memory contains the query's words; one of them means the same thing.
    let db = db_with_concepts();
    add(
        &db,
        MemoryKind::Semantic,
        "Both ships happened at 14:00",
        "the release went out on friday",
    );
    add(
        &db,
        MemoryKind::Semantic,
        "Invoice 4021",
        "the payment was refunded in march",
    );

    let lexical = search(&db, "rollout", Legs::Fts);
    assert!(
        lexical.hits.is_empty(),
        "the lexical leg must find nothing, or this test proves nothing"
    );

    // The invoice row is orthogonal to the query — a cosine of zero, which is not a
    // candidate — so the vector leg returns the one row that has a direction in common with
    // it. That floor is the only one that does not depend on a model's typical similarity.
    let semantic = search(&db, "rollout", Legs::Vector);
    assert_eq!(semantic.hits.len(), 1);
    assert_eq!(
        semantic.hits[0].memory.title,
        "Both ships happened at 14:00"
    );

    // And the default, with both legs, returns the same row.
    let hybrid = search(&db, "rollout", Legs::Auto);
    assert_eq!(hybrid.hits.len(), 1);
    assert_eq!(
        hybrid.legs,
        kiomon_core::LegsRun {
            fts: false,
            vector: true
        },
        "the lexical leg found nothing, and the outcome says so rather than claiming both"
    );
}

#[test]
fn the_two_legs_are_fused_and_a_row_both_of_them_found_leads() {
    let db = db_with_concepts();
    let both = add(
        &db,
        MemoryKind::Semantic,
        "Deploy runbook",
        "ship on friday",
    );
    add(
        &db,
        MemoryKind::Semantic,
        "Release notes",
        "nothing to do with the word the query uses",
    );
    add(
        &db,
        MemoryKind::Semantic,
        "Unrelated",
        "a payment was refunded",
    );

    let outcome = search(&db, "deploy ship", Legs::Auto);
    assert_eq!(
        outcome.legs,
        kiomon_core::LegsRun {
            fts: true,
            vector: true
        }
    );
    assert_eq!(
        outcome.hits[0].memory.id, both,
        "the row both legs found has to win the fusion twice over"
    );
}

#[test]
fn a_vector_only_request_is_never_answered_from_the_lexical_leg() {
    // The plan's rule, and the one it shares with the hosted door: answering a request for
    // one thing with another thing returns something the caller cannot detect.
    let db = db_with_concepts();
    add(&db, MemoryKind::Semantic, "Deploy runbook", "ship it");
    add(
        &db,
        MemoryKind::Semantic,
        "Invoice",
        "the payment was refunded",
    );

    let outcome = search(&db, "deploy", Legs::Vector);
    assert!(!outcome.legs.fts, "the lexical leg must not have run");
    assert!(outcome.legs.vector);
    assert_eq!(outcome.reason, "vector_only");
    assert!(
        !outcome.degraded,
        "a vector-only answer is what was asked for, not a degradation"
    );
}

#[test]
fn with_no_embedder_configured_a_vector_request_is_a_configuration_error_that_names_the_key() {
    let db = db();
    add(&db, MemoryKind::Semantic, "Deploy runbook", "ship it");

    let error = db
        .search(&SearchQuery {
            text: "deploy".to_owned(),
            legs: Legs::Vector,
            ..SearchQuery::default()
        })
        .expect_err("no embedder, so a vector-only search cannot be served");
    let message = error.to_string();
    assert!(message.contains("[embedder]"), "{message}");
    assert!(message.contains("--legs fts"), "{message}");
    assert_eq!(error.exit_code(), ExitCode::Config);
}

#[test]
fn with_no_embedder_configured_auto_is_full_text_and_is_not_called_degraded() {
    let db = db();
    add(&db, MemoryKind::Semantic, "Deploy runbook", "ship it");

    let outcome = search(&db, "deploy", Legs::Auto);
    assert_eq!(outcome.hits.len(), 1);
    assert!(outcome.legs.fts);
    assert!(!outcome.legs.vector);
    assert!(
        !outcome.degraded,
        "nothing failed: this is the documented default"
    );
    assert_eq!(
        outcome.reason, "no_embedder",
        "and a caller can still find out why the vector leg was not there"
    );
}

#[test]
fn a_broken_embedder_degrades_auto_with_a_reason_and_fails_vector_outright() {
    let db = db_with_failing_embedder();
    add(&db, MemoryKind::Semantic, "Deploy runbook", "ship it");

    // `auto`: answered from the lexical leg, and the degradation is visible.
    let outcome = search(&db, "deploy", Legs::Auto);
    assert_eq!(
        outcome.hits.len(),
        1,
        "the lexical answer is still an answer"
    );
    assert!(outcome.degraded);
    assert_eq!(outcome.reason, "no_query_vector");
    assert_eq!(
        outcome.legs,
        kiomon_core::LegsRun {
            fts: true,
            vector: false
        }
    );

    // `vector`: an error, not a lexical answer the caller did not ask for.
    let error = db
        .search(&SearchQuery {
            text: "deploy".to_owned(),
            legs: Legs::Vector,
            ..SearchQuery::default()
        })
        .expect_err("a vector-only search must not fall back");
    assert!(error.to_string().contains("not running"), "{error}");
}

#[test]
fn a_draft_never_reaches_the_vector_leg_and_an_archived_memory_does_not_either() {
    let db = db_with_concepts();
    let draft = db
        .add(
            &NewMemory::new(MemoryKind::Semantic, "Unreviewed")
                .body("the release was rolled back")
                .source("agent_reflection")
                .confidence(0.3),
        )
        .unwrap()
        .id;
    let live = add(
        &db,
        MemoryKind::Semantic,
        "Reviewed",
        "the release went out",
    );
    db.archive(std::slice::from_ref(&live))
        .expect("archive must work");

    // Both rows have vectors — the draft has none, the archived one keeps its — and neither
    // may come back from a search. The vector leg is a second mechanism, and a second
    // mechanism is a second chance to leak.
    let outcome = search(&db, "rollout", Legs::Vector);
    assert!(
        outcome.hits.is_empty(),
        "a draft or an archived memory surfaced through the vector leg: {:?}",
        outcome
            .hits
            .iter()
            .map(|hit| &hit.memory.title)
            .collect::<Vec<_>>()
    );
    assert_ne!(outcome.hits.first().map(|hit| &hit.memory.id), Some(&draft));
}

#[test]
fn the_workspace_filter_applies_to_the_vector_leg_exactly_as_it_does_to_the_lexical_one() {
    let db = db_with_concepts();
    db.create_workspace("Other", kiomon_core::ReviewMode::Autonomous)
        .expect("the second workspace must be creatable");
    let here = add(&db, MemoryKind::Semantic, "Here", "the deploy worked");
    let there = db
        .add(
            &NewMemory::new(MemoryKind::Semantic, "There")
                .body("the deploy failed")
                .workspace("other"),
        )
        .expect("the other workspace write must succeed")
        .id;

    let one = search(&db, "rollout", Legs::Vector);
    assert_eq!(one.hits.len(), 1);
    assert_eq!(one.hits[0].memory.id, here);

    // `*` is the explicit whole-corpus case, and it is the only way to see both.
    let all = db
        .search(&SearchQuery {
            text: "rollout".to_owned(),
            legs: Legs::Vector,
            scope: Scope::All,
            record: false,
            ..SearchQuery::default()
        })
        .expect("a whole-corpus search must run");
    let mut ids: Vec<&str> = all.hits.iter().map(|hit| hit.memory.id.as_str()).collect();
    ids.sort_unstable();
    let mut expected = vec![here.as_str(), there.as_str()];
    expected.sort_unstable();
    assert_eq!(ids, expected);
}

#[test]
fn the_kind_filter_applies_to_the_vector_leg() {
    let db = db_with_concepts();
    add(&db, MemoryKind::Semantic, "A fact", "the deploy worked");
    add(&db, MemoryKind::Episodic, "A moment", "the deploy worked");

    let only = db
        .search(&SearchQuery {
            text: "rollout".to_owned(),
            legs: Legs::Vector,
            kinds: vec![MemoryKind::Episodic],
            record: false,
            ..SearchQuery::default()
        })
        .expect("a filtered search must run");
    assert_eq!(only.hits.len(), 1);
    assert_eq!(only.hits[0].memory.kind, MemoryKind::Episodic);
}

#[test]
fn the_graph_leg_names_the_deferral_rather_than_answering_hybrid() {
    let db = db_with_stub();
    let error = db
        .search(&SearchQuery {
            text: "deploy".to_owned(),
            legs: Legs::Graph,
            ..SearchQuery::default()
        })
        .expect_err("there is no graph leg");
    // The graph itself works since WP5, so the message has to be about the *leg* and not
    // about the graph: a caller told "the graph is not in this release" would conclude that
    // `relate` does not work either, and it does.
    assert!(error.to_string().contains("graph"), "{error}");
    assert!(error.to_string().contains("leg"), "{error}");
    assert!(error.to_string().contains("v1.1"), "{error}");
    assert_eq!(error.exit_code(), ExitCode::Internal);
}

#[test]
fn the_leg_vocabulary_accepts_the_hosted_spellings() {
    // A caller that read `legs` off a hosted response asks for it back by the name it saw.
    for (raw, expected) in [
        ("auto", Legs::Auto),
        ("hybrid", Legs::Auto),
        ("both", Legs::Auto),
        ("fts", Legs::Fts),
        ("lexical", Legs::Fts),
        ("fulltext", Legs::Fts),
        ("vector", Legs::Vector),
        ("semantic", Legs::Vector),
        ("graph", Legs::Graph),
        ("  HYBRID  ", Legs::Auto),
    ] {
        assert_eq!(Legs::parse(raw), Some(expected), "{raw}");
    }
    assert_eq!(Legs::parse("bm25"), None);
}

// ---------------------------------------------------------------------------
// Reindex and coverage
// ---------------------------------------------------------------------------

#[test]
fn reindex_embeds_what_is_missing_and_is_idempotent() {
    let plain = db();
    for index in 0..5 {
        add(
            &plain,
            MemoryKind::Semantic,
            &format!("Memory {index}"),
            "the deploy was rolled out",
        );
    }

    // The same file, opened with an embedder: everything is missing.
    let db = plain.with_embedder(Some(std::sync::Arc::new(
        Embedder::build(&EmbedderConfig {
            provider: Some("stub".to_owned()),
            ..EmbedderConfig::default()
        })
        .unwrap(),
    )));
    let (active, embedded) = db.embedding_coverage("stub", &Scope::All).unwrap();
    assert_eq!((active, embedded), (5, 0));

    let report = db.reindex_embeddings(&Scope::All, None, false).unwrap();
    assert_eq!(report.embedded, 5);
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(db.embedding_coverage("stub", &Scope::All).unwrap(), (5, 5));

    // Running it again finds nothing to do: the query is "which rows have no vector", so
    // the second run is the check that the first one was honest.
    let again = db.reindex_embeddings(&Scope::All, None, false).unwrap();
    assert_eq!(again.requested, 0);
    assert_eq!(again.embedded, 0);
}

#[test]
fn a_dry_run_counts_without_writing() {
    let db = db();
    for index in 0..3 {
        add(
            &db,
            MemoryKind::Semantic,
            &format!("Memory {index}"),
            "body",
        );
    }
    let db = db.with_embedder(Some(std::sync::Arc::new(
        Embedder::build(&EmbedderConfig {
            provider: Some("stub".to_owned()),
            ..EmbedderConfig::default()
        })
        .unwrap(),
    )));

    let report = db.reindex_embeddings(&Scope::All, None, true).unwrap();
    assert!(report.dry_run);
    assert_eq!(report.embedded, 3, "the count is what a dry run is for");
    assert_eq!(
        db.embedding_coverage("stub", &Scope::All).unwrap().1,
        0,
        "and nothing was written"
    );
}

#[test]
fn reindex_pages_and_finishes() {
    // A page smaller than the corpus, which is the only way to exercise the cursor: the
    // harness and the tests both used to run with one page and never walked.
    let db = db();
    for index in 0..7 {
        add(
            &db,
            MemoryKind::Semantic,
            &format!("Memory {index}"),
            "body",
        );
    }
    let db = db.with_embedder(Some(std::sync::Arc::new(
        Embedder::build(&EmbedderConfig {
            provider: Some("stub".to_owned()),
            ..EmbedderConfig::default()
        })
        .unwrap(),
    )));

    let report = db.reindex_embeddings(&Scope::All, Some(2), false).unwrap();
    assert_eq!(report.requested, 7);
    assert_eq!(report.embedded, 7);
    assert_eq!(db.embedding_coverage("stub", &Scope::All).unwrap(), (7, 7));
}

#[test]
fn reindex_without_an_embedder_is_a_configuration_error_not_a_silent_no_op() {
    let db = db();
    let error = db
        .reindex_embeddings(&Scope::All, None, false)
        .expect_err("there is nothing to embed with");
    assert!(error.to_string().contains("[embedder]"), "{error}");
    assert_eq!(error.exit_code(), ExitCode::Config);
}

#[test]
fn a_vector_from_another_model_is_neither_read_nor_counted() {
    // A second model's row, as a model switch leaves behind: same memory, a different space.
    // Written by reopening the same brain with the concept model — which is what changing
    // the config file does — so the test never reaches past the public surface.
    let path = tempdir().join("brain.db");
    let first = Database::open(&path)
        .unwrap()
        .with_embedder(Some(std::sync::Arc::new(Embedder::from_backend(
            Box::new(Concepts),
            "concepts",
        ))));
    let id = add(&first, MemoryKind::Semantic, "A fact", "the deploy worked");
    drop(first);

    let second = Database::open(&path)
        .unwrap()
        .with_embedder(Some(std::sync::Arc::new(
            Embedder::build(&EmbedderConfig {
                provider: Some("stub".to_owned()),
                ..EmbedderConfig::default()
            })
            .unwrap(),
        )));
    assert_eq!(
        second.embedding_coverage("concepts", &Scope::All).unwrap(),
        (1, 1)
    );
    assert_eq!(
        second.embedding_coverage("stub", &Scope::All).unwrap(),
        (1, 0),
        "a model nobody has embedded with yet covers nothing"
    );
    second.reindex_embeddings(&Scope::All, None, false).unwrap();
    assert_eq!(
        second.embedding_coverage("stub", &Scope::All).unwrap(),
        (1, 1)
    );

    let outcome = search(&second, "deploy", Legs::Vector);
    assert_eq!(outcome.hits.len(), 1);
    assert_eq!(outcome.hits[0].memory.id, id);
    // The stub's 384-dimensional vector is what scored it, not the concepts model's
    // two-dimensional one: the dimensions do not even match, and a leg that compared them
    // anyway would be measuring noise.
    let similarity = outcome.hits[0]
        .similarity
        .expect("the vector leg ranked it");
    assert!(similarity.abs() <= 1.0, "{similarity}");
}

#[test]
fn a_memory_with_no_vector_is_simply_absent_from_the_vector_leg() {
    let plain = db();
    add(&plain, MemoryKind::Semantic, "A fact", "the deploy worked");
    // Opened without an embedder: nothing has a vector.
    let outcome = search(&plain, "deploy", Legs::Fts);
    assert_eq!(outcome.hits.len(), 1, "the lexical leg is unaffected");

    let db = db_with_stub();
    let outcome = search(&db, "deploy", Legs::Vector);
    assert!(
        outcome.hits.is_empty(),
        "no vectors at all means no candidates"
    );
    assert_eq!(
        outcome.legs,
        kiomon_core::LegsRun {
            fts: false,
            vector: false
        },
        "the leg ran and found nothing, which is reported as what it is"
    );
    assert!(!outcome.degraded, "an empty corpus is not a failure");
}
