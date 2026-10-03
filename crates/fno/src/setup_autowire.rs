//! `fno config setup auto-wire`: wire the fno plugin into every agent CLI on
//! PATH, no questions asked. `fno.sh` runs it right after a verified install,
//! so the CLI and the plugin arrive together and the first agent session
//! starts whole. The wizard (`fno config setup wizard`) stays for anyone who
//! wants to choose.
//!
//! Per-harness semantics mirror cli/src/fno/setup/integration.py, which stays
//! the wizard's engine. opencode, pi and agy install through the same
//! fno-agents doors the Python adapters call. Codex stays on the Python
//! converge engine (ship-phase ruling): the front door reaches it through the
//! wheel's fno-py, adding no Python of its own. Every outcome prints; nothing
//! is silent. Best-effort by design: a harness that fails to wire never fails
//! the CLI install that already succeeded, so the verb always exits 0.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::Value;

use crate::process_admission::{std_command, std_spawn_for_human};

/// The marketplace / repo the integrations install from. Mirrors
/// _MARKETPLACE / _REPO_URL in cli/src/fno/setup/integration.py.
const MARKETPLACE: &str = "bllshttng/footnote";
const REPO_URL: &str = "https://github.com/bllshttng/footnote";
/// The skills-dir fallback drop's directory name under ~/.claude/skills.
/// A named constant rather than a literal: the seam-crossings ratchet reads
/// a bare join("fno") as a porcelain resolver site, and this joins a
/// directory, not the binary.
pub(crate) const SKILLS_DROP: &str = "fno";

/// The one argv this verb claims, lexically, before clap: exactly
/// `fno config setup auto-wire`, no flags. Everything else forwards to the
/// Python `config setup` namespace untouched.
pub fn classify(args: &[OsString]) -> Option<()> {
    let words: Vec<&str> = args.iter().filter_map(|a| a.to_str()).collect();
    if words.len() != args.len() {
        return None;
    }
    match words.as_slice() {
        ["config", "setup", "auto-wire"] => Some(()),
        _ => None,
    }
}

/// One harness wiring outcome. `manual` is NOT installed: a step succeeded
/// but the integration needs a human finish, so it never prints as installed.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Installed(String),
    Already(String),
    Manual(String),
    Failed(String),
}

/// The summary line one harness renders. Pure, so the tests pin the wording
/// the wizard's echo side has taught users to expect.
pub fn outcome_line(label: &str, outcome: &Outcome) -> String {
    match outcome {
        Outcome::Installed(note) if note.is_empty() => format!("  {label}: installed"),
        Outcome::Installed(note) => format!("  {label}: installed ({note})"),
        Outcome::Already(note) if note.is_empty() => format!("  {label}: already installed"),
        Outcome::Already(note) => format!("  {label}: already installed ({note})"),
        Outcome::Manual(note) => format!("  {label}: needs a manual finish - {note}"),
        Outcome::Failed(note) => format!("  {label}: FAILED ({note})"),
    }
}

/// A captured subprocess: exit code plus both streams, lossy-UTF-8. `Err`
/// carries the command and its stderr so a summary line names what failed.
type Run<'a> = &'a dyn Fn(&[&str]) -> Result<String, String>;

fn real_run(argv: &[&str]) -> Result<String, String> {
    let mut cmd = std_command(argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = std_spawn_for_human(&mut cmd)
        .and_then(|child| child.wait_with_output())
        .map_err(|e| format!("{}: {e}", argv[0]))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let mut detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if detail.is_empty() {
        detail = String::from_utf8_lossy(&out.stdout).trim().to_string();
    }
    Err(format!(
        "`{}` exited {}: {detail}",
        argv.join(" "),
        out.status.code().unwrap_or(-1)
    ))
}

/// Run the wheel Python engine with `code`, returning trimmed stdout. `None`
/// when fno-py is absent or the snippet fails: every caller degrades to a
/// named failed/manual line rather than guessing.
fn py_eval(code: &str) -> Option<String> {
    let py = crate::bootstrap::resolved_python_script()?;
    let mut cmd = std_command(py);
    cmd.arg("-c")
        .arg(code)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = std_spawn_for_human(&mut cmd)
        .ok()?
        .wait_with_output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

fn on_path(name: &str) -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).any(|dir| {
        std::fs::metadata(dir.join(name))
            .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
            .unwrap_or(false)
    })
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

// --- claude -----------------------------------------------------------------

/// Whether `claude plugin list --json` output names a footnote plugin: rows
/// carry an "id" of the form "<plugin>@<marketplace>", so any id starting
/// "fno@" is ours (mirrors _claude_is_installed in integration.py).
pub fn claude_list_has_fno(list_json: &str) -> bool {
    serde_json::from_str::<Value>(list_json)
        .ok()
        .and_then(|v| v.as_array().map(|rows| rows.iter().any(id_starts_fno)))
        .unwrap_or(false)
}

fn id_starts_fno(row: &Value) -> bool {
    row.get("id")
        .and_then(Value::as_str)
        .map(|id| id.starts_with("fno@"))
        .unwrap_or(false)
}

pub fn claude_wire(home: &Path, run: Run) -> Outcome {
    // The skills-dir fallback drop loads as fno@skills-dir; detect it by the
    // plugin manifest it lands.
    let dest = home.join(".claude").join("skills").join(SKILLS_DROP);
    if dest.join(".claude-plugin").join("plugin.json").exists() {
        return Outcome::Already("skills-dir".into());
    }
    if let Ok(list) = run(&["claude", "plugin", "list", "--json"]) {
        if claude_list_has_fno(&list) {
            return Outcome::Already(String::new());
        }
    }
    // Preferred path: marketplace add + plugin install. An old `claude`
    // lacks the `plugin` subcommand entirely; route to skills-dir.
    if run(&["claude", "plugin", "--help"]).is_ok()
        && run(&["claude", "plugin", "marketplace", "add", MARKETPLACE]).is_ok()
        && run(&["claude", "plugin", "install", "fno@footnote"]).is_ok()
    {
        return Outcome::Installed(String::new());
    }
    // A prior clone that failed leaves a non-empty dest without a valid
    // plugin.json; git clone refuses to write into it. Clear it so a re-run
    // recovers.
    if dest.exists() {
        let _ = std::fs::remove_dir_all(&dest);
    }
    match run(&[
        "git",
        "clone",
        "--depth",
        "1",
        REPO_URL,
        &dest.display().to_string(),
    ]) {
        Ok(_) => Outcome::Installed("skills-dir; no `claude plugin update`".into()),
        Err(e) => Outcome::Failed(e),
    }
}

// --- gemini -----------------------------------------------------------------

fn gemini_wire(run: Run) -> Outcome {
    if let Ok(list) = run(&["gemini", "extensions", "list"]) {
        if list.contains("footnote") {
            return Outcome::Already(String::new());
        }
    }
    match run(&["gemini", "extensions", "install", REPO_URL]) {
        Ok(_) => Outcome::Installed(String::new()),
        Err(e) => Outcome::Failed(e),
    }
}

// --- codex ------------------------------------------------------------------

/// The codex adapter, verbatim semantics of integration.py's pair
/// (inspect_freshness -> fresh means already-installed, else converge on the
/// release channel), executed by the wheel's own engine. Lives here because
/// the codex arm stays on the Python converge engine by ruling, and because
/// this file may add no Python to cli/src/fno.
const CODEX_WIRE_PY: &str = r#"import json
from fno.setup.codex_plugin import CodexPluginError, converge, inspect_freshness
out = {"status": "failed", "note": ""}
try:
    if inspect_freshness().get("status") == "fresh":
        out["status"] = "already-installed"
    else:
        r = converge(channel="release")
        out["status"] = "already-installed" if r.action == "no-op" else "installed"
        out["note"] = f"{r.plugin_id} {r.version}; start a new Codex session"
except CodexPluginError as e:
    out["status"] = "failed"
    out["note"] = f"{e.stage}: {e.detail}"
print(json.dumps(out))"#;

/// Map the codex snippet's JSON receipt to an outcome. Pure.
pub fn parse_codex_receipt(out: &str) -> Outcome {
    let Ok(v) = serde_json::from_str::<Value>(out.trim()) else {
        return Outcome::Failed("unreadable receipt from the fno-py engine".into());
    };
    let note = v
        .get("note")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    match v.get("status").and_then(Value::as_str) {
        Some("installed") => Outcome::Installed(note),
        Some("already-installed") => Outcome::Already(note),
        _ => Outcome::Failed(note),
    }
}

fn codex_wire() -> Outcome {
    match py_eval(CODEX_WIRE_PY) {
        Some(out) => parse_codex_receipt(&out),
        None => Outcome::Failed(
            "the fno-py engine is missing or failed; run `fno doctor update`".into(),
        ),
    }
}

// --- opencode ---------------------------------------------------------------

/// The opencode receipt's `status` decides: installed/partial count as
/// installed (mirrors _opencode_install in integration.py). Pure.
pub fn parse_opencode_receipt(out: &str) -> Outcome {
    let Ok(v) = serde_json::from_str::<Value>(out.trim()) else {
        return Outcome::Failed("unreadable install receipt".into());
    };
    let note = format!(
        "{} file(s) (footnote {}) -> {}",
        v.get("written").and_then(Value::as_u64).unwrap_or(0),
        v.get("version").and_then(Value::as_str).unwrap_or("?"),
        v.get("config_dir").and_then(Value::as_str).unwrap_or("?"),
    );
    match v.get("status").and_then(Value::as_str) {
        Some(s) if s == "installed" || s == "partial" => Outcome::Installed(note),
        _ => Outcome::Failed(note),
    }
}

fn opencode_wire(agents: &Path, run: Run) -> Outcome {
    // The flags ride AHEAD of the harness word: a deployed binary older than
    // that change parses the first flag as the mode and refuses, so a stale
    // binary can answer a PROBE with an install, never the reverse.
    if let Ok(out) = run(&[
        agents_str(agents),
        "plugin-install",
        "--installed",
        "--json",
        "opencode",
    ]) {
        if let Ok(v) = serde_json::from_str::<Value>(out.trim()) {
            if v.get("status").and_then(Value::as_str) == Some("installed") {
                return Outcome::Already(String::new());
            }
        }
    }
    match run(&[agents_str(agents), "plugin-install", "--json", "opencode"]) {
        Ok(out) => parse_opencode_receipt(&out),
        Err(e) => Outcome::Failed(e),
    }
}

fn agents_str(agents: &Path) -> &str {
    // The resolved sibling path is valid UTF-8 on every supported host; a
    // non-UTF-8 PATH entry here means the run cannot name the binary anyway.
    agents.to_str().unwrap_or("fno-agents")
}

// --- pi ---------------------------------------------------------------------

const PI_SRC_PY: &str = "from fno.setup.integration import _pi_extension_src as p; print(p())";

fn pi_wire(agents: &Path, src: Option<&str>, run: Run) -> Outcome {
    let Some(src) = src else {
        return Outcome::Failed(
            "the fno-py engine is missing; cannot locate the pi extension source".into(),
        );
    };
    if let Ok(out) = run(&[
        agents_str(agents),
        "plugin-install",
        "pi",
        "--status",
        "--extension-src",
        src,
        "--json",
    ]) {
        if let Ok(v) = serde_json::from_str::<Value>(out.trim()) {
            if v.get("installed").and_then(Value::as_bool) == Some(true) {
                return Outcome::Already(String::new());
            }
        }
    }
    match run(&[
        agents_str(agents),
        "plugin-install",
        "pi",
        "--extension-src",
        src,
        "--json",
    ]) {
        Ok(out) => match serde_json::from_str::<Value>(out.trim()) {
            Ok(v) => {
                let note = v
                    .get("note")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                match v.get("status").and_then(Value::as_str) {
                    Some("installed") => Outcome::Installed(note),
                    Some(other) => Outcome::Failed(format!("{other}: {note}")),
                    None => Outcome::Failed("unreadable install receipt".into()),
                }
            }
            Err(_) => Outcome::Failed("unreadable install receipt".into()),
        },
        Err(e) => Outcome::Failed(format!("{e}; run `fno doctor update --rust`")),
    }
}

// --- agy --------------------------------------------------------------------

/// The three plugin-shipped agy adapters (Stop adapter, crown inject, king
/// guard), one per line, "-" when this CLI-only install carries none (a bare
/// empty line would not survive the trimmed capture). The Stop adapter is
/// load-bearing: without it the wiring degrades to manual.
const AGY_PATHS_PY: &str = r#"from fno.setup.integration import (
    _agy_adapter_path,
    _agy_crown_adapter_path,
    _agy_guard_adapter_path,
)
for p in (_agy_adapter_path(), _agy_crown_adapter_path(), _agy_guard_adapter_path()):
    print(p if p is not None else "-")"#;

pub fn parse_agy_paths(out: &str) -> Option<[String; 3]> {
    let lines: Vec<&str> = out.lines().collect();
    if lines.len() < 3 {
        return None;
    }
    let one = |l: &str| {
        let t = l.trim();
        if t == "-" {
            String::new()
        } else {
            t.to_string()
        }
    };
    Some([one(lines[0]), one(lines[1]), one(lines[2])])
}

fn agy_wire(agents: &Path, paths: Option<[String; 3]>, home: &Path, run: Run) -> Outcome {
    let Some([adapter, crown, guard]) = paths else {
        return Outcome::Failed(
            "the fno-py engine is missing; cannot locate the agy adapters".into(),
        );
    };
    if adapter.is_empty() {
        return Outcome::Manual(
            "adapter ships in the plugin (not this CLI-only install); wire \
             hooks/footnote-agy-target-stop-hook.sh into ~/.gemini/config/hooks.json by hand"
                .into(),
        );
    }
    let hooks_file = home.join(".gemini").join("config").join("hooks.json");
    let hooks = hooks_file.display().to_string();
    // Probe first: a stale fno-agents binary IGNORES unknown flags and would
    // fall through to the old full plugin install. A current one answers
    // --hooks-status with a JSON status object naming the file.
    let probe_ok = run(&[
        agents_str(agents),
        "plugin-install",
        "agy",
        "--hooks-status",
        "--hooks-file",
        &hooks,
        "--json",
    ])
    .ok()
    .and_then(|out| serde_json::from_str::<Value>(out.trim()).ok())
    .map(|v| v.get("file").is_some())
    .unwrap_or(false);
    if !probe_ok {
        return Outcome::Failed(
            "the fno-agents binary does not answer --hooks-status; run `fno doctor update --rust`"
                .into(),
        );
    }
    let mut argv = vec![
        agents_str(agents).to_string(),
        "plugin-install".into(),
        "agy".into(),
        "--hooks".into(),
        "--adapter".into(),
        adapter,
        "--hooks-file".into(),
        hooks,
    ];
    if !crown.is_empty() {
        argv.push("--crown".into());
        argv.push(crown);
    }
    if !guard.is_empty() {
        argv.push("--guard".into());
        argv.push(guard);
    }
    argv.push("--json".into());
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    match run(&refs) {
        Ok(out) => {
            let note = serde_json::from_str::<Value>(out.trim())
                .ok()
                .and_then(|v| v.get("note").and_then(Value::as_str).map(String::from))
                .unwrap_or_else(|| "Stop hook installed".into());
            Outcome::Installed(note)
        }
        Err(e) => Outcome::Failed(e),
    }
}

// --- main -------------------------------------------------------------------

struct Harness {
    name: &'static str,
    label: &'static str,
}

const HARNESS_LIST: [Harness; 6] = [
    Harness {
        name: "claude",
        label: "Claude Code",
    },
    Harness {
        name: "gemini",
        label: "Gemini CLI",
    },
    Harness {
        name: "codex",
        label: "Codex CLI",
    },
    Harness {
        name: "opencode",
        label: "OpenCode",
    },
    Harness {
        name: "pi",
        label: "pi",
    },
    Harness {
        name: "agy",
        label: "Antigravity CLI",
    },
];

/// Detect, wire, print. Always exit 0: the CLI install this follows already
/// succeeded, and every wiring outcome - installed, kept, manual, failed -
/// is named on its own line.
pub fn run() -> i32 {
    let home = home_dir();
    let agents = crate::digest_overlay::fno_agents_bin();
    let agents_ok = std::fs::metadata(&agents)
        .map(|m| m.is_file())
        .unwrap_or(false);
    let mut lines: Vec<String> = Vec::new();
    let mut skipped: Vec<&str> = Vec::new();
    // An unset HOME would resolve the claude/agy config paths against the
    // working directory; refuse those two rather than wire into the CWD.
    let home_missing = home.as_os_str().is_empty();
    let no_home =
        || Outcome::Failed("HOME is not set; cannot resolve the harness config dir".into());
    for h in HARNESS_LIST {
        if !on_path(h.name) {
            skipped.push(h.label);
            continue;
        }
        let outcome = match h.name {
            "claude" if home_missing => no_home(),
            "claude" => claude_wire(&home, &real_run),
            "gemini" => gemini_wire(&real_run),
            "codex" => codex_wire(),
            "opencode" => {
                if agents_ok {
                    opencode_wire(&agents, &real_run)
                } else {
                    Outcome::Failed(
                        "the fno-agents binary is missing; run `fno doctor update`".into(),
                    )
                }
            }
            "pi" => {
                if !agents_ok {
                    Outcome::Failed(
                        "the fno-agents binary is missing; run `fno doctor update`".into(),
                    )
                } else {
                    let src = py_eval(PI_SRC_PY);
                    pi_wire(&agents, src.as_deref(), &real_run)
                }
            }
            _ => {
                if home_missing {
                    no_home()
                } else if !agents_ok {
                    Outcome::Failed(
                        "the fno-agents binary is missing; run `fno doctor update`".into(),
                    )
                } else {
                    let paths = py_eval(AGY_PATHS_PY).and_then(|out| parse_agy_paths(&out));
                    agy_wire(&agents, paths, &home, &real_run)
                }
            }
        };
        lines.push(outcome_line(h.label, &outcome));
    }
    if !skipped.is_empty() {
        lines.push(format!("  skipped (not on PATH): {}", skipped.join(", ")));
    }
    if lines.iter().all(|l| l.starts_with("  skipped")) {
        println!("  no agent CLIs detected on PATH - nothing to wire.");
        return 0;
    }
    println!("wired the fno plugin into your agent CLIs:");
    for line in &lines {
        println!("{line}");
    }
    0
}
