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
        walk(doc, self)
    }
}

fn walk(v: Value, ctx: &DocumentCompactor) -> Value {
    match v {
        Value::Object(map) => walk_object(map, ctx),
        Value::Array(items) => walk_array(items, ctx),
        Value::String(s) => walk_string(s, ctx),
        scalar => scalar,
    }
}

fn walk_object(map: Map<String, Value>, ctx: &DocumentCompactor) -> Value {
    Value::Object(map.into_iter().map(|(k, v)| (k, walk(v, ctx))).collect())
}

fn walk_array(items: Vec<Value>, ctx: &DocumentCompactor) -> Value {
    let inner: Vec<Value> = items.into_iter().map(|i| walk(i, ctx)).collect();
    let c = compact(&inner, &ctx.config);
    if c.was_compacted() {
        Value::String(ctx.formatter.format(&c))
    } else {
        Value::Array(inner)
    }
}

fn walk_string(s: String, ctx: &DocumentCompactor) -> Value {
    if let Some(parsed) = try_parse_json_container(&s) {
        let recursed = walk(parsed.clone(), ctx);
        if recursed == parsed {
            return Value::String(s);
        }
        return match recursed {
            Value::String(rendered) => Value::String(rendered),
            other => Value::String(serde_json::to_string(&other).unwrap_or(s)),
        };
    }

    Value::String(s)
}

/// Parse a string as JSON IF it looks like a container (starts with `{`
/// or `[`) AND parses cleanly to Object/Array. Returns None otherwise.
pub fn try_parse_json_container(s: &str) -> Option<Value> {
    let trimmed = s.trim_start();
    if !matches!(trimmed.chars().next(), Some('{') | Some('[')) {
        return None;
    }
    serde_json::from_str::<Value>(s)
        .ok()
        .filter(|v| matches!(v, Value::Object(_) | Value::Array(_)))
}

pub fn compact_document(doc: Value) -> Value {
    DocumentCompactor::new().compact(doc)
}
