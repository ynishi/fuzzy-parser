//! String distance/similarity calculation utilities
//!
//! This module provides wrappers around strsim algorithms for fuzzy matching.

use strsim::{damerau_levenshtein, jaro_winkler, levenshtein};

/// Algorithm for similarity calculation
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Algorithm {
    /// Jaro-Winkler similarity (recommended for typos)
    ///
    /// Good for: transpositions, prefix matching
    /// Returns: 0.0 to 1.0 (1.0 = identical)
    #[default]
    JaroWinkler,

    /// Levenshtein distance (edit distance)
    ///
    /// Good for: insertions, deletions, substitutions
    /// Normalized to 0.0 to 1.0 (1.0 = identical)
    Levenshtein,

    /// Damerau-Levenshtein distance
    ///
    /// Like Levenshtein but also handles transpositions
    /// Normalized to 0.0 to 1.0 (1.0 = identical)
    DamerauLevenshtein,
}

/// Calculate similarity between two strings
///
/// Returns a value between 0.0 (completely different) and 1.0 (identical).
pub fn similarity(a: &str, b: &str, algo: Algorithm) -> f64 {
    if a == b {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    match algo {
        Algorithm::JaroWinkler => jaro_winkler(a, b),
        Algorithm::Levenshtein => {
            let dist = levenshtein(a, b);
            let max_len = a.len().max(b.len());
            1.0 - (dist as f64 / max_len as f64)
        }
        Algorithm::DamerauLevenshtein => {
            let dist = damerau_levenshtein(a, b);
            let max_len = a.len().max(b.len());
            1.0 - (dist as f64 / max_len as f64)
        }
    }
}

/// Match result with similarity score
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    /// The matched candidate string
    pub candidate: String,
    /// Similarity score (0.0 to 1.0)
    pub similarity: f64,
}

impl Match {
    pub fn new(candidate: impl Into<String>, similarity: f64) -> Self {
        Self {
            candidate: candidate.into(),
            similarity,
        }
    }
}

/// Find the closest match from a list of candidates
///
/// Returns `None` if no candidate meets the minimum similarity threshold.
pub fn find_closest<'a>(
    input: &str,
    candidates: impl IntoIterator<Item = &'a str>,
    min_similarity: f64,
    algo: Algorithm,
) -> Option<Match> {
    candidates
        .into_iter()
        .map(|c| Match::new(c, similarity(input, c, algo)))
        .filter(|m| m.similarity >= min_similarity)
        .max_by(|a, b| a.similarity.partial_cmp(&b.similarity).unwrap())
}

/// Find all matches above the minimum similarity threshold
///
/// Returns matches sorted by similarity (highest first).
pub fn find_all_matches<'a>(
    input: &str,
    candidates: impl IntoIterator<Item = &'a str>,
    min_similarity: f64,
    algo: Algorithm,
) -> Vec<Match> {
    let mut matches: Vec<_> = candidates
        .into_iter()
        .map(|c| Match::new(c, similarity(input, c, algo)))
        .filter(|m| m.similarity >= min_similarity)
        .collect();

    matches.sort_by(|a, b| b.similarity.partial_cmp(&a.similarity).unwrap());
    matches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identical_strings() {
        assert_eq!(similarity("hello", "hello", Algorithm::JaroWinkler), 1.0);
        assert_eq!(similarity("hello", "hello", Algorithm::Levenshtein), 1.0);
        assert_eq!(
            similarity("hello", "hello", Algorithm::DamerauLevenshtein),
            1.0
        );
    }

    #[test]
    fn test_empty_strings() {
        assert_eq!(similarity("", "", Algorithm::JaroWinkler), 1.0);
        assert_eq!(similarity("hello", "", Algorithm::JaroWinkler), 0.0);
        assert_eq!(similarity("", "hello", Algorithm::JaroWinkler), 0.0);
    }

    #[test]
    fn test_typo_detection_jaro_winkler() {
        // Typos should have high similarity with Jaro-Winkler
        let sim = similarity("AddDeriv", "AddDerive", Algorithm::JaroWinkler);
        assert!(sim > 0.9, "Expected > 0.9, got {}", sim);

        let sim = similarity("RenamIdent", "RenameIdent", Algorithm::JaroWinkler);
        assert!(sim > 0.9, "Expected > 0.9, got {}", sim);
    }

    #[test]
    fn test_field_name_typo() {
        let sim = similarity("target_name", "target", Algorithm::JaroWinkler);
        assert!(sim > 0.7, "Expected > 0.7, got {}", sim);

        let sim = similarity("struct_nam", "struct_name", Algorithm::JaroWinkler);
        assert!(sim > 0.9, "Expected > 0.9, got {}", sim);
    }

    #[test]
    fn test_find_closest() {
        let candidates = ["AddDerive", "RemoveDerive", "AddField", "RemoveField"];
        let result = find_closest(
            "AddDeriv",
            candidates.iter().copied(),
            0.7,
            Algorithm::JaroWinkler,
        );

        assert!(result.is_some());
        let m = result.unwrap();
        assert_eq!(m.candidate, "AddDerive");
        assert!(m.similarity > 0.9);
    }

    #[test]
    fn test_find_closest_no_match() {
        let candidates = ["AddDerive", "RemoveDerive"];
        let result = find_closest(
            "CompletelyDifferent",
            candidates.iter().copied(),
            0.9, // High threshold
            Algorithm::JaroWinkler,
        );

        assert!(result.is_none());
    }

    #[test]
    fn test_find_all_matches() {
        let candidates = ["target", "target_mod", "target_fn", "body"];
        let matches = find_all_matches(
            "target_name",
            candidates.iter().copied(),
            0.6,
            Algorithm::JaroWinkler,
        );

        assert!(!matches.is_empty());
        // Results should be sorted by similarity (highest first)
        for i in 1..matches.len() {
            assert!(matches[i - 1].similarity >= matches[i].similarity);
        }
    }

    #[test]
    fn test_transposition_damerau() {
        // Damerau-Levenshtein handles transpositions better
        let sim_dl = similarity("teh", "the", Algorithm::DamerauLevenshtein);
        let sim_l = similarity("teh", "the", Algorithm::Levenshtein);
        // DL should give same or higher similarity for transpositions
        assert!(sim_dl >= sim_l);
    }
}
