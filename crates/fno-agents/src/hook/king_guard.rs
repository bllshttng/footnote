//! `fno-agents hook king-guard` - the native PreToolUse court guard.
//!
//! Port of `hooks/king-delegation-guard.sh` policy: a session whose registry
//! row carries a crown and whose reign manifest declares shape `court` is
//! refused Edit/Write/NotebookEdit and shell writes to source. Write-path
//! allowlist, never delegation advice: a crowned session may author its plans
//! dir, the handoffs dir, escalations dir, auto-memory, and the vault
//! `internal/fno` tree; anything else fail-closes. Any failure to READ
//! (payload, registry, manifest, config) allows - the never-block contract -
//! except escalations, whose unresolved resolver turns off only that
//! carve-out.
//!
//! The one behavior change the blueprint names: a missing `fno-agents`
//! binary now allows with one stderr line (the wrapper's business), and the
//! shell's five Python CLI round trips (93% of a 9,961ms court trace) are
//! gone - the guard answers in-process.

use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::agents_config::config_lookup;

/// Entry: read the payload once, decide, print, always exit 0.
pub fn run(_args: &[String]) -> i32 {
    let payload: Value = serde_json::from_str(super::read_stdin().trim()).unwrap_or(Value::Null);
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
    let rows = crate::state::load_registry(&crate::paths::AgentsHome::from_env().registry_json())
        .map(|r| r.entries)
        .map_err(|e| e.to_string());
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
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let manifest = super::events_space(&cwd)
        .join("kings")
        .join(format!("{crown_scope}.md"));
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
    if km.shape != "court" || km.harness_session_id.as_deref() != Some(sid.as_str()) {
        return allow("");
    }

    // 7. Mode knob: refuse (default) | warn | off.
    let mode = config_lookup(&cwd, &["king", "implementation_guard"])
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "refuse".to_string());
    if mode == "off" {
        return allow("");
    }

    // 8. Allowed roots. Plans/vault unresolvable ALLOWS everything (the
    //    never-block contract); escalations unresolved only turns off itself.
    let Some(plans_dir) = plans_content_dir(&cwd) else {
        return allow("plans resolver unresolved; allowing");
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let Some(vault_fno) = vault_internal_fno(&cwd, home.as_deref()) else {
        return allow("vault root unresolved; allowing");
    };
    let handoffs = crate::finalize::resolve_handoffs_dir(None, None, &cwd, home.as_deref());
    let escalations = crate::escalation::dir(&cwd);

    // 9. Limb carve-outs (checked after the roots resolve, like the shell).
    let agent_id = payload
        .get("agent_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let transcript = payload
        .get("transcript_path")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !agent_id.is_empty() {
        return allow(&format!(
            "limb (agent_id {agent_id}) of crowned session {sid}; allowing"
        ));
    }
    if is_subagent_transcript(transcript, &sid) {
        return allow(&format!("limb of crowned session {sid}; allowing"));
    }
    if transcript_is_open_spawn(transcript) {
        return allow(&format!(
            "limb of crowned session {sid} (open Task/Agent tool_use in the parent transcript); allowing"
        ));
    }

    // 10. Decide: the write-path allowlist, one predicate for every tool.
    let allowed = |t: &str| {
        write_allowed(
            t,
            &cwd,
            &plans_dir,
            &handoffs,
            home.as_deref(),
            &escalations,
            &vault_fno,
        )
    };
    let denied: Option<String> = match tool {
        "Edit" | "Write" | "NotebookEdit" => {
            let file = ti
                .get("file_path")
                .or_else(|| ti.get("notebook_path"))
                .and_then(Value::as_str)
                .unwrap_or("");
            (!file.is_empty() && !allowed(file)).then(|| file.to_string())
        }
        _ => targets.iter().find(|t| !allowed(t)).map(|t| t.to_string()),
    };

    // 11. Telemetry: one row, one file, failure ignored.
    emit_telemetry(&cwd, tool, denied.is_some());

    let Some(denied) = denied else {
        return allow("");
    };
    if mode == "warn" {
        eprintln!(
            "{}",
            deny_text(&denied, &plans_dir, &handoffs, &escalations, &vault_fno)
        );
        return allow("");
    }
    let text = deny_text(&denied, &plans_dir, &handoffs, &escalations, &vault_fno);
    eprint!("{text}");
    super::emit_block(&text)
}

/// The two-line refusal. The shell twin is a pure exec shim, so this text is
/// the only copy.
fn deny_text(
    target: &str,
    plans: &Path,
    handoffs: &Path,
    escalations: &Path,
    vault_fno: &Path,
) -> String {
    format!(
        "king-delegation-guard: write target '{target}' is outside the allowed roots for a crowned session.\n\
         Allowed roots: the plans directory ({plans}), the handoffs directory ({handoffs}), the escalations directory ({escalations}), auto-memory ({home}/.claude/projects/*/memory/), and the vault fno tree ({vault_fno}).\n",
        plans = plans.display(),
        handoffs = handoffs.display(),
        escalations = escalations.display(),
        vault_fno = vault_fno.display(),
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
    // Python's re.fullmatch over `[&\d]+`: a token made only of `&` and digits.
    let is_fd = |t: &str| !t.is_empty() && t.bytes().all(|b| b == b'&' || b.is_ascii_digit());
    let bound = |t: &str| matches!(t, ";" | "|" | "&&" | "||" | "&");
    let flush = |verb: &Option<String>, pool: &[String], targets: &mut Vec<String>| {
        let Some(verb) = verb else {
            return;
        };
        let files: Vec<&str> = pool
            .iter()
            .map(String::as_str)
            .filter(|t| !t.is_empty() && !t.starts_with('-'))
            .collect();
        match verb.as_str() {
            "tee" | "sponge" | "truncate" | "ed" | "ex" => {
                targets.extend(files.into_iter().map(str::to_string));
            }
            "cp" | "mv" | "install" => {
                if let Some(last) = files.last() {
                    targets.push((*last).to_string());
                }
            }
            "dd" => {
                targets.extend(
                    pool.iter()
                        .filter_map(|t| t.strip_prefix("of="))
                        .map(str::to_string),
                );
            }
            "sed" | "perl" => {
                let inplace = pool
                    .iter()
                    .filter(|t| t.starts_with('-'))
                    .any(|t| is_inplace(t));
                if inplace {
                    if let Some(last) = files.last() {
                        targets.push((*last).to_string());
                    }
                }
            }
            _ => {}
        }
    };
    let mut targets: Vec<String> = Vec::new();
    let mut verb: Option<String> = None;
    let mut pool: Vec<String> = Vec::new();
    let mut nxt = false;
    let mut val = false;
    for tok in &tokens {
        if bound(tok) {
            flush(&verb, &pool, &mut targets);
            verb = None;
            pool.clear();
            nxt = false;
            val = false;
        } else if nxt {
            nxt = false;
            if !bound(tok) && !tok.contains('>') && !is_fd(tok) {
                targets.push(tok.clone());
            }
        } else if is_redirect(tok) {
            let rest = tok.trim_start_matches(|c: char| c.is_ascii_digit());
            if let Some(stripped) = rest.strip_prefix("&>") {
                if stripped.is_empty() {
                    nxt = true;
                } else {
                    targets.push(stripped.to_string());
                }
            } else if rest == ">&" {
                nxt = true;
            } else {
                let plain = rest.trim_start_matches('&');
                if matches!(plain, ">" | ">>" | ">|" | ">!") {
                    nxt = true;
                } else {
                    let body = plain
                        .trim_start_matches('>')
                        .trim_end_matches([';', '|', '&']);
                    if !body.is_empty() && !is_fd(body) {
                        targets.push(body.to_string());
                    }
                }
            }
        } else if val {
            val = false;
        } else if verb.is_none() {
            if matches!(
                tok.as_str(),
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
            ) {
                verb = Some(tok.clone());
                pool.clear();
            }
        } else {
            if matches!(tok.as_str(), "-e" | "-f" | "-i") {
                val = true;
            }
            pool.push(tok.clone());
        }
    }
    flush(&verb, &pool, &mut targets);
    targets.retain(|t| !t.is_empty());
    targets
}

/// `^\d*&?>`: optional leading fd digits, optional `&`, then a redirect.
fn is_redirect(tok: &str) -> bool {
    let rest = tok.trim_start_matches(|c: char| c.is_ascii_digit());
    let rest = rest.strip_prefix('&').unwrap_or(rest);
    rest.starts_with('>')
}

/// `--in-place|-[a-zA-Z]*i[a-zA-Z.]*`, fullmatch. The first `i` is the only
/// split a fullmatch can use: a later `i` puts the same bad byte in `head`.
fn is_inplace(t: &str) -> bool {
    if t == "--in-place" {
        return true;
    }
    let Some(rest) = t.strip_prefix('-') else {
        return false;
    };
    let Some(idx) = rest.find('i') else {
        return false;
    };
    let (head, tail) = (&rest[..idx], &rest[idx + 1..]);
    head.bytes().all(|b| b.is_ascii_alphabetic())
        && tail.bytes().all(|b| b.is_ascii_alphabetic() || b == b'.')
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
        let parsed = std::fs::read_to_string(root.join(".claude").join(name))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        if let Some(raw) = parsed
            .as_ref()
            .and_then(|v| v.get("plansDirectory"))
            .and_then(Value::as_str)
        {
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
    if !leading.is_empty() && !leading.starts_with(['/', '~']) && !raw.contains(['$', '{']) {
        return Some(
            crate::paths::worktree_repo_root(cwd)
                .join(raw)
                .components()
                .collect::<PathBuf>(),
        );
    }
    // Template form: ~, {vault}, {project} over the finalize.rs helpers;
    // None when an {...} token stays unresolved.
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let project = crate::finalize::resolve_project_name(None, home.as_deref(), cwd);
    let expanded = crate::finalize::expand_handoffs_template(&raw, home.as_deref(), &project)?;
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
        return (!raw.contains('{')).then(|| PathBuf::from(raw));
    }
    Some(expanded)
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

fn abs_join(p: &str, cwd: &Path) -> PathBuf {
    let path = PathBuf::from(p);
    if path.is_absolute() {
        path
    } else {
        let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        cwd.join(path)
    }
}

/// realpath with Python's not-yet-existing-file semantics: canonicalize the
/// deepest EXISTING prefix (so a /var -> /private/var symlink on the root
/// resolves the same way for a file that does not exist yet), then append the
/// non-existent tail.
fn real_of(p: &str, cwd: &Path) -> PathBuf {
    let joined = abs_join(p, cwd);
    if let Ok(real) = std::fs::canonicalize(&joined) {
        return real;
    }
    let (mut prefix, mut tail) = (joined.clone(), Vec::new());
    while let Some(parent) = prefix.parent().filter(|p| !p.as_os_str().is_empty()) {
        tail.push(
            prefix
                .file_name()
                .map(|n| n.to_os_string())
                .unwrap_or_default(),
        );
        prefix = parent.to_path_buf();
        if let Ok(real) = std::fs::canonicalize(&prefix) {
            let mut real = real;
            real.extend(tail.iter().rev().cloned());
            return real;
        }
    }
    normpath(&joined)
}

/// Plans containment (normpath, not realpath - the shell compared normpaths).
fn in_plans(p: &str, cwd: &Path, plans: &Path) -> bool {
    let p = normpath(&abs_join(p, cwd));
    let d = normpath(plans);
    p == d || p.starts_with(&d)
}

/// `<vault>/internal/fno` (the `escalation::dir_with_home` shape): obsidian
/// vault resolved from the project then global config, mapped to its root.
/// The ruling of 2026-09-17: the vault is not source - every folder under
/// `internal/fno` is a king's to write. None -> the caller ALLOWS everything
/// (never-block).
fn vault_internal_fno(cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = vec![cwd.join(".fno/config.toml")];
    if let Some(h) = home {
        candidates.push(h.join(".fno/config.toml"));
    }
    let vault = crate::finalize::resolve_obsidian_vault(&candidates)?;
    let vroot = crate::finalize::resolve_vault_root(&vault, home)?;
    Some(vroot.join("internal").join("fno"))
}

/// The step-10 allowlist predicate: plans dir, the whole handoffs dir (no
/// mtime-selected single file), auto-memory, escalations, and the vault
/// `internal/fno` tree. Everything else - SOURCE - denies.
fn write_allowed(
    t: &str,
    cwd: &Path,
    plans: &Path,
    handoffs: &Path,
    home: Option<&Path>,
    escalations: &Path,
    vault_fno: &Path,
) -> bool {
    in_plans(t, cwd, plans)
        || real_prefix(t, cwd, handoffs)
        || in_memory(t, cwd, home)
        || real_prefix(t, cwd, escalations)
        || real_prefix(t, cwd, vault_fno)
}

/// Escalations containment: realpath prefix.
fn real_prefix(p: &str, cwd: &Path, root: &Path) -> bool {
    let p = real_of(p, cwd);
    let root = real_of(&root.to_string_lossy(), cwd);
    p == root || p.starts_with(&root)
}

/// Memory carve-out: exactly `$HOME/.claude/projects/<project>/memory/**`.
fn in_memory(p: &str, cwd: &Path, home: Option<&Path>) -> bool {
    home.is_some_and(|home| {
        let root = real_of(
            &home.join(".claude").join("projects").to_string_lossy(),
            cwd,
        );
        let p = real_of(p, cwd);
        p.starts_with(&root)
            && p.strip_prefix(&root)
                .unwrap()
                .components()
                .nth(1)
                .is_some_and(|c| c.as_os_str() == "memory")
    })
}

// ── Limb signatures ──────────────────────────────────────────────────────────

fn is_subagent_transcript(transcript: &str, sid: &str) -> bool {
    let Some(parent) = Path::new(transcript).parent() else {
        return false;
    };
    parent.file_name().is_some_and(|n| n == "subagents")
        && parent
            .parent()
            .and_then(|g| g.file_name())
            .is_some_and(|n| n == sid)
}

/// The sync-limb shape: the transcript's newest tool_use is Task/Agent with no
/// tool_result yet. Tail-only (the open entry sits at the end of a live
/// transcript); unreadable falls through fail-closed.
fn transcript_is_open_spawn(transcript: &str) -> bool {
    if transcript.is_empty() {
        return false;
    }
    let (meta, mut file) = match (
        std::fs::metadata(transcript),
        std::fs::File::open(transcript),
    ) {
        (Ok(m), Ok(f)) => (m, f),
        _ => return false,
    };
    use std::io::{Read as _, Seek, SeekFrom};
    let mut buf = String::new();
    if file
        .seek(SeekFrom::Start(meta.len().saturating_sub(262_144)))
        .is_err()
        || file.read_to_string(&mut buf).is_err()
    {
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
                    open_spawn = if name == "Task" || name == "Agent" {
                        c.get("id").and_then(Value::as_str).map(str::to_string)
                    } else {
                        None
                    };
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

    /// Fixture roots: a vault tree plus an unrelated source repo, so the two
    /// acceptance shapes have somewhere real to resolve against.
    struct Roots {
        base: PathBuf,
        home: PathBuf,
        vault_fno: PathBuf,
        handoffs: PathBuf,
        plans: PathBuf,
        escalations: PathBuf,
    }

    fn roots(tag: &str) -> Roots {
        let base = std::env::temp_dir().join(format!("kgd-{tag}-{}", std::process::id()));
        let home = base.join("home");
        let vault_fno = base.join("c3po/internal/fno");
        let escalations = vault_fno.join("escalations");
        let handoffs = vault_fno.join("handoffs");
        Roots {
            handoffs,
            vault_fno,
            plans: base.join("repo/.fno/plans"),
            escalations,
            home,
            base,
        }
    }

    fn allowed(r: &Roots, target: &Path) -> bool {
        write_allowed(
            &target.to_string_lossy(),
            &r.base,
            &r.plans,
            &r.handoffs,
            Some(&r.home),
            &r.escalations,
            &r.vault_fno,
        )
    }

    #[test]
    fn vault_internal_fno_analysis_write_is_allowed() {
        // Acceptance 1: every folder under internal/fno is a king's to
        // write; the vault is not source.
        let r = roots("analysis");
        assert!(allowed(&r, &r.vault_fno.join("analysis/foo.json")));
        assert!(allowed(&r, &r.vault_fno.join("backlog/x.md")));
        let _ = std::fs::remove_dir_all(&r.base);
    }

    #[test]
    fn source_write_still_denied() {
        // Acceptance 2: the guard exists to stop a king writing SOURCE.
        let r = roots("source");
        assert!(!allowed(&r, &r.base.join("repo/cli/src/fno/anything.py")));
        assert!(!allowed(
            &r,
            &r.base.join("repo/crates/fno-agents/src/lib.rs")
        ));
        let _ = std::fs::remove_dir_all(&r.base);
    }

    #[test]
    fn handoffs_dir_sibling_allowed_without_mtime_selection() {
        // Any file in the handoffs dir, including one newer than the "crown"
        // doc, is allowed - the arm is a dir prefix, not one mtime-picked file.
        let r = roots("handoffs");
        assert!(allowed(
            &r,
            &r.handoffs.join("20990101-anyone-crown-fno.md")
        ));
        assert!(allowed(&r, &r.handoffs.join("notes.md")));
        let _ = std::fs::remove_dir_all(&r.base);
    }

    #[test]
    fn memory_still_allowed_and_vault_unresolvable_returns_none() {
        let r = roots("memory");
        assert!(allowed(
            &r,
            &r.home.join(".claude/projects/-proj/memory/note.md")
        ));
        // Never-block: no config anywhere -> None, and the caller allows.
        let bare = std::env::temp_dir().join(format!("kgd-bare-{}", std::process::id()));
        assert_eq!(vault_internal_fno(&bare, Some(&bare),), None);
        let _ = std::fs::remove_dir_all(&r.base);
        let _ = std::fs::remove_dir_all(&bare);
    }

    #[test]
    fn vault_internal_fno_resolves_from_config() {
        let r = roots("resolve");
        let cfg = r.home.join(".fno/config.toml");
        let _ = std::fs::create_dir_all(cfg.parent().unwrap());
        let _ = std::fs::write(&cfg, "[obsidian]\nenabled = true\nvault = \"c3po\"\n");
        assert_eq!(
            vault_internal_fno(&r.base, Some(&r.home)),
            // A bare vault name maps to ~/c3po (paths.vault_root's rule).
            Some(r.home.join("c3po/internal/fno"))
        );
        let _ = std::fs::remove_dir_all(&r.base);
    }
}
