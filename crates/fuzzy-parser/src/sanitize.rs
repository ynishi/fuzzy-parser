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
//! - **Mismatched closing delimiters**: `{"a": [1}` → `{"a": [1]}`
//! - **Stray closing delimiters**: `{"a": 1}}` → `{"a": 1}`
//! - **Single-quoted strings and keys**: `{'a': 'b'}` → `{"a": "b"}`
//! - **Unquoted object keys**: `{a: 1}` → `{"a": 1}`
//! - **Python-style literals**: `True` / `False` / `None` → `true` / `false` / `null`
//! - **Comments**: `// line`, `/* block */`, and `# line` comments are removed
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
/// - Mismatched closing delimiters (the expected closer is inserted first)
/// - Stray closing delimiters with no matching opener (dropped)
/// - Single-quoted strings / keys (`{'a': 'b'}` → `{"a": "b"}`)
/// - Unquoted object keys (`{a: 1}` → `{"a": 1}`)
/// - Python-style literals (`True` / `False` / `None` → `true` / `false` / `null`)
/// - `//` line, `/* */` block, and `#` line comments (removed)
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
/// // Single quotes, unquoted keys, Python literals, comments
/// assert_eq!(
///     sanitize_json(r#"{name: 'test', flag: True} // done"#),
///     r#"{"name": "test", "flag": true} "#
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

    // Step 1: Normalize lenient syntax (comments, single quotes,
    // unquoted keys, Python literals)
    let normalized = normalize_lenient_syntax(trimmed);

    // Step 2: Fix missing closing delimiters
    let with_delimiters = fix_missing_delimiters(&normalized);

    // Step 3: Remove trailing commas (now that delimiters exist)
    remove_trailing_commas(&with_delimiters)
}

/// String-scanner state for [`normalize_lenient_syntax`].
enum StrState {
    /// Outside any string literal
    None,
    /// Inside a double-quoted string
    Double,
    /// Inside a single-quoted string (being converted to double-quoted)
    Single,
}

/// Normalize lenient (non-JSON) syntax into strict JSON in one
/// string-aware scan:
///
/// - `// line`, `/* block */`, and `# line` comments are removed (only
///   outside strings; an unclosed block comment runs to end of input).
/// - Single-quoted strings become double-quoted: embedded `"` is escaped,
///   `\'` becomes a plain `'`, other escape pairs pass through.
/// - Bare identifiers directly followed by `:` are quoted as object keys
///   (`{a: 1}` → `{"a": 1}`).
/// - Bare Python literals are mapped: `True` → `true`, `False` → `false`,
///   `None` → `null`. Other bare identifiers are left untouched.
fn normalize_lenient_syntax(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut state = StrState::None;
    let mut escape_next = false;

    while let Some(c) = chars.next() {
        match state {
            StrState::Double => {
                if escape_next {
                    result.push(c);
                    escape_next = false;
                    continue;
                }
                match c {
                    '\\' => {
                        result.push(c);
                        escape_next = true;
                    }
                    '"' => {
                        state = StrState::None;
                        result.push(c);
                    }
                    _ => result.push(c),
                }
            }
            StrState::Single => {
                if escape_next {
                    if c == '\'' {
                        // \' has no meaning in JSON — a plain apostrophe
                        result.push('\'');
                    } else {
                        result.push('\\');
                        result.push(c);
                    }
                    escape_next = false;
                    continue;
                }
                match c {
                    '\\' => escape_next = true,
                    '\'' => {
                        state = StrState::None;
                        result.push('"');
                    }
                    '"' => result.push_str("\\\""),
                    _ => result.push(c),
                }
            }
            StrState::None => match c {
                '"' => {
                    state = StrState::Double;
                    result.push(c);
                }
                '\'' => {
                    state = StrState::Single;
                    result.push('"');
                }
                '/' if chars.peek() == Some(&'/') => {
                    // Line comment: skip to (but keep) the newline
                    while let Some(&n) = chars.peek() {
                        if n == '\n' {
                            break;
                        }
                        chars.next();
                    }
                }
                '/' if chars.peek() == Some(&'*') => {
                    // Block comment: skip past the closing */ (or to EOF)
                    chars.next(); // consume '*'
                    let mut prev = '\0';
                    for n in chars.by_ref() {
                        if prev == '*' && n == '/' {
                            break;
                        }
                        prev = n;
                    }
                }
                '#' => {
                    // Line comment: skip to (but keep) the newline
                    while let Some(&n) = chars.peek() {
                        if n == '\n' {
                            break;
                        }
                        chars.next();
                    }
                }
                c if c.is_ascii_alphabetic() || c == '_' || c == '$' => {
                    let mut ident = String::new();
                    ident.push(c);
                    while let Some(&n) = chars.peek() {
                        if n.is_ascii_alphanumeric() || n == '_' || n == '$' {
                            ident.push(n);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    // Peek past whitespace: is this identifier a key?
                    let mut look = chars.clone();
                    let next_non_ws = loop {
                        match look.next() {
                            Some(w) if w.is_whitespace() => continue,
                            other => break other,
                        }
                    };
                    if next_non_ws == Some(':') {
                        // Unquoted object key
                        result.push('"');
                        result.push_str(&ident);
                        result.push('"');
                    } else {
                        match ident.as_str() {
                            "True" => result.push_str("true"),
                            "False" => result.push_str("false"),
                            "None" => result.push_str("null"),
                            _ => result.push_str(&ident),
                        }
                    }
                }
                _ => result.push(c),
            },
        }
    }

    // An unclosed single-quoted string was already reopened as `"`; the
    // delimiter pass will close it like any other unclosed string.
    result
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

/// Fix missing or mismatched closing braces `}` and brackets `]`
///
/// Rebuilds the input while tracking opening delimiters on a stack, so that
/// the output nesting is always balanced:
///
/// - A closer matching the stack top passes through unchanged.
/// - A closer that mismatches the stack top first closes the intervening
///   open scopes (`{"a": [1}` → `{"a": [1]}`). This recovers the common LLM
///   failure of closing an outer scope while forgetting an inner one.
/// - A stray closer with no matching opener on the stack is dropped
///   (`{"a":1}}` → `{"a":1}`), since keeping it can only produce trailing
///   garbage that serde_json rejects.
/// - Any scopes still open at end of input are closed in reverse order.
fn fix_missing_delimiters(input: &str) -> String {
    let mut result = String::with_capacity(input.len() + 4);
    let mut in_string = false;
    let mut escape_next = false;

    // Stack to track opening delimiters: '{' or '['
    let mut stack: Vec<char> = Vec::new();

    for c in input.chars() {
        if escape_next {
            escape_next = false;
            result.push(c);
            continue;
        }

        match c {
            '\\' if in_string => {
                escape_next = true;
                result.push(c);
            }
            '"' => {
                in_string = !in_string;
                result.push(c);
            }
            '{' | '[' if !in_string => {
                stack.push(c);
                result.push(c);
            }
            '}' | ']' if !in_string => {
                let opener = if c == '}' { '{' } else { '[' };
                if stack.contains(&opener) {
                    // Close intervening (mismatched) open scopes so the
                    // output nesting stays valid, then emit this closer.
                    while let Some(&top) = stack.last() {
                        if top == opener {
                            stack.pop();
                            break;
                        }
                        stack.pop();
                        result.push(closer_for(top));
                    }
                    result.push(c);
                }
                // No matching opener anywhere: drop the stray closer.
            }
            _ => {
                result.push(c);
            }
        }
    }

    // Close unclosed string if any
    if in_string {
        result.push('"');
    }

    // Append missing closing delimiters in reverse order
    for &opener in stack.iter().rev() {
        result.push(closer_for(opener));
    }

    result
}

/// The closing delimiter that matches an opening `{` or `[`
fn closer_for(opener: char) -> char {
    if opener == '{' {
        '}'
    } else {
        ']'
    }
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
    fn test_mismatched_closer_object_over_array() {
        // '}' arrives while '[' is still open: close the array first.
        assert_eq!(sanitize_json(r#"{"a": [1}"#), r#"{"a": [1]}"#);
    }

    #[test]
    fn test_mismatched_closer_array_over_object() {
        // ']' arrives while '{' is still open: close the object first.
        assert_eq!(sanitize_json(r#"[{"a":1]"#), r#"[{"a":1}]"#);
    }

    #[test]
    fn test_stray_extra_closing_brace() {
        // Second '}' has no matching opener: dropped.
        assert_eq!(sanitize_json(r#"{"a":1}}"#), r#"{"a":1}"#);
    }

    #[test]
    fn test_stray_extra_closing_bracket() {
        assert_eq!(sanitize_json(r#"[1, 2]]"#), r#"[1, 2]"#);
    }

    #[test]
    fn test_mismatched_closer_deep_nesting() {
        // ']' closes both inner objects before matching the array.
        assert_eq!(sanitize_json(r#"[{"a": {"b": 1]"#), r#"[{"a": {"b": 1}}]"#);
    }

    #[test]
    fn test_mismatched_outputs_are_valid_json() {
        for input in [r#"{"a": [1}"#, r#"{"a":1}}"#, r#"[{"a":1]"#, r#"[1, 2]]"#] {
            let fixed = sanitize_json(input);
            assert!(
                serde_json::from_str::<serde_json::Value>(&fixed).is_ok(),
                "sanitize_json({:?}) produced invalid JSON: {:?}",
                input,
                fixed
            );
        }
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
    // Lenient Syntax Tests (single quotes, unquoted keys, literals, comments)
    // =========================================================================

    #[test]
    fn test_single_quoted_strings() {
        assert_eq!(sanitize_json(r#"{'a': 'b'}"#), r#"{"a": "b"}"#);
    }

    #[test]
    fn test_single_quoted_with_embedded_double_quote() {
        assert_eq!(
            sanitize_json(r#"{'msg': 'say "hi"'}"#),
            r#"{"msg": "say \"hi\""}"#
        );
    }

    #[test]
    fn test_single_quoted_with_escaped_apostrophe() {
        assert_eq!(
            sanitize_json(r#"{'msg': 'it\'s ok'}"#),
            r#"{"msg": "it's ok"}"#
        );
    }

    #[test]
    fn test_apostrophe_inside_double_string_untouched() {
        assert_eq!(
            sanitize_json(r#"{"msg": "it's ok"}"#),
            r#"{"msg": "it's ok"}"#
        );
    }

    #[test]
    fn test_single_quote_comma_brace_protected() {
        // Commas and braces inside single-quoted strings survive conversion
        assert_eq!(sanitize_json(r#"{'msg': 'a,}'}"#), r#"{"msg": "a,}"}"#);
    }

    #[test]
    fn test_unclosed_single_quoted_string() {
        assert_eq!(sanitize_json(r#"{'a': 'test"#), r#"{"a": "test"}"#);
    }

    #[test]
    fn test_unquoted_keys() {
        assert_eq!(
            sanitize_json(r#"{a: 1, b_c: 2, $d: 3}"#),
            r#"{"a": 1, "b_c": 2, "$d": 3}"#
        );
    }

    #[test]
    fn test_unquoted_key_with_whitespace_before_colon() {
        assert_eq!(sanitize_json("{timeout : 30}"), r#"{"timeout" : 30}"#);
    }

    #[test]
    fn test_python_literals() {
        assert_eq!(
            sanitize_json(r#"{"a": True, "b": False, "c": None}"#),
            r#"{"a": true, "b": false, "c": null}"#
        );
    }

    #[test]
    fn test_python_literals_in_string_untouched() {
        assert_eq!(sanitize_json(r#"{"a": "True"}"#), r#"{"a": "True"}"#);
    }

    #[test]
    fn test_json_literals_untouched() {
        assert_eq!(
            sanitize_json(r#"{"a": true, "b": false, "c": null}"#),
            r#"{"a": true, "b": false, "c": null}"#
        );
    }

    #[test]
    fn test_line_comment_removed() {
        assert_eq!(
            sanitize_json("{\"a\": 1, // the answer\n\"b\": 2}"),
            "{\"a\": 1, \n\"b\": 2}"
        );
    }

    #[test]
    fn test_block_comment_removed() {
        assert_eq!(sanitize_json(r#"{"a": /* note */ 1}"#), r#"{"a":  1}"#);
    }

    #[test]
    fn test_hash_comment_removed() {
        assert_eq!(
            sanitize_json("{\"a\": 1 # trailing note\n}"),
            "{\"a\": 1 \n}"
        );
    }

    #[test]
    fn test_unclosed_block_comment_runs_to_eof() {
        assert_eq!(sanitize_json(r#"{"a": 1} /* dangling"#), "{\"a\": 1} ");
    }

    #[test]
    fn test_url_in_string_not_treated_as_comment() {
        assert_eq!(
            sanitize_json(r#"{"url": "https://example.com"}"#),
            r#"{"url": "https://example.com"}"#
        );
    }

    #[test]
    fn test_hash_in_string_not_treated_as_comment() {
        assert_eq!(sanitize_json(r##"{"tag": "#1"}"##), r##"{"tag": "#1"}"##);
    }

    #[test]
    fn test_lenient_combo_all_at_once() {
        let input = "{\n  name: 'test', // comment\n  flag: True,\n}";
        let fixed = sanitize_json(input);
        let value: serde_json::Value = serde_json::from_str(&fixed).unwrap();
        assert_eq!(value["name"], "test");
        assert_eq!(value["flag"], true);
    }

    #[test]
    fn test_lenient_outputs_are_valid_json() {
        for input in [
            r#"{'a': 'b'}"#,
            r#"{a: 1}"#,
            r#"{"a": True}"#,
            "{\"a\": 1 // c\n}",
            r#"{'msg': 'it\'s "x", ok'}"#,
            "{a: 'b', c: None, /* x */ d: [1,2,],",
        ] {
            let fixed = sanitize_json(input);
            assert!(
                serde_json::from_str::<serde_json::Value>(&fixed).is_ok(),
                "sanitize_json({:?}) produced invalid JSON: {:?}",
                input,
                fixed
            );
        }
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
