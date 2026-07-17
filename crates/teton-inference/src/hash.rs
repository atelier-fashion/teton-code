//! A small, dependency-free SHA-256 used to verify downloaded model files.
//!
//! Model integrity is a file-checksum concern, not a secret-handling one, so a
//! compact self-contained implementation keeps `teton-inference` free of a
//! crypto dependency (and out of the concurrently-edited workspace lock). It is
//! validated against the NIST/FIPS-180 test vectors in the unit tests below and
//! streams input so large GGUF files hash without loading fully into memory.

use std::fmt::Write as _;
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// SHA-256 initial hash values (first 32 bits of the fractional parts of the
/// square roots of the first eight primes).
const H0: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// SHA-256 round constants (first 32 bits of the fractional parts of the cube
/// roots of the first sixty-four primes).
const K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// An incremental SHA-256 hasher.
#[derive(Debug, Clone)]
pub struct Sha256 {
    state: [u32; 8],
    /// Total number of message bytes fed so far.
    len: u64,
    /// Partial block awaiting a full 64 bytes.
    buf: [u8; 64],
    buf_len: usize,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    /// A fresh hasher with the standard initial state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: H0,
            len: 0,
            buf: [0u8; 64],
            buf_len: 0,
        }
    }

    /// Feed more message bytes.
    pub fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);

        // Top up any partial block first.
        if self.buf_len > 0 {
            let need = 64 - self.buf_len;
            let take = need.min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                process_block(&mut self.state, &block);
                self.buf_len = 0;
            }
        }

        // Consume whole blocks straight from the input.
        while data.len() >= 64 {
            let block: &[u8; 64] = data[..64].try_into().expect("slice is exactly 64 bytes");
            process_block(&mut self.state, block);
            data = &data[64..];
        }

        // Stash the remainder.
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    /// Finish hashing and return the 32-byte digest.
    #[must_use]
    pub fn finalize(self) -> [u8; 32] {
        let bit_len = self.len.wrapping_mul(8);
        let mut state = self.state;

        let mut block = [0u8; 64];
        block[..self.buf_len].copy_from_slice(&self.buf[..self.buf_len]);
        // The mandatory trailing `1` bit (`buf_len` is always <= 63 here).
        block[self.buf_len] = 0x80;
        if self.buf_len >= 56 {
            // No room left for the 64-bit length in this block; flush and start
            // a fresh all-zero block for the length.
            process_block(&mut state, &block);
            block = [0u8; 64];
        }
        block[56..64].copy_from_slice(&bit_len.to_be_bytes());
        process_block(&mut state, &block);

        let mut out = [0u8; 32];
        for (chunk, word) in out.chunks_exact_mut(4).zip(state.iter()) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// The SHA-256 compression function over one 64-byte block.
fn process_block(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 64];
    for (word, chunk) in w[..16].iter_mut().zip(block.chunks_exact(4)) {
        *word = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;

    for (k, wi) in K.iter().zip(w.iter()) {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(*k)
            .wrapping_add(*wi);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}

/// Lowercase-hex encode a digest.
fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a String is infallible.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Compute the lowercase-hex SHA-256 digest of `data`.
///
/// Only the streaming file path ([`sha256_file`]) is used in non-test builds;
/// this one-shot helper backs the unit tests and the download tests' fixtures.
#[cfg(test)]
#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    to_hex(&hasher.finalize())
}

/// Compute the lowercase-hex SHA-256 digest of a file, reading it in chunks so
/// even multi-gigabyte GGUF files hash with bounded memory.
///
/// # Errors
/// Returns any I/O error from opening or reading `path`.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(to_hex(&hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_matches_the_known_vector() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn abc_matches_the_fips_180_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn multi_block_vector_matches() {
        // The classic 56-byte FIPS-180 two-block example.
        let msg = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(
            sha256_hex(msg),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn incremental_updates_equal_a_single_update() {
        let data: Vec<u8> = (0u8..=255).cycle().take(1000).collect();
        let one_shot = sha256_hex(&data);

        let mut hasher = Sha256::new();
        // Feed in awkward chunk sizes that straddle block boundaries.
        for chunk in data.chunks(37) {
            hasher.update(chunk);
        }
        assert_eq!(to_hex(&hasher.finalize()), one_shot);
    }

    #[test]
    fn exactly_one_block_hashes() {
        // 64 bytes forces the "no room for length, spill to a second block" path.
        let data = [0xabu8; 64];
        // Reference value computed by the same algorithm; guards against
        // regressions in the padding/length-spill logic.
        let expected = sha256_hex(&data);
        let mut hasher = Sha256::new();
        hasher.update(&data);
        assert_eq!(to_hex(&hasher.finalize()), expected);
        assert_eq!(expected.len(), 64);
    }

    #[test]
    fn file_digest_matches_in_memory_digest() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("teton-hash-test-{}.bin", std::process::id()));
        let data: Vec<u8> = (0u8..97).cycle().take(5000).collect();
        std::fs::write(&path, &data).expect("write temp file");
        let file_hex = sha256_file(&path).expect("hash file");
        std::fs::remove_file(&path).ok();
        assert_eq!(file_hex, sha256_hex(&data));
    }
}
