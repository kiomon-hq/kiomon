// SPDX-License-Identifier: Apache-2.0

//! Text normalisation and the content hash that makes `add` idempotent.
//!
//! # Why a non-cryptographic hash is the right one here
//!
//! `content_hash` is a **lookup key, not a decision**. [`find_duplicate`] matches on
//! it and then compares the normalised title and body again before concluding that
//! two memories are the same. A collision therefore costs a missed dedupe — one extra
//! row — and can never cost a memory. That is what makes 128 bits of FNV-1a enough,
//! and what keeps `sha2` and its dependency tree out of a tool whose budget is
//! measured in kilobytes.
//!
//! The hash is still wide on purpose: at 128 bits, a corpus would need to be roughly
//! 10^19 memories strong before a collision was likely, and the comparison is what
//! actually decides.
//!
//! [`find_duplicate`]: crate::store::Database::find_duplicate

use crate::model::MemoryKind;

/// FNV-1a, 128-bit: offset basis and prime, from the reference specification.
const FNV128_OFFSET_BASIS: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
const FNV128_PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;

/// Fold a string to the form two memories are compared in.
///
/// Lowercase, trimmed, with runs of whitespace collapsed to one space. `to_lowercase`
/// rather than `to_ascii_lowercase`: an agent writing in German or Turkish should get
/// the same dedupe as one writing in English.
///
/// **Unicode normalisation is deliberately absent.** `Café` composed and `Café`
/// decomposed hash differently and are stored as two memories. Detecting that needs
/// the `unicode-normalization` crate and its tables, and the failure mode is an extra
/// row rather than a wrong answer — so it waits for evidence that it happens, which
/// `plans/001-local-v1-core.md` §10 records as the trigger.
#[must_use]
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.trim().chars() {
        if ch.is_whitespace() {
            // Deferred rather than emitted: this is what collapses a run, and it is
            // why a trailing run cannot leave a space at the end.
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        for lower in ch.to_lowercase() {
            out.push(lower);
        }
    }
    out
}

/// The hash two otherwise-identical memories share.
///
/// `kind` is part of the input because the same words filed as a Fact and as a Moment
/// are two different memories: they decay differently, they are retrieved for
/// different reasons, and a `search --kind semantic` that returned a Moment because it
/// matched a Fact's hash would be wrong.
#[must_use]
pub fn content_hash(kind: MemoryKind, title: &str, body: &str) -> String {
    let mut hasher = Fnv1a128::new();
    // The separator cannot appear in a normalised string (whitespace is collapsed to
    // a single space), so `("a", "b c")` and `("a b", "c")` cannot collide by
    // construction rather than by luck.
    hasher.write(kind.as_str().as_bytes());
    hasher.write(b"\x1f");
    hasher.write(normalize(title).as_bytes());
    hasher.write(b"\x1f");
    hasher.write(normalize(body).as_bytes());
    format!("{:032x}", hasher.finish())
}

/// FNV-1a over 128 bits.
///
/// Streaming rather than one-shot so a body is hashed without building an
/// intermediate concatenation of it, which on a 100 KB memory would be a second copy
/// of the memory for no reason.
struct Fnv1a128 {
    state: u128,
}

impl Fnv1a128 {
    fn new() -> Self {
        Self {
            state: FNV128_OFFSET_BASIS,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.state ^= u128::from(*byte);
            self.state = self.state.wrapping_mul(FNV128_PRIME);
        }
    }

    fn finish(&self) -> u128 {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_empty_string_hashes_to_the_fnv_offset_basis() {
        // The reference specification's own vector: FNV-1a of no bytes is the offset
        // basis. If this fails, the constants were transcribed wrong.
        assert_eq!(Fnv1a128::new().finish(), FNV128_OFFSET_BASIS);
    }

    #[test]
    fn normalisation_is_idempotent() {
        for input in [
            "Hello World",
            "  spaced   out  ",
            "\ttabs\nand\r\nnewlines",
            "MiXeD cAsE",
            "ÄÖÜ äöü",
            "",
        ] {
            let once = normalize(input);
            assert_eq!(normalize(&once), once, "{input:?}");
        }
    }

    #[test]
    fn normalisation_collapses_case_and_whitespace() {
        assert_eq!(normalize("The Deploy  Step"), "the deploy step");
        assert_eq!(normalize("  the\tdeploy\nstep  "), "the deploy step");
        assert_eq!(normalize("THE DEPLOY STEP"), "the deploy step");
    }

    #[test]
    fn normalisation_lowercases_beyond_ascii() {
        assert_eq!(normalize("ÄÖÜ"), "äöü");
        assert_eq!(normalize("İstanbul"), normalize("i̇stanbul"));
    }

    #[test]
    fn the_same_text_hashes_the_same() {
        let a = content_hash(MemoryKind::Semantic, "Title", "Body");
        let b = content_hash(MemoryKind::Semantic, "  tItLe ", "body\n");
        assert_eq!(a, b);
    }

    #[test]
    fn the_kind_is_part_of_the_hash() {
        assert_ne!(
            content_hash(MemoryKind::Semantic, "T", "B"),
            content_hash(MemoryKind::Episodic, "T", "B")
        );
    }

    #[test]
    fn a_boundary_between_title_and_body_cannot_be_forged() {
        // Without a separator, ("ab", "c") and ("a", "bc") would concatenate to the
        // same bytes and hash the same, silently deduping two different memories.
        assert_ne!(
            content_hash(MemoryKind::Semantic, "ab", "c"),
            content_hash(MemoryKind::Semantic, "a", "bc")
        );
        assert_ne!(
            content_hash(MemoryKind::Semantic, "Deploy", "Step"),
            content_hash(MemoryKind::Semantic, "Deploy Step", "")
        );
    }

    #[test]
    fn different_text_hashes_differently() {
        let mut seen = std::collections::HashSet::new();
        for index in 0..5_000 {
            let hash = content_hash(
                MemoryKind::Semantic,
                &format!("title {index}"),
                "shared body",
            );
            assert!(seen.insert(hash), "collision at {index}");
        }
    }

    #[test]
    fn the_hash_is_a_fixed_width_hex_string() {
        let hash = content_hash(MemoryKind::Reference, "", "");
        assert_eq!(hash.len(), 32, "{hash}");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()), "{hash}");
    }
}
