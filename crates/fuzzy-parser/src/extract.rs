//! Extraction of JSON payloads from raw LLM output
//!
//! LLMs rarely return a bare JSON document: the payload is typically
//! wrapped in a Markdown code fence, surrounded by explanatory prose,
//! or both. This module locates the JSON payload(s) inside such output
//! so they can be fed to [`sanitize_json`](crate::sanitize_json) and the
//! repair functions.
//!
//! # Pipeline position
//!
//! Extraction is stage 0 of the repair pipeline:
//!
//! ```text
//! raw LLM output --extract_json--> JSON-ish text --sanitize_json--> valid JSON
//!                                                --repair_*-------> typo-fixed JSON
//! ```
//!
//! # Design
//!
//! Extraction is intentionally lenient: the returned slice does not have
//! to be valid JSON (it may contain trailing commas or be truncated) —
//! that is what the sanitize stage is for. Balanced-delimiter tracking is
//! string-aware, so braces inside string literals do not confuse it.

/// Strip a Markdown code fence wrapper from the input.
///
/// If the (trimmed) input is a single fenced block — ` ``` ` or
/// ` ```json ` etc. — the inner content is returned. A missing closing
/// fence (truncated output) is tolerated. Inputs that are not a single
/// fenced block are returned unchanged.
///
/// # Example
///
/// ```
/// use fuzzy_parser::strip_code_fences;
///
/// let output = "```json\n{\"a\": 1}\n```";
/// assert_eq!(strip_code_fences(output), "{\"a\": 1}");
/// ```
pub fn strip_code_fences(input: &str) -> &str {
    let trimmed = input.trim();
    let Some(after_open) = trimmed.strip_prefix("```") else {
        return input;
    };
    // Skip the info string (e.g. "json") up to the end of the opening line
    let Some(newline) = after_open.find('\n') else {
        return input; // single-line fence marker; nothing to unwrap
    };
    let body = after_open[newline + 1..].trim_end();
    // Drop the closing fence if present (tolerate truncation if absent)
    let body = body.strip_suffix("```").unwrap_or(body);
    body.trim()
}

/// Extract the first JSON payload from raw LLM output.
///
/// Handles Markdown code fences and surrounding prose. Among the balanced
/// `{...}` / `[...]` blocks found, the first one that parses as JSON
/// (directly, or after [`sanitize_json`](crate::sanitize_json)) is
/// preferred; otherwise the first block is returned as-is (it may be
/// truncated output, which the sanitize stage can close).
///
/// Returns `None` when the input contains no `{` or `[` at all.
///
/// # Example
///
/// ````
/// use fuzzy_parser::extract_json;
///
/// let output = r#"Sure! Here is the result:
///
/// ```json
/// {"type": "AddDerive", "target": "User"}
/// ```
///
/// Let me know if you need anything else."#;
///
/// assert_eq!(
///     extract_json(output),
///     Some(r#"{"type": "AddDerive", "target": "User"}"#)
/// );
/// ````
pub fn extract_json(input: &str) -> Option<&str> {
    let blocks = extract_json_blocks(input);

    // Prefer the first block that already parses as JSON
    for block in &blocks {
        if serde_json::from_str::<serde_json::Value>(block).is_ok() {
            return Some(block);
        }
    }
    // Then the first block that parses after sanitization
    for block in &blocks {
        let sanitized = crate::sanitize::sanitize_json(block);
        if serde_json::from_str::<serde_json::Value>(&sanitized).is_ok() {
            return Some(block);
        }
    }
    // Fall back to the first balanced block (e.g. heavily malformed input)
    blocks.first().copied()
}

/// Extract all top-level JSON payloads from raw LLM output.
///
/// Scans the input (after unwrapping a surrounding code fence, if any) for
/// balanced `{...}` / `[...]` blocks. A final unbalanced block (truncated
/// LLM output) is included as the last element so the sanitize stage can
/// close it.
///
/// # Example
///
/// ```
/// use fuzzy_parser::extract_json_blocks;
///
/// let output = r#"First: {"a": 1} and second: {"b": 2}"#;
/// let blocks = extract_json_blocks(output);
/// assert_eq!(blocks, vec![r#"{"a": 1}"#, r#"{"b": 2}"#]);
/// ```
pub fn extract_json_blocks(input: &str) -> Vec<&str> {
    let mut blocks = Vec::new();

    // Prefer payloads inside Markdown code fences: this keeps fence
    // markers out of truncated tails and skips prose braces entirely.
    for segment in fenced_segments(input) {
        scan_blocks(segment, &mut blocks);
    }
    if !blocks.is_empty() {
        return blocks;
    }

    scan_blocks(strip_code_fences(input), &mut blocks);
    blocks
}

/// Collect the inner content of every Markdown code fence in the input.
///
/// A dangling opening fence (truncated output) contributes the rest of
/// the input as its segment.
fn fenced_segments(input: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut rest = input;

    while let Some(open) = rest.find("```") {
        let after_open = &rest[open + 3..];
        // The fence body starts after the opening line (info string)
        let Some(newline) = after_open.find('\n') else {
            break;
        };
        let body = &after_open[newline + 1..];
        match body.find("```") {
            Some(close) => {
                segments.push(&body[..close]);
                rest = &body[close + 3..];
            }
            None => {
                segments.push(body); // truncated: no closing fence
                break;
            }
        }
    }

    segments
}

/// Scan a text segment for balanced JSON blocks, appending to `blocks`.
fn scan_blocks<'a>(s: &'a str, blocks: &mut Vec<&'a str>) {
    let mut pos = 0;

    while let Some(offset) = s[pos..].find(['{', '[']) {
        let start = pos + offset;
        match balanced_end(&s[start..]) {
            Some(len) => {
                blocks.push(&s[start..start + len]);
                pos = start + len;
            }
            None => {
                // Truncated final block: include the tail for the
                // sanitize stage to close.
                blocks.push(s[start..].trim_end());
                break;
            }
        }
    }
}

/// Find the byte length of the balanced block starting at the first
/// character of `s` (which must be `{` or `[`).
///
/// String-aware: delimiters inside string literals are ignored. Mismatched
/// closers (`{"a": [1}`) still close the block — the sanitize stage
/// repairs the mismatch itself.
fn balanced_end(s: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (i, c) in s.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i + c.len_utf8());
                }
            }
            _ => {}
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_fence_with_language_tag() {
        let input = "```json\n{\"a\": 1}\n```";
        assert_eq!(strip_code_fences(input), "{\"a\": 1}");
    }

    #[test]
    fn test_strip_fence_without_language_tag() {
        let input = "```\n[1, 2]\n```";
        assert_eq!(strip_code_fences(input), "[1, 2]");
    }

    #[test]
    fn test_strip_fence_missing_closing() {
        // Truncated output: opening fence but no closing fence
        let input = "```json\n{\"a\": 1";
        assert_eq!(strip_code_fences(input), "{\"a\": 1");
    }

    #[test]
    fn test_strip_fence_not_fenced() {
        let input = "{\"a\": 1}";
        assert_eq!(strip_code_fences(input), "{\"a\": 1}");
    }

    #[test]
    fn test_extract_from_prose() {
        let input = "Here is the JSON you asked for: {\"a\": 1} — enjoy!";
        assert_eq!(extract_json(input), Some("{\"a\": 1}"));
    }

    #[test]
    fn test_extract_from_fenced_prose() {
        let input = "Sure!\n\n```json\n{\"type\": \"AddDerive\"}\n```\n\nAnything else?";
        assert_eq!(extract_json(input), Some("{\"type\": \"AddDerive\"}"));
    }

    #[test]
    fn test_extract_array_payload() {
        let input = "Result: [1, 2, 3] done";
        assert_eq!(extract_json(input), Some("[1, 2, 3]"));
    }

    #[test]
    fn test_extract_prefers_parseable_block() {
        // The first brace pair is prose, not JSON; the parseable block wins.
        let input = "In {this} example the payload is {\"a\": 1}.";
        assert_eq!(extract_json(input), Some("{\"a\": 1}"));
    }

    #[test]
    fn test_extract_prefers_sanitizable_block() {
        // Not directly parseable, but sanitize can fix the trailing comma.
        let input = "In {this} example the payload is {\"a\": 1,}.";
        assert_eq!(extract_json(input), Some("{\"a\": 1,}"));
    }

    #[test]
    fn test_extract_truncated_payload() {
        // Truncated output: unbalanced block is returned for sanitize to close
        let input = "Here you go: {\"a\": {\"b\": 1";
        assert_eq!(extract_json(input), Some("{\"a\": {\"b\": 1"));
    }

    #[test]
    fn test_extract_none_when_no_json() {
        assert_eq!(extract_json("no json here"), None);
    }

    #[test]
    fn test_extract_string_aware_braces() {
        // Braces inside string literals must not close the block
        let input = r#"{"note": "use } carefully", "a": 1}"#;
        assert_eq!(extract_json(input), Some(input));
    }

    #[test]
    fn test_extract_escaped_quote_in_string() {
        let input = r#"{"note": "say \"hi\" {ok}", "a": 1}"#;
        assert_eq!(extract_json(input), Some(input));
    }

    #[test]
    fn test_extract_multiple_blocks() {
        let input = r#"First: {"a": 1} and second: {"b": 2}"#;
        let blocks = extract_json_blocks(input);
        assert_eq!(blocks, vec![r#"{"a": 1}"#, r#"{"b": 2}"#]);
    }

    #[test]
    fn test_extract_blocks_includes_truncated_tail() {
        let input = r#"{"a": 1} then {"b": 2"#;
        let blocks = extract_json_blocks(input);
        assert_eq!(blocks, vec![r#"{"a": 1}"#, r#"{"b": 2"#]);
    }

    #[test]
    fn test_full_pipeline_extract_sanitize() {
        // extract -> sanitize on fenced, trailing-comma, truncated output
        let raw = "```json\n{\"items\": [1, 2,], \"done\": true\n```";
        let extracted = extract_json(raw).unwrap();
        let sanitized = crate::sanitize::sanitize_json(extracted);
        let value: serde_json::Value = serde_json::from_str(&sanitized).unwrap();
        assert_eq!(value["items"][1], 2);
        assert_eq!(value["done"], true);
    }
}
