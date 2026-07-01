//! JSON-Schema cleaning for the antigravity upstream, ported from CLIProxyAPI
//! `internal/util/gemini_schema.go` (`CleanJSONSchemaForAntigravity` /
//! `CleanJSONSchemaForGemini`).
//!
//! cloudcode-pa's antigravity endpoint translates the Gemini-shaped request to
//! the target backend (Vertex Anthropic for `claude-*`, Gemini for the rest). It
//! reads tool schemas from `functionDeclarations[].parameters` — **not**
//! `parametersJsonSchema` — and rejects unsupported JSON-Schema keywords. So
//! before sending we (1) rename `parametersJsonSchema`→`parameters` and (2) run
//! this cleaner over each schema. Without it the backend gets an empty tool
//! schema (`tools.0.custom.input_schema: Field required`) or chokes on keywords
//! like `$schema`/`exclusiveMinimum`.
//!
//! `add_placeholder = true` (antigravity / Claude VALIDATED mode) also injects a
//! placeholder required property into otherwise-empty object schemas, which that
//! mode demands. Done as a recursive `serde_json::Value` rewrite — equivalent to
//! the Go path-based passes but simpler to follow.

use serde_json::{json, Map, Value};

const PLACEHOLDER_REASON_DESC: &str = "Brief explanation of why you are calling this tool";

/// Constraints Claude's VALIDATED mode rejects — moved into the description, then
/// stripped. (Note: `minimum`/`maximum` are deliberately NOT here — kept as-is,
/// matching CLIProxyAPI.)
const UNSUPPORTED_CONSTRAINTS: &[&str] = &[
    "minLength",
    "maxLength",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "pattern",
    "minItems",
    "maxItems",
    "uniqueItems",
    "format",
    "default",
    "examples",
];

/// Schema keywords/metadata the upstream doesn't support — stripped after any
/// hint extraction. (`x-*` extension keys are stripped separately.)
const REMOVE_METADATA: &[&str] = &[
    "$schema",
    "$defs",
    "definitions",
    "const",
    "$ref",
    "$id",
    "additionalProperties",
    "propertyNames",
    "patternProperties",
    "enumTitles",
    "prefill",
    "deprecated",
];

pub fn clean_for_antigravity(mut v: Value) -> Value {
    clean(&mut v, true, true);
    v
}

pub fn clean_for_gemini(mut v: Value) -> Value {
    clean(&mut v, false, true);
    v
}

/// Recursively clean one schema node. `add_placeholder` injects VALIDATED-mode
/// placeholders (antigravity/Claude); `is_root` suppresses the `_` placeholder at
/// the top level (matching CLIProxyAPI).
fn clean(node: &mut Value, add_placeholder: bool, is_root: bool) {
    let obj = match node.as_object_mut() {
        Some(o) => o,
        None => return,
    };

    // $ref -> description hint, replacing the node with a bare object.
    if obj.contains_key("$ref") {
        let refv = obj
            .get("$ref")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        let name = refv.rsplit('/').next().unwrap_or("").to_string();
        let existing = obj
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();
        let hint = combine(&existing, &format!("See: {name}"));
        obj.clear();
        obj.insert("type".into(), json!("object"));
        obj.insert("description".into(), json!(hint));
    }

    // const -> single-value enum.
    if obj.contains_key("const") && !obj.contains_key("enum") {
        if let Some(c) = obj.get("const").cloned() {
            obj.insert("enum".into(), json!([c]));
        }
    }

    merge_all_of(obj);
    flatten_union(obj, "anyOf");
    flatten_union(obj, "oneOf");

    // enum values -> strings, type -> string.
    if let Some(arr) = obj.get("enum").and_then(|e| e.as_array()).cloned() {
        let strs: Vec<Value> = arr.iter().map(|v| json!(value_to_string(v))).collect();
        let n = strs.len();
        obj.insert("enum".into(), json!(strs));
        obj.insert("type".into(), json!("string"));
        if (2..=10).contains(&n) {
            let vals: Vec<String> = obj["enum"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .collect();
            append_hint(obj, &format!("Allowed: {}", vals.join(", ")));
        }
    }

    if obj.get("additionalProperties") == Some(&Value::Bool(false)) {
        append_hint(obj, "No extra properties allowed");
    }

    // Move scalar unsupported constraints into the description before removal.
    for &key in UNSUPPORTED_CONSTRAINTS {
        if let Some(val) = obj.get(key) {
            if !val.is_object() && !val.is_array() {
                let hint = format!("{}: {}", key, value_to_string(val));
                append_hint(obj, &hint);
            }
        }
    }

    // type: [x, "null"] -> x (+ "Accepts:" hint when several non-null types).
    if let Some(types) = obj.get("type").and_then(|t| t.as_array()).cloned() {
        let mut non_null: Vec<String> = Vec::new();
        for t in &types {
            if let Some(s) = t.as_str() {
                if s != "null" && !s.is_empty() {
                    non_null.push(s.to_string());
                }
            }
        }
        let first = non_null
            .first()
            .cloned()
            .unwrap_or_else(|| "string".to_string());
        obj.insert("type".into(), json!(first));
        if non_null.len() > 1 {
            append_hint(obj, &format!("Accepts: {}", non_null.join(" | ")));
        }
    }

    // Strip unsupported keywords + x-* extensions at this node.
    for &key in UNSUPPORTED_CONSTRAINTS {
        obj.remove(key);
    }
    for &key in REMOVE_METADATA {
        obj.remove(key);
    }
    if !add_placeholder {
        obj.remove("nullable");
        obj.remove("title");
    }
    let x_keys: Vec<String> = obj
        .keys()
        .filter(|k| k.starts_with("x-"))
        .cloned()
        .collect();
    for k in x_keys {
        obj.remove(&k);
    }

    // Which properties were nullable (type contained "null") — drop from required.
    let nullable_fields: Vec<String> = obj
        .get("properties")
        .and_then(|p| p.as_object())
        .map(|m| {
            m.iter()
                .filter(|(_, c)| is_nullable_type(c))
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default();

    // Recurse into child schemas.
    if let Some(props) = obj.get_mut("properties").and_then(|p| p.as_object_mut()) {
        for (name, child) in props.iter_mut() {
            clean(child, add_placeholder, false);
            if nullable_fields.iter().any(|n| n == name) {
                if let Some(co) = child.as_object_mut() {
                    let existing = co
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .to_string();
                    co.insert(
                        "description".into(),
                        json!(combine(&existing, "(nullable)")),
                    );
                }
            }
        }
    }
    if let Some(items) = obj.get_mut("items") {
        clean(items, add_placeholder, false);
    }

    cleanup_required(obj, &nullable_fields);

    if add_placeholder {
        add_empty_schema_placeholder(obj, is_root);
    } else {
        remove_placeholder_fields(obj);
    }
}

fn merge_all_of(obj: &mut Map<String, Value>) {
    let all_of = match obj.get("allOf").and_then(|a| a.as_array()).cloned() {
        Some(a) => a,
        None => return,
    };
    for item in &all_of {
        if let Some(props) = item.get("properties").and_then(|p| p.as_object()) {
            let dest = obj.entry("properties").or_insert_with(|| json!({}));
            if let Some(dm) = dest.as_object_mut() {
                for (k, v) in props {
                    dm.insert(k.clone(), v.clone());
                }
            }
        }
        if let Some(req) = item.get("required").and_then(|r| r.as_array()) {
            let mut cur: Vec<String> = obj
                .get("required")
                .and_then(|r| r.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            for r in req {
                if let Some(s) = r.as_str() {
                    if !cur.iter().any(|c| c == s) {
                        cur.push(s.to_string());
                    }
                }
            }
            obj.insert("required".into(), json!(cur));
        }
    }
    obj.remove("allOf");
}

/// Flatten an `anyOf`/`oneOf` to its "best" subschema (object > array > scalar),
/// merging the parent description and an "Accepts:" hint over the union types.
fn flatten_union(obj: &mut Map<String, Value>, key: &str) {
    let arr = match obj
        .get(key)
        .and_then(|a| a.as_array())
        .filter(|a| !a.is_empty())
        .cloned()
    {
        Some(a) => a,
        None => return,
    };
    let parent_desc = obj
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("")
        .to_string();
    let (best_idx, all_types) = select_best(&arr);
    let mut selected = arr[best_idx].clone();
    if let Some(so) = selected.as_object_mut() {
        if !parent_desc.is_empty() {
            let cd = so
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string();
            let merged = if cd.is_empty() {
                parent_desc.clone()
            } else if cd == parent_desc {
                cd
            } else {
                format!("{parent_desc} ({cd})")
            };
            so.insert("description".into(), json!(merged));
        }
        if all_types.len() > 1 {
            let cd = so
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string();
            so.insert(
                "description".into(),
                json!(combine(&cd, &format!("Accepts: {}", all_types.join(" | ")))),
            );
        }
        *obj = so.clone();
    } else {
        obj.remove(key);
    }
}

fn select_best(items: &[Value]) -> (usize, Vec<String>) {
    let mut best_score = -1i32;
    let mut best_idx = 0;
    let mut types: Vec<String> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let t0 = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let has_props = item.get("properties").is_some();
        let has_items = item.get("items").is_some();
        let (score, t) = if t0 == "object" || has_props {
            (3, if t0.is_empty() { "object" } else { t0 })
        } else if t0 == "array" || has_items {
            (2, if t0.is_empty() { "array" } else { t0 })
        } else if !t0.is_empty() && t0 != "null" {
            (1, t0)
        } else {
            (0, if t0.is_empty() { "null" } else { t0 })
        };
        if !t.is_empty() {
            types.push(t.to_string());
        }
        if score > best_score {
            best_score = score;
            best_idx = i;
        }
    }
    (best_idx, types)
}

fn cleanup_required(obj: &mut Map<String, Value>, nullable_fields: &[String]) {
    let req = match obj.get("required").and_then(|r| r.as_array()).cloned() {
        Some(r) => r,
        None => return,
    };
    let has_props_obj = obj
        .get("properties")
        .map(|p| p.is_object())
        .unwrap_or(false);
    let prop_keys: Vec<String> = if has_props_obj {
        obj["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let filtered: Vec<Value> = req
        .iter()
        .filter_map(|r| r.as_str())
        .filter(|s| !nullable_fields.iter().any(|n| n == s))
        .filter(|s| !has_props_obj || prop_keys.iter().any(|k| k == s))
        .map(|s| json!(s))
        .collect();
    if filtered.len() != req.len() {
        if filtered.is_empty() {
            obj.remove("required");
        } else {
            obj.insert("required".into(), json!(filtered));
        }
    }
}

/// Claude VALIDATED mode needs every object schema to declare at least one
/// required property. Empty object schemas get a `reason` string; non-empty ones
/// with no required get a boolean `_` (except the root).
fn add_empty_schema_placeholder(obj: &mut Map<String, Value>, is_root: bool) {
    if obj.get("type").and_then(|t| t.as_str()) != Some("object") {
        return;
    }
    let props_empty = match obj.get("properties") {
        None => true,
        Some(p) => p.as_object().map(|m| m.is_empty()).unwrap_or(true),
    };
    if props_empty {
        let mut pm = obj
            .remove("properties")
            .and_then(|p| match p {
                Value::Object(m) => Some(m),
                _ => None,
            })
            .unwrap_or_default();
        pm.insert(
            "reason".into(),
            json!({ "type": "string", "description": PLACEHOLDER_REASON_DESC }),
        );
        obj.insert("properties".into(), Value::Object(pm));
        obj.insert("required".into(), json!(["reason"]));
        return;
    }
    let has_required = obj
        .get("required")
        .and_then(|r| r.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if !has_required && !is_root {
        if let Some(pm) = obj.get_mut("properties").and_then(|p| p.as_object_mut()) {
            pm.entry("_".to_string())
                .or_insert_with(|| json!({ "type": "boolean" }));
        }
        obj.insert("required".into(), json!(["_"]));
    }
}

/// Gemini path: drop synthetic placeholder props (`_`, placeholder-only `reason`).
fn remove_placeholder_fields(obj: &mut Map<String, Value>) {
    let mut removed: Vec<String> = Vec::new();
    if let Some(props) = obj.get_mut("properties").and_then(|p| p.as_object_mut()) {
        if props.remove("_").is_some() {
            removed.push("_".into());
        }
        let only_reason = props.len() == 1 && props.contains_key("reason");
        if only_reason {
            let is_ph = props
                .get("reason")
                .and_then(|r| r.get("description"))
                .and_then(|d| d.as_str())
                == Some(PLACEHOLDER_REASON_DESC);
            if is_ph {
                props.remove("reason");
                removed.push("reason".into());
            }
        }
    }
    if !removed.is_empty() {
        if let Some(req) = obj.get("required").and_then(|r| r.as_array()).cloned() {
            let filtered: Vec<Value> = req
                .iter()
                .filter_map(|r| r.as_str())
                .filter(|s| !removed.iter().any(|x| x == s))
                .map(|s| json!(s))
                .collect();
            if filtered.is_empty() {
                obj.remove("required");
            } else {
                obj.insert("required".into(), json!(filtered));
            }
        }
    }
}

fn is_nullable_type(child: &Value) -> bool {
    child
        .get("type")
        .and_then(|t| t.as_array())
        .map(|a| a.iter().any(|x| x.as_str() == Some("null")))
        .unwrap_or(false)
}

/// `description = existing` with `(extra)` appended (matching CLIProxyAPI's
/// `appendHint`).
fn combine(existing: &str, extra: &str) -> String {
    if existing.is_empty() {
        extra.to_string()
    } else {
        format!("{existing} ({extra})")
    }
}

fn append_hint(obj: &mut Map<String, Value>, hint: &str) {
    let existing = obj
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("")
        .to_string();
    obj.insert("description".into(), json!(combine(&existing, hint)));
}

/// Mirror gjson's `.String()` for scalar coercion.
fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}
