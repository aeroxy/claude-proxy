//! `DocumentCompactor` — recursive walker. The live compactor path
//! uses `compact()` directly; the document-wide walker is staged.
#![allow(dead_code)]

//! `DocumentCompactor` — recursive walker that finds compactable spots
//! anywhere in a JSON document and replaces them in place.

use serde_json::{Map, Value};

use super::compactor::{compact, CompactConfig};
use super::formatter::{CsvSchemaFormatter, Formatter};

pub struct DocumentCompactor {
    pub config: CompactConfig,
    pub formatter: Box<dyn Formatter>,
}

impl Default for DocumentCompactor {
    fn default() -> Self {
        Self {
            config: CompactConfig::default(),
            formatter: Box::new(CsvSchemaFormatter::new()),
        }
    }
}

impl DocumentCompactor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_formatter(mut self, formatter: Box<dyn Formatter>) -> Self {
        self.formatter = formatter;
        self
    }

    pub fn with_config(mut self, config: CompactConfig) -> Self {
        self.config = config;
        self
    }

    pub fn compact(&self, doc: Value) -> Value {
        walk(doc, self).0
    }
}

fn walk(v: Value, ctx: &DocumentCompactor) -> (Value, bool) {
    match v {
        Value::Object(map) => walk_object(map, ctx),
        Value::Array(items) => walk_array(items, ctx),
        Value::String(s) => walk_string(s, ctx),
        scalar => (scalar, false),
    }
}

fn walk_object(map: Map<String, Value>, ctx: &DocumentCompactor) -> (Value, bool) {
    let mut modified = false;
    let mut new_map = Map::new();
    for (k, v) in map {
        let (new_v, val_modified) = walk(v, ctx);
        if val_modified {
            modified = true;
        }
        new_map.insert(k, new_v);
    }
    (Value::Object(new_map), modified)
}

fn walk_array(items: Vec<Value>, ctx: &DocumentCompactor) -> (Value, bool) {
    let mut modified = false;
    let mut inner = Vec::with_capacity(items.len());
    for i in items {
        let (new_i, item_modified) = walk(i, ctx);
        if item_modified {
            modified = true;
        }
        inner.push(new_i);
    }
    let c = compact(&inner, &ctx.config);
    if c.was_compacted() {
        (Value::String(ctx.formatter.format(&c)), true)
    } else {
        (Value::Array(inner), modified)
    }
}

fn walk_string(s: String, ctx: &DocumentCompactor) -> (Value, bool) {
    if let Some(parsed) = try_parse_json_container(&s) {
        let (recursed, inner_modified) = walk(parsed, ctx);
        if !inner_modified {
            return (Value::String(s), false);
        }
        let rendered = match recursed {
            Value::String(rendered) => rendered,
            other => serde_json::to_string(&other).unwrap_or(s),
        };
        return (Value::String(rendered), true);
    }

    (Value::String(s), false)
}

/// Parse a string as JSON IF it looks like a container (starts with `{`
/// or `[`) AND parses cleanly to Object/Array. Returns None otherwise.
pub fn try_parse_json_container(s: &str) -> Option<Value> {
    let trimmed = s.trim_start();
    if !matches!(trimmed.as_bytes().first(), Some(&b'{') | Some(&b'[')) {
        return None;
    }
    serde_json::from_str::<Value>(s)
        .ok()
        .filter(|v| matches!(v, Value::Object(_) | Value::Array(_)))
}

pub fn compact_document(doc: Value) -> Value {
    DocumentCompactor::new().compact(doc)
}
