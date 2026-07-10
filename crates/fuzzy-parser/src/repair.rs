//! Generic JSON repair logic
//!
//! This module provides generic fuzzy repair functions that work with
//! any schema provided by the caller.

use crate::distance::{find_closest, Algorithm};
use crate::error::FuzzyError;
use crate::schema::{ObjectSchema, TaggedEnumSchema};
use serde_json::{Map, Value};

/// Options for fuzzy repair
#[derive(Debug, Clone)]
pub struct FuzzyOptions {
    /// Minimum similarity threshold (0.0 to 1.0)
    ///
    /// Values below this threshold will not be corrected.
    /// Default: 0.7
    pub min_similarity: f64,

    /// Algorithm to use for similarity calculation
    ///
    /// Default: JaroWinkler (best for typos)
    pub algorithm: Algorithm,
}

impl Default for FuzzyOptions {
    fn default() -> Self {
        Self {
            min_similarity: 0.7,
            algorithm: Algorithm::JaroWinkler,
        }
    }
}

impl FuzzyOptions {
    /// Create options with a custom minimum similarity threshold
    pub fn with_min_similarity(mut self, min_similarity: f64) -> Self {
        self.min_similarity = min_similarity;
        self
    }

    /// Create options with a custom algorithm
    pub fn with_algorithm(mut self, algorithm: Algorithm) -> Self {
        self.algorithm = algorithm;
        self
    }
}

/// A single correction made during repair
#[derive(Debug, Clone, PartialEq)]
pub struct Correction {
    /// The original (incorrect) value
    pub original: String,
    /// The corrected value
    pub corrected: String,
    /// Similarity score (0.0 to 1.0)
    pub similarity: f64,
    /// JSON path to the corrected field (e.g., "$.type", "$.target")
    pub field_path: String,
}

impl Correction {
    /// Create a new correction
    pub fn new(original: String, corrected: String, similarity: f64, field_path: String) -> Self {
        Self {
            original,
            corrected,
            similarity,
            field_path,
        }
    }
}

/// Result of a repair operation
#[derive(Debug, Clone)]
pub struct RepairResult {
    /// The repaired JSON value
    pub repaired: Value,
    /// List of corrections made
    pub corrections: Vec<Correction>,
}

impl RepairResult {
    /// Check if any corrections were made
    pub fn has_corrections(&self) -> bool {
        !self.corrections.is_empty()
    }

    /// Get the number of corrections made
    pub fn correction_count(&self) -> usize {
        self.corrections.len()
    }
}

// ============================================================================
// Generic Repair Functions
// ============================================================================

/// Repair field names in a JSON object using an ObjectSchema
///
/// Returns the list of corrections made.
pub fn repair_object_fields(
    obj: &mut Map<String, Value>,
    schema: &ObjectSchema,
    path: &str,
    options: &FuzzyOptions,
) -> Vec<Correction> {
    repair_fields_with_list(obj, schema.valid_fields, path, options)
}

/// Repair field names in a JSON object using a field list
///
/// Returns the list of corrections made.
///
/// # Collision behavior (first-win)
///
/// A key is only renamed when the target field does not already exist in the
/// object. This guards against destroying data: if two typo keys resolve to
/// the same candidate, the first one processed wins and the later key is left
/// unchanged (no correction is recorded for it). The same applies when the
/// candidate is already present as a literal key. Keys are processed in the
/// object's iteration order (for `serde_json::Map` this is sorted key order
/// by default, or insertion order with the `preserve_order` feature).
/// Recording skipped collisions in the result is planned for a future
/// release, as it requires extending the public result types.
pub fn repair_fields_with_list(
    obj: &mut Map<String, Value>,
    valid_fields: &[&str],
    path: &str,
    options: &FuzzyOptions,
) -> Vec<Correction> {
    let mut corrections = Vec::new();

    // Collect keys that need correction
    let keys_to_check: Vec<String> = obj
        .keys()
        .filter(|k| !valid_fields.contains(&k.as_str()))
        .cloned()
        .collect();

    // Process each invalid key
    for key in keys_to_check {
        if let Some(m) = find_closest(
            &key,
            valid_fields.iter().copied(),
            options.min_similarity,
            options.algorithm,
        ) {
            // Only correct if the target field doesn't already exist
            if !obj.contains_key(&m.candidate) {
                if let Some(val) = obj.remove(&key) {
                    corrections.push(Correction::new(
                        key.clone(),
                        m.candidate.clone(),
                        m.similarity,
                        format!("{}.{}", path, key),
                    ));
                    obj.insert(m.candidate, val);
                }
            }
        }
    }

    corrections
}

/// Repair a tagged enum JSON object using a TaggedEnumSchema
///
/// This repairs:
/// 1. The tag field value (e.g., "AddDeriv" -> "AddDerive")
/// 2. The field names based on the tag value
/// 3. Values in enum array fields (e.g., ["Debg"] -> ["Debug"])
/// 4. Field names in nested objects
///
/// Returns the list of corrections made.
///
/// # Collision behavior (first-win)
///
/// Field-name repair never overwrites an existing key: when two typo keys
/// resolve to the same candidate, only the first is renamed and the later one
/// is left as-is without a recorded correction. See
/// [`repair_fields_with_list`] for details.
pub fn repair_tagged_enum<F>(
    obj: &mut Map<String, Value>,
    schema: &TaggedEnumSchema<F>,
    path: &str,
    options: &FuzzyOptions,
) -> Vec<Correction>
where
    F: Fn(&str) -> Option<&'static [&'static str]>,
{
    let mut corrections = Vec::new();

    // Step 1: Repair tag field value
    let tag_value = if let Some(tag_val) = obj.get(schema.tag_field).and_then(|v| v.as_str()) {
        if !schema.is_valid_tag(tag_val) {
            // Try to find closest match
            if let Some(m) = find_closest(
                tag_val,
                schema.valid_tags.iter().copied(),
                options.min_similarity,
                options.algorithm,
            ) {
                corrections.push(Correction::new(
                    tag_val.to_string(),
                    m.candidate.clone(),
                    m.similarity,
                    format!("{}.{}", path, schema.tag_field),
                ));
                obj.insert(
                    schema.tag_field.to_string(),
                    Value::String(m.candidate.clone()),
                );
                m.candidate
            } else {
                tag_val.to_string()
            }
        } else {
            tag_val.to_string()
        }
    } else {
        return corrections; // No tag field, can't repair fields
    };

    // Step 2: Repair field names based on tag value
    if let Some(valid_fields) = schema.get_fields(&tag_value) {
        // Filter out the tag field itself from the check
        let keys_to_check: Vec<String> = obj
            .keys()
            .filter(|k| *k != schema.tag_field && !valid_fields.contains(&k.as_str()))
            .cloned()
            .collect();

        for key in keys_to_check {
            if let Some(m) = find_closest(
                &key,
                valid_fields.iter().copied(),
                options.min_similarity,
                options.algorithm,
            ) {
                if !obj.contains_key(&m.candidate) {
                    if let Some(val) = obj.remove(&key) {
                        corrections.push(Correction::new(
                            key.clone(),
                            m.candidate.clone(),
                            m.similarity,
                            format!("{}.{}", path, key),
                        ));
                        obj.insert(m.candidate, val);
                    }
                }
            }
        }
    }

    // Step 3: Repair enum array values
    for (field_name, valid_values) in &schema.enum_arrays {
        if let Some(Value::Array(arr)) = obj.get_mut(*field_name) {
            let field_path = format!("{}.{}", path, field_name);
            let arr_corrections = repair_enum_array(arr, valid_values, &field_path, options);
            corrections.extend(arr_corrections);
        }
    }

    // Step 4: Repair nested object fields
    for (field_name, valid_fields) in &schema.nested_objects {
        if let Some(Value::Object(nested_obj)) = obj.get_mut(*field_name) {
            let nested_path = format!("{}.{}", path, field_name);
            let nested_corrections =
                repair_fields_with_list(nested_obj, valid_fields, &nested_path, options);
            corrections.extend(nested_corrections);
        }
    }

    corrections
}

/// Repair values in an enum array
///
/// Each string value in the array is fuzzy-matched against `valid_values`.
pub fn repair_enum_array(
    arr: &mut [Value],
    valid_values: &[&str],
    path: &str,
    options: &FuzzyOptions,
) -> Vec<Correction> {
    let mut corrections = Vec::new();

    for (i, item) in arr.iter_mut().enumerate() {
        if let Value::String(s) = item {
            if !valid_values.contains(&s.as_str()) {
                if let Some(m) = find_closest(
                    s,
                    valid_values.iter().copied(),
                    options.min_similarity,
                    options.algorithm,
                ) {
                    corrections.push(Correction::new(
                        s.clone(),
                        m.candidate.clone(),
                        m.similarity,
                        format!("{}[{}]", path, i),
                    ));
                    *item = Value::String(m.candidate);
                }
            }
        }
    }

    corrections
}

/// Repair a tagged enum from JSON string
pub fn repair_tagged_enum_json<F>(
    json: &str,
    schema: &TaggedEnumSchema<F>,
    options: &FuzzyOptions,
) -> Result<RepairResult, FuzzyError>
where
    F: Fn(&str) -> Option<&'static [&'static str]>,
{
    let mut value: Value = serde_json::from_str(json)?;

    let corrections = if let Some(obj) = value.as_object_mut() {
        repair_tagged_enum(obj, schema, "$", options)
    } else {
        return Err(FuzzyError::NotObject);
    };

    Ok(RepairResult {
        repaired: value,
        corrections,
    })
}

/// Repair an array of tagged enums
pub fn repair_tagged_enum_array<F>(
    arr: &mut [Value],
    schema: &TaggedEnumSchema<F>,
    path: &str,
    options: &FuzzyOptions,
) -> Vec<Correction>
where
    F: Fn(&str) -> Option<&'static [&'static str]>,
{
    let mut all_corrections = Vec::new();

    for (i, item) in arr.iter_mut().enumerate() {
        if let Some(obj) = item.as_object_mut() {
            let item_path = format!("{}[{}]", path, i);
            let corrections = repair_tagged_enum(obj, schema, &item_path, options);
            all_corrections.extend(corrections);
        }
    }

    all_corrections
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Field resolver signature used by the test schemas: maps a tag to its
    /// allowed field list. Aliased to keep the `test_schema` return type
    /// readable (and to satisfy `clippy::type_complexity`).
    type FieldResolver = fn(&str) -> Option<&'static [&'static str]>;

    fn test_schema() -> TaggedEnumSchema<FieldResolver> {
        TaggedEnumSchema::new(
            "type",
            &["AddDerive", "RemoveDerive", "RenameIdent"],
            |tag| match tag {
                "AddDerive" | "RemoveDerive" => Some(&["target", "derives"]),
                "RenameIdent" => Some(&["from", "to", "kind"]),
                _ => None,
            },
        )
    }

    #[test]
    fn test_repair_tagged_enum_type_typo() {
        let schema = test_schema();
        let json = r#"{"type": "AddDeriv", "target": "User", "derives": ["Debug"]}"#;
        let options = FuzzyOptions::default();

        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert_eq!(result.repaired["type"], "AddDerive");
        assert_eq!(result.corrections.len(), 1);
        assert_eq!(result.corrections[0].original, "AddDeriv");
        assert_eq!(result.corrections[0].corrected, "AddDerive");
    }

    #[test]
    fn test_repair_tagged_enum_field_typo() {
        let schema = test_schema();
        let json = r#"{"type": "AddDerive", "taget": "User", "derives": ["Debug"]}"#;
        let options = FuzzyOptions::default();

        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert!(result.repaired.get("target").is_some());
        assert!(result.repaired.get("taget").is_none());
        assert_eq!(result.corrections.len(), 1);
    }

    #[test]
    fn test_repair_tagged_enum_multiple_typos() {
        let schema = test_schema();
        let json = r#"{"type": "RenamIdent", "form": "old", "too": "new"}"#;
        let options = FuzzyOptions::default();

        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert_eq!(result.repaired["type"], "RenameIdent");
        assert!(result.repaired.get("from").is_some());
        assert!(result.repaired.get("to").is_some());
        assert_eq!(result.corrections.len(), 3);
    }

    #[test]
    fn test_repair_object_fields() {
        let schema = ObjectSchema::new(&["name", "module", "derives"]);
        let mut obj: Map<String, Value> =
            serde_json::from_str(r#"{"nam": "Test", "modul": "foo"}"#).unwrap();
        let options = FuzzyOptions::default();

        let corrections = repair_object_fields(&mut obj, &schema, "$", &options);

        assert!(obj.contains_key("name"));
        assert!(obj.contains_key("module"));
        assert_eq!(corrections.len(), 2);
    }

    #[test]
    fn test_no_correction_needed() {
        let schema = test_schema();
        let json = r#"{"type": "AddDerive", "target": "User", "derives": ["Debug"]}"#;
        let options = FuzzyOptions::default();

        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert!(!result.has_corrections());
    }

    #[test]
    fn test_high_similarity_threshold() {
        let schema = test_schema();
        let json = r#"{"type": "AddDeriv", "target": "User", "derives": ["Debug"]}"#;
        let options = FuzzyOptions::default().with_min_similarity(0.99);

        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        // With very high threshold, typo should not be corrected
        assert_eq!(result.repaired["type"], "AddDeriv");
        assert!(!result.has_corrections());
    }

    #[test]
    fn test_repair_array() {
        let schema = test_schema();
        let mut arr: Vec<Value> = serde_json::from_str(
            r#"[
                {"type": "AddDeriv", "taget": "User", "derives": ["Debug"]},
                {"type": "RenamIdent", "form": "old", "too": "new"}
            ]"#,
        )
        .unwrap();
        let options = FuzzyOptions::default();

        let corrections = repair_tagged_enum_array(&mut arr, &schema, "$.intents", &options);

        assert_eq!(arr[0]["type"], "AddDerive");
        assert!(arr[0].get("target").is_some());
        assert_eq!(arr[1]["type"], "RenameIdent");
        assert!(arr[1].get("from").is_some());
        assert!(corrections.len() >= 4);
    }

    #[test]
    fn test_repair_enum_array_values() {
        let schema =
            TaggedEnumSchema::new("type", &["AddDerive"], |_| Some(&["target", "derives"][..]))
                .with_enum_array("derives", &["Debug", "Clone", "Serialize", "Default"]);

        let json =
            r#"{"type": "AddDerive", "target": "User", "derives": ["Debg", "Clne", "Serializ"]}"#;
        let options = FuzzyOptions::default();

        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert_eq!(result.repaired["derives"][0], "Debug");
        assert_eq!(result.repaired["derives"][1], "Clone");
        assert_eq!(result.repaired["derives"][2], "Serialize");
        assert_eq!(result.corrections.len(), 3);
    }

    #[test]
    fn test_repair_nested_object_fields() {
        let schema =
            TaggedEnumSchema::new("type", &["Configure"], |_| Some(&["name", "config"][..]))
                .with_nested_object("config", &["timeout", "retries", "enabled"]);

        let json =
            r#"{"type": "Configure", "name": "test", "config": {"timout": 30, "retres": 3}}"#;
        let options = FuzzyOptions::default();

        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert!(result.repaired["config"].get("timeout").is_some());
        assert!(result.repaired["config"].get("retries").is_some());
        assert_eq!(result.repaired["config"]["timeout"], 30);
        assert_eq!(result.repaired["config"]["retries"], 3);
        assert_eq!(result.corrections.len(), 2);
    }

    #[test]
    fn test_repair_combined_all_features() {
        let schema = TaggedEnumSchema::new("type", &["AddDerive"], |_| {
            Some(&["target", "derives", "config"][..])
        })
        .with_enum_array("derives", &["Debug", "Clone", "Serialize"])
        .with_nested_object("config", &["timeout", "retries"]);

        let json = r#"{
            "type": "AddDeriv",
            "taget": "User",
            "derives": ["Debg", "Clne"],
            "config": {"timout": 30}
        }"#;
        let options = FuzzyOptions::default();

        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        // Tag value repaired
        assert_eq!(result.repaired["type"], "AddDerive");
        // Field name repaired
        assert!(result.repaired.get("target").is_some());
        assert_eq!(result.repaired["target"], "User");
        // Enum array values repaired
        assert_eq!(result.repaired["derives"][0], "Debug");
        assert_eq!(result.repaired["derives"][1], "Clone");
        // Nested object field repaired
        assert!(result.repaired["config"].get("timeout").is_some());
        assert_eq!(result.repaired["config"]["timeout"], 30);
        // Total corrections: type + target + 2 derives + timeout = 5
        assert_eq!(result.corrections.len(), 5);
    }

    #[test]
    fn test_collision_first_win_two_typos_same_candidate() {
        // Two typo keys ("taget", "targt") both resolve to "target".
        // First-win: the first key processed is renamed, the second is left
        // unchanged and no correction is recorded for it. This test pins the
        // current behavior; recording skipped collisions requires extending
        // the public result types and is deferred to a future release.
        let json = r#"{"type": "AddDerive", "taget": "User", "targt": "Post"}"#;
        let schema = test_schema();
        let options = FuzzyOptions::default();

        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        // Exactly one key won the rename; the other survives verbatim.
        assert_eq!(result.repaired["target"], "User");
        assert_eq!(result.repaired["targt"], "Post");
        assert!(result.repaired.get("taget").is_none());
        assert_eq!(result.corrections.len(), 1);
        assert_eq!(result.corrections[0].original, "taget");
        assert_eq!(result.corrections[0].corrected, "target");
    }

    #[test]
    fn test_collision_first_win_existing_key_preserved() {
        // The candidate already exists as a literal key: the typo key is NOT
        // renamed onto it (no data loss), and no correction is recorded.
        let json = r#"{"type": "AddDerive", "target": "User", "taget": "Post"}"#;
        let schema = test_schema();
        let options = FuzzyOptions::default();

        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert_eq!(result.repaired["target"], "User");
        assert_eq!(result.repaired["taget"], "Post");
        assert!(!result.has_corrections());
    }

    #[test]
    fn test_repair_enum_array_no_correction_needed() {
        let schema =
            TaggedEnumSchema::new("type", &["AddDerive"], |_| Some(&["target", "derives"][..]))
                .with_enum_array("derives", &["Debug", "Clone"]);

        let json = r#"{"type": "AddDerive", "target": "User", "derives": ["Debug", "Clone"]}"#;
        let options = FuzzyOptions::default();

        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert!(!result.has_corrections());
    }
}
