//! The prompt-parked reading: every live worker whose pane currently shows a
//! harness permission, approval, or picker prompt (a manifest `blocked`
//! verdict over the pane's grid). wake-a9770da3 sat 56m on an Enter/ExitWorktree
//! prompt while the lead checkin, agents top and the watchdog all read
//! `in_progress` - the state badge answers "working or idle", never "is a
//! prompt up". This reading asks exactly that question over every pane the
//! mux can see, joined to the registry by `fno_id`.
//!
//! Detection reuses the bundled manifests (claude `permission_prompt`,
//! codex `approval_prompt`, ...) through [`Manifest::evaluate_answerable`], so
//! the pinned rules and the answer keystrokes stay in one place. A pane with
//! no registry join or an unknown harness is skipped, never guessed. The mux
//! being unreachable reads as no panes, not as a failed beat - a dead server
//! has no answerable prompts - but the reading carries
//! `server_reachable: false` so the fact stays named.

use crate::manifest::{load_manifest, Manifest};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

/// The system-sender arm parked-prompt announcements ride to the bell. The
/// `fno/` prefix is what marks the row system in the read model.
const ANNOUNCE_FROM: &str = "fno/prompt-parked";

/// One worker parked on a prompt.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParkedRow {
    pub holder: String,
    pub harness: String,
    pub pane_id: u64,
    pub rule_id: String,
    /// First line of the prompt body (display only; empty falls back to the
    /// rule id).
    pub prompt_head: String,
    /// The keystroke that answers the first menu option, when the pinned
    /// grammar extracted a clean numbered menu.
    pub answer_key: Option<String>,
}

impl ParkedRow {
    /// The check-in line: the finding and the remedy in one sentence.
    pub(crate) fn line(&self) -> String {
        let head = if self.prompt_head.is_empty() {
            self.rule_id.as_str()
        } else {
            self.prompt_head.as_str()
        };
        match &self.answer_key {
            Some(key) => format!(
                "WAITING ON APPROVAL: {} ({}) pane {} {} - answer with: fno mux pane send {} --raw {}",
                self.holder, self.harness, self.pane_id, head, self.pane_id, key
            ),
            None => format!(
                "WAITING ON APPROVAL: {} ({}) pane {} {} - no numbered menu; focus or attach to answer",
                self.holder, self.harness, self.pane_id, head
            ),
        }
    }
}

/// Classify one pane's grid text against one parsed manifest. Pure so the
/// classifier is testable without a registry, a mux, or a binary.
pub(crate) fn classify_pane(
    holder: &str,
    harness: &str,
    pane_id: u64,
    manifest: &Manifest,
    text: &str,
) -> Option<ParkedRow> {
    let screen = crate::readiness::ScreenView {
        visible_text: text,
        cursor_row: 0,
        cursor_col: 0,
        osc_title: None,
        osc_progress: None,
    };
    let (verdict, answerable) = manifest.evaluate_answerable(&screen)?;
    if verdict.state != "blocked" {
        return None;
    }
    let prompt_head = answerable
        .as_ref()
        .and_then(|a| a.prompt.lines().next())
        .map(|l| l.trim().to_string())
        .unwrap_or_default();
    let answer_key = answerable.and_then(|a| {
        a.options
            .first()
            .map(|o| String::from_utf8_lossy(&o.keystroke).to_string())
    });
    Some(ParkedRow {
        holder: holder.to_string(),
        harness: harness.to_string(),
        pane_id,
        rule_id: verdict.rule_id.to_string(),
        prompt_head,
        answer_key,
    })
}

/// True when the registry row is past answering anything.
fn row_terminal(status: &crate::AgentStatus) -> bool {
    matches!(
        status,
        crate::AgentStatus::Exited
            | crate::AgentStatus::PermanentDead
            | crate::AgentStatus::Orphaned
            | crate::AgentStatus::Failed
    )
}

/// The reading: every joined pane read once, the parked rows returned. The
/// lead's own session is a worker like any other - a lead parked on its own
/// prompt cannot run this reading anyway, so no self-exclusion exists.
pub(super) fn reading() -> Result<Value, String> {
    let home = crate::paths::AgentsHome::from_env();
    let reg = crate::state::load_registry(&home.registry_json())
        .map_err(|e| format!("registry unreadable: {e}"))?;
    reading_with(&reg, &crate::scrape::fno_bin(), Some(&home.manifests_dir()))
}

/// The injected core of [`reading`], so the composition is testable without
/// env races over `FNO_AGENTS_HOME`/`FNO_BIN`.
fn reading_with(
    reg: &crate::state::Registry,
    bin: &std::ffi::OsStr,
    override_dir: Option<&std::path::Path>,
) -> Result<Value, String> {
    let mut by_fno: HashMap<&str, (&str, &str)> = HashMap::new();
    for e in &reg.entries {
        if row_terminal(&e.status) {
            continue;
        }
        if let Some(id) = e.fno_id.as_deref().filter(|s| !s.is_empty()) {
            by_fno.insert(id, (e.name.as_str(), e.harness_name()));
        }
    }
    let Some(panes) = crate::scrape::mux_pane_ls(bin, None) else {
        return Ok(json!({"server_reachable": false, "rows": []}));
    };
    let mut manifests: BTreeMap<String, Option<Manifest>> = BTreeMap::new();
    let mut rows = Vec::new();
    for row in panes {
        let Some(fno_id) = row.fno_id.as_deref() else {
            continue;
        };
        let Some((holder, harness)) = by_fno.get(fno_id) else {
            continue;
        };
        let manifest = manifests
            .entry((*harness).to_string())
            .or_insert_with(|| match load_manifest(harness, override_dir) {
                Some(Ok(m)) => Some(m),
                Some(Err(e)) => {
                    // Present-but-malformed (a bad hand-authored override)
                    // fails loud, the way the scrape sweep does - a silent
                    // skip would disable detection for the whole harness.
                    eprintln!("prompt_parked: manifest {harness} unusable: {e}");
                    None
                }
                None => None,
            })
            .as_ref();
        let Some(manifest) = manifest else {
            continue;
        };
        let Some(text) = crate::scrape::mux_pane_read(bin, None, row.pane_id) else {
            continue;
        };
        if let Some(parked) = classify_pane(holder, harness, row.pane_id, manifest, &text) {
            rows.push(parked);
        }
    }
    rows.sort_by(|a, b| a.holder.cmp(&b.holder).then(a.pane_id.cmp(&b.pane_id)));
    Ok(json!({
        "server_reachable": true,
        "rows": rows.iter().map(|r| json!({
            "holder": r.holder,
            "harness": r.harness,
            "pane_id": r.pane_id,
            "rule_id": r.rule_id,
            "prompt_head": r.prompt_head,
            "answer_key": r.answer_key,
            "line": r.line(),
        })).collect::<Vec<_>>(),
    }))
}

/// The worker names in one beat's `prompt_parked` reading, for the
/// enter-detection the announce keys on.
fn parked_workers(value: Option<&Value>) -> HashSet<String> {
    value
        .and_then(|v| v.get("rows"))
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r.get("holder").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The attention rows the beat's change derivation folds in, so a parked
/// worker never journals as "no change".
pub(super) fn attention(data: &Map<String, Value>) -> Vec<String> {
    data.get("prompt_parked_rows")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.get("line").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The workers parked now that were NOT parked in the previous beat. Pure, so
/// the enter-detection is testable without a bus.
fn entered_workers(current: Option<&Value>, previous: Option<&Value>) -> Vec<String> {
    let now = parked_workers(current);
    let before = parked_workers(previous);
    let mut entered: Vec<String> = now.difference(&before).cloned().collect();
    entered.sort();
    entered
}

/// Announce to the bell every worker that ENTERED a prompt this beat (present
/// now, absent from the previous beat's rows). Best-effort: a failed announce
/// prints to stderr and never fails the beat; the check-in lines stay the
/// live truth either way. Announcements expire on their own, so an answered
/// prompt needs no clear send.
pub(super) fn announce_entries(data: &Map<String, Value>, previous_data: Option<&Value>) {
    let entered = entered_workers(
        data.get("prompt_parked"),
        previous_data.and_then(|p| p.get("prompt_parked")),
    );
    if entered.is_empty() {
        return;
    }
    let rows = data
        .get("prompt_parked_rows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for row in &rows {
        let Some(holder) = row.get("holder").and_then(Value::as_str) else {
            continue;
        };
        if !entered.iter().any(|h| h == holder) {
            continue;
        }
        let line = row.get("line").and_then(Value::as_str).unwrap_or(holder);
        let subject = format!("waiting on approval: {holder}");
        if let Err(e) = crate::announce::announce_all(ANNOUNCE_FROM, &subject, line) {
            eprintln!("fno-agents lead-checkin: prompt-parked announce failed: {e}");
        }
    }
}

/// The render: one line per parked worker, or a READER FAILED line. A clean
/// read prints nothing - the beat's silence is the good news.
pub(super) fn lines(readings: &[crate::lead_checkin::Reading]) -> Vec<String> {
    let reading = readings.iter().find(|r| r.name == "prompt_parked");
    let Some(reading) = reading else {
        return Vec::new();
    };
    if !reading.ok {
        return vec![format!("READER FAILED prompt_parked: {}", reading.error)];
    }
    reading
        .value
        .get("rows")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.get("line").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundled(agent: &str) -> Manifest {
        Manifest::parse(crate::manifest::bundled_manifest(agent).expect("bundled")).expect("parses")
    }

    #[test]
    fn a_blocked_manifest_verdict_classifies_parked_and_anything_else_does_not() {
        let claude = bundled("claude");
        // Permission prompt: parked, with the pinned answer key and remedy line.
        let screen =
            "Do you want to proceed?\n  ❯ 1. Yes\n  2. No, and tell Claude what to do differently";
        let row = classify_pane("wake-a9770da3", "claude", 42, &claude, screen).expect("parked");
        assert_eq!(row.holder, "wake-a9770da3");
        assert_eq!(row.pane_id, 42);
        assert_eq!(row.rule_id, "permission_prompt");
        assert_eq!(row.answer_key.as_deref(), Some("1"));
        let line = row.line();
        assert!(
            line.starts_with("WAITING ON APPROVAL: wake-a9770da3 (claude) pane 42"),
            "{line}"
        );
        assert!(line.contains("fno mux pane send 42 --raw 1"), "{line}");
        // Codex approval: parked the same way (different rule, same state).
        let codex = bundled("codex");
        let screen = "Would you like to run the following command?\n\
            $ touch /tmp/x\n\
            \u{203a} 1. Yes, proceed (y)\n\
              2. No, and tell Codex what to do differently (esc)\n\
            Press enter to confirm or esc to cancel";
        let row = classify_pane("t-xd55-cx", "codex", 7, &codex, screen).expect("parked");
        assert_eq!(row.rule_id, "approval_prompt");
        assert_eq!(row.answer_key.as_deref(), Some("1"));
        // Working and idle panes never classify.
        assert!(classify_pane(
            "w",
            "claude",
            1,
            &claude,
            "✳ Working… (esc to interrupt)\n❯ "
        )
        .is_none());
        assert!(classify_pane("w", "codex", 1, &codex, "reply\n❯ ").is_none());
        // An arrow-only menu is parked but focus-only: no numbered menu, no key.
        let screen = "Select an option\n  enter to select  ·  esc to cancel\n  ❯ option one";
        let row = classify_pane("w", "claude", 3, &claude, screen).expect("parked (blocked form)");
        assert!(row.answer_key.is_none());
        assert!(
            row.line().contains("focus or attach to answer"),
            "{}",
            row.line()
        );
    }

    #[test]
    fn the_reading_reads_rows_from_a_registry_and_a_stub_mux() {
        let home = tempfile::tempdir().unwrap();
        // Registry with one live row joined to the pane the stub mux serves.
        let mut reg = crate::state::Registry {
            schema_version: crate::state::REGISTRY_SCHEMA_VERSION,
            ..Default::default()
        };
        let mut entry = crate::state::RegistryEntry::default();
        entry.name = "wake-a9770da3".into();
        entry.harness = Some("claude".into());
        entry.status = crate::AgentStatus::Live;
        entry.fno_id = Some("fid-1".into());
        reg.entries.push(entry);
        // Stub mux: one pane whose grid shows a permission prompt. The ❯ is
        // a literal UTF-8 byte in the file (POSIX printf has no \xHH), and
        // the stub goes through write_exec_stub, never a bare test-process
        // write (Text file busy under a sibling fork).
        let script = r#"#!/bin/sh
case "$1 $2" in
  "mux pane")
    if [ "$3" = "ls" ]; then
      printf '[{"pane_id": 9, "fno_id": "fid-1"}]\n'
    else
      printf '{"text": "Do you want to proceed?\\n  ❯ 1. Yes\\n  2. No"}\n'
    fi
    ;;
  *) exit 3 ;;
esac
"#;
        let stub = crate::write_exec_stub(home.path(), "fno-stub", script);
        let value = reading_with(&reg, stub.as_os_str(), None).expect("reading");
        assert_eq!(value["server_reachable"], true);
        let rows = value["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{value}");
        assert_eq!(rows[0]["holder"], "wake-a9770da3");
        assert_eq!(rows[0]["pane_id"], 9);
        assert!(rows[0]["line"]
            .as_str()
            .unwrap()
            .contains("fno mux pane send 9 --raw 1"));
        // An unreachable server reads as no panes, named, never a failed beat.
        let value = reading_with(
            &reg,
            std::ffi::OsStr::new("/nonexistent/fno-stub-88a2"),
            None,
        )
        .unwrap();
        assert_eq!(value["server_reachable"], false);
        assert_eq!(value["rows"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn announce_entries_fires_once_per_entered_worker_and_stays_quiet_on_holds() {
        let row = |holder: &str| json!({"holder": holder, "line": format!("WAITING ON APPROVAL: {holder}")});
        let now = json!({"rows": [row("a"), row("b")]});
        let before = json!({"rows": [row("a")]});
        assert_eq!(
            entered_workers(Some(&now), Some(&before)),
            vec!["b".to_string()]
        );
        // A beat with no previous row enters everyone.
        assert_eq!(entered_workers(Some(&now), None).len(), 2);
        // A held prompt (present in both beats) enters nobody.
        assert!(entered_workers(Some(&now), Some(&now)).is_empty());
    }
}
