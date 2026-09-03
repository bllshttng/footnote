//! The `rename` verb's client half: argv-to-request shaping and the success
//! receipt. The daemon owns the transaction.

use serde_json::{Map, Value};

/// Shape `rename <token> --name <new-label>`: swap the `--name` value (which
/// lands in `params.name`) with the positional token, carrying the label as
/// `new_name`.
pub fn request(params: &mut Map<String, Value>, positional: &[String]) -> Result<(), String> {
    let new_name = params
        .remove("name")
        .ok_or("rename needs --name <new-label>")?;
    let token = positional
        .first()
        .ok_or("rename needs a <name> to rename")?;
    params.insert("new_name".into(), new_name);
    params.insert("name".into(), Value::String(token.clone()));
    Ok(())
}

/// The receipt names BOTH labels: the new one is the live address, the old one
/// the alias a later `peek`/`mail send` may still use.
pub fn receipt(name: &str, result: &Value) -> Option<String> {
    let old = result
        .get("old_name")
        .and_then(Value::as_str)
        .unwrap_or(name);
    let new = result
        .get("new_name")
        .and_then(Value::as_str)
        .unwrap_or("(unknown)");
    Some(format!("renamed {old} -> {new}"))
}
