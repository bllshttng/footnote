//! The `--` picker's runtime flag capture: the installed harness's own
//! `--help`, parsed into flag rows with their one-line descriptions, cached
//! per binary version so the spawns pay once per binary change. A failed or
//! empty capture is `Some(vec![])` and the caller keeps the static toml
//! capture as the offline fallback.

use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::Duration;

/// One picker row: the entry spelling (`--flag <value>`) and the `--help`
/// description beside it.
pub(crate) type FlagRow = (String, String);

/// The capture channel: the launcher's kick hands the sender to the pump in
/// `attach_and_run`, whose select arm lands rows through [`land`].
pub(super) fn channel() -> (
    tokio::sync::mpsc::Sender<(String, Option<Vec<FlagRow>>)>,
    tokio::sync::mpsc::Receiver<(String, Option<Vec<FlagRow>>)>,
) {
    tokio::sync::mpsc::channel(4)
}

/// Flags the composer, the door or help itself owns: never suggestions.
const OWNED: &[&str] = &[
    "--harness",
    "--cwd",
    "--substrate",
    "--model",
    "-m",
    "--provider",
    "-P",
    "--route",
    "--effort",
    "--permission-mode",
    "--tab",
    "--portal",
    "--split",
    "--node",
    "--mux-session",
    "--no-wait",
    "--prompt-file",
    "--force",
    "--help",
    "--version",
];

/// The binary is the harness's own name on `$PATH`; an absent binary fails
/// the spawn and the toml capture stays.
pub(crate) async fn capture(harness: &str) -> Option<Vec<FlagRow>> {
    let version = run_capture(harness, &["--version"]).await?;
    let cache = cache_path(harness, &version);
    if let Some(rows) = read_cache(&cache) {
        return Some(rows);
    }
    let text = run_capture(harness, &["--help"]).await?;
    let rows = parse_help(&text);
    // An empty parse is a transient misread, not a fact about the binary:
    // never cache it, so the next open retries the capture.
    if rows.is_empty() {
        return Some(rows);
    }
    let _ = std::fs::create_dir_all(cache.parent()?);
    let _ = std::fs::write(
        &cache,
        serde_json::to_string(&serde_json::json!({ "rows": rows })).ok()?,
    );
    Some(rows)
}

/// The harness whose `--help` rows are missing, when a probe may run: one
/// in flight, and the selected harness differs from the captured one.
pub(super) fn probe_due(view: &super::View) -> Option<String> {
    view.launcher
        .as_ref()
        .filter(|l| !l.flags_inflight && l.runtime_flags_harness != l.draft.harness())
        .map(|l| l.draft.harness())
}

/// Kick the capture off the UI loop for the harness [`probe_due`] named.
pub(super) fn kick(
    view: &mut super::View,
    tx: tokio::sync::mpsc::Sender<(String, Option<Vec<FlagRow>>)>,
) {
    let Some(harness) = probe_due(view) else {
        return;
    };
    if let Some(l) = view.launcher.as_mut() {
        l.flags_inflight = true;
    }
    tokio::spawn(async move {
        let rows = capture(&harness).await;
        let _ = tx.send((harness, rows)).await;
    });
}

/// Land a finished capture: rows only ever join the launcher whose
/// harness still matches; a failed read lands as an empty row set (the
/// toml capture shows) so a missing binary never loops the probe. A capture
/// racing a closed launcher lands nowhere - the flag died with the
/// instance, and the next open probes fresh.
pub(super) fn land(view: &mut super::View, harness: &str, rows: Option<Vec<FlagRow>>) {
    if let Some(l) = view.launcher.as_mut() {
        l.flags_inflight = false;
        if l.draft.harness() == harness {
            l.runtime_flags = rows.unwrap_or_default();
            l.runtime_flags_harness = harness.to_string();
        }
    }
}

async fn run_capture(bin: &str, args: &[&str]) -> Option<String> {
    let mut cmd = crate::process_admission::tokio_command(bin);
    cmd.args(args);
    let fut = crate::process_admission::tokio_output(&mut cmd);
    let out = tokio::time::timeout(Duration::from_secs(5), fut)
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    (!text.is_empty()).then_some(text)
}

/// The cache file: one per harness per binary version, the `--version`
/// output standing in for the version.
fn cache_path(harness: &str, version: &str) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    harness.hash(&mut hasher);
    version.hash(&mut hasher);
    base_dir().join(format!("{harness}-{:016x}.json", hasher.finish()))
}

fn base_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_AGENTS_HOME") {
        return PathBuf::from(v).join("cache").join("harness-flags");
    }
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(".fno").join("cache").join("harness-flags")
}

fn read_cache(path: &PathBuf) -> Option<Vec<FlagRow>> {
    let raw = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let rows = value.get("rows")?.as_array()?;
    Some(
        rows.iter()
            .filter_map(|r| {
                let pair = r.as_array()?;
                Some((
                    pair.first()?.as_str()?.to_string(),
                    pair.get(1)?.as_str()?.to_string(),
                ))
            })
            .collect(),
    )
}

/// One flag row per long option the help declares, its description from the
/// same line or the first following indented line (claude's commander style
/// puts the description on the next line; codex keeps it beside). A flag
/// with no description yet still becomes a row, so a two-column help is not
/// required.
pub(super) fn parse_help(text: &str) -> Vec<FlagRow> {
    let mut rows: Vec<FlagRow> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut pending: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        let is_flag = t.starts_with("--")
            || (t.starts_with('-') && t.len() > 1 && t.as_bytes()[1].is_ascii_alphanumeric());
        if is_flag {
            if let Some(entry) = pending.take() {
                rows.push((entry, String::new()));
            }
            let spell_end = t.find("  ").unwrap_or(t.len());
            let spell = t[..spell_end].trim_end();
            let name = spell
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_end_matches(',');
            if !name.starts_with("--") || OWNED.contains(&name) || !seen.insert(name.to_string()) {
                continue;
            }
            let desc = t[spell_end..].trim();
            if desc.is_empty() {
                pending = Some(spell.to_string());
            } else {
                rows.push((spell.to_string(), desc.to_string()));
            }
        } else if !t.is_empty() {
            if let Some(entry) = pending.take() {
                rows.push((entry, t.to_string()));
            }
        }
        if rows.len() >= 64 {
            break;
        }
    }
    if let Some(entry) = pending.take() {
        rows.push((entry, String::new()));
    }
    rows
}
