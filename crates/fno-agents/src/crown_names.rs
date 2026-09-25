//! The crown name store: `crown_names.json`, beside `registry.json` in the
//! agents home (law d-1726ee1c, reversible: the name belongs to the crown,
//! keyed on its canonical scope - the manifest was measured too lossy to
//! carry it, since 3 of 5 live crowns had no manifest and Python rewrites
//! the rest with `force=True`).
//!
//! Wire contract. `crates/fno/src/crown_names.rs` reads this FILE (the mux
//! never links fno-agents), so the shape below is frozen:
//!
//! ```json
//! {"version": 1, "crowns": {"<canonical scope>": {
//!   "name": "Barnaby", "regnal": 1,
//!   "holder_session": "<harness session uuid>" | null,
//!   "nodes": ["x-aaaa"], "updated_at": "2026-09-23T20:00:00Z"}}}
//! ```
//!
//! `holder_session` is the live holder row's `harness_session_id` (law
//! d-e952ed19: keyed on session id, never the mutable row name), `null`
//! while a succession heir has not checked in yet. A MISSING file reads as
//! an empty store; an unreadable or malformed one is an error, never
//! "no names". Writes take the exclusive sidecar flock and rename a temp
//! file over the target, the same way the registry does.

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrownNameRecord {
    pub name: String,
    pub regnal: u32,
    #[serde(default)]
    pub holder_session: Option<String>,
    #[serde(default)]
    pub nodes: Vec<String>,
    #[serde(default)]
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Store {
    version: u32,
    #[serde(default)]
    crowns: BTreeMap<String, CrownNameRecord>,
}

impl Store {
    fn empty() -> Self {
        Store {
            version: 1,
            crowns: BTreeMap::new(),
        }
    }
}

fn read(path: &Path) -> Result<Store, String> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Store::empty()),
        Err(e) => return Err(format!("crown names unreadable at {}: {e}", path.display())),
    };
    let store: Store = serde_json::from_slice(&bytes).map_err(|e| {
        format!(
            "crown names malformed at {} ({e}); fix or delete the file",
            path.display()
        )
    })?;
    if store.version != 1 {
        return Err(format!(
            "crown names at {} has version {}, this fno understands 1",
            path.display(),
            store.version
        ));
    }
    Ok(store)
}

#[cfg(test)]
fn write(path: &Path, store: &Store) -> Result<(), String> {
    let lock = crate::state::acquire_exclusive(&crate::state::lock_path(path))
        .map_err(|e| e.to_string())?;
    let out = crate::state::write_json_atomic(path, store).map_err(|e| e.to_string());
    let _ = lock.unlock();
    out
}

/// Read-modify-write under the exclusive sidecar lock, the registry's own
/// contract (`state::update_registry`): a check-in naming, a settle effect
/// and the daemon prune all mutate this file, and two writers that each
/// read-then-write outside one lock would lose a record.
fn update<T>(path: &Path, f: impl FnOnce(&mut Store) -> Result<T, String>) -> Result<T, String> {
    let lock = crate::state::acquire_exclusive(&crate::state::lock_path(path))
        .map_err(|e| e.to_string())?;
    let out = read(path).and_then(|mut store| {
        let value = f(&mut store)?;
        crate::state::write_json_atomic(path, &store).map_err(|e| e.to_string())?;
        Ok(value)
    });
    let _ = lock.unlock();
    out
}

fn now_stamp() -> String {
    crate::daemon::now_rfc3339_like()
}

/// `^[A-Za-z][A-Za-z'-]{1,23}$` - 2 to 24 chars, letters with apostrophes and
/// hyphens after the first. Hand-rolled: the pattern is the whole spec and no
/// regex crate rides in for it.
fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() >= 2
        && b.len() <= 24
        && b[0].is_ascii_alphabetic()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_alphabetic() || *c == b'\'' || *c == b'-')
}

/// The regnal display: first letter upper-cased, then ` II` and so on.
/// Regnal 0 and 1 carry no numeral; past the table the bare number reads.
const ROMAN: [&str; 19] = [
    "II", "III", "IV", "V", "VI", "VII", "VIII", "IX", "X", "XI", "XII", "XIII", "XIV", "XV",
    "XVI", "XVII", "XVIII", "XIX", "XX",
];

pub fn display(name: &str, regnal: u32) -> String {
    let mut chars = name.chars();
    let first = chars
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default();
    let numeral = match regnal {
        0 | 1 => String::new(),
        2..=20 => format!(" {}", ROMAN[(regnal - 2) as usize]),
        n => format!(" {n}"),
    };
    format!("{first}{}{numeral}", chars.as_str())
}

/// The live crowns indexed by canonical scope - the one liveness read every
/// rule here shares (`territory::live_crowns`, never a private copy).
fn live_index(registry_path: &Path) -> Result<BTreeMap<String, crate::territory::Crown>, String> {
    Ok(crate::territory::live_crowns(registry_path)
        .map_err(|e| e.0)?
        .into_iter()
        .map(|c| (c.scope.clone(), c))
        .collect())
}

/// Which records count: the scope has a live crown AND the record's
/// `holder_session` is null or that crown's session.
fn live_names_in(
    store: &Store,
    live: &BTreeMap<String, crate::territory::Crown>,
) -> BTreeMap<String, String> {
    store
        .crowns
        .iter()
        .filter_map(|(scope, rec)| {
            let crown = live.get(scope)?;
            let bound = rec.holder_session.as_deref();
            if bound.is_some() && bound != crown.holder_session.as_deref() {
                return None;
            }
            Some((scope.clone(), display(&rec.name, rec.regnal)))
        })
        .collect()
}

/// Scope -> display name for every record that counts as live.
pub fn live_names(
    store_path: &Path,
    registry_path: &Path,
) -> Result<BTreeMap<String, String>, String> {
    let store = read(store_path)?;
    let live = live_index(registry_path)?;
    Ok(live_names_in(&store, &live))
}

/// Name an un-named live crown. Refusals (pattern, a duplicate live name,
/// an already-named crown) name the holder so the king can pick again.
/// Success writes regnal 1 bound to the live holder's session and answers
/// the display string.
pub fn name_crown(
    store_path: &Path,
    registry_path: &Path,
    scope: &str,
    name: &str,
) -> Result<String, String> {
    let name = name.trim();
    if !valid_name(name) {
        return Err(format!(
            "a crown name is 2-24 letters (apostrophes and hyphens allowed after the first), got {name:?}"
        ));
    }
    let canon = crate::territory::canonical_scope(scope);
    let live = live_index(registry_path)?;
    let shown = update(store_path, |store| {
        let names = live_names_in(store, &live);
        if let Some(existing) = names.get(&canon) {
            if store
                .crowns
                .get(&canon)
                .is_some_and(|record| record.name.eq_ignore_ascii_case(name))
            {
                return Ok(existing.clone());
            }
            return Err(format!(
                "this crown is already named {existing}; the name belongs to the crown"
            ));
        }
        if let Some((held_scope, _)) = store.crowns.iter().find(|(s, rec)| {
            *s != &canon && names.contains_key(*s) && rec.name.eq_ignore_ascii_case(name)
        }) {
            let holder = live
                .get(held_scope)
                .map(|c| c.holder.as_str())
                .unwrap_or("another crown");
            return Err(format!(
                "the name {} is held by {holder} over {held_scope}; pick another name",
                display(
                    &store.crowns[held_scope].name,
                    store.crowns[held_scope].regnal
                )
            ));
        }
        let holder_session = live.get(&canon).and_then(|c| c.holder_session.clone());
        store.crowns.insert(
            canon.clone(),
            CrownNameRecord {
                name: name.to_string(),
                regnal: 1,
                holder_session,
                nodes: Vec::new(),
                updated_at: now_stamp(),
            },
        );
        Ok(display(name, 1))
    })?;
    ensure_named_crown(store_path, registry_path, &canon)?;
    Ok(shown)
}

/// Keep the live holder's registry label in step with the name bound to this
/// crown. Returns false when this live crown has no name yet.
pub fn ensure_named_crown(
    store_path: &Path,
    registry_path: &Path,
    scope: &str,
) -> Result<bool, String> {
    let canon = crate::territory::canonical_scope(scope);
    let live = live_index(registry_path)?;
    let Some(crown) = live.get(&canon) else {
        return Err(format!("no live crown holds {canon}"));
    };
    let store = read(store_path)?;
    let Some(rec) = store.crowns.get(&canon) else {
        return Ok(false);
    };
    if rec.holder_session.is_some() && rec.holder_session != crown.holder_session {
        return Ok(false);
    }
    let label = format!("king-{}", rec.name.to_ascii_lowercase());
    crate::state::rename_agent(registry_path, &crown.holder, &label, None)
        .map_err(|e| format!("rename crowned holder: {e}"))?;
    Ok(true)
}

/// Move the record from `old_scope` to `new_scope`, keeping name and regnal,
/// when the old record's holder is the live crown now holding `new_scope`
/// (a told-to re-scope). Otherwise refuse and name the holder.
pub fn keep_from(
    store_path: &Path,
    registry_path: &Path,
    old_scope: &str,
    new_scope: &str,
) -> Result<(), String> {
    let old = crate::territory::canonical_scope(old_scope);
    let new = crate::territory::canonical_scope(new_scope);
    let live = live_index(registry_path)?;
    update(store_path, |store| {
        let Some(rec) = store.crowns.get(&old).cloned() else {
            if store.crowns.get(&new).is_some_and(|rec| {
                live.get(&new).is_some_and(|crown| {
                    rec.holder_session.is_none() || rec.holder_session == crown.holder_session
                })
            }) {
                return Ok(());
            }
            return Err(format!(
                "no crown name is recorded over {old}; nothing to keep"
            ));
        };
        let Some(crown) = live.get(&new) else {
            return Err(format!("no live crown holds {new}"));
        };
        if rec.holder_session.is_none() || rec.holder_session != crown.holder_session {
            return Err(format!(
                "the name {} over {old} belongs to another holder ({}), not to {} holding {new}",
                display(&rec.name, rec.regnal),
                rec.holder_session
                    .as_deref()
                    .unwrap_or("(no session bound)"),
                crown.holder,
            ));
        }
        store.crowns.remove(&old);
        store.crowns.insert(
            new.clone(),
            CrownNameRecord {
                holder_session: crown.holder_session.clone(),
                nodes: Vec::new(),
                updated_at: now_stamp(),
                ..rec
            },
        );
        Ok(())
    })?;
    ensure_named_crown(store_path, registry_path, &new)?;
    Ok(())
}

/// A succession: regnal + 1, the heir unbound until its first beat binds it.
/// A crown with no record succeeds to no record.
pub fn carry_succession(store_path: &Path, scope: &str) -> Result<(), String> {
    let canon = crate::territory::canonical_scope(scope);
    update(store_path, |store| {
        if let Some(rec) = store.crowns.get_mut(&canon) {
            rec.regnal = rec.regnal.saturating_add(1);
            rec.holder_session = None;
            rec.updated_at = now_stamp();
        }
        Ok(())
    })
}

/// Drop the record (a fresh grant over the scope starts unnamed).
pub fn forget(store_path: &Path, scope: &str) -> Result<(), String> {
    let canon = crate::territory::canonical_scope(scope);
    update(store_path, |store| {
        store.crowns.remove(&canon);
        Ok(())
    })
}

/// An heir's (or any) beat: bind a null `holder_session` to the live
/// holder's session and replace the node list. No record, no-op. Callers
/// treat an error as a stated line in the beat, never a failed beat.
pub fn bind_and_refresh(
    store_path: &Path,
    registry_path: &Path,
    scope: &str,
    nodes: Vec<String>,
) -> Result<(), String> {
    let canon = crate::territory::canonical_scope(scope);
    let live = live_index(registry_path)?;
    update(store_path, |store| {
        let Some(rec) = store.crowns.get_mut(&canon) else {
            return Ok(());
        };
        if rec.holder_session.is_none() {
            if let Some(crown) = live.get(&canon) {
                rec.holder_session = crown.holder_session.clone();
            }
        }
        rec.nodes = nodes;
        rec.updated_at = now_stamp();
        Ok(())
    })
}

/// Drop every record `live_names` would not count. Answers the dropped
/// scope keys.
pub fn prune(store_path: &Path, registry_path: &Path) -> Result<Vec<String>, String> {
    let dropped = prune_dry(store_path, registry_path)?;
    if dropped.is_empty() {
        return Ok(dropped);
    }
    update(store_path, |store| {
        for scope in &dropped {
            store.crowns.remove(scope);
        }
        Ok(())
    })?;
    Ok(dropped)
}

/// [`prune`] without writing: the scopes the apply run would drop.
pub fn prune_dry(store_path: &Path, registry_path: &Path) -> Result<Vec<String>, String> {
    let store = read(store_path)?;
    let live = live_index(registry_path)?;
    let names = live_names_in(&store, &live);
    Ok(store
        .crowns
        .keys()
        .filter(|s| !names.contains_key(*s))
        .cloned()
        .collect())
}

/// The whole store as JSON, for `court --json -n` style readers. Not part of
/// the file contract; a debug convenience.
pub fn snapshot(store_path: &Path) -> Result<serde_json::Value, String> {
    let store = read(store_path)?;
    Ok(json!({
        "version": store.version,
        "crowns": store.crowns,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn store_path(tmp: &Path) -> PathBuf {
        tmp.join("crown_names.json")
    }

    fn registry_path(tmp: &Path) -> PathBuf {
        tmp.join("registry.json")
    }

    /// One live crown over `scope` held by `name` with `session`. The row
    /// shape mirrors territory.rs's registry_fixture.
    fn write_registry(tmp: &Path, rows: serde_json::Value) {
        let doc = json!({
            "schema_version": crate::state::REGISTRY_SCHEMA_VERSION,
            "agents": rows,
        });
        std::fs::write(registry_path(tmp), doc.to_string()).unwrap();
    }

    fn crown_row(name: &str, scope: &str, level: u8, session: &str) -> serde_json::Value {
        json!({
            "name": name, "status": "live", "crown_scope": scope,
            "crown_level": level, "cwd": "/repo", "harness": "claude",
            "harness_session_id": session,
            "created_at": "2026-09-23T20:00:00Z",
        })
    }

    #[test]
    fn naming_binds_regnal_one_to_the_holder_session_and_display_capitalizes() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_registry(tmp.path(), json!([crown_row("king-a", "fno", 1, "sess-a")]));
        let shown = name_crown(
            &store_path(tmp.path()),
            &registry_path(tmp.path()),
            "fno",
            "barnaby",
        )
        .unwrap();
        assert_eq!(shown, "Barnaby");
        let names = live_names(&store_path(tmp.path()), &registry_path(tmp.path())).unwrap();
        assert_eq!(names.get("fno").unwrap(), "Barnaby");
        let registry = crate::state::load_registry(&registry_path(tmp.path())).unwrap();
        assert_eq!(registry.entries[0].name, "king-barnaby");
        let dump = snapshot(&store_path(tmp.path())).unwrap();
        assert_eq!(dump["crowns"]["fno"]["regnal"], json!(1));
        assert_eq!(dump["crowns"]["fno"]["holder_session"], json!("sess-a"));
    }

    #[test]
    fn retrying_the_same_name_repairs_a_label_rename_refusal() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_registry(
            tmp.path(),
            json!([
                crown_row("king-a", "fno", 1, "sess-a"),
                crown_row("king-barnaby", "x-aaaa", 2, "sess-b")
            ]),
        );
        let store = store_path(tmp.path());
        let registry = registry_path(tmp.path());
        let refused = name_crown(&store, &registry, "fno", "barnaby").unwrap_err();
        assert!(
            refused.contains("already names another worker"),
            "{refused}"
        );

        write_registry(tmp.path(), json!([crown_row("king-a", "fno", 1, "sess-a")]));
        assert_eq!(
            name_crown(&store, &registry, "fno", "BARNABY").unwrap(),
            "Barnaby"
        );
        let registry = crate::state::load_registry(&registry).unwrap();
        assert_eq!(registry.entries[0].name, "king-barnaby");
    }

    #[test]
    fn a_duplicate_live_name_refuses_and_names_the_holder_and_scope() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_registry(
            tmp.path(),
            json!([
                crown_row("king-a", "x-aaaa", 2, "sess-a"),
                crown_row("king-b", "fno", 1, "sess-b"),
            ]),
        );
        name_crown(
            &store_path(tmp.path()),
            &registry_path(tmp.path()),
            "x-aaaa",
            "barnaby",
        )
        .unwrap();
        let err = name_crown(
            &store_path(tmp.path()),
            &registry_path(tmp.path()),
            "fno",
            "BARNABY",
        )
        .unwrap_err();
        assert!(err.contains("king-barnaby"), "{err}");
        assert!(err.contains("x-aaaa"), "{err}");
    }

    #[test]
    fn an_already_named_crown_refuses_a_second_naming() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_registry(tmp.path(), json!([crown_row("king-a", "fno", 1, "sess-a")]));
        name_crown(
            &store_path(tmp.path()),
            &registry_path(tmp.path()),
            "fno",
            "barnaby",
        )
        .unwrap();
        let err = name_crown(
            &store_path(tmp.path()),
            &registry_path(tmp.path()),
            "fno",
            "ernest",
        )
        .unwrap_err();
        assert!(err.contains("already named Barnaby"), "{err}");
    }

    #[test]
    fn a_bad_name_pattern_refuses() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_registry(tmp.path(), json!([crown_row("king-a", "fno", 1, "sess-a")]));
        let err = name_crown(
            &store_path(tmp.path()),
            &registry_path(tmp.path()),
            "fno",
            "1x",
        )
        .unwrap_err();
        assert!(err.contains("2-24 letters"), "{err}");
        assert!(name_crown(
            &store_path(tmp.path()),
            &registry_path(tmp.path()),
            "fno",
            "o'brien-x"
        )
        .is_ok());
        let registry = crate::state::load_registry(&registry_path(tmp.path())).unwrap();
        assert_eq!(registry.entries[0].name, "king-o'brien-x");
    }

    #[test]
    fn carry_succession_bumps_regnal_and_unbinds_twice_for_two_successions() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_registry(
            tmp.path(),
            json!([crown_row("king-a", "x-aaaa", 2, "sess-a")]),
        );
        name_crown(
            &store_path(tmp.path()),
            &registry_path(tmp.path()),
            "x-aaaa",
            "barnaby",
        )
        .unwrap();
        carry_succession(&store_path(tmp.path()), "x-aaaa").unwrap();
        let dump = snapshot(&store_path(tmp.path())).unwrap();
        assert_eq!(dump["crowns"]["x-aaaa"]["regnal"], json!(2));
        assert_eq!(dump["crowns"]["x-aaaa"]["holder_session"], json!(null));
        // A second succession before the heir checks in reads regnal 3.
        carry_succession(&store_path(tmp.path()), "x-aaaa").unwrap();
        let dump = snapshot(&store_path(tmp.path())).unwrap();
        assert_eq!(dump["crowns"]["x-aaaa"]["regnal"], json!(3));
    }

    #[test]
    fn an_heirs_first_beat_binds_the_carried_name_and_refreshes_nodes() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_registry(
            tmp.path(),
            json!([crown_row("king-heir", "x-aaaa", 2, "sess-heir")]),
        );
        name_crown(
            &store_path(tmp.path()),
            &registry_path(tmp.path()),
            "x-aaaa",
            "barnaby",
        )
        .unwrap();
        carry_succession(&store_path(tmp.path()), "x-aaaa").unwrap();
        bind_and_refresh(
            &store_path(tmp.path()),
            &registry_path(tmp.path()),
            "x-aaaa",
            vec!["x-bbbb".into()],
        )
        .unwrap();
        let dump = snapshot(&store_path(tmp.path())).unwrap();
        assert_eq!(
            dump["crowns"]["x-aaaa"]["holder_session"],
            json!("sess-heir")
        );
        assert_eq!(dump["crowns"]["x-aaaa"]["nodes"], json!(["x-bbbb"]));
        // The carried record still counts while unbound (null session).
    }

    #[test]
    fn a_fresh_grant_forgets_and_a_dead_crown_is_pruned_only_on_apply() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_registry(tmp.path(), json!([crown_row("king-a", "fno", 1, "sess-a")]));
        name_crown(
            &store_path(tmp.path()),
            &registry_path(tmp.path()),
            "fno",
            "barnaby",
        )
        .unwrap();
        forget(&store_path(tmp.path()), "fno").unwrap();
        assert!(snapshot(&store_path(tmp.path())).unwrap()["crowns"]
            .as_object()
            .unwrap()
            .is_empty());

        name_crown(
            &store_path(tmp.path()),
            &registry_path(tmp.path()),
            "fno",
            "barnaby",
        )
        .unwrap();
        // The scope's crown dies (registry row gone): the record is stale.
        write_registry(tmp.path(), json!([]));
        let dropped = prune(&store_path(tmp.path()), &registry_path(tmp.path())).unwrap();
        assert_eq!(dropped, ["fno"]);
    }

    #[test]
    fn keep_from_moves_the_record_only_to_the_same_holder() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_registry(
            tmp.path(),
            json!([crown_row("king-a", "new-scope", 1, "sess-a")]),
        );
        // A record over the old scope bound to sess-a.
        let store = Store {
            version: 1,
            crowns: BTreeMap::from([(
                "old-scope".into(),
                CrownNameRecord {
                    name: "barnaby".into(),
                    regnal: 2,
                    holder_session: Some("sess-a".into()),
                    nodes: vec!["x-1".into()],
                    updated_at: now_stamp(),
                },
            )]),
        };
        write(&store_path(tmp.path()), &store).unwrap();
        keep_from(
            &store_path(tmp.path()),
            &registry_path(tmp.path()),
            "old-scope",
            "new-scope",
        )
        .unwrap();
        let dump = snapshot(&store_path(tmp.path())).unwrap();
        assert!(dump["crowns"].get("old-scope").is_none());
        assert_eq!(dump["crowns"]["new-scope"]["name"], json!("barnaby"));
        assert_eq!(dump["crowns"]["new-scope"]["regnal"], json!(2));
        let registry = crate::state::load_registry(&registry_path(tmp.path())).unwrap();
        assert_eq!(registry.entries[0].name, "king-barnaby");

        // A different holder is refused and names the holder.
        let store = Store {
            version: 1,
            crowns: BTreeMap::from([(
                "old-scope".into(),
                CrownNameRecord {
                    name: "barnaby".into(),
                    regnal: 2,
                    holder_session: Some("sess-other".into()),
                    nodes: Vec::new(),
                    updated_at: now_stamp(),
                },
            )]),
        };
        write(&store_path(tmp.path()), &store).unwrap();
        let err = keep_from(
            &store_path(tmp.path()),
            &registry_path(tmp.path()),
            "old-scope",
            "new-scope",
        )
        .unwrap_err();
        assert!(err.contains("sess-other"), "{err}");
    }

    #[test]
    fn a_missing_store_reads_empty_and_a_malformed_one_is_an_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_registry(tmp.path(), json!([crown_row("king-a", "fno", 1, "sess-a")]));
        let names = live_names(&store_path(tmp.path()), &registry_path(tmp.path())).unwrap();
        assert!(names.is_empty());
        std::fs::write(store_path(tmp.path()), "{not json").unwrap();
        assert!(live_names(&store_path(tmp.path()), &registry_path(tmp.path())).is_err());
    }

    #[test]
    fn display_uses_the_numeral_table_then_the_bare_number() {
        assert_eq!(display("barnaby", 1), "Barnaby");
        assert_eq!(display("barnaby", 2), "Barnaby II");
        assert_eq!(display("barnaby", 3), "Barnaby III");
        assert_eq!(display("barnaby", 20), "Barnaby XX");
        assert_eq!(display("barnaby", 21), "Barnaby 21");
    }
}
