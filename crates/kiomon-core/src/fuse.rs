// SPDX-License-Identifier: Apache-2.0

//! Reciprocal rank fusion.
//!
//! One function, ported from the hosted product's `worker/src/lib/rrf.ts` and kept
//! verbatim in formula and constant, so a memory that ranks third locally ranks third
//! remotely. Fusion takes rank lists and returns ids with scores — it never touches a
//! row. That seam is deliberate: it is what lets a second or third leg be one more call
//! rather than another copy of the loop, and it is the seam the platform settled on for
//! the same reason.
//!
//! # Why rank and not score
//!
//! bm25 and cosine similarity are not comparable — one is unbounded and negative, the
//! other is bounded and positive — so a fusion over raw scores needs a normalisation
//! nobody agrees on. Reciprocal rank needs neither: a document's contribution depends
//! only on where it came, so a leg that is right about the top of its list is loud and a
//! leg that is merely confident is not.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

/// The fusion constant. Ported unchanged.
///
/// It sets how quickly a lower rank stops mattering: the difference between rank 0 and
/// rank 1 is 1/61 − 1/62, while the difference between rank 40 and 41 is 1/101 − 1/102.
/// 60 is the value from the original paper that the platform adopted, and matching it
/// matters more than justifying it — a different constant would reorder results against
/// the hosted product for no gain anybody could measure.
pub const RRF_K: u32 = 60;

/// The score one appearance at `rank` (zero-based) contributes.
///
/// `rank + 1` because the formula in the source is `1 / (RRF_K + rank + 1)` and `rank`
/// there is zero-based; keeping the expression identical to the port is worth more than
/// the saved addition.
///
/// The casts are exact rather than approximate: `RRF_K` is 60 and `rank` is a position in
/// a ranked window, bounded by `WINDOW_OVERFETCH * limit` at 150 today, so both are
/// integers an `f32` represents without loss — the point at which it stops doing so is
/// 2^24, and a ranked window of sixteen million rows is not a page anybody asked for.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn rrf_score(rank: usize) -> f32 {
    1.0 / (RRF_K as f32 + rank as f32 + 1.0)
}

/// Fuse ranked id lists into one ordering by reciprocal-rank sum, best first.
///
/// The output is truncated to `limit`, so a caller never pays for rows it will not use.
/// Ties keep the order in which the ids were first seen — which is the order of `lists`
/// — because the sort is stable. That is the same behaviour as the JavaScript original,
/// where `Array.prototype.sort` has been stable since ES2019, and it is what makes the
/// result deterministic when two ids have only ever been seen at the same rank in
/// different legs.
#[must_use]
pub fn fuse(lists: &[Vec<i64>], limit: usize) -> Vec<(i64, f32)> {
    let mut fused: Vec<(i64, f32)> = Vec::new();
    // An index alongside the vector rather than a map of id -> score, because the
    // result has to come out in insertion order for the stable sort to mean anything,
    // and a map would give that up.
    let mut seen: HashMap<i64, usize> = HashMap::new();

    for list in lists {
        for (rank, id) in list.iter().copied().enumerate() {
            let contribution = rrf_score(rank);
            match seen.entry(id) {
                Entry::Occupied(slot) => fused[*slot.get()].1 += contribution,
                Entry::Vacant(slot) => {
                    slot.insert(fused.len());
                    fused.push((id, contribution));
                }
            }
        }
    }

    // `total_cmp` rather than `partial_cmp().unwrap()`: the scores here are always
    // finite, but a comparison that cannot panic is the same speed and does not depend
    // on that being true forever.
    fused.sort_by(|a, b| b.1.total_cmp(&a.1));
    fused.truncate(limit);
    fused
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_score_is_the_documented_formula() {
        // The formula is the contract with the hosted product, so it is pinned by value
        // rather than by recomputing it the way the implementation does.
        // Compared bit for bit rather than with an epsilon: the assertion is that this
        // *is* the formula, and an approximate one would pass for a formula that was
        // merely close.
        assert_eq!(rrf_score(0).to_bits(), (1.0f32 / 61.0).to_bits());
        assert_eq!(rrf_score(1).to_bits(), (1.0f32 / 62.0).to_bits());
        assert_eq!(rrf_score(59).to_bits(), (1.0f32 / 120.0).to_bits());
        assert!(rrf_score(0) > rrf_score(1));
    }

    #[test]
    fn one_list_comes_back_in_the_order_it_went_in() {
        let fused = fuse(&[vec![7, 3, 9]], 10);
        assert_eq!(
            fused.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![7, 3, 9]
        );
    }

    #[test]
    fn a_row_in_both_lists_outranks_a_row_in_either_alone() {
        // Hand-computed: 20 is rank 1 in the first list and rank 0 in the second, so it
        // scores 1/62 + 1/61; 10 and 30 score 1/61 and 1/62. Second-sum wins even
        // though nothing ranks it first.
        let fused = fuse(&[vec![10, 20], vec![20, 30]], 10);
        assert_eq!(
            fused.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![20, 10, 30]
        );
        let top = fused[0].1;
        assert!((top - (1.0 / 62.0 + 1.0 / 61.0)).abs() < f32::EPSILON);
    }

    #[test]
    fn the_limit_applies_after_fusing_not_per_list() {
        // The row that only exists in the second list must still make the cut, which is
        // the whole reason fusion is not "concatenate and truncate".
        let fused = fuse(&[vec![1, 2, 3], vec![9, 2]], 3);
        assert_eq!(fused.len(), 3);
        assert_eq!(fused[0].0, 2, "the shared row wins");
        assert!(fused.iter().any(|(id, _)| *id == 9));
    }

    #[test]
    fn a_tie_keeps_the_order_the_lists_were_given_in() {
        // Same rank in separate lists, so the scores are equal and only the stable sort
        // decides. Reversing the arguments must reverse the answer, or the tiebreak is
        // not the one documented.
        let fused = fuse(&[vec![1], vec![2]], 10);
        assert_eq!(
            fused.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![1, 2]
        );
        let reversed = fuse(&[vec![2], vec![1]], 10);
        assert_eq!(
            reversed.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![2, 1]
        );
        assert_eq!(fused[0].1.to_bits(), reversed[0].1.to_bits());
    }

    #[test]
    fn no_lists_and_empty_lists_fuse_to_nothing() {
        assert_eq!(fuse(&[], 10).len(), 0);
        assert_eq!(fuse(&[vec![], vec![]], 10).len(), 0);
        assert_eq!(fuse(&[vec![1, 2]], 0).len(), 0);
    }

    #[test]
    fn a_repeated_id_inside_one_list_is_counted_twice() {
        // Not a case any leg produces, and pinned as behaviour anyway: fusion does not
        // dedupe within a list, so a call site that does not dedupe would be visible
        // here rather than as a mystery in the ranking.
        let fused = fuse(&[vec![5, 5]], 10);
        assert_eq!(fused.len(), 1);
        assert!((fused[0].1 - (rrf_score(0) + rrf_score(1))).abs() < f32::EPSILON);
    }
}
