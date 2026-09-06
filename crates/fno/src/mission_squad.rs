//! Synthetic mission-squad headers: one per active mission, rendered under the
//! mux `~ missions` band.
//!
//! A mission squad is a render-time grouping, not a workspace (an agent is
//! never assigned a mission id), so its identity is derived, not stored: the
//! id is a hash of the epic id (stable across ticks), and the name carries the
//! live `done/total` counter. With several active missions each name also
//! carries its rotation `(i of n)` in epic-id order - the drain's own order -
//! so one header sampled from the band never reads as the whole population; a
//! lone mission keeps its bare name, because `(1 of 1)` reads as a fault, not
//! a count.

use crate::backlog_view::Mission;
use crate::proto::{SquadMeta, MISSION_SQUAD_BASE};

/// FNV-1a over bytes: tiny, dependency-free, deterministic - exactly what a
/// stable-per-epic synthetic id needs (no crypto property required).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// The synthetic squad id for a mission's `SquadMeta` header, deterministic
/// per epic id so the same mission maps to the same id across ticks. High bit
/// (`proto::MISSION_SQUAD_BASE`) set so it never collides with a real squad
/// id (those start at 1 and increment by one - see `next_squad_id`).
pub fn mission_sid(epic_id: &str) -> u64 {
    MISSION_SQUAD_BASE | (fnv1a(epic_id.as_bytes()) & (MISSION_SQUAD_BASE - 1))
}

/// One `SquadMeta` header per active mission, epic-id order preserved by the
/// caller (`derive_missions` sorts). done/total is baked into the name so no
/// proto bump is needed; the rotation suffix follows the same rule.
pub fn headers(missions: &[Mission]) -> Vec<SquadMeta> {
    let rotation_total = missions.len();
    missions
        .iter()
        .enumerate()
        .map(|(i, m)| SquadMeta {
            id: mission_sid(&m.epic_id),
            name: format!(
                "{}  {}/{}{}",
                m.slug,
                m.done,
                m.total,
                if rotation_total > 1 {
                    format!("  ({} of {})", i + 1, rotation_total)
                } else {
                    String::new()
                }
            ),
            canonical_cwd: String::new(),
            tabs: vec![],
            active_tab: 0,
            panes: 0,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mission(epic_id: &str, slug: &str, done: u32, total: u32) -> Mission {
        Mission {
            epic_id: epic_id.into(),
            slug: slug.into(),
            done,
            total,
        }
    }

    #[test]
    fn multi_mission_headers_name_the_rotation() {
        // Two active missions: each header carries `(i of n)` so one sample of
        // the rotation cannot read as the whole population.
        let got = headers(&[
            mission("x-aaaa", "alpha", 0, 2),
            mission("x-bbbb", "beta", 1, 3),
        ]);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "alpha  0/2  (1 of 2)");
        assert_eq!(got[0].id, mission_sid("x-aaaa"));
        assert_eq!(got[1].name, "beta  1/3  (2 of 2)");
    }

    #[test]
    fn single_mission_header_stays_bare() {
        // Control half: a lone mission names itself without a `(1 of 1)` -
        // a bare parenthetical reads as a fault, not a count.
        let got = headers(&[mission("x-aaaa", "mux-squad", 1, 2)]);
        assert_eq!(got[0].name, "mux-squad  1/2");
    }
}
