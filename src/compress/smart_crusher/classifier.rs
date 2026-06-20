//! JSON array element-type classification.
//!
//! Classification drives compression strategy: dict arrays go through
//! `crush_array`, string arrays through `crush_string_array`, etc.
//!
//! # Type representation: bool vs int
//!
//! JSON treats booleans and numbers as distinct types, so
//! classification is highly robust and clean. We walk every element (not a sample) 
//! to guarantee correct classification on adversarial inputs.

use serde_json::Value;

/// JSON array element type classification.
///
/// String representation matches strategy debug output (e.g. `"dict_array(100->10)"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArrayType {
    /// `[{...}, {...}, ...]` — dict array, full statistical path.
    DictArray,
    /// `["a", "b", "c", ...]` — string array.
    StringArray,
    /// `[1, 2.5, 3, ...]` — number array (excludes bools).
    NumberArray,
    /// `[true, false, ...]` — pure bool array.
    BoolArray,
    /// `[[...], [...], ...]` — array of arrays.
    NestedArray,
    /// Anything else: heterogeneous or unclassifiable.
    MixedArray,
    /// `[]` — empty array.
    Empty,
}

impl ArrayType {
    /// Lowercase string representation.
    /// Used in strategy debug strings.
    pub fn as_str(self) -> &'static str {
        match self {
            ArrayType::DictArray => "dict_array",
            ArrayType::StringArray => "string_array",
            ArrayType::NumberArray => "number_array",
            ArrayType::BoolArray => "bool_array",
            ArrayType::NestedArray => "nested_array",
            ArrayType::MixedArray => "mixed_array",
            ArrayType::Empty => "empty",
        }
    }
}

/// Classify a JSON array by its element types.
///
/// Walks every element (not a sample) to guarantee correct classification
/// even on adversarial inputs where the first few items hide a type
/// transition deeper in the list. `Value::is_*` is O(1), so the full
/// walk is fine.
///
/// Returns `ArrayType::Empty` for an empty slice.
pub fn classify_array(items: &[Value]) -> ArrayType {
    if items.is_empty() {
        return ArrayType::Empty;
    }

    // Track which Value variants we've seen. We collapse Number into
    // either "int-like" or "float-like" once below; here we only need to
    // know whether there's at least one of each high-level kind.
    let mut has_bool = false;
    let mut has_number = false;
    let mut has_string = false;
    let mut has_object = false;
    let mut has_array = false;
    let mut has_null = false;

    for item in items {
        match item {
            Value::Bool(_) => has_bool = true,
            Value::Number(_) => has_number = true,
            Value::String(_) => has_string = true,
            Value::Object(_) => has_object = true,
            Value::Array(_) => has_array = true,
            Value::Null => has_null = true,
        }
    }

    // Pure bool array.
    if has_bool && !has_number && !has_string && !has_object && !has_array && !has_null {
        return ArrayType::BoolArray;
    }

    // Pure dict array.
    if has_object && !has_bool && !has_number && !has_string && !has_array && !has_null {
        return ArrayType::DictArray;
    }

    // Pure string array.
    if has_string && !has_bool && !has_number && !has_object && !has_array && !has_null {
        return ArrayType::StringArray;
    }

    // Pure number array — explicitly excludes bool here.
    if has_number && !has_bool && !has_string && !has_object && !has_array && !has_null {
        return ArrayType::NumberArray;
    }

    // Pure nested array.
    if has_array && !has_bool && !has_number && !has_string && !has_object && !has_null {
        return ArrayType::NestedArray;
    }

    // Anything else — heterogeneous types, or types involving null.
    ArrayType::MixedArray
}

