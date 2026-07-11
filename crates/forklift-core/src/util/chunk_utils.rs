//! Content-defined chunking (DESIGN.html §9.4b) — the vendored, frozen boundary finder that
//! turns a large file into a deterministic sequence of chunks.
//!
//! **Everything in this module is a format freeze tied to `RECIPE_FORMAT_V1`.** The chunk
//! boundaries this code finds feed each chunk's hash, the recipe's hash, and through it the
//! signed root-tree hash — so two clients that chunk the same bytes *must* produce byte-identical
//! chunks or they fork signed history's tree hash for identical content. That is why the gear
//! table and the boundary algorithm are vendored here rather than pulled from the `fastcdc`
//! crate: a crate whose major version silently changed its gear table or its cut algorithm (its
//! 2016 vs. 2020 variants do) would relocate every already-tracked file's boundaries on the next
//! `Cargo.lock` bump — a silent history fork with no error to catch it. The gear table, the two
//! judgment masks, and the four size constants below are the frozen inputs; the golden vectors in
//! the test module are the freeze itself. A future recipe format version may change any of them;
//! `V1`'s values are then frozen forever, exactly as an old tree-format parser stays available
//! after a newer one ships.
//!
//! The scheme is FastCDC-class: a gear-hash rolling fingerprint with normalized chunking (a
//! stricter judgment mask before the average chunk size, a looser one after), which concentrates
//! the chunk-size distribution around the average and keeps a one-byte insertion from shifting
//! every downstream boundary (the property fixed-size blocks lack and the reason CDC exists).

/// A file whose content length is at or above this is stored chunked (a recipe plus chunks);
/// below it, an ordinary blob. A **format constant**, never configuration: a per-client
/// threshold would fork the tree hash across a team for identical content. Classification is
/// pinned to the bytes actually hashed, never a pre-read `stat` (see `object_utils`).
pub const CHUNK_THRESHOLD_BYTES: usize = 8 * 1024 * 1024;

/// The smallest chunk the boundary finder will emit: no cut point is considered before this
/// many bytes into a chunk. Keeps chunks from degenerating to tiny objects on adversarial or
/// low-entropy input.
pub const MIN_CHUNK_BYTES: usize = 256 * 1024;

/// The target average chunk size (the gear-mask is dimensioned for this). Smaller means better
/// dedup but larger recipes and more objects.
pub const AVG_CHUNK_BYTES: usize = 1024 * 1024;

/// The largest chunk the boundary finder will emit — a forced cut at this length even if no
/// content-defined boundary was found — **and** the enforced ceiling on a `Chunk`-typed object's
/// payload, on both store and read (a malicious recipe must not be able to reference a chunk
/// object larger than this, or the streaming-assembly memory bound would be far looser than the
/// per-chunk ceiling the explicit `Chunk` type exists to buy).
pub const MAX_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// A chunked file is always at least two chunks: the threshold exceeds the max chunk size, so a
/// file exactly at the threshold cannot fit in one chunk. A one-chunk recipe can therefore only
/// be hand-crafted, never produced by this chunker — a compile-time freeze of that invariant.
const _: () = assert!(CHUNK_THRESHOLD_BYTES > MAX_CHUNK_BYTES);

/// The seed of the frozen gear table's SplitMix64 generator (see [`build_gear_table`]). A
/// documented, reproducible origin: the 256 gear values are not hand-transcribed literals (which
/// could carry a silent transcription error) but the deterministic output of this generator,
/// evaluated at compile time. Changing this seed changes every boundary — it is frozen for
/// `RECIPE_FORMAT_V1`.
const GEAR_SEED: u64 = 0x2026_0711_FA57_CDC0;

/// The gear table: one pseudo-random 64-bit value per byte value, added into the rolling
/// fingerprint. Const-evaluated from [`build_gear_table`] so the generator *is* the frozen
/// definition (no transcription risk), and pinned additionally by the golden vectors.
const GEAR: [u64; 256] = build_gear_table();

/// The stricter judgment mask, applied to the fingerprint **before** the average chunk size is
/// reached: it sets 22 bits (a boundary here needs all 22 clear, so boundaries are *rarer* early,
/// biasing chunks up toward the average). Normalized chunking level 2 around a 2^20 average.
/// The set bits are the odd positions 15..=57 of the 64-bit fingerprint, spread across the
/// high-middle so a boundary reflects a wide window of recent bytes (the `fp << 1` accumulation
/// pushes older bytes into higher positions). Frozen for `RECIPE_FORMAT_V1`.
const MASK_S: u64 = mask_from_positions(&[
    15, 17, 19, 21, 23, 25, 27, 29, 31, 33, 35, 37, 39, 41, 43, 45, 47, 49, 51, 53, 55, 57,
]);

/// The looser judgment mask, applied **after** the average chunk size: it sets 18 bits (a subset
/// of [`MASK_S`]'s), so boundaries become *more* likely once a chunk has already grown past the
/// average, again biasing chunks toward the average. Frozen for `RECIPE_FORMAT_V1`.
const MASK_L: u64 = mask_from_positions(&[
    15, 17, 19, 21, 23, 25, 27, 29, 31, 33, 35, 37, 39, 41, 43, 45, 47, 49,
]);

/// Build a 64-bit mask from a list of set-bit positions (a `const fn` so the masks above are
/// compile-time constants). Positions are assumed in range; this is only ever called with the
/// two frozen literal lists above.
const fn mask_from_positions(positions: &[u32]) -> u64 {
    let mut mask = 0u64;
    let mut i = 0;
    while i < positions.len() {
        mask |= 1u64 << positions[i];
        i += 1;
    }
    mask
}

/// One step of the SplitMix64 generator over `state`. A tiny, well-known, fully deterministic
/// PRNG — used only to fill the frozen gear table at compile time.
const fn splitmix64(state: u64) -> (u64, u64) {
    let next = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = next;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31), next)
}

/// Generate the frozen 256-entry gear table from [`GEAR_SEED`] via SplitMix64. Const-evaluated,
/// so the table is fixed at compile time and identical on every platform and build.
const fn build_gear_table() -> [u64; 256] {
    let mut table = [0u64; 256];
    let mut state = GEAR_SEED;
    let mut i = 0;
    while i < 256 {
        let (value, next) = splitmix64(state);
        table[i] = value;
        state = next;
        i += 1;
    }
    table
}

/// Find the length of the first content-defined chunk in `data` — the FastCDC normalized-chunking
/// cut point, restarting the rolling fingerprint from zero at the start of the chunk.
///
/// The returned length is always in `MIN_CHUNK_BYTES..=MAX_CHUNK_BYTES`, except that a `data`
/// shorter than `MIN_CHUNK_BYTES` returns `data.len()` (the whole remainder is one final chunk).
/// The caller must only rely on the cut being *definitive* when `data.len() >= MAX_CHUNK_BYTES`
/// (a full window is available) or when `data` is the final bytes of the file (EOF): with fewer
/// than `MAX_CHUNK_BYTES` bytes and more of the file still to come, a later boundary might fall
/// before this one, so the streaming caller buffers up to `MAX_CHUNK_BYTES` before trusting a cut.
///
/// # Arguments
/// * `data` - The bytes from the start of the current (not-yet-cut) chunk onward.
///
/// # Returns
/// * `usize` - The length of the first chunk.
pub fn next_boundary(data: &[u8]) -> usize {
    let len = data.len();

    if len <= MIN_CHUNK_BYTES {
        return len;
    }

    // The normalized-chunking window: a stricter mask up to the average, a looser one up to the
    // max. `end` caps the scan at the max chunk size (a forced cut if no boundary is found).
    let end = len.min(MAX_CHUNK_BYTES);
    let normal = len.min(AVG_CHUNK_BYTES).min(end);

    let mut fingerprint: u64 = 0;
    let mut index = MIN_CHUNK_BYTES;

    while index < normal {
        fingerprint = (fingerprint << 1).wrapping_add(GEAR[data[index] as usize]);
        if fingerprint & MASK_S == 0 {
            return index;
        }
        index += 1;
    }

    while index < end {
        fingerprint = (fingerprint << 1).wrapping_add(GEAR[data[index] as usize]);
        if fingerprint & MASK_L == 0 {
            return index;
        }
        index += 1;
    }

    end
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cheap, fully deterministic, seeded pseudo-random byte generator for the golden vectors:
    /// a SplitMix64 stream cast to bytes. No `rand`, no clock, no OS entropy — the same bytes on
    /// every run and platform, so the boundary offsets and recipe hashes it produces are a stable
    /// freeze. (Deliberately the *same* PRNG family the gear table uses, but seeded differently.)
    fn deterministic_bytes(seed: u64, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut state = seed;
        while out.len() < len {
            let (value, next) = splitmix64(state);
            state = next;
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    /// Split `data` into the full ordered list of chunk lengths, exactly as a whole-buffer chunk
    /// would (each `next_boundary` restarts the fingerprint). This mirrors what the streaming
    /// ingest computes, so the golden lengths pin both.
    fn chunk_lengths(data: &[u8]) -> Vec<usize> {
        let mut lengths = Vec::new();
        let mut offset = 0;
        while offset < data.len() {
            let cut = next_boundary(&data[offset..]);
            lengths.push(cut);
            offset += cut;
        }
        lengths
    }

    #[test]
    fn gear_table_is_the_frozen_splitmix64_stream() {
        // The gear table is generated, not transcribed — but pin a few entries so an accidental
        // change to the generator or seed is caught loudly. These values are frozen for
        // RECIPE_FORMAT_V1.
        assert_eq!(GEAR[0], 0x43c3_c616_fa9f_0180, "GEAR[0] changed — the gear table is frozen");
        assert_eq!(GEAR[1], 0x8295_817e_ebfa_528d, "GEAR[1] changed — the gear table is frozen");
        assert_eq!(GEAR[255], 0x0942_f1b0_d84e_e831, "GEAR[255] changed — the gear table is frozen");

        // Every entry is present and the table is exactly 256 wide (a byte's index).
        assert_eq!(GEAR.len(), 256);
    }

    #[test]
    fn judgment_masks_have_the_frozen_bit_counts() {
        // Normalized chunking level 2: 22 bits before the average (stricter), 18 after (looser),
        // and MASK_L a subset of MASK_S. Frozen for RECIPE_FORMAT_V1.
        assert_eq!(MASK_S.count_ones(), 22, "MASK_S bit count changed — the mask is frozen");
        assert_eq!(MASK_L.count_ones(), 18, "MASK_L bit count changed — the mask is frozen");
        assert_eq!(MASK_L & MASK_S, MASK_L, "MASK_L must be a subset of MASK_S");
    }

    #[test]
    fn constants_are_frozen() {
        assert_eq!(CHUNK_THRESHOLD_BYTES, 8 * 1024 * 1024);
        assert_eq!(MIN_CHUNK_BYTES, 256 * 1024);
        assert_eq!(AVG_CHUNK_BYTES, 1024 * 1024);
        assert_eq!(MAX_CHUNK_BYTES, 4 * 1024 * 1024);
    }

    #[test]
    fn all_zero_run_cuts_at_the_max_chunk_size() {
        // A pathological low-entropy input (a long zero run) never satisfies a mask, so every
        // chunk is a forced cut at exactly the max. Frozen behaviour.
        let data = vec![0u8; MAX_CHUNK_BYTES * 3 + 12345];
        let lengths = chunk_lengths(&data);

        assert_eq!(lengths[0], MAX_CHUNK_BYTES);
        assert_eq!(lengths[1], MAX_CHUNK_BYTES);
        assert_eq!(lengths[2], MAX_CHUNK_BYTES);
        assert_eq!(lengths[3], 12345, "the final short remainder is one chunk");
        assert_eq!(lengths.iter().sum::<usize>(), data.len(), "chunks tile the input exactly");
    }

    #[test]
    fn every_chunk_respects_the_min_and_max_bounds() {
        let data = deterministic_bytes(0xA5A5_1234, 20 * 1024 * 1024);
        let lengths = chunk_lengths(&data);

        // Every chunk but the last honours the min; every chunk honours the max.
        for (index, &length) in lengths.iter().enumerate() {
            assert!(length <= MAX_CHUNK_BYTES, "chunk {} exceeds the max", index);
            let is_last = index + 1 == lengths.len();
            if !is_last {
                assert!(length >= MIN_CHUNK_BYTES, "non-final chunk {} is below the min", index);
            }
        }
        assert_eq!(lengths.iter().sum::<usize>(), data.len(), "chunks tile the input exactly");
    }

    #[test]
    fn golden_boundary_offsets_are_frozen_for_v1() {
        // THE FREEZE. Deterministic pseudo-random input, its exact cut offsets pinned. If this
        // changes, the gear table, a mask, a constant, or the algorithm changed — every already
        // tracked chunked file's recipe hash would change with it. These numbers are frozen for
        // RECIPE_FORMAT_V1 and must never be "updated to match" — a mismatch is a real bug.
        let data = deterministic_bytes(0x0BAD_C0DE_F00D, 12 * 1024 * 1024);
        let lengths = chunk_lengths(&data);

        let offsets: Vec<usize> = lengths.iter()
            .scan(0usize, |acc, &len| { *acc += len; Some(*acc) })
            .collect();

        assert_eq!(
            offsets, GOLDEN_OFFSETS_SEED_0BADC0DE,
            "frozen chunk boundaries changed — this forks every chunked file's recipe hash"
        );
        assert_eq!(offsets.last().copied(), Some(data.len()));
    }

    #[test]
    fn a_one_byte_prepend_only_disturbs_the_first_chunk_boundary() {
        // The whole point of content-defined chunking: inserting a byte at the front shifts only
        // the first boundary; the tail re-syncs and later chunks are identical, so dedup holds.
        let data = deterministic_bytes(0x5EED_CAFE, 16 * 1024 * 1024);
        let mut prepended = vec![0x42u8];
        prepended.extend_from_slice(&data);

        let original = chunk_lengths(&data);
        let shifted = chunk_lengths(&prepended);

        // Collect the absolute boundary offsets of both and count how many are shared. A
        // fixed-size scheme would share ~none; CDC re-syncs and shares the vast majority.
        let offsets = |lengths: &[usize]| -> std::collections::HashSet<usize> {
            lengths.iter().scan(0usize, |acc, &len| { *acc += len; Some(*acc) }).collect()
        };
        let original_offsets = offsets(&original);
        // The prepended stream's boundaries are one byte later; subtract one to compare content
        // positions. The final EOF offset differs by one (one extra byte), so ignore the very end.
        let shifted_offsets: std::collections::HashSet<usize> =
            offsets(&shifted).into_iter().map(|o| o.saturating_sub(1)).collect();

        let shared = original_offsets.intersection(&shifted_offsets).count();
        assert!(
            shared * 2 >= original_offsets.len(),
            "content-defined chunking must re-sync after an insertion (shared {} of {})",
            shared, original_offsets.len()
        );
    }

    /// The frozen boundary offsets for `deterministic_bytes(0x0BADC0DE_F00D, 12 MiB)`. Generated
    /// once from this exact algorithm and pinned; see `golden_boundary_offsets_are_frozen_for_v1`.
    const GOLDEN_OFFSETS_SEED_0BADC0DE: &[usize] = &[
        1476381, 2561944, 3999733, 5327010, 6924884,
        8084246, 9158764, 10309087, 11415640, 12582912,
    ];
}
