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

/// Validate one query before aggregation, so a mean cannot hide a failed task.
/// Baselines must use the same corpus, query, labels, and result limit.
pub fn validate_query_score(score: f64, baseline: Option<f64>) -> Result<(), String> {
    if !score.is_finite() || !(0.0..=1.0).contains(&score) || score == 0.0 {
        return Err(format!("no relevant evidence or invalid score: {score}"));
    }
    if let Some(baseline) = baseline {
        if !baseline.is_finite() || !(0.0..=1.0).contains(&baseline) {
            return Err(format!("invalid baseline: {baseline}"));
        }
        if score < baseline {
            return Err(format!(
                "query regressed: {score:.3} vs baseline {baseline:.3}"
            ));
        }
    }
    Ok(())
}

/// Deduplicate file hits without promoting past malformed response entries.
/// `None` represents a missing or non-string JSON path; it is a protocol error,
/// not evidence that may be discarded before computing success at rank one.
pub fn checked_file_order<'a>(
    paths: impl IntoIterator<Item = Option<&'a str>>,
) -> Result<Vec<String>, String> {
    let mut seen = std::collections::HashSet::new();
    let mut order = Vec::new();
    for (index, path) in paths.into_iter().enumerate() {
        let path = path
            .filter(|path| !path.trim().is_empty())
            .ok_or_else(|| format!("match {index} must contain a nonempty string path"))?;
        if seen.insert(path.to_string()) {
            order.push(path.to_string());
        }
    }
    Ok(order)
}

#[cfg(test)]
mod relevance_tests {
    use super::{checked_file_order, validate_query_score};

    #[test]
    fn malformed_match_cannot_manufacture_top_one_success() {
        for invalid in [None, Some(""), Some("  ")] {
            assert!(checked_file_order([invalid, Some("correct.rs")]).is_err());
            assert!(checked_file_order([Some("correct.rs"), invalid]).is_err());
        }
    }

    #[test]
    fn file_deduplication_preserves_first_seen_order() {
        assert_eq!(
            checked_file_order([Some("b.rs"), Some("a.rs"), Some("b.rs")]).unwrap(),
            ["b.rs", "a.rs"]
        );
        assert!(checked_file_order([]).unwrap().is_empty());
    }

    #[test]
    fn zero_and_nonfinite_quality_fail() {
        for score in [0.0, -0.1, 1.1, f64::NAN, f64::INFINITY] {
            assert!(validate_query_score(score, None).is_err(), "{score}");
        }
        assert!(validate_query_score(0.7, None).is_ok());
    }

    #[test]
    fn individual_regression_cannot_hide_in_mean() {
        let baseline = [0.8, 0.2];
        let candidate = [0.5, 0.8];
        assert!(candidate.iter().sum::<f64>() > baseline.iter().sum::<f64>());
        assert!(validate_query_score(candidate[0], Some(baseline[0])).is_err());
        assert!(validate_query_score(candidate[1], Some(baseline[1])).is_ok());
    }

    #[test]
    fn regressions_and_invalid_baselines_are_explicit() {
        assert!(validate_query_score(0.8, Some(0.8)).is_ok());
        assert!(validate_query_score(0.79, Some(0.8)).is_err());
        assert!(validate_query_score(0.9, Some(f64::NAN)).is_err());
    }
}
