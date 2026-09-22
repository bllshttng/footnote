//! `fno-agents hook king-guard` - the native PreToolUse court guard.
//!
//! Port of `hooks/king-delegation-guard.sh` policy: a session whose registry
//! row carries a crown and whose reign manifest declares shape `court` is
//! refused Edit/Write/NotebookEdit and shell writes to source. Inverted
//! predicate, never delegation advice: SOURCE is any path realpath-inside
//! the repo root, except the repo's `.fno` state tree, build output, and
//! the `king.write_roots` entries the operator lists; everything else - the
//! vault wherever it lives, memory wherever the harness keeps it - allows.
//! Any failure to READ (payload, registry, manifest, config) allows - the
//! never-block contract.
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

    // 8. The repo root is the only DENY region. Realpath containment has no
    //    unresolvable state, so the never-block contract needs no escape
    //    hatch here: outside the repo allows, whatever it is.
    let repo_root = crate::paths::worktree_repo_root(&cwd);
    let roots = write_roots(config_lookup(&cwd, &["king", "write_roots"]), &repo_root);

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

    // 10. Decide: deny SOURCE, allow everything else, one predicate for
    //     every tool.
    let allowed = |t: &str| !write_denied(t, &cwd, &repo_root, &roots);
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
    super::emit_guard_decision(&cwd, "king-delegation-guard", tool, denied.is_some());

    let Some(denied) = denied else {
        return allow("");
    };
    if mode == "warn" {
        eprintln!("{}", deny_text(&denied, &repo_root));
        return allow("");
    }
    let text = deny_text(&denied, &repo_root);
    eprint!("{text}");
    super::emit_block(&text)
}

/// The two-line refusal. The shell twin is a pure exec shim, so this text is
/// the only copy of the rule it enforces.
fn deny_text(target: &str, repo_root: &Path) -> String {
    format!(
        "king-delegation-guard: write target '{target}' is inside the repo ({repo}), and a crowned session does not write SOURCE.\n\
         A king operates the machine and does not author it: deploy and repair verbs (fno config plugin install, fno doctor update) run, build output and everything outside the repo allow, repo source does not. Delegate the edit or escalate. An operator can list an in-repo path in config.king.write_roots.\n",
        repo = repo_root.display(),
    )
}

// ── Shell write classification (the tokenizer port) ──────────────────────────

/// Write targets a Bash command binds: redirects, tee/sponge/truncate
/// operands, cp/mv/install destinations, dd of=, in-place sed/perl files,
/// ed/ex files. A `lex` failure (unterminated quote) means no targets - a
/// malformed shell never executes.
fn write_targets(command: &str) -> Vec<String> {
    let Some(tokens) = lex(command) else {
        return Vec::new();
    };
    // Python's re.fullmatch over `[&\d]+`: a token made only of `&` and digits.
    let is_fd = |t: &str| !t.is_empty() && t.bytes().all(|b| b == b'&' || b.is_ascii_digit());
    let bound = |t: &str| {
        matches!(t, ";" | ";;" | "|" | "||" | "&&" | "&" | "(" | ")" | "\n") || t.starts_with('<')
    };
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
    let mut at_command = true;
    for tok in &tokens {
        if bound(tok) {
            flush(&verb, &pool, &mut targets);
            verb = None;
            pool.clear();
            nxt = false;
            val = false;
            at_command = true;
        } else if nxt {
            nxt = false;
            if !tok.contains('>') && !is_fd(tok) {
                targets.push(tok.clone());
            }
        } else if is_redirect(tok) {
            // `lex` emits redirects as their own tokens (`2>`, `>`, `>>`,
            // `>&`, `&>`, `>|`, `>`!), so every shape here just arms `nxt`
            // and the next word is judged as the redirect target.
            let rest = tok.trim_start_matches(|c: char| c.is_ascii_digit());
            let plain = rest.strip_prefix('&').unwrap_or(rest);
            if rest == "&>" || matches!(plain, ">" | ">>" | ">|" | ">!" | ">&") {
                nxt = true;
            }
        } else if val {
            val = false;
        } else if at_command {
            // The verb table only applies in command position, so `install`
            // in `fno config plugin install claude` stays a positional word.
            // An assignment (`B=/path mv a b`) and a wrapper word (sudo, env,
            // command, nohup, time) hand command position to the next word.
            if !is_assignment(tok)
                && !matches!(tok.as_str(), "sudo" | "env" | "command" | "nohup" | "time")
            {
                at_command = false;
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
            }
        } else if verb.is_some() {
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

/// Lex a command into words and operator tokens; `None` on an unterminated
/// quote or an unclosed substitution (a malformed shell never executes).
/// Unlike `shlex::split`, unquoted `;` `|` `&` `(` `)` newline and redirects
/// arrive as their own tokens, so `2>&1|tail` can never read as one word.
///
/// `<<DELIM`/`<<-DELIM` heredoc bodies are swallowed whole, never re-lexed as
/// more shell text - unless the reading command is a shell (bash/sh/zsh), in
/// which case the body is lexed again and its tokens spliced in, so `bash
/// <<EOF` still judges a real write in its body, but a heredoc mailed as a
/// file body never donates a phantom write target.
///
/// Each command substitution - `$( )` and backticks, quoted or not - stays
/// one word in place (so `mv $(pick_build x) /tmp/out` still binds
/// `/tmp/out`) and its body is lexed again, appended after a `\n` token so
/// callers read it as one more command.
pub(super) fn lex(command: &str) -> Option<Vec<String>> {
    let mut bodies: Vec<Vec<String>> = Vec::new();
    let mut toks = lex_until(&mut command.chars().peekable(), false, &mut bodies)?;
    for body in bodies {
        toks.push("\n".to_string());
        toks.extend(body);
    }
    Some(toks)
}

type Src<'a> = std::iter::Peekable<std::str::Chars<'a>>;

/// Entered just past a glued `(`: the group lexes by the same rules up to
/// its matching `)`. A `$( )` body is recorded as a command; `$(( ))`
/// arithmetic and a bare group such as `arr=(a b)` stay words only.
fn group(chars: &mut Src, bodies: &mut Vec<Vec<String>>, command: bool) -> Option<String> {
    let arith = chars.peek() == Some(&'(');
    let body = lex_until(chars, true, bodies)?;
    let word = format!("({})", body.join(" "));
    if command && !arith {
        bodies.push(body);
    }
    Some(word)
}

/// Entered just past an opening backtick: raw text to the next unescaped
/// backtick, lexed again as its own command.
fn backtick(chars: &mut Src, bodies: &mut Vec<Vec<String>>) -> Option<String> {
    let mut text = String::new();
    loop {
        match chars.next()? {
            '`' => break,
            '\\' if matches!(chars.peek(), Some('`' | '\\' | '$')) => text.push(chars.next()?),
            c => text.push(c),
        }
    }
    bodies.push(lex(&text)?);
    Some(format!("`{text}`"))
}

/// The lexer worker: tokens of one command, stopping at the matching `)`
/// when entered inside a substitution. Operator parens opened here nest
/// through `depth`, so a `case` pattern or arithmetic never ends a
/// substitution body early.
fn lex_until(
    chars: &mut Src,
    in_subst: bool,
    bodies: &mut Vec<Vec<String>>,
) -> Option<Vec<String>> {
    let mut toks: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    // Set right after a `<<`/`<<-` token: (strip_tabs, reader_is_shell), for
    // the newline arm to act on once the delimiter word lands in `toks`.
    let mut heredoc: Option<(bool, bool)> = None;
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(&n) = chars.peek() {
                    cur.push(n);
                    chars.next();
                } else {
                    cur.push('\\');
                }
            }
            '\'' => loop {
                match chars.next() {
                    Some('\'') => break,
                    Some(ch) => cur.push(ch),
                    None => return None,
                }
            },
            '"' => loop {
                match chars.next() {
                    Some('"') => break,
                    // Inside double quotes a backslash keeps its special
                    // meaning only before these four characters.
                    Some('\\') if matches!(chars.peek(), Some('"' | '\\' | '$' | '`')) => {
                        cur.push(chars.next()?);
                    }
                    // A substitution inside double quotes lexes as its own
                    // command too; its body would otherwise ride inside one
                    // literal word, unread by any caller.
                    Some('$') if chars.peek() == Some(&'(') => {
                        chars.next();
                        cur.push('$');
                        cur.push_str(&group(chars, bodies, true)?);
                    }
                    Some('`') => cur.push_str(&backtick(chars, bodies)?),
                    Some(ch) => cur.push(ch),
                    None => return None,
                }
            },
            ' ' | '\t' | '\r' => {
                if !cur.is_empty() {
                    toks.push(std::mem::take(&mut cur));
                }
            }
            ';' | '|' | '&' | '\n' => {
                if !cur.is_empty() {
                    toks.push(std::mem::take(&mut cur));
                }
                if c == '\n' && heredoc.is_some() {
                    let (strip_tabs, reader_is_shell) = heredoc.take().unwrap();
                    let delim = toks.last().cloned().unwrap_or_default();
                    let mut body = String::new();
                    let mut line = String::new();
                    while let Some(bc) = chars.next() {
                        if bc != '\n' {
                            line.push(bc);
                            continue;
                        }
                        let bare = if strip_tabs {
                            line.trim_start_matches('\t')
                        } else {
                            &line[..]
                        };
                        if bare == delim {
                            break;
                        }
                        body.push_str(&line);
                        body.push('\n');
                        line.clear();
                    }
                    if reader_is_shell {
                        if let Some(sub) = lex(&body) {
                            toks.extend(sub);
                        }
                    }
                    toks.push("\n".to_string());
                    continue;
                }
                let mut op = c.to_string();
                while chars.peek() == Some(&c) {
                    op.push(c);
                    chars.next();
                }
                // `&>` redirects both streams; it is not the background `&`.
                if c == '&' && chars.peek() == Some(&'>') {
                    chars.next();
                    op.push('>');
                }
                toks.push(op);
            }
            '(' if !cur.is_empty() => {
                // A glued `(` keeps the word one token (`mv $(pick x)` binds
                // `/tmp/out`); the body lexes as its own command.
                let command = cur.ends_with('$');
                cur.push_str(&group(chars, bodies, command)?);
            }
            ')' if in_subst && depth == 0 => {
                if !cur.is_empty() {
                    toks.push(std::mem::take(&mut cur));
                }
                return Some(toks);
            }
            '(' | ')' => {
                // Parens are operators at a word boundary; an open group is
                // never glued into a word, so no closer ever rides in one.
                if !cur.is_empty() {
                    toks.push(std::mem::take(&mut cur));
                }
                if c == '(' {
                    depth += 1;
                } else {
                    depth = depth.saturating_sub(1);
                }
                toks.push(c.to_string());
            }
            '`' => cur.push_str(&backtick(chars, bodies)?),
            '<' | '>' => {
                let mut redir = String::new();
                if c == '>' && !cur.is_empty() && cur.bytes().all(|b| b.is_ascii_digit()) {
                    // The digits are the fd prefix of `2>`.
                    redir.push_str(&cur);
                    cur.clear();
                } else if !cur.is_empty() {
                    toks.push(std::mem::take(&mut cur));
                }
                redir.push(c);
                while matches!(chars.peek(), Some('>' | '<' | '&' | '|' | '!')) {
                    redir.push(chars.next()?);
                }
                if redir == "<<" && chars.peek() == Some(&'-') {
                    redir.push('-');
                    chars.next();
                }
                // The reader is just the word right before `<<DELIM`: real
                // heredocs are always `bash <<EOF`, never preceded by a
                // redirect target, so the immediate predecessor suffices.
                if redir == "<<" || redir == "<<-" {
                    let shell = toks.last().is_some_and(|t| {
                        matches!(t.rsplit('/').next().unwrap_or(t), "bash" | "sh" | "zsh")
                    });
                    heredoc = Some((redir == "<<-", shell));
                }
                toks.push(redir);
            }
            _ => cur.push(c),
        }
    }
    if in_subst {
        // An unclosed `$( ` never executes, like an unterminated quote.
        return None;
    }
    if !cur.is_empty() {
        toks.push(cur);
    }
    Some(toks)
}

/// `^[A-Za-z_][A-Za-z0-9_]*=`: a leading assignment keeps command position,
/// so `B=/path mv a b` is a mv, not a command named `B=/path`.
fn is_assignment(t: &str) -> bool {
    let Some(eq) = t.find('=') else {
        return false;
    };
    let name = &t[..eq];
    let mut ch = name.chars();
    matches!(ch.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && ch.all(|c| c.is_ascii_alphanumeric() || c == '_')
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
    // A leading `~/` is $HOME (bash expands it unquoted); without this the
    // tilde path joined the cwd and read as a repo-relative write.
    let path = crate::king_board::scope::expand_home(p);
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

/// Realpath prefix containment.
fn real_prefix(p: &str, cwd: &Path, root: &Path) -> bool {
    let p = real_of(p, cwd);
    let root = real_of(&root.to_string_lossy(), cwd);
    p == root || p.starts_with(&root)
}

/// `king.write_roots`: in-repo paths the operator lets a court session write.
/// A bare string is one root. A relative entry joins the repo root, a `~/`
/// entry joins $HOME, and a blank entry is dropped (it would name the repo).
fn write_roots(v: Option<toml::Value>, repo_root: &Path) -> Vec<PathBuf> {
    let raw = match v {
        Some(toml::Value::String(s)) => vec![s],
        Some(toml::Value::Array(a)) => a
            .into_iter()
            .filter_map(|e| e.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    raw.iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| repo_root.join(crate::king_board::scope::expand_home(s)))
        .collect()
}

/// The inverted step-10 predicate (2026-09-17 ruling): SOURCE is any path
/// realpath-inside the repo root, with carve-outs for the repo's `.fno`
/// state tree, for build output, and for the operator's `king.write_roots`
/// entries. The vault is not source; a write outside the repo allows
/// wherever it lands.
fn write_denied(t: &str, cwd: &Path, repo_root: &Path, roots: &[PathBuf]) -> bool {
    real_prefix(t, cwd, repo_root)
        && !real_prefix(t, cwd, &repo_root.join(".fno"))
        && !is_build_output(t, cwd)
        && !roots.iter().any(|r| real_prefix(t, cwd, r))
}

/// A path whose nearest ancestor directory holds a `CACHEDIR.TAG` is build
/// output, not source. Cargo target dirs sit inside the repo and outside
/// `.fno`; matched by the tag file, never by the name `target`, which is
/// also a source directory name (`.claude/rules/worktrees.md`).
fn is_build_output(t: &str, cwd: &Path) -> bool {
    let mut dir = real_of(t, cwd).parent().map(Path::to_path_buf);
    while let Some(d) = dir {
        if d.join("CACHEDIR.TAG").is_file() {
            return true;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    false
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
/// transcript); unreadable falls through fail-closed. The pairing is the one
/// shared walk (`interrupt_classify::trailing_open_call`), not a private leg.
fn transcript_is_open_spawn(transcript: &str) -> bool {
    if transcript.is_empty() {
        return false;
    }
    crate::tail_text_strict(Path::new(transcript), 262_144)
        .and_then(|tail| {
            crate::interrupt_classify::trailing_open_call(&tail)
                .filter(|c| c.name == "Task" || c.name == "Agent")
        })
        .is_some()
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
        // A dup inside a command substitution: `lex` glues the closers
        // into the word while `$( ` is open, so `1)` must still read as fd.
        assert!(
            targets("N=$(cmd 2>&1)").is_empty(),
            "subst dup is not a write"
        );
        assert!(
            targets("N=`cmd 2>&1`").is_empty(),
            "backtick dup is not a write"
        );
        assert!(
            targets("N=$(a $(b 2>&1))").is_empty(),
            "nested subst dup is not a write"
        );
        assert!(
            targets("N=$(cmd >&2)").is_empty(),
            "reversed dup is not a write"
        );
        assert_eq!(
            targets("X=$(cmd > out.txt)"),
            vec!["out.txt"],
            "a real write inside a substitution still binds"
        );
        assert_eq!(
            targets("N=$(cmd | tee out.txt)"),
            vec!["out.txt"],
            "tee inside a substitution still binds"
        );
        assert_eq!(
            targets("printf x > '1)'"),
            vec!["1)"],
            "a quoted literal closer is part of the path"
        );
        // Quoted "a > b" is one word, not an operator.
        assert!(targets("echo \"a > b\"").is_empty());
    }

    #[test]
    fn verb_operands_bind() {
        // `<` now lexes as a read operator, so the stdin file is no
        // longer bound as a tee write (deliberate narrowing of the old
        // over-broad rule; the real writes still bind).
        assert_eq!(
            targets("tee /tmp/x /tmp/y < /tmp/in"),
            vec!["/tmp/x", "/tmp/y"]
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
        assert!(targets("echo \"unterminated").is_empty());
    }

    /// A cwd-only helper for the heredoc specimens below: session cwd is
    /// already the repo root, so a relative path resolves inside it exactly
    /// as a real crowned session would see it.
    fn bash_allowed_in(repo: &Path, cmd: &str) -> bool {
        targets(cmd)
            .iter()
            .all(|t| !write_denied(t, repo, repo, &[]))
    }

    #[test]
    fn heredoc_append_to_job_tmp_allows() {
        // Regression: a heredoc body that happens to quote a shell command
        // as prose (a mail draft showing `cat > payload-e9d6.txt` as an
        // example) must not donate that relative path as a write target.
        let repo = std::env::temp_dir().join(format!("kgd-heredoc-jobtmp-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&repo);
        let cmd = "cat >> /Users/bb16/.claude/jobs/X/tmp/payload.txt <<'EOF'\n\
                   run: cat > payload-e9d6.txt\nEOF";
        assert!(
            bash_allowed_in(&repo, cmd),
            "a heredoc body must not donate a phantom write target"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn heredoc_body_naming_a_repo_path_still_allows() {
        // The body names a real repo path next to a write-verb word (`cp`);
        // `cat` never reads its own stdin as commands, so the whole body is
        // inert text, not a `cp` invocation to classify.
        let repo = std::env::temp_dir().join(format!("kgd-heredoc-body-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&repo);
        let cmd = "cat >> /tmp/out.txt <<'EOF'\n\
                   example: cp notes.txt crates/fno-agents/src/lib.rs\nEOF";
        assert!(bash_allowed_in(&repo, cmd));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn shell_reading_heredoc_body_still_refuses_a_real_write() {
        // `bash <<'EOF'` DOES read its stdin as commands, so a real write
        // inside that body still refuses.
        let repo = std::env::temp_dir().join(format!("kgd-heredoc-shell-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&repo);
        let cmd = "bash <<'EOF'\ncat > crates/fno-agents/src/lib.rs\nEOF";
        assert!(!bash_allowed_in(&repo, cmd));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn plain_in_repo_heredoc_write_still_refuses() {
        // A heredoc attached to the command's OWN redirect, not its body,
        // still refuses - the heredoc parsing never loosens a real write.
        let repo = std::env::temp_dir().join(format!("kgd-heredoc-plain-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&repo);
        let cmd = "cat > crates/fno-agents/src/lib.rs <<'EOF'\nbody\nEOF";
        assert!(!bash_allowed_in(&repo, cmd));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn operators_and_positionals_never_bind() {
        // Crown-session specimens, refused on main, each a subcommand
        // argument or fd
        // duplication, never a path the command writes.
        assert!(targets("fno config plugin install claude").is_empty());
        assert!(targets("brew install jq").is_empty());
        assert!(targets("npm install --save-dev x").is_empty());
        assert!(
            targets("/usr/bin/git fetch origin pull/2175/head --quiet 2>&1|tail -1").is_empty()
        );
        // `;` glued to a word is a boundary: the printf operand never joins
        // the mv operand pool.
        assert_eq!(
            targets("B=x; mv a b; ls -l c; printf '{}' | bash h.sh"),
            vec!["b"]
        );
        // A subshell boundary flushes the pool like `;` does.
        assert_eq!(targets("(cd /tmp && mv a b)"), vec!["b"]);
        // Command substitution stays one word: no phantom operand.
        assert_eq!(targets("cp $(mktemp) /tmp/d"), vec!["/tmp/d"]);
        // An open substitution survives a whitespace split: the real
        // destination binds, never a fragment inside `$( )`.
        assert_eq!(targets("mv $(pick_build x) /tmp/out"), vec!["/tmp/out"]);
        assert_eq!(targets("mv $(a $(b) c) /tmp/z"), vec!["/tmp/z"]);
    }

    #[test]
    fn command_position_still_binds_real_writes() {
        // A genuine redirect binds, today and after (positive control).
        assert_eq!(
            targets("echo hi > cli/src/fno/x.py"),
            vec!["cli/src/fno/x.py"]
        );
        // A real `install` in command position binds its destination.
        assert_eq!(targets("install -d /tmp/x"), vec!["/tmp/x"]);
        // The keep words hand command position to the write verb.
        assert_eq!(targets("sudo mv a /tmp/b"), vec!["/tmp/b"]);
        assert_eq!(targets("command mv a /tmp/b"), vec!["/tmp/b"]);
        assert_eq!(targets("env FOO=bar cp /tmp/a /tmp/b"), vec!["/tmp/b"]);
        assert_eq!(targets("B=/path mv a b"), vec!["b"]);
    }

    #[test]
    fn substitution_bodies_bind_their_writes() {
        // Every substitution spelling binds exactly its one write target:
        // the body lexes as its own command, the outer word keeps its place.
        assert_eq!(targets("N=$(cp a b)"), vec!["b"]);
        assert_eq!(targets("$(cp a b)"), vec!["b"]);
        assert_eq!(targets("echo $(cp a b)"), vec!["b"]);
        assert_eq!(targets("echo \"$(cp a b)\""), vec!["b"]);
        assert_eq!(targets("\"$(cmd > out)\""), vec!["out"]);
        assert_eq!(targets("N=`cp a b`"), vec!["b"]);
        assert_eq!(targets("echo \"`cmd > out`\""), vec!["out"]);
        assert_eq!(targets("N=$(echo $(cp a b))"), vec!["b"]);
        assert_eq!(targets("echo \"$(printf \"%s\" x > out)\""), vec!["out"]);
    }

    #[test]
    fn process_substitution_keeps_binding() {
        // The forms that bound before the lexer change keep binding: a bare
        // group at a word boundary runs in this same token stream.
        assert_eq!(targets("diff <(cp a b) c"), vec!["b"]);
        assert_eq!(
            targets("tee >(cp /dev/stdin crates/fno-agents/src/lib.rs) <<< x"),
            vec!["crates/fno-agents/src/lib.rs"]
        );
    }

    #[test]
    fn substitution_lookalikes_bind_nothing() {
        // Quoted text is inert; arithmetic has no redirect; a heredoc body
        // read by a non-shell never donates a target; an unclosed `$( `
        // never executes.
        assert!(targets("echo '$(cp a b)'").is_empty());
        assert!(targets("echo \"a > b\"").is_empty());
        assert!(targets("N=$(git status 2>&1)").is_empty());
        assert!(targets("echo \"$((a > b))\"").is_empty());
        assert!(targets("echo $((a > b))").is_empty());
        assert!(targets("git commit -m \"$(cat <<'EOF'\nfix: a > b, cp x y\nEOF\n)\"").is_empty());
        assert!(targets("N=$(cat <<'EOF'\n1) cp x y\nEOF\n)").is_empty());
        assert!(
            targets("N=$(cp a b").is_empty(),
            "unclosed substitution never executes"
        );
    }

    #[test]
    fn substituted_source_write_refuses() {
        // The crowned-session shape that passed before: a source write read
        // only as a substitution body must refuse through the repo rule.
        let repo = std::env::temp_dir().join(format!("kgd-subst-src-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&repo);
        assert!(!bash_allowed_in(
            &repo,
            "N=$(cp notes.txt crates/fno-agents/src/lib.rs)"
        ));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn tilde_target_resolves_outside_the_repo() {
        // A leading `~/` expands against $HOME, never joins the cwd as a
        // repo-relative name; the relative control still refuses. HOME is
        // read, never set (env writes race the parallel test threads).
        let repo = std::env::temp_dir().join(format!("kgd-tilde-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&repo);
        assert!(!write_denied("~/.cargo/config.toml.bak", &repo, &repo, &[]));
        assert!(write_denied("x/.cargo/config.toml.bak", &repo, &repo, &[]));
        let _ = std::fs::remove_dir_all(&repo);
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

    /// Characterization fixtures for `transcript_is_open_spawn`: plant a
    /// transcript whose tail matches the named shape.
    fn plant_transcript(tag: &str, lines: &[String]) -> String {
        let dir = std::env::temp_dir().join(format!("kgd-spawn-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path.to_string_lossy().into_owned()
    }

    fn spawn_line(id: &str, name: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{{}}}}]}}}}"#
        )
    }

    fn result_line(id: &str) -> String {
        format!(
            r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"{id}","content":"ok"}}]}}}}"#
        )
    }

    // (a) The newest tool_use is an unanswered Task call: the limb is allowed.
    #[test]
    fn open_task_at_the_tail_is_an_open_spawn() {
        let path = plant_transcript(
            "open-task",
            &[spawn_line("t1", "Bash"), spawn_line("t2", "Task")],
        );
        assert!(transcript_is_open_spawn(&path));
    }

    // (b) The same call with its result reads closed.
    #[test]
    fn answered_task_is_not_an_open_spawn() {
        let path = plant_transcript("done-task", &[spawn_line("t1", "Task"), result_line("t1")]);
        assert!(!transcript_is_open_spawn(&path));
    }

    // (c) A newer open Bash call means the spawn is no longer the newest call.
    #[test]
    fn newer_plain_call_closes_an_older_spawn() {
        let path = plant_transcript(
            "bash-after",
            &[spawn_line("t1", "Task"), spawn_line("t2", "Bash")],
        );
        assert!(!transcript_is_open_spawn(&path));
    }

    // (d) A missing transcript fails closed, unchanged.
    #[test]
    fn missing_transcript_is_never_an_open_spawn() {
        assert!(!transcript_is_open_spawn(""));
        assert!(!transcript_is_open_spawn(
            "/nonexistent/kgd/no-such-transcript.jsonl"
        ));
    }

    // (e) A tail that is not valid UTF-8 is unreadable evidence: fail closed
    // the way the deleted private leg did, never a repaired guess.
    #[test]
    fn corrupt_tail_fails_closed() {
        let dir = std::env::temp_dir().join(format!("kgd-spawn-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let mut body = spawn_line("t1", "Task").into_bytes();
        body.push(0xFF);
        std::fs::write(&path, body).unwrap();
        assert!(!transcript_is_open_spawn(&path.to_string_lossy()));
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

    /// Fixture: a repo checkout plus a vault OUTSIDE it; the internal
    /// symlink is what a vault-backed checkout looks like on disk.
    struct Roots {
        base: PathBuf,
        repo: PathBuf,
    }

    fn roots(tag: &str) -> Roots {
        let base = std::env::temp_dir().join(format!("kgd-{tag}-{}", std::process::id()));
        Roots {
            repo: base.join("repo"),
            base,
        }
    }

    fn allowed(r: &Roots, target: &Path) -> bool {
        !write_denied(&target.to_string_lossy(), &r.base, &r.repo, &[])
    }

    #[test]
    fn vault_via_internal_symlink_is_allowed() {
        // Required test 1: the vault is not source. A write through the
        // internal symlink resolves outside the repo and allows.
        let r = roots("symlink");
        let _ = std::fs::create_dir_all(r.repo.join("cli/src"));
        let _ = std::fs::create_dir_all(r.base.join("vault/fno/analysis"));
        let _ = std::os::unix::fs::symlink(r.base.join("vault"), r.repo.join("internal"));
        assert!(allowed(&r, &r.repo.join("internal/fno/analysis/foo.json")));
        let _ = std::fs::remove_dir_all(&r.base);
    }

    #[test]
    fn source_write_is_denied() {
        // Required test 2: the guard exists to stop a king writing SOURCE.
        let r = roots("source");
        assert!(!allowed(&r, &r.repo.join("cli/src/fno/anything.py")));
        assert!(!allowed(&r, &r.repo.join("crates/fno-agents/src/lib.rs")));
        let _ = std::fs::remove_dir_all(&r.base);
    }

    #[test]
    fn repo_dotfno_state_is_allowed() {
        // Required test 3: the carve-out keeps the default .fno/plans path
        // writable for a vault-less user.
        let r = roots("dotfno");
        assert!(allowed(&r, &r.repo.join(".fno/plans/foo.md")));
        let _ = std::fs::remove_dir_all(&r.base);
    }

    #[test]
    fn build_output_with_cachedir_tag_is_allowed() {
        // Build output inside the repo is not source, matched by the
        // CACHEDIR.TAG file, never by the directory name (a name-based sweep
        // once deleted 66 real `target` source dirs).
        let r = roots("cachedir");
        let bin = r.repo.join("crates/fno-agents/target/debug/fno-agents");
        let _ = std::fs::create_dir_all(bin.parent().unwrap());
        let _ = std::fs::write(
            r.repo.join("crates/fno-agents/target/CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        );
        assert!(allowed(&r, &bin));
        // A name-alike source dir with no tag above it stays refused.
        assert!(!allowed(&r, &r.repo.join("skills/target/SKILL.md")));
        let _ = std::fs::remove_dir_all(&r.base);
    }

    #[test]
    fn refusal_names_the_rule_in_two_lines() {
        let text = deny_text("cli/src/fno/x.py", Path::new("/repo"));
        assert_eq!(text.lines().count(), 2, "two lines: {text:?}");
        assert!(text.contains("operates the machine and does not author it"));
        assert!(text.contains("fno config plugin install"));
        assert!(text.contains("Delegate the edit or escalate"));
        assert!(text.contains("config.king.write_roots"));
    }

    #[test]
    fn no_vault_still_answers_denies_source_allows_outside() {
        // Required test 4: no vault configured anywhere - the predicate only
        // knows the repo root. With `&[]` the guard answers as before:
        // source denies, anything outside allows.
        let r = roots("novault");
        assert!(!allowed(&r, &r.repo.join("cli/src/fno/anything.py")));
        assert!(allowed(&r, &r.base.join("anywhere/else/foo.md")));
        let _ = std::fs::remove_dir_all(&r.base);
    }

    #[test]
    fn listed_write_root_allows_its_subtree_only() {
        // An operator-named root allows its realpath subtree and nothing
        // else: docs allows docs/guide.md, not docsx/ and not cli/src.
        let r = roots("writeroots");
        let _ = std::fs::create_dir_all(r.repo.join("docs"));
        let listed = write_roots(
            Some(toml::Value::Array(vec![toml::Value::from("docs")])),
            &r.repo,
        );
        assert!(!write_denied(
            &r.repo.join("docs/guide.md").to_string_lossy(),
            &r.base,
            &r.repo,
            &listed
        ));
        assert!(write_denied(
            &r.repo.join("docs/guide.md").to_string_lossy(),
            &r.base,
            &r.repo,
            &[]
        ));
        assert!(write_denied(
            &r.repo.join("docsx/guide.md").to_string_lossy(),
            &r.base,
            &r.repo,
            &listed
        ));
        assert!(write_denied(
            &r.repo.join("cli/src/fno/x.py").to_string_lossy(),
            &r.base,
            &r.repo,
            &listed
        ));
        assert!(allowed(&r, &r.base.join("elsewhere/x.md")));
        let _ = std::fs::remove_dir_all(&r.base);
    }

    #[test]
    fn symlinked_write_root_resolves_to_its_target() {
        // A symlinked entry covers its target: notes -> docs allows a write
        // into docs, and still denies source beside it.
        let r = roots("writeroots-symlink");
        let _ = std::fs::create_dir_all(r.repo.join("docs"));
        let _ = std::fs::create_dir_all(r.repo.join("src"));
        let _ = std::os::unix::fs::symlink(r.repo.join("docs"), r.repo.join("notes"));
        let listed = write_roots(Some(toml::Value::from("notes")), &r.repo);
        assert!(!write_denied(
            &r.repo.join("docs/guide.md").to_string_lossy(),
            &r.base,
            &r.repo,
            &listed
        ));
        assert!(write_denied(
            &r.repo.join("src/lib.rs").to_string_lossy(),
            &r.base,
            &r.repo,
            &listed
        ));
        let _ = std::fs::remove_dir_all(&r.base);
    }

    #[test]
    fn write_roots_parse_relative_home_absolute_and_drop_blank() {
        // The parser: relative joins the repo, absolute stays absolute,
        // blanks and non-strings drop, a bare string is one root, `~/` joins
        // $HOME. HOME is read, never set: env writes race the parallel
        // test threads.
        assert_eq!(
            write_roots(
                Some(toml::Value::Array(vec![
                    toml::Value::from("docs"),
                    toml::Value::from(" "),
                    toml::Value::from("/abs/x"),
                    toml::Value::Integer(7),
                ])),
                Path::new("/repo"),
            ),
            vec![PathBuf::from("/repo/docs"), PathBuf::from("/abs/x")]
        );
        assert_eq!(
            write_roots(Some(toml::Value::from(".claude/rules")), Path::new("/repo")),
            vec![PathBuf::from("/repo/.claude/rules")]
        );
        assert_eq!(write_roots(None, Path::new("/repo")), Vec::<PathBuf>::new());
        assert_eq!(
            write_roots(Some(toml::Value::Integer(7)), Path::new("/repo")),
            Vec::<PathBuf>::new()
        );
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(
                write_roots(Some(toml::Value::from("~/vault")), Path::new("/repo")),
                vec![PathBuf::from(home).join("vault")]
            );
        }
    }
}
