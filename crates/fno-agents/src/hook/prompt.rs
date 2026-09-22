//! The UserPromptSubmit entry: the open list where the user types.
//!
//! Reads `prompt` from the hook payload. A turn that carries the mail
//! envelope, or that the provenance classifier reads as machine-shaped,
//! renders nothing. Otherwise it prints one `hookSpecificOutput` whose
//! `additionalContext` lists the cached projection (top five ready items)
//! and a count. It never folds a journal: the daemon's `attention` arm
//! writes `~/.fno/attention/items.json`, this reads it.

use serde_json::{json, Value};
use std::io::Read;

/// A cache older than this names its age instead of the items.
const CACHE_MAX_AGE_SECS: u64 = 120;
const MAX_ITEMS: usize = 5;

/// `fno-agents hook prompt`: payload on stdin, the envelope (or nothing) on
/// stdout, exit 0 always.
pub fn run(_args: &[String]) -> i32 {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return 0;
    }
    let payload: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    let prompt = payload.get("prompt").and_then(Value::as_str).unwrap_or("");
    if crate::mail_inject::contains_fno_mail_tag_anywhere(prompt)
        || crate::provenance::classify(prompt).is_err()
    {
        return 0;
    }
    let event_name = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or("UserPromptSubmit");
    let text = match read_cache() {
        None => {
            "No attention cache yet: the daemon's attention arm writes it every beat.".to_string()
        }
        Some((as_of, items)) => {
            let now = now_secs();
            if now.saturating_sub(as_of) > CACHE_MAX_AGE_SECS {
                format!(
                    "Attention cache is {}s old (max {CACHE_MAX_AGE_SECS}s); showing nothing.",
                    now.saturating_sub(as_of)
                )
            } else {
                render_block(&items)
            }
        }
    };
    println!(
        "{}",
        json!({
            "hookSpecificOutput": {
                "hookEventName": event_name,
                "additionalContext": text,
            }
        })
    );
    0
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `{as_of, items}` as the arm writes it; a malformed cache reads as absent.
fn read_cache() -> Option<(u64, Vec<Value>)> {
    let path = crate::attention_arm::attention_dir()
        .ok()?
        .join("items.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    Some((
        v.get("as_of").and_then(Value::as_u64)?,
        v.get("items")
            .and_then(Value::as_array)?
            .iter()
            .filter(|i| i.get("ready").and_then(Value::as_bool) == Some(true))
            .cloned()
            .collect(),
    ))
}

fn render_block(items: &[Value]) -> String {
    let total = items.len();
    let mut lines: Vec<String> = items
        .iter()
        .take(MAX_ITEMS)
        .map(|i| {
            let kind = i.get("kind").and_then(Value::as_str).unwrap_or("?");
            let title = i.get("title").and_then(Value::as_str).unwrap_or("");
            let asker = i
                .get("asker")
                .and_then(|a| a.get("handle"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if asker.is_empty() {
                format!("- {kind}: {title}")
            } else {
                format!("- {kind}: {title} (from {asker})")
            }
        })
        .collect();
    if total > MAX_ITEMS {
        lines.push(format!(
            "... and {} more; the full list is on your questions file.",
            total - MAX_ITEMS
        ));
    }
    if lines.is_empty() {
        return String::from("Nothing is waiting on you.");
    }
    let mut out = format!("Waiting on you ({}):\n", total);
    out.push_str(&lines.join("\n"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, kind: &str, asker: &str) -> Value {
        json!({"kind": kind, "title": title, "asker": {"handle": asker}, "ready": true})
    }

    #[test]
    fn ac12_hp_block_lists_items_with_a_count() {
        let items = vec![
            item("Which reading?", "question", "king-fno-g6"),
            item("Publish the crate", "pin", "worker-2"),
            item("ship tonight", "mine", ""),
        ];
        let text = render_block(&items);
        assert!(text.contains("Waiting on you (3)"));
        assert!(text.contains("- question: Which reading? (from king-fno-g6)"));
        assert!(text.contains("- pin: Publish the crate (from worker-2)"));
        assert!(text.contains("- mine: ship tonight"));
    }

    #[test]
    fn ac12_hp_more_than_five_items_collapse_to_a_count() {
        let items: Vec<Value> = (0..7)
            .map(|i| item(&format!("q{i}"), "question", "worker"))
            .collect();
        let text = render_block(&items);
        assert!(text.contains("Waiting on you (7)"));
        assert!(text.contains("... and 2 more"));
        assert_eq!(text.matches("- question:").count(), 5);
    }

    #[test]
    fn an_empty_projection_renders_one_calm_line() {
        let text = render_block(&[]);
        assert_eq!(text, "Nothing is waiting on you.");
    }
}
