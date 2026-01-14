# fuzzy-parser

Automatic JSON repair for LLM-generated output

[![Crates.io](https://img.shields.io/crates/v/fuzzy-parser.svg)](https://crates.io/crates/fuzzy-parser)
[![Documentation](https://docs.rs/fuzzy-parser/badge.svg)](https://docs.rs/fuzzy-parser)
[![License](https://img.shields.io/crates/l/fuzzy-parser.svg)](LICENSE)

## Overview

LLM-generated JSON often contains typos and syntax errors. `fuzzy-parser` automatically repairs these issues, enabling robust LLM integration.

```rust
use fuzzy_parser::{sanitize_json, repair_tagged_enum_json, TaggedEnumSchema, FuzzyOptions};

// LLM output (typos + syntax errors)
let llm_output = r#"{"type": "AddDeriv", "taget": "User", "derives": ["Debg",],}"#;

// Step 1: Fix syntax errors
let sanitized = sanitize_json(llm_output);

// Step 2: Fix typos
let schema = TaggedEnumSchema::new("type", &["AddDerive"], |_| Some(&["target", "derives"][..]))
    .with_enum_array("derives", &["Debug", "Clone", "Serialize"]);

let result = repair_tagged_enum_json(&sanitized, &schema, &FuzzyOptions::default())?;

assert_eq!(result.repaired["type"], "AddDerive");      // AddDeriv → AddDerive
assert_eq!(result.repaired["target"], "User");          // taget → target
assert_eq!(result.repaired["derives"][0], "Debug");     // Debg → Debug
```

## Features

### JSON Sanitization (Syntax Repair)

| Error | Before | After |
|-------|--------|-------|
| Trailing comma (object) | `{"a": 1,}` | `{"a": 1}` |
| Trailing comma (array) | `[1, 2,]` | `[1, 2]` |
| Missing closing brace | `{"a": 1` | `{"a": 1}` |
| Missing closing bracket | `["a"` | `["a"]` |
| Unclosed string | `{"a": "test` | `{"a": "test"}` |

### Fuzzy Repair (Typo Correction)

| Target | Before | After |
|--------|--------|-------|
| Tag value (enum discriminator) | `"AddDeriv"` | `"AddDerive"` |
| Field name | `"taget"` | `"target"` |
| Enum array value | `["Debg"]` | `["Debug"]` |
| Nested object field | `{"timout": 30}` | `{"timeout": 30}` |

## Installation

```toml
[dependencies]
fuzzy-parser = "0.1"
```

## Usage

### Basic Usage

```rust
use fuzzy_parser::{sanitize_json, repair_tagged_enum_json, TaggedEnumSchema, FuzzyOptions};

// Define schema
let schema = TaggedEnumSchema::new(
    "type",                                    // tag field name
    &["AddDerive", "RemoveDerive", "Rename"],  // valid tag values
    |tag| match tag {
        "AddDerive" | "RemoveDerive" => Some(&["target", "derives"][..]),
        "Rename" => Some(&["from", "to"][..]),
        _ => None,
    },
);

// Repair
let json = r#"{"type": "AddDeriv", "taget": "User"}"#;
let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default())?;

println!("Repaired: {}", result.repaired);
println!("Corrections: {:?}", result.corrections);
```

### Enum Array Repair

```rust
let schema = TaggedEnumSchema::new("type", &["AddDerive"], |_| Some(&["target", "derives"][..]))
    .with_enum_array("derives", &["Debug", "Clone", "Serialize", "Default"]);

let json = r#"{"type": "AddDerive", "target": "User", "derives": ["Debg", "Clne"]}"#;
let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default())?;

// derives: ["Debug", "Clone"]
```

### Nested Object Repair

```rust
let schema = TaggedEnumSchema::new("type", &["Configure"], |_| Some(&["name", "config"][..]))
    .with_nested_object("config", &["timeout", "retries", "enabled"]);

let json = r#"{"type": "Configure", "name": "api", "config": {"timout": 30, "retres": 3}}"#;
let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default())?;

// config: {"timeout": 30, "retries": 3}
```

### Combined Sanitization + Repair

```rust
use fuzzy_parser::{sanitize_json, repair_tagged_enum_json, TaggedEnumSchema, FuzzyOptions};

let schema = TaggedEnumSchema::new("type", &["Action"], |_| Some(&["name", "data"][..]))
    .with_nested_object("data", &["value", "count"]);

// LLM output (syntax errors + typos)
let malformed = r#"{
    "type": "Acton",
    "nam": "test",
    "data": {"valeu": 42,},
}"#;

// Step 1: Sanitize (fix trailing commas, missing braces, etc.)
let sanitized = sanitize_json(malformed);

// Step 2: Repair (fix typos)
let result = repair_tagged_enum_json(&sanitized, &schema, &FuzzyOptions::default())?;
```

### Custom Options

```rust
use fuzzy_parser::{FuzzyOptions, Algorithm};

// Customize similarity threshold and algorithm
let options = FuzzyOptions::default()
    .with_min_similarity(0.8)                 // default: 0.7
    .with_algorithm(Algorithm::Levenshtein);  // default: JaroWinkler
```

### Inspecting Corrections

```rust
let result = repair_tagged_enum_json(json, &schema, &options)?;

if result.has_corrections() {
    println!("{} corrections made:", result.correction_count());
    for correction in &result.corrections {
        println!(
            "  {} → {} (similarity: {:.2}, path: {})",
            correction.original,
            correction.corrected,
            correction.similarity,
            correction.field_path
        );
    }
}
```

## Algorithms

| Algorithm | Characteristics | Best For |
|-----------|-----------------|----------|
| **Jaro-Winkler** (default) | Prefix-weighted, handles transpositions | General typo correction |
| Levenshtein | Equal cost for insert/delete/substitute | Edit distance based |
| Damerau-Levenshtein | Levenshtein + transposition support | Transposition-heavy typos |

## Design Principles

1. **Two-stage processing**: Syntax repair (sanitize) and typo repair (repair) are separated
2. **Schema-driven**: Caller defines the schema (library remains generic)
3. **Transparency**: All corrections are recorded as `Correction` structs
4. **Safety**: No corrections made below similarity threshold

## License

MIT OR Apache-2.0
