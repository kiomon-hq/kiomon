// SPDX-License-Identifier: Apache-2.0

//! The vector leg: brute-force cosine over the stored embeddings.
//!
//! # Why brute force, and why two statements
//!
//! An ANN index is a second data structure to keep consistent with the first, and the
//! first is a SQLite file that a user copies, backs up and inspects. At the scale this tool
//! is built for — ten thousand memories is a *large* local brain — a scan of 10 000 × 384 ×
//! 4 bytes is 15 MB of blobs and a few million multiply-adds, which is measured in the plan
//! (§8 WP7) rather than assumed. The index goes in when the measurement says it has to, and
//! the measurement is a bench row, not an opinion.
//!
//! The scan is **two statements rather than a join**, and the reason is a measured one: a
//! join of `memory_embeddings` against `memories` for the status and workspace filters lets
//! SQLite drive from the memories index and probe the embedding table once per row, which
//! measured 78.6 ms for ten thousand vectors against 28.4 ms for a sequential scan of the
//! vectors plus one cheap index read of the eligible ids. The set intersection is a few
//! thousand `i64` comparisons; the join was fifteen megabytes read through ten thousand
//! random probes.
//!
//! # Why the scores are not comparable across models
//!
//! Vectors are only meaningful inside one model's space. Every row is keyed by model
//! name, every read filters on the configured one, and a corpus with vectors from two
//! models is two corpora. Nothing errors: the rows for the other model are simply not
//! this leg's candidates, which is the only safe reading.
//!
//! # Why the row is normalised on write
//!
//! The read path wants a dot product, and it wants it in a tight loop. Normalising at
//! write time moves the square root off the read path and makes the stored bytes the
//! thing that is compared. A zero vector stays zero — see `embed::normalize`.

use rusqlite::types::Value as Sql;

use crate::db::Database;
use crate::error::{Error, Result};
use crate::model::{MemoryKind, Scope};

/// The width of a stored vector, in bytes per coordinate.
const F32_BYTES: usize = 4;

/// Encode a vector as little-endian `f32`, the form `memory_embeddings.vec` holds.
///
/// Little-endian rather than native: a brain file is copied between machines, and a
/// big-endian reader must not silently reinterpret every coordinate.
#[must_use]
pub fn encode(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * F32_BYTES);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Decode a stored vector, or say why the blob is not one.
///
/// # Errors
///
/// Returns [`Error::Database`] when the blob's length is not a multiple of four, which
/// means the column holds something that was not written by [`encode`].
pub fn decode(bytes: &[u8]) -> Result<Vec<f32>> {
    if !bytes.len().is_multiple_of(F32_BYTES) {
        return Err(Error::Database(format!(
            "a stored embedding is {} bytes, which is not a whole number of f32 values",
            bytes.len()
        )));
    }
    let mut vector = Vec::with_capacity(bytes.len() / F32_BYTES);
    for chunk in bytes.as_chunks::<F32_BYTES>().0 {
        let mut word = [0u8; F32_BYTES];
        word.copy_from_slice(chunk);
        vector.push(f32::from_le_bytes(word));
    }
    Ok(vector)
}

/// The dot product of two normalised vectors: cosine similarity, and as fast as it gets.
///
/// Lengths are not checked here. `decode` guarantees a whole number of coordinates and
/// the caller compares vectors from one model, so a length mismatch is a corrupt file
/// rather than a query shape — and `dot` on unequal lengths would be a silent
/// truncation in the one place a silent truncation changes the answer. The caller
/// compares the dimension before calling this.
#[must_use]
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut sum = 0.0f32;
    // Zipped chunks of four, which is what the autovectoriser on every target this
    // ships to turns into the same loop as the scalar one but without the bounds checks.
    // `as_chunks` rather than `chunks_exact`: it yields `&[f32; 4]`, so the four indexes
    // below are compile-time constants and neither the loop nor the multiplies carry a
    // length check. The remainder is the same slice `chunks_exact` would have left.
    let (a_chunks, a_rest) = a.as_chunks::<4>();
    let (b_chunks, b_rest) = b.as_chunks::<4>();
    for (left, right) in a_chunks.iter().zip(b_chunks) {
        sum += left[0] * right[0] + left[1] * right[1] + left[2] * right[2] + left[3] * right[3];
    }
    for (left, right) in a_rest.iter().zip(b_rest) {
        sum += left * right;
    }
    sum
}

/// The dot product of a query against a *stored* vector, without materialising one.
///
/// The difference is not style: a scan of ten thousand rows that decodes each blob into a
/// `Vec<f32>` allocates ten thousand vectors and copies fifteen megabytes to add up
/// four million products, and the measurement (plan §8 WP7) showed the copying is what the
/// row costs. Reading the bytes in place is the same arithmetic with no allocation.
///
/// Little-endian, like [`encode`]. A blob whose length disagrees with the query's dimension
/// is a row from a different model labelled with this one; `None` says so, and the caller
/// skips it rather than scoring it on the part of itself it happens to share.
#[must_use]
pub fn dot_blob(bytes: &[u8], query: &[f32]) -> Option<f32> {
    if bytes.len() != query.len() * F32_BYTES {
        return None;
    }
    let mut sum = 0.0f32;
    for (word, value) in bytes.as_chunks::<F32_BYTES>().0.iter().zip(query) {
        let mut chunk = [0u8; F32_BYTES];
        chunk.copy_from_slice(word);
        sum += f32::from_le_bytes(chunk) * value;
    }
    Some(sum)
}

/// One candidate from the vector leg.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candidate {
    /// The memory's integer row id.
    pub mem_id: i64,
    /// Cosine similarity, higher is closer.
    pub score: f32,
}

impl Database {
    /// The memories that have a vector for `model`, in scope, ranked by similarity.
    ///
    /// `scope` and `kinds` are the same pair the lexical leg uses, so the two legs cannot
    /// disagree about which corpus they ranked. The status filter is not a parameter:
    /// search returns active memories, always, and a leg that could be asked for pending
    /// drafts is a leg that can leak an unreviewed memory. The second number returned is how
    /// many stored vectors this leg could compare — not how many it returned — because
    /// "the leg saw ten thousand vectors and none of them were in this workspace" is a
    /// different fact from "the leg saw nothing".
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the query fails or a stored blob is not a vector.
    pub(crate) fn vector_window(
        &self,
        model: &str,
        query: &[f32],
        scope: &Scope,
        kinds: &[MemoryKind],
        limit: u32,
    ) -> Result<(Vec<Candidate>, usize)> {
        // The eligible ids first: one indexed read, and the set the vector scan is
        // filtered against. Cheap enough to always run, and it keeps the status rule in one
        // place — an embeddings row whose memory is a draft is not a candidate.
        let eligible = self.eligible_rowids(scope, kinds)?;
        if eligible.is_empty() {
            return Ok((Vec::new(), 0));
        }

        // `get_ref` rather than `get::<Vec<u8>>`: the blob is borrowed from the statement
        // instead of copied into a fresh allocation per row, and at ten thousand rows that
        // allocation is most of the leg. The dot product reads it where it lies.
        let mut statement = self
            .conn()
            .prepare("SELECT mem_id, vec FROM memory_embeddings WHERE model = ?1")?;
        let mut rows = statement.query([model])?;

        let mut scanned = 0usize;
        let mut skipped = 0usize;
        let mut candidates: Vec<Candidate> = Vec::new();
        while let Some(row) = rows.next()? {
            let mem_id: i64 = row.get(0)?;
            let blob = match row.get_ref(1)? {
                rusqlite::types::ValueRef::Blob(bytes) => bytes,
                other => {
                    return Err(Error::Database(format!(
                        "a stored embedding is not a blob but a {}",
                        other.data_type()
                    )));
                }
            };
            if !eligible.contains(&mem_id) {
                continue;
            }
            scanned += 1;
            // Scored in place. A row whose dimension differs from the query's is a row
            // from a *different model* labelled with this one — the one corruption the
            // model key cannot catch. Skipping it is the only local answer; the damage is
            // visible as a memory semantic search cannot find, and `doctor`'s coverage count
            // (with `reindex --embed` as the fix) is where it surfaces.
            match dot_blob(blob, query) {
                // A candidate has to point the same way as the query *at all*. Zero is the
                // only floor cosine gives a meaning to that does not depend on the model:
                // every real embedding model is anisotropic, so a constant like 0.3 that
                // separates signal from noise for one model is a filter that throws away
                // half the corpus for another. Orthogonal is not a candidate; weakly
                // relevant is, and the ranking and the reported similarity are what a caller
                // judges that with.
                Some(value) if value > 0.0 => candidates.push(Candidate {
                    mem_id,
                    score: value,
                }),
                // Either orthogonal, or a blob from a model whose dimension differs: both
                // are rows this leg cannot rank, and neither is an error worth failing a
                // search over.
                _ => skipped += 1,
            }
        }

        // Sorted by score, then by descending row id: the tie-break is not cosmetic.
        // Two memories with identical vectors are common — a duplicate the gate admitted
        // with `--force`, two rows embedded from the same text — and an unstable order
        // would make the same query return them in a different order on a different day.
        candidates.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then(right.mem_id.cmp(&left.mem_id))
        });
        candidates.truncate(limit as usize);
        Ok((candidates, scanned.saturating_sub(skipped)))
    }

    /// The integer rows of every active memory in scope, and of the requested kinds.
    ///
    /// The vector leg's half of "which corpus is this": the same filters the lexical leg
    /// puts in its `WHERE`, read once into a set. `mem_id` only, because that is all the
    /// intersection needs and a covering index can supply it without touching a body.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the query fails.
    fn eligible_rowids(
        &self,
        scope: &Scope,
        kinds: &[MemoryKind],
    ) -> Result<std::collections::HashSet<i64>> {
        let mut sql = String::from("SELECT mem_id FROM memories WHERE status = 'active'");
        let (scope_clause, values) = crate::store::scope_clause(None, scope);
        sql.push_str(&scope_clause);
        sql.push_str(&crate::store::kind_filter(None, kinds));

        let mut statement = self.conn().prepare(&sql)?;
        let rows =
            statement.query_map(rusqlite::params_from_iter(values.iter()), |row| row.get(0))?;
        let mut out = std::collections::HashSet::new();
        for row in rows {
            out.insert(row?);
        }
        Ok(out)
    }

    /// How many memories have a vector for `model`, against how many are searchable.
    ///
    /// The two numbers that make "semantic search is on" a claim a user can check:
    /// embeddings are written by `add`, `approve` and `reindex`, and a corpus that was
    /// imported or written before the embedder was configured has none.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] if the query fails.
    pub fn embedding_coverage(&self, model: &str, scope: &Scope) -> Result<(i64, i64)> {
        let (scope_clause, values) = crate::store::scope_clause(Some("memories."), scope);
        let sql = format!(
            "SELECT \
               (SELECT COUNT(*) FROM memories \
                 WHERE memories.status = 'active'{scope_clause}), \
               (SELECT COUNT(*) FROM memory_embeddings \
                  JOIN memories ON memories.mem_id = memory_embeddings.mem_id \
                 WHERE memories.status = 'active' \
                   AND memory_embeddings.model = ?{scope_clause})"
        );
        // The scope binding appears twice, in the order of the placeholders: the outer
        // count's, then the inner count's model, then the inner count's scope.
        let mut bindings: Vec<Sql> = Vec::with_capacity(values.len() * 2 + 1);
        bindings.extend(values.iter().cloned());
        bindings.push(Sql::Text(model.to_owned()));
        bindings.extend(values);

        let (active, embedded): (i64, i64) =
            self.conn()
                .query_row(&sql, rusqlite::params_from_iter(bindings.iter()), |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?;
        Ok((active, embedded))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vector_survives_the_round_trip_through_the_blob() {
        let vector = vec![0.5f32, -1.25, 0.0, 3.0e-5];
        let bytes = encode(&vector);
        assert_eq!(bytes.len(), vector.len() * 4);
        assert_eq!(decode(&bytes).unwrap(), vector);
    }

    #[test]
    fn the_bytes_are_little_endian_so_a_copied_file_means_the_same_thing() {
        // 1.0f32 is 0x3F800000, which is `00 00 80 3f` little-endian. Asserted literally
        // because the whole point of the encoding is that it does not depend on the host.
        assert_eq!(encode(&[1.0]), vec![0x00, 0x00, 0x80, 0x3f]);
    }

    #[test]
    fn a_blob_that_is_not_a_whole_number_of_floats_is_refused_by_name() {
        let err = decode(&[0, 0, 0]).unwrap_err().to_string();
        assert!(err.contains("3 bytes"), "{err}");
    }

    #[test]
    fn the_dot_product_of_normalised_vectors_is_the_cosine() {
        let unit_x = [1.0f32, 0.0];
        let unit_y = [0.0f32, 1.0];
        assert!((dot(&unit_x, &unit_x) - 1.0).abs() < 1e-6);
        assert!(dot(&unit_x, &unit_y).abs() < 1e-6);

        let diagonal = [std::f32::consts::FRAC_1_SQRT_2; 2];
        assert!((dot(&unit_x, &diagonal) - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
    }

    #[test]
    fn a_stored_vector_is_scored_without_being_decoded_first() {
        let stored = encode(&[0.6, 0.8]);
        let query = [0.6f32, 0.8];
        let score = dot_blob(&stored, &query).expect("two coordinates");
        assert!((score - 1.0).abs() < 1e-6, "{score}");
        // And a blob from a different model is refused rather than half-read: scoring the
        // shared prefix of two vectors is an integer-overflow away from a *plausible* wrong
        // answer, which is the only kind that survives a code review.
        assert_eq!(dot_blob(&encode(&[0.6]), &query), None);
        assert_eq!(dot_blob(&[], &query), None);
    }

    #[test]
    fn the_dot_product_handles_lengths_that_are_not_a_multiple_of_four() {
        // 384 is a multiple of four, but nothing in the type system says a model's
        // dimension will be, and a loop that dropped the remainder would score every
        // vector on 96% of its coordinates and never say so.
        let left = [0.5f32; 7];
        let right = [0.5f32; 7];
        assert!((dot(&left, &right) - 1.75).abs() < 1e-6);
    }
}
