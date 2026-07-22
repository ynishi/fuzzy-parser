//! End-to-end integration tests: the public API exercised the way a real
//! LLM-integration caller uses it (extract → sanitize → repair), including
//! the opt-in 0.4 options.

use fuzzy_parser::{
    extract_json, extract_json_blocks, repair_tagged_enum_json, sanitize_json, FieldKind,
    FuzzyOptions, ObjectSchema, TaggedEnumSchema,
};
use serde_json::json;

fn intent_schema() -> TaggedEnumSchema {
    TaggedEnumSchema::new(
        "type",
        &["AddDerive", "RemoveDerive", "Rename"],
        |tag| match tag {
            "AddDerive" | "RemoveDerive" => Some(&["target", "derives"][..]),
            "Rename" => Some(&["from", "to"][..]),
            _ => None,
        },
    )
    .with_enum_array("derives", ["Debug", "Clone", "Serialize", "Default"])
}

#[test]
fn full_pipeline_prose_fence_typos_truncation() {
    let raw = "Sure! Here's the edit you asked for:\n\n```json\n{\"type\": \"AddDeriv\", \"taget\": \"User\", \"derives\": [\"Debg\", \"Clne\",]\n```\n\nLet me know if you need more.";

    let payload = extract_json(raw).expect("payload should be found");
    let sanitized = sanitize_json(payload);
    let result =
        repair_tagged_enum_json(&sanitized, &intent_schema(), &FuzzyOptions::default()).unwrap();

    assert_eq!(result.repaired["type"], "AddDerive");
    assert_eq!(result.repaired["target"], "User");
    assert_eq!(result.repaired["derives"], json!(["Debug", "Clone"]));
}

#[test]
fn full_pipeline_multiple_blocks_picks_parseable() {
    let raw = r#"Two candidates: {"type": "Rename", "from": "a", "to": "b"} and {"broken": "#;

    let blocks = extract_json_blocks(raw);
    assert!(!blocks.is_empty());

    let payload = extract_json(raw).expect("payload should be found");
    let result =
        repair_tagged_enum_json(payload, &intent_schema(), &FuzzyOptions::default()).unwrap();
    assert_eq!(result.repaired["type"], "Rename");
}

#[test]
fn full_pipeline_with_all_opt_in_options() {
    // A messy but realistic LLM answer: single value where an array is
    // expected, a chatty extra field, and a missing defaulted field.
    let raw = r#"```json
{"type": "AddDeriv", "taget": "User", "derives": "Debg", "note": "hope this helps!"}
```"#;

    let schema = TaggedEnumSchema::new("type", &["AddDerive"], |_| {
        Some(&["target", "derives", "visibility"][..])
    })
    .with_enum_array("derives", ["Debug", "Clone"])
    .with_field_default("visibility", json!("pub"));

    let options = FuzzyOptions::default()
        .with_wrap_single_values(true)
        .with_fill_defaults(true)
        .with_drop_unknown_fields(true);

    let payload = extract_json(raw).unwrap();
    let sanitized = sanitize_json(payload);
    let result = repair_tagged_enum_json(&sanitized, &schema, &options).unwrap();

    assert_eq!(result.repaired["type"], "AddDerive");
    assert_eq!(result.repaired["target"], "User");
    assert_eq!(result.repaired["derives"], json!(["Debug"]));
    assert_eq!(result.repaired["visibility"], "pub");
    assert!(result.repaired.get("note").is_none());

    // Everything is accounted for in the log
    assert_eq!(result.filled.len(), 1);
    assert_eq!(result.dropped.len(), 1);
    assert_eq!(result.dropped[0].value, "hope this helps!");
    assert!(result.corrections.len() >= 3); // tag + field + wrap + enum fix
}

#[test]
fn full_pipeline_lenient_syntax() {
    // Python-ish LLM answer: single quotes, unquoted keys, True, comment
    let raw = "```json\n{type: 'AddDeriv', target: 'User', // fixed\n derives: ['Debg'], enabled: True}\n```";

    let payload = extract_json(raw).expect("payload should be found");
    let sanitized = sanitize_json(payload);
    let schema = TaggedEnumSchema::new("type", &["AddDerive"], |_| {
        Some(&["target", "derives", "enabled"][..])
    })
    .with_enum_array("derives", ["Debug", "Clone"]);

    let result = repair_tagged_enum_json(&sanitized, &schema, &FuzzyOptions::default()).unwrap();

    assert_eq!(result.repaired["type"], "AddDerive");
    assert_eq!(result.repaired["target"], "User");
    assert_eq!(result.repaired["derives"], json!(["Debug"]));
    assert_eq!(result.repaired["enabled"], true);
}

#[test]
fn json_schema_import_defaults_flow() {
    let schema_doc = json!({
        "oneOf": [
            {
                "type": "object",
                "properties": {
                    "type": {"type": "string", "const": "Configure"},
                    "name": {"type": "string"},
                    "retries": {"type": "integer", "default": 3},
                    "level": {"type": "string", "enum": ["debug", "info"], "default": "info"}
                }
            }
        ]
    });

    let import = TaggedEnumSchema::from_json_schema(&schema_doc).unwrap();
    assert!(import.warnings.is_empty());

    let options = FuzzyOptions::default().with_fill_defaults(true);
    let result = repair_tagged_enum_json(
        r#"{"type": "Configure", "name": "api"}"#,
        &import.schema,
        &options,
    )
    .unwrap();

    assert_eq!(result.repaired["retries"], 3);
    assert_eq!(result.repaired["level"], "info");
    assert_eq!(result.filled.len(), 2);
}

#[test]
fn strict_schema_output_mode() {
    // drop_unknown_fields + fill_defaults ≈ "the output always has exactly
    // the schema's shape" — the mode a downstream serde deserialize wants.
    let schema = TaggedEnumSchema::with_tag("type").with_variant(
        "Configure",
        ObjectSchema::new(["name"])
            .with_field_kind("timeout", FieldKind::Integer)
            .with_field_default("timeout", json!(30)),
    );

    let options = FuzzyOptions::default()
        .with_fill_defaults(true)
        .with_drop_unknown_fields(true);

    let result = repair_tagged_enum_json(
        r#"{"type": "Configure", "name": "api", "chatter": "yes", "timout": "15"}"#,
        &schema,
        &options,
    )
    .unwrap();

    // "timout" repaired then coerced; "chatter" dropped; nothing filled
    // (timeout became present via the rename).
    assert_eq!(result.repaired["timeout"], 15);
    assert!(result.repaired.get("chatter").is_none());
    assert!(!result.has_filled());

    let keys: Vec<&str> = result
        .repaired
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys.len(), 3); // type, name, timeout — nothing else
}

#[test]
fn collision_winner_is_deterministic_across_key_orders() {
    let schema = intent_schema();
    let options = FuzzyOptions::default();

    let a = repair_tagged_enum_json(
        r#"{"type": "AddDerive", "taget": "A", "targt": "B"}"#,
        &schema,
        &options,
    )
    .unwrap();
    let b = repair_tagged_enum_json(
        r#"{"type": "AddDerive", "targt": "B", "taget": "A"}"#,
        &schema,
        &options,
    )
    .unwrap();

    assert_eq!(a.repaired["target"], b.repaired["target"]);
    assert_eq!(a.skipped[0].original, b.skipped[0].original);
}
