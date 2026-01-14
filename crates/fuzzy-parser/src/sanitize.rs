//! JSON sanitization for malformed LLM output
//!
//! This module provides pre-processing functions to fix common JSON syntax errors
//! that LLMs often produce, making the JSON parseable before fuzzy repair.
//!
//! # Supported Fixes
//!
//! - **Trailing commas**: `{"a": 1,}` → `{"a": 1}`
//! - **Missing closing braces**: `{"a": 1` → `{"a": 1}`
//! - **Missing closing brackets**: `["a"` → `["a"]`
//!
//! # Example
//!
//! ```
//! use fuzzy_parser::sanitize_json;
//!
//! // Fix trailing comma
//! let input = r#"{"name": "test",}"#;
//! let fixed = sanitize_json(input);
//! assert_eq!(fixed, r#"{"name": "test"}"#);
//!
//! // Fix missing closing brace
//! let input = r#"{"name": "test""#;
//! let fixed = sanitize_json(input);
//! assert_eq!(fixed, r#"{"name": "test"}"#);
//!
//! // Combined with fuzzy repair
//! use fuzzy_parser::{repair_tagged_enum_json, TaggedEnumSchema, FuzzyOptions};
//!
//! let schema = TaggedEnumSchema::new("type", &["Action"], |_| Some(&["name"][..]));
//! let malformed = r#"{"type": "Action", "name": "test",}"#;
//!
//! let sanitized = sanitize_json(malformed);
//! let result = repair_tagged_enum_json(&sanitized, &schema, &FuzzyOptions::default()).unwrap();
//! assert_eq!(result.repaired["name"], "test");
//! ```
//!
//! # Design Notes
//!
//! This function performs **best-effort** sanitization. It handles common cases
//! but does not attempt to fix all possible JSON errors. For severely malformed
//! input, the result may still fail to parse.
//!
//! The function is designed to be:
//! - **Safe**: Never produces worse output than input
//! - **Fast**: Single-pass processing where possible
//! - **Predictable**: Only fixes well-defined error patterns

/// Sanitize malformed JSON string
///
/// Fixes common syntax errors that LLMs produce:
/// - Trailing commas before `}` or `]`
/// - Missing closing braces `}` or brackets `]`
///
/// # Arguments
///
/// * `input` - The potentially malformed JSON string
///
/// # Returns
///
/// A sanitized JSON string that may be parseable by serde_json.
///
/// # Examples
///
/// ```
/// use fuzzy_parser::sanitize_json;
///
/// // Trailing comma in object
/// assert_eq!(sanitize_json(r#"{"a": 1,}"#), r#"{"a": 1}"#);
///
/// // Trailing comma in array
/// assert_eq!(sanitize_json(r#"[1, 2, 3,]"#), r#"[1, 2, 3]"#);
///
/// // Missing closing brace
/// assert_eq!(sanitize_json(r#"{"a": 1"#), r#"{"a": 1}"#);
///
/// // Missing closing bracket
/// assert_eq!(sanitize_json(r#"["a", "b""#), r#"["a", "b"]"#);
///
/// // Nested structures
/// assert_eq!(
///     sanitize_json(r#"{"items": [1, 2,], "name": "test",}"#),
///     r#"{"items": [1, 2], "name": "test"}"#
/// );
///
/// // Already valid JSON passes through unchanged
/// assert_eq!(sanitize_json(r#"{"a": 1}"#), r#"{"a": 1}"#);
/// ```
pub fn sanitize_json(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    // Step 1: Fix missing closing delimiters first
    let with_delimiters = fix_missing_delimiters(trimmed);

    // Step 2: Remove trailing commas (now that delimiters exist)
    remove_trailing_commas(&with_delimiters)
}

/// Remove trailing commas before `}` or `]`
///
/// Handles commas inside strings correctly (does not remove them).
fn remove_trailing_commas(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escape_next = false;

    while let Some(c) = chars.next() {
        if escape_next {
            result.push(c);
            escape_next = false;
            continue;
        }

        match c {
            '\\' if in_string => {
                result.push(c);
                escape_next = true;
            }
            '"' => {
                in_string = !in_string;
                result.push(c);
            }
            ',' if !in_string => {
                // Look ahead to see if this comma is followed by } or ]
                // Skip whitespace when looking ahead
                let mut peek_iter = chars.clone();
                let next_non_ws = loop {
                    match peek_iter.next() {
                        Some(ws) if ws.is_whitespace() => continue,
                        other => break other,
                    }
                };

                if matches!(next_non_ws, Some('}') | Some(']')) {
                    // Skip this trailing comma
                    continue;
                }
                result.push(c);
            }
            _ => {
                result.push(c);
            }
        }
    }

    result
}

/// Fix missing closing braces `}` and brackets `]`
///
/// Counts unmatched opening delimiters and appends the necessary closing ones.
fn fix_missing_delimiters(input: &str) -> String {
    let mut result = String::from(input);
    let mut in_string = false;
    let mut escape_next = false;

    // Stack to track opening delimiters: '{' or '['
    let mut stack: Vec<char> = Vec::new();

    for c in input.chars() {
        if escape_next {
            escape_next = false;
            continue;
        }

        match c {
            '\\' if in_string => {
                escape_next = true;
            }
            '"' => {
                in_string = !in_string;
            }
            '{' if !in_string => {
                stack.push('{');
            }
            '[' if !in_string => {
                stack.push('[');
            }
            '}' if !in_string => {
                if let Some(&top) = stack.last() {
                    if top == '{' {
                        stack.pop();
                    }
                }
            }
            ']' if !in_string => {
                if let Some(&top) = stack.last() {
                    if top == '[' {
                        stack.pop();
                    }
                }
            }
            _ => {}
        }
    }

    // Close unclosed string if any
    if in_string {
        result.push('"');
    }

    // Append missing closing delimiters in reverse order
    for &opener in stack.iter().rev() {
        match opener {
            '{' => result.push('}'),
            '[' => result.push(']'),
            _ => {}
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Trailing Comma Tests
    // =========================================================================

    #[test]
    fn test_trailing_comma_object() {
        assert_eq!(sanitize_json(r#"{"a": 1,}"#), r#"{"a": 1}"#);
    }

    #[test]
    fn test_trailing_comma_array() {
        assert_eq!(sanitize_json(r#"[1, 2, 3,]"#), r#"[1, 2, 3]"#);
    }

    #[test]
    fn test_trailing_comma_nested_object() {
        assert_eq!(
            sanitize_json(r#"{"outer": {"inner": 1,},}"#),
            r#"{"outer": {"inner": 1}}"#
        );
    }

    #[test]
    fn test_trailing_comma_nested_array() {
        assert_eq!(sanitize_json(r#"[[1, 2,], [3,],]"#), r#"[[1, 2], [3]]"#);
    }

    #[test]
    fn test_trailing_comma_mixed() {
        assert_eq!(
            sanitize_json(r#"{"items": [1, 2,], "name": "test",}"#),
            r#"{"items": [1, 2], "name": "test"}"#
        );
    }

    #[test]
    fn test_trailing_comma_with_whitespace() {
        assert_eq!(sanitize_json(r#"{"a": 1 , }"#), r#"{"a": 1  }"#);
        assert_eq!(sanitize_json("{\n  \"a\": 1,\n}"), "{\n  \"a\": 1\n}");
    }

    #[test]
    fn test_comma_in_string_preserved() {
        // Commas inside strings should NOT be removed
        assert_eq!(
            sanitize_json(r#"{"msg": "hello, world"}"#),
            r#"{"msg": "hello, world"}"#
        );
        assert_eq!(sanitize_json(r#"{"msg": "a,}"}"#), r#"{"msg": "a,}"}"#);
    }

    #[test]
    fn test_no_trailing_comma() {
        assert_eq!(sanitize_json(r#"{"a": 1}"#), r#"{"a": 1}"#);
        assert_eq!(sanitize_json(r#"[1, 2, 3]"#), r#"[1, 2, 3]"#);
    }

    // =========================================================================
    // Missing Delimiter Tests
    // =========================================================================

    #[test]
    fn test_missing_closing_brace() {
        assert_eq!(sanitize_json(r#"{"a": 1"#), r#"{"a": 1}"#);
    }

    #[test]
    fn test_missing_closing_bracket() {
        assert_eq!(sanitize_json(r#"["a", "b""#), r#"["a", "b"]"#);
    }

    #[test]
    fn test_missing_multiple_braces() {
        assert_eq!(sanitize_json(r#"{"a": {"b": 1"#), r#"{"a": {"b": 1}}"#);
    }

    #[test]
    fn test_missing_multiple_brackets() {
        assert_eq!(sanitize_json(r#"[[1, 2], [3"#), r#"[[1, 2], [3]]"#);
    }

    #[test]
    fn test_missing_mixed_delimiters() {
        assert_eq!(sanitize_json(r#"{"items": [1, 2"#), r#"{"items": [1, 2]}"#);
    }

    #[test]
    fn test_brace_in_string_ignored() {
        // Braces inside strings should NOT be counted
        assert_eq!(sanitize_json(r#"{"msg": "{"}"#), r#"{"msg": "{"}"#);
    }

    #[test]
    fn test_no_missing_delimiters() {
        assert_eq!(sanitize_json(r#"{"a": 1}"#), r#"{"a": 1}"#);
        assert_eq!(sanitize_json(r#"[1, 2]"#), r#"[1, 2]"#);
    }

    // =========================================================================
    // Combined Tests
    // =========================================================================

    #[test]
    fn test_trailing_comma_and_missing_brace() {
        assert_eq!(sanitize_json(r#"{"a": 1,"#), r#"{"a": 1}"#);
    }

    #[test]
    fn test_trailing_comma_and_missing_bracket() {
        assert_eq!(sanitize_json(r#"[1, 2,"#), r#"[1, 2]"#);
    }

    #[test]
    fn test_complex_llm_output() {
        let input = r#"{
            "type": "AddDerive",
            "target": "User",
            "derives": ["Debug", "Clone",],
        "#;
        // Note: closing brace is appended directly (no formatting/indentation)
        let expected = r#"{
            "type": "AddDerive",
            "target": "User",
            "derives": ["Debug", "Clone"]}"#;
        assert_eq!(sanitize_json(input), expected);
    }

    // =========================================================================
    // Edge Cases
    // =========================================================================

    #[test]
    fn test_empty_input() {
        assert_eq!(sanitize_json(""), "");
        assert_eq!(sanitize_json("   "), "");
    }

    #[test]
    fn test_whitespace_only() {
        assert_eq!(sanitize_json("  \n\t  "), "");
    }

    #[test]
    fn test_simple_values() {
        assert_eq!(sanitize_json("null"), "null");
        assert_eq!(sanitize_json("true"), "true");
        assert_eq!(sanitize_json("123"), "123");
        assert_eq!(sanitize_json(r#""string""#), r#""string""#);
    }

    #[test]
    fn test_escaped_quote_in_string() {
        assert_eq!(
            sanitize_json(r#"{"msg": "say \"hello\""}"#),
            r#"{"msg": "say \"hello\""}"#
        );
    }

    #[test]
    fn test_escaped_backslash_in_string() {
        assert_eq!(
            sanitize_json(r#"{"path": "C:\\Users\\test"}"#),
            r#"{"path": "C:\\Users\\test"}"#
        );
    }

    #[test]
    fn test_unclosed_string() {
        // Unclosed string should be closed
        assert_eq!(sanitize_json(r#"{"a": "test"#), r#"{"a": "test"}"#);
    }

    #[test]
    fn test_deeply_nested() {
        assert_eq!(
            sanitize_json(r#"{"a": {"b": {"c": [1, 2,],"#),
            r#"{"a": {"b": {"c": [1, 2]}}}"#
        );
    }

    // =========================================================================
    // Real-world LLM Output Examples
    // =========================================================================

    #[test]
    fn test_llm_truncated_response() {
        let input = r#"{"type": "RenameIdent", "from": "old_name", "to": "new_na"#;
        let fixed = sanitize_json(input);
        assert_eq!(
            fixed,
            r#"{"type": "RenameIdent", "from": "old_name", "to": "new_na"}"#
        );
    }

    #[test]
    fn test_llm_array_with_trailing_comma() {
        let input = r#"{"intents": [
            {"type": "AddDerive", "target": "User",},
            {"type": "AddDerive", "target": "Post",},
        ]}"#;
        let fixed = sanitize_json(input);
        assert!(fixed.contains(r#""target": "User"}"#));
        assert!(fixed.contains(r#""target": "Post"}"#));
        assert!(!fixed.contains(",}"));
        assert!(!fixed.contains(",]"));
    }
}
