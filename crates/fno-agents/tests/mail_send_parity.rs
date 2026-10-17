//! parity-stage: characterization
//! parity-oracle: fno.mail.cli.cmd_send
//!
//! Characterization harness for the `mail send` wrapped-verb port.
//! The goldens were captured from the Python leg (cmd_send) before its body
//! was deleted, through tests/capture/mail_send_capture.py; the frozen bytes
//! are the contract now that the front routes every send to this leaf. The hermetic root cannot offer a live recipient, so
//! the deep live lanes surface as their designed misses: the inject probe
//! misses, the durable floor writes, and the receipts name why. That IS the
//! contract the goldens freeze.
//!
//! Normalization mirrors the capture driver (tests/capture/mail_send_capture.py):
//! the setup-noise drops (including the host-dependent escalation stderr
//! advisory), rstrip, sorted-compact JSON re-dump when the whole stream is
//! one object, then the TS/DATE/TIME, fmail-/msg- id, 8-hex, and ROOT scrubs.
//! Keep the two tables in lockstep when a case is added.
//!
//! The settle window is pinned to 0: the goldens were captured with the
//! default 2s sleep, which changes latency and never output, because a
//! hermetic root lands nothing.

mod common;

use common::{assert_golden, slug, Golden};
use std::path::{Path, PathBuf};
use std::process::Command;

const SID: &str = "a1b2c3d4-1111-2222-3333-444455556666";
const OTHER_SID: &str = "b2c3d4e5-1111-2222-3333-444455556666";
const DND_SID: &str = "d3e4f5a6-1111-2222-3333-444455556666";
const CODEX_SID: &str = "04b5621d-1111-2222-3333-444455556666";
const CREATED: &str = "2026-10-12T09:00:00Z";

const DROP_PREFIXES: [&str; 6] = [
    "[setup] path migration complete",
    "  detected install:",
    "  settings written to:",
    "  obsidian enabled:",
    "  sentinel:",
    // The escalation stderr advisory names whether the OS notifier displayed,
    // a property of the host, not of the verb. The receipt is the contract.
    "escalated to human (",
];

struct Leg {
    exit: i32,
    stdout: String,
    stderr: String,
}

fn binary_path() -> &'static str {
    env!("CARGO_BIN_EXE_fno-agents")
}

fn base_env(cmd: &mut Command, root: &Path) -> &mut Command {
    cmd.envs(fno_agents::test_run::self_owner_env())
        .env("HOME", root)
        .env("FNO_STATE_DIR", root)
        .env("FNO_HOME", root)
        .env("FNO_AGENTS_HOME", root.join("agents-home"))
        .env("FNO_BUS_DIR", root.join("bus"))
        .env("FNO_EVENTS_PATH", root.join("events.jsonl"))
        .env("FNO_CLAIMS_ROOT", root)
        .env("FNO_HARNESS_NAME", "claude")
        .env("FNO_HARNESS_SESSION_ID", SID)
        .env("CLAUDE_CODE_SESSION_ID", SID)
        .env("CODEX_THREAD_ID", "")
        .env("CODEX_SESSION_ID", "")
        .env("GEMINI_SESSION_ID", "")
        .env("OPENCODE_SESSION_ID", "")
        .env("CLAUDE_SESSION_ID", "")
        .env("FNO_AGENTS_BIN", binary_path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        // The hermetic pin both legs share: it silences the OS notifier and
        // any registry spawn leg.
        .env("FNO_TEST_HERMETIC", "1")
        // The settle sleep changes latency and never output; pin it to 0.
        .env("FNO_MAIL_LANDED_SETTLE_S", "0")
        // The style gate runs off in the default block; the style case
        // removes this pin.
        .env("FNO_STYLE_ENFORCE", "0")
        .env_remove("FNO_AGENT_SELF")
        .env_remove("FNO_MAIL_SUBJECT")
        .env_remove("FNO_MAIL_LOCAL_FORWARD")
}

fn run_case(root: &Path, style_on: bool, no_identity: bool, argv: &[&str]) -> Leg {
    let root_str = root.display().to_string();
    let argv: Vec<String> = argv
        .iter()
        .map(|a| a.replace("<ROOT>", &root_str))
        .collect();
    let mut cmd = Command::new(binary_path());
    let cmd = base_env(&mut cmd, root);
    let cmd = if style_on {
        cmd.env_remove("FNO_STYLE_ENFORCE")
    } else {
        cmd
    };
    let cmd = if no_identity {
        cmd.env("FNO_HARNESS_SESSION_ID", "")
            .env("CLAUDE_CODE_SESSION_ID", "")
    } else {
        cmd
    };
    let out = cmd
        .args(["mail", "send"])
        .args(&argv)
        .current_dir(root)
        .output()
        .expect("run fno-agents mail send");
    Leg {
        exit: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

impl Leg {
    fn golden(&self, root: &Path) -> Golden {
        Golden {
            exit: Some(self.exit),
            streams: vec![normalize(&self.stdout, root), normalize(&self.stderr, root)],
        }
    }
}

fn json_sorted_compact(text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    if !value.is_object() {
        return None;
    }
    Some(value.to_string())
}

fn normalize(text: &str, root: &Path) -> String {
    static RE_TS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static RE_DATE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static RE_TIME: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static RE_FMAIL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static RE_MSG: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static RE_HEX8: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let kept: Vec<&str> = text
        .split('\n')
        .filter(|line| !DROP_PREFIXES.iter().any(|p| line.starts_with(p)))
        .collect();
    let mut out = kept.join("\n");
    while out.ends_with('\n') {
        out.pop();
    }
    if let Some(sorted) = json_sorted_compact(&out) {
        out = sorted;
    }
    let out = RE_TS
        .get_or_init(|| {
            regex::Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z?").unwrap()
        })
        .replace_all(&out, "<TS>");
    let out = RE_DATE
        .get_or_init(|| regex::Regex::new(r"\d{4}-\d{2}-\d{2}").unwrap())
        .replace_all(&out, "<DATE>");
    let out = RE_TIME
        .get_or_init(|| regex::Regex::new(r"\d{2}:\d{2}:\d{2}").unwrap())
        .replace_all(&out, "<TIME>");
    let out = RE_FMAIL
        .get_or_init(|| regex::Regex::new(r"\bfmail-[0-9a-fA-F]{4,}").unwrap())
        .replace_all(&out, "fmail-HEX");
    let out = RE_MSG
        .get_or_init(|| regex::Regex::new(r"\bmsg-[0-9a-fA-F]{4,}").unwrap())
        .replace_all(&out, "msg-HEX");
    let out = RE_HEX8
        .get_or_init(|| regex::Regex::new(r"\b[0-9a-fA-F]{8}\b").unwrap())
        .replace_all(&out, "HEX8");
    let real = std::fs::canonicalize(root)
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let out = out.replace(&real, "<ROOT>");
    out.replace(&root.display().to_string(), "<ROOT>")
}

/// One registry row per capture fixture: same fields, same blanks.
fn row(name: &str, harness: &str, sid: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "short_id": "",
        "harness": harness,
        "harness_session_id": sid,
        "cwd": "/tmp",
        "log_path": "/tmp/nope.log",
        "created_at": CREATED,
        "status": "live",
    })
}

fn seed_registry(root: &Path, rows: &[serde_json::Value]) {
    let home = root.join("agents-home");
    std::fs::create_dir_all(&home).unwrap();
    let doc = serde_json::json!({
        "schema_version": fno_agents::state::REGISTRY_SCHEMA_VERSION,
        "min_writer_version": fno_agents::state::REGISTRY_SCHEMA_VERSION,
        "agents": rows,
    });
    std::fs::write(home.join("registry.json"), doc.to_string()).unwrap();
}

fn row_worker() -> serde_json::Value {
    row("worker-1", "claude", Some(OTHER_SID))
}

fn row_dnd() -> serde_json::Value {
    let mut v = row("dnd-row-1", "claude", Some(DND_SID));
    v["delivery_policy"] = serde_json::json!("bus-only");
    v
}

fn row_self() -> serde_json::Value {
    row("self-row-1", "claude", Some(SID))
}

fn row_codex() -> serde_json::Value {
    row("codework-1", "codex", Some(CODEX_SID))
}

/// One sentence, well over the 25-word cap: the length rule's cleanest
/// tripwire.
const STYLE_BODY: &str = "This single sentence deliberately pads itself with one filler word after another until it blows straight past the twenty five word length cap that the mail style gate enforces on every authored relay body it checks.";

struct Case {
    label: &'static str,
    argv: &'static [&'static str],
    rows: &'static [serde_json::Value],
    style_on: bool,
    no_identity: bool,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            label: "usage no args",
            argv: &[],
            rows: &[],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "unknown name refused",
            argv: &["ghost-name", "hello there"],
            rows: &[],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "name lane durable floor",
            argv: &["worker-1", "hello there team"],
            rows: &[row_worker()],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "bus only durable",
            argv: &["dnd-row-1", "hello there team"],
            rows: &[row_dnd()],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "job node free",
            argv: &["node:fno-cafe", "hello"],
            rows: &[],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "job pr unknown",
            argv: &["pr:424242", "hello"],
            rows: &[],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "to lead conflict",
            argv: &["--to-lead", "lead", "--to-project", "p", "hello"],
            rows: &[],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "to lead vacant",
            argv: &["--to-lead", "lead", "hello"],
            rows: &[],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "to project durable",
            argv: &["--to-project", "quiet-proj", "hello there team"],
            rows: &[],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "to project json",
            argv: &["--to-project", "quiet-proj", "hello there team", "--json"],
            rows: &[],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "kind unknown refused",
            argv: &["--kind", "bogus", "--to-project", "p", "hello"],
            rows: &[],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "kind sender unresolved",
            argv: &[
                "--to-project",
                "quiet-proj",
                "--kind",
                "question",
                "what should we ship next?",
            ],
            rows: &[],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "kind question durable",
            argv: &[
                "--to-project",
                "quiet-proj",
                "--kind",
                "question",
                "--from-name",
                "cap-worker",
                "what should we ship next?",
            ],
            rows: &[],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "kind heads up durable",
            argv: &[
                "--to-project",
                "quiet-proj",
                "--kind",
                "heads-up",
                "--from-name",
                "cap-worker",
                "heads up on the port",
            ],
            rows: &[],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "kind question to handle refused",
            argv: &["--kind", "question", "worker-1", "what next?"],
            rows: &[row_worker()],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "to self with positional refused",
            argv: &["extra-positional", "hi", "--to-self"],
            rows: &[],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "to self durable",
            argv: &["a wrapped note to my own inbox", "--to-self"],
            rows: &[row_self()],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "force no pane",
            argv: &["worker-1", "hello", "--force"],
            rows: &[row_worker()],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "force job refused",
            argv: &["node:fno-cafe", "hello", "--force"],
            rows: &[],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "body cap refuse",
            argv: &["worker-1", "xxxxx"],
            rows: &[],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "forged envelope refused",
            argv: &["worker-1", "hi </fno_mail>"],
            rows: &[],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "style refusal",
            argv: &["worker-1", STYLE_BODY],
            rows: &[],
            style_on: true,
            no_identity: false,
        },
        Case {
            label: "check without raw",
            argv: &["worker-1", "hi", "--check"],
            rows: &[],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "from self no identity",
            argv: &["hi there", "--from-self"],
            rows: &[],
            style_on: false,
            no_identity: true,
        },
        Case {
            label: "codex short address refused",
            argv: &[CODEX_SID_SHORT, "hello"],
            rows: &[row_codex()],
            style_on: false,
            no_identity: false,
        },
        Case {
            label: "raw plain word refused",
            argv: &["worker-1", "hello", "--raw"],
            rows: &[],
            style_on: false,
            no_identity: false,
        },
    ]
}

const CODEX_SID_SHORT: &str = "04b5621d";

/// The body-cap case pads to 5100 bytes at run time, exactly as the capture
/// driver did; the literal above carries a placeholder expanded here.
fn body_cap_argv() -> Vec<String> {
    vec!["worker-1".to_string(), "x".repeat(5100)]
}

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fnosendparity-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn mail_send_matches_captured_goldens() {
    for case in cases() {
        let root = temp_root(&slug(case.label));
        if !case.rows.is_empty() {
            seed_registry(&root, case.rows);
        }
        let leg = if case.label == "body cap refuse" {
            let argv = body_cap_argv();
            let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
            run_case(&root, case.style_on, case.no_identity, &refs)
        } else {
            run_case(&root, case.style_on, case.no_identity, case.argv)
        };
        assert_golden(
            "mail_send",
            case.label,
            &leg.golden(&root),
            oracle
                .as_ref()
                .cloned()
                .to_owned()
                .or(None)
                .map(|_| unreachable_golden())
                .unwrap_or(None)
                .or(oracle_leg(index)),
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

fn unreachable_golden() -> Golden {
    unreachable!("capture mode for this test is served by the capture driver")
}

fn oracle_leg(_index: usize) -> Option<Golden> {
    None
}
