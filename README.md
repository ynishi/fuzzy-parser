# fuzzy-parser

Automatic JSON repair for LLM-generated output

[![Crates.io](https://img.shields.io/crates/v/fuzzy-parser.svg)](https://crates.io/crates/fuzzy-parser)
[![Documentation](https://docs.rs/fuzzy-parser/badge.svg)](https://docs.rs/fuzzy-parser)
[![License](https://img.shields.io/crates/l/fuzzy-parser.svg)](LICENSE)

## Overview

LLM-generated JSON often arrives wrapped in prose or code fences, with syntax
errors and typos. `fuzzy-parser` repairs these issues in three independent
stages, enabling robust LLM integration.

```rust
use fuzzy_parser::{extract_json, sanitize_json, repair_tagged_enum_json, TaggedEnumSchema, FuzzyOptions};

// Raw LLM output (prose + typos + syntax errors)
let llm_output = r#"Here you go: {"type": "AddDeriv", "taget": "User", "derives": ["Debg",],}"#;

// Step 0: Extract the JSON payload from the surrounding text
let payload = extract_json(llm_output).unwrap();

// Step 1: Fix syntax errors
let sanitized = sanitize_json(payload);

// Step 2: Fix typos
let schema = TaggedEnumSchema::new("type", &["AddDerive"], |_| Some(&["target", "derives"][..]))
    .with_enum_array("derives", &["Debug", "Clone", "Serialize"]);

let result = repair_tagged_enum_json(&sanitized, &schema, &FuzzyOptions::default())?;

assert_eq!(result.repaired["type"], "AddDerive");      // AddDeriv → AddDerive
assert_eq!(result.repaired["target"], "User");          // taget → target
assert_eq!(result.repaired["derives"][0], "Debug");     // Debg → Debug
```

## Features

### JSON Extraction (Stage 0)

| Input shape | Handled by |
|-------------|------------|
| Markdown code fence (```` ```json … ``` ````) | `extract_json` / `strip_code_fences` |
| JSON embedded in surrounding prose | `extract_json` |
| Multiple JSON blocks in one response | `extract_json_blocks` |
| Truncated output (unclosed block) | included as final block for sanitize to close |

### JSON Sanitization (Syntax Repair)

| Error | Before | After |
|-------|--------|-------|
| Trailing comma (object) | `{"a": 1,}` | `{"a": 1}` |
| Trailing comma (array) | `[1, 2,]` | `[1, 2]` |
| Missing closing brace | `{"a": 1` | `{"a": 1}` |
| Missing closing bracket | `["a"` | `["a"]` |
| Unclosed string | `{"a": "test` | `{"a": "test"}` |
| Single-quoted strings / keys | `{'a': 'b'}` | `{"a": "b"}` |
| Unquoted object keys | `{a: 1}` | `{"a": 1}` |
| Python-style literals | `{"a": True}` | `{"a": true}` |
| Comments (`//`, `/* */`, `#`) | `{"a": 1 // note` | `{"a": 1}` |

### Fuzzy Repair (Typo Correction)

| Target | Before | After |
|--------|--------|-------|
| Tag value (enum discriminator) | `"AddDeriv"` | `"AddDerive"` |
| Tag key itself | `{"tpye": "AddDerive"}` | `{"type": "AddDerive"}` |
| Field name | `"taget"` | `"target"` |
| Enum string value | `"inof"` | `"info"` |
| Enum array value | `["Debg"]` | `["Debug"]` |
| Nested object field (any depth) | `{"server": {"prot": 80}}` | `{"server": {"port": 80}}` |
| Array of objects | `[{"nam": "a"}]` | `[{"name": "a"}]` |
| Nested tagged enum / intent array | `{"intents": [{"type": "AddDeriv", ...}]}` | `{"intents": [{"type": "AddDerive", ...}]}` |

### Type Coercion (Schema-Driven)

| Expected | Before | After |
|----------|--------|-------|
| `FieldKind::Integer` | `"42"` | `42` |
| `FieldKind::Number` | `"0.5"` | `0.5` |
| `FieldKind::Bool` | `"true"` | `true` |
| `FieldKind::String` | `42` | `"42"` |

Coercion is lossless-only: unparseable values are left untouched. It can be
disabled with `FuzzyOptions::with_coerce_types(false)`.

### Schema-Shape Options (Opt-In)

Three additional schema-driven repairs, all off by default and all recorded
in the result log:

| Option | Before | After | Logged as |
|--------|--------|-------|-----------|
| `with_wrap_single_values(true)` | `"derives": "Debug"` | `"derives": ["Debug"]` | `Correction` |
| `with_unwrap_singleton_arrays(true)` | `"level": ["info"]` | `"level": "info"` | `Correction` |
| `with_fill_defaults(true)` | `{}` (field missing, default defined) | `{"retries": 3}` | `FilledDefault` |
| `with_drop_unknown_fields(true)` | `{"note": "hope this helps!"}` | `{}` (value kept in log) | `DroppedField` |

### JSON Schema Import

Repair schemas can be derived from JSON Schema documents instead of being
hand-built — including the output of `schemars`, Pydantic, or any other
schema generator.

| Input | Result |
|-------|--------|
| `oneOf` + tag `const` (internally / adjacently tagged enum) | `TaggedEnumSchema` |
| Plain object schema | `ObjectSchema` |
| `string` / `integer` / `number` / `boolean` | coercion `FieldKind`s |
| `enum`, arrays of enums, nested objects, arrays of objects | matching `FieldKind`s |
| `$ref` / `$defs` (incl. Draft 2020-12 sibling `$ref`) | resolved; cycles cut to `Any` + warning |
| Nested `oneOf` + tag `const` on a field (incl. arrays of them) | `FieldKind::TaggedEnum` / `TaggedEnumArray` (recursive repair) |
| Unsupported constructs (`allOf`, tuples, non-tagged `oneOf`, ...) | degrade to `Any` + `ImportWarning` (never silent) |

Externally tagged enums (serde's default representation) and untagged enums
are rejected with an explicit error — annotate the enum with
`#[serde(tag = "...")]` so repair has a tag field to anchor on.

## Installation

```toml
[dependencies]
fuzzy-parser = "0.4"

# Optional: derive repair schemas from #[derive(JsonSchema)] types
fuzzy-parser = { version = "0.4", features = ["schemars"] }
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

### Extracting JSON from LLM Output

```rust
use fuzzy_parser::{extract_json, extract_json_blocks};

let response = r#"Sure! Here is the result:

```json
{"type": "AddDerive", "target": "User"}
```

Let me know if you need anything else."#;

let payload = extract_json(response).unwrap();
assert_eq!(payload, r#"{"type": "AddDerive", "target": "User"}"#);

// Multiple payloads in one response
let multi = r#"First: {"a": 1} and second: {"b": 2}"#;
assert_eq!(extract_json_blocks(multi).len(), 2);
```

### Recursive Schemas (Nested Objects, Arrays of Objects)

`FieldKind` describes the expected shape of each field's value; nesting is
unlimited.

```rust
use fuzzy_parser::{FieldKind, ObjectSchema, TaggedEnumSchema, repair_tagged_enum_json, FuzzyOptions};

let schema = TaggedEnumSchema::with_tag("type").with_variant(
    "Batch",
    ObjectSchema::new(["name"]).with_field_kind(
        "items",
        FieldKind::ObjectArray(
            ObjectSchema::new(["path"])
                .with_field_kind("kind", FieldKind::enum_of(["file", "dir"]))
                .with_field_kind("depth", FieldKind::Integer),
        ),
    ),
);

let json = r#"{"type": "Batch", "name": "x", "items": [{"pth": "/a", "kind": "fille", "depth": "3"}]}"#;
let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default())?;

assert_eq!(result.repaired["items"][0]["path"], "/a");     // pth → path
assert_eq!(result.repaired["items"][0]["kind"], "file");   // fille → file
assert_eq!(result.repaired["items"][0]["depth"], 3);       // "3" → 3
```

### Dynamic Schemas (Built at Runtime)

Schemas own their data — field names can come from config files, API
definitions, or any runtime source.

```rust
use fuzzy_parser::{ObjectSchema, TaggedEnumSchema};

let tags: Vec<String> = load_tags_from_config();      // runtime data
let fields: Vec<String> = load_fields_from_config();

let mut schema = TaggedEnumSchema::with_tag("kind");
for tag in &tags {
    schema = schema.with_variant(tag, ObjectSchema::new(&fields));
}
```

### Importing a Schema from JSON Schema / schemars

```rust
use fuzzy_parser::{TaggedEnumSchema, FuzzyOptions, repair_tagged_enum_json};

// From a JSON Schema document (any source: schemars, Pydantic, files, ...)
let json_schema: serde_json::Value = serde_json::from_str(schema_text)?;
let import = TaggedEnumSchema::from_json_schema(&json_schema)?;

// Constructs without repair semantics are reported, never silently dropped
for warning in &import.warnings {
    eprintln!("schema import: {} — {}", warning.path, warning.detail);
}

let result = repair_tagged_enum_json(llm_json, &import.schema, &FuzzyOptions::default())?;
```

With the `schemars` feature, straight from a Rust type:

```rust
use fuzzy_parser::TaggedEnumSchema;

#[derive(serde::Serialize, schemars::JsonSchema)]
#[serde(tag = "type")]
enum Intent {
    AddDerive { target: String, count: i32 },
    Rename { from: String, to: String },
}

let import = TaggedEnumSchema::from_type::<Intent>()?;
// Tag typos, field typos, and "3" → 3 coercion now repair automatically
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

### Custom Options

```rust
use fuzzy_parser::{FuzzyOptions, Algorithm};

// Customize similarity threshold and algorithm
let options = FuzzyOptions::default()
    .with_min_similarity(0.8)                 // default: 0.7
    .with_algorithm(Algorithm::Levenshtein);  // default: JaroWinkler
```

### Strict Schema-Shaped Output (Wrap / Defaults / Drop)

When the repaired JSON feeds straight into `serde` deserialization, the
opt-in shape options make the output match the schema exactly — while
keeping every change (and every removed value) in the log:

```rust
use fuzzy_parser::{FieldKind, FuzzyOptions, ObjectSchema, TaggedEnumSchema, repair_tagged_enum_json};

let schema = TaggedEnumSchema::with_tag("type").with_variant(
    "AddDerive",
    ObjectSchema::new(["target"])
        .with_field_kind("derives", FieldKind::enum_array(["Debug", "Clone"]))
        .with_field_default("visibility", serde_json::json!("pub")),
);

let options = FuzzyOptions::default()
    .with_wrap_single_values(true)   // "Debug"  → ["Debug"]
    .with_fill_defaults(true)        // missing "visibility" → "pub"
    .with_drop_unknown_fields(true); // "note": "hope this helps!" → removed

let json = r#"{"type": "AddDerive", "target": "User", "derives": "Debg", "note": "hope this helps!"}"#;
let result = repair_tagged_enum_json(json, &schema, &options)?;

assert_eq!(result.repaired["derives"][0], "Debug");     // wrapped + fuzzy-fixed
assert_eq!(result.repaired["visibility"], "pub");       // filled (in result.filled)
assert!(result.repaired.get("note").is_none());         // dropped (in result.dropped)
```

Defaults also flow in from JSON Schema / schemars: the `default` keyword on
a property (e.g. `#[serde(default)]` + `#[schemars(default = ...)]`) is
carried into the imported repair schema.

### Inspecting Corrections (and Skipped Corrections)

Every applied change is recorded; renames that were *skipped* for collision
safety (the target key already existed) are recorded too.

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

for skipped in &result.skipped {
    println!(
        "  skipped: {} → {} ({:?})",
        skipped.original, skipped.candidate, skipped.reason
    );
}
```

## Algorithms

| Algorithm | Characteristics | Best For |
|-----------|-----------------|----------|
| **Jaro-Winkler** (default) | Prefix-weighted, handles transpositions | General typo correction |
| Levenshtein | Equal cost for insert/delete/substitute | Edit distance based |
| Damerau-Levenshtein | Levenshtein + transposition support | Transposition-heavy typos |

## Design Principles

1. **Three-stage processing**: Extraction, syntax repair (sanitize), and typo
   repair (repair) are independent stages
2. **Schema-driven**: Caller defines the schema (library remains generic)
3. **Transparency**: All corrections are recorded as `Correction` structs;
   collision-skipped renames as `SkippedCorrection`, filled defaults as
   `FilledDefault`, dropped fields as `DroppedField` (removed values are
   preserved in the log), and duplicate input keys as `DuplicateKey`
   (`serde_json` keeps the last occurrence — the loss is made visible)
4. **Safety**: No corrections made below similarity threshold; type coercion
   is lossless-only; lossy-leaning repairs (wrap / fill / drop) are opt-in
   and off by default

## Migrating from 0.3

Most 0.3 code compiles unchanged. The differences:

- **Collision policy changed from first-win to best-match-win**: when two
  typo keys resolve to the same field, the higher-similarity key now wins
  the rename (deterministic, independent of key order). Literal existing
  keys still always win.
- `FuzzyOptions`, `FieldDef`, `RepairLog`, and `RepairResult` gained new
  public fields (`coerce_types`, `wrap_single_values`, `fill_defaults`,
  `drop_unknown_fields` / `default` / `filled`, `dropped`). Struct-literal
  construction needs the new fields; builder and constructor code is
  unaffected.

## Migrating from 0.2

`FieldKind` gained `TaggedEnum` / `TaggedEnumArray` variants and is now
`#[non_exhaustive]`: external `match` expressions over `FieldKind` need a
wildcard arm. Everything else is unchanged.

## Migrating from 0.1

Most 0.1 code compiles unchanged. The differences:

- `TaggedEnumSchema<F>` no longer has a generic parameter — the field-resolver
  closure passed to `new` is evaluated at construction time. Remove the type
  parameter from any explicit annotations.
- Schema structs now own their strings (`String` instead of
  `&'static str`); direct field access and literal struct construction need
  updating. The constructors and builder methods accept the same arguments as
  before.
- Low-level repair functions (`repair_tagged_enum`, `repair_object_fields`,
  `repair_fields_with_list`, `repair_enum_array`, `repair_tagged_enum_array`)
  return `RepairLog` (with `corrections` and `skipped`) instead of
  `Vec<Correction>`.
- `RepairResult` gained a `skipped` field.

## Known Limitations

`sanitize_json` is a **best-effort** syntax repair pass, not a full JSON5 or
lenient-JSON parser. It targets common LLM mistakes (trailing commas,
missing/mismatched/stray closing delimiters, unclosed strings, single quotes,
unquoted keys, Python-style literals, comments) and leaves the input
otherwise untouched. In particular, the following are **not** repaired:

- Missing commas between elements (`{"a": 1 "b": 2}`) — inserting them
  correctly requires guessing value boundaries, which risks corrupting data
- Unquoted bare-word *values* (`{"a": hello}`)
- String concatenation, NDJSON, JSONP wrappers, and other exotic shapes

If your inputs need broader lenient parsing, general-purpose crates such as
[`llm_json`](https://crates.io/crates/llm_json) cover many of these cases.

## License

MIT OR Apache-2.0
