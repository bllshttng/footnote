//! `fno-agents hook write-gate` - one PreToolUse gate for the three write
//! guards.
//!
//! Replaces `hooks/graph-write-protect.sh`, `hooks/claude-config-write-guard.sh`
//! and `hooks/generated-write-guard.sh`, which each paid a bash + jq spawn (plus
//! a native store commit) on every write-capable tool call: 56,455 hook-seconds
//! a day in the 2026-10-08 slowness audit, the largest line item. One fire here
//! is one process. The gate exits early when the payload names no protected
//! path; every refusal string is byte-identical to the shell guards', and the
//! `guard_decision` rows keep the original per-guard names so the journal
//! projection does not change shape.
//!
//! Sections run in hooks.json order (graph, config, generated); the first block
//! wins and later sections record nothing, exactly as when the harness denied
//! the call before their siblings launched.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// One section outcome. `Allow` carries nothing; a block carries the refusal
/// string the shell guard printed.
enum Sec {
    Allow,
    Block(String),
}

/// The refusal strings, byte-identical to the shell guards'.
const GRAPH_REASON: &str =
    "graph.json is retired; do not recreate it. Mutate the graph.db store via `fno backlog` commands.";
const DB_REASON: &str = "graph.db is the authoritative store; direct writes to it or its WAL files are blocked. Mutate via `fno backlog` commands.";
const MANIFEST_REASON: &str = "target-state.md is an immutable session manifest; direct Edit/Write is blocked. The only legal post-init write is first-fill of an empty plan_path via `fno do state set --field plan_path`. Use `fno do state` / `fno do target` verbs, not a hand edit.";
const CONFIG_REASON: &str = "Do not edit an fno config.toml by hand. Run `fno config set <key> <value>` (add `--local` for a repo file). It checks the key against the schema. A hand-added key that is not in the schema prints a warning on every fno call. If `fno config set` refuses the key, the key is not modeled yet: file a node with `fno backlog idea`.";
const BASH_BLOCK_SUFFIX: &str =
    " (this Bash write to a protected state file is blocked; use `fno backlog` / `fno do state`).";
const FAILCLOSED_MALFORMED: &str =
    "graph-write-protect: payload references a protected state file but could not be parsed; blocking fail-closed.";
/// Only the sh shim emits this one: it is the no-binary fallback, the same
/// posture the shell guard took when no parser existed.
pub const FAILCLOSED_NO_PARSER: &str = "graph-write-protect: neither jq nor python3 available to parse a payload referencing a protected state file; blocking fail-closed. Install jq or python3.";

/// Payload fields the three sections read, extracted once.
struct Payload {
    /// Raw stdin text: the token pre-filters scan this, JSON escaping included.
    raw: String,
    /// `tool_name`, or empty when absent (the shell guards read it as "").
    tool: String,
    file_path: String,
    command: String,
    cwd: String,
    session_id: String,
    /// `file_path` with separator-equivalent runs collapsed (`//` and `/./`),
    /// as the shell guards' jq `norm` did before every suffix check.
    fp_norm: String,
    /// `command` newline-flattened and separator-collapsed, for the Bash arms.
    cmd_norm: String,
}

fn norm(s: &str) -> String {
    // The shell guards' jq: gsub("/+";"/") | gsub("/\\./";"/"). Two single
    // passes, not a fixed point: jq's gsub is one left-to-right pass per
    // pattern, so "/././" folds once to "/./" here exactly as it did there.
    let mut out = String::with_capacity(s.len());
    let mut slashes = 0usize;
    for ch in s.chars() {
        if ch == '/' {
            slashes += 1;
        } else {
            if slashes > 0 {
                out.push('/');
                slashes = 0;
            }
            out.push(ch);
        }
    }
    if slashes > 0 {
        out.push('/');
    }
    out.replace("/./", "/")
}

/// Entry: read the payload once, run the three sections in order, print, and
/// always exit 0 (the verdict rides stdout JSON, never the exit code).
pub fn run(_args: &[String]) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let raw = super::read_stdin();
    let parsed: Option<Value> = serde_json::from_str(raw.trim()).ok();

    // The row's `tool` field: guard-mark scraped the raw payload with
    // `"tool_name":\s*"([A-Za-z|]+)"`, so a name carrying any other character
    // (apply_patch's underscore) read as "unknown". Replicate, not improve:
    // the journal projection keeps its old shape.
    let row_tool = parsed
        .as_ref()
        .and_then(|v| v.get("tool_name"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphabetic() || c == '|'))
        .unwrap_or("unknown")
        .to_string();

    // Malformed payload: the shell world split by guard - graph failed closed
    // on a protected token, config and generated failed open. A payload with
    // no token never reached a parser at all.
    let Some(value) = parsed else {
        if raw_tokens_hint_graph(&raw) {
            mark(&cwd, "graph-write-protect", &row_tool, true);
            return super::emit_block(FAILCLOSED_MALFORMED);
        }
        mark(&cwd, "graph-write-protect", &row_tool, false);
        mark(&cwd, "claude-config-write-guard", &row_tool, false);
        mark(&cwd, "generated-write-guard", &row_tool, false);
        return super::emit_allow();
    };

    // One normalized view per shell-guard contract: file_path collapsed for
    // the suffix checks; command newline-flattened and collapsed for the Bash
    // arms; the generated section reads both raw, as its jq -j did.
    let p = Payload {
        raw,
        tool: value
            .get("tool_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        file_path: value
            .pointer("/tool_input/file_path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        command: value
            .pointer("/tool_input/command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        cwd: value
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        session_id: value
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        fp_norm: norm(
            value
                .pointer("/tool_input/file_path")
                .and_then(Value::as_str)
                .unwrap_or(""),
        ),
        cmd_norm: norm(
            &value
                .pointer("/tool_input/command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .replace('\n', " "),
        ),
    };

    // Early exit: no section can name a protected path from this payload.
    // The shell guards' own pre-filters were the union of these token scans
    // plus "no file_path and no command to extract". The fast path still
    // writes one allow row per guard: the liveness contract is exactly one
    // guard_decision row per guard per invocation, whatever the verdict.
    if !raw_tokens_hint_graph(&p.raw)
        && !raw_tokens_hint_config(&p.raw)
        && p.file_path.is_empty()
        && p.command.is_empty()
    {
        mark(&cwd, "graph-write-protect", &row_tool, false);
        mark(&cwd, "claude-config-write-guard", &row_tool, false);
        mark(&cwd, "generated-write-guard", &row_tool, false);
        return super::emit_allow();
    }

    // Sections in hooks.json order; first block wins, later sections stay
    // silent exactly as when the harness denied before their siblings ran.
    match graph_section(&p, &cwd) {
        Sec::Block(reason) => {
            mark(&cwd, "graph-write-protect", &row_tool, true);
            return super::emit_block(&reason);
        }
        Sec::Allow => mark(&cwd, "graph-write-protect", &row_tool, false),
    }
    match config_section(&p, &cwd) {
        Sec::Block(reason) => {
            mark(&cwd, "claude-config-write-guard", &row_tool, true);
            return super::emit_block(&reason);
        }
        Sec::Allow => mark(&cwd, "claude-config-write-guard", &row_tool, false),
    }
    match generated_section(&p, &cwd) {
        Sec::Block(reason) => {
            mark(&cwd, "generated-write-guard", &row_tool, true);
            return super::emit_block(&reason);
        }
        Sec::Allow => mark(&cwd, "generated-write-guard", &row_tool, false),
    }
    super::emit_allow()
}

/// One `guard_decision` row. Best-effort by contract: a failed append can
/// never change the decision.
fn mark(cwd: &Path, guard: &str, tool: &str, denied: bool) {
    super::emit_guard_decision(cwd, guard, tool, denied);
}

/// The graph guard's raw pre-filter: none of the protected FILENAMEs appear
/// anywhere in the payload, so no write can target one (JSON-escaped slashes
/// and doubled separators still contain the filename).
fn raw_tokens_hint_graph(raw: &str) -> bool {
    raw.contains("graph.json")
        || raw.contains("target-state.md")
        || raw.contains("graph.db")
        || raw.contains(".fno/artifacts/")
}

/// The config guard's raw pre-filter tokens, evaluated only when the fast path
/// is allowed at all (a relocated config dir or state root names no token).
fn raw_tokens_hint_config(raw: &str) -> bool {
    let relocated_cfg = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(|v| !v.to_string_lossy().contains(".claude"))
        .unwrap_or(false);
    let relocated_state =
        std::env::var_os("FNO_STATE_DIR").is_some() || std::env::var_os("FNO_HOME").is_some();
    if relocated_cfg || relocated_state {
        return true; // fast path is off; the section must run
    }
    raw.contains(".claude")
        || raw.contains("CLAUDE_CONFIG_DIR")
        || raw.contains(".fno")
        || raw.contains("FNO_STATE_DIR")
        || raw.contains("FNO_HOME")
}

// ---------------------------------------------------------------------------
// Section A: graph + state surfaces (graph-write-protect.sh)
// ---------------------------------------------------------------------------

fn graph_section(p: &Payload, cwd: &Path) -> Sec {
    if config_toml_write(p) {
        return Sec::Block(CONFIG_REASON.to_string());
    }
    if !raw_tokens_hint_graph(&p.raw) {
        return Sec::Allow;
    }
    if p.tool.is_empty() {
        // A protected token is present but the payload carries no tool: fail
        // closed rather than approve a possible forge.
        return Sec::Block(FAILCLOSED_MALFORMED.to_string());
    }
    match p.tool.as_str() {
        "Edit" | "Write" => graph_edit_write(p, cwd),
        "Bash" => {
            if bash_targets_protected(&p.cmd_norm) {
                Sec::Block(format!("{MANIFEST_REASON}{BASH_BLOCK_SUFFIX}"))
            } else {
                Sec::Allow
            }
        }
        _ => Sec::Allow,
    }
}

/// Fixture/test scaffolding may hold a protected file under a test dir.
fn under_test_dir(fp: &str) -> bool {
    fp.contains("/test/") || fp.contains("/tests/") || fp.contains("/fixtures/")
}

/// A tool write to an fno config file. It stays outside the graph pre-filter
/// on purpose: a malformed payload naming config.toml fails open, as the
/// config guard does, not closed like the state files.
fn config_toml_write(p: &Payload) -> bool {
    const TAIL: &str = ".fno/config.toml";
    let is_config = |path: &str| {
        let path = norm(path);
        path.ends_with(TAIL) && !under_test_dir(&path)
    };
    // Every Bash call reaches this gate: compile no regex for a command that
    // never names the file.
    if !p.fp_norm.contains(TAIL) && !p.cmd_norm.contains(TAIL) {
        return false;
    }
    if p.command.contains("*** Begin Patch") {
        return write_targets("", &p.command).iter().any(|t| is_config(t));
    }
    match p.tool.as_str() {
        "Edit" | "Write" => is_config(&p.file_path),
        "Bash" => bash_writes(
            &p.cmd_norm,
            r"[^[:space:];|&<>]*\.fno/config\.toml",
            r"\.fno/config\.toml",
        ),
        _ => false,
    }
}

fn graph_edit_write(p: &Payload, cwd: &Path) -> Sec {
    let fp = &p.fp_norm;
    if under_test_dir(fp) {
        return Sec::Allow;
    }
    if fp.ends_with(".fno/graph.json") {
        return Sec::Block(GRAPH_REASON.to_string());
    }
    // The tail also names the -wal and -shm siblings; backups/ snapshots never
    // carry the `.fno/graph.db` substring and stay editable.
    if fp.contains(".fno/graph.db") {
        return Sec::Block(DB_REASON.to_string());
    }
    // The manifest moved into the repo's space; both shapes are refused, and
    // the old checkout path stays matched so a stale copy is still refused.
    if fp.ends_with(".fno/target-state.md")
        || (fp.contains("/.fno/spaces/") && fp.ends_with("target-state.md"))
    {
        if drive_authority_active() {
            emit_event(
                cwd,
                "hook",
                "gate_edit_forged_during_drive",
                json!({"file_path": p.file_path, "reason": "drive_authority_active"}),
            );
        }
        return Sec::Block(MANIFEST_REASON.to_string());
    }
    // An artifact edit during this agent's drive window is allowed and emits
    // the canonical operator audit event.
    if fp.contains("/.fno/artifacts/") && fp.ends_with(".md") && drive_authority_active() {
        emit_event(
            cwd,
            "hook",
            "operator_initiated",
            json!({
                "action_type": "artifact_edited_operator_initiated",
                "file_path": p.file_path,
                "last_operator_edit": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                "reason": "drive_authority_active"
            }),
        );
    }
    Sec::Allow
}

/// The graph guard's Bash arm: a write OPERATOR bound to a protected path,
/// keyed on operator+path adjacency, never on bare mention. Enumerated floor,
/// not Turing-complete coverage. Regexes are leftmost-first here where bash
/// `=~` was leftmost-longest; these patterns have no alternation whose capture
/// spans disagree between the two, so the decisions are identical.
fn bash_targets_protected(cmd: &str) -> bool {
    bash_writes(
        cmd,
        r"[^[:space:];|&<>]*\.fno/(graph\.json|target-state\.md|graph\.db(-wal|-shm)?)|[^[:space:];|&<>]*/spaces/[^[:space:];|&<>]*target-state\.md",
        r"\.fno/(graph\.json|target-state\.md|graph\.db(-wal|-shm)?)|/spaces/[^;|&]*target-state\.md",
    )
}

/// The write-operator arms over one protected path: `path` matches a whole
/// path token, `clause_path` the tail an in-place editor's clause must reach.
fn bash_writes(cmd: &str, path: &str, clause_path: &str) -> bool {
    // A protected-path token. The prefix (leading dir/`~`/`$HOME`/quote chars
    // up to `.fno/`) excludes only whitespace and command separators/redirects,
    // NOT quotes: a quoted path (`> "$HOME/.fno/graph.json"`) is normal shell
    // and must still match. Right-bounded by a shell separator, a closing
    // quote/backtick/paren, or end of line. Second arm: the manifest moved
    // into the repo's space (`~/.fno/spaces/<slug>[/worktrees/<name>]/
    // target-state.md`); the old checkout path stays matched so an edit to a
    // stale copy is still refused.
    let pp = format!(r#"({path})([[:space:];|&<>)`"']|$)"#);
    // Clause tails end at a protected path within one command clause.
    let path_in_clause = format!("({clause_path})");
    let nosep = "[^;|&]*";
    let lead = r"(^|[^[:alnum:]_])";
    let op = r"([>]{1,2}|&[>]|[>]&|[>][|]|[>]!)";
    let mut arms: Vec<String> = vec![
        // Redirects immediately targeting the path: >, >>, 2>, &>, >&, >|,
        // >! (bracket forms, not an escaped >, to avoid the GNU
        // word-boundary reading of \>).
        format!(r"{op}[[:space:]]*{pp}"),
        // tee [flags] path (also `... | tee path`), then sponge path.
        format!(r#"{lead}tee[[:space:]]+(-[^[:space:]]+[[:space:]]+)*{pp}"#),
        format!(r#"{lead}sponge[[:space:]]+{pp}"#),
        // cp / mv / install / truncate with the protected path as the (last)
        // argument; dd of=path.
        format!(r#"{lead}(cp|mv|install|truncate)[[:space:]].*[[:space:]]{pp}"#),
        format!(r#"{lead}dd[[:space:]].*of={pp}"#),
    ];
    // In-place editors bind the editor to the protected path WITHIN its own
    // command clause: `echo see .fno/graph.json; sed -i x notes.md` mentions
    // the path in a different clause and must NOT match. `[^;|&]*` keeps the
    // flag and the path in the same clause as the sed/perl/jq/ex/ed verb:
    // -i, combined short flags (-Ei, -ri), and the --in-place long form.
    arms.push(format!(
        r#"{lead}(sed|perl)[[:space:]]{nosep}(-[a-zA-Z]*i|--in-place){nosep}{path_in_clause}"#
    ));
    arms.push(format!(
        r#"{lead}jq[[:space:]]{nosep}(-i|--in-place){nosep}{path_in_clause}"#
    ));
    arms.push(format!(
        r#"{lead}(ex|ed)[[:space:]]{nosep}{path_in_clause}"#
    ));
    arms.iter().any(|a| {
        regex::Regex::new(a)
            .map(|re| re.is_match(cmd))
            .unwrap_or(false)
    })
}

// ---------------------------------------------------------------------------
// Drive authority (scripts/lib/drive-authority.sh, native)
// ---------------------------------------------------------------------------

/// A superuser drive window is open on THIS session's agent: the operator, not
/// the LLM, is typing. Fail-open everywhere: no self id, no agents home, or an
/// unreadable store all read as "no window", the shell lib's posture.
pub(crate) fn drive_authority_active() -> bool {
    let Ok(self_id) = std::env::var("FNO_AGENTS_SELF_SHORT_ID") else {
        return false;
    };
    if self_id.is_empty() {
        return false;
    }
    let Some(home) = crate::paths::AgentsHome::from_env_opt() else {
        return false;
    };
    let sessions = crate::client_verbs::active_drive_sessions(home.root());
    let active = sessions.iter().any(|s| s.short_id == self_id);
    if std::env::var_os("FNO_GUARD_TRACE").is_some() {
        eprintln!(
            "write-gate: drive check self={self_id} windows={} active={active}",
            sessions.len()
        );
    }
    active
}

/// One event row into the space journal, the `emit_event`/`emit_event_raw`
/// envelope shape ({ts, source, type, data} / {ts, type, source, data}).
fn emit_event(cwd: &Path, source: &str, event_type: &str, data: Value) {
    let path =
        crate::state_path::resolve("events", cwd).unwrap_or_else(|| crate::paths::events_path(cwd));
    let event = json!({
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "source": source,
        "type": event_type,
        "data": data,
    });
    let receipt =
        crate::claims::append_event_line(&path, &event, std::time::Duration::from_secs(2));
    if std::env::var_os("FNO_GUARD_TRACE").is_some() {
        if let Err(e) = receipt {
            eprintln!("write-gate: audit append refused: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Section B: Claude config dir + fno state root (claude-config-write-guard.sh)
// ---------------------------------------------------------------------------

/// One guarded root: the config dir as spelled (for the refusal text) and its
/// physical form (for the comparison).
struct Root {
    spelled: String,
    physical: PathBuf,
}

/// `~`, `$HOME`, `$CLAUDE_CONFIG_DIR`, quotes stripped, a RELATIVE token
/// resolved against the payload cwd; then physicalized. A relative token with
/// no payload cwd stays unresolved: guessing the hook's own cwd would
/// misresolve.
fn abs_of(tok: &str, cwd: &str) -> Option<PathBuf> {
    let mut t = tok.replace('"', "").replace('`', "");
    if t.is_empty() {
        return None;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    if t.starts_with('/') {
        // absolute as written
    } else if let Some(rest) = t.strip_prefix('~') {
        t = format!("{home}{rest}");
    } else if let Some(rest) = t.strip_prefix("$HOME") {
        t = format!("{home}{rest}");
    } else if let Some(rest) = t.strip_prefix("$CLAUDE_CONFIG_DIR") {
        let cfg = std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_else(|_| format!("{home}/.claude"));
        t = format!("{cfg}{rest}");
    } else {
        let c = cwd.trim_end_matches('/');
        if c.is_empty() {
            return None;
        }
        t = format!("{c}/{t}");
    }
    Some(physicalize(Path::new(&t)))
}

/// ABS with its nearest existing ancestor resolved physically, so a symlinked
/// config dir compares equal to the path a command writes.
pub(crate) fn physicalize(abs: &Path) -> PathBuf {
    let mut tail = PathBuf::new();
    let mut dir = abs.to_path_buf();
    loop {
        if dir.is_dir() {
            break;
        }
        match (dir.parent(), dir.file_name()) {
            (Some(parent), Some(name)) if parent != dir => {
                // Join onto an empty tail with the bare name: `join("")`
                // appends a separator, and a trailing slash here would
                // survive into every comparison downstream (exists(), the
                // depth checks) and flip them.
                if tail.as_os_str().is_empty() {
                    tail = PathBuf::from(name);
                } else {
                    tail = Path::new(name).join(tail);
                }
                dir = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let real = std::fs::canonicalize(&dir).unwrap_or(dir);
    // `join("")` appends a trailing separator, which would break every
    // equality and depth comparison downstream; an empty tail returns the
    // canonical ancestor as-is.
    if tail.as_os_str().is_empty() {
        real
    } else {
        real.join(tail)
    }
}

/// The session's job tmp dir under a namespace. The payload's own session
/// outranks an inherited CLAUDE_JOB_DIR: an exported env can belong to a
/// different session than the one gated.
fn jobdir_for(cfg: &str, session_id: &str) -> String {
    if !session_id.is_empty() {
        let short: String = session_id.chars().take(8).collect();
        format!("{cfg}/jobs/{short}/tmp")
    } else if let Ok(jd) = std::env::var("CLAUDE_JOB_DIR") {
        format!("{}/tmp", jd.trim_end_matches('/'))
    } else {
        format!("{cfg}/jobs/<session-id>/tmp")
    }
}

fn keeplisted(name: &str) -> bool {
    matches!(
        name,
        "settings.json" | "settings.local.json" | "keybindings.json" | "CLAUDE.md"
    ) || name.starts_with(".claude.json")
}

fn ambient_cfg() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{}/.claude", home.trim_end_matches('/'))
}

fn ambient_state() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{}/.fno", home.trim_end_matches('/'))
}

fn config_roots() -> (Vec<Root>, Root) {
    let ambient = ambient_cfg();
    let mut cfgs = vec![ambient.clone()];
    if let Ok(cfg) = std::env::var("CLAUDE_CONFIG_DIR") {
        let cfg = cfg.trim_end_matches('/').to_string();
        if cfg != ambient {
            cfgs.push(cfg);
        }
    }
    let state = std::env::var("FNO_STATE_DIR")
        .or_else(|_| std::env::var("FNO_HOME"))
        .unwrap_or_else(|_| ambient_state());
    let state = state.trim_end_matches('/').to_string();
    let roots = cfgs
        .iter()
        .map(|c| Root {
            spelled: c.clone(),
            physical: physicalize(Path::new(c)),
        })
        .collect();
    let state_root = Root {
        spelled: state.clone(),
        physical: physicalize(Path::new(&state)),
    };
    (roots, state_root)
}

/// True when ABS sits directly inside a guarded root (or is the root itself),
/// outside the keep-list; or when ABS would CREATE a new top-level entry in
/// the state root. The refusal names a job tmp dir under the VIOLATED
/// namespace, so an isolated-account write is never pointed at the ambient one.
fn refuse_for(abs: &Path, roots: &[Root], state: &Root, session_id: &str) -> Option<String> {
    let Some(phys) = abs.to_str() else {
        return None;
    };
    let name = abs
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    for root in roots {
        let rp = root.physical.to_string_lossy();
        let rp = rp.trim_end_matches('/');
        // Exactly one extra segment: the shell guard's `$phys/*` matched any
        // depth (bash [[ == ]] stars cross slashes) and `$phys/*/*` then
        // excluded two or more, so only DIRECT children count as top-level.
        // A literal `"{rp}/*/"` prefix never matches a real path, so the
        // exclusion must be a segment count, not a string compare.
        let under = phys.strip_prefix(&format!("{rp}/"));
        let top_level =
            phys == rp || under.is_some_and(|rest| !rest.is_empty() && !rest.contains('/'));
        if top_level {
            if keeplisted(&name) {
                return None;
            }
            let jd = jobdir_for(&root.spelled, session_id);
            return Some(format!(
                "{phys} is a write directly inside the Claude config dir ({}). Scratch belongs in this session's job dir: {jd}. Subdirectories (jobs/, projects/, plugins/) stay allowed.",
                root.spelled
            ));
        }
    }
    let sp = state
        .physical
        .to_string_lossy()
        .trim_end_matches('/')
        .to_string();
    if phys == sp {
        let jd = jobdir_for(&ambient_cfg(), session_id);
        return Some(format!(
            "{phys} is the fno state root itself; a copy there lands a new top-level file. State belongs in a named subfolder under it, or this session's job dir: {jd}."
        ));
    }
    let under_state = phys.strip_prefix(&format!("{sp}/"));
    let state_top_level = under_state.is_some_and(|rest| !rest.is_empty() && !rest.contains('/'));
    if state_top_level && !abs.exists() {
        let jd = jobdir_for(&ambient_cfg(), session_id);
        return Some(format!(
            "{phys} would create a new top-level entry in the fno state root ({}). Nothing writes at the top level: put state in a named subfolder under it, or this session's job dir: {jd}.",
            state.spelled
        ));
    }
    None
}

fn config_section(p: &Payload, _cwd: &Path) -> Sec {
    // Fail-open on an unparsable payload that still named a token (the
    // header's contract): an unreadable payload is never a refusal here.
    if p.tool.is_empty() {
        return Sec::Allow;
    }
    let (roots, state) = config_roots();
    let sd = &p.session_id;
    match p.tool.as_str() {
        "Edit" | "Write" => {
            if p.file_path.is_empty() {
                return Sec::Allow;
            }
            let Some(abs) = abs_of(&p.file_path, &p.cwd) else {
                return Sec::Allow;
            };
            match refuse_for(&abs, &roots, &state, sd) {
                Some(reason) => Sec::Block(reason),
                None => Sec::Allow,
            }
        }
        "Bash" => {
            if p.command.is_empty() {
                return Sec::Allow;
            }
            config_bash_arm(&p.cmd_norm, &p.cwd, &roots, &state, sd)
        }
        _ => Sec::Allow,
    }
}

/// The Bash arm: the same operator families graph-write-protect enumerates,
/// with runtime-resolved path arms. Every family extracts the pp/rpp OUTER
/// group: the full path token, base plus optional `/name`, one index past the
/// lead/operator groups.
fn config_bash_arm(
    cmd: &str,
    payload_cwd: &str,
    roots: &[Root],
    state: &Root,
    session_id: &str,
) -> Sec {
    let home_esc = regex::escape(
        &std::env::var("HOME")
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string(),
    );
    let mut arms: Vec<String> = vec![
        r"\$CLAUDE_CONFIG_DIR".into(),
        r"\$HOME/\.claude".into(),
        r"~/\.claude".into(),
        format!("{home_esc}/\\.claude"),
    ];
    if let Ok(cfg) = std::env::var("CLAUDE_CONFIG_DIR") {
        arms.push(regex::escape(cfg.trim_end_matches('/')));
    }
    let path_arm = arms.join("|");
    let name_cls = "[^[:space:];|&<>)]+";
    let pp = format!(r#"(({path_arm})(/{name_cls})?)($|[/[:space:];|&:)])"#);
    let rpp = format!(r#"((\./)?{name_cls})($|[/[:space:];|&:)])"#);
    let op = r"([>]{1,2}|&[>]|[>]&|[>][|]|[>]!)";
    let lead = r"(^|[^[:alnum:]_])";

    let mut families: Vec<(regex::Regex, usize)> = Vec::new();
    // Redirects, absolute arms then relative arms.
    families.push((
        regex::Regex::new(&format!(r"{op}[[:space:]]*{pp}")).unwrap(),
        2,
    ));
    families.push((
        regex::Regex::new(&format!(r"{op}[[:space:]]*{rpp}")).unwrap(),
        2,
    ));
    // tee / sponge, with flags.
    families.push((
        regex::Regex::new(&format!(
            r"{lead}(tee|sponge)[[:space:]]+(-[^[:space:]]+[[:space:]]+)*{pp}"
        ))
        .unwrap(),
        4,
    ));
    families.push((
        regex::Regex::new(&format!(
            r"{lead}(tee|sponge)[[:space:]]+(-[^[:space:]]+[[:space:]]+)*{rpp}"
        ))
        .unwrap(),
        4,
    ));
    // cp / mv / install / truncate with the path as the (last) argument.
    families.push((
        regex::Regex::new(&format!(
            r"{lead}(cp|mv|install|truncate)[[:space:]].*[[:space:]]{pp}"
        ))
        .unwrap(),
        3,
    ));
    families.push((
        regex::Regex::new(&format!(
            r"{lead}(cp|mv|install|truncate)[[:space:]].*[[:space:]]{rpp}"
        ))
        .unwrap(),
        3,
    ));
    // dd of=.
    families.push((
        regex::Regex::new(&format!(r"{lead}dd[[:space:]].*of={pp}")).unwrap(),
        2,
    ));
    families.push((
        regex::Regex::new(&format!(r"{lead}dd[[:space:]].*of={rpp}")).unwrap(),
        2,
    ));
    // In-place editors, clause-bounded.
    families.push((
        regex::Regex::new(&format!(
            r"{lead}(sed|perl|jq|ex|ed)[[:space:]][^;|&]*(-[a-zA-Z]*i|--in-place)[^;|&]*{pp}"
        ))
        .unwrap(),
        4,
    ));
    families.push((
        regex::Regex::new(&format!(
            r"{lead}(sed|perl|jq|ex|ed)[[:space:]][^;|&]*(-[a-zA-Z]*i|--in-place)[^;|&]*{rpp}"
        ))
        .unwrap(),
        4,
    ));

    for (re, grp) in &families {
        if let Some(caps) = re.captures(cmd) {
            let token = caps.get(*grp).map(|m| m.as_str()).unwrap_or("");
            if let Some(abs) = abs_of(token, payload_cwd) {
                if let Some(reason) = refuse_for(&abs, roots, state, session_id) {
                    return Sec::Block(reason);
                }
            }
        }
    }
    Sec::Allow
}

// ---------------------------------------------------------------------------
// Section C: generated copies (generated-write-guard.sh)
// ---------------------------------------------------------------------------

/// The paths one call writes: file_path first, then the Bash write-form
/// targets (or the apply_patch header paths). Paths come back as written.
fn write_targets(file_path: &str, command: &str) -> Vec<String> {
    let mut out = Vec::new();
    if !file_path.is_empty() {
        out.push(file_path.to_string());
    }
    if command.is_empty() {
        return out;
    }
    if command.contains("*** Begin Patch") {
        for line in command.lines() {
            for prefix in [
                "*** Add File: ",
                "*** Update File: ",
                "*** Delete File: ",
                "*** Move to: ",
            ] {
                if let Some(rest) = line.strip_prefix(prefix) {
                    let path = rest.trim_end_matches('\r');
                    if !path.is_empty() {
                        out.push(path.to_string());
                    }
                }
            }
        }
        // A patch BODY is file content, not shell: extracting write forms from
        // it would block a legitimate patch whose added text merely mentions a
        // redirect. Only a real Bash command goes through the shell grammar.
        return out;
    }
    bash_write_targets(command, &mut out);
    out
}

fn bash_write_targets(cmd: &str, out: &mut Vec<String>) {
    if cmd.is_empty() {
        return;
    }
    let tok = r#"("[^"]*"|'[^']*'|[^[:space:];|&<>"']+)"#;
    // Redirects read off the raw command; the positional forms read off a copy
    // with redirect clauses stripped, so the last argument of a cp is not a
    // trailing 2>/dev/null.
    let strip_fd_dups = regex::Regex::new(r"[0-9]*>&[0-9]+").unwrap();
    let strip_redirs = regex::Regex::new(&format!(
        r"(^|[[:space:]])(([0-9]*>>?)|&>|>\||>!)[[:space:]]*{tok}"
    ))
    .unwrap();
    let fd_dup_stripped = strip_fd_dups.replace_all(cmd, "");
    let clean = strip_redirs.replace_all(&fd_dup_stripped, "${1}");
    let mut push = |m: Option<regex::Match<'_>>| {
        if let Some(m) = m {
            let mut frag = m.as_str().to_string();
            let n = frag.len();
            if n >= 2
                && ((frag.starts_with('"') && frag.ends_with('"'))
                    || (frag.starts_with('\'') && frag.ends_with('\'')))
            {
                frag = frag[1..n - 1].to_string();
            }
            if !frag.is_empty() {
                out.push(frag);
            }
        }
    };
    let raw_families: Vec<(regex::Regex, usize)> = vec![
        (
            regex::Regex::new(&format!(r"[0-9]*(>>|>)[[:space:]]*{tok}")).unwrap(),
            2,
        ),
        (
            regex::Regex::new(&format!(r"(&>|>\||>!)[[:space:]]*{tok}")).unwrap(),
            2,
        ),
        (
            regex::Regex::new(&format!(
                r"(^|[^[:alnum:]_])tee[[:space:]]+(-[^[:space:]]+[[:space:]]+)*{tok}"
            ))
            .unwrap(),
            3,
        ),
        (
            regex::Regex::new(&format!(r"(^|[^[:alnum:]_])sponge[[:space:]]+{tok}")).unwrap(),
            2,
        ),
    ];
    let clean_families: Vec<(regex::Regex, usize)> = vec![
        (
            regex::Regex::new(&format!(
                r"(^|[^[:alnum:]_])(cp|mv|install|truncate)[[:space:]][^;|&]*[[:space:]]{tok}"
            ))
            .unwrap(),
            3,
        ),
        (
            regex::Regex::new(&format!(r"(^|[^[:alnum:]_])dd[[:space:]][^;|&]*of={tok}")).unwrap(),
            2,
        ),
        (
            regex::Regex::new(&format!(
                r"(^|[^[:alnum:]_])(sed|perl)[[:space:]][^;|&]*(-[a-zA-Z]*i|--in-place)[^;|&]*[[:space:]]{tok}"
            ))
            .unwrap(),
            4,
        ),
        (
            regex::Regex::new(&format!(
                r"(^|[^[:alnum:]_])jq[[:space:]][^;|&]*(-i|--in-place)[^;|&]*[[:space:]]{tok}"
            ))
            .unwrap(),
            3,
        ),
        (
            regex::Regex::new(&format!(r"(^|[^[:alnum:]_])(ex|ed)[[:space:]][^;|&]*[[:space:]]{tok}"))
                .unwrap(),
            3,
        ),
    ];
    // Every matching clause reports, not just the leftmost, so a second write
    // in a compound command is still seen.
    for (re, grp) in &raw_families {
        for caps in re.captures_iter(cmd) {
            push(caps.get(*grp));
        }
    }
    for (re, grp) in &clean_families {
        for caps in re.captures_iter(&clean) {
            push(caps.get(*grp));
        }
    }
}

fn generated_section(p: &Payload, _cwd: &Path) -> Sec {
    let targets = write_targets(&p.file_path, &p.command);
    trace_gen(&format!("targets={targets:?}"));
    if targets.is_empty() {
        return Sec::Allow;
    }
    for t in &targets {
        if let Some(reason) = generated_target(t, &p.cwd) {
            return Sec::Block(reason);
        }
    }
    Sec::Allow
}

fn trace_gen(msg: &str) {
    if std::env::var_os("FNO_GUARD_TRACE").is_some() {
        eprintln!("write-gate: generated {msg}");
    }
}

fn generated_target(t: &str, payload_cwd: &str) -> Option<String> {
    let abs = if t.starts_with('/') {
        PathBuf::from(t)
    } else {
        if payload_cwd.is_empty() {
            return None;
        }
        PathBuf::from(format!("{}/{}", payload_cwd.trim_end_matches('/'), t))
    };
    let abs = physicalize(&abs);
    let abs_str = abs.to_string_lossy().into_owned();

    // Installed plugin copies: the path under the installed prefix is the
    // source-checkout path to edit instead.
    let plugin_msg = |inner: String| {
        format!(
            "{abs_str} is the installed plugin copy. `fno doctor update` restages it from the footnote source checkout and discards this edit. Edit {inner} in a feature worktree of the source checkout, then run `fno doctor update`."
        )
    };
    if let Some(i) = abs_str.find("/plugin-stage/fno/") {
        return Some(plugin_msg(
            abs_str[i + "/plugin-stage/fno/".len()..].to_string(),
        ));
    }
    if let Some(i) = abs_str.find("/plugins/cache/footnote") {
        if let Some(j) = abs_str[i..].find("/fno/") {
            return Some(plugin_msg(abs_str[i + j + "/fno/".len()..].to_string()));
        }
    }

    // Repo manifests live at the git toplevel the target sits in. The target
    // is usually a file that does not exist yet, so walk up to the nearest
    // existing ancestor before asking git, exactly as the shell guard did.
    let mut dir = abs
        .parent()
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/"));
    while !dir.is_dir() {
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent.to_path_buf(),
            _ => break,
        }
    }
    let out = match std::process::Command::new("git")
        .args(["-C", &dir.to_string_lossy(), "rev-parse", "--show-toplevel"])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            trace_gen(&format!("git spawn failed for {}: {e}", dir.display()));
            return None;
        }
    };
    if !out.status.success() {
        trace_gen(&format!(
            "git rev-parse failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
        return None;
    }
    let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if root.is_empty() {
        return None;
    }
    let prefix = format!("{root}/");
    if !abs_str.starts_with(&prefix) {
        return None;
    }
    let rel = abs_str[prefix.len()..].to_string();
    trace_gen(&format!("root={root} rel={rel}"));

    // generated-artifacts.tsv rows: tab-separated glob, source, regen.
    let manifest = Path::new(&root).join("generated-artifacts.tsv");
    if manifest.is_file() {
        if let Ok(rows) = std::fs::read_to_string(&manifest) {
            for line in rows.lines() {
                let fields: Vec<&str> = line.split('\t').collect();
                if fields.len() < 3 || fields[0].is_empty() || fields[0].starts_with('#') {
                    continue;
                }
                if bash_glob_match(fields[0], &rel) {
                    let (source, regen) = (fields[1], fields[2]);
                    return Some(format!(
                        "{rel} is generated from {source}. The next `{regen}` run overwrites an edit here. Edit {source}, then run `{regen}`."
                    ));
                }
            }
        }
    }

    // Bundle copies: rows from skill-bundles.yaml via the shipped parser.
    if (rel.starts_with("skills/") || rel.starts_with("agents/"))
        && Path::new(&root).join("skill-bundles.yaml").is_file()
        && Path::new(&root)
            .join("scripts/lib/parse-bundle-manifest.py")
            .is_file()
    {
        let rows = load_bundle_rows(&root);
        for row in &rows {
            let fields: Vec<&str> = row.split('\t').collect();
            if fields.len() < 4 || fields[0].is_empty() {
                continue;
            }
            let out_path = if fields[0].starts_with("pack-") {
                fields[3].to_string()
            } else {
                format!("skills/{}/{}", fields[1], fields[3])
            };
            if rel == out_path || rel.starts_with(&format!("{out_path}/")) {
                let src = format!("{}{}", fields[2], &rel[out_path.len()..]);
                return Some(format!(
                    "{rel} is a bundled copy of {src} (skill-bundles.yaml). The next `bash scripts/generate-skill-bundles.sh` run overwrites an edit here. Edit {src}, then run `bash scripts/generate-skill-bundles.sh`."
                ));
            }
        }
    }
    None
}

fn load_bundle_rows(root: &str) -> Vec<String> {
    let parser = format!("{root}/scripts/lib/parse-bundle-manifest.py");
    let run = |interp: &str, pre: &[&str]| -> Option<String> {
        let mut c = std::process::Command::new(interp);
        c.args(pre)
            .arg(&parser)
            .arg("skill-bundles.yaml")
            .current_dir(root);
        let out = c.output().ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            None
        }
    };
    match run("python3", &[]) {
        Some(s) => s.lines().map(str::to_string).collect(),
        None => match run(
            "uv",
            &["run", "--no-project", "--with", "pyyaml", "python3"],
        ) {
            Some(s) => s.lines().map(str::to_string).collect(),
            None => {
                eprintln!(
                    "generated-write-guard: could not parse skill-bundles.yaml; bundle copies are not guarded for this call"
                );
                Vec::new()
            }
        },
    }
}

/// Bash `[[ str == pattern ]]`: `*`, `?`, `[set]` with `!`/`^` negation and
/// ranges, backslash escapes. Full-string match.
fn bash_glob_match(pat: &str, s: &str) -> bool {
    glob_impl(pat.as_bytes(), s.as_bytes())
}

fn glob_impl(p: &[u8], s: &[u8]) -> bool {
    if p.is_empty() {
        return s.is_empty();
    }
    match p[0] {
        b'*' => {
            for i in 0..=s.len() {
                if glob_impl(&p[1..], &s[i..]) {
                    return true;
                }
            }
            false
        }
        b'?' => !s.is_empty() && glob_impl(&p[1..], &s[1..]),
        b'[' => {
            if s.is_empty() {
                return false;
            }
            let mut i = 1;
            let negate = matches!(p.get(1), Some(b'!') | Some(b'^'));
            if negate {
                i += 1;
            }
            let mut matched = false;
            let mut first = true;
            while i < p.len() && (p[i] != b']' || first) {
                first = false;
                if p[i] == b']' {
                    break;
                }
                let lo = p[i];
                if p.get(i + 2) == Some(&b'-') && p.get(i + 3).is_some() && p[i + 3] != b']' {
                    if s[0] >= lo && s[0] <= p[i + 3] {
                        matched = true;
                    }
                    i += 4;
                } else {
                    if s[0] == lo {
                        matched = true;
                    }
                    i += 1;
                }
            }
            if i >= p.len() {
                return false;
            }
            if negate == matched {
                return false;
            }
            glob_impl(&p[i + 1..], &s[1..])
        }
        b'\\' => p.len() > 1 && !s.is_empty() && p[1] == s[0] && glob_impl(&p[2..], &s[1..]),
        c => !s.is_empty() && s[0] == c && glob_impl(&p[1..], &s[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn norm_folds_separators_in_one_pass_like_jq() {
        assert_eq!(norm("a//b"), "a/b");
        assert_eq!(norm("a/./b"), "a/b");
        assert_eq!(norm(".fno/./target-state.md"), ".fno/target-state.md");
        assert_eq!(norm("~/.fno//graph.json"), "~/.fno/graph.json");
    }

    #[test]
    fn graph_bash_arm_closes_the_write_operators() {
        assert!(bash_targets_protected("echo x >> .fno/target-state.md"));
        assert!(bash_targets_protected("jq '.x=1' -i ~/.fno/graph.json"));
        assert!(bash_targets_protected("cp staged ~/.fno/graph.db-shm"));
        assert!(bash_targets_protected(
            "sed --in-place s/a/b/ ~/.fno/graph.json"
        ));
        assert!(bash_targets_protected("sed -Ei s/a/b/ ~/.fno/graph.json"));
        // Reads and bare mentions stay allowed.
        assert!(!bash_targets_protected("cat ~/.fno/graph.json | jq .nodes"));
        assert!(!bash_targets_protected(
            "echo see .fno/graph.json; sed -i s/a/b/ notes.md"
        ));
        assert!(!bash_targets_protected(
            "grep .fno/target-state.md docs.md && jq -i . notes.json"
        ));
        assert!(!bash_targets_protected(
            "cp ~/.fno/graph.json /tmp/backup.json"
        ));
    }

    #[test]
    fn config_toml_hand_writes_refuse_and_reads_pass() {
        let call = |tool: &str, file_path: &str, command: &str| Payload {
            raw: String::new(),
            tool: tool.to_string(),
            file_path: file_path.to_string(),
            command: command.to_string(),
            cwd: String::new(),
            session_id: String::new(),
            fp_norm: norm(file_path),
            cmd_norm: norm(command),
        };
        // (tool, file_path, command, refused). Fixtures, reads, other config
        // files and the verb itself stay allowed.
        let cases = [
            ("Edit", "/Users/x/.fno/config.toml", "", true),
            ("Write", "/repo/.fno//config.toml", "", true),
            ("Bash", "", "echo '[store]' >> ~/.fno/config.toml", true),
            ("Bash", "", "sed -i s/a/b/ \"$HOME/.fno/config.toml\"", true),
            ("Write", "/repo/tests/x/.fno/config.toml", "", false),
            ("Edit", "/repo/.cargo/config.toml", "", false),
            ("Bash", "", "cat ~/.fno/config.toml", false),
            ("Bash", "", "fno config set store.share_backlog true", false),
            ("apply_patch", "", "*** Begin Patch\n*** Update File: .fno/config.toml\n@@\n+x = 1\n*** End Patch", true),
            ("apply_patch", "", "*** Begin Patch\n*** Update File: docs/a.md\n@@\n+see .fno/config.toml\n*** End Patch", false),
        ];
        for (tool, file_path, command, refused) in cases {
            let got = config_toml_write(&call(tool, file_path, command));
            assert_eq!(got, refused, "{tool} {file_path}{command}");
        }
        // The state-file arm reads the same as before the matcher took a path.
        assert!(bash_targets_protected(
            "echo x > ~/.fno/spaces/s/worktrees/w/target-state.md"
        ));
        assert!(!bash_targets_protected("echo x > ~/.fno/config.toml"));
    }

    #[test]
    fn write_targets_and_glob_rows_match_the_harness_patterns() {
        let mut v = Vec::new();
        bash_write_targets("cp docs/gen.src docs/gen.md 2>/dev/null", &mut v);
        assert!(v.contains(&"docs/gen.md".to_string()));
        let patch =
            "*** Begin Patch\n*** Update File: .codex/agents/archer.toml\n@@\n*** End Patch";
        assert_eq!(write_targets("", patch), vec![".codex/agents/archer.toml"]);
        // The generated-artifacts.tsv rows are bash [[ == ]] glob patterns:
        // a pattern star crosses slashes there, unlike pathname expansion.
        assert!(bash_glob_match("docs/gen.md", "docs/gen.md"));
        assert!(bash_glob_match(
            ".codex/agents/*.toml",
            ".codex/agents/archer.toml"
        ));
        assert!(bash_glob_match("docs/*.md", "docs/a/b.md"));
        assert!(!bash_glob_match("docs/*.md", "notes.md"));
        assert!(bash_glob_match("a?c", "abc"));
        assert!(bash_glob_match("[abc]x", "bx"));
        assert!(bash_glob_match("[!abc]x", "dx"));
    }
}
