//! Importing repair schemas from JSON Schema documents
//!
//! Converts a JSON Schema (as a `serde_json::Value`) into a
//! [`TaggedEnumSchema`] or [`ObjectSchema`], so schemas produced by
//! `schemars`, Pydantic, or any other tool can drive fuzzy repair without
//! hand-written builder code. With the `schemars` cargo feature enabled,
//! [`TaggedEnumSchema::from_type`] / [`ObjectSchema::from_type`] derive the
//! repair schema directly from a `#[derive(JsonSchema)]` type.
//!
//! # Supported subset (v1)
//!
//! - **Tagged enums**: `oneOf` where every branch carries a common tag
//!   property with a `const` (or single-element `enum`) string value — the
//!   shape schemars emits for `#[serde(tag = "...")]` (internally tagged)
//!   and `#[serde(tag = "...", content = "...")]` (adjacently tagged).
//! - **Keywords**: `properties`, `enum`, `const`, `type`, `items`,
//!   `$ref` / `$defs` (also legacy `definitions`), including the
//!   Draft 2020-12 sibling-`$ref` composition schemars emits for newtype
//!   variants. `Option<T>`-style nullable wrappers
//!   (`anyOf: [T, {type: null}]`) are unwrapped.
//! - **Type mapping**: `string` → [`FieldKind::String`],
//!   `integer` → [`FieldKind::Integer`], `number` → [`FieldKind::Number`],
//!   `boolean` → [`FieldKind::Bool`], `object` → [`FieldKind::Object`]
//!   (recursive), arrays of string enums → [`FieldKind::EnumArray`],
//!   arrays of objects → [`FieldKind::ObjectArray`].
//!
//! # Deliberately unsupported (v1)
//!
//! - **Externally tagged enums** (serde's default representation) and
//!   **untagged enums**: rejected with an error. Annotate the enum with
//!   `#[serde(tag = "...")]` so a tag field exists for repair to anchor on.
//! - **`required` / `default` completion**: missing fields are not filled
//!   in; the keywords are ignored.
//! - Constructs with no repair semantics (`allOf`, `patternProperties`,
//!   tuples via `prefixItems`, nested `oneOf` on fields, recursive `$ref`
//!   cycles) degrade to [`FieldKind::Any`] and are reported as
//!   [`ImportWarning`]s rather than silently dropped.

use crate::error::FuzzyError;
use crate::schema::{FieldKind, ObjectSchema, TaggedEnumSchema};
use serde_json::{Map, Value};

/// Result of a JSON Schema import: the converted schema plus warnings for
/// every construct that could not be mapped (degraded to [`FieldKind::Any`]).
#[derive(Debug, Clone)]
pub struct SchemaImport<S> {
    /// The converted repair schema
    pub schema: S,
    /// Constructs that were skipped or degraded during conversion
    pub warnings: Vec<ImportWarning>,
}

/// A construct in the source JSON Schema that could not be converted.
///
/// The affected field degrades to [`FieldKind::Any`] (no value repair)
/// instead of failing the whole import.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportWarning {
    /// Dotted path of the affected location in the source schema
    /// (e.g., `$.oneOf[2].properties.items`)
    pub path: String,
    /// What was skipped and why
    pub detail: String,
}

impl TaggedEnumSchema {
    /// Convert a JSON Schema document (e.g. `schemars` output) into a
    /// [`TaggedEnumSchema`].
    ///
    /// Expects a `oneOf` at the root whose branches share a tag property
    /// with a `const` string — the shape produced by
    /// `#[serde(tag = "type")]` (see the [module docs](crate::import) for
    /// the supported subset).
    ///
    /// # Example
    ///
    /// ```
    /// use fuzzy_parser::{TaggedEnumSchema, FieldKind};
    /// use serde_json::json;
    ///
    /// let json_schema = json!({
    ///     "oneOf": [
    ///         {
    ///             "type": "object",
    ///             "properties": {
    ///                 "type": {"type": "string", "const": "AddDerive"},
    ///                 "target": {"type": "string"},
    ///                 "derives": {"type": "array", "items": {"type": "string", "enum": ["Debug", "Clone"]}}
    ///             },
    ///             "required": ["type", "target"]
    ///         }
    ///     ]
    /// });
    ///
    /// let import = TaggedEnumSchema::from_json_schema(&json_schema).unwrap();
    /// assert!(import.schema.is_valid_tag("AddDerive"));
    /// assert!(import.warnings.is_empty());
    /// ```
    pub fn from_json_schema(root: &Value) -> Result<SchemaImport<TaggedEnumSchema>, FuzzyError> {
        let mut ctx = ImportCtx::new(root);
        let root_obj = ctx
            .resolve_schema_object(root, "$")
            .ok_or_else(|| FuzzyError::SchemaImport("root is not an object schema".into()))?;

        let Some(branches) = root_obj.get("oneOf").and_then(Value::as_array) else {
            if root_obj.contains_key("anyOf") {
                return Err(FuzzyError::SchemaImport(
                    "root uses `anyOf` — untagged enums are not supported; \
                     annotate the enum with #[serde(tag = \"...\")]"
                        .into(),
                ));
            }
            return Err(FuzzyError::SchemaImport(
                "expected `oneOf` at the schema root (internally or adjacently \
                 tagged enum); for plain objects use ObjectSchema::from_json_schema"
                    .into(),
            ));
        };

        // Resolve each branch (newtype variants carry a sibling $ref)
        let mut resolved = Vec::with_capacity(branches.len());
        for (i, branch) in branches.iter().enumerate() {
            let path = format!("$.oneOf[{}]", i);
            let obj = ctx.resolve_schema_object(branch, &path).ok_or_else(|| {
                FuzzyError::SchemaImport(format!("{} is not an object schema", path))
            })?;
            resolved.push(obj);
        }

        // Detect the tag field: a property present in every branch with a
        // const (or single-element enum) string value.
        let candidates = tag_candidates(&resolved);
        let tag_field = match candidates.len() {
            1 => candidates.into_iter().next().expect("len checked"),
            0 => {
                return Err(FuzzyError::SchemaImport(
                    "no common tag property with a `const` string was found across \
                     `oneOf` branches; externally tagged enums (serde's default \
                     representation) are not supported — annotate the enum with \
                     #[serde(tag = \"...\")]"
                        .into(),
                ))
            }
            _ => {
                return Err(FuzzyError::SchemaImport(format!(
                    "ambiguous tag field: multiple const properties are shared by \
                     every `oneOf` branch: {:?}",
                    candidates
                )))
            }
        };

        let mut schema = TaggedEnumSchema::with_tag(&tag_field);
        for (i, branch) in resolved.iter().enumerate() {
            let path = format!("$.oneOf[{}]", i);
            let tag_value = branch
                .get("properties")
                .and_then(Value::as_object)
                .and_then(|props| props.get(&tag_field))
                .and_then(tag_string)
                .expect("tag candidates verified per branch");
            if schema.is_valid_tag(&tag_value) {
                ctx.warn(&path, format!("duplicate tag value `{}`; branch replaces the earlier one", tag_value));
            }
            let variant = ctx.convert_object(branch, &path, Some(&tag_field));
            schema = schema.with_variant(tag_value, variant);
        }

        Ok(SchemaImport {
            schema,
            warnings: ctx.warnings,
        })
    }
}

impl ObjectSchema {
    /// Convert a JSON Schema document for a plain (non-enum) object into
    /// an [`ObjectSchema`].
    ///
    /// # Example
    ///
    /// ```
    /// use fuzzy_parser::{FieldKind, ObjectSchema};
    /// use serde_json::json;
    ///
    /// let json_schema = json!({
    ///     "type": "object",
    ///     "properties": {
    ///         "name": {"type": "string"},
    ///         "timeout": {"type": "integer"}
    ///     }
    /// });
    ///
    /// let import = ObjectSchema::from_json_schema(&json_schema).unwrap();
    /// assert_eq!(import.schema.kind_of("timeout"), Some(&FieldKind::Integer));
    /// ```
    pub fn from_json_schema(root: &Value) -> Result<SchemaImport<ObjectSchema>, FuzzyError> {
        let mut ctx = ImportCtx::new(root);
        let root_obj = ctx
            .resolve_schema_object(root, "$")
            .ok_or_else(|| FuzzyError::SchemaImport("root is not an object schema".into()))?;

        if !root_obj.contains_key("properties") {
            return Err(FuzzyError::SchemaImport(
                "expected an object schema with `properties` at the root".into(),
            ));
        }

        let schema = ctx.convert_object(&root_obj, "$", None);
        Ok(SchemaImport {
            schema,
            warnings: ctx.warnings,
        })
    }
}

#[cfg(feature = "schemars")]
impl TaggedEnumSchema {
    /// Derive a [`TaggedEnumSchema`] from a `#[derive(JsonSchema)]` type.
    ///
    /// The type must serialize as an internally or adjacently tagged enum
    /// (`#[serde(tag = "...")]`). Requires the `schemars` cargo feature.
    pub fn from_type<T: schemars::JsonSchema>() -> Result<SchemaImport<TaggedEnumSchema>, FuzzyError>
    {
        let json_schema = schemars::schema_for!(T);
        let value = serde_json::to_value(&json_schema)?;
        Self::from_json_schema(&value)
    }
}

#[cfg(feature = "schemars")]
impl ObjectSchema {
    /// Derive an [`ObjectSchema`] from a `#[derive(JsonSchema)]` struct.
    ///
    /// Requires the `schemars` cargo feature.
    pub fn from_type<T: schemars::JsonSchema>() -> Result<SchemaImport<ObjectSchema>, FuzzyError> {
        let json_schema = schemars::schema_for!(T);
        let value = serde_json::to_value(&json_schema)?;
        Self::from_json_schema(&value)
    }
}

// ============================================================================
// Conversion internals
// ============================================================================

/// Conversion state: the schema document (for `$ref` resolution), the
/// accumulated warnings, and the active `$ref` stack (cycle guard).
struct ImportCtx<'a> {
    root: &'a Value,
    warnings: Vec<ImportWarning>,
    ref_stack: Vec<String>,
}

impl<'a> ImportCtx<'a> {
    fn new(root: &'a Value) -> Self {
        Self {
            root,
            warnings: Vec::new(),
            ref_stack: Vec::new(),
        }
    }

    fn warn(&mut self, path: &str, detail: impl Into<String>) {
        self.warnings.push(ImportWarning {
            path: path.to_string(),
            detail: detail.into(),
        });
    }

    /// Look up an internal `$ref` (`#/$defs/Name` or `#/definitions/Name`).
    fn lookup_ref(&self, reference: &str) -> Option<Value> {
        let pointer = reference.strip_prefix('#')?;
        self.root.pointer(pointer).cloned()
    }

    /// Fully resolve a schema node into an object map, following `$ref`
    /// chains and merging sibling keywords (Draft 2020-12 allows `$ref`
    /// alongside other keywords). Returns `None` on cycles, external refs,
    /// or non-object schemas (a warning is recorded).
    fn resolve_schema_object(&mut self, value: &Value, path: &str) -> Option<Map<String, Value>> {
        let mut current = value.clone();
        let mut visited: Vec<String> = Vec::new();

        loop {
            let Value::Object(obj) = &current else {
                self.warn(path, "not an object schema");
                return None;
            };
            let Some(reference) = obj.get("$ref").and_then(Value::as_str).map(str::to_string)
            else {
                return Some(obj.clone());
            };
            if visited.contains(&reference) {
                self.warn(path, format!("recursive $ref `{}` cut", reference));
                return None;
            }
            let Some(target) = self.lookup_ref(&reference) else {
                self.warn(path, format!("unresolvable $ref `{}`", reference));
                return None;
            };
            let merged = merge_sibling_ref(&target, obj);
            visited.push(reference);
            current = merged;
        }
    }

    /// Convert an (already resolved) object schema's `properties` into an
    /// [`ObjectSchema`], skipping the tag field if given.
    fn convert_object(
        &mut self,
        obj: &Map<String, Value>,
        path: &str,
        skip: Option<&str>,
    ) -> ObjectSchema {
        for keyword in ["allOf", "patternProperties", "if", "not"] {
            if obj.contains_key(keyword) {
                self.warn(path, format!("unsupported keyword `{}` ignored", keyword));
            }
        }

        let mut schema = ObjectSchema::empty();
        if let Some(props) = obj.get("properties").and_then(Value::as_object) {
            for (name, prop) in props {
                if Some(name.as_str()) == skip {
                    continue;
                }
                let field_path = format!("{}.properties.{}", path, name);
                let kind = self.convert_kind(prop, &field_path);
                schema = schema.with_field_kind(name, kind);
            }
        }
        schema
    }

    /// Convert a schema node describing a field *value* into a [`FieldKind`].
    fn convert_kind(&mut self, prop: &Value, path: &str) -> FieldKind {
        let Some(obj) = prop.as_object() else {
            // `true` / `false` schemas: accept anything / nothing — no repair
            return FieldKind::Any;
        };

        // Follow $ref (with sibling merge and cycle guard)
        if let Some(reference) = obj.get("$ref").and_then(Value::as_str).map(str::to_string) {
            if self.ref_stack.contains(&reference) {
                self.warn(path, format!("recursive $ref `{}` cut to Any", reference));
                return FieldKind::Any;
            }
            let Some(target) = self.lookup_ref(&reference) else {
                self.warn(path, format!("unresolvable $ref `{}`; left as Any", reference));
                return FieldKind::Any;
            };
            let merged = merge_sibling_ref(&target, obj);
            self.ref_stack.push(reference);
            let kind = self.convert_kind(&merged, path);
            self.ref_stack.pop();
            return kind;
        }

        // Closed string sets
        if let Some(values) = obj.get("enum").and_then(Value::as_array) {
            if let Some(strings) = all_strings(values) {
                return FieldKind::Enum(strings);
            }
            self.warn(path, "`enum` with non-string values; left as Any");
            return FieldKind::Any;
        }
        if let Some(constant) = obj.get("const") {
            if let Some(s) = constant.as_str() {
                return FieldKind::Enum(vec![s.to_string()]);
            }
            return FieldKind::Any;
        }

        // Compositions: unwrap Option<T>-style nullables, reject the rest
        for keyword in ["anyOf", "oneOf"] {
            if let Some(branches) = obj.get(keyword).and_then(Value::as_array) {
                if let Some(inner) = nullable_unwrap(branches) {
                    return self.convert_kind(inner, path);
                }
                self.warn(
                    path,
                    format!(
                        "`{}` composition on a field (e.g. a nested tagged enum) \
                         is not supported; left as Any",
                        keyword
                    ),
                );
                return FieldKind::Any;
            }
        }

        match type_of(obj) {
            Some("string") => FieldKind::String,
            Some("integer") => FieldKind::Integer,
            Some("number") => FieldKind::Number,
            Some("boolean") => FieldKind::Bool,
            Some("object") => FieldKind::Object(self.convert_object(obj, path, None)),
            Some("array") => {
                if obj.contains_key("prefixItems") {
                    self.warn(path, "tuple (`prefixItems`) is not supported; left as Any");
                    return FieldKind::Any;
                }
                let Some(items) = obj.get("items") else {
                    return FieldKind::Any;
                };
                match self.convert_kind(items, &format!("{}.items", path)) {
                    FieldKind::Object(inner) => FieldKind::ObjectArray(inner),
                    FieldKind::Enum(values) => FieldKind::EnumArray(values),
                    // Arrays of plain scalars: nothing to repair per-element
                    _ => FieldKind::Any,
                }
            }
            // No / unknown type (accept-anything schema): no value repair
            _ => FieldKind::Any,
        }
    }
}

/// Merge a `$ref` target with the sibling keywords of the referencing node
/// (Draft 2020-12 sibling-`$ref` composition). Sibling keys win; the
/// `properties` maps are unioned.
fn merge_sibling_ref(target: &Value, sibling: &Map<String, Value>) -> Value {
    let mut merged = target.clone();
    let Value::Object(out) = &mut merged else {
        // Target is a bool schema: the siblings alone carry the shape
        let mut obj = sibling.clone();
        obj.remove("$ref");
        return Value::Object(obj);
    };

    for (key, value) in sibling {
        if key == "$ref" {
            continue;
        }
        match (out.get_mut(key), value) {
            (Some(Value::Object(dst)), Value::Object(src)) if key == "properties" => {
                for (prop_name, prop_value) in src {
                    dst.insert(prop_name.clone(), prop_value.clone());
                }
            }
            _ => {
                out.insert(key.clone(), value.clone());
            }
        }
    }
    merged
}

/// Extract the tag string from a tag property schema: `const` or a
/// single-element `enum`.
fn tag_string(prop: &Value) -> Option<String> {
    if let Some(s) = prop.get("const").and_then(Value::as_str) {
        return Some(s.to_string());
    }
    if let Some(values) = prop.get("enum").and_then(Value::as_array) {
        if let [single] = values.as_slice() {
            return single.as_str().map(str::to_string);
        }
    }
    None
}

/// Property names that carry a tag string in *every* branch, in the first
/// branch's (deterministic) property order.
fn tag_candidates(branches: &[Map<String, Value>]) -> Vec<String> {
    let Some(first) = branches.first() else {
        return Vec::new();
    };
    let Some(first_props) = first.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };

    first_props
        .iter()
        .filter(|(_, prop)| tag_string(prop).is_some())
        .map(|(name, _)| name.clone())
        .filter(|name| {
            branches.iter().skip(1).all(|branch| {
                branch
                    .get("properties")
                    .and_then(Value::as_object)
                    .and_then(|props| props.get(name))
                    .and_then(tag_string)
                    .is_some()
            })
        })
        .collect()
}

/// Unwrap `Option<T>`-style nullable compositions: exactly two branches,
/// one of which is `{"type": "null"}`.
fn nullable_unwrap(branches: &[Value]) -> Option<&Value> {
    let is_null =
        |v: &Value| v.get("type").and_then(Value::as_str) == Some("null");
    match branches {
        [a, b] if is_null(a) && !is_null(b) => Some(b),
        [a, b] if is_null(b) && !is_null(a) => Some(a),
        _ => None,
    }
}

/// Read the `type` keyword, tolerating the `["T", "null"]` nullable form.
fn type_of(obj: &Map<String, Value>) -> Option<&str> {
    match obj.get("type")? {
        Value::String(s) => Some(s.as_str()),
        Value::Array(types) => {
            let non_null: Vec<&str> = types
                .iter()
                .filter_map(Value::as_str)
                .filter(|t| *t != "null")
                .collect();
            match non_null.as_slice() {
                [single] => Some(single),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Collect an all-string JSON array into owned strings.
fn all_strings(values: &[Value]) -> Option<Vec<String>> {
    values
        .iter()
        .map(|v| v.as_str().map(str::to_string))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repair::{repair_tagged_enum_json, FuzzyOptions};
    use serde_json::json;

    /// The shape schemars emits for `#[serde(tag = "tag")]` (internally
    /// tagged): unit variant, struct variant, and a newtype variant with a
    /// sibling `$ref` (verbatim structure from the schemars integration
    /// snapshot for Draft 2020-12).
    fn internally_tagged_schema() -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Internal",
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "tag": {"type": "string", "const": "UnitOne"}
                    },
                    "required": ["tag"]
                },
                {
                    "type": "object",
                    "properties": {
                        "foo": {"type": "integer", "format": "int32"},
                        "bar": {"type": "boolean"},
                        "tag": {"type": "string", "const": "Struct"}
                    },
                    "required": ["tag", "foo", "bar"]
                },
                {
                    "type": "object",
                    "properties": {
                        "tag": {"type": "string", "const": "StructNewType"}
                    },
                    "$ref": "#/$defs/Struct",
                    "required": ["tag"]
                }
            ],
            "$defs": {
                "Struct": {
                    "type": "object",
                    "properties": {
                        "foo": {"type": "integer", "format": "int32"},
                        "bar": {"type": "boolean"}
                    },
                    "required": ["foo", "bar"]
                }
            }
        })
    }

    #[test]
    fn test_import_internally_tagged() {
        let import = TaggedEnumSchema::from_json_schema(&internally_tagged_schema()).unwrap();
        let schema = &import.schema;

        assert_eq!(schema.tag_field, "tag");
        assert!(schema.is_valid_tag("UnitOne"));
        assert!(schema.is_valid_tag("Struct"));
        assert!(schema.is_valid_tag("StructNewType"));
        assert!(import.warnings.is_empty());

        // Struct variant: fields mapped with coercion kinds
        let variant = schema.variant_schema("Struct").unwrap();
        assert_eq!(variant.kind_of("foo"), Some(&FieldKind::Integer));
        assert_eq!(variant.kind_of("bar"), Some(&FieldKind::Bool));
        // Tag field is not part of the variant fields
        assert!(!variant.is_valid_field("tag"));

        // Newtype variant: sibling $ref resolved into the same fields
        let newtype = schema.variant_schema("StructNewType").unwrap();
        assert_eq!(newtype.kind_of("foo"), Some(&FieldKind::Integer));
    }

    #[test]
    fn test_import_then_repair_end_to_end() {
        let import = TaggedEnumSchema::from_json_schema(&internally_tagged_schema()).unwrap();

        // Tag typo + field typo + string-encoded integer
        let llm_json = r#"{"tag": "Strct", "fo": "42", "bar": true}"#;
        let result =
            repair_tagged_enum_json(llm_json, &import.schema, &FuzzyOptions::default()).unwrap();

        assert_eq!(result.repaired["tag"], "Struct");
        assert_eq!(result.repaired["foo"], 42);
        assert_eq!(result.corrections.len(), 3); // tag + rename + coercion
    }

    #[test]
    fn test_import_adjacently_tagged() {
        // schemars output shape for #[serde(tag = "tag", content = "content")]
        let schema_doc = json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "tag": {"type": "string", "const": "Struct"},
                        "content": {
                            "type": "object",
                            "properties": {
                                "foo": {"type": "integer"},
                                "bar": {"type": "boolean"}
                            }
                        }
                    },
                    "required": ["tag", "content"]
                }
            ]
        });

        let import = TaggedEnumSchema::from_json_schema(&schema_doc).unwrap();
        let variant = import.schema.variant_schema("Struct").unwrap();
        let Some(FieldKind::Object(content)) = variant.kind_of("content") else {
            panic!("expected content to map to an Object kind");
        };
        assert_eq!(content.kind_of("foo"), Some(&FieldKind::Integer));

        // Repair reaches inside the content payload
        let llm_json = r#"{"tag": "Struct", "content": {"fo": 1, "bar": false}}"#;
        let result =
            repair_tagged_enum_json(llm_json, &import.schema, &FuzzyOptions::default()).unwrap();
        assert!(result.repaired["content"].get("foo").is_some());
    }

    #[test]
    fn test_import_externally_tagged_rejected() {
        // serde's default representation: variant name as the only key
        let schema_doc = json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": {"StringNewType": {"type": "string"}},
                    "additionalProperties": false,
                    "required": ["StringNewType"]
                },
                {
                    "type": "object",
                    "properties": {"StructVariant": {"type": "object"}},
                    "additionalProperties": false,
                    "required": ["StructVariant"]
                }
            ]
        });

        let err = TaggedEnumSchema::from_json_schema(&schema_doc).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("serde(tag"), "got: {}", message);
    }

    #[test]
    fn test_import_untagged_rejected() {
        let schema_doc = json!({
            "anyOf": [
                {"type": "null"},
                {"type": "integer"},
                {"type": "object", "properties": {"foo": {"type": "integer"}}}
            ]
        });

        let err = TaggedEnumSchema::from_json_schema(&schema_doc).unwrap_err();
        assert!(err.to_string().contains("untagged"));
    }

    #[test]
    fn test_import_ambiguous_tag_rejected() {
        // Two const properties shared by every branch
        let schema_doc = json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "type": {"type": "string", "const": "A"},
                        "version": {"type": "string", "const": "1"}
                    }
                },
                {
                    "type": "object",
                    "properties": {
                        "type": {"type": "string", "const": "B"},
                        "version": {"type": "string", "const": "1"}
                    }
                }
            ]
        });

        let err = TaggedEnumSchema::from_json_schema(&schema_doc).unwrap_err();
        assert!(err.to_string().contains("ambiguous"));
    }

    #[test]
    fn test_import_recursive_ref_cut_with_warning() {
        // struct Node { name: String, children: Vec<Node> }
        let schema_doc = json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "type": {"type": "string", "const": "Tree"},
                        "root": {"$ref": "#/$defs/Node"}
                    }
                }
            ],
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "children": {"type": "array", "items": {"$ref": "#/$defs/Node"}}
                    }
                }
            }
        });

        let import = TaggedEnumSchema::from_json_schema(&schema_doc).unwrap();
        // One level of Node is converted...
        let variant = import.schema.variant_schema("Tree").unwrap();
        let Some(FieldKind::Object(node)) = variant.kind_of("root") else {
            panic!("expected root to map to an Object kind");
        };
        assert_eq!(node.kind_of("name"), Some(&FieldKind::String));
        // ...and the self-reference is cut to Any with a warning
        assert_eq!(node.kind_of("children"), Some(&FieldKind::Any));
        assert!(import
            .warnings
            .iter()
            .any(|w| w.detail.contains("recursive $ref")));
    }

    #[test]
    fn test_import_option_nullable_unwrap() {
        let schema_doc = json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "type": {"type": "string", "const": "A"},
                        "maybe_count": {"anyOf": [{"type": "integer"}, {"type": "null"}]},
                        "maybe_level": {"type": ["string", "null"]}
                    }
                }
            ]
        });

        let import = TaggedEnumSchema::from_json_schema(&schema_doc).unwrap();
        let variant = import.schema.variant_schema("A").unwrap();
        assert_eq!(variant.kind_of("maybe_count"), Some(&FieldKind::Integer));
        assert_eq!(variant.kind_of("maybe_level"), Some(&FieldKind::String));
        assert!(import.warnings.is_empty());
    }

    #[test]
    fn test_import_enum_and_object_arrays() {
        let schema_doc = json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "type": {"type": "string", "const": "Batch"},
                        "derives": {
                            "type": "array",
                            "items": {"type": "string", "enum": ["Debug", "Clone"]}
                        },
                        "items": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {"path": {"type": "string"}}
                            }
                        },
                        "scores": {"type": "array", "items": {"type": "number"}}
                    }
                }
            ]
        });

        let import = TaggedEnumSchema::from_json_schema(&schema_doc).unwrap();
        let variant = import.schema.variant_schema("Batch").unwrap();
        assert_eq!(
            variant.kind_of("derives"),
            Some(&FieldKind::enum_array(["Debug", "Clone"]))
        );
        assert!(matches!(
            variant.kind_of("items"),
            Some(FieldKind::ObjectArray(_))
        ));
        // Arrays of plain scalars: nothing to repair per-element
        assert_eq!(variant.kind_of("scores"), Some(&FieldKind::Any));
    }

    #[test]
    fn test_import_nested_oneof_degrades_with_warning() {
        let schema_doc = json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "type": {"type": "string", "const": "A"},
                        "nested": {
                            "oneOf": [
                                {"type": "object", "properties": {"kind": {"const": "X"}}},
                                {"type": "object", "properties": {"kind": {"const": "Y"}}}
                            ]
                        }
                    }
                }
            ]
        });

        let import = TaggedEnumSchema::from_json_schema(&schema_doc).unwrap();
        let variant = import.schema.variant_schema("A").unwrap();
        assert_eq!(variant.kind_of("nested"), Some(&FieldKind::Any));
        assert!(import
            .warnings
            .iter()
            .any(|w| w.path.contains("nested") && w.detail.contains("oneOf")));
    }

    #[test]
    fn test_import_unresolvable_ref_degrades_with_warning() {
        let schema_doc = json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "type": {"type": "string", "const": "A"},
                        "ext": {"$ref": "https://example.com/other.json"}
                    }
                }
            ]
        });

        let import = TaggedEnumSchema::from_json_schema(&schema_doc).unwrap();
        let variant = import.schema.variant_schema("A").unwrap();
        assert_eq!(variant.kind_of("ext"), Some(&FieldKind::Any));
        assert!(import
            .warnings
            .iter()
            .any(|w| w.detail.contains("unresolvable $ref")));
    }

    #[test]
    fn test_import_plain_object_schema() {
        let schema_doc = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "timeout": {"type": "integer"},
                "level": {"type": "string", "enum": ["debug", "info"]}
            },
            "required": ["name"]
        });

        let import = ObjectSchema::from_json_schema(&schema_doc).unwrap();
        assert_eq!(import.schema.kind_of("timeout"), Some(&FieldKind::Integer));
        assert_eq!(
            import.schema.kind_of("level"),
            Some(&FieldKind::enum_of(["debug", "info"]))
        );
        assert!(import.warnings.is_empty());
    }

    #[test]
    fn test_import_plain_object_rejects_non_object() {
        let schema_doc = json!({"type": "string"});
        assert!(ObjectSchema::from_json_schema(&schema_doc).is_err());
    }

    #[test]
    fn test_import_legacy_definitions_ref() {
        // Draft-07 style `definitions` instead of `$defs`
        let schema_doc = json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "type": {"type": "string", "const": "A"},
                        "inner": {"$ref": "#/definitions/Inner"}
                    }
                }
            ],
            "definitions": {
                "Inner": {
                    "type": "object",
                    "properties": {"value": {"type": "number"}}
                }
            }
        });

        let import = TaggedEnumSchema::from_json_schema(&schema_doc).unwrap();
        let variant = import.schema.variant_schema("A").unwrap();
        let Some(FieldKind::Object(inner)) = variant.kind_of("inner") else {
            panic!("expected inner to map to an Object kind");
        };
        assert_eq!(inner.kind_of("value"), Some(&FieldKind::Number));
    }
}

#[cfg(all(test, feature = "schemars"))]
mod schemars_tests {
    use crate::repair::{repair_tagged_enum_json, FuzzyOptions};
    use crate::schema::{FieldKind, ObjectSchema, TaggedEnumSchema};

    #[derive(serde::Serialize, schemars::JsonSchema)]
    #[serde(tag = "type")]
    #[allow(dead_code)]
    enum Intent {
        AddDerive {
            target: String,
            count: i32,
        },
        Rename {
            from: String,
            to: String,
        },
    }

    #[test]
    fn test_from_type_internally_tagged() {
        let import = TaggedEnumSchema::from_type::<Intent>().unwrap();
        let schema = &import.schema;

        assert_eq!(schema.tag_field, "type");
        assert!(schema.is_valid_tag("AddDerive"));
        assert!(schema.is_valid_tag("Rename"));

        let variant = schema.variant_schema("AddDerive").unwrap();
        assert_eq!(variant.kind_of("target"), Some(&FieldKind::String));
        assert_eq!(variant.kind_of("count"), Some(&FieldKind::Integer));

        // End to end: derive → import → repair
        let llm_json = r#"{"type": "AddDeriv", "taget": "User", "count": "3"}"#;
        let result =
            repair_tagged_enum_json(llm_json, schema, &FuzzyOptions::default()).unwrap();
        assert_eq!(result.repaired["type"], "AddDerive");
        assert_eq!(result.repaired["target"], "User");
        assert_eq!(result.repaired["count"], 3);
    }

    #[derive(schemars::JsonSchema)]
    #[allow(dead_code)]
    struct Config {
        name: String,
        timeout: u32,
        enabled: bool,
    }

    #[test]
    fn test_from_type_plain_struct() {
        let import = ObjectSchema::from_type::<Config>().unwrap();
        assert_eq!(import.schema.kind_of("timeout"), Some(&FieldKind::Integer));
        assert_eq!(import.schema.kind_of("enabled"), Some(&FieldKind::Bool));
    }
}
