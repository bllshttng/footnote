//! The session-names overlay folds into the rows that answer to its ids.
//!
//! `~/.fno/session-names.json` was the old hex-handle -> legible-alias
//! overlay. The registry row is the primary name store now, so the sweep
//! folds every alias the file still carries into the row that session
//! answers to, on every tick, until nothing new folds. The file itself is
//! left untouched: external roster rows carry no fno row to hold an alias,
//! and its writer retirement belongs to the mail-address surface.

use crate::events::EventEmitter;
use crate::state;
use serde_json::Value;
use std::path::PathBuf;

/// `$HOME/.fno/session-names.json`, the overlay's default global path.
fn default_name_map_path() -> PathBuf {
    let base = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join(".fno").join("session-names.json")
}

/// Fold the overlay into the registry, once per sweep. Best-effort: an
/// unreadable file or registry folds nothing and never fails the sweep.
/// Emits `session_aliases_merged` with the count only when something folded.
pub(crate) fn fold_session_names(home: &crate::paths::AgentsHome, emitter: &EventEmitter) {
    fold_session_names_from(home, emitter, &default_name_map_path());
}

/// The same fold against an explicit overlay path (the test seam).
pub(crate) fn fold_session_names_from(
    home: &crate::paths::AgentsHome,
    emitter: &EventEmitter,
    name_map_path: &std::path::Path,
) {
    let Ok(raw) = std::fs::read_to_string(name_map_path) else {
        return;
    };
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&raw) else {
        return;
    };
    let pairs: Vec<(String, String)> = map
        .into_iter()
        .filter_map(|(sid, alias)| match alias {
            Value::String(a) if !sid.is_empty() && !a.trim().is_empty() => {
                Some((sid, a.trim().to_string()))
            }
            _ => None,
        })
        .collect();
    if pairs.is_empty() {
        return;
    }
    let merged = std::cell::Cell::new(0usize);
    let pairs = &pairs;
    let _ = state::update_registry(&home.registry_json(), |r| {
        let folded = fold_into(r, pairs);
        let minted = mint_default_aliases(r);
        merged.set(folded + minted);
    });
    let merged = merged.get();
    if merged > 0 {
        let _ = emitter.emit(
            "session_aliases_merged",
            &serde_json::json!({"count": merged}),
        );
    }
}

/// Mint the default legible alias `<cwd-basename>-<short_id>` for rows that
/// carry no legible alias yet: the sweep owns birth addressing, so a row
/// registered without one gains it within a tick. Same refusal rule as the
/// fold: an alias another row answers to is skipped, never stolen.
fn mint_default_aliases(r: &mut state::Registry) -> usize {
    let mut minted = 0usize;
    let rows: Vec<(String, String, String)> = r
        .entries
        .iter()
        .map(|e| (e.name.clone(), e.cwd.clone(), e.short_id.clone()))
        .collect();
    for (name, cwd, short) in &rows {
        let Some(idx) = r.entries.iter().position(|e| &e.name == name) else {
            continue;
        };
        let target = &r.entries[idx];
        if short.is_empty()
            || target.name.ends_with(short.as_str())
            || target.aliases.iter().any(|a| a.ends_with(short.as_str()))
        {
            continue;
        }
        let base = std::path::Path::new(cwd)
            .file_name()
            .and_then(|b| b.to_str())
            .unwrap_or("session")
            .to_string();
        let alias = format!("{base}-{short}");
        if alias_taken(r, &alias) {
            continue;
        }
        r.entries[idx].aliases.push(alias);
        minted += 1;
    }
    minted
}

/// True when any row already answers to `alias` (as a name or a carried
/// alias). Separated so callers compute the answer BEFORE taking their
/// mutable borrow of the target row (the NLL two-borrows trap).
fn alias_taken(r: &state::Registry, alias: &str) -> bool {
    r.entries
        .iter()
        .any(|other| other.name == alias || other.aliases.iter().any(|a| a == alias))
}

/// Append every overlay alias its row does not yet carry. An alias another
/// row already answers to (in `aliases` or as `name`) is refused - an
/// ambiguous alias is no address at all, the same rule the demoted label
/// lookup applies. Returns how many rows gained an alias.
fn fold_into(r: &mut state::Registry, pairs: &[(String, String)]) -> usize {
    let mut merged = 0usize;
    for (sid, alias) in pairs {
        let Some(idx) = r.entries.iter().position(|e| {
            e.harness_session_id.as_deref() == Some(sid.as_str())
                || (!e.short_id.is_empty() && e.short_id == *sid)
        }) else {
            continue;
        };
        let target = &r.entries[idx];
        if target.name == *alias || target.aliases.iter().any(|a| a == alias) {
            continue;
        }
        if alias_taken(r, alias) {
            continue;
        }
        r.entries[idx].aliases.push(alias.clone());
        merged += 1;
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(name: &str, sid: Option<&str>, short: &str) -> state::RegistryEntry {
        let mut e = state::RegistryEntry::default();
        e.name = name.into();
        e.harness_session_id = sid.map(str::to_string);
        e.short_id = short.into();
        e
    }

    #[test]
    fn the_fold_appends_the_alias_the_row_was_missing() {
        let mut r = state::Registry::default();
        r.entries.push(row("t-1ab9-w1", Some("sid-1"), "abc12345"));
        let merged = fold_into(
            &mut r,
            &[("sid-1".to_string(), "legible-alias".to_string())],
        );
        assert_eq!(merged, 1);
        assert_eq!(r.entries[0].aliases, vec!["legible-alias"]);
        // A second fold of the same overlay is a no-op: idempotent.
        assert_eq!(
            fold_into(
                &mut r,
                &[("sid-1".to_string(), "legible-alias".to_string())]
            ),
            0
        );
    }

    #[test]
    fn an_alias_another_row_answers_to_is_refused() {
        let mut r = state::Registry::default();
        r.entries.push(row("t-1ab9-w1", Some("sid-1"), "abc12345"));
        let mut taken = row("legible-alias", Some("sid-2"), "def23456");
        taken.aliases.push("legible-alias".to_string());
        r.entries.push(taken);
        assert_eq!(
            fold_into(
                &mut r,
                &[("sid-1".to_string(), "legible-alias".to_string())]
            ),
            0,
            "an ambiguous alias is no address at all"
        );
    }
}
