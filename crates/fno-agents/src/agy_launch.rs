//! Owner of the agy launch posture: which permission tokens a launch carries
//! and what the conversation mint launches with.
//!
//! The Python block this ports (`complete_launch_argv`'s bypass-then-mode
//! region) appended the always-on bypass BEFORE any mode, so an explicit
//! mode never narrowed the posture: agy ran with bypass AND mode tokens at
//! stacking instead of replacing. Here an explicit mode REPLACES the
//! always-on bypass, the default stays the never-prompt lane (agy declares
//! every permission_response unsupported, so an unattended worker cannot
//! answer an approval prompt), and every answer names its posture and
//! source so a launch can print the posture it ran with.

use crate::codex_posture::permission_pane_tokens;
use crate::harness_capabilities::HarnessContract;

/// The mint turn's prompt, moved from the Python adapter verbatim: a real
/// turn is the only thing agy 1.1.27 offers that returns a conversation id
/// (`--conversation` only resumes), and a no-op reply keeps the
/// conversation's first message harmless.
pub const MINT_PROMPT: &str = "Reply with exactly: OK";

/// One launch posture: the argv tokens, a one-word posture name for a
/// receipt line, what set it, and an operator-facing note when the posture
/// is one the caller did not choose.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Posture {
    pub tokens: Vec<String>,
    /// "bypass" | the chosen mode | "default"
    pub effective: String,
    /// "permission_mode" | "yolo" | "lane-default"
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

fn posture_err(mode: &str) -> String {
    format!(
        "spawn carries both --yolo and permission_mode {mode:?} (they are \
         mutually exclusive); pass one"
    )
}

/// The launch posture for one lane of one harness. `lane` is "pane" or
/// "thread". An explicit mode takes its tokens from the one permission
/// vocabulary and REPLACES the always-on bypass; with no mode, a
/// bypass_always row (agy) appends the bypass; `yolo` appends it on rows
/// that are not bypass_always; `yolo` plus a non-skip mode refuses.
pub fn keeper_posture(
    harness: &str,
    lane: &str,
    permission_mode: Option<&str>,
    yolo: bool,
) -> Result<Posture, String> {
    let mode = permission_mode.map(str::trim).filter(|m| !m.is_empty());
    let (bypass_flag, always) = row(harness);
    match mode {
        Some(mode) => {
            if yolo && mode != "skip" {
                return Err(posture_err(mode));
            }
            let tokens = permission_pane_tokens(harness, mode)?;
            let note = if harness == "agy" && mode != "skip" && lane == "thread" {
                format!(
                    "approval prompts are unanswerable on the agy thread lane \
                     until a view is attached"
                )
            } else {
                String::new()
            };
            Ok(Posture {
                effective: mode.to_string(),
                source: "permission_mode".to_string(),
                note,
                tokens,
            })
        }
        None => {
            if always && bypass_flag.is_some() {
                return Ok(Posture {
                    tokens: bypass_flag.clone().into_iter().collect(),
                    effective: "bypass".to_string(),
                    source: "lane-default".to_string(),
                    note: if harness == "agy" {
                        format!(
                            "agy {lane} runs with --dangerously-skip-permissions by default \
                             because nobody can answer an approval prompt; pass \
                             --permission-mode accept-edits|plan|sandbox|default to choose"
                        )
                    } else {
                        String::new()
                    },
                });
            }
            if yolo && bypass_flag.is_some() {
                return Ok(Posture {
                    tokens: bypass_flag.clone().into_iter().collect(),
                    effective: "bypass".to_string(),
                    source: "yolo".to_string(),
                    note: String::new(),
                });
            }
            Ok(Posture {
                tokens: Vec::new(),
                effective: "default".to_string(),
                source: "lane-default".to_string(),
                note: String::new(),
            })
        }
    }
}

/// The agy conversation-mint argv: the measured print turn (the only surface
/// that returns a conversation id on 1.1.27) carrying the spawn's selected
/// posture, model and effort, so the first turn of a thread no longer runs
/// on the harness default.
pub fn agy_mint_argv(
    model: Option<&str>,
    effort: Option<&str>,
    permission_mode: Option<&str>,
    yolo: bool,
) -> Result<Vec<String>, String> {
    let posture = keeper_posture("agy", "thread", permission_mode, yolo)?;
    let mut argv = vec![
        "agy".to_string(),
        "-p".to_string(),
        MINT_PROMPT.to_string(),
        "--output-format".to_string(),
        "json".to_string(),
    ];
    argv.extend(posture.tokens);
    if let Some(m) = model.map(str::trim).filter(|m| !m.is_empty()) {
        argv.push("--model".to_string());
        argv.push(m.to_string());
    }
    if let Some(e) = effort.map(str::trim).filter(|e| !e.is_empty()) {
        argv.push("--effort".to_string());
        argv.push(e.to_string());
    }
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC5-HP: an explicit mode REPLACES the always-on bypass; the combined
    /// mode carries both native flags and never the bypass.
    #[test]
    fn ac5_mode_replaces_bypass() {
        let p = keeper_posture("agy", "thread", Some("plan+sandbox"), false).unwrap();
        assert_eq!(p.tokens, vec!["--mode", "plan", "--sandbox"]);
        assert!(!p.tokens.iter().any(|t| t.contains("dangerously")));
        assert_eq!(p.effective, "plan+sandbox");
        assert_eq!(p.source, "permission_mode");
    }

    /// AC6-HP: no mode, yolo false: the lane default stays the bypass, and
    /// the note names the flag and how to choose otherwise.
    #[test]
    fn ac6_default_stays_bypass_with_note() {
        for lane in ["thread", "pane"] {
            let p = keeper_posture("agy", lane, None, false).unwrap();
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

    /// AC7-ERR: yolo plus a non-skip mode refuses before any subprocess.
    #[test]
    fn ac7_yolo_plus_mode_refuses() {
        let err = keeper_posture("agy", "thread", Some("plan"), true)
            .expect_err("two postures must refuse");
        assert!(err.contains("mutually exclusive"), "got: {err}");
        // skip names the same posture the default already carries, so it is
        // not a second knob.
        let p = keeper_posture("agy", "thread", Some("skip"), true).unwrap();
        assert_eq!(p.tokens, vec!["--dangerously-skip-permissions"]);
    }

    /// AC8-HP: the mint argv carries the selected model, effort and posture.
    #[test]
    fn ac8_mint_carries_axes() {
        let argv = agy_mint_argv(Some("gemini-3-pro"), Some("high"), Some("plan"), false).unwrap();
        let text = argv.join(" ");
        assert!(text.contains("--model gemini-3-pro"), "got: {text}");
        assert!(text.contains("--effort high"), "got: {text}");
        assert!(text.contains("--mode plan"), "got: {text}");
        assert!(!text.contains("dangerously"));
    }

    /// AC8 companion: the bare default mint keeps today's measured shape.
    #[test]
    fn mint_default_keeps_measured_shape() {
        let argv = agy_mint_argv(None, None, None, false).unwrap();
        assert_eq!(
            argv,
            vec![
                "agy",
                "-p",
                "Reply with exactly: OK",
                "--output-format",
                "json",
                "--dangerously-skip-permissions",
            ]
        );
    }

    /// AC4-HP companion: the non-agy keeper shapes the Python block produced
    /// are preserved. pi's row carries no bypass flag (it ships no permission
    /// popups), so yolo maps to nothing there - matching today's Python.
    #[test]
    fn non_agy_rows_keep_their_tokens() {
        // pi: no bypass_always, no bypass_flag; no mode, no yolo -> empty.
        let p = keeper_posture("pi", "thread", None, false).unwrap();
        assert!(p.tokens.is_empty());
        // pi + yolo -> still empty: the row declares no bypass flag, so the
        // answer is the lane default (yolo has no flag to apply).
        let p = keeper_posture("pi", "thread", None, true).unwrap();
        assert!(p.tokens.is_empty());
        assert_eq!(p.source, "lane-default");
    }
}
