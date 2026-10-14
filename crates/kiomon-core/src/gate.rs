// SPDX-License-Identifier: Apache-2.0

//! The write gate: which writes activate immediately, and which wait for review.
//!
//! Pure policy, no database. It is a separate module from [`crate::store`] because it
//! is the one decision the CLI, a future MCP server, and the auto-linker all have to
//! agree about, and agreement is easier to keep between a function and its callers
//! than between two copies of a rule.
//!
//! The rule is the hosted product's (`platform/worker/src/lib/write-gate.ts`), kept
//! exactly so that a local brain and a hosted brain admit writes the same way.

use crate::model::ReviewMode;

/// The confidence at which an agent's own reflection activates without review.
///
/// Named rather than inlined for the reason the hosted repository learned the hard
/// way: an unnamed threshold becomes a permanent argument, and a threshold with a name
/// can be measured and moved.
pub const AUTOPROMOTE_THRESHOLD: f64 = 0.75;

/// Sources trusted enough to activate without review, whatever their confidence.
///
/// Each of these is a write the user performed or explicitly asked for: they ran the
/// command, they imported the file, they connected the source. Holding those back
/// would make the tool refuse to record what it was told, which is a worse failure
/// than admitting a wrong fact that the review inbox was never going to catch.
///
/// `import` is on this list and is not on the hosted product's, because importing a
/// file *is* the local equivalent of a connector sync: the content is the user's and
/// an inbox full of their own files is a queue nobody asked for.
pub const HIGH_AUTHORITY_SOURCES: &[&str] =
    &["user_saved", "import", "connector_sync", "email_inbound"];

/// The source whose confidence has to be earned.
pub const REFLECTION_SOURCE: &str = "agent_reflection";

/// What the gate decided about a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Index it, link it, and make it retrievable now.
    Activate,
    /// Store it without an index row, and wait for `review approve`.
    Review,
}

impl Verdict {
    /// The status a write lands in under this verdict.
    #[must_use]
    pub const fn status(self) -> crate::model::Status {
        match self {
            Self::Activate => crate::model::Status::Active,
            Self::Review => crate::model::Status::PendingApproval,
        }
    }
}

/// Whether a write from `source` at `confidence` activates without review.
///
/// An unrecognised source never auto-activates. That direction is deliberate: a new
/// provenance value added by a future feature should default to being reviewed, so the
/// failure of forgetting to classify it is a slightly tedious inbox rather than a
/// memory nobody checked.
#[must_use]
pub fn should_auto_activate(source: &str, confidence: Option<f64>) -> bool {
    if HIGH_AUTHORITY_SOURCES.contains(&source) {
        return true;
    }
    if source == REFLECTION_SOURCE {
        return confidence.is_some_and(|value| value >= AUTOPROMOTE_THRESHOLD);
    }
    false
}

/// The full decision, including the workspace's review mode.
#[must_use]
pub fn decide(source: &str, confidence: Option<f64>, review_mode: ReviewMode) -> Verdict {
    if review_mode == ReviewMode::Review {
        return Verdict::Review;
    }
    if should_auto_activate(source, confidence) {
        Verdict::Activate
    } else {
        Verdict::Review
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Status;

    #[test]
    fn a_user_write_always_activates() {
        assert!(should_auto_activate("user_saved", None));
        assert!(should_auto_activate("import", None));
        assert!(should_auto_activate("connector_sync", None));
    }

    #[test]
    fn an_agent_reflection_has_to_earn_it() {
        assert!(should_auto_activate("agent_reflection", Some(0.75)));
        assert!(should_auto_activate("agent_reflection", Some(1.0)));
        assert!(!should_auto_activate("agent_reflection", Some(0.74)));
        assert!(!should_auto_activate("agent_reflection", None));
    }

    #[test]
    fn the_threshold_is_the_documented_one() {
        // In the contract and in `add --help`. Asserting the literal means moving it is
        // a failing test rather than a quietly different product.
        assert!((AUTOPROMOTE_THRESHOLD - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn an_unrecognised_source_is_reviewed_rather_than_trusted() {
        // The safe direction for a value a future feature has not classified yet.
        assert!(!should_auto_activate("something_new", Some(1.0)));
        assert!(!should_auto_activate("", Some(1.0)));
    }

    #[test]
    fn review_mode_holds_back_even_a_certain_write() {
        let verdict = decide("user_saved", Some(1.0), ReviewMode::Review);
        assert_eq!(verdict, Verdict::Review);
        assert_eq!(verdict.status(), Status::PendingApproval);
    }

    #[test]
    fn autonomous_mode_lets_a_high_authority_write_through() {
        let verdict = decide("user_saved", None, ReviewMode::Autonomous);
        assert_eq!(verdict, Verdict::Activate);
        assert_eq!(verdict.status(), Status::Active);
    }
}
