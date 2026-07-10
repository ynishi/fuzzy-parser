//! Schema definitions for fuzzy repair
//!
//! This module provides schema types that callers use to define
//! valid field names, type discriminators, and expected value shapes
//! for fuzzy matching and coercion.
//!
//! # Design
//!
//! Schemas are built from three layers:
//!
//! - [`FieldKind`] — the expected shape of a single field *value*
//!   (enum set, nested object, coercion target, ...). Nested kinds make
//!   the schema recursive: objects inside arrays inside objects can all
//!   be described and repaired.
//! - [`ObjectSchema`] — a set of named fields ([`FieldDef`]) for one object.
//! - [`TaggedEnumSchema`] — a discriminated union: a tag field selects
//!   which [`ObjectSchema`] applies.
//!
//! All schema types own their strings, so schemas can be built at runtime
//! (e.g. from a config file or an API definition), not only from `'static`
//! literals.

/// Expected shape of a single field value.
///
/// After a field's *name* has been repaired, its `FieldKind` decides what
/// repair (if any) is applied to the field's *value*:
///
/// - Fuzzy correction against a closed set ([`FieldKind::Enum`],
///   [`FieldKind::EnumArray`])
/// - Recursive repair with a nested schema ([`FieldKind::Object`],
///   [`FieldKind::ObjectArray`])
/// - Type coercion of string-encoded scalars ([`FieldKind::Integer`],
///   [`FieldKind::Number`], [`FieldKind::Bool`], [`FieldKind::String`])
///
/// Values that don't match the expected shape (e.g. a non-parseable string
/// for [`FieldKind::Integer`]) are left untouched — no lossy repair is made.
///
/// This enum is `#[non_exhaustive]`: new repair kinds may be added in minor
/// releases, so external `match` expressions need a wildcard arm.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub enum FieldKind {
    /// No value repair; the value is left untouched.
    #[default]
    Any,
    /// A string constrained to a closed set of values; fuzzy-corrected.
    Enum(Vec<String>),
    /// An array of strings, each constrained to a closed set; fuzzy-corrected.
    EnumArray(Vec<String>),
    /// A nested object repaired recursively with its own schema.
    Object(ObjectSchema),
    /// An array of objects, each repaired recursively with the same schema.
    ObjectArray(ObjectSchema),
    /// A nested tagged enum (discriminated union) repaired with its own
    /// [`TaggedEnumSchema`]: tag value, field names, and field values.
    TaggedEnum(TaggedEnumSchema),
    /// An array of tagged enums, each repaired with the same schema
    /// (e.g. a list of DSL intents).
    TaggedEnumArray(TaggedEnumSchema),
    /// Coerce string-encoded integers to numbers (`"42"` → `42`).
    Integer,
    /// Coerce string-encoded numbers to numbers (`"4.2"` → `4.2`).
    Number,
    /// Coerce string-encoded booleans to booleans (`"true"` → `true`).
    Bool,
    /// Coerce scalar numbers / booleans to their string rendering (`42` → `"42"`).
    String,
}

impl FieldKind {
    /// Build an [`FieldKind::Enum`] from any iterator of string-likes.
    pub fn enum_of<I, S>(values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self::Enum(values.into_iter().map(|s| s.as_ref().to_string()).collect())
    }

    /// Build an [`FieldKind::EnumArray`] from any iterator of string-likes.
    pub fn enum_array<I, S>(values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self::EnumArray(values.into_iter().map(|s| s.as_ref().to_string()).collect())
    }
}

/// A named field with an expected value shape.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldDef {
    /// The field name (used for fuzzy field-name repair).
    pub name: String,
    /// The expected shape of the field's value.
    pub kind: FieldKind,
}

impl FieldDef {
    /// Create a new field definition.
    pub fn new(name: impl AsRef<str>, kind: FieldKind) -> Self {
        Self {
            name: name.as_ref().to_string(),
            kind,
        }
    }
}

/// Schema for an object with known fields.
///
/// Field *names* are fuzzy-repaired against the defined names; field
/// *values* are repaired according to each field's [`FieldKind`].
/// Because [`FieldKind`] can contain nested `ObjectSchema`s, repair
/// recurses to any depth.
///
/// # Example
///
/// ```
/// use fuzzy_parser::{FieldKind, ObjectSchema};
///
/// let schema = ObjectSchema::new(["name"])
///     .with_field_kind("timeout", FieldKind::Integer)
///     .with_field_kind("derives", FieldKind::enum_array(["Debug", "Clone"]))
///     .with_field_kind(
///         "inner",
///         FieldKind::Object(ObjectSchema::new(["host", "port"])),
///     );
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ObjectSchema {
    /// The defined fields.
    pub fields: Vec<FieldDef>,
}

impl ObjectSchema {
    /// Create a schema from field names (all fields accept any value shape).
    pub fn new<I, S>(valid_fields: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            fields: valid_fields
                .into_iter()
                .map(|name| FieldDef::new(name, FieldKind::Any))
                .collect(),
        }
    }

    /// Create an empty schema (add fields with the builder methods).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Add a field that accepts any value shape.
    pub fn with_field(self, name: impl AsRef<str>) -> Self {
        self.with_field_kind(name, FieldKind::Any)
    }

    /// Add a field with an expected value shape.
    ///
    /// If a field with the same name already exists, its kind is replaced.
    pub fn with_field_kind(mut self, name: impl AsRef<str>, kind: FieldKind) -> Self {
        let name = name.as_ref();
        if let Some(def) = self.fields.iter_mut().find(|d| d.name == name) {
            def.kind = kind;
        } else {
            self.fields.push(FieldDef::new(name, kind));
        }
        self
    }

    /// Check if a field name is valid.
    pub fn is_valid_field(&self, field: &str) -> bool {
        self.fields.iter().any(|d| d.name == field)
    }

    /// Iterate over the defined field names.
    pub fn field_names(&self) -> impl Iterator<Item = &str> {
        self.fields.iter().map(|d| d.name.as_str())
    }

    /// Get the expected value shape for a field, if defined.
    pub fn kind_of(&self, field: &str) -> Option<&FieldKind> {
        self.fields.iter().find(|d| d.name == field).map(|d| &d.kind)
    }
}

/// Schema for a tagged enum (discriminated union)
///
/// Used for types with a discriminator field (e.g., tag: "type", "kind").
/// Each valid tag value maps to an [`ObjectSchema`] describing that
/// variant's fields. Fields registered globally (via
/// [`with_enum_array`](Self::with_enum_array),
/// [`with_nested_object`](Self::with_nested_object) or
/// [`with_field_kind`](Self::with_field_kind)) apply to every variant.
///
/// # Example (static, closure-based)
///
/// ```
/// use fuzzy_parser::TaggedEnumSchema;
///
/// let schema = TaggedEnumSchema::new(
///     "type",
///     &["AddDerive", "RemoveDerive"],
///     |tag| match tag {
///         "AddDerive" | "RemoveDerive" => Some(&["target", "derives"][..]),
///         _ => None,
///     },
/// )
/// .with_enum_array("derives", &["Debug", "Clone", "Serialize"])
/// .with_nested_object("config", &["timeout", "retries"]);
/// ```
///
/// # Example (dynamic, built at runtime)
///
/// ```
/// use fuzzy_parser::{FieldKind, ObjectSchema, TaggedEnumSchema};
///
/// // Field names can come from runtime data (config, API spec, ...).
/// let variant = String::from("AddDerive");
/// let schema = TaggedEnumSchema::with_tag("type").with_variant(
///     variant,
///     ObjectSchema::new(["target"])
///         .with_field_kind("derives", FieldKind::enum_array(["Debug", "Clone"]))
///         .with_field_kind("timeout", FieldKind::Integer),
/// );
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaggedEnumSchema {
    /// The discriminator field name (e.g., "type", "kind")
    pub tag_field: String,
    /// The variants: (tag value, schema for that variant's fields)
    pub variants: Vec<(String, ObjectSchema)>,
    /// Fields that apply to every variant (checked after variant fields)
    pub global_fields: Vec<FieldDef>,
}

impl TaggedEnumSchema {
    /// Create a new tagged enum schema from a field-resolver closure.
    ///
    /// The closure is evaluated once per valid tag at construction time,
    /// so the schema itself owns its data and carries no generic parameter.
    pub fn new<F>(tag_field: impl AsRef<str>, valid_tags: &[&str], fields_for_tag: F) -> Self
    where
        F: Fn(&str) -> Option<&'static [&'static str]>,
    {
        let variants = valid_tags
            .iter()
            .map(|tag| {
                let fields = fields_for_tag(tag)
                    .map(|fs| ObjectSchema::new(fs.iter().copied()))
                    .unwrap_or_default();
                (tag.to_string(), fields)
            })
            .collect();
        Self {
            tag_field: tag_field.as_ref().to_string(),
            variants,
            global_fields: Vec::new(),
        }
    }

    /// Create an empty schema for dynamic construction.
    ///
    /// Add variants with [`with_variant`](Self::with_variant).
    pub fn with_tag(tag_field: impl AsRef<str>) -> Self {
        Self {
            tag_field: tag_field.as_ref().to_string(),
            variants: Vec::new(),
            global_fields: Vec::new(),
        }
    }

    /// Add (or replace) a variant with its field schema.
    pub fn with_variant(mut self, tag: impl AsRef<str>, schema: ObjectSchema) -> Self {
        let tag = tag.as_ref();
        if let Some(entry) = self.variants.iter_mut().find(|(t, _)| t == tag) {
            entry.1 = schema;
        } else {
            self.variants.push((tag.to_string(), schema));
        }
        self
    }

    /// Add a global enum array field for repair (applies to every variant).
    ///
    /// Values in this array field will be fuzzy-matched against `valid_values`.
    ///
    /// # Example
    ///
    /// ```
    /// use fuzzy_parser::TaggedEnumSchema;
    ///
    /// let schema = TaggedEnumSchema::new("type", &["AddDerive"], |_| Some(&["derives"][..]))
    ///     .with_enum_array("derives", &["Debug", "Clone", "Serialize"]);
    /// // Now "Debg" in derives array will be corrected to "Debug"
    /// ```
    pub fn with_enum_array<I, S>(self, field: impl AsRef<str>, valid_values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.with_field_kind(field, FieldKind::enum_array(valid_values))
    }

    /// Add a global nested object field for repair (applies to every variant).
    ///
    /// Field names in this nested object will be fuzzy-matched against
    /// `valid_fields`. For deeper nesting or value shapes inside the nested
    /// object, use [`with_field_kind`](Self::with_field_kind) with
    /// [`FieldKind::Object`] instead.
    ///
    /// # Example
    ///
    /// ```
    /// use fuzzy_parser::TaggedEnumSchema;
    ///
    /// let schema = TaggedEnumSchema::new("type", &["Configure"], |_| Some(&["config"][..]))
    ///     .with_nested_object("config", &["timeout", "retries", "enabled"]);
    /// // Now "timout" in config object will be corrected to "timeout"
    /// ```
    pub fn with_nested_object<I, S>(self, field: impl AsRef<str>, valid_fields: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.with_field_kind(field, FieldKind::Object(ObjectSchema::new(valid_fields)))
    }

    /// Add (or replace) a global field with an expected value shape.
    ///
    /// Global fields apply to every variant, after the variant's own
    /// field kinds.
    pub fn with_field_kind(mut self, field: impl AsRef<str>, kind: FieldKind) -> Self {
        let field = field.as_ref();
        if let Some(def) = self.global_fields.iter_mut().find(|d| d.name == field) {
            def.kind = kind;
        } else {
            self.global_fields.push(FieldDef::new(field, kind));
        }
        self
    }

    /// Check if a tag value is valid
    pub fn is_valid_tag(&self, tag: &str) -> bool {
        self.variants.iter().any(|(t, _)| t == tag)
    }

    /// Iterate over the valid tag values.
    pub fn tag_values(&self) -> impl Iterator<Item = &str> {
        self.variants.iter().map(|(t, _)| t.as_str())
    }

    /// Get the field schema for a tag value, if the tag is valid.
    pub fn variant_schema(&self, tag: &str) -> Option<&ObjectSchema> {
        self.variants.iter().find(|(t, _)| t == tag).map(|(_, s)| s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tagged_enum_schema() {
        let schema =
            TaggedEnumSchema::new("type", &["AddDerive", "RemoveDerive"], |tag| match tag {
                "AddDerive" => Some(&["target", "derives"]),
                "RemoveDerive" => Some(&["target", "derives"]),
                _ => None,
            });

        assert!(schema.is_valid_tag("AddDerive"));
        assert!(!schema.is_valid_tag("InvalidType"));
        let fields: Vec<&str> = schema
            .variant_schema("AddDerive")
            .unwrap()
            .field_names()
            .collect();
        assert_eq!(fields, vec!["target", "derives"]);
    }

    #[test]
    fn test_object_schema() {
        let schema = ObjectSchema::new(["name", "value", "is_pub"]);

        assert!(schema.is_valid_field("name"));
        assert!(!schema.is_valid_field("invalid"));
    }

    #[test]
    fn test_dynamic_schema_from_owned_strings() {
        // Schema built entirely from runtime (non-'static) data
        let tag_field = String::from("kind");
        let tags = vec![String::from("Create"), String::from("Delete")];
        let fields = vec![String::from("name"), String::from("path")];

        let mut schema = TaggedEnumSchema::with_tag(&tag_field);
        for tag in &tags {
            schema = schema.with_variant(tag, ObjectSchema::new(&fields));
        }

        assert!(schema.is_valid_tag("Create"));
        assert!(schema.variant_schema("Delete").unwrap().is_valid_field("path"));
    }

    #[test]
    fn test_with_field_kind_replaces_existing() {
        let schema = ObjectSchema::new(["timeout"])
            .with_field_kind("timeout", FieldKind::Integer);

        assert_eq!(schema.fields.len(), 1);
        assert_eq!(schema.kind_of("timeout"), Some(&FieldKind::Integer));
    }

    #[test]
    fn test_recursive_schema_shape() {
        let schema = ObjectSchema::empty().with_field_kind(
            "outer",
            FieldKind::Object(
                ObjectSchema::empty()
                    .with_field_kind("inner", FieldKind::Object(ObjectSchema::new(["leaf"]))),
            ),
        );

        let FieldKind::Object(outer) = schema.kind_of("outer").unwrap() else {
            panic!("expected object kind");
        };
        let FieldKind::Object(inner) = outer.kind_of("inner").unwrap() else {
            panic!("expected object kind");
        };
        assert!(inner.is_valid_field("leaf"));
    }
}
