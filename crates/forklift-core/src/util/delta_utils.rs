//! Delta compression for bundles (§4.5 item 5, §9.1 #1): store an object as its difference
//! from a similar *base* object, so a bundle moves the **change** between versions of a file
//! rather than the whole file — git's biggest transfer win, on Forklift's substrate.
//!
//! The delta is **zstd with the base object as a dictionary**, never a bespoke diff format.
//! Unchanged regions of the target cost almost nothing because zstd references the base;
//! the entropy coding stays zstd's problem, not ours. Reconstruction needs the same base
//! present, and — like every imported object — the result is hash-verified before it is
//! stored (`object_utils::store_object_bytes`). So a bad or truncated delta can only ever
//! *fail* import (and the client falls back to a loose fetch), never corrupt the store.
//! That content-addressed safety net is why the simple, dependency-reusing choice is the
//! right one: correctness does not rest on the delta format being hand-audited.

/// The zstd level bundle deltas are compressed at. A bundle is built once and read many
/// times, so a moderate level — good ratio, still fast — is the right trade. (`0` would be
/// zstd's default of 3; 3 is spelled out for intent.)
const DELTA_LEVEL: i32 = 3;

/// Compress `target` as a delta against `base`: one self-contained zstd frame with `base`
/// installed as the dictionary. The frame is independent of any surrounding stream, so a
/// bundle can carry many deltas each against a different base.
///
/// # Arguments
/// * `base`   - The base object's raw bytes (the dictionary).
/// * `target` - The object to encode as a delta.
///
/// # Returns
/// * `Ok(Vec<u8>)` - The delta payload (a zstd frame).
/// * `Err(String)` - If the compressor could not be prepared or the frame written.
pub fn compress_delta(base: &[u8], target: &[u8]) -> Result<Vec<u8>, String> {
    let mut compressor = zstd::bulk::Compressor::with_dictionary(DELTA_LEVEL, base)
        .map_err(|e| format!("Error while preparing the delta compressor: {}", e))?;

    compressor.compress(target)
        .map_err(|e| format!("Error while compressing a delta: {}", e))
}

/// The largest target a delta may reconstruct — and, symmetrically, the largest object a
/// delta is ever *created* for. Deltating huge blobs costs more RAM/CPU than it saves and it
/// bounds window memory, so both writers (`pack_utils`, `bundle_utils`) store anything larger
/// in full.
///
/// Because no writer emits a delta above this size, the reader may enforce it — which is what
/// turns [`decompress_delta`]'s declared length into a real decompression-bomb bound rather
/// than a number the attacker chooses. Without it, a hostile record could declare `u64::MAX`
/// and a small zstd frame could expand without limit.
pub const MAX_DELTA_TARGET_BYTES: usize = 16 * 1024 * 1024;

/// Reconstruct a target from its delta against `base`.
///
/// `capacity` is the target's exact decompressed length (carried in the delta record). The
/// result must match it exactly, and it must not exceed [`MAX_DELTA_TARGET_BYTES`] — together
/// those make this a real decompression-bomb guard.
///
/// # Arguments
/// * `base`     - The base object's raw bytes (the dictionary the delta was made against).
/// * `payload`  - The delta payload (a zstd frame from [`compress_delta`]).
/// * `capacity` - The expected decompressed length.
///
/// # Returns
/// * `Ok(Vec<u8>)` - The reconstructed target bytes (still to be hash-verified by the caller).
/// * `Err(String)` - If the declared length is above the ceiling, the base is wrong, the
///   payload is corrupt, or the frame does not reconstruct to exactly `capacity` bytes.
pub fn decompress_delta(base: &[u8], payload: &[u8], capacity: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;

    // `capacity` is the *declared* length carried in a delta record — attacker-controlled in an
    // imported bundle and untrusted in an on-disk pack. Two things follow.
    //
    // It must never be pre-allocated: a lie (e.g. `u64::MAX`) would be a one-record denial of
    // service (a capacity-overflow panic, or an allocator abort for a large-but-representable
    // value). So the buffer below grows with the bytes actually produced.
    //
    // And it must never be trusted as the *bound*, or it is no bound at all: a declared
    // `u64::MAX` would let a small zstd frame expand until memory ran out. The ceiling is what
    // makes the guard real — and it is sound because no writer emits a delta above it.
    if capacity > MAX_DELTA_TARGET_BYTES {
        return Err(format!(
            "A delta record declares a {} byte target, above the {} byte ceiling deltas are \
            created under; refusing to reconstruct it.",
            capacity, MAX_DELTA_TARGET_BYTES
        ));
    }

    let mut decoder = zstd::stream::read::Decoder::with_dictionary(payload, base)
        .map_err(|e| format!("Error while preparing the delta decompressor: {}", e))?;

    // Read one byte past the declared length: producing it is what proves the frame lied.
    let mut output = Vec::new();
    decoder.by_ref().take(capacity as u64 + 1).read_to_end(&mut output)
        .map_err(|e| format!("Error while decompressing a delta: {}", e))?;

    // Exactly, not at most: the length is the record's own claim about the object, so a frame
    // that under-runs it is as corrupt as one that overruns it. Correctness never rests on this
    // — every caller hash-verifies the result — but the resource bound does.
    if output.len() != capacity {
        return Err(format!(
            "A delta reconstructed {} bytes but its record declared {}.",
            output.len(),
            capacity
        ));
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delta_round_trips_against_its_base() {
        let base = b"the quick brown fox jumps over the lazy dog\n".repeat(50);
        let mut target = base.clone();
        target.extend_from_slice(b"one extra line at the end\n");

        let delta = compress_delta(&base, &target).unwrap();

        // The delta of a near-identical target is far smaller than the target itself —
        // the whole point (the unchanged bulk is referenced from the base, not re-stored).
        assert!(delta.len() < target.len() / 2, "delta {} vs target {}", delta.len(), target.len());

        assert_eq!(decompress_delta(&base, &delta, target.len()).unwrap(), target);
    }

    #[test]
    fn a_dictionary_dependent_delta_needs_the_right_base() {
        // When the delta genuinely leans on the base (the base carries the bulk of the
        // target), decoding against the wrong base cannot reproduce the target — it errors
        // or returns different bytes. Either way the importer's hash check rejects it.
        // (The guarantee Forklift relies on is that check, not the delta format itself; for
        // tiny inputs zstd embeds the literals and the base is not load-bearing at all.)
        let base = b"the quick brown fox jumps over the lazy dog\n".repeat(50);
        let mut target = base.clone();
        target.extend_from_slice(b"one extra line at the end\n");

        let delta = compress_delta(&base, &target).unwrap();
        assert!(delta.len() < target.len() / 2, "the base must be load-bearing for this test");

        let wrong_base = b"a completely different base of a similar-ish size\n".repeat(50);

        match decompress_delta(&wrong_base, &delta, target.len()) {
            Ok(bytes) => assert_ne!(bytes, target),
            Err(_) => {}
        }
    }

    #[test]
    fn a_frame_that_exceeds_the_declared_capacity_is_rejected() {
        let base = b"base".to_vec();
        let target = b"a much longer target than the declared capacity".to_vec();

        let delta = compress_delta(&base, &target).unwrap();

        // Lying about the size (too small a capacity) fails rather than truncating silently.
        assert!(decompress_delta(&base, &delta, 4).is_err());
    }
}
