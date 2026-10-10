//! `fno-agents harness-beat`: the lead beat this harness runs. The key
//! `agents.<harness>.beat` defaults to `auto`, which reads the capability
//! row's `beat`; a member of [`BEATS`] set there overrides the row. The lead
//! skill arms what this prints instead of branching on the harness name.
//!
//! Transport-only and unregistered in `ALL_CLIENT_ACTIONS` (the shrink law
//! allows no new client action), like `harness-roster`.

use std::path::Path;

use crate::harness_capabilities::{HarnessContract, BEATS};

#[derive(Debug, PartialEq, Eq)]
pub struct Beat {
    pub beat: String,
    pub source: &'static str,
}

/// A configured word outside `auto` and [`BEATS`] degrades to the row and
/// says so, so a typo never silently stops a beat.
pub fn resolve_with(configured: Option<&str>, row: &str) -> Beat {
    let (beat, source) = match configured {
        Some(word) if BEATS.contains(&word) => (word, "config"),
        Some(word) if word != "auto" => (row, "capability row; config value is not a beat"),
        _ => (row, "capability row"),
    };
    Beat {
        beat: beat.to_string(),
        source,
    }
}

pub fn resolve(harness: &str, cwd: &Path) -> Result<Beat, String> {
    let contract = HarnessContract::packaged().map_err(|e| e.to_string())?;
    let row = contract
        .beat(harness)
        .ok_or_else(|| format!("no capability row for harness {harness:?}"))?;
    let configured = crate::agents_config::agents_beat(harness, cwd);
    Ok(resolve_with(configured.as_deref(), row))
}

/// `harness-beat [--harness <name>]` prints `beat: <beat> (<source>)`.
/// Without `--harness` the caller's own harness answers. Exit 0 answered,
/// 2 usage, an unresolved harness, or no row.
pub fn run(rest: &[String]) -> i32 {
    let harness = match rest {
        [] => crate::claims::resolve_identity().1,
        [flag, name] if flag == "--harness" => Some(name.clone()),
        _ => {
            eprintln!("fno-agents: usage: harness-beat [--harness <name>]");
            return 2;
        }
    };
    let Some(harness) = harness.filter(|h| !h.is_empty()) else {
        eprintln!("fno-agents harness-beat: no harness resolved; pass --harness <name>");
        return 2;
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    match resolve(&harness, &cwd) {
        Ok(beat) => {
            println!("beat: {} ({})", beat.beat, beat.source);
            0
        }
        Err(error) => {
            eprintln!("fno-agents harness-beat: {error}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_reads_the_row_and_a_beat_word_overrides_it() {
        use crate::harness_capabilities::CAPABILITY_TOML;

        // A row with no `beat` reads daemon; a row naming a non-member is refused.
        let bare: Vec<&str> = CAPABILITY_TOML
            .lines()
            .filter(|l| !l.starts_with("beat = "))
            .collect();
        let contract = HarnessContract::parse(&bare.join("\n")).unwrap();
        assert_eq!(contract.beat("claude"), Some("daemon"));
        assert_eq!(contract.beat("nonesuch"), None);
        let typo_row = CAPABILITY_TOML.replacen("\nbeat = \"", "\nbeat = \"x", 1);
        let err = HarnessContract::parse(&typo_row).unwrap_err().to_string();
        assert!(err.contains("\"beat\""), "{err}");

        assert_eq!(resolve_with(None, "loop").source, "capability row");
        assert_eq!(resolve_with(Some("auto"), "loop").beat, "loop");
        let set = resolve_with(Some("daemon"), "loop");
        assert_eq!((set.beat.as_str(), set.source), ("daemon", "config"));
        let typo = resolve_with(Some("cron"), "loop");
        assert_eq!(typo.beat, "loop");
        assert!(typo.source.contains("not a beat"), "{}", typo.source);
    }
}
