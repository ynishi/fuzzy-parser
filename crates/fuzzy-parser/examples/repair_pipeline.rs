//! Full repair pipeline demo: extract → sanitize → repair.
//!
//! Feeds a realistic raw LLM response (prose + code fence + syntax errors +
//! typos + string-encoded numbers) through all three stages and prints every
//! correction that was applied.
//!
//! Run with: `cargo run --example repair_pipeline`

use fuzzy_parser::{
    extract_json, repair_tagged_enum_json, sanitize_json, FieldKind, FuzzyOptions, ObjectSchema,
    TaggedEnumSchema,
};

fn main() {
    // A raw LLM response: prose around a fenced payload, a trailing comma,
    // a tag typo, field typos at two nesting levels, and a stringified int.
    let raw = r#"Sure! Here is the change you asked for:

```json
{
    "type": "AddDeriv",
    "taget": "User",
    "derives": ["Debg", "Clne"],
    "config": {"timout": "30",}
}
```

Let me know if you need anything else."#;

    // The schema the application expects (built with the fluent API; see the
    // schema_import example for deriving this from a JSON Schema instead).
    let schema = TaggedEnumSchema::with_tag("type").with_variant(
        "AddDerive",
        ObjectSchema::new(["target"])
            .with_field_kind(
                "derives",
                FieldKind::enum_array(["Debug", "Clone", "Serialize", "Default"]),
            )
            .with_field_kind(
                "config",
                FieldKind::Object(
                    ObjectSchema::empty().with_field_kind("timeout", FieldKind::Integer),
                ),
            ),
    );

    // Stage 0: pull the JSON payload out of the prose and code fence
    let payload = extract_json(raw).expect("no JSON payload found");
    println!("--- extracted ---\n{payload}\n");

    // Stage 1: fix syntax errors (trailing comma)
    let sanitized = sanitize_json(payload);
    println!("--- sanitized ---\n{sanitized}\n");

    // Stage 2: fix typos and coerce types, guided by the schema
    let result = repair_tagged_enum_json(&sanitized, &schema, &FuzzyOptions::default())
        .expect("repair failed");

    println!("--- repaired ---");
    println!(
        "{}\n",
        serde_json::to_string_pretty(&result.repaired).unwrap()
    );

    println!("--- corrections ({}) ---", result.correction_count());
    for c in &result.corrections {
        println!(
            "  {} -> {} (similarity {:.2}, at {})",
            c.original, c.corrected, c.similarity, c.field_path
        );
    }
    for s in &result.skipped {
        println!(
            "  skipped: {} -> {} ({:?}, at {})",
            s.original, s.candidate, s.reason, s.field_path
        );
    }
}
