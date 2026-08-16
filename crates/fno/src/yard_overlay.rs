//! The yard identity fold's shell-out leg (x-b2bf): a bounded, fail-open
//! call to `fno yard --json`, mirroring [`crate::needs_overlay`]'s idiom.
//!
//! The client owns the status leg (badge / need / PR readings from the
//! layout it already holds) and derives the eye from those values at render
//! time - one status value feeds both the row and the sprite. This module
//! supplies the identity leg the layout cannot see (species, rarity tier,
//! crown, first-sighting), which the Python fold computes over the registry
//! and the graph archive. The call runs off the UI loop on a spawned task
//! and reports back over a channel, so a slow `fno` never blocks the
//! overlay from opening.

use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

/// The Python CLI's startup (imports, config load) exceeds the needs fold's
/// 800ms Rust-only cap, so this leg gets roughly double. Still bounded: a
/// slow fold degrades the overlay to its roster-derived legs with a visible
/// notice, never blocks the UI.
const SHELLOUT_TIMEOUT: Duration = Duration::from_millis(1500);

/// One yard citizen, as emitted by `fno yard --json`. Identity channels
/// only - no status field lives here, by design: the eye is computed from
/// the roster row the client already renders, so the payload cannot
/// disagree with it.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct YardItem {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub species: usize,
    #[serde(default)]
    pub rarity: String,
    #[serde(default)]
    pub crown_level: u32,
    #[serde(default)]
    pub first_sighting: bool,
}

/// Resolve the `fno` binary through the server's one resolver
/// (`$FNO_BIN`, else the running executable - the mux binary forwards
/// non-native verbs to Python - else PATH). The crate already holds the
/// never-drift rule for this (connections_view imports the same fn), so a
/// second resolver with different semantics would let the yard degrade on a
/// checkout where every sibling leg still works.
fn fno_bin() -> PathBuf {
    crate::server::fno_bin()
}

/// Fold the yard identity leg now. `None` on any failure (timeout, nonzero
/// exit, unparseable JSON) - the caller shows the degraded notice; a clean
/// `Some(vec)` (possibly empty) is a good fold.
pub async fn fold_now() -> Option<Vec<YardItem>> {
    let fut = tokio::process::Command::new(fno_bin())
        .args(["yard", "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // Dropped on timeout; kill_on_drop reaps the child so a slow fold
        // can't orphan a Python process on each overlay open.
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(SHELLOUT_TIMEOUT, fut)
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse(&output.stdout)
}

/// Parse the verb's JSON payload. Fails quiet (returns `None`) on
/// unparseable output so a torn stdout degrades the overlay, not crashes it.
/// A species index outside the vendored table is the same class of damage:
/// the two halves of the fold upgrade independently, and `% SPECIES_COUNT`
/// downstream would fold the skew onto the wrong animal SILENTLY - degrade
/// loud instead of rendering a citizen as a species it is not.
fn parse(stdout: &[u8]) -> Option<Vec<YardItem>> {
    let payload: Payload = serde_json::from_slice(stdout).ok()?;
    if payload
        .citizens
        .iter()
        .any(|c| c.species >= crate::sprites::SPECIES_COUNT)
    {
        return None;
    }
    Some(payload.citizens)
}

#[derive(Debug, Deserialize)]
struct Payload {
    #[serde(default)]
    citizens: Vec<YardItem>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_citizen_array() {
        let json = br#"{"citizens":[{"id":"1111-aaaa","name":"worker","harness":"claude","species":4,"rarity":"common","crown_level":0,"first_sighting":false}]}"#;
        let items = parse(json).expect("valid payload parses");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "worker");
        assert_eq!(items[0].species, 4);
        assert_eq!(items[0].rarity, "common");
        assert!(!items[0].first_sighting);
    }

    #[test]
    fn missing_optional_fields_default() {
        // crown_level / first_sighting / rarity absent -> defaults, not a
        // parse failure (the fold may predate a field).
        let json = br#"{"citizens":[{"id":"i","name":"n"}]}"#;
        let items = parse(json).expect("parses with defaults");
        assert_eq!(items[0].crown_level, 0);
        assert!(!items[0].first_sighting);
        assert_eq!(items[0].rarity, "");
    }

    #[test]
    fn empty_citizens_is_a_clean_fold() {
        assert_eq!(parse(br#"{"citizens":[]}"#).expect("parses").len(), 0);
    }

    #[test]
    fn out_of_range_species_degrades_not_masks() {
        // Python widening SPECIES_COUNT ahead of this binary must read as a
        // failed fold (visible degrade), never as the wrong animal rendered.
        let json = br#"{"citizens":[{"id":"i","name":"n","species":18}]}"#;
        assert!(parse(json).is_none());
    }

    #[test]
    fn torn_json_fails_quiet() {
        assert!(parse(b"{not json").is_none());
    }
}
