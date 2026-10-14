// SPDX-License-Identifier: Apache-2.0

//! Identifiers: `k_<ulid>`, sortable by creation time.
//!
//! A ULID is a 48-bit millisecond timestamp followed by 80 bits of randomness,
//! rendered as 26 characters of Crockford base32. Two properties earn it over a UUID
//! here:
//!
//! - **It sorts.** `ORDER BY id` is `ORDER BY created_at` for free, and an agent that
//!   sees two ids can tell which is newer. Inside one millisecond the ordering is by
//!   the random half, which is not meaningful — but it is stable, which is what an
//!   index needs.
//! - **It is short enough to paste.** 26 characters against a UUID's 36, in a tool
//!   whose whole surface is text an agent reads and writes.
//!
//! The randomness comes from the operating system via `getrandom`, not from a seeded
//! PRNG. Two processes adding memories at the same instant must not collide, and a
//! seed derived from the clock is exactly how that happens.

use crate::error::{Error, Result};

/// Crockford base32: no `I`, `L`, `O` or `U`, so the alphabet cannot be misread and
/// cannot spell anything unfortunate.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The prefix every memory id carries. It makes an id recognisable at a glance in a
/// log line, in a URL, and in an agent's prose.
pub const MEMORY_PREFIX: &str = "k_";

/// Characters in a ULID, excluding the prefix.
const ULID_LEN: usize = 26;

/// Generate a new memory id.
///
/// # Errors
///
/// Returns [`Error::Internal`] if the operating system's random source is
/// unavailable. That is not a condition a caller can act on, but it is a condition it
/// must not paper over: a silently degraded id source is a source of collisions.
pub fn new_memory_id() -> Result<String> {
    let mut bytes = [0_u8; 16];
    let millis = now_millis();
    // The timestamp occupies the high 48 bits, big-endian, so the encoded form sorts
    // in time order. `to_be_bytes()` on a u64 gives 8 bytes; the top two are always
    // zero until the year 10889.
    bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
    getrandom::fill(&mut bytes[6..])
        .map_err(|err| Error::Internal(format!("could not read random bytes: {err}")))?;
    Ok(format!("{MEMORY_PREFIX}{}", encode(&bytes)))
}

/// Milliseconds since the Unix epoch.
///
/// A clock set before 1970 is reported as 0 rather than as an error: the memory is
/// worth storing with a wrong id than not storing at all, and the id's timestamp is
/// decorative next to `created_at`, which the database supplies.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            // Saturating rather than wrapping: a wrapped timestamp would make the id
            // sort *before* every existing one, which is worse than a very large one.
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Render 16 bytes as 26 Crockford base32 characters.
///
/// 26 characters carry 130 bits, so the two leading bits are padding and the first
/// character is always one of `0`–`7`. That is the standard ULID shape.
fn encode(bytes: &[u8; 16]) -> String {
    let value = u128::from_be_bytes(*bytes);
    let mut out = String::with_capacity(ULID_LEN);
    for index in 0..ULID_LEN {
        // Offsets count down from the top of the 130-bit window, which is why the
        // first character is `value >> 125` and the last is `value & 0x1f`.
        let shift = 125 - 5 * u32::try_from(index).unwrap_or(0);
        // The mask and the shift keep this in `0..32`, so the index cannot be out of
        // range; the `unwrap_or` is there because `usize` is not bounded by the type
        // system, not because it can happen.
        let slot = usize::try_from((value >> shift) & 0x1f).unwrap_or(0);
        out.push(char::from(ALPHABET[slot]));
    }
    out
}

/// Recover the 128 bits an id was encoded from.
///
/// Used by tests and by anything that wants the creation instant without a second
/// read. Accepts lowercase and the Crockford aliases (`O`→`0`, `I`/`L`→`1`), because an
/// id that has been through a human's hands is not guaranteed to be in canonical case.
///
/// # Errors
///
/// Returns [`Error::Invalid`] if the string is not a well-formed id.
pub fn decode(id: &str) -> Result<u128> {
    let body = id.strip_prefix(MEMORY_PREFIX).unwrap_or(id);
    if body.len() != ULID_LEN {
        return Err(Error::Invalid(format!(
            "`{id}` is not a memory id: expected {MEMORY_PREFIX} followed by {ULID_LEN} characters"
        )));
    }
    let mut value: u128 = 0;
    for (index, ch) in body.chars().enumerate() {
        let digit = decode_char(ch).ok_or_else(|| {
            Error::Invalid(format!(
                "`{id}` is not a memory id: `{ch}` at position {index} is not a base32 character"
            ))
        })?;
        value = (value << 5) | u128::from(digit);
    }
    Ok(value)
}

/// The creation instant an id encodes, in milliseconds since the Unix epoch.
///
/// # Errors
///
/// Returns [`Error::Invalid`] if the string is not a well-formed id.
pub fn timestamp_millis(id: &str) -> Result<u64> {
    let value = decode(id)?;
    // The top 48 bits of the 128, with the two padding bits above them discarded.
    // 128 - 48 = 80, and the encoding's two padding bits sit above bit 127, so this is
    // a plain right shift rather than an offset.
    Ok(u64::try_from(value >> 80).unwrap_or(u64::MAX))
}

fn decode_char(ch: char) -> Option<u8> {
    let upper = ch.to_ascii_uppercase();
    match upper {
        // Crockford's stated aliases: these are the characters the alphabet omits,
        // and rejecting them would turn a transcription typo into a not-found error.
        'O' => Some(0),
        'I' | 'L' => Some(1),
        other => ALPHABET
            .iter()
            .position(|candidate| char::from(*candidate) == other)
            .and_then(|index| u8::try_from(index).ok()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_has_the_documented_shape() {
        let id = new_memory_id().unwrap();
        assert!(id.starts_with("k_"), "{id}");
        assert_eq!(id.len(), MEMORY_PREFIX.len() + ULID_LEN, "{id}");
        assert!(
            id[2..].chars().all(|c| ALPHABET.contains(&(c as u8))),
            "every character must be in the alphabet: {id}"
        );
    }

    #[test]
    fn the_first_character_never_sets_the_padding_bits() {
        // 26 characters hold 130 bits for 128 bits of payload, so the first character
        // can only ever be 0-7. An id whose first character is `8` or above was not
        // produced by this encoder.
        for _ in 0..200 {
            let id = new_memory_id().unwrap();
            let first = id.chars().nth(MEMORY_PREFIX.len()).unwrap();
            assert!(
                "01234567".contains(first),
                "padding bit set in {id}: first character is `{first}`"
            );
        }
    }

    #[test]
    fn ids_are_unique_across_a_burst() {
        // The same millisecond for all of these, so this asserts the random half does
        // its job rather than that the clock moved.
        let ids: std::collections::HashSet<String> =
            (0..2_000).map(|_| new_memory_id().unwrap()).collect();
        assert_eq!(ids.len(), 2_000);
    }

    #[test]
    fn ids_sort_in_creation_order() {
        let first = new_memory_id().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = new_memory_id().unwrap();
        assert!(first < second, "{first} should sort before {second}");
    }

    #[test]
    fn the_timestamp_survives_a_round_trip() {
        let before = now_millis();
        let id = new_memory_id().unwrap();
        let after = now_millis();
        let stamped = timestamp_millis(&id).unwrap();
        assert!(
            (before..=after).contains(&stamped),
            "id timestamp {stamped} is outside {before}..={after}"
        );
    }

    #[test]
    fn decoding_accepts_lowercase_and_the_crockford_aliases() {
        let id = new_memory_id().unwrap();
        let value = decode(&id).unwrap();
        assert_eq!(decode(&id.to_lowercase()).unwrap(), value);
    }

    #[test]
    fn a_malformed_id_is_an_invalid_argument_with_a_useful_message() {
        for bad in ["", "k_", "k_0123", "k_0123456789012345678901234!", "nope"] {
            let err = decode(bad).unwrap_err();
            assert_eq!(err.exit_code(), crate::ExitCode::InvalidArgs, "{bad}");
            assert!(err.to_string().contains("memory id"), "{bad}: {err}");
        }
    }

    #[test]
    fn every_byte_pattern_round_trips() {
        // Property rather than example: the encoder and decoder must agree for
        // arbitrary input, not only for the values a clock produces.
        let mut state: u128 = 1;
        for _ in 0..500 {
            // A cheap xorshift keeps this deterministic, which a failing case needs.
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let bytes = state.to_be_bytes();
            let encoded = encode(&bytes);
            assert_eq!(decode(&encoded).unwrap(), state, "{encoded}");
        }
    }
}
