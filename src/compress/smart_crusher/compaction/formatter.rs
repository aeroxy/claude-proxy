//! Formatter trait + the built-in implementations.
//!
//! [`Formatter`] walks a [`Compaction`] tree and renders bytes. It's the
//! pluggable seam where users (or Enterprise plugins) choose how the
//! compacted output looks.
//!
//! # Built-ins
//!
//! - [`JsonFormatter`] — single-line / pretty JSON. Easy to parse,
//!   wider model familiarity, larger byte size. Default for the
//!   debugging path.
//! - [`CsvSchemaFormatter`] — `[N]{cols}` row-count-and-shape
//!   declaration + typed column header + CSV-escaped rows. Steals
//!   TOON's most useful idea (the `[N]{cols}` declaration) without
//!   adopting TOON's bespoke escaping rules — every model has seen
//!   millions of CSV examples in training.
//! - [`MarkdownKvFormatter`] — the same `[N]{cols}` declaration +
//!   one Markdown list item per row with `key: value` lines.
//!   Token-heavier than CSV (field names repeat per row) but
//!   format-comprehension benchmarks favor KV for read-back accuracy.
//!
//! # Nested cells
//!
//! The formatters handle [`CellValue::Nested`] by recursively
//! formatting the sub-compaction and embedding the result. The CSV
//! formatter wraps nested output in CSV-quoted form; the JSON
//! formatter embeds it as a structured JSON object.
//!
//! # Opaque cells
//!
//! [`CellValue::OpaqueRef`] renders as a structured marker the model
//! can recognize: `<<ccr:HASH,KIND,SIZE>>`. This format is fixed across
//! all built-in formatters so downstream consumers can pattern-match
//! markers regardless of which formatter produced them.

use serde_json::{json, Value};

use super::ir::{CellValue, Compaction, OpaqueKind, Row, Schema};

/// Format a `Compaction` tree into bytes.
pub trait Formatter: Send + Sync {
    /// Stable name for telemetry (e.g. `"json"`, `"csv-schema"`).
    fn name(&self) -> &str;

    /// Render the compaction. Implementations should be deterministic.
    fn format(&self, c: &Compaction) -> String;

    /// Cheap byte-size estimate. Default impl renders and measures.
    /// Override for cases where rendering is expensive.
    fn estimate_bytes(&self, c: &Compaction) -> usize {
        self.format(c).len()
    }
}

// ─────────────────────────── JSON formatter ───────────────────────────

/// Renders a `Compaction` as structured JSON. Single-line by default
/// for token-tight output; set `pretty = true` for human inspection.
#[derive(Debug, Clone, Default)]
pub struct JsonFormatter {
    pub pretty: bool,
}

impl JsonFormatter {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn pretty(mut self) -> Self {
        self.pretty = true;
        self
    }
}

impl Formatter for JsonFormatter {
    fn name(&self) -> &str {
        "json"
    }

    fn format(&self, c: &Compaction) -> String {
        let v = compaction_to_json(c);
        if self.pretty {
            serde_json::to_string_pretty(&v).unwrap_or_default()
        } else {
            serde_json::to_string(&v).unwrap_or_default()
        }
    }
}

fn compaction_to_json(c: &Compaction) -> Value {
    match c {
        Compaction::Table {
            schema,
            rows,
            original_count,
        } => json!({
            "_compaction": "table",
            "_schema": schema_to_json(schema),
            "_kept": rows.len(),
            "_total": original_count,
            "_rows": rows.iter().map(row_to_json).collect::<Vec<_>>(),
        }),
        Compaction::Buckets {
            discriminator,
            buckets,
            original_count,
        } => json!({
            "_compaction": "buckets",
            "_discriminator": discriminator,
            "_total": original_count,
            "_buckets": buckets
                .iter()
                .map(|b| json!({
                    "_key": b.key.clone(),
                    "_schema": schema_to_json(&b.schema),
                    "_rows": b.rows.iter().map(row_to_json).collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>(),
        }),
        Compaction::OpaqueRef {
            ccr_hash,
            byte_size,
            kind,
        } => json!({
            "_compaction": "ccr",
            "_hash": ccr_hash,
            "_size": byte_size,
            "_kind": opaque_kind_str(kind),
        }),
        Compaction::Untouched(v) => v.clone(),
    }
}

fn schema_to_json(s: &Schema) -> Value {
    Value::Array(
        s.fields
            .iter()
            .map(|f| {
                let mut obj = serde_json::Map::new();
                obj.insert("name".into(), Value::String(f.name.clone()));
                obj.insert("type".into(), Value::String(f.type_tag.clone()));
                if f.nullable {
                    obj.insert("nullable".into(), Value::Bool(true));
                }
                Value::Object(obj)
            })
            .collect(),
    )
}

fn row_to_json(row: &Row) -> Value {
    Value::Array(row.0.iter().map(cell_to_json).collect())
}

fn cell_to_json(c: &CellValue) -> Value {
    match c {
        CellValue::Scalar(v) => v.clone(),
        CellValue::Missing => Value::Null,
        CellValue::Nested(sub) => compaction_to_json(sub),
        CellValue::OpaqueRef {
            ccr_hash,
            byte_size,
            kind,
        } => json!({
            "_ccr": ccr_hash,
            "_size": byte_size,
            "_kind": opaque_kind_str(kind),
        }),
    }
}

fn opaque_kind_str(k: &OpaqueKind) -> String {
    match k {
        OpaqueKind::Base64Blob => "base64".into(),
        OpaqueKind::LongString => "string".into(),
        OpaqueKind::HtmlChunk => "html".into(),
        OpaqueKind::Other(s) => s.clone(),
    }
}

// ─────────────────────────── CSV+schema formatter ───────────────────────────

/// Renders a `Compaction` as `[N]{col1:type1,col2:type2}` declaration +
/// CSV-escaped rows. Nested cells render as JSON inline; opaque cells
/// render as `<<ccr:...>>` markers.
#[derive(Debug, Clone, Default)]
pub struct CsvSchemaFormatter {
    /// If true, emit a `__total:N` line when rows were dropped under
    /// budget. Costs a few bytes; useful for downstream telemetry.
    pub include_drop_summary: bool,
}

impl CsvSchemaFormatter {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_drop_summary(mut self) -> Self {
        self.include_drop_summary = true;
        self
    }
}

impl Formatter for CsvSchemaFormatter {
    fn name(&self) -> &str {
        "csv-schema"
    }

    fn format(&self, c: &Compaction) -> String {
        let mut out = String::new();
        write_compaction(&mut out, c, self);
        out
    }
}

fn write_compaction(out: &mut String, c: &Compaction, fmt: &CsvSchemaFormatter) {
    match c {
        Compaction::Table {
            schema,
            rows,
            original_count,
        } => {
            write_table(out, schema, rows, *original_count, fmt);
        }
        Compaction::Buckets {
            discriminator,
            buckets,
            original_count,
        } => {
            out.push_str("__buckets:");
            out.push_str(discriminator);
            if fmt.include_drop_summary {
                let kept: usize = buckets.iter().map(|b| b.rows.len()).sum();
                if kept < *original_count {
                    out.push_str(&format!(" __dropped:{}", original_count - kept));
                }
            }
            out.push('\n');
            for b in buckets {
                out.push_str(&format!("__key:{}\n", json_scalar_to_csv(&b.key)));
                write_table(out, &b.schema, &b.rows, b.rows.len(), fmt);
            }
        }
        Compaction::OpaqueRef {
            ccr_hash,
            byte_size,
            kind,
        } => {
            out.push_str(&format_ccr_marker(ccr_hash, *byte_size, kind));
        }
        Compaction::Untouched(v) => {
            out.push_str(&serde_json::to_string(v).unwrap_or_default());
        }
    }
}

fn write_table(
    out: &mut String,
    schema: &Schema,
    rows: &[Row],
    original_count: usize,
    fmt: &CsvSchemaFormatter,
) {
    // Declaration line: [N]{col:type,col:type,...}
    out.push('[');
    out.push_str(&rows.len().to_string());
    out.push_str("]{");
    let col_decl: Vec<String> = schema
        .fields
        .iter()
        .map(|f| {
            if f.nullable {
                format!("{}:{}?", f.name, f.type_tag)
            } else {
                format!("{}:{}", f.name, f.type_tag)
            }
        })
        .collect();
    out.push_str(&col_decl.join(","));
    out.push('}');
    if fmt.include_drop_summary && rows.len() < original_count {
        out.push_str(&format!(" __dropped:{}", original_count - rows.len()));
    }
    out.push('\n');

    // Rows.
    for row in rows {
        let cells: Vec<String> = row.0.iter().map(format_cell).collect();
        out.push_str(&cells.join(","));
        out.push('\n');
    }
}

fn format_cell(c: &CellValue) -> String {
    match c {
        CellValue::Missing => String::new(),
        CellValue::Scalar(v) => json_scalar_to_csv(v),
        CellValue::Nested(sub) => {
            // Render nested as compact JSON; CSV-quote because it
            // contains commas and structural chars.
            let nested_fmt = JsonFormatter::new();
            csv_quote(&nested_fmt.format(sub))
        }
        CellValue::OpaqueRef {
            ccr_hash,
            byte_size,
            kind,
        } => format_ccr_marker(ccr_hash, *byte_size, kind),
    }
}

fn format_ccr_marker(hash: &str, byte_size: usize, kind: &OpaqueKind) -> String {
    let kind_str = match kind {
        OpaqueKind::Base64Blob => "base64",
        OpaqueKind::LongString => "string",
        OpaqueKind::HtmlChunk => "html",
        OpaqueKind::Other(s) => s.as_str(),
    };
    format!(
        "<<ccr:{},{},{}>>",
        hash,
        kind_str,
        humanize_bytes(byte_size)
    )
}

fn humanize_bytes(n: usize) -> String {
    if n < 1024 {
        return format!("{n}B");
    }
    let kb = n as f64 / 1024.0;
    if kb < 1024.0 {
        return format!("{kb:.1}KB");
    }
    let mb = kb / 1024.0;
    format!("{mb:.1}MB")
}

fn json_scalar_to_csv(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => {
            if needs_csv_quote(s) {
                csv_quote(s)
            } else {
                s.clone()
            }
        }
        // Object/array fall back to JSON-quoted (rare — usually
        // already promoted to Nested by the compactor).
        _ => csv_quote(&serde_json::to_string(v).unwrap_or_default()),
    }
}

fn needs_csv_quote(s: &str) -> bool {
    s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r')
}

fn csv_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' {
            out.push('"');
            out.push('"');
        } else {
            out.push(c);
        }
    }
    out.push('"');
    out
}

// ─────────────────────────── Markdown-KV formatter ───────────────────────────

/// Renders a `Compaction` as a `[N]{cols}` declaration followed by one
/// Markdown list item per row, each cell on its own `key: value` line.
///
/// Token-heavier than [`CsvSchemaFormatter`] (field names repeat per
/// row), but format-comprehension benchmarks show models retrieve
/// values from Markdown-KV substantially more reliably than from CSV.
/// Offered as an opt-in trade of tokens for read accuracy.
///
/// Rendering rules:
/// - Missing cells are omitted entirely (no `key:` line) — sparse rows
///   cost nothing, unlike CSV's positional empty cells.
/// - Strings that would be ambiguous on a line (contain newlines,
///   leading/trailing whitespace, or are empty) render JSON-quoted;
///   everything else renders raw.
/// - Nested cells render as compact inline JSON, matching
///   [`CsvSchemaFormatter`].
/// - Opaque cells keep the fixed `<<ccr:HASH,KIND,SIZE>>` marker
///   contract shared by all formatters.
#[derive(Debug, Clone, Default)]
pub struct MarkdownKvFormatter {
    /// If true, emit a `__dropped:N` note on the declaration line when
    /// rows were dropped under budget. Mirrors
    /// [`CsvSchemaFormatter::include_drop_summary`].
    pub include_drop_summary: bool,
}

impl MarkdownKvFormatter {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_drop_summary(mut self) -> Self {
        self.include_drop_summary = true;
        self
    }
}

impl Formatter for MarkdownKvFormatter {
    fn name(&self) -> &str {
        "markdown-kv"
    }

    fn format(&self, c: &Compaction) -> String {
        let mut out = String::new();
        write_compaction_kv(&mut out, c, self);
        out
    }
}

fn write_compaction_kv(out: &mut String, c: &Compaction, fmt: &MarkdownKvFormatter) {
    match c {
        Compaction::Table {
            schema,
            rows,
            original_count,
        } => {
            write_kv_table(out, schema, rows, *original_count, fmt);
        }
        Compaction::Buckets {
            discriminator,
            buckets,
            original_count,
        } => {
            out.push_str("__buckets:");
            out.push_str(discriminator);
            if fmt.include_drop_summary {
                let kept: usize = buckets.iter().map(|b| b.rows.len()).sum();
                if kept < *original_count {
                    out.push_str(&format!(" __dropped:{}", original_count - kept));
                }
            }
            out.push('\n');
            for b in buckets {
                out.push_str(&format!("__key:{}\n", kv_scalar(&b.key)));
                write_kv_table(out, &b.schema, &b.rows, b.rows.len(), fmt);
            }
        }
        Compaction::OpaqueRef {
            ccr_hash,
            byte_size,
            kind,
        } => {
            out.push_str(&format_ccr_marker(ccr_hash, *byte_size, kind));
        }
        Compaction::Untouched(v) => {
            out.push_str(&serde_json::to_string(v).unwrap_or_default());
        }
    }
}

fn write_kv_table(
    out: &mut String,
    schema: &Schema,
    rows: &[Row],
    original_count: usize,
    fmt: &MarkdownKvFormatter,
) {
    // Same declaration line as the CSV formatter: keeps row count and
    // typed shape up front where the model (and telemetry) expect it.
    // Unlike CSV (pre-existing exposure, kept byte-identical), KV quotes
    // pathological field names here so the declaration parses the same
    // way as the row lines below.
    out.push('[');
    out.push_str(&rows.len().to_string());
    out.push_str("]{");
    let col_decl: Vec<String> = schema
        .fields
        .iter()
        .map(|f| {
            let name = kv_field_name(&f.name);
            if f.nullable {
                format!("{}:{}?", name, f.type_tag)
            } else {
                format!("{}:{}", name, f.type_tag)
            }
        })
        .collect();
    out.push_str(&col_decl.join(","));
    out.push('}');
    if fmt.include_drop_summary && rows.len() < original_count {
        out.push_str(&format!(" __dropped:{}", original_count - rows.len()));
    }
    out.push('\n');

    for row in rows {
        // Compactor invariant: one cell per schema field. zip() would
        // silently drop extras — fail loudly in debug builds instead.
        debug_assert_eq!(row.0.len(), schema.fields.len());
        let mut wrote_first = false;
        for (field, cell) in schema.fields.iter().zip(row.0.iter()) {
            let rendered = match cell {
                CellValue::Missing => continue,
                CellValue::Scalar(v) => kv_scalar(v),
                CellValue::Nested(sub) => JsonFormatter::new().format(sub),
                CellValue::OpaqueRef {
                    ccr_hash,
                    byte_size,
                    kind,
                } => format_ccr_marker(ccr_hash, *byte_size, kind),
            };
            out.push_str(if wrote_first { "  " } else { "- " });
            out.push_str(&kv_field_name(&field.name));
            out.push_str(": ");
            out.push_str(&rendered);
            out.push('\n');
            wrote_first = true;
        }
        // All-missing row: keep a bare list item so the rendered row
        // count still matches the declaration.
        if !wrote_first {
            out.push_str("-\n");
        }
    }
}

fn kv_scalar(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => {
            if needs_kv_quote(s) {
                serde_json::to_string(s).unwrap_or_default()
            } else {
                s.clone()
            }
        }
        // Object/array fall back to compact JSON (rare — usually
        // already promoted to Nested by the compactor).
        _ => serde_json::to_string(v).unwrap_or_default(),
    }
}

fn needs_kv_quote(s: &str) -> bool {
    s.is_empty()
        || s.contains('\n')
        || s.contains('\r')
        || s.starts_with(char::is_whitespace)
        || s.ends_with(char::is_whitespace)
}

/// Field names are normally bare identifiers, but nothing upstream
/// enforces that. Quote the pathological ones the same way as values:
/// an embedded newline would inject fake row lines, and `": "` inside
/// a key would split the line at the wrong colon on read-back.
fn kv_field_name(name: &str) -> String {
    if needs_kv_quote(name) || name.contains(": ") {
        serde_json::to_string(name).unwrap_or_default()
    } else {
        name.to_string()
    }
}

