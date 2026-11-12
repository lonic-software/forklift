//! Seeded byte mutation for the in-crate fuzz tests of the transport readers (packs, bundles),
//! whose fixtures are crate-private. `tests/fuzz_formats.rs` keeps its own copy for the public
//! parse entry points. The generator is splitmix64 with a fixed seed, so a failing case is
//! reproducible from its seed alone.

/// A deterministic splitmix64 stream.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..bound`; `bound` must not be zero.
    pub(crate) fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }

    pub(crate) fn byte(&mut self) -> u8 {
        self.next_u64() as u8
    }
}

/// Apply one to three mutations to `bytes`: truncation, a bit flip, a random or zeroed run, a
/// boundary value written over a length-sized field, an insertion, a deletion, or a duplicated
/// run. Each targets a class of reader mistake: short reads, trusted lengths, unread tails.
pub(crate) fn mutate(bytes: &mut Vec<u8>, rng: &mut Rng) {
    for _ in 0..1 + rng.below(3) {
        if bytes.is_empty() {
            bytes.push(rng.byte());
            continue;
        }
        let at = rng.below(bytes.len());
        let run_end = (at + 1 + rng.below(16)).min(bytes.len());
        match rng.below(8) {
            0 => bytes.truncate(at),
            1 => bytes[at] ^= 1 << rng.below(8),
            2 => bytes[at..run_end].iter_mut().for_each(|byte| *byte = rng.byte()),
            3 => bytes[at..run_end].iter_mut().for_each(|byte| *byte = 0),
            4 => {
                // A boundary value over a 4- or 8-byte field, in either byte order.
                let width = if rng.below(2) == 0 { 4 } else { 8 };
                let value: u64 = [0, 1, u32::MAX as u64, u64::MAX, 1 << 31, 1 << 63][rng.below(6)];
                let mut field = value.to_be_bytes()[8 - width..].to_vec();
                if rng.below(2) == 0 {
                    field.reverse();
                }
                let end = (at + width).min(bytes.len());
                bytes[at..end].copy_from_slice(&field[..end - at]);
            }
            5 => {
                let inserted: Vec<u8> = (0..1 + rng.below(8)).map(|_| rng.byte()).collect();
                bytes.splice(at..at, inserted);
            }
            6 => {
                bytes.drain(at..run_end);
            }
            _ => {
                let run = bytes[at..run_end].to_vec();
                bytes.splice(run_end..run_end, run);
            }
        }
    }
}
