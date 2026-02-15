//! Content-defined chunking (CDC) for splitting files into deduplicatable blocks.
//!
//! Uses a Rabin-style rolling hash with a gear table to find chunk boundaries.
//! This ensures that insertions or deletions only affect nearby chunks, maximizing
//! deduplication across versions of a file.

/// Configuration for the chunking algorithm.
#[derive(Debug, Clone)]
pub struct ChunkConfig {
    /// Minimum chunk size in bytes. No boundary below this offset.
    pub min_size: usize,
    /// Average target chunk size. The mask is derived from this.
    pub avg_size: usize,
    /// Maximum chunk size. A forced boundary if no natural one is found.
    pub max_size: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            min_size: 2 * 1024,     // 2 KiB
            avg_size: 8 * 1024,     // 8 KiB
            max_size: 64 * 1024,    // 64 KiB
        }
    }
}

/// A chunk produced by the CDC algorithm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Byte offset in the original data.
    pub offset: usize,
    /// Length of this chunk.
    pub length: usize,
}

/// Split data into content-defined chunks.
///
/// For small files (below `min_size`), returns a single chunk spanning the entire input.
/// For larger files, uses a gear-hash rolling window to find natural boundaries
/// that are stable across insertions and deletions.
pub fn chunk_data(data: &[u8], config: &ChunkConfig) -> Vec<Chunk> {
    if data.is_empty() {
        return Vec::new();
    }

    if data.len() <= config.min_size {
        return vec![Chunk {
            offset: 0,
            length: data.len(),
        }];
    }

    let mask = mask_for_avg(config.avg_size);
    let mut chunks = Vec::new();
    let mut start = 0;
    // Rolling hash persists across chunks for stability
    let mut hash: u64 = 0;

    while start < data.len() {
        let remaining = data.len() - start;
        if remaining <= config.min_size {
            chunks.push(Chunk {
                offset: start,
                length: remaining,
            });
            break;
        }

        // Advance the hash through the minimum-size window (no boundary check)
        for i in start..(start + config.min_size) {
            hash = hash.wrapping_shl(1).wrapping_add(GEAR_TABLE[data[i] as usize]);
        }

        let search_start = start + config.min_size;
        let search_end = (start + config.max_size).min(data.len());

        let mut found_boundary = false;

        for i in search_start..search_end {
            hash = hash.wrapping_shl(1).wrapping_add(GEAR_TABLE[data[i] as usize]);
            if hash & mask == 0 {
                let length = i + 1 - start;
                chunks.push(Chunk {
                    offset: start,
                    length,
                });
                start += length;
                found_boundary = true;
                break;
            }
        }

        if !found_boundary {
            let length = search_end - start;
            chunks.push(Chunk {
                offset: start,
                length,
            });
            start = search_end;
        }
    }

    chunks
}

/// Compute the mask from the target average size.
/// The mask has `log2(avg_size) - 1` low bits set.
fn mask_for_avg(avg_size: usize) -> u64 {
    let bits = (avg_size as f64).log2().round() as u32;
    (1u64 << bits) - 1
}

/// Gear table: 256 pseudo-random 64-bit values used for the rolling hash.
/// Generated deterministically so chunking is reproducible.
const GEAR_TABLE: [u64; 256] = {
    let mut table = [0u64; 256];
    let mut i = 0;
    while i < 256 {
        // Deterministic PRNG seeded from the byte value + offset.
        // Uses a splitmix64-style hash for good distribution.
        let mut x = (i as u64).wrapping_add(1).wrapping_mul(0x9e3779b97f4a7c15);
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
        x ^= x >> 31;
        table[i] = x;
        i += 1;
    }
    table
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_data_produces_no_chunks() {
        let chunks = chunk_data(&[], &ChunkConfig::default());
        assert!(chunks.is_empty());
    }

    #[test]
    fn small_data_single_chunk() {
        let data = vec![0u8; 100];
        let chunks = chunk_data(&data, &ChunkConfig::default());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].offset, 0);
        assert_eq!(chunks[0].length, 100);
    }

    #[test]
    fn chunks_cover_all_data() {
        let data = pseudo_random_data(100_000);
        let config = ChunkConfig::default();
        let chunks = chunk_data(&data, &config);

        // Verify complete coverage
        let total: usize = chunks.iter().map(|c| c.length).sum();
        assert_eq!(total, data.len());

        // Verify contiguous
        let mut offset = 0;
        for chunk in &chunks {
            assert_eq!(chunk.offset, offset);
            offset += chunk.length;
        }

        // Should produce multiple chunks for 100KB of data
        assert!(chunks.len() > 1, "expected multiple chunks, got {}", chunks.len());
    }

    /// Generate pseudo-random data using a simple xorshift PRNG.
    fn pseudo_random_data(len: usize) -> Vec<u8> {
        let mut state: u64 = 0xdeadbeef12345678;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state & 0xFF) as u8
            })
            .collect()
    }

    #[test]
    fn chunks_respect_min_size() {
        let data = pseudo_random_data(100_000);
        let config = ChunkConfig {
            min_size: 4096,
            avg_size: 8192,
            max_size: 65536,
        };
        let chunks = chunk_data(&data, &config);

        // All chunks except possibly the last should be >= min_size
        for chunk in chunks.iter().take(chunks.len().saturating_sub(1)) {
            assert!(
                chunk.length >= config.min_size,
                "chunk at offset {} has length {} < min {}",
                chunk.offset,
                chunk.length,
                config.min_size
            );
        }
    }

    #[test]
    fn chunks_respect_max_size() {
        let data = pseudo_random_data(200_000);
        let config = ChunkConfig::default();
        let chunks = chunk_data(&data, &config);

        for chunk in &chunks {
            assert!(
                chunk.length <= config.max_size,
                "chunk at offset {} has length {} > max {}",
                chunk.offset,
                chunk.length,
                config.max_size
            );
        }
    }

    #[test]
    fn insertion_stability() {
        // Verify that inserting data near the start doesn't change all later chunks.
        let original = pseudo_random_data(200_000);
        let mut modified = Vec::with_capacity(original.len() + 500);
        modified.extend_from_slice(&original[..1000]);
        modified.extend_from_slice(&[42u8; 500]); // insert 500 bytes near start
        modified.extend_from_slice(&original[1000..]);

        let config = ChunkConfig::default();
        let orig_chunks = chunk_data(&original, &config);
        let mod_chunks = chunk_data(&modified, &config);

        // Both should produce multiple chunks
        assert!(
            orig_chunks.len() > 5,
            "expected >5 chunks for 200KB, got {}",
            orig_chunks.len()
        );
        assert!(mod_chunks.len() > 5);

        // After re-sync, chunks should share content. Compare by
        // collecting chunk data hashes and checking overlap.
        let orig_hashes: Vec<blake3::Hash> = orig_chunks
            .iter()
            .map(|c| blake3::hash(&original[c.offset..c.offset + c.length]))
            .collect();
        let mod_hashes: Vec<blake3::Hash> = mod_chunks
            .iter()
            .map(|c| blake3::hash(&modified[c.offset..c.offset + c.length]))
            .collect();

        let shared = orig_hashes
            .iter()
            .filter(|h| mod_hashes.contains(h))
            .count();

        // At least some chunks should be shared (CDC should re-sync)
        assert!(
            shared > 0,
            "expected some shared chunks after insertion, got 0"
        );
    }

    #[test]
    fn deterministic() {
        let data = pseudo_random_data(30_000);
        let config = ChunkConfig::default();
        let chunks1 = chunk_data(&data, &config);
        let chunks2 = chunk_data(&data, &config);
        assert_eq!(chunks1, chunks2);
    }
}
