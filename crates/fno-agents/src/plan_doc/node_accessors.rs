//! Small JSON accessors shared by the plan_doc projection port.

use serde_json::Value;

/// str field on a dict row.
pub fn s_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// array field on a doc row.
pub fn s_list<'a>(v: &'a Value, key: &str) -> Option<&'a Vec<Value>> {
    v.get(key).and_then(Value::as_array)
}

/// Dict rows whose `parent` equals `parent_id`.
pub fn direct_children<'a>(entries: &'a [Value], parent_id: &str) -> Vec<&'a Value> {
    entries
        .iter()
        .filter(|n| n.is_object() && s_field(n, "parent") == Some(parent_id))
        .collect()
}

/// Exact-id node lookup (the callers resolve aliases before this runs).
pub fn find_node<'a>(entries: &'a [Value], node_id: &str) -> Option<&'a Value> {
    entries
        .iter()
        .find(|n| n.is_object() && s_field(n, "id") == Some(node_id))
}

/// Python `str()` rendering of a JSON value, for scalar comparison/writing:
/// strings verbatim, booleans as `True`/`False`, null as `None`, numbers as
/// their JSON text. This is what the Python converger compares and stores.
pub fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => {
            if *b {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Value::Null => "None".to_string(),
        v => v.to_string(),
    }
}
