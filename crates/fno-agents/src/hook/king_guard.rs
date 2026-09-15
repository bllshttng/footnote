//! `fno-agents hook king-guard` - the PreToolUse court guard, native (x-09d2).
//!
//! Port of `hooks/king-delegation-guard.sh` policy: a session whose registry
//! row carries a crown and whose reign manifest declares shape `court` is
//! refused Edit/Write/NotebookEdit and shell writes to source. Write-path
//! allowlist, never delegation advice: a crowned session may author its plans
//! dir, crown handoff doc, escalations dir, and auto-memory; anything else
//! fail-closes. Any failure to READ (payload, registry, manifest, config)
//! allows - the never-block contract - except escalations, whose unresolved
//! resolver turns off only that carve-out.
//!
//! The one behavior change the blueprint names: a missing `fno-agents`
//! binary now allows with one stderr line (the wrapper's business), and the
//! shell's five Python CLI round trips (93% of a 9,961ms court trace) are
//! gone - the guard answers in-process.

use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::agents_config::config_lookup;

/// Registry statuses that mean the crown row no longer answers. The Python
/// `resolve_king_manifest_path` reads the same set.
const TERMINAL_STATUSES: [&str; 4] = ["exited", "orphaned", "failed", "permanent_dead"];

/// Entry: read the payload once, decide, print, always exit 0.
pub fn run(args: &[String]) -> i32 {
    let _ = args;
    let input = super::read_stdin();
    let payload: Value = serde_json::from_str(input.trim()).unwrap_or(Value::Null);
    let trace = std::env::var_os("FNO_GUARD_TRACE").is_some();
    let allow = |why: &str| -> i32 {
        if !why.is_empty() {
            eprintln!("king-delegation-guard: {why}");
        }
        super::emit_allow()
    };
    let allow_at = |stage: &str| -> i32 {
        if trace {
            eprintln!("king-delegation-guard: allow at {stage}");
        }
        super::emit_allow()
    };

    // 1. Empty or unparseable payload: not a refusal.
    if payload.is_null() {
        return allow_at("payload-null");
    }
    let tool = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let ti = payload.get("tool_input").cloned().unwrap_or(Value::Null);

    // 2. Only the four implemented tools are judged.
    if !matches!(tool, "Edit" | "Write" | "NotebookEdit" | "Bash") {
        return allow_at("tool-not-judged");
    }

    // 3. Bash first classifies writes: a call with no write target allows
    //    before any registry, manifest or config read.
    let targets: Vec<String> = if tool == "Bash" {
        let cmd = ti.get("command").and_then(Value::as_str).unwrap_or("");
        write_targets(cmd)
            .into_iter()
            .filter(|t| t != "/dev" && !t.starts_with("/dev/"))
            .collect()
        // A malformed shell never executes: no targets, nothing to judge.
    } else {
        Vec::new()
    };
    if tool == "Bash" && targets.is_empty() {
        return allow_at("bash-no-write");
    }

    // 4. Session id: payload, transcript basename, then env markers.
    let sid = resolve_sid(
        payload
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or(""),
        payload
            .get("transcript_path")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    if sid.is_empty() {
        return allow("");
    }

    // 5. Registry: the crown row.
    let rows = load_registry_rows();
    let Some(row) = rows
        .as_ref()
        .ok()
        .and_then(|r| crate::loop_reign::find_by_session(r, &sid, None))
    else {
        if let Err(e) = &rows {
            eprintln!("king-delegation-guard: registry unreadable ({e}); allowing");
        }
        return allow("");
    };
    let crown_scope = row
        .crown_scope
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    if row.crown_level.is_none() && crown_scope.is_empty() {
        return allow("");
    }

    // 6. Manifest: the reign declaration. Registry row, not file presence,
    //    proved authority; the manifest must name court for THIS session.
    let cwd = payload_cwd(&payload);
    let space = super::events_space(&cwd);
    let manifest = space.join("kings").join(format!("{crown_scope}.md"));
    if !crown_scope.is_empty()
        && (crown_scope.contains("..") || crown_scope.contains('/') || crown_scope.contains('\\'))
    {
        return allow("");
    }
    let Ok(content) = std::fs::read_to_string(&manifest) else {
        return allow("");
    };
    let Some(km) = crate::loopcheck::parse_king_manifest(&content) else {
        return allow("");
    };
    if km.shape != "court" {
        return allow("");
    }
    if km.harness_session_id.as_deref() != Some(sid.as_str()) {
        return allow("");
    }

    // 7. Mode knob: refuse (default) | warn | off.
    let mode = config_lookup(&cwd, &["king", "implementation_guard"])
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "refuse".to_string());
    if mode == "off" {
        return allow("");
    }

    // 8. Allowed roots. Plans/handoff unresolvable ALLOWS everything (the
    //    never-block contract); escalations unresolved only turns off itself.
    let plans_dir = plans_content_dir(&cwd);
    let Some(plans_dir) = plans_dir else {
        return allow("plans resolver unresolved; allowing");
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let handoff = crown_handoff_path(&cwd, home.as_deref(), crown_scope, &sid);
    let Some(handoff) = handoff else {
        return allow("handoff resolver unresolved; allowing");
    };
    let escalations = crate::escalation::dir(&cwd);

    // 9. Limb carve-outs (checked after the roots resolve, like the shell).
    let agent_id = payload
        .get("agent_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let transcript = payload
        .get("transcript_path")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !agent_id.is_empty() {
        eprintln!(
            "king-delegation-guard: limb (agent_id {agent_id}) of crowned session {sid}; allowing"
        );
        return allow("");
    }
    if is_subagent_transcript(transcript, &sid) {
        eprintln!("king-delegation-guard: limb of crowned session {sid}; allowing");
        return allow("");
    }
    if transcript_is_open_spawn(transcript) {
        eprintln!(
            "king-delegation-guard: limb of crowned session {sid} (open Task/Agent tool_use in the parent transcript); allowing"
        );
        return allow("");
    }

    // 10. Decide.
    let denied: Option<String> = match tool {
        "Edit" | "Write" | "NotebookEdit" => {
            let file = ti
                .get("file_path")
                .or_else(|| ti.get("notebook_path"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if file.is_empty() {
                None
            } else if in_plans(file, &cwd, &plans_dir)
                || real_eq(file, &cwd, &handoff)
                || in_memory(file, &cwd, home.as_deref())
                || real_prefix(file, &cwd, &escalations)
            {
                None
            } else {
                Some(file.to_string())
            }
        }
        _ => {
            // Bash: every bound target must land inside a root.
            let mut denied: Option<String> = None;
            for t in &targets {
                if real_eq(t, &cwd, &handoff)
                    || in_memory(t, &cwd, home.as_deref())
                    || real_prefix(t, &cwd, &escalations)
                    || in_plans(t, &cwd, &plans_dir)
                {
                    continue;
                }
                denied = Some(t.clone());
                break;
            }
            denied
        }
    };

    // 11. Telemetry: one row, one file, failure ignored.
    emit_telemetry(&cwd, tool, denied.is_some());

    let Some(denied) = denied else {
        return allow("");
    };
    if mode == "warn" {
        eprintln!("{}", deny_text(&denied, &plans_dir, &handoff, &escalations));
        return allow("");
    }
    let text = deny_text(&denied, &plans_dir, &handoff, &escalations);
    eprint!("{text}");
    super::emit_block(&text)
}

/// The payload's cwd, falling back to the process cwd (the hook runner seeds
/// it with the session cwd).
fn payload_cwd(payload: &Value) -> PathBuf {
    payload
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn load_registry_rows() -> Result<Vec<crate::state::RegistryEntry>, String> {
    let home = crate::paths::AgentsHome::from_env();
    let reg = crate::state::load_registry(&home.registry_json()).map_err(|e| e.to_string())?;
    Ok(reg.entries)
}

/// The two-line refusal, byte-identical to the shell's `_deny_text`.
fn deny_text(target: &str, plans: &Path, handoff: &Path, escalations: &Path) -> String {
    format!(
        "king-delegation-guard: write target '{target}' is outside the allowed roots for a crowned session.\n\
         Allowed roots: the plans directory ({plans}), the crown handoff doc ({handoff}), the escalations directory ({escalations}), and auto-memory ({home}/.claude/projects/*/memory/).\n",
        plans = plans.display(),
        handoff = handoff.display(),
        escalations = escalations.display(),
        home = std::env::var("HOME").unwrap_or_else(|_| "~".to_string()),
    )
}

/// One `guard_decision` row into the space events file, the bounded appender
/// `emit_to_both` uses, one file only (as `hooks/lib/guard-mark.sh` did).
fn emit_telemetry(cwd: &Path, tool: &str, denied: bool) {
    let path = crate::paths::events_path(cwd);
    let event = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "type": "guard_decision",
        "data": {"guard": "king-delegation-guard", "decision": if denied { "block" } else { "allow" }, "tool": tool},
        "source": "hook"
    });
    let _ = crate::claims::append_event_line(&path, &event, std::time::Duration::from_secs(2));
}

// ── Shell write classification (the tokenizer port) ──────────────────────────

/// Write targets a Bash command binds, classified the way the shell saw them:
/// redirects, tee/sponge/truncate operands, cp/mv/install destinations, dd
/// of=, in-place sed/perl files, ed/ex files. A `shlex::split` failure means
/// no targets (a malformed shell never executes).
fn write_targets(command: &str) -> Vec<String> {
    let Some(tokens) = shlex::split(command) else {
        return Vec::new();
    };
    use std::cell::RefCell;
    let fd_dup = regex::Regex::new(r"[&\d]+").unwrap();
    let redir_start = regex::Regex::new(r"^\d*&?>").unwrap();
    let inplace = regex::Regex::new(r"--in-place|-[a-zA-Z]*i[a-zA-Z.]*").unwrap();
    let bound = |t: &str| matches!(t, ";" | "|" | "&&" | "||" | "&");
    let verbs = |t: &str| {
        matches!(
            t,
            "tee"
                | "sponge"
                | "truncate"
                | "cp"
                | "mv"
                | "install"
                | "dd"
                | "sed"
                | "perl"
                | "ed"
                | "ex"
        )
    };
    let is_opt = |t: &str| t.starts_with('-');

    struct St {
        verb: Option<String>,
        pool: Vec<String>,
        nxt: bool,
        val: bool,
    }
    let st = RefCell::new(St {
        verb: None,
        pool: Vec::new(),
        nxt: false,
        val: false,
    });
    let targets: RefCell<Vec<String>> = RefCell::new(Vec::new());

    // Python's re.fullmatch on the alternation; Rust regex is leftmost-first
    // over the whole string with anchors added here.
    let fullmatch = |re: &regex::Regex, t: &str| re.find(t).map(|m| m.as_str()) == Some(t);

    let flush = || {
        let mut st = st.borrow_mut();
        let Some(verb) = st.verb.clone() else {
            return;
        };
        let files: Vec<&str> = st
            .pool
            .iter()
            .map(String::as_str)
            .filter(|t| !t.is_empty() && !is_opt(t))
            .collect();
        if matches!(verb.as_str(), "tee" | "sponge" | "truncate" | "ed" | "ex") {
            for f in files {
                targets.borrow_mut().push(f.to_string());
            }
        } else if matches!(verb.as_str(), "cp" | "mv" | "install") {
            if let Some(last) = files.last() {
                targets.borrow_mut().push((*last).to_string());
            }
        } else if verb == "dd" {
            for t in &st.pool {
                if let Some(rest) = t.strip_prefix("of=") {
                    targets.borrow_mut().push(rest.to_string());
                }
            }
        } else if verb == "sed" || verb == "perl" {
            let has_inplace = st
                .pool
                .iter()
                .filter(|t| is_opt(t))
                .any(|t| fullmatch(&inplace, t));
            if has_inplace {
                if let Some(last) = files.last() {
                    targets.borrow_mut().push(last.to_string());
                }
            }
        }
    };

    for tok in &tokens {
        if bound(tok) {
            flush();
            let mut s = st.borrow_mut();
            *s = St {
                verb: None,
                pool: Vec::new(),
                nxt: false,
                val: false,
            };
            continue;
        }
        if st.borrow().nxt {
            st.borrow_mut().nxt = false;
            let is_target = !bound(tok) && !tok.contains('>') && !fullmatch(&fd_dup, tok);
            if is_target {
                targets.borrow_mut().push(tok.clone());
            }
            continue;
        }
        if redir_start.is_match(tok) {
            // out_redirect: strip the leading fd, classify the operator.
            let rest = tok.trim_start_matches(|c: char| c.is_ascii_digit());
            let kind: Option<Option<String>> = if let Some(stripped) = rest.strip_prefix("&>") {
                if stripped.is_empty() {
                    Some(None) // &> : bare, target is next token
                } else {
                    Some(Some(stripped.to_string()))
                }
            } else if rest == ">&" {
                Some(None)
            } else {
                let body = rest
                    .trim_start_matches('&')
                    .trim_start_matches('>')
                    .trim_end_matches([';', '|', '&'])
                    .to_string();
                if matches!(rest.trim_start_matches('&'), ">" | ">>" | ">|" | ">!") {
                    Some(None)
                } else if !body.is_empty() && !fullmatch(&fd_dup, &body) {
                    Some(Some(body))
                } else {
                    None
                }
            };
            match kind {
                Some(None) => {
                    st.borrow_mut().nxt = true;
                }
                Some(Some(t)) => {
                    targets.borrow_mut().push(t);
                }
                None => {}
            }
            continue;
        }
        if st.borrow().val {
            st.borrow_mut().val = false;
            continue;
        }
        if st.borrow().verb.is_none() {
            if verbs(tok) {
                let mut s = st.borrow_mut();
                s.verb = Some(tok.clone());
                s.pool = Vec::new();
            }
            continue;
        }
        {
            let mut s = st.borrow_mut();
            s.pool.push(tok.clone());
            if matches!(tok.as_str(), "-e" | "-f" | "-i") {
                s.val = true;
            }
        }
    }
    flush();
    targets
        .into_inner()
        .into_iter()
        .filter(|t| !t.is_empty())
        .collect()
}

// ── Session identity ─────────────────────────────────────────────────────────

/// `scripts/lib/postcompact-carrier.sh` port: payload id, transcript basename
/// (without .jsonl), then harness env markers with the same-family rule.
fn resolve_sid(payload_sid: &str, transcript: &str) -> String {
    let mut sid = payload_sid.trim().to_string();
    if sid.is_empty() && !transcript.is_empty() {
        let name = Path::new(transcript)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        sid = name.strip_suffix(".jsonl").unwrap_or(&name).to_string();
    }
    if sid.is_empty() {
        let claude_root = std::env::var("CLAUDE_PLUGIN_ROOT").is_ok_and(|v| !v.is_empty());
        if std::env::var("FNO_PLATFORM").as_deref() == Ok("claude") || claude_root {
            return std::env::var("CLAUDE_CODE_SESSION_ID").unwrap_or_default();
        }
        let mut sid = String::new();
        for candidate in [
            std::env::var("CODEX_THREAD_ID").unwrap_or_default(),
            std::env::var("CLAUDE_CODE_SESSION_ID").unwrap_or_default(),
            std::env::var("CODEX_SESSION_ID").unwrap_or_default(),
        ] {
            if candidate.is_empty() {
                continue;
            }
            if sid.is_empty() {
                sid = candidate;
            } else if sid != candidate {
                return String::new();
            }
        }
        return sid;
    }
    sid
}

// ── Allowed roots ────────────────────────────────────────────────────────────

/// `cli/src/fno/paths.py` `plans_content_dir` port: `.claude/settings*.json`
/// tiers, then `plans_dir` (default `.fno/plans/` -> the space's plans dir).
/// `None` = unresolvable (the caller allows everything, never blocks).
fn plans_content_dir(cwd: &Path) -> Option<PathBuf> {
    let root = crate::paths::worktree_repo_root(cwd);
    for name in ["settings.local.json", "settings.json"] {
        let Ok(text) = std::fs::read_to_string(root.join(".claude").join(name)) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let Some(raw) = v.get("plansDirectory").and_then(Value::as_str) {
            if !raw.is_empty() {
                let p = PathBuf::from(raw);
                return Some(if p.is_absolute() { p } else { root.join(p) });
            }
        }
    }
    plans_dir(cwd)
}

/// `plans_dir` port: config `plans_dir`, default `.fno/plans/` -> the space's
/// plans dir; plain-relative values anchor at the repo root; template or
/// absolute values expand ~, {vault}, {project} (unresolvable -> None).
fn plans_dir(cwd: &Path) -> Option<PathBuf> {
    let raw = config_lookup(cwd, &["plans_dir"])
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| ".fno/plans/".to_string());
    if raw == ".fno/plans/" {
        return Some(crate::paths::space_dir(cwd).join("plans"));
    }
    let leading = raw.trim_start();
    let plain_relative = !leading.is_empty()
        && !leading.starts_with('/')
        && !leading.starts_with('~')
        && !raw.contains('$')
        && !raw.contains('{');
    if plain_relative {
        return Some(
            crate::paths::worktree_repo_root(cwd)
                .join(raw)
                .components()
                .collect::<PathBuf>(),
        );
    }
    expand_template(&raw, cwd)
}

/// `~`, `{vault}` and `{project}` expansion over the finalize.rs helpers;
/// `None` when an `{...}` token stays unresolved.
fn expand_template(raw: &str, cwd: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let project = crate::finalize::resolve_project_name(None, home.as_deref(), cwd);
    let expanded = crate::finalize::expand_handoffs_template(raw, home.as_deref(), &project)?;
    // `{vault}` reaches here only as a literal-brace token: expansion refuses
    // unknown tokens, so resolve the vault root the way Python's _resolve did.
    if expanded.to_string_lossy().contains('{') {
        let candidates = vec![
            cwd.join(".fno/config.toml"),
            home.clone()?.join(".fno/config.toml"),
        ];
        let vault = crate::finalize::resolve_obsidian_vault(&candidates)?;
        let vroot = crate::finalize::resolve_vault_root(&vault, home.as_deref())?;
        let raw = expanded
            .to_string_lossy()
            .replace("{vault}", &vroot.to_string_lossy());
        if raw.contains('{') {
            return None;
        }
        return Some(PathBuf::from(raw));
    }
    Some(expanded)
}

/// The crown handoff doc path (paths_cli.py `handoff` port): the scope form
/// names `crown-<sanitized scope>` and takes the NEWEST existing `*-<key>.md`
/// in the handoffs dir; the session form is today's `<YYYYMMDD>-<first 8 of
/// the sid>.md`. Unresolvable dir -> None (the caller allows).
fn crown_handoff_path(cwd: &Path, home: Option<&Path>, scope: &str, sid: &str) -> Option<PathBuf> {
    let dir = crate::finalize::resolve_handoffs_dir(None, None, cwd, home);
    if !scope.is_empty() {
        let key = format!("crown-{}", crate::king_checkin::sanitize_scope_key(scope));
        let mut newest: Option<(PathBuf, std::time::SystemTime)> = None;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
                let Some(name) = name else { continue };
                if name.starts_with('.') {
                    continue;
                }
                let Some(stem) = name.strip_suffix(".md") else {
                    continue;
                };
                // The glob `*-<key>.md` takes ANY prefix, empty included; a
                // bare `sibling<key>.md` without the separator must not.
                if !stem
                    .strip_suffix(&key)
                    .is_none_or(|head| head.ends_with('-') || head.is_empty())
                {
                    continue;
                }
                let mtime = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                if newest.as_ref().is_none_or(|(_, best)| mtime > *best) {
                    newest = Some((p, mtime));
                }
            }
        }
        return Some(newest.map(|(p, _)| p).unwrap_or_else(|| {
            let today = chrono::Local::now().format("%Y%m%d");
            dir.join(format!("{today}-{key}.md"))
        }));
    }
    let key: String = sid.chars().take(8).collect();
    let today = chrono::Local::now().format("%Y%m%d");
    Some(dir.join(format!("{today}-{key}.md")))
}

// ── Containment ──────────────────────────────────────────────────────────────

/// Lexical normalization (Python's os.path.normpath): collapse //, . and ..
/// without touching the filesystem.
fn normpath(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn abs_norm(p: &str, cwd: &Path) -> PathBuf {
    let path = PathBuf::from(p);
    let joined = if path.is_absolute() {
        path
    } else {
        let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        cwd.join(path)
    };
    normpath(&joined)
}

/// realpath with Python's not-yet-existing-file semantics: canonicalize the
/// deepest EXISTING prefix (so a /var -> /private/var symlink on the root
/// resolves the same way for a file that does not exist yet), then append the
/// non-existent tail.
fn real_of(p: &str, cwd: &Path) -> PathBuf {
    let path = PathBuf::from(p);
    let joined = if path.is_absolute() {
        path
    } else {
        let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        cwd.join(path)
    };
    if let Ok(real) = std::fs::canonicalize(&joined) {
        return real;
    }
    let mut prefix = joined.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match prefix.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => {
                tail.push(
                    prefix
                        .file_name()
                        .map(std::borrow::ToOwned::to_owned)
                        .unwrap_or_default(),
                );
                prefix = parent.to_path_buf();
                if let Ok(real) = std::fs::canonicalize(&prefix) {
                    let mut real = real;
                    for part in tail.iter().rev() {
                        real.push(part);
                    }
                    return real;
                }
            }
            _ => break,
        }
    }
    normpath(&joined)
}

/// Plans containment (normpath, not realpath - the shell compared normpaths).
fn in_plans(p: &str, cwd: &Path, plans: &Path) -> bool {
    let p = abs_norm(p, cwd);
    let d = normpath(plans);
    p == d || p.starts_with(&d)
}

/// Handoff containment: realpath equality (the vault symlink must not split
/// the two spellings).
fn real_eq(p: &str, cwd: &Path, target: &Path) -> bool {
    real_of(p, cwd) == real_of(&target.to_string_lossy(), cwd)
}

/// Escalations containment: realpath prefix.
fn real_prefix(p: &str, cwd: &Path, root: &Path) -> bool {
    let p = real_of(p, cwd);
    let root = real_of(&root.to_string_lossy(), cwd);
    p == root || p.starts_with(&root)
}

/// Memory carve-out: exactly `$HOME/.claude/projects/<project>/memory/**`.
fn in_memory(p: &str, cwd: &Path, home: Option<&Path>) -> bool {
    let Some(home) = home else {
        return false;
    };
    let root = home.join(".claude").join("projects");
    let p = real_of(p, cwd);
    let root = real_of(&root.to_string_lossy(), cwd);
    if p == root || !p.starts_with(&root) {
        return false;
    }
    let rel = p.strip_prefix(&root).unwrap();
    let mut comps = rel.components();
    let _project = comps.next();
    comps.next().map(|c| c.as_os_str() == "memory") == Some(true)
}

// ── Limb signatures ──────────────────────────────────────────────────────────

fn is_subagent_transcript(transcript: &str, sid: &str) -> bool {
    if transcript.is_empty() {
        return false;
    }
    let path = Path::new(transcript);
    let Some(parent) = path.parent() else {
        return false;
    };
    if parent.file_name().map(|n| n == "subagents") != Some(true) {
        return false;
    }
    parent
        .parent()
        .map(|g| g.file_name().map(|n| n == sid).unwrap_or(false))
        .unwrap_or(false)
}

/// The sync-limb shape: the transcript's newest tool_use is Task/Agent with no
/// tool_result yet. Tail-only (the open entry sits at the end of a live
/// transcript); unreadable falls through fail-closed.
fn transcript_is_open_spawn(transcript: &str) -> bool {
    if transcript.is_empty() {
        return false;
    }
    let Ok(meta) = std::fs::metadata(transcript) else {
        return false;
    };
    let size = meta.len() as usize;
    let start = size.saturating_sub(262_144);
    let Ok(file) = std::fs::File::open(transcript) else {
        return false;
    };
    use std::io::{Read as _, Seek, SeekFrom};
    let mut file = file;
    if file.seek(SeekFrom::Start(start as u64)).is_err() {
        return false;
    }
    let mut buf = String::new();
    if file.read_to_string(&mut buf).is_err() {
        return false;
    }
    let mut open_spawn: Option<String> = None;
    let mut done: std::collections::HashSet<String> = Default::default();
    for line in buf.lines() {
        let Ok(e) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let Some(content) = e
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for c in content {
            let Some(c) = c.as_object() else {
                continue;
            };
            match c.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    let name = c.get("name").and_then(Value::as_str).unwrap_or("");
                    if name == "Task" || name == "Agent" {
                        open_spawn = c.get("id").and_then(Value::as_str).map(str::to_string);
                    } else {
                        open_spawn = None;
                    }
                }
                Some("tool_result") => {
                    if let Some(id) = c.get("tool_use_id").and_then(Value::as_str) {
                        done.insert(id.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    open_spawn.is_some_and(|id| !done.contains(&id))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(cmd: &str) -> Vec<String> {
        write_targets(cmd)
    }

    #[test]
    fn redirects_bind_targets() {
        assert_eq!(targets("echo hi > /tmp/a"), vec!["/tmp/a"]);
        assert_eq!(targets("echo hi >> /tmp/a"), vec!["/tmp/a"]);
        assert_eq!(targets("cmd 2> /tmp/err"), vec!["/tmp/err"]);
        assert_eq!(targets("cmd &> /tmp/both"), vec!["/tmp/both"]);
        assert_eq!(targets("cmd &>/tmp/both"), vec!["/tmp/both"]);
        assert_eq!(targets("cmd >& /tmp/both"), vec!["/tmp/both"]);
        assert_eq!(targets("cmd >| /tmp/a"), vec!["/tmp/a"]);
        assert_eq!(targets("cmd >! /tmp/a"), vec!["/tmp/a"]);
        assert_eq!(targets("cmd 2>&1; echo x > /tmp/a"), vec!["/tmp/a"]);
        assert!(targets("echo hi").is_empty(), "no redirect binds nothing");
        assert!(
            targets("cmd 2>&1 | tee").is_empty(),
            "fd dup is not a write"
        );
        // Quoted "a > b" is one word, not an operator.
        assert!(targets("echo \"a > b\"").is_empty());
    }

    #[test]
    fn verb_operands_bind() {
        // The shell policy took EVERY tee operand as a write, including stdin
        // redirection words (`<`, the file): over-broad by design, fail-closed.
        assert_eq!(
            targets("tee /tmp/x /tmp/y < /tmp/in"),
            vec!["/tmp/x", "/tmp/y", "<", "/tmp/in"]
        );
        // `-s`'s value is treated as a file too (the shell policy never
        // modelled option values; over-broad, fail-closed).
        assert_eq!(targets("truncate -s 0 /tmp/x"), vec!["0", "/tmp/x"]);
        assert_eq!(
            targets("cp /tmp/a /tmp/b"),
            vec!["/tmp/b"],
            "only the destination writes"
        );
        assert_eq!(targets("mv a b && mv c d"), vec!["b", "d"]);
        assert_eq!(targets("dd if=/x of=/tmp/y"), vec!["/tmp/y"]);
        assert_eq!(targets("sed -i 's/a/b/' /tmp/f"), vec!["/tmp/f"]);
        assert_eq!(
            targets("sed 's/a/b/' /tmp/f"),
            Vec::<String>::new(),
            "no -i, no write"
        );
        assert_eq!(targets("perl -i -pe s/a/b/ /tmp/f"), vec!["/tmp/f"]);
        assert_eq!(targets("printf x | sponge /tmp/f"), vec!["/tmp/f"]);
        assert_eq!(targets("printf x | ed /tmp/f"), vec!["/tmp/f"]);
        assert_eq!(targets("printf x | ex /tmp/f"), vec!["/tmp/f"]);
    }

    #[test]
    fn malformed_shell_never_judged() {
        assert!(targets("echo 'unterminated").is_empty());
    }

    #[test]
    fn sid_resolves_payload_first_and_env_agreement() {
        assert_eq!(resolve_sid("  abc  ", "/x/def.jsonl"), "abc");
        assert_eq!(resolve_sid("", "/x/def.jsonl"), "def");
        assert_eq!(resolve_sid("", "/x/def"), "def");
    }

    #[test]
    fn normpath_handles_dots() {
        assert_eq!(
            normpath(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
    }

    #[test]
    fn subagent_layout_detected() {
        assert!(is_subagent_transcript(
            "/base/123/subagents/agent-1.jsonl",
            "123"
        ));
        assert!(!is_subagent_transcript("/base/123/main.jsonl", "123"));
        assert!(!is_subagent_transcript(
            "/base/456/subagents/a.jsonl",
            "123"
        ));
    }

    #[test]
    fn scope_traversal_never_names_a_manifest() {
        for scope in ["../x", "a/b", "a\\b", ""] {
            let refused = scope.is_empty()
                || scope.contains("..")
                || scope.contains('/')
                || scope.contains('\\');
            assert!(refused, "scope {scope:?} must refuse");
        }
    }

    #[test]
    fn court_manifest_with_identity_parses() {
        let content =
            "---\nfno_id: 20260915T190000Z-kg1-abcdef\nscope: fno\nshape: court\nharness_session_id: sess-king\n---\n";
        let km = crate::loopcheck::parse_king_manifest(content).expect("parses");
        assert_eq!(km.shape, "court");
        assert_eq!(km.harness_session_id.as_deref(), Some("sess-king"));
    }
}
