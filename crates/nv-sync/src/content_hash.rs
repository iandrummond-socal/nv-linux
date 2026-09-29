//! Dropbox content hashing.

use sha2::{Digest, Sha256};

/// Size of each block in Dropbox's content hash algorithm.
pub const BLOCK_SIZE: usize = 4 * 1024 * 1024;

/// Compute Dropbox's content hash as lowercase hexadecimal.
pub fn content_hash(bytes: &[u8]) -> String {
    let mut hasher = ContentHasher::new();
    hasher.update(bytes);
    hasher.finish()
}

/// Incrementally hash blocks without retaining their input bytes.
#[derive(Default)]
pub struct ContentHasher {
    block: Sha256,
    block_len: usize,
    overall: Sha256,
}

impl ContentHasher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            let take = data.len().min(BLOCK_SIZE - self.block_len);
            self.block.update(&data[..take]);
            self.block_len += take;
            data = &data[take..];

            if self.block_len == BLOCK_SIZE {
                let digest: [u8; 32] = self.block.finalize_reset().into();
                self.overall.update(digest);
                self.block_len = 0;
            }
        }
    }

    pub fn finish(mut self) -> String {
        if self.block_len != 0 {
            let digest: [u8; 32] = self.block.finalize().into();
            self.overall.update(digest);
        }
        let digest: [u8; 32] = self.overall.finalize().into();
        let mut hex = String::with_capacity(64);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in digest {
            hex.push(char::from(HEX[usize::from(byte >> 4)]));
            hex.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        hex
    }
}

#[cfg(test)]
mod tests {
    use super::{content_hash, ContentHasher, BLOCK_SIZE};
    use proptest::prelude::*;
    use sha2::{Digest, Sha256};

    fn pattern(size: usize) -> Vec<u8> {
        (0..size).map(|i| (i * 31 % 251) as u8).collect()
    }

    fn naive_content_hash(bytes: &[u8]) -> String {
        let digests: Vec<u8> = bytes
            .chunks(BLOCK_SIZE)
            .flat_map(|block| {
                let digest: [u8; 32] = Sha256::digest(block).into();
                digest
            })
            .collect();
        Sha256::digest(&digests)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    #[test]
    fn empty_input() {
        let expected = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(content_hash(b""), expected);
        assert_eq!(ContentHasher::default().finish(), expected);
    }

    #[test]
    fn matches_naive_at_block_boundaries() {
        for size in [
            0,
            1,
            BLOCK_SIZE - 1,
            BLOCK_SIZE,
            BLOCK_SIZE + 1,
            2 * BLOCK_SIZE,
            9 * 1024 * 1024,
        ] {
            let data = pattern(size);
            assert_eq!(
                content_hash(&data),
                naive_content_hash(&data),
                "size {size}"
            );
        }
    }

    #[test]
    fn exact_block_is_one_block() {
        let data = pattern(BLOCK_SIZE);
        let block_digest: [u8; 32] = Sha256::digest(&data).into();
        let expected: String = Sha256::digest(block_digest)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let mut hasher = ContentHasher::new();
        hasher.update(&data);
        hasher.update(&[]);
        assert_eq!(hasher.finish(), expected);
        assert_eq!(content_hash(&data), expected);
    }

    #[test]
    fn streams_multiple_blocks_in_odd_chunks() {
        let data = pattern(9 * 1024 * 1024);
        let mut hasher = ContentHasher::new();
        for chunk in data.chunks(1_000_003) {
            hasher.update(chunk);
        }
        assert_eq!(hasher.finish(), content_hash(&data));
    }

    proptest! {
        #[test]
        fn random_chunk_splits_match_one_shot(
            data in prop::collection::vec(any::<u8>(), 0..=10_240),
            chunk_sizes in prop::collection::vec(1usize..=2048, 1..=64),
        ) {
            let mut hasher = ContentHasher::new();
            hasher.update(&[]);
            let mut remaining = data.as_slice();
            for size in chunk_sizes.iter().cycle() {
                if remaining.is_empty() {
                    break;
                }
                let take = remaining.len().min(*size);
                hasher.update(&remaining[..take]);
                remaining = &remaining[take..];
                hasher.update(&[]);
            }
            prop_assert_eq!(hasher.finish(), content_hash(&data));
        }
    }
}
