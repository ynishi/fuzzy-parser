//! JSON Schema import demo: derive the repair schema from a JSON Schema
//! document instead of hand-building it.
//!
//! The document below is the shape `schemars` emits for an internally
//! tagged enum (`#[serde(tag = "type")]`), but any JSON Schema source
//! (Pydantic, OpenAPI components, hand-written files) works the same way.
//!
//! Run with: `cargo run --example schema_import`

use fuzzy_parser::{repair_tagged_enum_json, FuzzyOptions, TaggedEnumSchema};
use serde_json::json;

fn main() {
    let json_schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Intent",
        "oneOf": [
            {
                "type": "object",
                "properties": {
                    "type": {"type": "string", "const": "AddDerive"},
                    "target": {"type": "string"},
                    "derives": {
                        "type": "array",
                        "items": {"type": "string", "enum": ["Debug", "Clone", "Serialize"]}
                    },
                    "count": {"type": "integer"}
                },
                "required": ["type", "target"]
            },
            {
                "type": "object",
                "properties": {
                    "type": {"type": "string", "const": "Rename"},
                    "from": {"type": "string"},
                    "to": {"type": "string"}
                },
                "required": ["type", "from", "to"]
            }
        ]
    });

    let import = TaggedEnumSchema::from_json_schema(&json_schema).expect("import failed");
    for w in &import.warnings {
        println!("import warning at {}: {}", w.path, w.detail);
    }

    // Tag typo, field typo, enum-array typo, and a stringified integer —
    // all repaired using only the imported schema.
    let llm_json = r#"{"type": "AddDeriv", "taget": "User", "derives": ["Debg"], "count": "3"}"#;
    let result = repair_tagged_enum_json(llm_json, &import.schema, &FuzzyOptions::default())
        .expect("repair failed");

    println!("repaired: {}", result.repaired);
    println!("corrections:");
    for c in &result.corrections {
        println!(
            "  {} -> {} (similarity {:.2}, at {})",
            c.original, c.corrected, c.similarity, c.field_path
        );
    }
}
