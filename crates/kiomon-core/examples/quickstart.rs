// SPDX-License-Identifier: Apache-2.0

//! The Rust SDK, which is this crate: `kiomon-core` in process, no socket and no server.
//!
//! ```text
//! cargo run --example quickstart -p kiomon-core
//! ```
//!
//! An in-memory database, so the example leaves nothing behind. Swap
//! [`Database::open_in_memory`] for `Database::open(path)` and the same code is a real brain —
//! that is the whole difference between this and `kiomon serve`.
//!
//! The order is the order an agent uses: write, retrieve, act, report. The `retrieve` call is
//! the important one: it returns a [`ContextPacket`], not a list of hits.

use kiomon_core::{
    Assessment, ContextPacket, Database, Intent, MemoryKind, NewMemory, Rating, RetrieveQuery,
    Scope, SearchQuery, Status,
};

fn main() -> kiomon_core::Result<()> {
    let db = Database::open_in_memory()?;

    // ── Write ──────────────────────────────────────────────────────────────
    // A user save activates immediately: the source carries the authority, not a flag.
    let procedure = db.add(
        &NewMemory::new(MemoryKind::Procedural, "Roll back a bad deploy")
            .body("Promote the previous tag, then run the migrations that tag expects.")
            .excerpt("Previous tag first, migrations second.")
            .tags(vec!["deploy".to_owned(), "rollback".to_owned()])
            .confidence(0.9),
    )?;
    println!(
        "wrote {} [{}] as {}",
        procedure.id,
        MemoryKind::Procedural.as_str(),
        procedure.status.as_str()
    );

    // A reflection below the gate is stored but not indexed, and the answer says so.
    let reflection = db.add(
        &NewMemory::new(MemoryKind::Semantic, "The worker retries twice")
            .body("Unverified: a failed job appears to retry twice before dead-lettering.")
            .source("agent_reflection")
            .confidence(0.4),
    )?;
    println!(
        "wrote {} as {} (below the {} gate)",
        reflection.id,
        reflection.status.as_str(),
        kiomon_core::AUTOPROMOTE_THRESHOLD
    );
    assert_eq!(reflection.status, Status::PendingApproval);

    // ── Search ─────────────────────────────────────────────────────────────
    // `search` ranks by words. `record: true` counts the retrieval, which is what decay reads
    // when it decides a memory is still in use.
    let found = db.search(&SearchQuery {
        text: "roll back a deploy".to_owned(),
        scope: Scope::One("default".to_owned()),
        limit: 5,
        record: true,
        ..SearchQuery::default()
    })?;
    println!(
        "
search: {} hit(s), attempt {:?}",
        found.hits.len(),
        found.attempt
    );
    for hit in &found.hits {
        println!(
            "  {:.3}  {}  {}",
            hit.score,
            hit.memory.kind.as_str(),
            hit.memory.title
        );
    }

    // ── Retrieve ───────────────────────────────────────────────────────────
    // The packet: facts, at most one procedure, episodes, evidence, warnings. `intent` is a
    // re-ranking, not a filter — the same memories in a different order for a different job.
    let packet: ContextPacket = db.retrieve(&RetrieveQuery {
        intent: Intent::Execute,
        text: "how do I roll back a bad deploy?".to_owned(),
        scope: Scope::One("default".to_owned()),
        ..RetrieveQuery::default()
    })?;
    println!("\nretrieve: {}", packet.summary);
    for fact in &packet.facts {
        println!("  fact      {:.2}  {}", fact.confidence, fact.claim);
    }
    if let Some(procedure) = &packet.procedure {
        println!("  procedure {}", procedure.claim);
    }
    for warning in &packet.warnings {
        println!("  warning   {warning}");
    }
    // The ids the packet handed out are the ones worth reporting back.
    assert!(packet.retrieved_ids.contains(&procedure.id));

    // ── Feedback ───────────────────────────────────────────────────────────
    // Outcomes credit the memories that were actually used; a rating is the coarser signal.
    // Neither touches `strength` — decay owns that.
    db.rate(&procedure.id, Rating::Up)?;
    let report = db.record_outcome(
        kiomon_core::TaskOutcome::Success,
        &[(procedure.id.clone(), Assessment::Helpful)],
        &Scope::One("default".to_owned()),
    )?;
    println!(
        "
feedback: {} credited, {} skipped",
        report.applied.len(),
        report.skipped.len()
    );

    let after = db.get(&procedure.id)?;
    println!(
        "  confidence {:.2}, strength {:.2}, feedback {:+.2} over {}",
        after.confidence.unwrap_or_default(),
        after.strength,
        after.feedback_score,
        after.feedback_count
    );

    // ── Lifecycle ──────────────────────────────────────────────────────────
    // Pin exempts a memory from decay; forget deletes it and, by cascade, its edges.
    db.set_pinned(std::slice::from_ref(&procedure.id), true)?;
    println!(
        "\npinned: {} is now immune to decay",
        db.get(&procedure.id)?.pinned
    );

    let stats = db.stats(&Scope::One("default".to_owned()), &[])?;
    println!("brain: {} memories, {} pinned", stats.total, stats.pinned);

    Ok(())
}
