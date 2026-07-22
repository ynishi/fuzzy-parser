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
//!    schema's field names and renamed (collision-safe, best-match-win;
//!    skipped renames are recorded as [`SkippedCorrection`]s).
//! 2. **Value repair** — each field's value is repaired according to its
//!    [`FieldKind`]: fuzzy correction against closed sets, recursive descent
//!    into nested objects / arrays of objects, or scalar type coercion.
//!
//! Opt-in extras (all off by default, see [`FuzzyOptions`]): wrapping single
//! values into expected arrays, filling missing fields with schema defaults,
//! and dropping unknown fields.
//!
//! Every change is recorded as a [`Correction`]; every rename that was
//! *not* applied because the target key already existed is recorded as a
//! [`SkippedCorrection`]; filled defaults and dropped fields are recorded as
//! [`FilledDefault`] / [`DroppedField`]. Nothing is changed silently.

use crate::distance::{find_closest, Algorithm};
use crate::error::FuzzyError;
use crate::sanitize::{detect_duplicate_keys, DuplicateKey};
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

    /// Coerce string-encoded scalars toward the schema's expected type
    /// (`"42"` → `42`, lossless only). See [`FieldKind::Integer`] et al.
    ///
    /// Default: `true` (matches pre-0.4 behavior)
    pub coerce_types: bool,

    /// Wrap a single value in an array when the schema expects an array
    /// (`"Debug"` → `["Debug"]` for [`FieldKind::EnumArray`], a lone object
    /// for [`FieldKind::ObjectArray`] / [`FieldKind::TaggedEnumArray`]).
    ///
    /// Only values that match the array's element shape are wrapped; the
    /// wrap is recorded as a [`Correction`] with similarity `1.0`.
    ///
    /// Default: `false`
    pub wrap_single_values: bool,

    /// Unwrap a one-element array when the schema expects a single value
    /// (`["Debug"]` → `"Debug"` for [`FieldKind::Enum`], a one-object array
    /// for [`FieldKind::Object`] / [`FieldKind::TaggedEnum`], one-scalar
    /// arrays for the coercion kinds). The reverse of
    /// [`wrap_single_values`](Self::wrap_single_values).
    ///
    /// Only one-element arrays whose element matches the expected shape are
    /// unwrapped; the unwrap is recorded as a [`Correction`] with
    /// similarity `1.0`.
    ///
    /// Default: `false`
    pub unwrap_singleton_arrays: bool,

    /// Insert schema-defined default values for missing fields
    /// (see [`FieldDef::default`](crate::FieldDef) and the
    /// `with_field_default` builders). Each insertion is recorded as a
    /// [`FilledDefault`].
    ///
    /// Default: `false`
    pub fill_defaults: bool,

    /// Remove object keys that are neither valid schema fields nor
    /// fuzzy-repairable to one. Each removal is recorded as a
    /// [`DroppedField`] (the dropped value is preserved in the log).
    ///
    /// Keys whose rename was collision-skipped are also removed (they are
    /// still unknown after the rename pass); the [`SkippedCorrection`]
    /// remains in the log alongside the [`DroppedField`].
    ///
    /// Default: `false`
    pub drop_unknown_fields: bool,
}

impl Default for FuzzyOptions {
    fn default() -> Self {
        Self {
            min_similarity: 0.7,
            algorithm: Algorithm::JaroWinkler,
            coerce_types: true,
            wrap_single_values: false,
            unwrap_singleton_arrays: false,
            fill_defaults: false,
            drop_unknown_fields: false,
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

    /// Enable / disable lossless scalar type coercion (default: enabled)
    pub fn with_coerce_types(mut self, coerce_types: bool) -> Self {
        self.coerce_types = coerce_types;
        self
    }

    /// Enable / disable wrapping single values into expected arrays
    /// (default: disabled)
    pub fn with_wrap_single_values(mut self, wrap_single_values: bool) -> Self {
        self.wrap_single_values = wrap_single_values;
        self
    }

    /// Enable / disable unwrapping one-element arrays into expected single
    /// values (default: disabled)
    pub fn with_unwrap_singleton_arrays(mut self, unwrap_singleton_arrays: bool) -> Self {
        self.unwrap_singleton_arrays = unwrap_singleton_arrays;
        self
    }

    /// Enable / disable filling missing fields with schema defaults
    /// (default: disabled)
    pub fn with_fill_defaults(mut self, fill_defaults: bool) -> Self {
        self.fill_defaults = fill_defaults;
        self
    }

    /// Enable / disable dropping unknown (unrepairable) fields
    /// (default: disabled)
    pub fn with_drop_unknown_fields(mut self, drop_unknown_fields: bool) -> Self {
        self.drop_unknown_fields = drop_unknown_fields;
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
    /// would have overwritten data (best-match-win collision policy).
    TargetExists,
}

/// A correction that was found but *not* applied.
///
/// Recorded when a typo key resolves to a candidate field name but the
/// candidate already exists in the object (either as a literal key or
/// because a higher-similarity typo key won the rename). See
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

/// A missing field that was filled with its schema-defined default value.
///
/// Recorded only when [`FuzzyOptions::fill_defaults`] is enabled.
#[derive(Debug, Clone, PartialEq)]
pub struct FilledDefault {
    /// The field name that was inserted
    pub field: String,
    /// The default value that was inserted
    pub value: Value,
    /// JSON path to the inserted field (e.g., "$.retries")
    pub field_path: String,
}

/// An unknown field that was removed from the object.
///
/// Recorded only when [`FuzzyOptions::drop_unknown_fields`] is enabled.
/// The removed value is preserved here, so nothing is lost silently.
#[derive(Debug, Clone, PartialEq)]
pub struct DroppedField {
    /// The key that was removed
    pub field: String,
    /// The value the key held when it was removed
    pub value: Value,
    /// JSON path to the removed field (e.g., "$.extraneous")
    pub field_path: String,
}

/// Accumulated log of a repair pass: applied and skipped corrections.
#[derive(Debug, Clone, Default)]
pub struct RepairLog {
    /// Corrections that were applied
    pub corrections: Vec<Correction>,
    /// Corrections that were found but skipped (collision safety)
    pub skipped: Vec<SkippedCorrection>,
    /// Missing fields filled with schema defaults ([`FuzzyOptions::fill_defaults`])
    pub filled: Vec<FilledDefault>,
    /// Unknown fields removed ([`FuzzyOptions::drop_unknown_fields`])
    pub dropped: Vec<DroppedField>,
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

    /// Check if any defaults were filled in
    pub fn has_filled(&self) -> bool {
        !self.filled.is_empty()
    }

    /// Check if any unknown fields were dropped
    pub fn has_dropped(&self) -> bool {
        !self.dropped.is_empty()
    }

    /// Merge another log into this one
    pub fn merge(&mut self, other: RepairLog) {
        self.corrections.extend(other.corrections);
        self.skipped.extend(other.skipped);
        self.filled.extend(other.filled);
        self.dropped.extend(other.dropped);
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
    /// Missing fields filled with schema defaults ([`FuzzyOptions::fill_defaults`])
    pub filled: Vec<FilledDefault>,
    /// Unknown fields removed ([`FuzzyOptions::drop_unknown_fields`])
    pub dropped: Vec<DroppedField>,
    /// Duplicate keys detected in the *input text* (always populated by
    /// [`repair_tagged_enum_json`]; `serde_json` keeps the last occurrence)
    pub duplicates: Vec<DuplicateKey>,
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

    /// Check if any defaults were filled in
    pub fn has_filled(&self) -> bool {
        !self.filled.is_empty()
    }

    /// Check if any unknown fields were dropped
    pub fn has_dropped(&self) -> bool {
        !self.dropped.is_empty()
    }

    /// Check if any duplicate keys were detected in the input
    pub fn has_duplicates(&self) -> bool {
        !self.duplicates.is_empty()
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
/// # Collision behavior (best-match-win)
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
    fill_missing_defaults(obj, &schema.fields, path, options, &mut log);
    apply_field_kinds(obj, &schema.fields, path, options, &mut log);
    log
}

/// Repair field names in a JSON object using a field list
///
/// # Collision behavior (best-match-win)
///
/// A key is only renamed when the target field does not already exist in the
/// object. This guards against destroying data: if two typo keys resolve to
/// the same candidate, the key with the **highest similarity** wins (ties
/// broken by lexicographic key order, so the outcome is deterministic and
/// independent of the map's iteration order). The losing key is left
/// unchanged and recorded as a [`SkippedCorrection`] with
/// [`SkipReason::TargetExists`]. The same applies when the candidate is
/// already present as a literal key — literal keys always win.
///
/// # Unknown fields
///
/// With [`FuzzyOptions::drop_unknown_fields`] enabled, keys that are still
/// unknown after the rename pass (no candidate above the threshold, or
/// collision-skipped) are removed and recorded as [`DroppedField`]s.
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

/// Core field-name rename pass (collision-safe, best-match-win).
fn repair_field_names_into(
    obj: &mut Map<String, Value>,
    valid_fields: &[&str],
    skip_key: Option<&str>,
    path: &str,
    options: &FuzzyOptions,
    log: &mut RepairLog,
) {
    // Collect keys that need correction, with their best candidate (if any)
    let mut matched: Vec<(String, crate::distance::Match)> = Vec::new();
    for key in obj.keys() {
        if Some(key.as_str()) == skip_key || valid_fields.contains(&key.as_str()) {
            continue;
        }
        if let Some(m) = find_closest(
            key,
            valid_fields.iter().copied(),
            options.min_similarity,
            options.algorithm,
        ) {
            matched.push((key.clone(), m));
        }
    }

    // Best-match-win: process higher similarity first so that when two typo
    // keys resolve to the same candidate, the closer one gets the rename.
    // Ties break by key order — deterministic regardless of map order.
    matched.sort_by(|(ka, ma), (kb, mb)| {
        mb.similarity
            .partial_cmp(&ma.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| ka.cmp(kb))
    });

    for (key, m) in matched {
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

    // Optionally drop keys that are still unknown after the rename pass
    if options.drop_unknown_fields {
        let to_drop: Vec<String> = obj
            .keys()
            .filter(|k| Some(k.as_str()) != skip_key && !valid_fields.contains(&k.as_str()))
            .cloned()
            .collect();
        for key in to_drop {
            if let Some(value) = obj.remove(&key) {
                log.dropped.push(DroppedField {
                    field: key.clone(),
                    value,
                    field_path: format!("{}.{}", path, key),
                });
            }
        }
    }
}

/// Insert schema defaults for missing fields (when `fill_defaults` is on).
fn fill_missing_defaults(
    obj: &mut Map<String, Value>,
    fields: &[FieldDef],
    path: &str,
    options: &FuzzyOptions,
    log: &mut RepairLog,
) {
    if !options.fill_defaults {
        return;
    }
    for def in fields {
        if let Some(default) = &def.default {
            if !obj.contains_key(&def.name) {
                obj.insert(def.name.clone(), default.clone());
                log.filled.push(FilledDefault {
                    field: def.name.clone(),
                    value: default.clone(),
                    field_path: format!("{}.{}", path, def.name),
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
            if options.unwrap_singleton_arrays {
                unwrap_singleton_array(value, Value::is_string, path, log);
            }
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
            if options.wrap_single_values {
                wrap_single_into_array(value, Value::is_string, path, log);
            }
            if let Value::Array(arr) = value {
                let valid: Vec<&str> = valid_values.iter().map(|v| v.as_str()).collect();
                let arr_log = repair_enum_array(arr, &valid, path, options);
                log.merge(arr_log);
            }
        }
        FieldKind::Object(schema) => {
            if options.unwrap_singleton_arrays {
                unwrap_singleton_array(value, Value::is_object, path, log);
            }
            if let Value::Object(nested) = value {
                let nested_log = repair_object_fields(nested, schema, path, options);
                log.merge(nested_log);
            }
        }
        FieldKind::ObjectArray(schema) => {
            if options.wrap_single_values {
                wrap_single_into_array(value, Value::is_object, path, log);
            }
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
        FieldKind::TaggedEnum(schema) => {
            if options.unwrap_singleton_arrays {
                unwrap_singleton_array(value, Value::is_object, path, log);
            }
            if let Value::Object(nested) = value {
                let nested_log = repair_tagged_enum(nested, schema, path, options);
                log.merge(nested_log);
            }
        }
        FieldKind::TaggedEnumArray(schema) => {
            if options.wrap_single_values {
                wrap_single_into_array(value, Value::is_object, path, log);
            }
            if let Value::Array(arr) = value {
                let arr_log = repair_tagged_enum_array(arr, schema, path, options);
                log.merge(arr_log);
            }
        }
        FieldKind::Integer | FieldKind::Number | FieldKind::Bool | FieldKind::String => {
            if options.unwrap_singleton_arrays {
                unwrap_singleton_array(
                    value,
                    |v| v.is_string() || v.is_number() || v.is_boolean(),
                    path,
                    log,
                );
            }
            if options.coerce_types {
                coerce_value(value, kind, path, log);
            }
        }
    }
}

/// Unwrap a one-element array into its element when the schema expects a
/// single value and the element matches the expected shape.
///
/// The reverse of [`wrap_single_into_array`]; recorded as a [`Correction`]
/// with similarity `1.0` (like coercions).
fn unwrap_singleton_array(
    value: &mut Value,
    element_matches: impl Fn(&Value) -> bool,
    path: &str,
    log: &mut RepairLog,
) {
    let Value::Array(arr) = &*value else {
        return;
    };
    if arr.len() != 1 || !element_matches(&arr[0]) {
        return;
    }
    let original = value.to_string();
    let Value::Array(arr) = std::mem::take(value) else {
        unreachable!("checked above");
    };
    *value = arr.into_iter().next().expect("len checked above");
    log.corrections.push(Correction::new(
        original,
        value.to_string(),
        1.0,
        path.to_string(),
    ));
}

/// Wrap a single value into a one-element array when the schema expects an
/// array and the value matches the array's element shape.
///
/// Recorded as a [`Correction`] with similarity `1.0` (like coercions).
fn wrap_single_into_array(
    value: &mut Value,
    element_matches: impl Fn(&Value) -> bool,
    path: &str,
    log: &mut RepairLog,
) {
    if value.is_array() || value.is_null() || !element_matches(value) {
        return;
    }
    let original = value.to_string();
    let single = std::mem::take(value);
    *value = Value::Array(vec![single]);
    log.corrections.push(Correction::new(
        original,
        value.to_string(),
        1.0,
        path.to_string(),
    ));
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
/// 1. The tag field *key* if it is missing due to a typo (e.g. `"tpye"` →
///    `"type"`) — only when the typo key's value resolves to a known tag
/// 2. The tag field value (e.g., "AddDeriv" -> "AddDerive")
/// 3. The field names based on the tag value (variant fields + global fields)
/// 4. Field values according to their [`FieldKind`] — enum arrays, nested
///    objects (recursively, to any depth), arrays of objects, and scalar
///    type coercion
///
/// # Missing / unusable tag
///
/// When no usable tag remains (absent even after key recovery, or not a
/// string), the variant cannot be determined — but the schema's *global*
/// fields still apply to every variant, so they are repaired (names,
/// defaults, kinds) before returning. Unknown-field dropping is suppressed
/// in this fallback: without a tag, variant fields cannot be told apart
/// from junk.
///
/// # Collision behavior (best-match-win)
///
/// Field-name repair never overwrites an existing key: when two typo keys
/// resolve to the same candidate, the higher-similarity key is renamed and
/// the other is recorded as a [`SkippedCorrection`]. See
/// [`repair_fields_with_list`] for details.
pub fn repair_tagged_enum(
    obj: &mut Map<String, Value>,
    schema: &TaggedEnumSchema,
    path: &str,
    options: &FuzzyOptions,
) -> RepairLog {
    let mut log = RepairLog::default();

    // Step 0: If the tag field is missing, try to recover a typo'd tag key
    if !obj.contains_key(&schema.tag_field) {
        recover_tag_key(obj, schema, path, options, &mut log);
    }

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
                obj.insert(schema.tag_field.clone(), Value::String(m.candidate.clone()));
                m.candidate
            } else {
                tag_val.to_string()
            }
        } else {
            tag_val.to_string()
        }
    } else {
        // No usable tag: the variant is unknown, but global fields apply to
        // every variant — repair them instead of bailing out entirely.
        // Dropping is suppressed (variant fields look like unknown keys).
        if !schema.global_fields.is_empty() {
            let global_names: Vec<&str> = schema
                .global_fields
                .iter()
                .map(|d| d.name.as_str())
                .collect();
            let no_drop = options.clone().with_drop_unknown_fields(false);
            repair_field_names_into(
                obj,
                &global_names,
                Some(schema.tag_field.as_str()),
                path,
                &no_drop,
                &mut log,
            );
            fill_missing_defaults(obj, &schema.global_fields, path, options, &mut log);
            apply_field_kinds(obj, &schema.global_fields, path, options, &mut log);
        }
        return log;
    };

    // Step 2: Repair field names (variant fields + global fields)
    let variant = schema.variant_schema(&tag_value);
    let mut valid_fields: Vec<&str> = variant
        .map(|s| s.field_names().collect())
        .unwrap_or_default();
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

    // Step 3: Fill missing fields with schema defaults (opt-in)
    if let Some(variant) = variant {
        fill_missing_defaults(obj, &variant.fields, path, options, &mut log);
    }
    fill_missing_defaults(obj, &schema.global_fields, path, options, &mut log);

    // Step 4: Apply variant field kinds (recursive)
    if let Some(variant) = variant {
        apply_field_kinds(obj, &variant.fields, path, options, &mut log);
    }

    // Step 5: Apply global field kinds (enum arrays, nested objects, coercion)
    apply_field_kinds(obj, &schema.global_fields, path, options, &mut log);

    log
}

/// Recover a typo'd tag *key* (e.g. `"tpye": "AddDerive"` → `"type"`).
///
/// Conservative double evidence is required before renaming:
///
/// 1. the key is fuzzy-close to the schema's tag field name, and
/// 2. the key's value is a string that is (or is fuzzy-close to) a known
///    tag value.
///
/// This keeps ordinary data fields with tag-like names from being hijacked
/// into the tag slot.
fn recover_tag_key(
    obj: &mut Map<String, Value>,
    schema: &TaggedEnumSchema,
    path: &str,
    options: &FuzzyOptions,
    log: &mut RepairLog,
) {
    // Candidate keys whose *value* could plausibly be a tag value
    let tag_like_keys: Vec<&str> = obj
        .iter()
        .filter(|(_, v)| {
            v.as_str().is_some_and(|s| {
                schema.is_valid_tag(s)
                    || find_closest(
                        s,
                        schema.tag_values(),
                        options.min_similarity,
                        options.algorithm,
                    )
                    .is_some()
            })
        })
        .map(|(k, _)| k.as_str())
        .collect();

    if let Some(m) = find_closest(
        &schema.tag_field,
        tag_like_keys,
        options.min_similarity,
        options.algorithm,
    ) {
        if let Some(val) = obj.remove(&m.candidate) {
            log.corrections.push(Correction::new(
                m.candidate.clone(),
                schema.tag_field.clone(),
                m.similarity,
                format!("{}.{}", path, m.candidate),
            ));
            obj.insert(schema.tag_field.clone(), val);
        }
    }
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
///
/// Also scans the input text for duplicate object keys (which `serde_json`
/// collapses to the last occurrence during parsing) and reports them in
/// [`RepairResult::duplicates`] — detection only, no merge.
pub fn repair_tagged_enum_json(
    json: &str,
    schema: &TaggedEnumSchema,
    options: &FuzzyOptions,
) -> Result<RepairResult, FuzzyError> {
    let mut value: Value = serde_json::from_str(json)?;
    let duplicates = detect_duplicate_keys(json);

    let log = if let Some(obj) = value.as_object_mut() {
        repair_tagged_enum(obj, schema, "$", options)
    } else {
        return Err(FuzzyError::NotObject);
    };

    Ok(RepairResult {
        repaired: value,
        corrections: log.corrections,
        skipped: log.skipped,
        filled: log.filled,
        dropped: log.dropped,
        duplicates,
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
        let schema =
            TaggedEnumSchema::new("type", &["AddDerive"], |_| Some(&["target", "derives"][..]))
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
        // Best-match-win: the higher-similarity key ("targt") is renamed;
        // the other is left unchanged and recorded as a SkippedCorrection.
        let json = r#"{"type": "AddDerive", "taget": "User", "targt": "Post"}"#;
        let schema = test_schema();
        let options = FuzzyOptions::default();

        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        // Exactly one key won the rename; the other survives verbatim.
        assert_eq!(result.repaired["target"], "Post");
        assert_eq!(result.repaired["taget"], "User");
        assert!(result.repaired.get("targt").is_none());
        assert_eq!(result.corrections.len(), 1);
        assert_eq!(result.corrections[0].original, "targt");
        assert_eq!(result.corrections[0].corrected, "target");
        // The losing key is recorded as skipped
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].original, "taget");
        assert_eq!(result.skipped[0].candidate, "target");
        assert_eq!(result.skipped[0].reason, SkipReason::TargetExists);
        // The winner's similarity is >= the loser's (best-match-win invariant)
        assert!(result.corrections[0].similarity >= result.skipped[0].similarity);
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
        let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

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
        let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

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
        let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

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
        let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

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
        let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

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
        let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

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
        let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

        assert_eq!(result.repaired["kind"], "Create");
        assert!(result.repaired.get("name").is_some());
        assert!(result.repaired.get("path").is_some());
        assert_eq!(result.corrections.len(), 3);
    }

    #[test]
    fn test_nested_tagged_enum_field_repair() {
        // A variant field holds another tagged enum
        let action_schema = TaggedEnumSchema::with_tag("kind")
            .with_variant("Move", ObjectSchema::new(["from", "to"]))
            .with_variant("Copy", ObjectSchema::new(["from", "to"]));
        let schema = TaggedEnumSchema::with_tag("type").with_variant(
            "Command",
            ObjectSchema::new(["name"])
                .with_field_kind("action", FieldKind::TaggedEnum(action_schema)),
        );

        let json = r#"{
            "type": "Command",
            "name": "x",
            "action": {"kind": "Mve", "frm": "/a", "to": "/b"}
        }"#;
        let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

        assert_eq!(result.repaired["action"]["kind"], "Move");
        assert!(result.repaired["action"].get("from").is_some());
        assert_eq!(result.corrections.len(), 2);
        assert!(result
            .corrections
            .iter()
            .any(|c| c.field_path == "$.action.kind"));
    }

    #[test]
    fn test_tagged_enum_array_field_repair() {
        // A variant field holds an array of tagged enums (DSL intents)
        let intent_schema = TaggedEnumSchema::with_tag("type")
            .with_variant("AddDerive", ObjectSchema::new(["target"]))
            .with_variant("Rename", ObjectSchema::new(["from", "to"]));
        let schema = TaggedEnumSchema::with_tag("type").with_variant(
            "Batch",
            ObjectSchema::empty()
                .with_field_kind("intents", FieldKind::TaggedEnumArray(intent_schema)),
        );

        let json = r#"{
            "type": "Batch",
            "intents": [
                {"type": "AddDeriv", "taget": "User"},
                {"type": "Renme", "from": "a", "too": "b"}
            ]
        }"#;
        let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

        assert_eq!(result.repaired["intents"][0]["type"], "AddDerive");
        assert!(result.repaired["intents"][0].get("target").is_some());
        assert_eq!(result.repaired["intents"][1]["type"], "Rename");
        assert!(result.repaired["intents"][1].get("to").is_some());
        assert_eq!(result.corrections.len(), 4);
        assert!(result
            .corrections
            .iter()
            .any(|c| c.field_path.starts_with("$.intents[1]")));
    }

    // ------------------------------------------------------------------
    // New in 0.4: coercion toggle, single-value wrap, defaults, drop
    // ------------------------------------------------------------------

    #[test]
    fn test_coerce_types_disabled() {
        let schema = TaggedEnumSchema::with_tag("type").with_variant(
            "Configure",
            ObjectSchema::empty().with_field_kind("timeout", FieldKind::Integer),
        );

        let json = r#"{"type": "Configure", "timeout": "30"}"#;
        let options = FuzzyOptions::default().with_coerce_types(false);
        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        // String stays a string when coercion is off
        assert_eq!(result.repaired["timeout"], "30");
        assert!(!result.has_corrections());
    }

    #[test]
    fn test_wrap_single_value_into_enum_array() {
        let schema =
            TaggedEnumSchema::new("type", &["AddDerive"], |_| Some(&["target", "derives"][..]))
                .with_enum_array("derives", ["Debug", "Clone"]);

        // LLM emitted a lone string (with a typo) where an array was expected
        let json = r#"{"type": "AddDerive", "target": "User", "derives": "Debg"}"#;
        let options = FuzzyOptions::default().with_wrap_single_values(true);
        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert_eq!(result.repaired["derives"], serde_json::json!(["Debug"]));
        // Two corrections: the wrap and the fuzzy fix inside the array
        assert_eq!(result.corrections.len(), 2);
    }

    #[test]
    fn test_wrap_single_value_into_object_array() {
        let schema = TaggedEnumSchema::with_tag("type").with_variant(
            "Batch",
            ObjectSchema::empty()
                .with_field_kind("items", FieldKind::ObjectArray(ObjectSchema::new(["name"]))),
        );

        let json = r#"{"type": "Batch", "items": {"nam": "a"}}"#;
        let options = FuzzyOptions::default().with_wrap_single_values(true);
        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert_eq!(result.repaired["items"][0]["name"], "a");
    }

    #[test]
    fn test_wrap_single_value_shape_mismatch_left_untouched() {
        let schema = TaggedEnumSchema::new("type", &["AddDerive"], |_| Some(&["derives"][..]))
            .with_enum_array("derives", ["Debug"]);

        // A number does not match the enum array's element shape — no wrap
        let json = r#"{"type": "AddDerive", "derives": 42}"#;
        let options = FuzzyOptions::default().with_wrap_single_values(true);
        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert_eq!(result.repaired["derives"], 42);
        assert!(!result.has_corrections());
    }

    #[test]
    fn test_wrap_disabled_by_default() {
        let schema = TaggedEnumSchema::new("type", &["AddDerive"], |_| Some(&["derives"][..]))
            .with_enum_array("derives", ["Debug"]);

        let json = r#"{"type": "AddDerive", "derives": "Debug"}"#;
        let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

        // Default options: single value is left as-is
        assert_eq!(result.repaired["derives"], "Debug");
    }

    #[test]
    fn test_fill_defaults_inserts_missing_field() {
        let schema = TaggedEnumSchema::with_tag("type").with_variant(
            "Configure",
            ObjectSchema::new(["name"])
                .with_field_kind("retries", FieldKind::Integer)
                .with_field_default("retries", serde_json::json!(3)),
        );

        let json = r#"{"type": "Configure", "name": "api"}"#;
        let options = FuzzyOptions::default().with_fill_defaults(true);
        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert_eq!(result.repaired["retries"], 3);
        assert_eq!(result.filled.len(), 1);
        assert_eq!(result.filled[0].field, "retries");
        assert_eq!(result.filled[0].field_path, "$.retries");
        // A filled default is not a correction
        assert!(!result.has_corrections());
    }

    #[test]
    fn test_fill_defaults_does_not_overwrite_present_field() {
        let schema = TaggedEnumSchema::with_tag("type").with_variant(
            "Configure",
            ObjectSchema::empty().with_field_default("retries", serde_json::json!(3)),
        );

        let json = r#"{"type": "Configure", "retries": 7}"#;
        let options = FuzzyOptions::default().with_fill_defaults(true);
        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert_eq!(result.repaired["retries"], 7);
        assert!(!result.has_filled());
    }

    #[test]
    fn test_fill_defaults_off_by_default() {
        let schema = TaggedEnumSchema::with_tag("type").with_variant(
            "Configure",
            ObjectSchema::empty().with_field_default("retries", serde_json::json!(3)),
        );

        let json = r#"{"type": "Configure"}"#;
        let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

        assert!(result.repaired.get("retries").is_none());
        assert!(!result.has_filled());
    }

    #[test]
    fn test_fill_defaults_applies_after_rename() {
        // A typo key renames onto the defaulted field first, so no fill happens
        let schema = TaggedEnumSchema::with_tag("type").with_variant(
            "Configure",
            ObjectSchema::new(["retries"]).with_field_default("retries", serde_json::json!(3)),
        );

        let json = r#"{"type": "Configure", "retres": 7}"#;
        let options = FuzzyOptions::default().with_fill_defaults(true);
        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert_eq!(result.repaired["retries"], 7);
        assert!(!result.has_filled());
        assert_eq!(result.corrections.len(), 1);
    }

    #[test]
    fn test_drop_unknown_fields() {
        let schema = test_schema();

        // "commentary" matches nothing; "taget" is repairable
        let json = r#"{"type": "AddDerive", "taget": "User", "commentary": "sure, here you go"}"#;
        let options = FuzzyOptions::default().with_drop_unknown_fields(true);
        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert_eq!(result.repaired["target"], "User");
        assert!(result.repaired.get("commentary").is_none());
        assert_eq!(result.dropped.len(), 1);
        assert_eq!(result.dropped[0].field, "commentary");
        // The dropped value is preserved in the log
        assert_eq!(result.dropped[0].value, "sure, here you go");
    }

    #[test]
    fn test_drop_unknown_keeps_tag_and_valid_fields() {
        let schema = test_schema();

        let json = r#"{"type": "AddDerive", "target": "User", "derives": ["Debug"]}"#;
        let options = FuzzyOptions::default().with_drop_unknown_fields(true);
        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert_eq!(result.repaired["type"], "AddDerive");
        assert_eq!(result.repaired["target"], "User");
        assert!(!result.has_dropped());
    }

    #[test]
    fn test_drop_unknown_fields_off_by_default() {
        let schema = test_schema();

        let json = r#"{"type": "AddDerive", "target": "User", "commentary": "x"}"#;
        let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

        assert_eq!(result.repaired["commentary"], "x");
        assert!(!result.has_dropped());
    }

    #[test]
    fn test_drop_removes_collision_skipped_key() {
        // "taget" loses the rename race against the literal "target" key;
        // with drop_unknown_fields it is then removed (skip still recorded).
        let json = r#"{"type": "AddDerive", "target": "User", "taget": "Post"}"#;
        let schema = test_schema();
        let options = FuzzyOptions::default().with_drop_unknown_fields(true);

        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert_eq!(result.repaired["target"], "User");
        assert!(result.repaired.get("taget").is_none());
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.dropped.len(), 1);
        assert_eq!(result.dropped[0].field, "taget");
    }

    #[test]
    fn test_collision_deterministic_regardless_of_key_order() {
        // Same pair of typo keys in both lexicographic orders must produce
        // the same winner (best-match-win, not map-order-win).
        let schema = test_schema();
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
        assert_eq!(a.corrections[0].original, b.corrections[0].original);
    }

    // ------------------------------------------------------------------
    // New in 0.5: unwrap, tag-key recovery, tagless fallback, duplicates
    // ------------------------------------------------------------------

    #[test]
    fn test_unwrap_singleton_array_to_enum() {
        let schema = TaggedEnumSchema::with_tag("type").with_variant(
            "SetLevel",
            ObjectSchema::empty()
                .with_field_kind("level", FieldKind::enum_of(["debug", "info", "warn"])),
        );

        // LLM emitted a one-element array (with a typo) where a scalar
        // enum was expected
        let json = r#"{"type": "SetLevel", "level": ["inof"]}"#;
        let options = FuzzyOptions::default().with_unwrap_singleton_arrays(true);
        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert_eq!(result.repaired["level"], "info");
        // Two corrections: the unwrap and the fuzzy fix
        assert_eq!(result.corrections.len(), 2);
    }

    #[test]
    fn test_unwrap_singleton_array_to_scalar_coercion() {
        let schema = TaggedEnumSchema::with_tag("type").with_variant(
            "Configure",
            ObjectSchema::empty().with_field_kind("timeout", FieldKind::Integer),
        );

        let json = r#"{"type": "Configure", "timeout": ["30"]}"#;
        let options = FuzzyOptions::default().with_unwrap_singleton_arrays(true);
        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        // Unwrapped then coerced
        assert_eq!(result.repaired["timeout"], 30);
    }

    #[test]
    fn test_unwrap_singleton_array_to_nested_object() {
        let schema = TaggedEnumSchema::with_tag("type").with_variant(
            "Configure",
            ObjectSchema::empty()
                .with_field_kind("config", FieldKind::Object(ObjectSchema::new(["timeout"]))),
        );

        let json = r#"{"type": "Configure", "config": [{"timout": 5}]}"#;
        let options = FuzzyOptions::default().with_unwrap_singleton_arrays(true);
        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert_eq!(result.repaired["config"]["timeout"], 5);
    }

    #[test]
    fn test_unwrap_leaves_multi_element_and_mismatched_arrays() {
        let schema = TaggedEnumSchema::with_tag("type").with_variant(
            "SetLevel",
            ObjectSchema::empty()
                .with_field_kind("level", FieldKind::enum_of(["debug", "info"]))
                .with_field_kind("timeout", FieldKind::Integer),
        );

        // Multi-element array and shape-mismatched element are left alone
        let json = r#"{"type": "SetLevel", "level": ["debug", "info"], "timeout": [{"x": 1}]}"#;
        let options = FuzzyOptions::default().with_unwrap_singleton_arrays(true);
        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert!(result.repaired["level"].is_array());
        assert!(result.repaired["timeout"].is_array());
        assert!(!result.has_corrections());
    }

    #[test]
    fn test_unwrap_disabled_by_default() {
        let schema = TaggedEnumSchema::with_tag("type").with_variant(
            "SetLevel",
            ObjectSchema::empty().with_field_kind("level", FieldKind::enum_of(["info"])),
        );

        let json = r#"{"type": "SetLevel", "level": ["info"]}"#;
        let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

        assert!(result.repaired["level"].is_array());
        assert!(!result.has_corrections());
    }

    #[test]
    fn test_tag_key_typo_recovered() {
        // The tag *key* itself is typo'd: "tpye" -> "type", then the tag
        // value and fields repair as usual.
        let schema = test_schema();
        let json = r#"{"tpye": "AddDeriv", "taget": "User"}"#;
        let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

        assert_eq!(result.repaired["type"], "AddDerive");
        assert_eq!(result.repaired["target"], "User");
        assert!(result.repaired.get("tpye").is_none());
        // tag key + tag value + field name
        assert_eq!(result.corrections.len(), 3);
        assert!(result.corrections.iter().any(|c| c.original == "tpye"));
    }

    #[test]
    fn test_tag_key_recovery_requires_tag_like_value() {
        // "tpye" is close to "type" but its value is not tag-like — the
        // key must NOT be hijacked into the tag slot.
        let schema = test_schema();
        let json = r#"{"tpye": 42, "target": "User"}"#;
        let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

        assert_eq!(result.repaired["tpye"], 42);
        assert!(result.repaired.get("type").is_none());
    }

    #[test]
    fn test_missing_tag_still_repairs_global_fields() {
        // No tag at all: variant is unknown, but global fields are still
        // repaired (previously the whole object was returned untouched).
        let schema = TaggedEnumSchema::new("type", &["AddDerive"], |_| Some(&["target"][..]))
            .with_enum_array("derives", ["Debug", "Clone"])
            .with_field_kind("timeout", FieldKind::Integer);

        let json = r#"{"derivs": ["Debg"], "timeout": "30", "taget": "User"}"#;
        let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

        // Global field name + enum value + coercion repaired
        assert_eq!(result.repaired["derives"][0], "Debug");
        assert_eq!(result.repaired["timeout"], 30);
        // Variant field "taget" untouched (variant unknown)
        assert_eq!(result.repaired["taget"], "User");
    }

    #[test]
    fn test_missing_tag_fallback_never_drops_fields() {
        // Even with drop_unknown_fields on, the tagless fallback must not
        // drop anything — variant fields can't be told apart from junk.
        let schema = TaggedEnumSchema::new("type", &["AddDerive"], |_| Some(&["target"][..]))
            .with_field_kind("timeout", FieldKind::Integer);

        let json = r#"{"target": "User", "extra": 1}"#;
        let options = FuzzyOptions::default().with_drop_unknown_fields(true);
        let result = repair_tagged_enum_json(json, &schema, &options).unwrap();

        assert_eq!(result.repaired["target"], "User");
        assert_eq!(result.repaired["extra"], 1);
        assert!(!result.has_dropped());
    }

    #[test]
    fn test_duplicate_keys_reported() {
        let schema = test_schema();
        let json = r#"{"type": "AddDerive", "target": "User", "target": "Post"}"#;
        let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

        // serde_json kept the last occurrence; the loss is made visible
        assert_eq!(result.repaired["target"], "Post");
        assert!(result.has_duplicates());
        assert_eq!(result.duplicates.len(), 1);
        assert_eq!(result.duplicates[0].key, "target");
        assert_eq!(result.duplicates[0].field_path, "$.target");
        assert_eq!(result.duplicates[0].count, 2);
    }

    #[test]
    fn test_no_duplicates_reported_for_clean_input() {
        let schema = test_schema();
        let json = r#"{"type": "AddDerive", "target": "User"}"#;
        let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

        assert!(!result.has_duplicates());
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
        let result = repair_tagged_enum_json(json, &schema, &FuzzyOptions::default()).unwrap();

        assert_eq!(result.repaired["config"]["derives"][0], "Debug");
        assert_eq!(result.corrections.len(), 1);
        assert_eq!(result.corrections[0].field_path, "$.config.derives[0]");
    }
}
