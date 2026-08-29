//! Benchmark support: a real-source corpus builder shared by benches.

/// Concatenated real Rust source from this workspace, repeated up to
/// `target_bytes`. Real code (not random bytes) so gram statistics reflect
/// the actual workload.
pub fn source_corpus(target_bytes: usize) -> Vec<u8> {
    let seed: &[&str] = &[
        include_str!("../../pixel-index/src/gram.rs"),
        include_str!("../../pixel-index/src/posting.rs"),
        include_str!("../../pixel-index/src/weights.rs"),
        include_str!("../../pixel-index/src/lib.rs"),
    ];
    let mut corpus = Vec::with_capacity(target_bytes + 4096);
    while corpus.len() < target_bytes {
        for s in seed {
            corpus.extend_from_slice(s.as_bytes());
            if corpus.len() >= target_bytes {
                break;
            }
        }
    }
    corpus.truncate(target_bytes);
    corpus
}
