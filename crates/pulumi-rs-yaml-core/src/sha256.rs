// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! SHA-256, because a derived name is a compatibility contract.
//!
//! One function exists here, and it exists to give [`crate::eval::builtins`] a
//! digest whose value can never change. A builtin that derives a resource's
//! name from a seed writes that name into cloud state: change the algorithm and
//! every resource named through it is renamed, which a provider carries out as
//! a delete and a create. So the requirement is not "a good hash", it is "a
//! hash whose output is fixed forever and provable against something that is
//! not this code".
//!
//! That rules out the obvious alternatives. `std`'s `DefaultHasher` is
//! SipHash-1-3 today and the standard library explicitly declines to promise it
//! will stay that way, so a toolchain bump would rename a fleet. A seeded
//! `rand` generator is no better: the crate reserves the right to change
//! generator algorithms between minor versions. A hand-rolled FNV-1a would be
//! stable, being ours, but it has nothing to be checked against and poor
//! avalanche on exactly the input shape this sees — short seeds differing in
//! their last few bytes.
//!
//! A published standard has neither problem. FIPS 180-4 fixes the output for
//! all time, and the same digest can be produced by any other implementation,
//! so the tests below are a differential against the specification rather than
//! a recording of whatever this file happens to compute.
//!
//! # Scope
//!
//! A single one-shot function over a byte slice. No streaming `update`/`finish`
//! pair, no `Digest` trait, no HMAC — a seed arrives whole and is hashed whole,
//! and an API wider than its one caller needs is surface to keep correct for no
//! benefit. It allocates nothing: the block buffer is a fixed array and the
//! message schedule lives on the stack.

#![forbid(unsafe_code)]

/// Round constants: the first 32 bits of the fractional parts of the cube roots
/// of the first sixty-four primes (FIPS 180-4 §4.2.2).
#[rustfmt::skip]
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Initial hash value: the first 32 bits of the fractional parts of the square
/// roots of the first eight primes (FIPS 180-4 §5.3.3).
const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// The number of bytes in one SHA-256 block.
const BLOCK: usize = 64;

/// The SHA-256 digest of `bytes`.
///
/// Allocates nothing. Cost is linear in the input and a fixed two compressions
/// at most for the padded tail, so a short seed costs one or two compressions
/// in total.
pub fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut h = H0;

    let mut blocks = bytes.chunks_exact(BLOCK);
    for block in &mut blocks {
        // `chunks_exact` yields exactly BLOCK bytes, so the conversion holds.
        compress(&mut h, block.try_into().expect("chunks_exact yields BLOCK"));
    }
    let rest = blocks.remainder();

    // The padded tail is one block, or two when the length field will not fit
    // beside the remaining bytes and the 0x80 terminator. Both cases are
    // written into the same zeroed buffer, so there is one code path.
    let mut tail = [0u8; BLOCK * 2];
    tail[..rest.len()].copy_from_slice(rest);
    tail[rest.len()] = 0x80;
    let tail_len = if rest.len() + 1 + 8 <= BLOCK { BLOCK } else { BLOCK * 2 };
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    tail[tail_len - 8..tail_len].copy_from_slice(&bit_len.to_be_bytes());
    for block in tail[..tail_len].chunks_exact(BLOCK) {
        compress(&mut h, block.try_into().expect("chunks_exact yields BLOCK"));
    }

    let mut out = [0u8; 32];
    for (word, slot) in h.iter().zip(out.chunks_exact_mut(4)) {
        slot.copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// One compression round over a single block (FIPS 180-4 §6.2.2).
fn compress(h: &mut [u32; 8], block: &[u8; BLOCK]) {
    let mut w = [0u32; 64];
    for (slot, chunk) in w.iter_mut().zip(block.chunks_exact(4)) {
        *slot = u32::from_be_bytes(chunk.try_into().expect("chunks_exact yields 4"));
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let t1 = hh
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);

        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }

    for (slot, add) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
        *slot = slot.wrapping_add(add);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            use std::fmt::Write as _;
            write!(s, "{b:02x}").expect("writing to a String cannot fail");
        }
        s
    }

    /// The published FIPS 180-4 vectors. These are the specification's own
    /// answers, not this implementation's, which is the whole point: a
    /// transcription error here would be caught by anyone reading the standard.
    #[test]
    fn the_published_vectors_are_reproduced() {
        for (input, want) in [
            ("", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
            ("abc", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
            (
                "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
            (
                "abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmno\
                 ijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu",
                "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1",
            ),
        ] {
            assert_eq!(hex(&digest(input.as_bytes())), want, "input {input:?}");
        }
    }

    /// The padding boundary is where a one-shot SHA-256 is got wrong. At 55
    /// bytes the terminator and the 8-byte length still fit in one block; at 56
    /// they do not and a whole second block is needed; at 64 the input fills a
    /// block exactly and the padding is a block of its own. Each expected value
    /// came from an independent implementation.
    #[test]
    fn the_padding_boundary_is_crossed_correctly() {
        for (n, want) in [
            (55usize, "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318"),
            (56, "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"),
            (63, "7d3e74a05d7db15bce4ad9ec0658ea98e3f06eeecf16b4c6fff2da457ddc2f34"),
            (64, "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"),
            (65, "635361c48bb9eab14198e76ea8ab7f1a41685d6ad62aa9146d301d4f17eb0ae0"),
        ] {
            let input = vec![b'a'; n];
            assert_eq!(hex(&digest(&input)), want, "{n} bytes");
        }
    }

    /// A million bytes exercises the multi-block loop rather than only the tail,
    /// and is the standard's own long vector.
    #[test]
    fn the_long_vector_is_reproduced() {
        let input = vec![b'a'; 1_000_000];
        assert_eq!(
            hex(&digest(&input)),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    /// Every length from nothing to past two blocks produces 32 bytes and never
    /// panics. The boundary cases above pin values; this pins totality, which is
    /// what a caller hashing author-supplied text depends on.
    #[test]
    fn every_length_through_two_blocks_is_total() {
        let mut seen = std::collections::HashSet::new();
        for n in 0..200usize {
            let d = digest(&vec![b'z'; n]);
            assert_eq!(d.len(), 32);
            assert!(seen.insert(d), "two different lengths produced one digest at {n}");
        }
    }

    /// Non-UTF-8 input is just bytes here. The builtin above hashes a string,
    /// but nothing in this module may assume that.
    #[test]
    fn arbitrary_bytes_are_accepted() {
        let d = digest(&[0x00, 0xff, 0x80, 0xfe, 0x00]);
        assert_eq!(d.len(), 32);
        assert_ne!(d, digest(&[0x00, 0xff, 0x80, 0xfe]));
    }

    /// The same input gives the same answer in one process and across calls.
    /// Trivially true of a pure function, and the single property the fleet's
    /// resource names rest on, so it is stated rather than assumed.
    #[test]
    fn the_digest_is_a_pure_function_of_its_input() {
        let a = digest(b"voice-usage-egress");
        let b = digest(b"voice-usage-egress");
        assert_eq!(a, b);
        assert_ne!(a, digest(b"voice-usage-egres"));
        assert_ne!(a, digest(b"voice-usage-egresss"));
    }
}
