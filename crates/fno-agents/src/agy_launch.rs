//! Owner of the launch posture: which permission tokens a launch carries.
//!
//! The spawn axes that overrode the posture are retired, so the answer is
//! the lane default alone: a bypass_always row (agy) appends its bypass -
//! agy declares every permission_response unsupported, so an unattended
//! worker cannot answer an approval prompt - and every other row launches
//! under the harness's own ambient permission stance. Every answer names
//! its posture and source so a launch can print the posture it ran with.

use crate::harness_capabilities::HarnessContract;

/// One launch posture: the argv tokens, a one-word posture name for a
/// receipt line, what set it, and an operator-facing note when the posture
/// is one the caller did not choose.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Posture {
    pub tokens: Vec<String>,
    /// "bypass" | "default"
    pub effective: String,
    /// "lane-default"
    pub source: String,
    pub note: String,
}

/// The keeper row's bypass facts for one harness, read from the embedded
/// capability table (bypass_always is agy-only today; pi, cursor-agent and
/// grok keep today's tokens).
fn row(harness: &str) -> (Option<String>, bool) {
    let arm = HarnessContract::packaged()
        .ok()
        .and_then(|c| c.harness.get(harness).cloned());
    let keeper = arm.as_ref().and_then(|a| a.keeper.clone());
    let bypass_flag = keeper
        .as_ref()
        .map(|k| k.bypass_flag.clone())
        .filter(|f| !f.is_empty());
    let always = keeper.map(|k| k.bypass_always).unwrap_or(false);
    (bypass_flag, always)
}

/// The lane-default posture for one lane of one harness: a bypass_always
/// row (agy) appends its bypass, everything else launches unmodified.
pub fn keeper_posture(harness: &str, lane: &str) -> Result<Posture, String> {
    let (bypass_flag, always) = row(harness);
    if always && bypass_flag.is_some() {
        return Ok(Posture {
            tokens: bypass_flag.clone().into_iter().collect(),
            effective: "bypass".to_string(),
            source: "lane-default".to_string(),
            note: if harness == "agy" {
                format!(
                    "agy {lane} runs with --dangerously-skip-permissions by default \
                     because nobody can answer an approval prompt; the permission \
                     axes that chose otherwise are retired"
                )
            } else {
                String::new()
            },
        });
    }
    Ok(Posture {
        tokens: Vec::new(),
        effective: "default".to_string(),
        source: "lane-default".to_string(),
        note: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC6-HP: the lane default stays the bypass, and the note names the
    /// flag it carries.
    #[test]
    fn ac6_default_stays_bypass_with_note() {
        for lane in ["thread", "pane"] {
            let p = keeper_posture("agy", lane).unwrap();
            assert_eq!(
                p.tokens,
                vec!["--dangerously-skip-permissions"],
                "lane {lane}"
            );
            assert_eq!(p.effective, "bypass");
            assert_eq!(p.source, "lane-default");
            assert!(p.note.contains("--dangerously-skip-permissions"));
        }
    }

    /// AC4-HP companion: the non-agy keeper shapes the Python block produced
    /// are preserved. pi's row carries no bypass flag (it ships no permission
    /// popups), so the answer is the empty lane default.
    #[test]
    fn non_agy_rows_keep_their_tokens() {
        let p = keeper_posture("pi", "thread").unwrap();
        assert!(p.tokens.is_empty());
        assert_eq!(p.source, "lane-default");
        assert_eq!(p.effective, "default");
    }
}
