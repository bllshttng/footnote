//! The one owner of the Codex permission posture: its vocabulary, the typed
//! pair a spawn request resolves to, and the reconstruction a resume reads
//! back from a registry row.
//!
//! The lane this feeds used to collapse the posture onto one `bool`
//! (`resolve_thread_posture` -> `Result<bool, String>`), which made read-only
//! and workspace-write the same answer and dropped the approval half on the
//! floor. The pair is the fix: both halves survive to the frame, and the
//! operator's exact input rides along so a record can replay the request
//! rather than a derived name.

use serde_json::Value;

/// The sandbox half, in the spellings codex's own CLI takes (`--sandbox
/// <MODE>`, and the scalar `sandbox` on `thread/start` / `thread/resume`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexSandbox {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl CodexSandbox {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "read-only" => Some(Self::ReadOnly),
            "workspace-write" => Some(Self::WorkspaceWrite),
            "danger-full-access" => Some(Self::DangerFullAccess),
            _ => None,
        }
    }

    /// The scalar `thread/start` / `thread/resume` take.
    pub fn as_scalar(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }

    /// The `type` spelling the app-server reports a RESOLVED posture with
    /// (`/result/sandbox/type`).
    pub fn as_policy_type(self) -> &'static str {
        match self {
            Self::ReadOnly => "readOnly",
            Self::WorkspaceWrite => "workspaceWrite",
            Self::DangerFullAccess => "dangerFullAccess",
        }
    }
}

/// The approval half, in codex's own `--ask-for-approval` vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexApproval {
    Untrusted,
    OnFailure,
    OnRequest,
    Never,
}

impl CodexApproval {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "untrusted" => Some(Self::Untrusted),
            "on-failure" => Some(Self::OnFailure),
            "on-request" => Some(Self::OnRequest),
            "never" => Some(Self::Never),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Untrusted => "untrusted",
            Self::OnFailure => "on-failure",
            Self::OnRequest => "on-request",
            Self::Never => "never",
        }
    }
}

/// The posture a codex spawn REQUESTED: both halves plus the operator's exact
/// string. `requested` is empty when the spawn named no mode (the bare `yolo`
/// bool or the default), because an unset mode must not grow a name that reads
/// as a decision nobody made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexPosture {
    pub sandbox: CodexSandbox,
    pub approval: CodexApproval,
    pub requested: String,
}

impl CodexPosture {
    /// The posture a bare `yolo` bool names, for callers that predate the
    /// pair and for tests. `yolo` is `danger-full-access` with approvals
    /// never; absent is the bounded default.
    pub fn from_yolo(yolo: bool) -> Self {
        if yolo {
            Self::full_access()
        } else {
            Self::bounded()
        }
    }

    /// `danger-full-access:never` - what `--yolo` always meant on this lane.
    pub fn full_access() -> Self {
        Self {
            sandbox: CodexSandbox::DangerFullAccess,
            approval: CodexApproval::Never,
            requested: String::new(),
        }
    }

    /// `workspace-write:never` - the lane's bounded default frame.
    pub fn bounded() -> Self {
        Self {
            sandbox: CodexSandbox::WorkspaceWrite,
            approval: CodexApproval::Never,
            requested: String::new(),
        }
    }

    pub fn is_full_access(&self) -> bool {
        self.sandbox == CodexSandbox::DangerFullAccess
    }

    /// Rebuild the posture a registry row records. The v35 requested mode
    /// wins when it still resolves; otherwise the recorded posture name sets
    /// the sandbox half with the lane's historical `never` approval (the only
    /// approval a pre-v35 row can mean), and a row with neither gets the safe
    /// bounded default. Never guesses a half the record does not carry: a
    /// garbage requested mode falls through to the name rather than being
    /// repaired into a posture nobody asked for.
    pub fn from_record(requested: Option<&str>, posture_name: Option<&str>) -> Self {
        if let Some(mode) = requested.map(str::trim).filter(|m| !m.is_empty()) {
            if let Ok(posture) = resolve_thread_posture(None, Some(mode)) {
                return posture;
            }
        }
        Self {
            sandbox: posture_name
                .and_then(CodexSandbox::parse)
                .unwrap_or(CodexSandbox::WorkspaceWrite),
            approval: CodexApproval::Never,
            requested: String::new(),
        }
    }
}

/// Resolve the thread lane's launch posture from BOTH spellings a spawn can
/// use, or refuse.
///
/// `spawn_codex_thread_lane` read the `yolo` bool alone and dropped
/// `permission_mode` on the floor. Dropping an axis is not neutral here: the
/// lane then starts bounded, which is a SILENT downgrade of the exact posture
/// the caller was trying to name. Both CLI front doors happen to refuse
/// `--permission-mode` for codex today, so nothing reaches this with the key
/// set - but the daemon RPC is the trust boundary, and a boundary that ignores
/// a permission axis it does not understand is one caller away from the defect.
///
/// The vocabulary is codex's own, and it is the one `permission_pane_tokens`
/// maps for the pane lane (`fno.agents.mux_spawn`): the `full-auto` and `yolo`
/// shortcuts, or the explicit `<sandbox>:<approval>` pair. Keep the two in
/// step; a third spelling invented here would be a second vocabulary for one
/// axis.
///
/// Fail closed on anything else, and on both keys at once - "one knob at a
/// time" is the rule the CLIs already enforce, and guessing which of two
/// disagreeing postures a caller meant is how a bypass gets granted by
/// accident.
pub fn resolve_thread_posture(
    yolo: Option<bool>,
    permission_mode: Option<&str>,
) -> Result<CodexPosture, String> {
    let mode = permission_mode.map(str::trim).filter(|m| !m.is_empty());
    let Some(mode) = mode else {
        return Ok(CodexPosture::from_yolo(yolo.unwrap_or(false)));
    };
    if yolo == Some(true) {
        return Err(format!(
            "spawn carries both yolo=true and permission_mode {mode:?}; pass one \
             (they are mutually exclusive, as on `fno agents spawn`)"
        ));
    }
    match mode {
        "yolo" => Ok(CodexPosture {
            sandbox: CodexSandbox::DangerFullAccess,
            approval: CodexApproval::Never,
            requested: mode.to_string(),
        }),
        "full-auto" => Ok(CodexPosture {
            sandbox: CodexSandbox::WorkspaceWrite,
            approval: CodexApproval::OnFailure,
            requested: mode.to_string(),
        }),
        _ => match mode.split_once(':') {
            Some((sandbox, approval)) if !sandbox.is_empty() && !approval.is_empty() => {
                let sandbox = CodexSandbox::parse(sandbox).ok_or_else(|| {
                    format!(
                        "codex permission_mode {mode:?} names sandbox {sandbox:?}, which the \
                         thread lane cannot resolve; use read-only, workspace-write, or \
                         danger-full-access"
                    )
                })?;
                let approval = CodexApproval::parse(approval).ok_or_else(|| {
                    format!(
                        "codex permission_mode {mode:?} names approval {approval:?}, which the \
                         thread lane cannot resolve; use untrusted, on-failure, on-request, \
                         or never"
                    )
                })?;
                Ok(CodexPosture {
                    sandbox,
                    approval,
                    requested: mode.to_string(),
                })
            }
            _ => Err(format!(
                "codex permission_mode {mode:?} unmappable on the thread lane; use a shortcut \
                 (full-auto, yolo) or the <sandbox>:<approval> form \
                 (e.g. workspace-write:on-request)"
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC1-HP: a typed pair survives to both halves, and the operator's exact
    /// string rides beside them.
    #[test]
    fn typed_pairs_keep_both_halves_distinct() {
        for (mode, sandbox, approval) in [
            (
                "read-only:on-request",
                CodexSandbox::ReadOnly,
                CodexApproval::OnRequest,
            ),
            (
                "workspace-write:on-failure",
                CodexSandbox::WorkspaceWrite,
                CodexApproval::OnFailure,
            ),
            (
                "danger-full-access:never",
                CodexSandbox::DangerFullAccess,
                CodexApproval::Never,
            ),
            (
                "read-only:untrusted",
                CodexSandbox::ReadOnly,
                CodexApproval::Untrusted,
            ),
        ] {
            let posture = resolve_thread_posture(None, Some(mode)).expect("pair maps");
            assert_eq!(posture.sandbox, sandbox, "sandbox half of {mode}");
            assert_eq!(posture.approval, approval, "approval half of {mode}");
            assert_eq!(posture.requested, mode, "exact requested string");
        }
        // The two bounded sandbox spellings are no longer the same answer.
        let read_only = resolve_thread_posture(None, Some("read-only:never")).unwrap();
        let write = resolve_thread_posture(None, Some("workspace-write:never")).unwrap();
        assert_ne!(read_only.sandbox, write.sandbox);
    }

    /// AC1-EDGE: the shortcuts carry their own halves. `yolo` and
    /// `danger-full-access:never` both name full access while `full-auto`
    /// stays bounded, and each records its own approval.
    #[test]
    fn shortcuts_carry_their_own_halves() {
        let yolo = resolve_thread_posture(Some(true), None).unwrap();
        assert!(yolo.is_full_access());
        assert_eq!(yolo.approval, CodexApproval::Never);

        let named = resolve_thread_posture(None, Some("yolo")).unwrap();
        assert_eq!(named.sandbox, yolo.sandbox);
        assert_eq!(named.approval, yolo.approval);
        assert_eq!(named.requested, "yolo");

        let auto = resolve_thread_posture(None, Some("full-auto")).unwrap();
        assert!(!auto.is_full_access());
        assert_eq!(auto.approval, CodexApproval::OnFailure);
        assert_eq!(auto.requested, "full-auto");

        let paired = resolve_thread_posture(None, Some("danger-full-access:never")).unwrap();
        assert!(paired.is_full_access());
        assert_eq!(paired.requested, "danger-full-access:never");
    }

    /// AC1-ERR: fail closed, and say which value. An unknown half refuses
    /// even though the other half parsed; no posture is guessed.
    #[test]
    fn refuses_rather_than_degrading_or_guessing() {
        for mode in [
            "accept-edits",
            "bypassPermissions",
            "danger-full-access",
            ":never",
        ] {
            let err = resolve_thread_posture(None, Some(mode))
                .expect_err("an unmappable mode must refuse");
            assert!(
                err.contains(mode),
                "refusal must name the value it could not map; got: {err}"
            );
        }
        let err = resolve_thread_posture(None, Some("full-access:never"))
            .expect_err("an unknown sandbox must refuse");
        assert!(err.contains("full-access"), "got: {err}");
        let err = resolve_thread_posture(None, Some("read-only:sometimes"))
            .expect_err("an unknown approval must refuse");
        assert!(err.contains("sometimes"), "got: {err}");
        // The sandbox half that parsed must not leak into a partial answer.
        let err = resolve_thread_posture(None, Some("workspace-write:sometimes"))
            .expect_err("an unknown approval must refuse");
        assert!(!err.contains("workspace-write:never"), "got: {err}");
        let err = resolve_thread_posture(Some(true), Some("full-auto"))
            .expect_err("two postures at once must refuse");
        assert!(err.contains("mutually exclusive"), "got: {err}");
        // An empty mode is UNSET, not a mode: the bool still decides.
        assert_eq!(
            resolve_thread_posture(Some(true), Some("")).unwrap(),
            CodexPosture::full_access()
        );
        assert_eq!(
            resolve_thread_posture(None, None).unwrap(),
            CodexPosture::bounded()
        );
    }

    /// AC6/AC3 groundwork: a record rebuilds the posture it names, a garbage
    /// requested mode falls through to the posture name, and an empty record
    /// reads as the safe default.
    #[test]
    fn from_record_prefers_the_requested_mode_then_the_name() {
        let posture =
            CodexPosture::from_record(Some("read-only:on-request"), Some("workspace-write"));
        assert_eq!(posture.sandbox, CodexSandbox::ReadOnly);
        assert_eq!(posture.approval, CodexApproval::OnRequest);

        let by_name = CodexPosture::from_record(Some("garbage:mode"), Some("danger-full-access"));
        assert!(by_name.is_full_access());
        assert_eq!(by_name.approval, CodexApproval::Never);

        let bare = CodexPosture::from_record(None, None);
        assert_eq!(bare, CodexPosture::bounded());
    }
}

/// ---------------------------------------------------------------------------
/// The one per-harness permission vocabulary (ported from the pane lane's
/// Python, `fno.agents.mux_spawn.permission_pane_tokens`, which shrinks to a
/// bridge over this answer). Fail-closed: an unmappable (provider, value)
/// pair refuses with the harness's own vocabulary, never a silent downgrade.
/// Keep every refusal message byte-identical to the Python it replaced.
/// ---------------------------------------------------------------------------
pub fn permission_pane_tokens(provider: &str, mode: &str) -> Result<Vec<String>, String> {
    if mode.is_empty() {
        return Err("--permission-mode requires a value".to_string());
    }
    match provider {
        "claude" => {
            // Exact passthrough; claude's own CLI validates the vocabulary.
            Ok(vec!["--permission-mode".into(), mode.to_string()])
        }
        "gemini" => Ok(if mode == "yolo" {
            vec!["--yolo".into()]
        } else {
            vec!["--approval-mode".into(), mode.to_string()]
        }),
        "codex" => match mode {
            "full-auto" => Ok(vec!["--full-auto".into()]),
            "yolo" => Ok(vec!["--dangerously-bypass-approvals-and-sandbox".into()]),
            _ => match mode.split_once(':') {
                Some((sandbox, approval)) if !sandbox.is_empty() && !approval.is_empty() => {
                    Ok(vec![
                        "--sandbox".into(),
                        sandbox.to_string(),
                        "--ask-for-approval".into(),
                        approval.to_string(),
                    ])
                }
                _ => Err(format!(
                    "codex --permission-mode {mode:?} unmappable; use a shortcut \
                     (full-auto, yolo) or the <sandbox>:<approval> form \
                     (e.g. workspace-write:on-request)"
                )),
            },
        },
        "opencode" => {
            if mode == "auto" {
                Ok(vec!["--auto".into()])
            } else {
                Err(format!(
                    "opencode --permission-mode {mode:?} unmappable; only 'auto' maps \
                     (--auto). Per-tool permissions are config-only (permission table)."
                ))
            }
        }
        "agy" => {
            if mode == "skip" {
                // The argv already carries --dangerously-skip-permissions
                // unconditionally.
                Ok(Vec::new())
            } else {
                Err(format!(
                    "agy --permission-mode {mode:?} unmappable; only 'skip' maps \
                     (--dangerously-skip-permissions). Finer control is config-only \
                     (toolPermission)."
                ))
            }
        }
        "pi" => Err(format!(
            "pi --permission-mode {mode:?} unmappable, and this is an absence in \
             pi rather than a gap in fno: pi ships NO permission popups (its own \
             docs say so) and `pi --help` carries no bypass flag, so there is \
             nothing to answer and nothing to skip. `--approve` trusts \
             project-local FILES, a different axis, and stays an operator choice."
        )),
        "cursor-agent" => {
            if mode == "force" || mode == "yolo" {
                Ok(vec!["--force".into()])
            } else {
                Err(format!(
                    "cursor-agent --permission-mode {mode:?} unmappable; only \
                     'force' maps to --force; --auto-review and --sandbox are separate settings"
                ))
            }
        }
        // grok's `--permission-mode <MODE>` carries the same vocabulary claude
        // does (default, acceptEdits, auto, dontAsk, bypassPermissions, plan
        // per `grok --help` on 1.0.13), so this is exact passthrough and
        // grok's own CLI validates it. `--always-approve` is the bypass AXIS
        // and stays the yolo spelling, not a permission-mode value.
        "grok" => Ok(vec!["--permission-mode".into(), mode.to_string()]),
        _ => Err(format!(
            "provider {provider:?} has no permission-mode mapping"
        )),
    }
}

/// The mappability answer the spawn seam and both front doors read: whether
/// the (harness, mode, substrate) triple carries the permission axis. The
/// capability table is the declaration for the thread lanes; the pane lane is
/// answered by the vocabulary itself; headless stays claude-only, matching
/// the client.rs guard. An undeclared lane answers a declared `false` so the
/// seam names the skip, never a guessed yes.
pub fn permission_mappable(provider: &str, mode: &str, substrate: &str) -> Result<bool, String> {
    if mode.trim().is_empty() {
        return Err("--permission-mode requires a value".to_string());
    }
    match substrate {
        "pane" => permission_pane_tokens(provider, mode).map(|_| true),
        "thread" | "bg" => {
            let declared = crate::harness_capabilities::HarnessContract::packaged()
                .ok()
                .and_then(|contract| contract.harness.get(provider).cloned())
                .map(|row| {
                    row.thread
                        .map(|thread| thread.carries.iter().any(|axis| axis == "permission_mode"))
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            if !declared {
                return Ok(false);
            }
            if provider == "codex" {
                // The thread lane resolves the codex posture natively.
                return resolve_thread_posture(None, Some(mode)).map(|_| true);
            }
            permission_pane_tokens(provider, mode).map(|_| true)
        }
        "headless" => Ok(provider == "claude"),
        _ => Err(format!(
            "unknown substrate {substrate:?}; use pane, thread (bg), or headless"
        )),
    }
}

#[cfg(test)]
mod mappable_tests {
    use super::*;

    #[test]
    fn pane_answers_match_the_python_vocabulary() {
        assert_eq!(
            permission_pane_tokens("claude", "bypassPermissions").unwrap(),
            vec!["--permission-mode", "bypassPermissions"]
        );
        assert_eq!(
            permission_pane_tokens("codex", "full-auto").unwrap(),
            vec!["--full-auto"]
        );
        assert_eq!(
            permission_pane_tokens("codex", "workspace-write:on-request").unwrap(),
            vec![
                "--sandbox",
                "workspace-write",
                "--ask-for-approval",
                "on-request"
            ]
        );
        assert_eq!(
            permission_pane_tokens("gemini", "yolo").unwrap(),
            vec!["--yolo"]
        );
        assert_eq!(
            permission_pane_tokens("agy", "skip").unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            permission_pane_tokens("cursor-agent", "force").unwrap(),
            vec!["--force"]
        );
        assert!(permission_pane_tokens("pi", "yolo").is_err());
        assert!(permission_pane_tokens("codex", "banana").is_err());
        assert!(permission_pane_tokens("nonexistent", "yolo").is_err());
    }

    /// AC4-HP: the codex thread lane carries the axis, per the capability
    /// table, and a mapped pair form resolves natively there. An undeclared
    /// lane answers false; a DECLARED lane with an unmappable mode refuses
    /// with the harness's own vocabulary (AC4-ERR).
    #[test]
    fn the_capability_table_declares_the_codex_thread_lane() {
        assert!(permission_mappable("codex", "workspace-write:on-request", "thread").unwrap());
        assert!(!permission_mappable("opencode", "auto", "thread").unwrap());
        assert!(!permission_mappable("pi", "yolo", "thread").unwrap());
        let err = permission_mappable("codex", "banana", "thread")
            .expect_err("a declared lane refuses an unmappable mode by name");
        assert!(err.contains("banana"), "got: {err}");
        assert!(permission_mappable("claude", "bypassPermissions", "bg").unwrap());
        assert!(!permission_mappable("codex", "yolo", "headless").unwrap_or(false));
    }
}

/// The hidden client verb: one JSON payload in
/// (`{"provider", "mode", "substrate"?}`), one answer out. No `substrate`
/// asks the pane vocabulary; a `substrate` asks the mappability answer. A
/// mapping refusal lands as `{"refusal": "..."}` so the Python bridge can
/// re-raise it in the caller's own error type; a transport problem exits 2
/// with a named reason on stderr, never a silent zero.
pub fn run_permission_tokens(args: &[String]) -> i32 {
    use std::io::Read;
    let mut payload = String::new();
    let raw = if let Some(path) = args.iter().find_map(|a| a.strip_prefix("--payload-file=")) {
        std::fs::read_to_string(path)
    } else {
        std::io::stdin()
            .read_to_string(&mut payload)
            .map(|_| payload)
    };
    let raw = match raw {
        Ok(text) => text,
        Err(e) => {
            eprintln!("permission-tokens: cannot read payload: {e}");
            return 2;
        }
    };
    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("permission-tokens: bad payload: {e}");
            return 2;
        }
    };
    let provider = parsed.get("provider").and_then(Value::as_str).unwrap_or("");
    let mode = parsed.get("mode").and_then(Value::as_str).unwrap_or("");
    if let Some(substrate) = parsed.get("substrate").and_then(Value::as_str) {
        return match permission_mappable(provider, mode, substrate) {
            Ok(mappable) => {
                println!(
                    "{}",
                    serde_json::json!({"mappable": mappable, "refusal": null})
                );
                0
            }
            Err(reason) => {
                println!(
                    "{}",
                    serde_json::json!({"mappable": false, "refusal": reason})
                );
                0
            }
        };
    }
    match permission_pane_tokens(provider, mode) {
        Ok(tokens) => {
            println!("{}", serde_json::json!({"tokens": tokens, "refusal": null}));
            0
        }
        Err(reason) => {
            println!("{}", serde_json::json!({"tokens": [], "refusal": reason}));
            0
        }
    }
}
