//! The one resolver of a Claude re-entry (x-d285).
//!
//! Every door that re-enters a Claude session (`fno agents attach`, the dead
//! and live resume arms, the mux attach/ResumeAgent gestures, the recovery
//! verb) must restore the SAME launch context the worker was spawned with or
//! refuse before launching anything. Before this module each door rebuilt its
//! own provider argv, and the account binding dispatch applies at spawn was
//! applied on none of them - a worker launched under one account was
//! reattached under whatever namespace the caller happened to sit in, which
//! either missed the transcript (loud) or billed the wrong account (silent).
//!
//! The contract, stated once here:
//!
//! - The route contract is the FULL launch context: harness session id,
//!   launch account, `CLAUDE_CONFIG_DIR`, `route_settings_path`, cwd, and the
//!   recorded substrate. A re-entry either restores all of it or refuses
//!   naming the missing piece. Missing evidence never means "default
//!   Anthropic".
//! - Secrets never cross this boundary. The plan carries PATHS and ids; the
//!   route-settings file is opened only to prove it still records a route,
//!   and no value read from it is ever emitted.
//! - Account truth lives in the Python account store, so account resolution
//!   shells `fno config accounts show <id> --print-binding` (the same
//!   shells-don't-reimplement rule the loop's account picker follows). The
//!   ambient `CLAUDE_CONFIG_DIR` is never consulted.
//! - A proven default row (`launch_account: "default"`, no route file) keeps
//!   the historical bare behavior: no account prefix, no settings flag. A
//!   ROUTED or non-Anthropic row with an UNKNOWN account refuses, because
//!   guessing a namespace is the wrong-bill door this module exists to close.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::state::{load_registry, MuxRef, Registry, RegistryEntry};

/// Exit code for a refused re-entry: the evidence is named on stderr, no argv
/// was constructed, and nothing launched.
pub const REENTRY_REFUSED_EXIT: i32 = 3;

/// The transitions this resolver serves. One vocabulary so a plan's consumer
/// can tell an attach (re-enter a live session) from a resume (relaunch a
/// dead one) from a recover (operator-selected id) without re-deriving it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReentryTransition {
    Attach,
    Resume,
    Recover,
}

impl ReentryTransition {
    pub fn as_str(self) -> &'static str {
        match self {
            ReentryTransition::Attach => "attach",
            ReentryTransition::Resume => "resume",
            ReentryTransition::Recover => "recover",
        }
    }

    /// True only for RECOVER, the explicit chooser. An attach re-enters a
    /// session that is already running (the row's own transport key is the
    /// target), and a smart resume continues the row's PRIMARY id - the
    /// canonical address delivery already follows - recording any different
    /// observed id as the related id rather than demanding a selection.
    /// Recovery exists precisely to select between two valid ids, so it
    /// refuses until the caller names one.
    fn requires_selection(self) -> bool {
        matches!(self, ReentryTransition::Recover)
    }
}

/// The machine-readable re-entry plan. `argv` and `env` carry only ids and
/// paths; `resolved` is the positive marker a consumer must assert before
/// spawning anything.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ReentryPlan {
    pub resolved: bool,
    pub transition: String,
    pub name: String,
    pub fno_id: Option<String>,
    /// The selected harness session id (primary, or the related id when the
    /// caller selected it).
    pub session_id: String,
    /// The 8-hex claude jobId where one is derivable ("" otherwise).
    pub short_id: String,
    /// "default" or the account id, exactly as the row records it.
    pub launch_account: String,
    /// The account's config dir, or None when the row rides the default
    /// namespace.
    pub claude_config_dir: Option<String>,
    pub route_settings_path: Option<String>,
    pub cwd: String,
    pub substrate: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mux: Option<MuxRef>,
    /// The provider invocation, shell-safe. Contains ids and file PATHS
    /// only; a route's credential stays inside the 0600 settings file the
    /// argv names.
    pub argv: Vec<String>,
    /// Env the caller must apply for the argv to land in the right
    /// namespace. Only ever `CLAUDE_CONFIG_DIR`; a value is a path, never a
    /// credential.
    pub env: BTreeMap<String, String>,
}

/// How an account id resolves: its config dir when the lane has one. `Err`
/// carries the refusal receipt (unknown account, unresolvable record).
pub type AccountBinding = dyn Fn(&str) -> Result<Option<String>, String>;

/// The production account resolver: shells the one implementation of account
/// truth (the Python store) and parses its secret-free `--print-binding`
/// projection. Same rule as the loop's picker: never reimplement the store.
pub fn shell_account_binding(account_id: &str) -> Result<Option<String>, String> {
    let out = std::process::Command::new("fno")
        .args(["config", "accounts", "show", account_id, "--print-binding"])
        .output()
        .map_err(|e| format!("could not run `fno config accounts show`: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let reason = stderr
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .next_back()
            .unwrap_or("no reason given");
        return Err(reason.to_string());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines().map(str::trim) {
        if let Some(dir) = line.strip_prefix("CLAUDE_CONFIG_DIR=") {
            if !dir.is_empty() {
                return Ok(Some(dir.to_string()));
            }
        }
    }
    // Exit 0 with no config-dir line: the account exists and rides the
    // default namespace (an api-key lane). The id resolving is the fact; the
    // namespace is the default slot.
    Ok(None)
}

/// Prove the recorded route-settings file still records a route. Reads it,
/// checks that at least one non-empty value other than the provider STAMP
/// exists, and emits nothing it read - the file holds a live credential and
/// the plan is a loggable artifact.
pub fn validate_route_settings(path: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("route settings file {path} is unreadable: {e}"))?;
    let payload: Value = serde_json::from_str(&text)
        .map_err(|e| format!("route settings file {path} is malformed: {e}"))?;
    let env = payload
        .get("env")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("route settings file {path} has no env mapping"))?;
    let carries_route = env.iter().any(|(k, v)| {
        // The stamp NAMES a route; it never is one. Counting it would let a
        // scrub-floor-only file pass and relaunch with no endpoint, no auth
        // and no model - the silent wrong-bill shape.
        k != "FNO_ROUTE_PROVIDER" && v.as_str().is_some_and(|s| !s.is_empty())
    });
    if !carries_route {
        return Err(format!(
            "route settings file {path} carries only the scrub floor; the route it recorded is gone"
        ));
    }
    Ok(())
}

fn derived_short_id(session_id: &str) -> String {
    // The claude jobId is the leading 8 hex of the session UUID by
    // construction; re-derive it only when the id actually has that shape.
    let lead = session_id.split('-').next().unwrap_or("");
    if lead.len() == 8 && lead.bytes().all(|b| b.is_ascii_hexdigit()) {
        lead.to_ascii_lowercase()
    } else {
        String::new()
    }
}

fn substrate_of(entry: &RegistryEntry) -> &'static str {
    if entry.mux.is_some() {
        "pane"
    } else if entry.host_mode.as_deref() == Some(crate::state::HOST_MODE_INTERACTIVE) {
        "daemon"
    } else {
        "bg"
    }
}

/// Resolve one row's re-entry plan, or refuse naming the missing evidence.
///
/// Read-only: no registry write, no launch. `select_session` is the explicit
/// id selection (`--session`); it must name the row's primary or related id.
pub fn resolve_reentry_with(
    registry: &Registry,
    name: &str,
    transition: ReentryTransition,
    select_session: Option<&str>,
    account_binding: &AccountBinding,
) -> Result<ReentryPlan, String> {
    if name.trim().is_empty() {
        return Err("no agent named".to_string());
    }
    let candidates: Vec<&RegistryEntry> =
        registry.entries.iter().filter(|e| e.name == name).collect();
    let entry = match candidates.as_slice() {
        [] => return Err(format!("no registry row named {name:?}")),
        [one] => *one,
        _ => {
            return Err(format!(
                "agent name {name:?} is ambiguous across {} rows; name one row exactly",
                candidates.len()
            ))
        }
    };
    if entry.harness.as_deref().is_some_and(|h| h != "claude") {
        return Err(format!(
            "row {name:?} is a {:?} row; the re-entry resolver is claude-only",
            entry.harness
        ));
    }

    // Session identity: primary first, one optional related. An explicit
    // selection must name one of them. A LAUNCHING transition on a two-id row
    // refuses until the caller selects - recovery chooses a valid id, never a
    // winner, and resume has the same two valid ids in front of it.
    let primary = entry.harness_session_id.clone().unwrap_or_default();
    let related = entry.related_session_id.clone().unwrap_or_default();
    let session_id = match select_session {
        Some(id) => {
            if !id.is_empty() && (primary == id || (!related.is_empty() && related == id)) {
                id.to_string()
            } else {
                let a = if primary.is_empty() { "-" } else { primary.as_str() };
                let b = if related.is_empty() { "-" } else { related.as_str() };
                return Err(format!(
                    "session {id:?} is not one of the ids row {name:?} records (primary {a}, related {b})"
                ));
            }
        }
        None => {
            if transition.requires_selection() && !primary.is_empty() && !related.is_empty() {
                return Err(format!(
                    "row {name:?} holds two valid session ids ({primary}, {related}); name one with --session"
                ));
            }
            if !primary.is_empty() {
                primary.clone()
            } else if !related.is_empty() {
                related.clone()
            } else {
                return Err(format!(
                    "row {name:?} records no harness session id; nothing to re-enter"
                ));
            }
        }
    };
    if session_id.is_empty() {
        return Err(format!("row {name:?} records no harness session id; nothing to re-enter"));
    }
    let short_id = if !entry.short_id.is_empty() {
        entry.short_id.clone()
    } else {
        derived_short_id(&session_id)
    };
    if short_id.is_empty() && transition == ReentryTransition::Attach {
        return Err(format!(
            "row {name:?} carries no transport key (short_id) and the session id derives none; claude attach has no target"
        ));
    }

    // Route evidence: a recorded path must still hold a route.
    if let Some(path) = entry.route_settings_path.as_deref() {
        if !path.is_empty() {
            validate_route_settings(path)?;
        }
    }

    // The account axis. Routed or non-Anthropic rows refuse on an unknown
    // account; a proven default row keeps the historical bare behavior.
    let routed = entry
        .route_settings_path
        .as_deref()
        .is_some_and(|p| !p.is_empty());
    let non_anthropic = entry
        .provider
        .as_deref()
        .is_some_and(|p| !p.is_empty() && p != "anthropic");
    let launch_account = entry.launch_account.clone();
    let claude_config_dir = match launch_account.as_deref() {
        None if routed || non_anthropic => {
            return Err(format!(
                "row {name:?} is {} and records no launch account; re-entering it would guess a namespace - restamp the row or re-spawn the worker",
                if routed { "routed" } else { "on a non-Anthropic provider" }
            ))
        }
        None | Some("default") => None,
        Some(id) => match account_binding(id) {
            Ok(dir) => dir,
            Err(reason) => {
                return Err(format!(
                    "launch account {id:?} recorded on row {name:?} no longer resolves: {reason}"
                ))
            }
        },
    };

    // cwd: a relaunch needs a working directory that exists; an attach does
    // too (claude resolves the session against its project dir).
    if !Path::new(&entry.cwd).is_dir() {
        return Err(format!(
            "row {name:?} cwd {:?} is unreachable; re-entry would launch somewhere that does not exist",
            entry.cwd
        ));
    }

    let mut argv: Vec<String> = Vec::new();
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    if let Some(dir) = claude_config_dir.as_deref() {
        env.insert("CLAUDE_CONFIG_DIR".to_string(), dir.to_string());
    }
    match transition {
        ReentryTransition::Attach => {
            argv.push("claude".into());
            argv.push("attach".into());
            argv.push(short_id.clone());
        }
        ReentryTransition::Resume | ReentryTransition::Recover => {
            argv.push("claude".into());
            argv.push("--resume".into());
            argv.push(session_id.clone());
        }
    }
    if let Some(path) = entry
        .route_settings_path
        .as_deref()
        .filter(|p| !p.is_empty())
    {
        argv.push("--settings".into());
        argv.push(path.to_string());
    }

    Ok(ReentryPlan {
        resolved: true,
        transition: transition.as_str().to_string(),
        name: entry.name.clone(),
        fno_id: entry.fno_id.clone(),
        session_id,
        short_id,
        launch_account: launch_account.unwrap_or_else(|| "unknown".to_string()),
        claude_config_dir,
        route_settings_path: entry
            .route_settings_path
            .clone()
            .filter(|p| !p.is_empty()),
        cwd: entry.cwd.clone(),
        substrate: substrate_of(entry).to_string(),
        mux: entry.mux.clone(),
        argv,
        env,
    })
}

/// The registry-reading wrapper the CLI action calls: load, resolve, refuse
/// on an unreadable store ("registry incomplete" is evidence missing, never
/// an empty answer).
pub fn resolve_reentry(
    registry_path: &Path,
    name: &str,
    transition: ReentryTransition,
    select_session: Option<&str>,
) -> Result<ReentryPlan, String> {
    let registry = load_registry(registry_path)
        .map_err(|e| format!("registry unreadable at {}: {e}", registry_path.display()))?;
    resolve_reentry_with(
        &registry,
        name,
        transition,
        select_session,
        &shell_account_binding,
    )
}

/// The `fno-agents reentry-plan` machine action:
/// `reentry-plan <name> [--transition attach|resume|recover] [--session <id>]`.
/// Exit 0 prints the plan as one JSON object; exit 3 prints the refusal on
/// stderr and constructs no argv.
pub fn run_reentry_plan(args: &[String], home: &crate::paths::AgentsHome) -> i32 {
    let mut name: Option<&str> = None;
    let mut transition = ReentryTransition::Resume;
    let mut select_session: Option<String> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--transition" => match it.next().map(String::as_str) {
                Some("attach") => transition = ReentryTransition::Attach,
                Some("resume") => transition = ReentryTransition::Resume,
                Some("recover") => transition = ReentryTransition::Recover,
                other => {
                    eprintln!("reentry-plan: unknown --transition {other:?} (attach|resume|recover)");
                    return 2;
                }
            },
            "--session" => match it.next() {
                Some(id) => select_session = Some(id.clone()),
                None => {
                    eprintln!("reentry-plan: --session needs a value");
                    return 2;
                }
            },
            other if !other.starts_with('-') && name.is_none() => name = Some(other),
            other => {
                eprintln!("reentry-plan: unexpected argument {other:?}");
                return 2;
            }
        }
    }
    let Some(name) = name else {
        eprintln!("reentry-plan: an agent name is required");
        return 2;
    };
    match resolve_reentry(
        &home.registry_json(),
        name,
        transition,
        select_session.as_deref(),
    ) {
        Ok(plan) => {
            match serde_json::to_string_pretty(&plan) {
                Ok(json) => println!("{json}"),
                Err(e) => {
                    eprintln!("reentry-plan: could not serialize the plan: {e}");
                    return 1;
                }
            }
            0
        }
        Err(reason) => {
            eprintln!("reentry: refused: {reason}");
            REENTRY_REFUSED_EXIT
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Registry;

    const SECRET: &str = "zai-secret-token";

    fn row(name: &str) -> RegistryEntry {
        RegistryEntry {
            name: name.into(),
            short_id: String::new(),
            legacy_provider: String::new(),
            provider: None,
            model: None,
            model_basis: None,
            effort: None,
            cwd: std::env::temp_dir().to_string_lossy().to_string(),
            project_root: String::new(),
            session_id: None,
            claude_session_uuid: None,
            messaging_socket_path: None,
            codex_session_id: None,
            gemini_session_id: None,
            mcp_channel_id: None,
            host_mode: None,
            cc_session_id: None,
            status: crate::AgentStatus::Live,
            last_message_at: None,
            created_at: "2026-08-27T00:00:00Z".into(),
            pid: None,
            pid_start_time: None,
            log_path: None,
            last_reconciled_at: None,
            inside_leg: None,
            exited_at: None,
            mux: None,
            screen_state: None,
            crown_level: None,
            crown_scope: None,
            crown_grantor: None,
            route_settings_path: None,
            fno_id: None,
            delivery_policy: None,
            origin: None,
            spawn_trigger: None,
            spawned_by_session: None,
            spawned_by_harness: None,
            spawned_by_cwd: None,
            legacy_claude_short_id: None,
            harness: Some("claude".into()),
            harness_session_id: None,
            predecessor_session_ids: Vec::new(),
            forked_from_session_id: None,
            launch_account: None,
            related_session_id: None,
            sandbox_posture: None,
        }
    }

    fn reg(entries: Vec<RegistryEntry>) -> Registry {
        Registry {
            schema_version: crate::state::REGISTRY_SCHEMA_VERSION,
            entries,
        }
    }

    fn binding_ok(id: &str) -> Result<Option<String>, String> {
        if id == "makers" {
            Ok(Some("/acct/makers/.claude".into()))
        } else {
            Err(format!("account {id:?} is not registered"))
        }
    }

    fn write_route(path: &std::path::Path, floor_only: bool) {
        let env = if floor_only {
            serde_json::json!({
                "ANTHROPIC_API_KEY": "",
                "ANTHROPIC_BASE_URL": "",
                "FNO_ROUTE_PROVIDER": "zai",
            })
        } else {
            serde_json::json!({
                "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
                "ANTHROPIC_AUTH_TOKEN": SECRET,
                "FNO_ROUTE_PROVIDER": "zai",
            })
        };
        std::fs::write(path, serde_json::json!({"env": env}).to_string()).unwrap();
    }

    #[test]
    fn reentry_plan_resolves_a_complete_routed_glm_row() {
        let dir = std::env::temp_dir().join("reentry-test-route-a.json");
        write_route(&dir, false);
        let mut e = row("glm");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "aaaaaaaa".into();
        e.provider = Some("zai".into());
        e.launch_account = Some("makers".into());
        e.route_settings_path = Some(dir.to_string_lossy().to_string());
        e.cwd = std::env::temp_dir().to_string_lossy().to_string();

        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "glm",
            ReentryTransition::Resume,
            None,
            &binding_ok,
        )
        .unwrap();
        assert!(plan.resolved);
        assert_eq!(plan.claude_config_dir.as_deref(), Some("/acct/makers/.claude"));
        let want_path = dir.to_string_lossy().to_string();
        assert_eq!(plan.route_settings_path.as_deref(), Some(want_path.as_str()));
        assert_eq!(plan.argv, vec![
            "claude".to_string(),
            "--resume".to_string(),
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string(),
            "--settings".to_string(),
            dir.to_string_lossy().to_string(),
        ]);
        assert_eq!(plan.env.get("CLAUDE_CONFIG_DIR").map(String::as_str), Some("/acct/makers/.claude"));
        // Secrets never cross the boundary: the token lives in the 0600 file
        // the plan only NAMES.
        let json = serde_json::to_string(&plan).unwrap();
        assert!(!json.contains(SECRET));
    }

    #[test]
    fn reentry_plan_refuses_an_unknown_account_on_a_routed_row() {
        let dir = std::env::temp_dir().join("reentry-test-route-b.json");
        write_route(&dir, false);
        let mut e = row("glm");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.provider = Some("zai".into());
        e.route_settings_path = Some(dir.to_string_lossy().to_string());
        e.cwd = std::env::temp_dir().to_string_lossy().to_string();
        let err = resolve_reentry_with(
            &reg(vec![e]),
            "glm",
            ReentryTransition::Attach,
            None,
            &binding_ok,
        )
        .unwrap_err();
        assert!(err.contains("no launch account"), "{err}");
    }

    #[test]
    fn reentry_plan_refuses_a_missing_route_file() {
        let mut e = row("glm");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "aaaaaaaa".into();
        e.provider = Some("zai".into());
        e.launch_account = Some("makers".into());
        e.route_settings_path =
            Some("/nonexistent/route-settings/gone.json".into());
        e.cwd = std::env::temp_dir().to_string_lossy().to_string();
        let err = resolve_reentry_with(
            &reg(vec![e]),
            "glm",
            ReentryTransition::Attach,
            None,
            &binding_ok,
        )
        .unwrap_err();
        assert!(err.contains("unreadable"), "{err}");
    }

    #[test]
    fn reentry_plan_refuses_a_floor_only_route_file() {
        let dir = std::env::temp_dir().join("reentry-test-floor.json");
        write_route(&dir, true);
        let mut e = row("glm");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "aaaaaaaa".into();
        e.launch_account = Some("makers".into());
        e.route_settings_path = Some(dir.to_string_lossy().to_string());
        e.cwd = std::env::temp_dir().to_string_lossy().to_string();
        let err = resolve_reentry_with(
            &reg(vec![e]),
            "glm",
            ReentryTransition::Attach,
            None,
            &binding_ok,
        )
        .unwrap_err();
        assert!(err.contains("scrub floor"), "{err}");
    }

    #[test]
    fn reentry_plan_refuses_an_unreachable_cwd() {
        let mut e = row("dead");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "aaaaaaaa".into();
        e.launch_account = Some("default".into());
        e.cwd = "/no/such/dir/anywhere".into();
        let err = resolve_reentry_with(
            &reg(vec![e]),
            "dead",
            ReentryTransition::Attach,
            None,
            &binding_ok,
        )
        .unwrap_err();
        assert!(err.contains("unreachable"), "{err}");
    }

    #[test]
    fn reentry_plan_refuses_a_row_with_no_session_identity() {
        let mut e = row("blank");
        e.launch_account = Some("default".into());
        let err = resolve_reentry_with(
            &reg(vec![e]),
            "blank",
            ReentryTransition::Attach,
            None,
            &binding_ok,
        )
        .unwrap_err();
        assert!(err.contains("no harness session id"), "{err}");
    }

    #[test]
    fn reentry_plan_keeps_a_proven_default_row_bare() {
        let mut e = row("plain");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "aaaaaaaa".into();
        e.launch_account = Some("default".into());
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "plain",
            ReentryTransition::Attach,
            None,
            &binding_ok,
        )
        .unwrap();
        assert_eq!(plan.argv, vec!["claude", "attach", "aaaaaaaa"]);
        assert!(plan.env.is_empty());
        assert_eq!(plan.launch_account, "default");
    }

    #[test]
    fn reentry_plan_keeps_a_legacy_default_row_on_its_historical_behavior() {
        let mut e = row("legacy");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "aaaaaaaa".into();
        // launch_account None + provider None + no route = proven legacy
        // default-Anthropic shape; the historical bare attach survives.
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "legacy",
            ReentryTransition::Attach,
            None,
            &binding_ok,
        )
        .unwrap();
        assert_eq!(plan.argv, vec!["claude", "attach", "aaaaaaaa"]);
        assert!(plan.env.is_empty());
    }

    #[test]
    fn reentry_plan_names_both_ids_and_requires_selection_to_launch() {
        let mut e = row("forked");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.related_session_id = Some("11111111-2222-3333-4444-555555555555".into());
        e.short_id = "aaaaaaaa".into();
        e.launch_account = Some("default".into());
        let r = &reg(vec![e]);

        let err = resolve_reentry_with(r, "forked", ReentryTransition::Recover, None, &binding_ok)
            .unwrap_err();
        assert!(err.contains("two valid session ids"), "{err}");

        // An unrecorded id is refused naming BOTH recorded ids.
        let err = resolve_reentry_with(
            r,
            "forked",
            ReentryTransition::Recover,
            Some("99999999-9999-9999-9999-999999999999"),
            &binding_ok,
        )
        .unwrap_err();
        assert!(err.contains("aaaaaaaa-bbbb"), "{err}");
        assert!(err.contains("11111111-2222"), "{err}");

        // Either recorded id resolves; neither replaces the other on the row.
        for id in ["aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "11111111-2222-3333-4444-555555555555"] {
            let plan = resolve_reentry_with(r, "forked", ReentryTransition::Recover, Some(id), &binding_ok)
                .unwrap();
            assert_eq!(plan.session_id, id);
        }

        // Attach needs no selection: it targets the row's own transport key.
        let plan = resolve_reentry_with(r, "forked", ReentryTransition::Attach, None, &binding_ok)
            .unwrap();
        assert_eq!(plan.session_id, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");

        // A smart resume also needs no selection: it continues the primary
        // (the canonical address delivery follows) and never demands one.
        let plan = resolve_reentry_with(r, "forked", ReentryTransition::Resume, None, &binding_ok)
            .unwrap();
        assert_eq!(plan.session_id, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    }

    #[test]
    fn reentry_plan_refuses_a_dead_account() {
        let mut e = row("orphan");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "aaaaaaaa".into();
        e.launch_account = Some("removed-acct".into());
        let err = resolve_reentry_with(
            &reg(vec![e]),
            "orphan",
            ReentryTransition::Attach,
            None,
            &binding_ok,
        )
        .unwrap_err();
        assert!(err.contains("removed-acct") && err.contains("no longer resolves"), "{err}");
    }

    #[test]
    fn reentry_plan_refuses_an_ambiguous_or_missing_row() {
        let e1 = row("dup");
        let e2 = row("dup");
        let err = resolve_reentry_with(
            &reg(vec![e1, e2]),
            "dup",
            ReentryTransition::Attach,
            None,
            &binding_ok,
        )
        .unwrap_err();
        assert!(err.contains("ambiguous"), "{err}");

        let err = resolve_reentry_with(
            &reg(vec![row("other")]),
            "ghost",
            ReentryTransition::Attach,
            None,
            &binding_ok,
        )
        .unwrap_err();
        assert!(err.contains("no registry row"), "{err}");
    }

    #[test]
    fn reentry_plan_refuses_a_non_claude_row() {
        let mut e = row("cx");
        e.harness = Some("codex".into());
        let err = resolve_reentry_with(
            &reg(vec![e]),
            "cx",
            ReentryTransition::Attach,
            None,
            &binding_ok,
        )
        .unwrap_err();
        assert!(err.contains("claude-only"), "{err}");
    }
}
