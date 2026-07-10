# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1] - 2026-07-10

### Fixed

- Levenshtein and Damerau-Levenshtein similarity scores are now normalized by
  the character count of the longer string instead of its byte length. Byte-based
  normalization deflated scores for multi-byte (non-ASCII) input and could even
  yield out-of-range values, since `strsim` computes edit distance over `char`s.
- Match selection and sorting in `find_closest` / `find_all_matches` now use
  `f64::total_cmp` instead of `partial_cmp(...).unwrap()`, removing a potential
  panic path when a similarity score is `NaN`.
- `sanitize_json` now repairs mismatched closing delimiters (e.g. `{"a": [1}` →
  `{"a": [1]}`) and drops stray closing delimiters with no matching opener
  (e.g. `{"a":1}}` → `{"a":1}`). The delimiter pass rebuilds the input on a
  stack so its output nesting is always balanced.

### Added

- `#![warn(missing_docs)]` on the crate root, plus rustdoc for previously
  undocumented public items (`Match::new`, similarity normalization notes).
- Documented the first-win collision behavior of field-name repair
  (`repair_fields_with_list` / `repair_tagged_enum`): an existing key is never
  overwritten, so when two typo keys resolve to the same candidate only the
  first is renamed and the later one is left unchanged.
- Continuous integration workflow running `cargo fmt --check`,
  `cargo clippy --all-targets -- -D warnings`, and `cargo test`.
- Test coverage for non-ASCII similarity normalization, mismatched and stray
  delimiter sanitization, and first-win field collision cases.
- Package metadata: `homepage` and `documentation` fields, author contact
  in `authors`.

## [0.1.0] - 2026-01-14

### Added

- Initial release.
- `sanitize_json`: syntax repair for common LLM JSON errors (trailing commas,
  missing closing braces/brackets, unclosed strings).
- Fuzzy repair for internally-tagged enums (`repair_tagged_enum_json`,
  `TaggedEnumSchema`) covering tag values, field names, enum-array values, and
  nested-object fields.
- Configurable string-distance algorithms (Jaro-Winkler, Levenshtein,
  Damerau-Levenshtein) via `FuzzyOptions`.
- `Correction` records exposing every applied repair for transparency.

[Unreleased]: https://github.com/ynishi/fuzzy-parser/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/ynishi/fuzzy-parser/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/ynishi/fuzzy-parser/releases/tag/v0.1.0
