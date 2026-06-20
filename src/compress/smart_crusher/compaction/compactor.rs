//! TabularCompactor — array → [`Compaction`] IR.
//!
//! # Pipeline
//!
//! ```text
//! &[Value]  →  detect uniformity  →  build schema  →  build rows
//!                    │
//!                    ├─ heterogeneous? → bucket by discriminator
//!                    │                    (Compaction::Buckets)
//!                    │
//!                    └─ homogeneous → flatten nested-uniform columns
//!                                        (Compaction::Table)
//! ```
//!
//! # Decision rules
//!
//! - **Untouched fall-through.** Items < 2, non-object items, or a key
//!   distribution too uneven for tabular form → return [`Compaction::Untouched`]
//!   so the existing lossy path takes over.
//! - **Schema = union of all keys**, sorted by descending frequency then
//!   alphabetically. Sparse fields keep their slot — cells in rows that
//!   lack the field render as [`CellValue::Missing`].
//! - **Heterogeneous case.** When < 50% of keys appear in >= 80% of rows,
//!   look for a discriminator (a string field present in every row whose
//!   value distribution partitions cleanly). If found, emit
//!   [`Compaction::Buckets`]; else [`Compaction::Untouched`].
//! - **Nested-uniform flatten.** A field that's an object in every row
//!   with the same inner key set, where flattening doesn't blow up the
//!   column count by more than `max_flatten_inner_keys`, gets promoted
//!   into dotted columns (`meta.region`, `meta.tier`).
//! - **Stringified-JSON.** Cells that classify as
//!   [`CellClass::StringifiedJson`] become [`CellValue::Nested`] when the
//!   parsed value is an array of objects (recursive table); otherwise
//!   [`CellValue::Scalar`] of the parsed value (saves escaping cost).
//! - **Opaque blob.** [`CellClass::Opaque`] cells become
//!   [`CellValue::OpaqueRef`] keyed by a 12-char SHA-256 prefix.
//!
//! [`CellClass`]: super::classifier::CellClass
//! [`CellClass::StringifiedJson`]: super::classifier::CellClass::StringifiedJson
//! [`CellClass::Opaque`]: super::classifier::CellClass::Opaque

use std::collections::BTreeMap;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::classifier::{classify_cell, CellClass, ClassifyConfig};
use super::ir::{Bucket, CellValue, Compaction, FieldSpec, Row, Schema};

/// Config for the compactor.
#[derive(Debug, Clone)]
pub struct CompactConfig {
    pub classify: ClassifyConfig,

    /// Minimum item count to attempt tabular compaction. Below this,
    /// return [`Compaction::Untouched`]. Default: 2.
    pub min_items: usize,

    /// A field is "core" if it appears in at least this fraction of
    /// rows. Schemas with too few core fields trigger heterogeneous
    /// (bucket) handling. Default: 0.8.
    pub core_field_fraction: f64,

    /// Heterogeneity threshold: when fewer than this fraction of all
    /// observed keys are core, treat the array as heterogeneous and
    /// look for a discriminator. Default: 0.6.
    pub heterogeneous_core_ratio: f64,

    /// Cap on inner-key count for nested-uniform flattening. Larger
    /// inner schemas stay nested rather than exploding column count.
    /// Default: 6.
    pub max_flatten_inner_keys: usize,

    /// Minimum bucket count before considering a candidate discriminator
    /// "useful". Default: 2.
    pub min_buckets: usize,

    /// Maximum bucket count — too many buckets means the discriminator
    /// is too granular (e.g. an ID column). Default: 8.
    pub max_buckets: usize,
}

impl Default for CompactConfig {
    fn default() -> Self {
        Self {
            classify: ClassifyConfig::default(),
            min_items: 2,
            core_field_fraction: 0.8,
            heterogeneous_core_ratio: 0.6,
            max_flatten_inner_keys: 6,
            min_buckets: 2,
            max_buckets: 8,
        }
    }
}

/// Top-level compaction entry point.
pub fn compact(items: &[Value], cfg: &CompactConfig) -> Compaction {
    if items.len() < cfg.min_items {
        return Compaction::Untouched(Value::Array(items.to_vec()));
    }
    if !items.iter().all(|v| matches!(v, Value::Object(_))) {
        return Compaction::Untouched(Value::Array(items.to_vec()));
    }

    let key_freqs = compute_key_freqs(items);
    let total = items.len();
    let core_threshold = (total as f64 * cfg.core_field_fraction).ceil() as usize;
    let core_count = key_freqs.values().filter(|&&f| f >= core_threshold).count();
    let total_keys = key_freqs.len();

    let core_ratio = if total_keys == 0 {
        1.0
    } else {
        core_count as f64 / total_keys as f64
    };

    if core_ratio < cfg.heterogeneous_core_ratio {
        if let Some(disc) = detect_discriminator(items, &key_freqs, cfg) {
            return bucket_by(items, &disc, cfg);
        }
        // No clean discriminator — fall through to a sparse Table
        // rather than refusing. A sparse table is still better than
        // letting the lossy path drop fields wholesale.
    }

    build_homogeneous_table(items, &key_freqs, cfg)
}

fn compute_key_freqs(items: &[Value]) -> BTreeMap<String, usize> {
    let mut freqs: BTreeMap<String, usize> = BTreeMap::new();
    for item in items {
        if let Value::Object(map) = item {
            for k in map.keys() {
                *freqs.entry(k.clone()).or_insert(0) += 1;
            }
        }
    }
    freqs
}

fn build_homogeneous_table(
    items: &[Value],
    key_freqs: &BTreeMap<String, usize>,
    cfg: &CompactConfig,
) -> Compaction {
    // Order: descending frequency, then alphabetical for stability.
    let mut keys: Vec<(&String, &usize)> = key_freqs.iter().collect();
    keys.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let ordered_keys: Vec<String> = keys.into_iter().map(|(k, _)| k.clone()).collect();

    let total = items.len();
    let mut field_specs: Vec<FieldSpec> = ordered_keys
        .iter()
        .map(|k| FieldSpec {
            name: k.clone(),
            type_tag: infer_type_tag(items, k),
            nullable: key_freqs[k] < total
                || items
                    .iter()
                    .filter_map(|v| v.as_object())
                    .any(|o| matches!(o.get(k), Some(Value::Null))),
        })
        .collect();

    let mut rows: Vec<Row> = items
        .iter()
        .map(|item| build_row(item, &ordered_keys, cfg))
        .collect();

    flatten_uniform_nested(&mut field_specs, &mut rows, cfg);

    Compaction::Table {
        schema: Schema {
            fields: field_specs,
        },
        rows,
        original_count: items.len(),
    }
}

fn build_row(item: &Value, ordered_keys: &[String], cfg: &CompactConfig) -> Row {
    let obj = match item.as_object() {
        Some(o) => o,
        None => return Row::new(vec![]),
    };
    let cells: Vec<CellValue> = ordered_keys
        .iter()
        .map(|k| match obj.get(k) {
            None => CellValue::Missing,
            Some(v) => cell_from_value(v, cfg),
        })
        .collect();
    Row::new(cells)
}

fn cell_from_value(v: &Value, cfg: &CompactConfig) -> CellValue {
    match classify_cell(v, &cfg.classify) {
        CellClass::Scalar => CellValue::Scalar(v.clone()),
        CellClass::JsonObject => CellValue::Scalar(v.clone()), // flatten pass may promote
        CellClass::JsonArray => {
            // Recurse if the inner array is array-of-objects; else scalar.
            if let Value::Array(items) = v {
                if items.iter().all(|i| matches!(i, Value::Object(_))) && items.len() >= 2 {
                    return CellValue::Nested(Box::new(compact(items, cfg)));
                }
            }
            CellValue::Scalar(v.clone())
        }
        CellClass::StringifiedJson(parsed) => {
            // If the parsed JSON is an array of objects, recurse; else
            // store the parsed value as a Scalar (un-escapes for free).
            if let Value::Array(items) = &parsed {
                if items.iter().all(|i| matches!(i, Value::Object(_))) && items.len() >= 2 {
                    return CellValue::Nested(Box::new(compact(items, cfg)));
                }
            }
            CellValue::Scalar(parsed)
        }
        CellClass::Opaque(kind) => {
            let bytes = match v {
                Value::String(s) => s.as_bytes(),
                _ => return CellValue::Scalar(v.clone()),
            };
            CellValue::OpaqueRef {
                ccr_hash: hash_opaque(bytes),
                byte_size: bytes.len(),
                kind,
            }
        }
    }
}

/// Promote fields whose every row holds an object with the same key
/// set into dotted columns. Bounded by `cfg.max_flatten_inner_keys` so
/// a 50-key inner schema doesn't blow up the table width.
fn flatten_uniform_nested(specs: &mut Vec<FieldSpec>, rows: &mut [Row], cfg: &CompactConfig) {
    let mut i = 0;
    while i < specs.len() {
        let inner_keys = match uniform_object_keys(specs, rows, i) {
            Some(keys) if !keys.is_empty() && keys.len() <= cfg.max_flatten_inner_keys => keys,
            _ => {
                i += 1;
                continue;
            }
        };

        let parent_name = specs[i].name.clone();
        let new_specs: Vec<FieldSpec> = inner_keys
            .iter()
            .map(|k| FieldSpec {
                name: format!("{parent_name}.{k}"),
                type_tag: "string".into(),
                nullable: false,
            })
            .collect();
        let n_new = new_specs.len();

        // Splice into specs: replace specs[i] with new_specs.
        specs.splice(i..i + 1, new_specs);

        // Rewrite each row: replace row.0[i] with N expanded cells.
        for row in rows.iter_mut() {
            let original = row.0.remove(i);
            let inner_obj: Option<serde_json::Map<String, Value>> = match original {
                CellValue::Scalar(Value::Object(map)) => Some(map),
                CellValue::Missing => None,
                _ => unreachable!(
                    "uniform_object_keys guarantees every cell is Scalar(Object) or Missing"
                ),
            };
            let expanded: Vec<CellValue> = inner_keys
                .iter()
                .map(|k| match &inner_obj {
                    None => CellValue::Missing,
                    Some(map) => match map.get(k) {
                        None => CellValue::Missing,
                        Some(v) => CellValue::Scalar(v.clone()),
                    },
                })
                .collect();
            for (offset, cell) in expanded.into_iter().enumerate() {
                row.0.insert(i + offset, cell);
            }
        }

        // Refine type tags + nullability from data.
        for offset in 0..n_new {
            let col_idx = i + offset;
            let mut nullable = false;
            let inferred = infer_type_tag_from_cells(rows, col_idx, &mut nullable);
            specs[col_idx].type_tag = inferred;
            specs[col_idx].nullable = nullable;
        }

        i += n_new;
    }
}

fn infer_type_tag_from_cells(rows: &[Row], col: usize, nullable: &mut bool) -> String {
    let mut tag = "null";
    let mut saw_value = false;
    for row in rows {
        if let Some(cell) = row.0.get(col) {
            match cell {
                CellValue::Missing => *nullable = true,
                CellValue::Scalar(Value::Null) => *nullable = true,
                CellValue::Scalar(v) => {
                    if !saw_value {
                        tag = type_tag_for(v);
                        saw_value = true;
                    } else if type_tag_for(v) != tag {
                        tag = "json";
                    }
                }
                _ => tag = "json",
            }
        }
    }
    tag.to_string()
}

/// Returns Some(inner_keys) if every row's cell at `col` is an object
/// with the same key set (or Missing). None otherwise.
fn uniform_object_keys(specs: &[FieldSpec], rows: &[Row], col: usize) -> Option<Vec<String>> {
    if specs[col].name.contains('.') {
        // Already a flattened column.
        return None;
    }
    let mut canonical: Option<Vec<String>> = None;
    let mut saw_object = false;
    for row in rows {
        let cell = row.0.get(col)?;
        match cell {
            CellValue::Missing => continue,
            CellValue::Scalar(Value::Object(map)) => {
                let keys: Vec<String> = map.keys().cloned().collect();
                saw_object = true;
                match &canonical {
                    None => canonical = Some(keys),
                    Some(existing) => {
                        if existing != &keys {
                            return None;
                        }
                    }
                }
            }
            _ => return None,
        }
    }
    if !saw_object {
        return None;
    }
    canonical
}

fn infer_type_tag(items: &[Value], key: &str) -> String {
    let mut tag: Option<&'static str> = None;
    for it in items {
        if let Some(v) = it.as_object().and_then(|m| m.get(key)) {
            if matches!(v, Value::Null) {
                continue;
            }
            let t = type_tag_for(v);
            match tag {
                None => tag = Some(t),
                Some(existing) if existing != t => {
                    tag = Some("json");
                    break;
                }
                _ => {}
            }
        }
    }
    tag.unwrap_or("null").to_string()
}

fn type_tag_for(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_i64() || n.is_u64() => "int",
        Value::Number(_) => "float",
        Value::String(_) => "string",
        Value::Object(_) | Value::Array(_) => "json",
    }
}

fn hash_opaque(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    // 12-char hex prefix — collision-resistant enough for a single
    // payload in flight, short enough to keep the marker compact.
    let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    hex
}

// ─────────────────────────── heterogeneous bucketing ───────────────────────────

/// Find a discriminator field — string-typed, present in every row,
/// with a value distribution that partitions cleanly into 2..=max_buckets
/// non-trivial buckets.
fn detect_discriminator(
    items: &[Value],
    key_freqs: &BTreeMap<String, usize>,
    cfg: &CompactConfig,
) -> Option<String> {
    let total = items.len();
    let mut best: Option<(String, usize)> = None; // (key, bucket_count)

    for (k, &freq) in key_freqs {
        if freq < total {
            continue; // must be present in every row
        }
        // Collect values; require all strings.
        let mut values: Vec<&str> = Vec::with_capacity(total);
        let mut all_strings = true;
        for item in items {
            match item.as_object().and_then(|m| m.get(k)) {
                Some(Value::String(s)) => values.push(s.as_str()),
                _ => {
                    all_strings = false;
                    break;
                }
            }
        }
        if !all_strings {
            continue;
        }
        let mut distinct: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for v in &values {
            distinct.insert(*v);
        }
        let n = distinct.len();
        if n < cfg.min_buckets || n > cfg.max_buckets {
            continue;
        }
        // Reject discriminators that are essentially unique (1 row per
        // bucket — that's an ID, not a category).
        if n as f64 / total as f64 > 0.7 {
            continue;
        }
        let score = n; // prefer more buckets up to max
        match &best {
            None => best = Some((k.clone(), score)),
            Some((_, s)) if score > *s => best = Some((k.clone(), score)),
            _ => {}
        }
    }
    best.map(|(k, _)| k)
}

fn bucket_by(items: &[Value], discriminator: &str, cfg: &CompactConfig) -> Compaction {
    let mut groups: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for item in items {
        let key = item
            .as_object()
            .and_then(|m| m.get(discriminator))
            .and_then(|v| v.as_str())
            .unwrap_or("__missing__")
            .to_string();
        groups.entry(key).or_default().push(item.clone());
    }
    let buckets: Vec<Bucket> = groups
        .into_iter()
        .map(|(key, group_items)| {
            let inner = compact(&group_items, cfg);
            match inner {
                Compaction::Table { schema, rows, .. } => Bucket {
                    key: Value::String(key),
                    schema,
                    rows,
                },
                _ => {
                    // Sub-compaction declined — fall back to a degenerate
                    // single-column "value" table holding the raw items.
                    Bucket {
                        key: Value::String(key),
                        schema: Schema {
                            fields: vec![FieldSpec {
                                name: "value".into(),
                                type_tag: "json".into(),
                                nullable: false,
                            }],
                        },
                        rows: group_items
                            .into_iter()
                            .map(|v| Row::new(vec![CellValue::Scalar(v)]))
                            .collect(),
                    }
                }
            }
        })
        .collect();
    Compaction::Buckets {
        discriminator: discriminator.to_string(),
        buckets,
        original_count: items.len(),
    }
}

