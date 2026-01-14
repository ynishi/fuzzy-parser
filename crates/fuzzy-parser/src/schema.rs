//! Schema definitions for fuzzy repair
//!
//! This module provides schema types that callers use to define
//! valid field names and type discriminators for fuzzy matching.

/// Schema for a tagged enum (discriminated union)
///
/// Used for types with a discriminator field (e.g., tag: "type", "kind")
///
/// # Example
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
pub struct TaggedEnumSchema<F>
where
    F: Fn(&str) -> Option<&'static [&'static str]>,
{
    /// The discriminator field name (e.g., "type", "kind")
    pub tag_field: &'static str,
    /// Valid tag values (e.g., ["AddDerive", "RenameIdent", ...])
    pub valid_tags: &'static [&'static str],
    /// Function to get valid fields for a given tag value
    pub fields_for_tag: F,
    /// Fields that contain arrays of enum values: (field_name, valid_values)
    pub enum_arrays: Vec<(&'static str, &'static [&'static str])>,
    /// Fields that contain nested objects: (field_name, valid_fields)
    pub nested_objects: Vec<(&'static str, &'static [&'static str])>,
}

impl<F> TaggedEnumSchema<F>
where
    F: Fn(&str) -> Option<&'static [&'static str]>,
{
    /// Create a new tagged enum schema
    pub fn new(
        tag_field: &'static str,
        valid_tags: &'static [&'static str],
        fields_for_tag: F,
    ) -> Self {
        Self {
            tag_field,
            valid_tags,
            fields_for_tag,
            enum_arrays: Vec::new(),
            nested_objects: Vec::new(),
        }
    }

    /// Add an enum array field for repair
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
    pub fn with_enum_array(
        mut self,
        field: &'static str,
        valid_values: &'static [&'static str],
    ) -> Self {
        self.enum_arrays.push((field, valid_values));
        self
    }

    /// Add a nested object field for repair
    ///
    /// Field names in this nested object will be fuzzy-matched against `valid_fields`.
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
    pub fn with_nested_object(
        mut self,
        field: &'static str,
        valid_fields: &'static [&'static str],
    ) -> Self {
        self.nested_objects.push((field, valid_fields));
        self
    }

    /// Check if a tag value is valid
    pub fn is_valid_tag(&self, tag: &str) -> bool {
        self.valid_tags.contains(&tag)
    }

    /// Get valid fields for a tag value
    pub fn get_fields(&self, tag: &str) -> Option<&'static [&'static str]> {
        (self.fields_for_tag)(tag)
    }

    /// Get valid enum values for an array field
    pub fn get_enum_array_values(&self, field: &str) -> Option<&'static [&'static str]> {
        self.enum_arrays
            .iter()
            .find(|(f, _)| *f == field)
            .map(|(_, v)| *v)
    }

    /// Get valid fields for a nested object
    pub fn get_nested_object_fields(&self, field: &str) -> Option<&'static [&'static str]> {
        self.nested_objects
            .iter()
            .find(|(f, _)| *f == field)
            .map(|(_, v)| *v)
    }
}

/// Schema for a simple object with known fields
pub struct ObjectSchema {
    /// Valid field names
    pub valid_fields: &'static [&'static str],
}

impl ObjectSchema {
    /// Create a new object schema
    pub const fn new(valid_fields: &'static [&'static str]) -> Self {
        Self { valid_fields }
    }

    /// Check if a field name is valid
    pub fn is_valid_field(&self, field: &str) -> bool {
        self.valid_fields.contains(&field)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tagged_enum_schema() {
        let schema =
            TaggedEnumSchema::new("type", &["AddDerive", "RemoveDerive"], |tag| match tag {
                "AddDerive" => Some(&["target", "derives"][..]),
                "RemoveDerive" => Some(&["target", "derives"][..]),
                _ => None,
            });

        assert!(schema.is_valid_tag("AddDerive"));
        assert!(!schema.is_valid_tag("InvalidType"));
        assert_eq!(
            schema.get_fields("AddDerive"),
            Some(&["target", "derives"][..])
        );
    }

    #[test]
    fn test_object_schema() {
        let schema = ObjectSchema::new(&["name", "value", "is_pub"]);

        assert!(schema.is_valid_field("name"));
        assert!(!schema.is_valid_field("invalid"));
    }
}
