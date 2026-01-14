//! Error types for fuzzy parsing

use thiserror::Error;

/// Errors that can occur during fuzzy JSON repair
#[derive(Debug, Error)]
pub enum FuzzyError {
    /// JSON parsing failed
    #[error("JSON parse error: {0}")]
    JsonParse(#[from] serde_json::Error),

    /// Expected an object but got something else
    #[error("Expected JSON object, got different type")]
    NotObject,
}
