//! Python-byte renderers for the porcelain reads: `json.dumps` output shapes
//! (compact and indent-2, both `ensure_ascii`) and the opt-in grouped view.
//! The goldens these renderers must reproduce were captured from the Python
//! surface before its deletion; the tests pin the exact bytes.

use serde_json::Value;

/// Python `json.dumps` string escaping: ASCII stays, control characters take
/// their short forms, and everything non-ASCII becomes `\uXXXX` (surrogate
/// pairs above the BMP).
fn escape_py_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c if (c as u32) <= 0x7e => out.push(c),
            c => {
                let cp = c as u32;
                if cp <= 0xffff {
                    out.push_str(&format!("\\u{:04x}", cp));
                } else {
                    let v = cp - 0x10000;
                    let hi = 0xd800 + (v >> 10);
                    let lo = 0xdc00 + (v & 0x3ff);
                    out.push_str(&format!("\\u{:04x}\\u{:04x}", hi, lo));
                }
            }
        }
    }
    out.push('"');
}

fn write_json(v: &Value, pretty: bool, indent: usize, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => escape_py_string(s, out),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                    if !pretty {
                        out.push(' ');
                    }
                }
                if pretty {
                    out.push('\n');
                    out.push_str(&" ".repeat(indent + 2));
                }
                write_json(item, pretty, indent + 2, out);
            }
            if pretty {
                out.push('\n');
                out.push_str(&" ".repeat(indent));
            }
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, val)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                    if !pretty {
                        out.push(' ');
                    }
                }
                if pretty {
                    out.push('\n');
                    out.push_str(&" ".repeat(indent + 2));
                }
                escape_py_string(k, out);
                out.push_str(": ");
                write_json(val, pretty, indent + 2, out);
            }
            if pretty {
                out.push('\n');
                out.push_str(&" ".repeat(indent));
            }
            out.push('}');
        }
    }
}

/// Python `json.dumps(v)` byte shape (compact, ensure_ascii).
pub fn py_json_compact(v: &Value) -> String {
    let mut out = String::new();
    write_json(v, false, 0, &mut out);
    out
}

/// Python `json.dumps(v, indent=2)` byte shape.
pub fn py_json_pretty(v: &Value) -> String {
    let mut out = String::new();
    write_json(v, true, 0, &mut out);
    out
}

// ── the grouped view ────────────────────────────────────────────────────────

/// The concept groups, in display order: `(heading, fields)`. Mirrors the
/// Python table the grouped renderer was ported from.
const GROUPS: &[(&str, &[&str])] = &[
    ("Identity", &["id", "slug", "title", "type", "project"]),
    (
        "Lifecycle",
        &[
            "status",
            "persisted_status",
            "priority",
            "rank",
            "size",
            "batch",
            "deferred_kind",
        ],
    ),
    (
        "Timestamps",
        &[
            "created_at",
            "touched_at",
            "locked_at",
            "completed_at",
            "deferred_at",
            "reopened_at",
            "queued_at",
            "archived_at",
        ],
    ),
    (
        "Reasons",
        &[
            "details",
            "blocked_reason",
            "deferred_reason",
            "reopened_reason",
            "completion_note",
        ],
    ),
    (
        "Hierarchy",
        &[
            "parent",
            "children",
            "blocked_by",
            "related",
            "contained_in",
            "group_slug",
            "tasks",
        ],
    ),
    (
        "Supersession",
        &["superseded_by", "supersedes", "supersession"],
    ),
    (
        "Provenance",
        &[
            "source",
            "source_kind",
            "source_project",
            "source_session_id",
            "source_harness",
            "source_cwd",
            "source_node_id",
            "source_plan_path",
            "request_origin",
            "origin_evidence",
        ],
    ),
    (
        "Execution",
        &[
            "locked_by",
            "locked_by_harness",
            "locked_by_harness_session",
            "session_id",
            "ownership_defect",
            "sessions",
            "dispatch_verb",
            "dispatch_brief",
        ],
    ),
    (
        "Delivery",
        &[
            "plan_path",
            "pr_number",
            "pr_url",
            "additional_prs",
            "merge_status",
            "artifact_url",
            "collisions_acknowledged",
        ],
    ),
    ("Cost", &["cost_usd", "cost_sessions"]),
    (
        "Content",
        &[
            "has_brief",
            "roadmap_id",
            "vision_path",
            "think_output_path",
            "think_session_id",
        ],
    ),
];

/// Render populated entry fields in stable concept sections, unknown keys in
/// `Residual` in their original order. No key is dropped.
pub fn render_grouped(entry: &Value) -> String {
    let Some(map) = entry.as_object() else {
        return String::new();
    };
    let is_populated = |v: &Value| match v {
        Value::Null => false,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        _ => true,
    };
    let display = |v: &Value| match v {
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Value::String(s) => s.clone(),
        other => py_json_compact_sorted(other),
    };
    let mut sections: Vec<String> = Vec::new();
    for (heading, fields) in GROUPS {
        let mut lines: Vec<String> = Vec::new();
        for field in *fields {
            if let Some(v) = map.get(*field) {
                if is_populated(v) {
                    lines.push(format!("{field}: {}", display(v)));
                }
            }
        }
        if !lines.is_empty() {
            sections.push(heading.to_string());
            sections.extend(lines);
            sections.push(String::new());
        }
    }
    let residual: Vec<String> = map
        .iter()
        .filter(|(k, v)| !GROUPS.iter().any(|(_, f)| f.contains(&k.as_str())) && is_populated(v))
        .map(|(k, v)| format!("{k}: {}", display(v)))
        .collect();
    if !residual.is_empty() {
        sections.push("Residual".to_string());
        sections.extend(residual);
        sections.push(String::new());
    }
    let joined = sections.join("\n");
    joined.trim_end_matches('\n').to_string()
}

/// The grouped display of containers: `json.dumps(..., sort_keys=True)`.
fn py_json_compact_sorted(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let inner: Vec<String> = keys
                .iter()
                .map(|k| {
                    format!(
                        "{}: {}",
                        py_json_compact(&Value::String((*k).clone())),
                        py_json_compact_sorted(&map[*k])
                    )
                })
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(py_json_compact_sorted).collect();
            format!("[{}]", inner.join(", "))
        }
        other => py_json_compact(other),
    }
}

/// Reorder one row's keys into the canonical graph order, extras appended.
pub fn canonical_order(row: &Value) -> Value {
    let order = crate::graph_store::CANONICAL_FIELD_ORDER;
    let Some(obj) = row.as_object() else {
        return row.clone();
    };
    let mut out = serde_json::Map::with_capacity(obj.len());
    for key in order {
        if let Some(v) = obj.get(*key) {
            out.insert((*key).to_string(), v.clone());
        }
    }
    for (k, v) in obj {
        if !order.contains(&k.as_str()) {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compact_matches_python_json_dumps_bytes() {
        let v = json!({"title": "Gamma épic", "n": 3, "ok": true, "none": null,
                       "list": ["one", "two"]});
        let py = "{\"title\": \"Gamma \\u00e9pic\", \"n\": 3, \"ok\": true, \"none\": null, \"list\": [\"one\", \"two\"]}";
        assert_eq!(py_json_compact(&v), py);
    }

    #[test]
    fn pretty_matches_python_indent_two_bytes() {
        let v = json!({"a": 1, "b": ["x", "y"]});
        let expected = "{\n  \"a\": 1,\n  \"b\": [\n    \"x\",\n    \"y\"\n  ]\n}";
        assert_eq!(py_json_pretty(&v), expected);
    }

    #[test]
    fn astral_chars_take_surrogate_pairs() {
        // U+1F423 (the turtle), escaped in source so no sanitizer touches it.
        let v = json!("\u{1F423}");
        assert_eq!(py_json_compact(&v), "\"\\ud83d\\udc23\"");
    }

    #[test]
    fn grouped_matches_the_captured_python_bytes() {
        let entry = json!({
            "id": "x-aaaa1111", "slug": "alpha-node", "title": "Alpha node",
            "type": "feature", "project": "fno", "status": "blocked",
            "priority": "p1", "created_at": "2026-09-01T00:00:00+00:00",
            "touched_at": "2026-09-02T00:00:00+00:00",
            "details": "the alpha details",
            "blocked_reason": "blocked-by:x-bbbb2222",
            "parent": "x-cccc3333", "blocked_by": ["x-bbbb2222"],
            "source_kind": "organic", "plan_path": "plans/alpha.md",
        });
        let expected = "Identity\nid: x-aaaa1111\nslug: alpha-node\ntitle: Alpha node\ntype: feature\nproject: fno\n\nLifecycle\nstatus: blocked\npriority: p1\n\nTimestamps\ncreated_at: 2026-09-01T00:00:00+00:00\ntouched_at: 2026-09-02T00:00:00+00:00\n\nReasons\ndetails: the alpha details\nblocked_reason: blocked-by:x-bbbb2222\n\nHierarchy\nparent: x-cccc3333\nblocked_by: [\"x-bbbb2222\"]\n\nProvenance\nsource_kind: organic\n\nDelivery\nplan_path: plans/alpha.md";
        assert_eq!(render_grouped(&entry), expected);
    }

    #[test]
    fn unknown_keys_land_in_residual_in_order() {
        let entry = json!({"id": "x-1", "custom_thing": "kept", "difficulty": "high"});
        let out = render_grouped(&entry);
        assert!(out.starts_with("Identity\nid: x-1"), "{out}");
        assert!(
            out.contains("Residual\ncustom_thing: kept\ndifficulty: high"),
            "{out}"
        );
    }
}
