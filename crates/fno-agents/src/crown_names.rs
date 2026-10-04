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
//!   "nodes": ["x-aaaa"], "updated_at": "2026-09-23T20:00:00Z",
//!   "theme": "native backlog", "title": "Lead of native backlog",
//!   "reign": {"session": "<uuid>", "scope": "<canonical scope>",
//!             "armed_at": "<ts>", "started_at": "<ts>",
//!             "term": "span:200h"} | absent}}}
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

use crate::loopcheck::KingManifest;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The succession carried but not yet proven. Skipped in the JSON while
    /// absent, so the frozen wire shape above holds unless a succession is
    /// mid-flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_succession: Option<PendingSuccession>,
    /// The reign clock carried across a re-scope (see [`ReignClock`]).
    /// Skipped in the JSON while absent, like `pending_succession`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reign: Option<ReignClock>,
}

/// A succession carried but not yet proven: written when `carry_succession`
/// nulls the holder, cleared when the heir's beat refreshes the record or a
/// fresh grant forgets it, and reverted by the reap sweep once the heir is
/// provably gone unbound.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingSuccession {
    pub heir_name: String,
    /// The heir row's session id at settle time, so the revert's join keys
    /// on identity and a rename between write and read cannot break it.
    /// Absent on legacy records, which keep the name join.
    #[serde(default)]
    pub heir_session: Option<String>,
    pub predecessor_name: String,
    #[serde(default)]
    pub predecessor_session: Option<String>,
    pub ts: String,
}

/// When a holder's reign began and the term it declared. Keyed on the
/// session: a re-scope by the same session carries it, a new session (an
/// heir) or a same-scope re-arm starts fresh.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReignClock {
    pub session: String,
    /// Canonical scope of the manifest last stamped.
    pub scope: String,
    /// That manifest's created_at.
    pub armed_at: String,
    /// The reign's first arm.
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term: Option<String>,
}

/// One reverted succession: the receipt the reap sweep reports and journals.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RevertedSuccession {
    pub scope: String,
    pub heir_name: String,
    pub predecessor_name: String,
    pub predecessor_session: Option<String>,
    pub evidence: String,
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

/// The people title for a level (L0 Chief of a portfolio, L1 Head of a
/// project, L2 Lead of a theme). `scope` is the fallback text when no theme
/// names the lead's epics, so a Chief with no named portfolio reads
/// "Chief of" its project list.
pub fn title(level: u32, scope: &str, theme: Option<&str>) -> String {
    match level {
        0 => format!("Chief of {}", theme.unwrap_or(scope)),
        1 => format!("Head of {scope}"),
        2 => format!("Lead of {}", theme.unwrap_or(scope)),
        n => format!("L{n} {scope}"),
    }
}

/// The pre-title rank string, `L{level} {scope}` - what queued mail from
/// before the upgrade carries and what every stored rank must still verify
/// against during the one-release window.
pub fn legacy_label(level: u32, scope: &str) -> String {
    format!("L{level} {scope}")
}

/// A theme is 2 to 40 characters of letters, digits, spaces, hyphens and
/// apostrophes. No quote or angle bracket: the title rides inside a quoted
/// mail attribute.
fn valid_theme(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() >= 2
        && b.len() <= 40
        && b[0].is_ascii_alphanumeric()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b' ' || *c == b'\'' || *c == b'-')
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

/// Scope -> stored people title for every record that counts as live (the
/// same liveness rule [`live_names`] applies). A record with no title yet is
/// skipped, so the fold stamp reads `null` and the ledger falls back.
pub fn live_titles(
    store_path: &Path,
    registry_path: &Path,
) -> Result<BTreeMap<String, String>, String> {
    let store = read(store_path)?;
    let live = live_index(registry_path)?;
    Ok(store
        .crowns
        .iter()
        .filter_map(|(scope, rec)| {
            let crown = live.get(scope)?;
            let bound = rec.holder_session.as_deref();
            if bound.is_some() && bound != crown.holder_session.as_deref() {
                return None;
            }
            Some((scope.clone(), rec.title.clone()?))
        })
        .collect())
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
        check_duplicate_name(store, &names, &live, &canon, name)?;
        let holder_session = live.get(&canon).and_then(|c| c.holder_session.clone());
        store.crowns.insert(
            canon.clone(),
            CrownNameRecord {
                name: name.to_string(),
                regnal: 1,
                holder_session,
                nodes: Vec::new(),
                updated_at: now_stamp(),
                theme: None,
                title: live
                    .get(&canon)
                    .map(|c| title(c.level as u32, &canon, None)),
                pending_succession: None,
                reign: None,
            },
        );
        Ok(display(name, 1))
    })?;
    ensure_named_crown(store_path, registry_path, &canon)?;
    Ok(shown)
}

fn check_duplicate_name(
    store: &Store,
    names: &BTreeMap<String, String>,
    live: &BTreeMap<String, crate::territory::Crown>,
    canon: &str,
    name: &str,
) -> Result<(), String> {
    if let Some((held_scope, _)) = store.crowns.iter().find(|(scope, rec)| {
        scope.as_str() != canon && names.contains_key(*scope) && rec.name.eq_ignore_ascii_case(name)
    }) {
        let holder = live
            .get(held_scope)
            .map(|crown| crown.holder.as_str())
            .unwrap_or("another crown");
        return Err(format!(
            "the name {} is held by {holder} over {held_scope}; pick another name",
            display(
                &store.crowns[held_scope].name,
                store.crowns[held_scope].regnal
            )
        ));
    }
    Ok(())
}

/// Rename the named live crown held by `session`, or return `None` when the
/// session does not hold one. With `apply = false`, validate without writing.
pub fn rename_crown(
    store_path: &Path,
    registry_path: &Path,
    session: &str,
    new_name: &str,
    apply: bool,
) -> Result<Option<(String, String)>, String> {
    if !valid_name(new_name) {
        let live = live_index(registry_path)?;
        let holder = live
            .values()
            .find(|crown| crown.holder_session.as_deref() == Some(session));
        if let Some(crown) = holder {
            return Err(format!(
                "{} holds the crown over {}, so its name is the crown name: 2-24 letters (apostrophes and hyphens after the first), got {new_name:?}",
                crown.holder, crown.scope
            ));
        }
        return Ok(None);
    }
    let live = live_index(registry_path)?;
    let Some((scope, _crown)) = live
        .iter()
        .find(|(_, crown)| crown.holder_session.as_deref() == Some(session))
    else {
        return Ok(None);
    };
    let canon = crate::territory::canonical_scope(scope);
    let prior = read(store_path)?;
    let Some(record) = prior.crowns.get(&canon) else {
        return Ok(None);
    };
    if record
        .holder_session
        .as_deref()
        .is_some_and(|holder| holder != session)
    {
        return Ok(None);
    }
    if !apply {
        let from = display(&record.name, record.regnal);
        let to = display(new_name, 1);
        let names = live_names_in(&prior, &live);
        check_duplicate_name(&prior, &names, &live, &canon, new_name)?;
        return Ok(Some((from, to)));
    }
    let result = update(store_path, |store| {
        let names = live_names_in(store, &live);
        check_duplicate_name(store, &names, &live, &canon, new_name)?;
        let current = store
            .crowns
            .get(&canon)
            .ok_or_else(|| format!("no named crown over {canon}"))?;
        if current
            .holder_session
            .as_deref()
            .is_some_and(|holder| holder != session)
        {
            return Err(format!("crown over {canon} changed holder before rename"));
        }
        let from = display(&current.name, current.regnal);
        let to = display(new_name, 1);
        let record = store
            .crowns
            .get_mut(&canon)
            .ok_or_else(|| format!("no named crown over {canon}"))?;
        record.name = new_name.to_string();
        record.regnal = 1;
        record.holder_session = Some(session.to_string());
        record.pending_succession = None;
        record.updated_at = now_stamp();
        Ok((from, to))
    })?;
    Ok(Some(result))
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
    let label = rec.name.to_ascii_lowercase();
    // A succession the predecessor survived leaves the carried label parked
    // on its still-live row. A row holding no live crown keeps nothing: the
    // label and alias move off it in the SAME transaction that names the new
    // holder, so the label never resolves to two rows. The predicate reads
    // the store and the transaction's own rows, so the verdict under the
    // lock is as fresh as the two files allow.
    let displaceable = |row: &crate::state::RegistryEntry,
                        entries: &[crate::state::RegistryEntry]| {
        let Some(sid) = row.harness_session_id.as_deref() else {
            return false;
        };
        if holds_live_crown(entries, sid) {
            return false;
        }
        let Ok(current) = read(store_path) else {
            return false;
        };
        current
            .crowns
            .values()
            .all(|r| r.holder_session.as_deref() != Some(sid))
    };
    crate::state::rename_agent_displacing(registry_path, &crown.holder, &label, None, displaceable)
        .map_err(|e| format!("rename crowned holder: {e}"))?;
    Ok(true)
}

/// Whether `sid` is the first liveish row over some non-empty canonical
/// scope - `territory::live_crowns`'s holder rule, run over the caller's
/// rows so the displacement verdict cannot read a pre-transaction snapshot.
fn holds_live_crown(entries: &[crate::state::RegistryEntry], sid: &str) -> bool {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for row in entries {
        let raw = row.crown_scope.as_deref().unwrap_or("").trim();
        if raw.is_empty() || !crate::spawn_gate::status_is_liveish(&row.status) {
            continue;
        }
        let canon = crate::territory::canonical_scope(raw);
        if canon.is_empty() || !seen.insert(canon) {
            continue;
        }
        if row.harness_session_id.as_deref() == Some(sid) {
            return true;
        }
    }
    false
}

/// Move the record from `old_scope` to `new_scope`, keeping name, regnal,
/// theme and title, when the old record's holder is the live crown now
/// holding `new_scope` (a told-to re-scope). Otherwise refuse and name the
/// holder.
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
        // The theme belongs to the lead, not the scope: a re-scope carries
        // it, so growing the epic list never drops the rank back to the raw
        // scope text. The title always rebuilds from the landing crown's
        // level and the carried theme - the same string when the level is
        // unchanged, the right rank on a level change - and an L1 takes no
        // theme at all (set_theme refuses one), so a re-scope that lands on
        // a Head row drops the carried theme with it.
        let carried_theme = rec.theme.clone().filter(|_| crown.level != 1);
        let carried_title = Some(title(crown.level as u32, &new, carried_theme.as_deref()));
        store.crowns.insert(
            new.clone(),
            CrownNameRecord {
                holder_session: crown.holder_session.clone(),
                nodes: Vec::new(),
                updated_at: now_stamp(),
                theme: carried_theme,
                title: carried_title,
                ..rec
            },
        );
        Ok(())
    })?;
    ensure_named_crown(store_path, registry_path, &new)?;
    Ok(())
}

/// A succession: regnal + 1, the heir unbound until its first beat binds it.
/// `pending` records the succession so the reap sweep can revert it when the
/// heir proves unable to bind; `None` (an old caller) keeps today's shape.
/// A crown with no record succeeds to no record.
pub fn carry_succession(
    store_path: &Path,
    scope: &str,
    pending: Option<PendingSuccession>,
) -> Result<(), String> {
    let canon = crate::territory::canonical_scope(scope);
    update(store_path, |store| {
        if let Some(rec) = store.crowns.get_mut(&canon) {
            rec.regnal = rec.regnal.saturating_add(1);
            rec.holder_session = None;
            rec.pending_succession = pending;
            rec.updated_at = now_stamp();
        }
        Ok(())
    })
}

/// RFC3339 compare that survives `Z` and `+00:00` spellings; a string
/// compare backstops an unparsable stamp.
fn ts_earlier(a: &str, b: &str) -> String {
    match (
        chrono::DateTime::parse_from_rfc3339(a),
        chrono::DateTime::parse_from_rfc3339(b),
    ) {
        (Ok(x), Ok(y)) => {
            if x <= y {
                a.to_string()
            } else {
                b.to_string()
            }
        }
        _ if a <= b => a.to_string(),
        _ => b.to_string(),
    }
}

/// The live clock for this manifest, if one applies. Candidates are every
/// record whose `reign.session` names the manifest's own session; the
/// newest `armed_at` wins, parsed as a `DateTime` because `Z` and `+00:00`
/// spellings coexist in written stores. It applies on a re-scope not yet
/// stamped (the clock's scope differs and this manifest is newer than the
/// clock's arm) or on the scope it was stamped for; a same-scope re-arm
/// (a newer manifest, same scope) and an older manifest apply nothing.
fn clock_for(store: &Store, m: &KingManifest) -> Option<ReignClock> {
    let session = m
        .harness_session_id
        .as_deref()
        .filter(|s| !s.trim().is_empty())?;
    let scope = crate::territory::canonical_scope(&m.scope);
    let created = m.created_at.as_deref().unwrap_or_default();
    let mut best: Option<ReignClock> = None;
    for rec in store.crowns.values() {
        let Some(reign) = rec.reign.as_ref() else {
            continue;
        };
        if reign.session != session || reign.armed_at.is_empty() {
            continue;
        }
        let take = match best.as_ref() {
            None => true,
            Some(cur) => match (
                chrono::DateTime::parse_from_rfc3339(&reign.armed_at),
                chrono::DateTime::parse_from_rfc3339(&cur.armed_at),
            ) {
                (Ok(a), Ok(b)) => a > b,
                _ => reign.armed_at.as_str() > cur.armed_at.as_str(),
            },
        };
        if take {
            best = Some(reign.clone());
        }
    }
    let clock = best?;
    let applies = if clock.scope != scope {
        !created.is_empty()
            && match (
                chrono::DateTime::parse_from_rfc3339(created),
                chrono::DateTime::parse_from_rfc3339(&clock.armed_at),
            ) {
                (Ok(c), Ok(a)) => c >= a,
                _ => created >= clock.armed_at.as_str(),
            }
    } else {
        clock.armed_at == created
    };
    applies.then_some(clock)
}

/// The manifest as the reign reads it: with a carried clock, the start is
/// the reign's first arm and an undeclared term takes the carried one. An
/// unreadable store returns the manifest unchanged.
pub(crate) fn reign_view_in(store_path: &Path, m: &KingManifest) -> KingManifest {
    let Ok(store) = read(store_path) else {
        return m.clone();
    };
    let view_from = |clock: ReignClock| {
        let mut view = m.clone();
        view.created_at = Some(match m.created_at.as_deref() {
            Some(created) if !created.is_empty() => ts_earlier(&clock.started_at, created),
            _ => clock.started_at.clone(),
        });
        view.term = m.term.clone().or(clock.term);
        view
    };
    clock_for(&store, m).map_or_else(|| m.clone(), view_from)
}

/// [`reign_view_in`] against the ambient agents home. `None` there (a test
/// that declared no home) reads the manifest unchanged.
pub(crate) fn reign_view(m: &KingManifest) -> KingManifest {
    match crate::paths::AgentsHome::from_env_opt() {
        Some(home) => reign_view_in(&home.crown_names_json(), m),
        None => m.clone(),
    }
}

/// Stamp the reign clock for this manifest arm onto the record for its
/// canonical scope, inside the store lock. A carried clock keeps the
/// earlier start and the declared term; no record, or no session, stamps
/// nothing - the clock never creates a record.
pub(crate) fn stamp_reign(
    store_path: &Path,
    m: &KingManifest,
    term: Option<&str>,
) -> Result<(), String> {
    let Some(session) = m
        .harness_session_id
        .clone()
        .filter(|s| !s.trim().is_empty())
    else {
        return Ok(());
    };
    let Some(created) = m.created_at.clone().filter(|c| !c.trim().is_empty()) else {
        return Ok(());
    };
    let canon = crate::territory::canonical_scope(&m.scope);
    update(store_path, |store| {
        let carried = clock_for(store, m);
        let Some(rec) = store.crowns.get_mut(&canon) else {
            return Ok(());
        };
        rec.reign = Some(ReignClock {
            session,
            scope: canon.clone(),
            armed_at: created.clone(),
            started_at: match &carried {
                Some(c) => ts_earlier(&c.started_at, &created),
                None => created.clone(),
            },
            term: term
                .map(str::to_string)
                .or_else(|| m.term.clone())
                .or_else(|| carried.as_ref().and_then(|c| c.term.clone())),
        });
        rec.updated_at = now_stamp();
        Ok(())
    })
}

/// Stamp the beat's manifest arm and return the holder session it named.
/// Best-effort: any failure prints one warning and never fails the beat.
pub(crate) fn stamp_beat_reign(store_path: &Path, cwd: &Path, scope: &str) -> Option<String> {
    let stamped = (|| -> Result<String, String> {
        // The manifest lives under the canonical scope; a caller passing an
        // unsorted member list still reads its own armed manifest.
        let path = crate::loop_reign::manifest_path(
            &crate::paths::space_dir(cwd),
            &crate::territory::canonical_scope(scope),
        )?;
        let content =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let manifest = crate::loopcheck::parse_king_manifest(&content)
            .ok_or_else(|| format!("{}: no frontmatter", path.display()))?;
        stamp_reign(store_path, &manifest, None)?;
        Ok(manifest.harness_session_id.unwrap_or_default())
    })();
    match stamped {
        Ok(s) if !s.is_empty() => Some(s),
        Ok(_) => None,
        Err(e) => {
            eprintln!("king-checkin: WARNING: reign clock not stamped: {e}");
            None
        }
    }
}

/// Set (once per scope) a lead's theme. The crown must be live and named;
/// L1 refuses ("Head of <project> takes the project name; no theme"); the
/// same theme again is a no-op; a different theme names the current one and
/// refuses (set once per scope, like the name).
pub fn set_theme(
    store_path: &Path,
    registry_path: &Path,
    scope: &str,
    theme: &str,
) -> Result<String, String> {
    let theme = theme.trim();
    if !valid_theme(theme) {
        return Err(format!(
            "a theme is 2-40 characters (letters, digits, spaces, hyphens and apostrophes; no quote or angle bracket), got {theme:?}"
        ));
    }
    let canon = crate::territory::canonical_scope(scope);
    let live = live_index(registry_path)?;
    update(store_path, |store| {
        let names = live_names_in(store, &live);
        if !names.contains_key(&canon) {
            return Err("every live crown needs a name; check in with --name <name>".into());
        }
        if live.get(&canon).is_some_and(|c| c.level == 1) {
            return Err("Head of <project> takes the project name; no theme".into());
        }
        let verdict = {
            let rec = store
                .crowns
                .get(&canon)
                .expect("the named check passed, so the record exists");
            match rec.theme.as_deref() {
                Some(existing) if existing == theme => Ok(rec
                    .title
                    .clone()
                    .unwrap_or_else(|| title(live[&canon].level as u32, &canon, Some(existing)))),
                Some(existing) => Err(format!(
                    "this lead's theme is already {existing:?}; the theme is set once per scope, like the name"
                )),
                None => Ok(String::new()),
            }
        };
        verdict?;
        let rec = store
            .crowns
            .get_mut(&canon)
            .expect("the named check passed, so the record exists");
        rec.theme = Some(theme.to_string());
        rec.title = Some(title(live[&canon].level as u32, &canon, Some(theme)));
        rec.updated_at = now_stamp();
        Ok(rec.title.clone().unwrap_or_default())
    })
}

/// The live theme over `scope`, or None. A missing or malformed store reads
/// as no theme (the tolerant read the mail envelope uses).
pub fn theme_for(store_path: &Path, scope: &str) -> Option<String> {
    let canon = crate::territory::canonical_scope(scope);
    let store = read(store_path).ok()?;
    store.crowns.get(&canon)?.theme.clone()
}

/// Every stored theme keyed by canonical scope, tolerant (a missing or
/// malformed file reads as empty) - the once-per-fold read the feed's
/// crown rows and owner text render from.
pub fn theme_map(store_path: &Path) -> BTreeMap<String, String> {
    let store = match read(store_path) {
        Ok(s) => s,
        Err(_) => return BTreeMap::new(),
    };
    store
        .crowns
        .into_iter()
        .filter_map(|(scope, rec)| rec.theme.map(|t| (scope, t)))
        .collect()
}

/// The stored people title over `scope`, tolerant like [`theme_for`].
pub fn stored_title(store_path: &Path, scope: &str) -> Option<String> {
    let canon = crate::territory::canonical_scope(scope);
    let store = read(store_path).ok()?;
    store.crowns.get(&canon)?.title.clone()
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
        // A beat over the scope by the live holder is the proof the
        // succession waited for: the heir is alive and reading its crown.
        rec.pending_succession = None;
        rec.nodes = nodes;
        rec.updated_at = now_stamp();
        Ok(())
    })
}

/// Revert successions whose heir died before binding. A pending record
/// reverts only when the heir never bound (`holder_session` still null),
/// the window elapsed, and the heir's registry row is gone (the codex
/// bind-window reaper removes rows) or terminal. The predecessor's session
/// is restored so resume is the recovery path; the regnal stays bumped, a
/// monotonic lineage counter. `apply: false` reports the same lists and
/// writes nothing, the sweep's dry-run contract. Answers the reverted rows
/// and the kept reasons (a live heir keeps its succession).
pub fn revert_stale_pending(
    store_path: &Path,
    registry_path: &Path,
    now: chrono::DateTime<chrono::Utc>,
    window_s: i64,
    apply: bool,
) -> Result<(Vec<RevertedSuccession>, Vec<String>), String> {
    let mut reverted = Vec::new();
    let mut kept = Vec::new();
    let store_doc = read(store_path)?;
    let mut stale: Vec<(String, PendingSuccession)> = Vec::new();
    for (scope, rec) in &store_doc.crowns {
        let Some(pending) = rec.pending_succession.as_ref() else {
            continue;
        };
        if rec.holder_session.is_some() {
            continue;
        }
        let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&pending.ts) else {
            kept.push(format!("{scope}: pending succession has an unparsable ts"));
            continue;
        };
        if now
            .signed_duration_since(ts.with_timezone(&chrono::Utc))
            .num_seconds()
            <= window_s
        {
            continue;
        }
        stale.push((scope.clone(), pending.clone()));
    }
    if stale.is_empty() {
        return Ok((reverted, kept));
    }
    let reg = crate::state::load_registry(registry_path)
        .map_err(|e| format!("registry unreadable for succession revert: {e}"))?;
    for (scope, pending) in stale {
        // Id-first: a record carrying heir_session resolves through the
        // session id (a rename cannot break the join); a legacy record
        // falls back to the name join and its ambiguity refusal, unchanged.
        let join = match pending.heir_session.as_deref() {
            Some(session) => crate::agent_ref::resolve(
                &reg.entries,
                crate::agent_ref::Key::Id(session),
                |row| !crate::loop_reign::is_terminal(row),
            ),
            None => crate::loop_reign::live_name_join(&reg.entries, &pending.heir_name),
        };
        let evidence = match join {
            crate::loop_reign::NameJoin::One(row) => {
                kept.push(format!("{scope}: heir row {} still {:?}", row.name, row.status));
                continue;
            }
            crate::loop_reign::NameJoin::Ambiguous => {
                kept.push(format!(
                    "{scope}: heir row {} name ambiguous; revert refused",
                    pending.heir_name
                ));
                continue;
            }
            crate::loop_reign::NameJoin::None => {
                // No live row answers the heir; the raw matches are all
                // terminal by the join's construction, so the first names
                // why the succession reverts.
                let raw = match pending.heir_session.as_deref() {
                    Some(session) => reg.entries.iter().find(|e| {
                        e.harness_session_id.as_deref() == Some(session)
                            || e.related_session_id.as_deref() == Some(session)
                    }),
                    None => reg.entries.iter().find(|e| {
                        e.name == pending.heir_name
                            || e.aliases.iter().any(|a| *a == pending.heir_name)
                    }),
                };
                match raw {
                    None => "heir row removed".to_string(),
                    Some(row) => format!("heir row {:?}", row.status),
                }
            }
        };
        if apply {
            // The write-lock re-check: a bind landing between the classify
            // above and this write must not be clobbered by the revert, and
            // a declined write must never read as a reverted succession.
            let mut written = false;
            update(store_path, |store| {
                if let Some(rec) = store.crowns.get_mut(&scope) {
                    if rec.holder_session.is_none() && rec.pending_succession.is_some() {
                        rec.holder_session = pending.predecessor_session.clone();
                        rec.pending_succession = None;
                        rec.updated_at = now_stamp();
                        written = true;
                    }
                }
                Ok(())
            })?;
            if !written {
                kept.push(format!("{scope}: heir bound mid-sweep; revert skipped"));
                continue;
            }
        }
        reverted.push(RevertedSuccession {
            scope,
            heir_name: pending.heir_name,
            predecessor_name: pending.predecessor_name,
            predecessor_session: pending.predecessor_session,
            evidence,
        });
    }
    Ok((reverted, kept))
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

/// Apply `--name` / `--keep-name-from` / `--theme` before the beat runs. A
/// refusal (duplicate live name, already-named crown, wrong holder, missing
/// name or theme) names the holder or the missing flag; the caller prints
/// it and exits 2 with no beat journalled.
pub fn apply_crown_naming(
    store_path: &Path,
    registry_path: &Path,
    name: Option<&str>,
    rescope_from: Option<&str>,
    theme: Option<&str>,
    level: Option<i64>,
    scope: &str,
) -> Result<(), String> {
    match (name, rescope_from) {
        (Some(_), Some(_)) => Err("use one of --name or --keep-name-from, not both".into()),
        (Some(n), None) => name_crown(&store_path, &registry_path, scope, n).map(|_| ()),
        (None, Some(old)) => crate::crown_names::keep_from(&store_path, &registry_path, old, scope),
        (None, None) => Ok(()),
    }?;
    if let Some(theme) = theme {
        set_theme(&store_path, &registry_path, scope, theme)?;
    }
    if !ensure_named_crown(&store_path, &registry_path, scope)? {
        return Err("every live crown needs a name; check in with --name <name>".into());
    }
    if matches!(level, Some(0) | Some(2)) && theme_for(&store_path, scope).is_none() {
        return Err(
            "every lead names its theme once per scope; check in with --theme <theme>".into(),
        );
    }
    Ok(())
}

/// The first line of every beat: identity first, then the facts. An unnamed
/// lead gets the once-only instruction instead of a name.
pub fn crown_line_text(name: Option<&str>, title_txt: &str, scope: &str) -> String {
    match name {
        Some(name) => format!("{name}, {title_txt} ({scope})"),
        None => "unnamed lead - name it once: fno agents org checkin --name <name>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_checkin_naming_and_theme_flows() {
        fn an_unnamed_live_crown_cannot_complete_checkin() {
            let tmp = tempfile::TempDir::new().unwrap();
            write_registry(tmp.path(), json!([crown_row("king-a", "fno", 1, "sess-a")]));
            let err = apply_crown_naming(
                &store_path(tmp.path()),
                &registry_path(tmp.path()),
                None,
                None,
                None,
                Some(1),
                "fno",
            )
            .unwrap_err();
            assert!(err.contains("every live crown needs a name"), "{err}");
        }

        fn a_successor_checkin_carries_the_name_into_its_registry_label() {
            let tmp = tempfile::TempDir::new().unwrap();
            let store = store_path(tmp.path());
            let registry = registry_path(tmp.path());
            write_registry(
                tmp.path(),
                json!([crown_row("king-old", "x-aaaa", 2, "sess-old")]),
            );
            name_crown(&store, &registry, "x-aaaa", "barnaby").unwrap();
            carry_succession(&store, "x-aaaa", None).unwrap();
            write_registry(
                tmp.path(),
                json!([crown_row("king-heir", "x-aaaa", 2, "sess-heir")]),
            );
            apply_crown_naming(
                &store,
                &registry,
                None,
                None,
                Some("native backlog"),
                Some(2),
                "x-aaaa",
            )
            .unwrap();
            let rows = crate::state::load_registry(&registry).unwrap();
            assert_eq!(rows.entries[0].name, "barnaby");
        }

        fn a_duplicate_live_name_refuses_naming_and_names_the_holder() {
            let tmp = tempfile::TempDir::new().unwrap();
            write_registry(
                tmp.path(),
                json!([
                    crown_row("king-a", "x-aaaa", 2, "sess-a"),
                    crown_row("king-b", "fno", 1, "sess-b"),
                ]),
            );
            let store = store_path(tmp.path());
            let registry = registry_path(tmp.path());
            apply_crown_naming(
                &store,
                &registry,
                Some("barnaby"),
                None,
                Some("native backlog"),
                Some(2),
                "x-aaaa",
            )
            .unwrap();
            let err = apply_crown_naming(
                &store,
                &registry,
                Some("barnaby"),
                None,
                None,
                Some(1),
                "fno",
            )
            .unwrap_err();
            assert!(err.contains("barnaby"), "{err}");
            assert!(err.contains("x-aaaa"), "{err}");
        }

        fn naming_an_already_named_crown_refuses() {
            let tmp = tempfile::TempDir::new().unwrap();
            write_registry(tmp.path(), json!([crown_row("king-b", "fno", 1, "sess-b")]));
            let store = store_path(tmp.path());
            let registry = registry_path(tmp.path());
            apply_crown_naming(
                &store,
                &registry,
                Some("barnaby"),
                None,
                None,
                Some(1),
                "fno",
            )
            .unwrap();
            let err = apply_crown_naming(
                &store,
                &registry,
                Some("ernest"),
                None,
                None,
                Some(1),
                "fno",
            )
            .unwrap_err();
            assert!(err.contains("already named"), "{err}");
        }

        fn combining_the_two_naming_flags_refuses() {
            let tmp = tempfile::TempDir::new().unwrap();
            assert!(apply_crown_naming(
                &store_path(tmp.path()),
                &registry_path(tmp.path()),
                Some("a"),
                Some("old"),
                None,
                None,
                "fno"
            )
            .is_err());
        }

        fn the_crown_line_leads_and_the_unnamed_line_teaches_the_flag() {
            assert_eq!(
                crown_line_text(
                    Some("Kestrel"),
                    "Lead of native backlog",
                    "x-dddd,x-eeee,x-ffff"
                ),
                "Kestrel, Lead of native backlog (x-dddd,x-eeee,x-ffff)"
            );
            assert_eq!(
                crown_line_text(None, "L1 fno", "fno"),
                "unnamed lead - name it once: fno agents org checkin --name <name>"
            );
        }

        fn a_themed_l2_checkin_records_theme_and_title() {
            let tmp = tempfile::TempDir::new().unwrap();
            write_registry(
                tmp.path(),
                json!([crown_row("kestrel", "x-dddd,x-eeee,x-ffff", 2, "sess-k")]),
            );
            let store = store_path(tmp.path());
            let registry = registry_path(tmp.path());
            name_crown(&store, &registry, "x-dddd,x-eeee,x-ffff", "kestrel").unwrap();
            let shown =
                set_theme(&store, &registry, "x-dddd,x-eeee,x-ffff", "native backlog").unwrap();
            assert_eq!(shown, "Lead of native backlog");
            let dump = snapshot(&store).unwrap();
            let rec = &dump["crowns"]["x-dddd,x-eeee,x-ffff"];
            assert_eq!(rec["theme"], json!("native backlog"));
            assert_eq!(rec["title"], json!("Lead of native backlog"));
            assert_eq!(
                stored_title(&store, "x-dddd,x-eeee,x-ffff").as_deref(),
                Some("Lead of native backlog")
            );
        }

        fn a_beat_without_a_theme_refuses_and_names_the_flag() {
            let tmp = tempfile::TempDir::new().unwrap();
            write_registry(
                tmp.path(),
                json!([crown_row("kestrel", "x-aaaa", 2, "sess-k")]),
            );
            let store = store_path(tmp.path());
            let registry = registry_path(tmp.path());
            name_crown(&store, &registry, "x-aaaa", "kestrel").unwrap();
            let err = apply_crown_naming(&store, &registry, None, None, None, Some(2), "x-aaaa")
                .unwrap_err();
            assert!(
                err.contains("every lead names its theme once per scope"),
                "{err}"
            );
            assert!(err.contains("--theme"), "{err}");
        }

        fn an_l1_refuses_a_theme_and_titles_by_project() {
            let tmp = tempfile::TempDir::new().unwrap();
            write_registry(tmp.path(), json!([crown_row("folio", "fno", 1, "sess-f")]));
            let store = store_path(tmp.path());
            let registry = registry_path(tmp.path());
            name_crown(&store, &registry, "fno", "folio").unwrap();
            let err = set_theme(&store, &registry, "fno", "native backlog").unwrap_err();
            assert!(err.contains("takes the project name"), "{err}");
            assert_eq!(title(1, "fno", None), "Head of fno");
            let dump = snapshot(&store).unwrap();
            assert_eq!(dump["crowns"]["fno"]["title"], json!("Head of fno"));
        }

        fn titles_fall_back_to_the_scope_and_unknown_levels_keep_the_level_form() {
            assert_eq!(title(0, "fno", None), "Chief of fno");
            assert_eq!(title(0, "fno", Some("ReadyRule")), "Chief of ReadyRule");
            assert_eq!(title(2, "x-aaaa", None), "Lead of x-aaaa");
            assert_eq!(title(7, "fno", Some("native backlog")), "L7 fno");
            assert_eq!(
                legacy_label(2, "x-dddd,x-eeee,x-ffff"),
                "L2 x-dddd,x-eeee,x-ffff"
            );
        }

        fn a_theme_with_a_quote_or_bad_length_refuses() {
            let tmp = tempfile::TempDir::new().unwrap();
            write_registry(
                tmp.path(),
                json!([crown_row("kestrel", "x-aaaa", 2, "sess-k")]),
            );
            let store = store_path(tmp.path());
            let registry = registry_path(tmp.path());
            name_crown(&store, &registry, "x-aaaa", "kestrel").unwrap();
            for bad in ["na\"tive", "na<ti", "x"] {
                let err = set_theme(&store, &registry, "x-aaaa", bad).unwrap_err();
                assert!(err.contains("2-40 characters"), "{err}");
            }
            assert!(set_theme(&store, &registry, "x-aaaa", "o'brien-team").is_ok());
        }

        fn the_theme_is_set_once_per_scope_and_the_same_theme_is_a_noop() {
            let tmp = tempfile::TempDir::new().unwrap();
            write_registry(
                tmp.path(),
                json!([crown_row("kestrel", "x-aaaa", 2, "sess-k")]),
            );
            let store = store_path(tmp.path());
            let registry = registry_path(tmp.path());
            name_crown(&store, &registry, "x-aaaa", "kestrel").unwrap();
            set_theme(&store, &registry, "x-aaaa", "native backlog").unwrap();
            assert_eq!(
                set_theme(&store, &registry, "x-aaaa", "native backlog").unwrap(),
                "Lead of native backlog"
            );
            let err = set_theme(&store, &registry, "x-aaaa", "other theme").unwrap_err();
            assert!(err.contains("native backlog"), "{err}");
            assert!(err.contains("set once per scope"), "{err}");
        }

        fn a_rescope_carries_the_theme_and_title_and_the_next_beat_needs_no_new_one() {
            let tmp = tempfile::TempDir::new().unwrap();
            write_registry(
                tmp.path(),
                json!([crown_row("kestrel", "x-aaaa", 2, "sess-k")]),
            );
            let store = store_path(tmp.path());
            let registry = registry_path(tmp.path());
            name_crown(&store, &registry, "x-aaaa", "kestrel").unwrap();
            set_theme(&store, &registry, "x-aaaa", "native backlog").unwrap();
            // AC1-HP: the clock stamped on the old arm, the re-scope, and the
            // next stamp on the new arm: the carried start and term survive.
            let mk = |scope: &str, created: &str, term: Option<&str>| KingManifest {
                scope: scope.to_string(),
                created_at: Some(created.to_string()),
                harness_session_id: Some("sess-k".to_string()),
                term: term.map(str::to_string),
                ..Default::default()
            };
            let (t0, t1, t2) = (
                "2026-09-20T12:00:00Z",
                "2026-09-29T21:00:00Z",
                "2026-09-29T22:00:00Z",
            );
            stamp_reign(&store, &mk("x-aaaa", t0, Some("span:200h")), None).unwrap();
            // The told-to re-scope: the same holder now holds a scope grown
            // by one epic. The theme and title it titled stay.
            write_registry(
                tmp.path(),
                json!([crown_row("kestrel", "x-aaaa,x-bbbb", 2, "sess-k")]),
            );
            keep_from(&store, &registry, "x-aaaa", "x-aaaa,x-bbbb").unwrap();
            let dump = snapshot(&store).unwrap();
            let rec = &dump["crowns"]["x-aaaa,x-bbbb"];
            assert_eq!(rec["theme"], json!("native backlog"));
            assert_eq!(rec["title"], json!("Lead of native backlog"));
            // The next arm on the new scope folds the carried clock in.
            stamp_reign(&store, &mk("x-aaaa,x-bbbb", t1, None), None).unwrap();
            let view = reign_view_in(&store, &mk("x-aaaa,x-bbbb", t1, None));
            assert_eq!(view.created_at.as_deref(), Some(t0));
            assert_eq!(view.term.as_deref(), Some("span:200h"));
            // AC4-EDGE: a same-scope re-arm (a newer manifest, same scope)
            // starts fresh; the carried clock does not apply.
            let view = reign_view_in(&store, &mk("x-aaaa,x-bbbb", t2, None));
            assert_eq!(view.created_at.as_deref(), Some(t2));
            assert_eq!(view.term, None);
            // The carried theme satisfies the once-per-crown check: the next
            // beat needs no new theme.
            apply_crown_naming(
                &store,
                &registry,
                None,
                None,
                None,
                Some(2),
                "x-aaaa,x-bbbb",
            )
            .unwrap();
            // A level change rebuilds the title: the themed L2 lead landing
            // on an L0 Chief row reads Chief of the same theme.
            write_registry(
                tmp.path(),
                json!([crown_row("kestrel", "x-cccc", 0, "sess-k")]),
            );
            keep_from(&store, &registry, "x-aaaa,x-bbbb", "x-cccc").unwrap();
            let dump = snapshot(&store).unwrap();
            assert_eq!(dump["crowns"]["x-cccc"]["theme"], json!("native backlog"));
            assert_eq!(
                dump["crowns"]["x-cccc"]["title"],
                json!("Chief of native backlog")
            );
            // A re-scope that lands on an L1 row drops the theme: a Head
            // takes no theme, and its title recomputes from the project.
            write_registry(
                tmp.path(),
                json!([crown_row("kestrel", "fno", 1, "sess-k")]),
            );
            keep_from(&store, &registry, "x-cccc", "fno").unwrap();
            let dump = snapshot(&store).unwrap();
            assert!(dump["crowns"]["fno"].get("theme").is_none());
            assert_eq!(dump["crowns"]["fno"]["title"], json!("Head of fno"));
        }
        an_unnamed_live_crown_cannot_complete_checkin();
        a_successor_checkin_carries_the_name_into_its_registry_label();
        a_duplicate_live_name_refuses_naming_and_names_the_holder();
        naming_an_already_named_crown_refuses();
        combining_the_two_naming_flags_refuses();
        the_crown_line_leads_and_the_unnamed_line_teaches_the_flag();
        a_themed_l2_checkin_records_theme_and_title();
        a_beat_without_a_theme_refuses_and_names_the_flag();
        an_l1_refuses_a_theme_and_titles_by_project();
        titles_fall_back_to_the_scope_and_unknown_levels_keep_the_level_form();
        a_theme_with_a_quote_or_bad_length_refuses();
        the_theme_is_set_once_per_scope_and_the_same_theme_is_a_noop();
        a_rescope_carries_the_theme_and_title_and_the_next_beat_needs_no_new_one();
    }
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
        assert_eq!(registry.entries[0].name, "barnaby");
        assert!(registry.entries[0]
            .aliases
            .iter()
            .any(|alias| alias == "king-a"));
        let dump = snapshot(&store_path(tmp.path())).unwrap();
        assert_eq!(dump["crowns"]["fno"]["regnal"], json!(1));
        // The numeral table then the bare number.
        assert_eq!(display("barnaby", 1), "Barnaby");
        assert_eq!(display("barnaby", 2), "Barnaby II");
        assert_eq!(display("barnaby", 3), "Barnaby III");
        assert_eq!(display("barnaby", 20), "Barnaby XX");
        assert_eq!(display("barnaby", 21), "Barnaby 21");
        assert_eq!(dump["crowns"]["fno"]["holder_session"], json!("sess-a"));
    }

    #[test]
    fn retrying_the_same_name_repairs_a_label_rename_refusal() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_registry(
            tmp.path(),
            json!([
                crown_row("king-a", "fno", 1, "sess-a"),
                crown_row("barnaby", "x-aaaa", 2, "sess-b")
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
        assert_eq!(registry.entries[0].name, "barnaby");
    }

    /// A succession the predecessor survived leaves the carried
    /// label on its still-live row. ensure_named_crown moves the label and
    /// alias off the crownless row in the same transaction that renames the
    /// heir, instead of refusing.
    #[test]
    fn a_live_heir_takes_the_carried_label_off_a_crownless_predecessor_row() {
        let tmp = tempfile::TempDir::new().unwrap();
        // The heir holds the live crown; the predecessor row is alive but
        // crownless, and still carries the label "folio" as its name.
        write_registry(
            tmp.path(),
            json!([
                crown_row("vellum", "fno", 1, "sess-heir"),
                json!({
                    "name": "folio", "status": "live", "cwd": "/repo",
                    "harness": "claude", "harness_session_id": "sess-old",
                    "short_id": "oldrow21",
                    "created_at": "2026-09-23T20:00:00Z",
                }),
            ]),
        );
        let store = store_path(tmp.path());
        let registry = registry_path(tmp.path());
        // The carried record: name folio, regnal 2, unbound until the heir's
        // first beat binds it - carry_succession's shape.
        write(
            &store,
            &Store {
                version: 1,
                crowns: BTreeMap::from([(
                    "fno".into(),
                    CrownNameRecord {
                        name: "Folio".into(),
                        regnal: 2,
                        holder_session: None,
                        nodes: Vec::new(),
                        updated_at: now_stamp(),
                        theme: None,
                        title: None,
                        pending_succession: None,
                        reign: None,
                    },
                )]),
            },
        )
        .unwrap();

        ensure_named_crown(&store, &registry, "fno").unwrap();

        let rows = crate::state::load_registry(&registry).unwrap();
        let heir = rows
            .entries
            .iter()
            .find(|e| e.harness_session_id.as_deref() == Some("sess-heir"))
            .unwrap();
        let old = rows
            .entries
            .iter()
            .find(|e| e.harness_session_id.as_deref() == Some("sess-old"))
            .unwrap();
        assert_eq!(heir.name, "folio");
        assert!(heir.aliases.iter().any(|a| a == "vellum"));
        assert_ne!(old.name, "folio");
        assert!(!old.aliases.iter().any(|a| a == "folio"));
        assert!(crate::state::is_valid_registry_label(&old.name));
        // The label answers for exactly one row again.
        let label_holders = rows
            .entries
            .iter()
            .filter(|e| e.name == "folio" || e.aliases.iter().any(|a| a == "folio"))
            .count();
        assert_eq!(label_holders, 1);
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
        assert!(err.contains("barnaby"), "{err}");
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
        assert_eq!(registry.entries[0].name, "o'brien-x");
    }

    #[test]
    fn carry_succession_bumps_regnal_pends_the_predecessor_and_unbinds_twice() {
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
        // The predecessor's reign clock: the heir must not read it.
        stamp_reign(
            &store_path(tmp.path()),
            &KingManifest {
                scope: "x-aaaa".into(),
                created_at: Some("2026-09-20T12:00:00Z".into()),
                harness_session_id: Some("sess-a".into()),
                term: Some("span:200h".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        // An unmarked carry (an old caller) keeps the frozen wire shape.
        carry_succession(&store_path(tmp.path()), "x-aaaa", None).unwrap();
        let dump = snapshot(&store_path(tmp.path())).unwrap();
        assert_eq!(dump["crowns"]["x-aaaa"]["regnal"], json!(2));
        assert_eq!(dump["crowns"]["x-aaaa"]["holder_session"], json!(null));
        let raw = std::fs::read_to_string(store_path(tmp.path())).unwrap();
        assert!(
            !raw.contains("pending_succession"),
            "the frozen wire shape must not grow a null key: {raw}"
        );
        // A marked carry pends the succession for the reap sweep's revert.
        carry_succession(
            &store_path(tmp.path()),
            "x-aaaa",
            Some(PendingSuccession {
                heir_name: "king-heir".into(),
                predecessor_name: "king-a".into(),
                predecessor_session: Some("sess-a".into()),
                ts: now_stamp(),
            }),
        )
        .unwrap();
        // A second succession before the heir checks in reads regnal 3.
        let dump = snapshot(&store_path(tmp.path())).unwrap();
        assert_eq!(dump["crowns"]["x-aaaa"]["regnal"], json!(3));
        assert_eq!(dump["crowns"]["x-aaaa"]["holder_session"], json!(null));
        let pending = &dump["crowns"]["x-aaaa"]["pending_succession"];
        assert_eq!(pending["heir_name"], json!("king-heir"));
        assert_eq!(pending["predecessor_name"], json!("king-a"));
        assert_eq!(pending["predecessor_session"], json!("sess-a"));
        assert!(pending["ts"].is_string());
        // AC3-EDGE: an heir is a new session. The carried clock keys on the
        // predecessor's session, so the heir's manifest reads from its own
        // arm; the predecessor's term is not read.
        let heir_manifest = KingManifest {
            scope: "x-aaaa".into(),
            created_at: Some("2026-09-29T23:00:00Z".into()),
            harness_session_id: Some("sess-heir".into()),
            ..Default::default()
        };
        let view = reign_view_in(&store_path(tmp.path()), &heir_manifest);
        assert_eq!(
            view.created_at.as_deref(),
            Some("2026-09-29T23:00:00Z"),
            "an heir starts from its own manifest"
        );
        assert_eq!(view.term, None);
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
        carry_succession(
            &store_path(tmp.path()),
            "x-aaaa",
            Some(PendingSuccession {
                heir_name: "king-heir".into(),
                predecessor_name: "king-old".into(),
                predecessor_session: Some("sess-old".into()),
                ts: now_stamp(),
            }),
        )
        .unwrap();
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
        // The beat is the proof the succession waited for: the pending
        // marker clears and the heir's session holds the name.
        assert!(dump["crowns"]["x-aaaa"].get("pending_succession").is_none());
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
                    theme: None,
                    title: None,
                    pending_succession: None,
                    reign: None,
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
        // An un-themed record recomputes its scope-derived title.
        assert_eq!(
            dump["crowns"]["new-scope"]["title"],
            json!("Head of new-scope")
        );
        let registry = crate::state::load_registry(&registry_path(tmp.path())).unwrap();
        assert_eq!(registry.entries[0].name, "barnaby");

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
                    theme: None,
                    title: None,
                    pending_succession: None,
                    reign: None,
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

    // -- pending succession: revert matrix --

    fn pending_record(heir: &str, pred: &str, session: Option<&str>, ts: &str) -> CrownNameRecord {
        CrownNameRecord {
            name: "Folio".into(),
            regnal: 2,
            holder_session: None,
            nodes: Vec::new(),
            updated_at: now_stamp(),
            theme: None,
            title: None,
            pending_succession: Some(PendingSuccession {
                heir_name: heir.into(),
                heir_session: None,
                predecessor_name: pred.into(),
                predecessor_session: session.map(String::from),
                ts: ts.into(),
            }),
            reign: None,
        }
    }

    fn old_ts() -> &'static str {
        "2026-08-01T00:00:00Z"
    }

    #[test]
    fn a_renamed_heir_still_keeps_through_heir_session() {
        let tmp = tempfile::TempDir::new().unwrap();
        // The heir row was renamed after the succession carried: the
        // session-keyed join still finds it, and the succession stays.
        write_registry(
            tmp.path(),
            json!([crown_row("renamed-heir", "other", 1, "sess-heir")]),
        );
        let mut pending = pending_record("original-name", "king-old", Some("sess-old"), old_ts());
        pending.pending_succession.as_mut().unwrap().heir_session = Some("sess-heir".into());
        let store = store_path(tmp.path());
        write(
            &store,
            &Store {
                version: 1,
                crowns: BTreeMap::from([("fno".into(), pending)]),
            },
        )
        .unwrap();
        let (reverted, kept) = revert_stale_pending(
            &store,
            &registry_path(tmp.path()),
            chrono::Utc::now(),
            3_600,
            true,
        )
        .unwrap();
        assert!(reverted.is_empty(), "{reverted:?}");
        assert!(
            kept.iter()
                .any(|k| k.contains("renamed-heir") && k.contains("still")),
            "{kept:?}"
        );
        let dump = snapshot(&store).unwrap();
        assert!(dump["crowns"]["fno"]["pending_succession"].is_object());
    }

    #[test]
    fn a_stale_pending_succession_reverts_by_heir_evidence() {
        let tmp = tempfile::TempDir::new().unwrap();
        // The predecessor row survives (exited, resumable); one heir's row
        // was REMOVED by the bind-window reaper, another went terminal, one
        // heir is still live, one is inside the window.
        write_registry(
            tmp.path(),
            json!([
                json!({
                    "name": "king-old", "status": "exited", "cwd": "/repo",
                    "harness": "claude", "harness_session_id": "sess-old",
                    "created_at": "2026-09-23T20:00:00Z",
                }),
                json!({
                    "name": "heir-terminal", "status": "exited", "cwd": "/repo",
                    "harness": "claude", "harness_session_id": "sess-t",
                    "created_at": "2026-09-23T20:00:00Z",
                }),
                crown_row("heir-live", "other", 1, "sess-l"),
                json!({
                    "name": "heir-dup", "status": "live", "cwd": "/repo",
                    "harness": "claude", "harness_session_id": "sess-d1",
                    "created_at": "2026-09-23T20:00:00Z",
                }),
                json!({
                    "name": "heir-dup", "status": "live", "cwd": "/repo",
                    "harness": "claude", "harness_session_id": "sess-d2",
                    "created_at": "2026-09-23T20:00:00Z",
                }),
                json!({
                    "name": "heir-twin", "status": "failed", "cwd": "/repo",
                    "harness": "claude", "harness_session_id": "sess-tw1",
                    "created_at": "2026-09-23T20:00:00Z",
                }),
                crown_row("heir-twin", "other", 1, "sess-tw2"),
            ]),
        );
        let store = store_path(tmp.path());
        write(
            &store,
            &Store {
                version: 1,
                crowns: BTreeMap::from([
                    (
                        "fno".into(),
                        pending_record("jolly-finch", "king-old", Some("sess-old"), old_ts()),
                    ),
                    (
                        "x-tttt".into(),
                        pending_record("heir-terminal", "king-two", Some("sess-two"), old_ts()),
                    ),
                    (
                        "x-llll".into(),
                        pending_record("heir-live", "king-three", Some("sess-3"), old_ts()),
                    ),
                    (
                        "x-yyyy".into(),
                        pending_record("young-heir", "king-four", Some("sess-4"), &now_stamp()),
                    ),
                    (
                        "x-dupd".into(),
                        pending_record("heir-dup", "king-five", Some("sess-5"), old_ts()),
                    ),
                    (
                        "x-twin".into(),
                        pending_record("heir-twin", "king-six", Some("sess-6"), old_ts()),
                    ),
                ]),
            },
        )
        .unwrap();
        let (reverted, kept) = revert_stale_pending(
            &store,
            &registry_path(tmp.path()),
            chrono::Utc::now(),
            3_600,
            true,
        )
        .unwrap();
        assert_eq!(reverted.len(), 2, "{reverted:?}");
        assert_eq!(reverted[0].scope, "fno");
        assert_eq!(reverted[0].heir_name, "jolly-finch");
        assert_eq!(reverted[0].evidence, "heir row removed");
        assert_eq!(reverted[1].scope, "x-tttt");
        assert!(reverted[1].evidence.contains("heir row"), "{reverted:?}");
        assert_eq!(kept.len(), 3, "{kept:?}");
        assert!(kept.iter().any(|k| k.contains("heir-live")), "{kept:?}");
        // A name two live rows answer is ambiguous: keep, never guess.
        assert!(
            kept.iter()
                .any(|k| k.contains("heir-dup") && k.contains("ambiguous")),
            "{kept:?}"
        );
        // A dead twin never answers for the live heir.
        assert!(kept.iter().any(|k| k.contains("heir-twin")), "{kept:?}");
        let dump = snapshot(&store).unwrap();
        // Reverted records name the predecessor again, marker gone.
        assert_eq!(dump["crowns"]["fno"]["holder_session"], json!("sess-old"));
        assert!(dump["crowns"]["fno"].get("pending_succession").is_none());
        assert_eq!(
            dump["crowns"]["x-tttt"]["holder_session"],
            json!("sess-two")
        );
        // The live heir's record is untouched, and the young one keeps.
        assert_eq!(dump["crowns"]["x-llll"]["holder_session"], json!(null));
        assert_eq!(
            dump["crowns"]["x-llll"]["pending_succession"]["heir_name"],
            json!("heir-live")
        );
        assert_eq!(
            dump["crowns"]["x-yyyy"]["pending_succession"]["heir_name"],
            json!("young-heir")
        );
        // The ambiguous and twin records keep too.
        assert_eq!(dump["crowns"]["x-dupd"]["holder_session"], json!(null));
        assert_eq!(
            dump["crowns"]["x-dupd"]["pending_succession"]["heir_name"],
            json!("heir-dup")
        );
        assert_eq!(
            dump["crowns"]["x-twin"]["pending_succession"]["heir_name"],
            json!("heir-twin")
        );

        // The dry run reports the same revert and writes nothing.
        write(
            &store,
            &Store {
                version: 1,
                crowns: BTreeMap::from([(
                    "fno".into(),
                    pending_record("gone-heir", "king-old", Some("sess-old"), old_ts()),
                )]),
            },
        )
        .unwrap();
        let (reverted, _kept) = revert_stale_pending(
            &store,
            &registry_path(tmp.path()),
            chrono::Utc::now(),
            3_600,
            false,
        )
        .unwrap();
        assert_eq!(reverted.len(), 1, "{reverted:?}");
        let dump = snapshot(&store).unwrap();
        assert_eq!(dump["crowns"]["fno"]["holder_session"], json!(null));
        assert_eq!(
            dump["crowns"]["fno"]["pending_succession"]["heir_name"],
            json!("gone-heir")
        );
        // A forgotten record (a fresh grant) is a no-op for the revert.
        forget(&store, "fno").unwrap();
        let (reverted, _kept) = revert_stale_pending(
            &store,
            &registry_path(tmp.path()),
            chrono::Utc::now(),
            3_600,
            true,
        )
        .unwrap();
        assert!(reverted.is_empty(), "{reverted:?}");
    }
}
