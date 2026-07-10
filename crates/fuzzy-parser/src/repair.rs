//! Generic JSON repair logic
//!
//! This module provides generic fuzzy repair functions that work with
//! any schema provided by the caller.
//!
//! # Architecture
//!
//! Repair walks the JSON value tree guided by the schema:
//!
//! 1. **Field-name repair** — object keys are fuzzy-matched against the
//!    schema's field names and renamed (collision-safe, first-win; skipped
//!    renames are recorded as [`SkippedCorrection`]s).
//! 2. **Value repair** — each field's value is repaired according to its
//!    [`FieldKind`]: fuzzy correction against closed sets, recursive descent
//!    into nested objects / arrays of objects, or scalar type coercion.
//!
//! Every change is recorded as a [`Correction`]; every rename that was
//! *not* applied because the target key already existed is recorded as a
//! [`SkippedCorrection`]. Nothing is changed silently.

use crate::distance::{find_closest, Algorithm};
use crate::error::FuzzyError;
use crate::schema::{FieldDef, FieldKind, ObjectSchema, TaggedEnumSchema};
use serde_json::{Map, Number, Value};

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
    /// Similarity score (0.0 to 1.0); `1.0` for type coercions
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

/// Why a candidate correction was not applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The rename target key already exists in the object, so renaming
    /// would have overwritten data (first-win collision policy).
    TargetExists,
}

/// A correction that was found but *not* applied.
///
/// Recorded when a typo key resolves to a candidate field name but the
/// candidate already exists in the object (either as a literal key or
/// because an earlier typo key won the rename). See
/// [`repair_fields_with_list`] for the collision policy.
#[derive(Debug, Clone, PartialEq)]
pub struct SkippedCorrection {
    /// The original key that was left unchanged
    pub original: String,
    /// The candidate it would have been renamed to
    pub candidate: String,
    /// Similarity score (0.0 to 1.0)
    pub similarity: f64,
    /// JSON path to the field (e.g., "$.targt")
    pub field_path: String,
    /// Why the correction was skipped
    pub reason: SkipReason,
}

/// Accumulated log of a repair pass: applied and skipped corrections.
#[derive(Debug, Clone, Default)]
pub struct RepairLog {
    /// Corrections that were applied
    pub corrections: Vec<Correction>,
    /// Corrections that were found but skipped (collision safety)
    pub skipped: Vec<SkippedCorrection>,
}

impl RepairLog {
    /// Check if any corrections were applied
    pub fn has_corrections(&self) -> bool {
        !self.corrections.is_empty()
    }

    /// Get the number of corrections applied
    pub fn correction_count(&self) -> usize {
        self.corrections.len()
    }

    /// Check if any corrections were skipped
    pub fn has_skipped(&self) -> bool {
        !self.skipped.is_empty()
    }

    /// Merge another log into this one
    pub fn merge(&mut self, other: RepairLog) {
        self.corrections.extend(other.corrections);
        self.skipped.extend(other.skipped);
    }
}

/// Result of a repair operation
#[derive(Debug, Clone)]
pub struct RepairResult {
    /// The repaired JSON value
    pub repaired: Value,
    /// List of corrections made
    pub corrections: Vec<Correction>,
    /// List of corrections that were found but skipped (collision safety)
    pub skipped: Vec<SkippedCorrection>,
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

    /// Check if any corrections were skipped
    pub fn has_skipped(&self) -> bool {
        !self.skipped.is_empty()
    }
}

// ============================================================================
// Generic Repair Functions
// ============================================================================

/// Repair a JSON object using an [`ObjectSchema`]
///
/// This repairs:
/// 1. Field names (fuzzy-matched against the schema's field names)
/// 2. Field values according to each field's [`FieldKind`] — recursing
///    into nested objects and arrays of objects to any depth
///
/// # Collision behavior (first-win)
///
/// See [`repair_fields_with_list`]. Skipped renames are recorded in the
/// returned log's `skipped` list.
pub fn repair_object_fields(
    obj: &mut Map<String, Value>,
    schema: &ObjectSchema,
    path: &str,
    options: &FuzzyOptions,
) -> RepairLog {
    let mut log = RepairLog::default();
    let valid_fields: Vec<&str> = schema.field_names().collect();
    repair_field_names_into(obj, &valid_fields, None, path, options, &mut log);
    apply_field_kinds(obj, &schema.fields, path, options, &mut log);
    log
}

/// Repair field names in a JSON object using a field list
///
/// # Collision behavior (first-win)
///
/// A key is only renamed when the target field does not already exist in the
/// object. This guards against destroying data: if two typo keys resolve to
/// the same candidate, the first one processed wins and the later key is left
/// unchanged (recorded as a [`SkippedCorrection`] with
/// [`SkipReason::TargetExists`]). The same applies when the candidate is
/// already present as a literal key. Keys are processed in the object's
/// iteration order (for `serde_json::Map` this is sorted key order by
/// default, or insertion order with the `preserve_order` feature).
pub fn repair_fields_with_list(
    obj: &mut Map<String, Value>,
    valid_fields: &[&str],
    path: &str,
    options: &FuzzyOptions,
) -> RepairLog {
    let mut log = RepairLog::default();
    repair_field_names_into(obj, valid_fields, None, path, options, &mut log);
    log
}

/// Core field-name rename pass (collision-safe, first-win).
fn repair_field_names_into(
    obj: &mut Map<String, Value>,
    valid_fields: &[&str],
    skip_key: Option<&str>,
    path: &str,
    options: &FuzzyOptions,
    log: &mut RepairLog,
) {
    // Collect keys that need correction
    let keys_to_check: Vec<String> = obj
        .keys()
        .filter(|k| Some(k.as_str()) != skip_key && !valid_fields.contains(&k.as_str()))
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
                    log.corrections.push(Correction::new(
                        key.clone(),
                        m.candidate.clone(),
                        m.similarity,
                        format!("{}.{}", path, key),
                    ));
                    obj.insert(m.candidate, val);
                }
            } else {
                log.skipped.push(SkippedCorrection {
                    original: key.clone(),
                    candidate: m.candidate,
                    similarity: m.similarity,
                    field_path: format!("{}.{}", path, key),
                    reason: SkipReason::TargetExists,
                });
            }
        }
    }
}

/// Apply each defined field's [`FieldKind`] to the object's values.
fn apply_field_kinds(
    obj: &mut Map<String, Value>,
    fields: &[FieldDef],
    path: &str,
    options: &FuzzyOptions,
    log: &mut RepairLog,
) {
    for def in fields {
        if let Some(value) = obj.get_mut(&def.name) {
            let field_path = format!("{}.{}", path, def.name);
            apply_kind(value, &def.kind, &field_path, options, log);
        }
    }
}

/// Apply a single [`FieldKind`] to a value (recursive entry point).
fn apply_kind(
    value: &mut Value,
    kind: &FieldKind,
    path: &str,
    options: &FuzzyOptions,
    log: &mut RepairLog,
) {
    match kind {
        FieldKind::Any => {}
        FieldKind::Enum(valid_values) => {
            if let Value::String(s) = value {
                if !valid_values.iter().any(|v| v == s) {
                    if let Some(m) = find_closest(
                        s,
                        valid_values.iter().map(|v| v.as_str()),
                        options.min_similarity,
                        options.algorithm,
                    ) {
                        log.corrections.push(Correction::new(
                            s.clone(),
                            m.candidate.clone(),
                            m.similarity,
                            path.to_string(),
                        ));
                        *value = Value::String(m.candidate);
                    }
                }
            }
        }
        FieldKind::EnumArray(valid_values) => {
            if let Value::Array(arr) = value {
                let valid: Vec<&str> = valid_values.iter().map(|v| v.as_str()).collect();
                let arr_log = repair_enum_array(arr, &valid, path, options);
                log.merge(arr_log);
            }
        }
        FieldKind::Object(schema) => {
            if let Value::Object(nested) = value {
                let nested_log = repair_object_fields(nested, schema, path, options);
                log.merge(nested_log);
            }
        }
        FieldKind::ObjectArray(schema) => {
            if let Value::Array(arr) = value {
                for (i, item) in arr.iter_mut().enumerate() {
                    if let Value::Object(nested) = item {
                        let item_path = format!("{}[{}]", path, i);
                        let nested_log = repair_object_fields(nested, schema, &item_path, options);
                        log.merge(nested_log);
                    }
                }
            }
        }
        FieldKind::Integer | FieldKind::Number | FieldKind::Bool | FieldKind::String => {
            coerce_value(value, kind, path, log);
        }
    }
}

/// Coerce a scalar value toward the expected type (lossless only).
///
/// Unparseable or already-correct values are left untouched.
fn coerce_value(value: &mut Value, kind: &FieldKind, path: &str, log: &mut RepairLog) {
    let new_value = match (kind, &*value) {
        (FieldKind::Integer, Value::String(s)) => s
            .trim()
            .parse::<i64>()
            .ok()
            .map(|n| Value::Number(n.into())),
        (FieldKind::Number, Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(Number::from_f64)
            .map(Value::Number),
        (FieldKind::Bool, Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },
        (FieldKind::String, Value::Number(n)) => Some(Value::String(n.to_string())),
        (FieldKind::String, Value::Bool(b)) => Some(Value::String(b.to_string())),
        _ => None,
    };

    if let Some(new_value) = new_value {
        log.corrections.push(Correction::new(
            value.to_string(),
            new_value.to_string(),
            1.0,
            path.to_string(),
        ));
        *value = new_value;
    }
}

/// Repair a tagged enum JSON object using a TaggedEnumSchema
///
/// This repairs:
/// 1. The tag field value (e.g., "AddDeriv" -> "AddDerive")
/// 2. The field names based on the tag value (variant fields + global fields)
/// 3. Field values according to their [`FieldKind`] — enum arrays, nested
///    objects (recursively, to any depth), arrays of objects, and scalar
///    type coercion
///
/// # Collision behavior (first-win)
///
/// Field-name repair never overwrites an existing key: when two typo keys
/// resolve to the same candidate, only the first is renamed and the later one
/// is recorded as a [`SkippedCorrection`]. See [`repair_fields_with_list`]
/// for details.
pub fn repair_tagged_enum(
    obj: &mut Map<String, Value>,
    schema: &TaggedEnumSchema,
    path: &str,
    options: &FuzzyOptions,
) -> RepairLog {
    let mut log = RepairLog::default();

    // Step 1: Repair tag field value
    let tag_value = if let Some(tag_val) = obj.get(&schema.tag_field).and_then(|v| v.as_str()) {
        if !schema.is_valid_tag(tag_val) {
            // Try to find closest match
            if let Some(m) = find_closest(
                tag_val,
                schema.tag_values(),
                options.min_similarity,
                options.algorithm,
            ) {
                log.corrections.push(Correction::new(
                    tag_val.to_string(),
                    m.candidate.clone(),
                    m.similarity,
                    format!("{}.{}", path, schema.tag_field),
                ));
                obj.insert(
                    schema.tag_field.clone(),
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
        return log; // No tag field, can't repair fields
    };

    // Step 2: Repair field names (variant fields + global fields)
    let variant = schema.variant_schema(&tag_value);
    let mut valid_fields: Vec<&str> = variant.map(|s| s.field_names().collect()).unwrap_or_default();
    for def in &schema.global_fields {
        if !valid_fields.contains(&def.name.as_str()) {
            valid_fields.push(def.name.as_str());
        }
    }
    if !valid_fields.is_empty() {
        repair_field_names_into(
            obj,
            &valid_fields,
            Some(schema.tag_field.as_str()),
            path,
            options,
            &mut log,
        );
    }

    // Step 3: Apply variant field kinds (recursive)
    if let Some(variant) = variant {
        apply_field_kinds(obj, &variant.fields, path, options, &mut log);
    }

    // Step 4: Apply global field kinds (enum arrays, nested objects, coercion)
    apply_field_kinds(obj, &schema.global_fields, path, options, &mut log);

    log
}

/// Repair values in an enum array
///
/// Each string value in the array is fuzzy-matched against `valid_values`.
pub fn repair_enum_array(
    arr: &mut [Value],
    valid_values: &[&str],
    path: &str,
    options: &FuzzyOptions,
) -> RepairLog {
    let mut log = RepairLog::default();

    for (i, item) in arr.iter_mut().enumerate() {
        if let Value::String(s) = item {
            if !valid_values.contains(&s.as_str()) {
                if let Some(m) = find_closest(
                    s,
                    valid_values.iter().copied(),
                    options.min_similarity,
                    options.algorithm,
                ) {
                    log.corrections.push(Correction::new(
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

    log
}

/// Repair a tagged enum from JSON string
pub fn repair_tagged_enum_json(
    json: &str,
    schema: &TaggedEnumSchema,
    options: &FuzzyOptions,
) -> Result<RepairResult, FuzzyError> {
    let mut value: Value = serde_json::from_str(json)?;

    let log = if let Some(obj) = value.as_object_mut() {
        repair_tagged_enum(obj, schema, "$", options)
    } else {
        return Err(FuzzyError::NotObject);
    };

    Ok(RepairResult {
        repaired: value,
        corrections: log.corrections,
        skipped: log.skipped,
    })
}

/// Repair an array of tagged enums
pub fn repair_tagged_enum_array(
    arr: &mut [Value],
    schema: &TaggedEnumSchema,
    path: &str,
    options: &FuzzyOptions,
) -> RepairLog {
    let mut log = RepairLog::default();

    for (i, item) in arr.iter_mut().enumerate() {
        if let Some(obj) = item.as_object_mut() {
            let item_path = format!("{}[{}]", path, i);
            let item_log = repair_tagged_enum(obj, schema, &item_path, options);
            log.merge(item_log);
        }
    }

    log
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{FieldKind, ObjectSchema};

    /// Pins the 0.1 call style (borrowed `'static` slices) so it keeps
    /// compiling against the 0.2 owned-schema API.
    #[test]
    #[allow(clippy::needless_borrows_for_generic_args)]
    fn test_v01_call_style_still_compiles() {
        let schema = TaggedEnumSchema::new("type", &["AddDerive"], |_| {
            Some(&["target", "derives"][..])
        })
        .with_enum_array("derives", &["Debug", "Clone"])
        .with_nested_object("config", &["timeout"]);
        let object_schema = ObjectSchema::new(&["name", "value"]);

        assert!(schema.is_valid_tag("AddDerive"));
        assert!(object_schema.is_valid_field("name"));
    }

    fn test_schema() -> TaggedEnumSchema {
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
        let schema = ObjectSchema::new(["name", "module", "derives"]);
        let mut obj: Map<String, Value> =
            serde_json::from_str(r#"{"nam": "Test", "modul": "foo"}"#).unwrap();
        let options = FuzzyOptions::default();

        let log = repair_object_fields(&mut obj, &schema, "$", &options);

        assert!(obj.contains_key("name"));
        assert!(obj.contains_key("module"));
        assert_eq!(log.correction_count(), 2);
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

        let log = repair_tagged_enum_array(&mut arr, &schema, "$.intents", &options);

        assert_eq!(arr[0]["type"], "AddDerive");
        assert!(arr[0].get("target").is_some());
        assert_eq!(arr[1]["type"], "RenameIdent");
        assert!(arr[1].get("from").is_some());
        assert!(log.correction_count() >= 4);
    }

    #[test]
    fn test_repair_enum_array_values() {
        let schema =
            TaggedEnumSchema::new("type", &["AddDerive"], |_| Some(&["target", "derives"][..]))
                .with_enum_array("derives", ["Debug", "Clone", "Serialize", "Default"]);

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
                .with_nested_object("config", ["timeout", "retries", "enabled"]);

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
        .with_enum_array("derives", ["Debug", "Clone", "Serialize"])
        .with_nested_object("config", ["timeout", "retries"]);

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
    fn test_collision_skip_is_recorded() {
        // Two typo keys ("taget", "targt") both resolve to "target".
        // First-win: the first key processed is renamed; the second is left
        // unchanged and recorded as a SkippedCorrection.
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
        // The losing key is recorded as skipped
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].original, "targt");
        assert_eq!(result.skipped[0].candidate, "target");
        assert_eq!(result.skipped[0].reason, SkipReason::TargetExists);
    }

    #[test]
    fn test_collision_existing_key_skip_is_recorded() {
        // The candidate already exists as a literal key: the typo key is NOT
        // renamed onto it (no data loss), and the skip is recorded.
        let json = r#"{"type": "AddDerive", "target": "User", "taget": "Post"}"#;
        let schema = test_schema();
        let options = FuzzyOptions::default();

        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert_eq!(result.repaired["target"], "User");
        assert_eq!(result.repaired["taget"], "Post");
        assert!(!result.has_corrections());
        assert!(result.has_skipped());
        assert_eq!(result.skipped[0].original, "taget");
        assert_eq!(result.skipped[0].reason, SkipReason::TargetExists);
    }

    #[test]
    fn test_repair_enum_array_no_correction_needed() {
        let schema =
            TaggedEnumSchema::new("type", &["AddDerive"], |_| Some(&["target", "derives"][..]))
                .with_enum_array("derives", ["Debug", "Clone"]);

        let json = r#"{"type": "AddDerive", "target": "User", "derives": ["Debug", "Clone"]}"#;
        let options = FuzzyOptions::default();

        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert!(!result.has_corrections());
    }

    // ------------------------------------------------------------------
    // New in 0.2: recursion, object arrays, coercion, enum values, dynamic
    // ------------------------------------------------------------------

    #[test]
    fn test_deeply_nested_object_repair() {
        // depth 3: config -> server -> limits
        let schema = TaggedEnumSchema::with_tag("type").with_variant(
            "Configure",
            ObjectSchema::new(["name"]).with_field_kind(
                "config",
                FieldKind::Object(ObjectSchema::new(["host"]).with_field_kind(
                    "server",
                    FieldKind::Object(ObjectSchema::new(["port"]).with_field_kind(
                        "limits",
                        FieldKind::Object(ObjectSchema::new(["max_conn", "timeout"])),
                    )),
                )),
            ),
        );

        let json = r#"{
            "type": "Configure",
            "name": "api",
            "config": {"host": "x", "server": {"prot": 80, "limits": {"max_con": 10, "timout": 5}}}
        }"#;
        let result =
            repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

        let server = &result.repaired["config"]["server"];
        assert_eq!(server["port"], 80);
        assert_eq!(server["limits"]["max_conn"], 10);
        assert_eq!(server["limits"]["timeout"], 5);
        assert_eq!(result.corrections.len(), 3);
        // Paths reflect the nesting
        assert!(result
            .corrections
            .iter()
            .any(|c| c.field_path == "$.config.server.limits.timout"));
    }

    #[test]
    fn test_object_array_repair() {
        let schema = TaggedEnumSchema::with_tag("type").with_variant(
            "Batch",
            ObjectSchema::empty().with_field_kind(
                "items",
                FieldKind::ObjectArray(
                    ObjectSchema::new(["name"])
                        .with_field_kind("kind", FieldKind::enum_of(["file", "dir"])),
                ),
            ),
        );

        let json = r#"{
            "type": "Batch",
            "items": [
                {"nam": "a", "kind": "fil"},
                {"name": "b", "knd": "dirr"}
            ]
        }"#;
        let result =
            repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

        assert_eq!(result.repaired["items"][0]["name"], "a");
        assert_eq!(result.repaired["items"][0]["kind"], "file");
        assert_eq!(result.repaired["items"][1]["kind"], "dir");
        assert_eq!(result.corrections.len(), 4);
        assert!(result
            .corrections
            .iter()
            .any(|c| c.field_path.starts_with("$.items[1]")));
    }

    #[test]
    fn test_enum_value_repair_on_string_field() {
        let schema = TaggedEnumSchema::with_tag("type").with_variant(
            "SetLevel",
            ObjectSchema::empty()
                .with_field_kind("level", FieldKind::enum_of(["debug", "info", "warn"])),
        );

        let json = r#"{"type": "SetLevel", "level": "inof"}"#;
        let result =
            repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

        assert_eq!(result.repaired["level"], "info");
        assert_eq!(result.corrections.len(), 1);
    }

    #[test]
    fn test_type_coercion() {
        let schema = TaggedEnumSchema::with_tag("type").with_variant(
            "Configure",
            ObjectSchema::empty()
                .with_field_kind("timeout", FieldKind::Integer)
                .with_field_kind("rate", FieldKind::Number)
                .with_field_kind("enabled", FieldKind::Bool)
                .with_field_kind("label", FieldKind::String),
        );

        let json = r#"{
            "type": "Configure",
            "timeout": "30",
            "rate": "0.5",
            "enabled": "true",
            "label": 42
        }"#;
        let result =
            repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

        assert_eq!(result.repaired["timeout"], 30);
        assert_eq!(result.repaired["rate"], 0.5);
        assert_eq!(result.repaired["enabled"], true);
        assert_eq!(result.repaired["label"], "42");
        assert_eq!(result.corrections.len(), 4);
        // Coercions are recorded with similarity 1.0
        assert!(result.corrections.iter().all(|c| c.similarity == 1.0));
    }

    #[test]
    fn test_type_coercion_leaves_unparseable_untouched() {
        let schema = TaggedEnumSchema::with_tag("type").with_variant(
            "Configure",
            ObjectSchema::empty()
                .with_field_kind("timeout", FieldKind::Integer)
                .with_field_kind("enabled", FieldKind::Bool),
        );

        let json = r#"{"type": "Configure", "timeout": "soon", "enabled": "maybe"}"#;
        let result =
            repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

        assert_eq!(result.repaired["timeout"], "soon");
        assert_eq!(result.repaired["enabled"], "maybe");
        assert!(!result.has_corrections());
    }

    #[test]
    fn test_type_coercion_noop_when_already_typed() {
        let schema = TaggedEnumSchema::with_tag("type").with_variant(
            "Configure",
            ObjectSchema::empty().with_field_kind("timeout", FieldKind::Integer),
        );

        let json = r#"{"type": "Configure", "timeout": 30}"#;
        let result =
            repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

        assert_eq!(result.repaired["timeout"], 30);
        assert!(!result.has_corrections());
    }

    #[test]
    fn test_dynamic_schema_repair() {
        // Schema built from runtime strings (no 'static required)
        let tags = vec![String::from("Create"), String::from("Delete")];
        let mut schema = TaggedEnumSchema::with_tag("kind");
        for tag in &tags {
            schema = schema.with_variant(tag, ObjectSchema::new(vec!["name", "path"]));
        }

        let json = r#"{"kind": "Creat", "nme": "x", "pth": "/tmp"}"#;
        let result =
            repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

        assert_eq!(result.repaired["kind"], "Create");
        assert!(result.repaired.get("name").is_some());
        assert!(result.repaired.get("path").is_some());
        assert_eq!(result.corrections.len(), 3);
    }

    #[test]
    fn test_enum_array_inside_nested_object() {
        // EnumArray nested inside an Object kind — impossible in 0.1
        let schema = TaggedEnumSchema::with_tag("type").with_variant(
            "AddDerive",
            ObjectSchema::new(["target"]).with_field_kind(
                "config",
                FieldKind::Object(ObjectSchema::empty().with_field_kind(
                    "derives",
                    FieldKind::enum_array(["Debug", "Clone", "Serialize"]),
                )),
            ),
        );

        let json = r#"{"type": "AddDerive", "target": "User", "config": {"derives": ["Debg"]}}"#;
        let result =
            repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

        assert_eq!(result.repaired["config"]["derives"][0], "Debug");
        assert_eq!(result.corrections.len(), 1);
        assert_eq!(result.corrections[0].field_path, "$.config.derives[0]");
    }
}
