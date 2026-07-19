//! The `Provider` abstraction (design module `provider.rs`, LD8).
//!
//! Provider knowledge lives ONLY here. AgentRelay's mistake was spreading
//! per-CLI logic across `command_parse.rs` + `readiness.rs` + `injection_format.rs`;
//! this trait absorbs all of it into one source-of-truth file. Nothing crosses
//! module boundaries except via the well-defined types in [`crate`].
//!
//! Two traits, per the design:
//!
//! - [`Provider`]: every provider implements it (argv construction, stream-event
//!   parsing, reachability probe).
//! - [`ProviderWithPty`]: the PTY-managed extension (readiness detector,
//!   anti-injection envelope, restart policy). [`Provider::as_pty`] returns
//!   `Option<&dyn ProviderWithPty>` so a non-PTY provider's status is visible in
//!   the type system rather than expressed as no-op impls (LD8 / Rejected
//!   Alternative 7).
//!
//! Phase 6 ships three impls:
//! - [`ClaudeProvider`] — shellout to `claude --bg`; NOT PTY-managed
//!   (`as_pty()` -> `None`). Billing posture LD38: `--bg`, never `claude -p`.
//! - [`CodexProvider`] — full PTY-managed; JSONL stream parser.
//! - [`GeminiProvider`] — full PTY-managed; single JSON blob at EOF (the
//!   structural cleavage from codex established in US4-gemini).
//!
//! Argv shapes mirror the validated US4 Python adapters
//! (`cli/src/fno/agents/providers/{claude,codex,gemini}.py`) so the Rust
//! daemon invokes the CLIs identically to the proven implementations.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::envelope::{Envelope, JsonEnvelope, NoEnvelope};
use crate::readiness::{
    AgyReadinessDetector, CodexReadinessDetector, GeminiReadinessDetector,
    OpencodeReadinessDetector, ReadinessDetector,
};
use crate::supervisor::RestartPolicy;
use crate::ParsedEvent;

/// Inputs for a fresh spawn (`fno agents spawn`).
#[derive(Debug, Clone)]
pub struct CreateContext {
    pub name: String,
    pub message: String,
    pub cwd: PathBuf,
    /// Operator/peer attribution, threaded into the envelope on PTY paths.
    pub from_name: Option<String>,
    /// Caller-assigned session id. codex/gemini accept a pre-assigned UUID;
    /// claude assigns its own short-id (so this is `None` for claude create).
    pub session_id: Option<String>,
    /// Yolo / sandbox-bypass opt-in (codex/gemini); maps to provider-specific
    /// flags. Claude ignores it.
    pub yolo: bool,
    /// Pre-validated provider-native reasoning effort; `None` preserves argv.
    pub reasoning_effort: Option<String>,
    /// Optional system prompt appended at spawn (interactive claude only, the
    /// "sentinel-prompt seam", inside-out-multiplexer E4.1): a relay-targeted
    /// spawn passes the relay sentinel prompt (`RELAY_SYSTEM_PROMPT`, the
    /// bracket-free `RELAY9BEGIN`/`RELAY9END` markers) so its replies are
    /// parseable; a grid-spawned claude passes `None` and runs unsteered. One
    /// PTY, two consumers with different prompts (the seam the design flags).
    pub append_system_prompt: Option<String>,
}

/// Inputs for continuing an existing session (`fno agents ask`).
#[derive(Debug, Clone)]
pub struct ResumeContext {
    pub session_id: String,
    pub message: String,
    pub cwd: PathBuf,
    pub from_name: Option<String>,
    pub yolo: bool,
}

/// Lean registry projection a reachability probe needs. Wave 3's full registry
/// `AgentEntry` is a superset; the fields here are the load-bearing subset for
/// [`Provider::reachability`] and are additive-compatible with the Wave 3 shape.
#[derive(Debug, Clone)]
pub struct AgentEntry {
    pub name: String,
    pub provider: String,
    /// `None` when no session id was ever recorded (e.g. a create that failed
    /// before the id was captured); reachability treats it as inconclusive.
    pub session_id: Option<String>,
    pub cwd: PathBuf,
}

/// Tri-state reachability probe failure (mirrors the Python
/// `ReachabilityProbeError` contract from US4-gemini). An `Err` means the probe
/// was **inconclusive** (store inaccessible, no session id), NOT that the agent
/// is unreachable. Callers MUST NOT flip an agent to `orphaned` on `Err`; they
/// preserve the prior status (Failure Modes / Errors invariant).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("reachability probe inconclusive for provider '{provider}': {reason}")]
pub struct ReachabilityProbeError {
    pub provider: String,
    pub reason: String,
}

impl ReachabilityProbeError {
    pub fn new(provider: &str, reason: impl Into<String>) -> Self {
        ReachabilityProbeError {
            provider: provider.to_string(),
            reason: reason.into(),
        }
    }
}

/// The central per-CLI abstraction. Send + Sync so the daemon can hold trait
/// objects across tasks.
pub trait Provider: Send + Sync {
    /// Stable provider identifier (`"claude"` / `"codex"` / `"gemini"`).
    fn name(&self) -> &'static str;

    /// Argv for a fresh session.
    fn create_argv(&self, ctx: &CreateContext) -> Vec<String>;

    /// Argv for continuing a session.
    fn resume_argv(&self, ctx: &ResumeContext) -> Vec<String>;

    /// Parse one unit of provider stream output into the sealed [`ParsedEvent`]
    /// vocabulary. The unit is provider-shaped: codex is fed one JSONL line at a
    /// time; gemini is fed its complete JSON blob (it emits a single document at
    /// EOF, so per-line feeding yields [`ParsedEvent::Unknown`] until the daemon
    /// has the whole blob). Unrecognized input becomes [`ParsedEvent::Unknown`]
    /// rather than an error, so a provider version bump degrades gracefully.
    fn parse_stream_event(&self, chunk: &str) -> ParsedEvent;

    /// Probe whether `entry`'s session is still reachable. Returns `Ok(true)` /
    /// `Ok(false)` for a definitive answer, `Err(ReachabilityProbeError)` when
    /// the probe is inconclusive. `timeout` bounds any I/O the probe performs.
    fn reachability(
        &self,
        entry: &AgentEntry,
        timeout: Duration,
    ) -> Result<bool, ReachabilityProbeError>;

    /// Downcast to the PTY-managed extension, or `None` for shellout providers
    /// (claude). Marks a provider as PTY-capable; the readiness/envelope/restart
    /// surface hangs off it.
    fn as_pty(&self) -> Option<&dyn ProviderWithPty> {
        None
    }
}

/// PTY-managed extension of [`Provider`]. Implemented by codex / gemini (and
/// Phase 7's OpenCode), NOT by claude.
pub trait ProviderWithPty: Provider {
    /// Per-CLI readiness signal over the terminal grid.
    fn readiness_detector(&self) -> Box<dyn ReadinessDetector>;

    /// Structural anti-injection envelope for input on the PTY stdin path.
    fn envelope(&self) -> Box<dyn Envelope>;

    /// Provider-recommended restart policy. The daemon's enforcer still imposes
    /// the hard ceiling ([`crate::supervisor::HARD_FAILURE_CEILING`], LD36)
    /// regardless of what a provider returns.
    fn default_restart_policy(&self) -> RestartPolicy;
}

// ---------------------------------------------------------------------------
// Claude — shellout, not PTY-managed (LD38 billing: `--bg`, never `-p`).
// ---------------------------------------------------------------------------

/// Claude provider. The daemon shells out to `claude --bg` (the per-user
/// supervisor owns the session) and follows up over the Phase 5 messaging
/// socket. [`as_pty`](Provider::as_pty) returns `None`.
pub struct ClaudeProvider;

/// The stream-json host-lane resume argv for claude adoption (Group 1,
/// ab-5896938c). The daemon builds this to launch the per-session stream worker.
///
/// Unlike [`ClaudeProvider::create_argv`] (which uses `--bg`, the subscription
/// lane), the stream-json host lane REQUIRES `claude -p`: `--input-format
/// stream-json` only works with `--print`/`-p` (Domain Pitfall). Per Locked
/// Decision 1 this is a DELIBERATE, resolved choice - `-p` draws a dedicated
/// Agent SDK credit isolated from interactive limits - so it is NOT an LD38
/// violation but the explicit, opt-in adoption lane. Resume keys on the FULL
/// session UUID (`claude_session_uuid`), never the 8-hex jobId (a 32-bit prefix,
/// not collision-proof). `--include-partial-messages` surfaces streamed tokens;
/// `--replay-user-messages` echoes injected turns back as delivery receipts
/// (the frame parser discriminates the echo from the reply).
pub fn claude_stream_json_resume_argv(session_uuid: &str) -> Vec<String> {
    vec![
        "claude".into(),
        "-p".into(),
        "--resume".into(),
        session_uuid.into(),
        "--input-format".into(),
        "stream-json".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--include-partial-messages".into(),
        "--replay-user-messages".into(),
    ]
}

impl Provider for ClaudeProvider {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn create_argv(&self, ctx: &CreateContext) -> Vec<String> {
        // Mirrors claude.py `_build_argv`: `claude --bg --name <name> <message>`.
        // LD38: `--bg` is the subscription-billed mode; `claude -p` is
        // Agent-SDK-credit-billed and MUST NOT be used.
        vec![
            "claude".into(),
            "--bg".into(),
            "--name".into(),
            ctx.name.clone(),
            ctx.message.clone(),
        ]
    }

    fn resume_argv(&self, ctx: &ResumeContext) -> Vec<String> {
        // Subprocess fallback form (`claude --resume <id> --print <msg>`). The
        // production daemon prefers the Phase 5 messaging-socket poke when a
        // `messaging_socket_path` is registered; this argv exists so the trait
        // is satisfiable without the socket (e.g. tests, socket-unavailable
        // degradation). `--print` is non-streaming (Domain Pitfall).
        vec![
            "claude".into(),
            "--resume".into(),
            ctx.session_id.clone(),
            "--print".into(),
            ctx.message.clone(),
        ]
    }

    fn parse_stream_event(&self, chunk: &str) -> ParsedEvent {
        // `claude --bg` prints a single line like
        // "backgrounded · 7c5dcf5d · <name>"; the only structured datum is the
        // 8-hex short-id, which is the session id. Anything else is Unknown.
        match parse_claude_short_id(chunk) {
            Some(id) => ParsedEvent::SessionCreated { session_id: id },
            None => ParsedEvent::Unknown {
                raw: chunk.to_string(),
            },
        }
    }

    fn reachability(
        &self,
        entry: &AgentEntry,
        _timeout: Duration,
    ) -> Result<bool, ReachabilityProbeError> {
        // Claude liveness is the supervisor's `~/.claude/jobs/<short_id>` dir.
        let short_id = entry
            .session_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ReachabilityProbeError::new("claude", "no session id in entry"))?;
        let jobs = home_dir()
            .ok_or_else(|| ReachabilityProbeError::new("claude", "HOME unset"))?
            .join(".claude")
            .join("jobs");
        if !jobs.exists() {
            // Supervisor never ran / store absent: inconclusive, not "dead".
            return Err(ReachabilityProbeError::new(
                "claude",
                "~/.claude/jobs absent",
            ));
        }
        Ok(jobs.join(short_id).exists())
    }

    // as_pty() uses the default None: claude is not PTY-managed.
}

/// Extract a claude `--bg` short-id (`^[0-9a-f]{8}$`) from a line like
/// "backgrounded · 7c5dcf5d · name". Returns the first 8-hex token found.
fn parse_claude_short_id(line: &str) -> Option<String> {
    // Split on non-hexdigits, so every token is already all-hexdigit; we only
    // need to reject the uppercase-hex case (claude short-ids are lowercase).
    line.split(|c: char| !c.is_ascii_hexdigit())
        .find(|tok| tok.len() == 8 && tok.chars().all(|c| !c.is_ascii_uppercase()))
        .map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Claude (interactive) — PTY-managed, subscription-billed (E1 keystone).
// ---------------------------------------------------------------------------

/// Interactive subscription-billed claude, PTY-hosted by the daemon exactly as
/// codex/gemini are (inside-out-multiplexer E1, the keystone). This is the
/// `ProviderWithPty` counterpart to the shellout [`ClaudeProvider`] and the
/// stream-json lane ([`claude_stream_json_resume_argv`]): the grid tiles it, the
/// relay injects through it, the inside leg reports against it - one PTY, three
/// consumers.
///
/// Billing posture (Locked Decision 2 / D2): the argv is interactive `claude`
/// with `--session-id <uuid>` pinned for transcript discovery + the claim
/// interlock, NEVER `claude -p`/`--print` (that bills the Agent SDK pool). The
/// daemon's billing guard rejects any `-p`/`--print` argv before spawning; this
/// provider only ever emits the interactive form.
pub struct ClaudeInteractiveProvider;

impl ClaudeInteractiveProvider {
    /// Interactive argv with the session id pinned, used by `create_argv`: a
    /// daemon-hosted claude is always interactive, so there is no separate exec
    /// form. `claude --session-id
    /// <uuid> [message]` - the relay-proven vehicle (roundtrip.py pins
    /// `--session-id` at spawn so the transcript is discoverable and the
    /// `session:<uuid>` claim keys on it).
    fn interactive_argv(ctx: &CreateContext) -> Vec<String> {
        let mut argv = vec!["claude".into()];
        if let Some(sid) = ctx.session_id.as_deref().filter(|s| !s.is_empty()) {
            argv.push("--session-id".into());
            argv.push(sid.to_string());
        }
        // Sentinel-prompt seam (E4.1): a relay-targeted spawn steers replies via
        // `--append-system-prompt`; absent, the pane runs unsteered. Pushed before
        // the positional message (which must stay last).
        if let Some(prompt) = ctx
            .append_system_prompt
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            argv.push("--append-system-prompt".into());
            argv.push(prompt.to_string());
        }
        if !ctx.message.is_empty() {
            argv.push(ctx.message.clone());
        }
        argv
    }
}

impl Provider for ClaudeInteractiveProvider {
    fn name(&self) -> &'static str {
        "claude"
    }

    // A daemon-hosted claude has no one-shot exec form; create == interactive.
    // `provider_for_pty` only resolves this provider on the interactive route,
    // so this is a defensive alias rather than a reachable exec path.
    fn create_argv(&self, ctx: &CreateContext) -> Vec<String> {
        Self::interactive_argv(ctx)
    }

    fn resume_argv(&self, ctx: &ResumeContext) -> Vec<String> {
        // Interactive resume: `claude --resume <uuid>` reattaches the session's
        // TUI. The resume id IS the session, so no separate `--session-id` pin.
        let mut argv = vec!["claude".into(), "--resume".into(), ctx.session_id.clone()];
        if !ctx.message.is_empty() {
            argv.push(ctx.message.clone());
        }
        argv
    }

    fn parse_stream_event(&self, chunk: &str) -> ParsedEvent {
        // The interactive TUI emits no JSONL stream (the daemon snapshots the
        // screen via the readiness detector, as for codex/gemini interactive).
        ParsedEvent::Unknown {
            raw: chunk.to_string(),
        }
    }

    fn reachability(
        &self,
        _entry: &AgentEntry,
        _timeout: Duration,
    ) -> Result<bool, ReachabilityProbeError> {
        // Interactive PTY rows are governed by PTY liveness (the worker pid +
        // ConnState), the authoritative signal per D4 - NOT a store scan. Report
        // inconclusive so a caller never false-orphans a live pane on this probe;
        // reconcile uses worker liveness for interactive rows.
        Err(ReachabilityProbeError::new(
            "claude",
            "interactive claude liveness is PTY-governed (pid/ConnState), not store-probed",
        ))
    }

    fn as_pty(&self) -> Option<&dyn ProviderWithPty> {
        Some(self)
    }
}

impl ProviderWithPty for ClaudeInteractiveProvider {
    fn readiness_detector(&self) -> Box<dyn ReadinessDetector> {
        Box::new(crate::readiness::ClaudeReadinessDetector)
    }

    fn envelope(&self) -> Box<dyn Envelope> {
        // No structural anti-injection envelope on the interactive TUI path
        // (input is human keystrokes / send-keys, not a JSON frame). The plain
        // envelope passes text through unchanged, matching the send-keys vehicle.
        Box::new(JsonEnvelope)
    }

    fn default_restart_policy(&self) -> RestartPolicy {
        RestartPolicy::default()
    }
}

// ---------------------------------------------------------------------------
// Codex — full PTY-managed; JSONL stream.
// ---------------------------------------------------------------------------

/// Codex provider. Argv mirrors codex.py (`codex exec --json ...` /
/// `codex exec resume <id> --json ...`); the stream parser is pinned to the
/// codex 0.130 JSONL vocabulary captured in
/// `cli/tests/agents/fixtures/codex-jsonl-sample.jsonl`.
pub struct CodexProvider;

impl CodexProvider {
    fn sandbox_create(yolo: bool) -> Vec<String> {
        // codex.py::sandbox_flag (LD5/LD6): mutually exclusive; never both.
        if yolo {
            vec!["--dangerously-bypass-approvals-and-sandbox".into()]
        } else {
            vec!["--sandbox".into(), "workspace-write".into()]
        }
    }

    fn sandbox_resume(yolo: bool) -> Vec<String> {
        // codex.py::sandbox_flag_resume: resume has no `--sandbox`; only the
        // bypass flag is honored, else inherit the session's original mode.
        if yolo {
            vec!["--dangerously-bypass-approvals-and-sandbox".into()]
        } else {
            vec![]
        }
    }
}

impl Provider for CodexProvider {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn create_argv(&self, ctx: &CreateContext) -> Vec<String> {
        let mut argv = vec![
            "codex".into(),
            "exec".into(),
            "--json".into(),
            "-C".into(),
            ctx.cwd.to_string_lossy().into_owned(),
            // codex exec refuses to run in a non-git dir without this; the
            // validated codex.py create path always passes it.
            "--skip-git-repo-check".into(),
        ];
        argv.extend(Self::sandbox_create(ctx.yolo));
        if let Some(effort) = ctx.reasoning_effort.as_deref().filter(|e| !e.is_empty()) {
            argv.push("-c".into());
            argv.push(format!("model_reasoning_effort={effort}"));
        }
        argv.push(ctx.message.clone());
        argv
    }

    fn resume_argv(&self, ctx: &ResumeContext) -> Vec<String> {
        let mut argv = vec![
            "codex".into(),
            "exec".into(),
            "resume".into(),
            ctx.session_id.clone(),
            "--json".into(),
            "--skip-git-repo-check".into(),
        ];
        argv.extend(Self::sandbox_resume(ctx.yolo));
        argv.push(ctx.message.clone());
        argv
    }

    fn parse_stream_event(&self, chunk: &str) -> ParsedEvent {
        parse_codex_line(chunk)
    }

    fn reachability(
        &self,
        entry: &AgentEntry,
        _timeout: Duration,
    ) -> Result<bool, ReachabilityProbeError> {
        codex_reachable(entry)
    }

    fn as_pty(&self) -> Option<&dyn ProviderWithPty> {
        Some(self)
    }
}

/// Codex reachability via the authoritative session index (mirrors codex.py
/// `load_known_session_ids`). NOT a scan of `~/.codex/sessions/` — those are
/// historical rollout artifacts that persist after a session is removed, so a
/// file-existence scan would report a dead session `Ok(true)` and reconcile
/// would never orphan it (Codex review P1). The index drops the id when the
/// session ends, making membership the real liveness signal.
///
/// Tri-state: index missing -> `Err` (fresh install / can't determine, preserve
/// status, never orphan); index unreadable -> `Err` (inconclusive); index
/// present + id found -> `Ok(true)`; present + absent -> `Ok(false)`.
fn codex_reachable(entry: &AgentEntry) -> Result<bool, ReachabilityProbeError> {
    let sid = entry
        .session_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ReachabilityProbeError::new("codex", "no session id in entry"))?;
    let home = home_dir().ok_or_else(|| ReachabilityProbeError::new("codex", "HOME unset"))?;
    let index = home.join(".codex").join("session_index.jsonl");
    match std::fs::read_to_string(&index) {
        Ok(text) => Ok(text.contains(sid)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ReachabilityProbeError::new(
            "codex",
            format!("session index absent (fresh install?): {}", index.display()),
        )),
        Err(e) => Err(ReachabilityProbeError::new(
            "codex",
            format!("cannot read session index {}: {e}", index.display()),
        )),
    }
}

impl ProviderWithPty for CodexProvider {
    fn readiness_detector(&self) -> Box<dyn ReadinessDetector> {
        Box::new(CodexReadinessDetector)
    }

    fn envelope(&self) -> Box<dyn Envelope> {
        Box::new(JsonEnvelope)
    }

    fn default_restart_policy(&self) -> RestartPolicy {
        RestartPolicy::default()
    }
}

/// Parse one codex JSONL line into a [`ParsedEvent`]. Non-JSON preamble (e.g.
/// "Reading additional input from stdin...") and unrecognized event types map
/// to [`ParsedEvent::Unknown`].
fn parse_codex_line(line: &str) -> ParsedEvent {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return ParsedEvent::Unknown {
            raw: line.to_string(),
        };
    }
    let v: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => {
            return ParsedEvent::Unknown {
                raw: line.to_string(),
            }
        }
    };
    let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match typ {
        "thread.started" => match v.get("thread_id").and_then(|t| t.as_str()) {
            Some(id) => ParsedEvent::SessionCreated {
                session_id: id.to_string(),
            },
            None => ParsedEvent::Unknown {
                raw: line.to_string(),
            },
        },
        "item.started" | "item.completed" => {
            let item = v.get("item");
            let item_type = item
                .and_then(|i| i.get("type"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            match item_type {
                // codex delivers assistant text as discrete agent_message
                // items; the daemon concatenates OutputChunks and treats
                // turn.completed as the reply boundary.
                "agent_message" => ParsedEvent::OutputChunk {
                    text: item
                        .and_then(|i| i.get("text"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string(),
                },
                "error" => ParsedEvent::ProviderError {
                    message: item
                        .and_then(|i| i.get("message"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string(),
                },
                "command_execution" => ParsedEvent::ToolUse {
                    name: "command_execution".to_string(),
                    args: item.cloned(),
                },
                _ => ParsedEvent::Unknown {
                    raw: line.to_string(),
                },
            }
        }
        // Terminal marker for a turn. Reply text arrives via agent_message
        // OutputChunks; codex's usage payload carries no wall-clock duration, so
        // duration_ms is 0 and the daemon fills it from its own timing.
        "turn.completed" => ParsedEvent::ReplyComplete {
            text: String::new(),
            duration_ms: 0,
        },
        // turn.started and any future control frame: not an error, just not a
        // model-facing event.
        _ => ParsedEvent::Unknown {
            raw: line.to_string(),
        },
    }
}

// ---------------------------------------------------------------------------
// Gemini — full PTY-managed; single JSON blob at EOF.
// ---------------------------------------------------------------------------

/// Gemini provider. Argv mirrors gemini.py
/// (`gemini --skip-trust -p <msg> --output-format json --session-id <uuid>` /
/// `... --resume <uuid>`). The parser consumes gemini's single JSON document
/// (pinned to `cli/tests/agents/fixtures/gemini-json-sample.json`).
pub struct GeminiProvider;

impl GeminiProvider {
    fn sandbox(yolo: bool) -> Vec<String> {
        // gemini.py::sandbox_flag (Wave 2.0 OQ5).
        if yolo {
            vec!["--yolo".into()]
        } else {
            vec!["--approval-mode".into(), "default".into()]
        }
    }
}

impl Provider for GeminiProvider {
    fn name(&self) -> &'static str {
        "gemini"
    }

    fn create_argv(&self, ctx: &CreateContext) -> Vec<String> {
        let mut argv = vec![
            "gemini".into(),
            "--skip-trust".into(),
            "-p".into(),
            ctx.message.clone(),
            "--output-format".into(),
            "json".into(),
        ];
        argv.extend(Self::sandbox(ctx.yolo));
        // gemini accepts a caller-assigned session UUID on create.
        if let Some(sid) = ctx.session_id.as_deref() {
            argv.push("--session-id".into());
            argv.push(sid.to_string());
        }
        argv
    }

    fn resume_argv(&self, ctx: &ResumeContext) -> Vec<String> {
        let mut argv = vec![
            "gemini".into(),
            "--skip-trust".into(),
            "-p".into(),
            ctx.message.clone(),
            "--output-format".into(),
            "json".into(),
        ];
        argv.extend(Self::sandbox(ctx.yolo));
        argv.push("--resume".into());
        argv.push(ctx.session_id.clone());
        argv
    }

    fn parse_stream_event(&self, chunk: &str) -> ParsedEvent {
        parse_gemini_blob(chunk)
    }

    fn reachability(
        &self,
        entry: &AgentEntry,
        timeout: Duration,
    ) -> Result<bool, ReachabilityProbeError> {
        gemini_reachable(entry, timeout)
    }

    fn as_pty(&self) -> Option<&dyn ProviderWithPty> {
        Some(self)
    }
}

/// Gemini reachability, cwd-pinned to `~/.gemini/tmp/<cwd-basename>/chats` and
/// matched on the session-id 8-char short prefix (mirrors gemini.py
/// `_gemini_chats_dir` + `gemini_session_reachable`). Gemini's on-disk
/// filenames carry the short prefix, NOT the full UUID, and the store is
/// per-cwd; a full-UUID recursive scan would `Ok(false)` a live session and a
/// caller would false-orphan it. `Err` is inconclusive (chats dir absent can't
/// be distinguished from a fresh install without scanning every cwd).
fn gemini_reachable(entry: &AgentEntry, budget: Duration) -> Result<bool, ReachabilityProbeError> {
    let sid = entry
        .session_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ReachabilityProbeError::new("gemini", "no session id in entry"))?;
    // `.get(..8)` is None for a too-short id OR a non-UTF-8-boundary index 8,
    // so a multibyte session_id can never panic on a hardcoded slice.
    let short = sid.get(..8).ok_or_else(|| {
        ReachabilityProbeError::new(
            "gemini",
            format!("session_id too short or non-char-boundary at 8: {sid:?}"),
        )
    })?;
    let home = home_dir().ok_or_else(|| ReachabilityProbeError::new("gemini", "HOME unset"))?;
    let basename = entry.cwd.file_name().ok_or_else(|| {
        ReachabilityProbeError::new(
            "gemini",
            format!("cwd has no basename: {}", entry.cwd.display()),
        )
    })?;
    let chats_dir = home
        .join(".gemini")
        .join("tmp")
        .join(basename)
        .join("chats");
    if !chats_dir.exists() {
        return Err(ReachabilityProbeError::new(
            "gemini",
            format!("chats dir absent: {}", chats_dir.display()),
        ));
    }
    let deadline = std::time::Instant::now() + budget;
    let dir = std::fs::read_dir(&chats_dir).map_err(|e| {
        ReachabilityProbeError::new("gemini", format!("read_dir {}: {e}", chats_dir.display()))
    })?;
    // Iterate WITHOUT `.flatten()`: a dropped per-entry read error would turn an
    // inconclusive probe into a definitive Ok(false) and false-orphan a live
    // session (Codex P2). Propagate as inconclusive instead.
    for ent in dir {
        if std::time::Instant::now() >= deadline {
            return Err(ReachabilityProbeError::new(
                "gemini",
                "probe budget exceeded before definitive result",
            ));
        }
        let ent = ent.map_err(|e| {
            ReachabilityProbeError::new(
                "gemini",
                format!("dir entry in {}: {e}", chats_dir.display()),
            )
        })?;
        let name = ent.file_name();
        if !name.to_string_lossy().contains(short) {
            continue;
        }
        // Short-prefix match is a candidate only. Verify the FULL UUID appears
        // in the file's first line to defeat short-prefix collisions (Codex P2;
        // mirrors gemini.py's first-line full-UUID check). Read only the first
        // line - chat logs can be multi-MB. I/O errors stay inconclusive.
        let file = std::fs::File::open(ent.path()).map_err(|e| {
            ReachabilityProbeError::new("gemini", format!("open {}: {e}", ent.path().display()))
        })?;
        let mut first_line = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(file), &mut first_line).map_err(
            |e| {
                ReachabilityProbeError::new("gemini", format!("read {}: {e}", ent.path().display()))
            },
        )?;
        if first_line.contains(sid) {
            return Ok(true);
        }
    }
    Ok(false)
}

impl ProviderWithPty for GeminiProvider {
    fn readiness_detector(&self) -> Box<dyn ReadinessDetector> {
        Box::new(GeminiReadinessDetector)
    }

    fn envelope(&self) -> Box<dyn Envelope> {
        Box::new(JsonEnvelope)
    }

    fn default_restart_policy(&self) -> RestartPolicy {
        RestartPolicy::default()
    }
}

// ---------------------------------------------------------------------------
// Agy — PTY-managed pane, PLAIN-TEXT (no JSON envelope, no session id).
// ---------------------------------------------------------------------------

/// Agy (Antigravity CLI) provider. Runs Gemini models under the hood but, unlike
/// gemini, has NO `--output-format json` — it emits plain text. So its envelope
/// is [`NoEnvelope`] (no structural JSON wrapper) and it carries no parseable
/// session id (reachability is always inconclusive; the headless one-shot path
/// lives in `agy_ask.rs`). `-p`/`--print` takes the prompt as its VALUE, so it
/// is appended LAST in every argv.
///
/// x-3ab8 caveat — STATELESS interactive: a plain `spawn --provider agy` now
/// defaults to an owned interactive pane (like the other PTY providers), and the
/// pane is drivable WHILE attached. But because agy mints no session id, there is
/// NO re-attach after it settles — nothing to key a resume on (`resume_argv`
/// below keys on a conversation id agy v1.0.x does not surface). Treat the agy
/// pane as live-only; once it settles, dispatch a fresh `--once` instead.
pub struct AgyProvider;

impl AgyProvider {}

impl Provider for AgyProvider {
    fn name(&self) -> &'static str {
        "agy"
    }

    fn create_argv(&self, ctx: &CreateContext) -> Vec<String> {
        // Headless create is the autonomous lane: ALWAYS never-prompt so an
        // unattended agy cannot wedge on its first approval prompt.
        let mut argv = vec!["agy".into(), "--dangerously-skip-permissions".into()];
        argv.push("-p".into());
        argv.push(ctx.message.clone());
        argv
    }

    fn resume_argv(&self, ctx: &ResumeContext) -> Vec<String> {
        // agy resume keys on the conversation id (`--conversation <id>`); plain
        // -p prompt as the value, last.
        let mut argv = vec![
            "agy".into(),
            "--dangerously-skip-permissions".into(),
            "--conversation".into(),
            ctx.session_id.clone(),
        ];
        argv.push("-p".into());
        argv.push(ctx.message.clone());
        argv
    }

    fn parse_stream_event(&self, chunk: &str) -> ParsedEvent {
        // agy has no structured stream — the whole chunk is the reply text. An
        // empty chunk is Unknown (nothing to surface).
        if chunk.trim().is_empty() {
            ParsedEvent::Unknown {
                raw: chunk.to_string(),
            }
        } else {
            ParsedEvent::ReplyComplete {
                text: chunk.to_string(),
                duration_ms: 0,
            }
        }
    }

    fn reachability(
        &self,
        _entry: &AgentEntry,
        _timeout: Duration,
    ) -> Result<bool, ReachabilityProbeError> {
        // agy carries no easily-probed session store and no parseable session id,
        // so a probe is ALWAYS inconclusive — never false-orphan a live agy pane.
        Err(ReachabilityProbeError::new(
            "agy",
            "agy sessions are not probeable (plain-text, no session store)",
        ))
    }

    fn as_pty(&self) -> Option<&dyn ProviderWithPty> {
        Some(self)
    }
}

impl ProviderWithPty for AgyProvider {
    fn readiness_detector(&self) -> Box<dyn ReadinessDetector> {
        Box::new(AgyReadinessDetector)
    }

    fn envelope(&self) -> Box<dyn Envelope> {
        // Plain-text stdin: no JSON wrapper (agy has no structured input).
        Box::new(NoEnvelope)
    }

    fn default_restart_policy(&self) -> RestartPolicy {
        RestartPolicy::default()
    }
}

// ---------------------------------------------------------------------------
// Opencode — PTY-managed pane, PLAIN-TEXT (v1 hosts the TUI; acp is a future lane).
// ---------------------------------------------------------------------------

/// opencode provider (x-51f6). v1 hosts the interactive PTY-TUI — bare
/// `opencode`, with the message on `--prompt` (the positional is a PROJECT
/// PATH, not a prompt) — mirroring the codex/gemini pane pattern; opencode's
/// structured `acp` protocol is a documented future lane, not wired here.
/// The pane argv itself lives in `mux_spawn.build_pane_argv` (Python owns the
/// pane back half since 4a-G2); the argv forms below are the HEADLESS
/// `opencode run` shapes, carried for trait completeness — no client-side
/// one-shot lane is wired in v1 (`spawn --substrate headless` refuses with a
/// pointer to `pane`). Session ids are captured at spawn, probed against
/// opencode's own store, and resumable (x-830c) — unlike agy, whose rows stay
/// live-only.
/// Trailing argv for an `opencode run` dispatch: route a footnote slash command
/// through `--command`, else pass a prose prompt as the message positional.
///
/// `opencode run <message>` treats a leading-slash string as PROSE - it does NOT
/// expand the plugin command (verified against v1.14.50: `run "/fno:target ..."`
/// starts a model turn on the literal text). The fno opencode plugin registers
/// the footnote verbs, so a rendered `/fno:verb args` must ride `opencode run
/// --command fno:verb <args>` to actually invoke the command (x-de43 / codex P1).
/// A non-slash prompt (a plain `ask`/build message) passes through unchanged.
pub(crate) fn opencode_run_tail(message: &str) -> Vec<String> {
    if let Some(rest) = message.strip_prefix('/') {
        let mut parts = rest.splitn(2, ' ');
        // `/fno:target no-merge x` -> --command fno:target, args "no-merge x".
        if let Some(cmd) = parts.next().filter(|c| !c.is_empty()) {
            let mut tail = vec!["--command".to_string(), cmd.to_string()];
            if let Some(args) = parts.next().filter(|a| !a.is_empty()) {
                tail.push(args.to_string());
            }
            return tail;
        }
    }
    vec![message.to_string()]
}

pub struct OpencodeProvider;

impl Provider for OpencodeProvider {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn create_argv(&self, ctx: &CreateContext) -> Vec<String> {
        // `opencode run [prompt]` is the headless one-shot;
        // `--dangerously-skip-permissions` (auto-approve permissions) is the
        // never-prompt lane so an unattended run cannot wedge on its first
        // approval. Confirmed vs opencode v1.14.50 `run --help` (x-567d); the
        // docs' `--auto` is stale.
        let mut argv = vec![
            "opencode".into(),
            "run".into(),
            "--dangerously-skip-permissions".into(),
        ];
        argv.extend(opencode_run_tail(&ctx.message));
        argv
    }

    fn resume_argv(&self, ctx: &ResumeContext) -> Vec<String> {
        // opencode continues a session via `--session <id>` (run cmd).
        let mut argv = vec![
            "opencode".into(),
            "run".into(),
            "--dangerously-skip-permissions".into(),
            "--session".into(),
            ctx.session_id.clone(),
        ];
        argv.extend(opencode_run_tail(&ctx.message));
        argv
    }

    fn parse_stream_event(&self, chunk: &str) -> ParsedEvent {
        // The hosted TUI is plain text — no structured stream to parse (the
        // acp lane would change this). Same shape as agy.
        if chunk.trim().is_empty() {
            ParsedEvent::Unknown {
                raw: chunk.to_string(),
            }
        } else {
            ParsedEvent::ReplyComplete {
                text: chunk.to_string(),
                duration_ms: 0,
            }
        }
    }

    fn reachability(
        &self,
        entry: &AgentEntry,
        timeout: Duration,
    ) -> Result<bool, ReachabilityProbeError> {
        opencode_reachable_with(entry, timeout, &(run_opencode_db as OpencodeDbRunner))
    }

    fn as_pty(&self) -> Option<&dyn ProviderWithPty> {
        Some(self)
    }
}

/// Runs an opencode store query, yielding `(exited_zero, stdout)`. Injected at
/// the [`opencode_reachable_with`] seam so unit tests need no opencode binary.
type OpencodeDbRunner = fn(&str, Duration) -> Result<(bool, String), String>;

/// True iff `s` is a well-formed opencode session id (`ses_` + ASCII
/// alphanumerics). Gates SQL interpolation in the probe: no quote, space, or
/// shell metacharacter can reach the subprocess.
fn is_opencode_session_id(s: &str) -> bool {
    match s.strip_prefix("ses_") {
        Some(tail) => !tail.is_empty() && tail.chars().all(|c| c.is_ascii_alphanumeric()),
        None => false,
    }
}

/// opencode reachability: membership in opencode's own session store, the same
/// question codex's probe answers ("does the session still exist" = resumable),
/// NOT "is the pane live" — a default TUI leaves no on-disk liveness artifact,
/// so liveness stays the registry row's pid axis (x-5e58).
///
/// Tri-state mirrors codex: id present -> `Ok(true)`, clean query without it ->
/// `Ok(false)`, any infrastructure failure -> `Err` (inconclusive), so a missing
/// binary or unreadable store never false-orphans a live pane. Verified against
/// opencode v1.14.50: an absent id exits 0 with empty stdout, while bad SQL, a
/// usage error, and an unopenable database all exit nonzero — so exit status
/// alone separates "gone" from "could not tell".
///
/// Matching is substring containment on the shape-validated id rather than a
/// JSON parse: opencode plugins print banners to stdout ahead of real output
/// (verified live), which would break any structured read.
fn opencode_reachable_with(
    entry: &AgentEntry,
    timeout: Duration,
    run: &OpencodeDbRunner,
) -> Result<bool, ReachabilityProbeError> {
    let sid = entry
        .session_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ReachabilityProbeError::new("opencode", "no session id in entry"))?;
    if !is_opencode_session_id(sid) {
        return Err(ReachabilityProbeError::new(
            "opencode",
            format!("malformed opencode session id {sid:?} (expected ses_<alnum>)"),
        ));
    }
    let sql = format!("select id from session where id='{sid}'");
    match run(&sql, timeout) {
        Ok((true, stdout)) => Ok(stdout.contains(sid)),
        Ok((false, _)) => Err(ReachabilityProbeError::new(
            "opencode",
            "`opencode db` exited nonzero (store unreadable or query rejected)",
        )),
        Err(e) => Err(ReachabilityProbeError::new(
            "opencode",
            format!("cannot run `opencode db`: {e}"),
        )),
    }
}

/// The real [`OpencodeDbRunner`]: `opencode db <sql>`, bounded by `timeout`.
///
/// Shelling out to opencode's own binary (rather than opening the sqlite file)
/// inherits its channel-aware database resolution (`opencode-<channel>.db`,
/// `OPENCODE_DB`) for free, keeps this crate free of a sqlite dependency, and
/// pins to the CLI verb rather than a storage layout mid-migration to v2.
///
/// The query is a single-row lookup, so its output cannot fill the stdout pipe
/// while we poll. On timeout only the child pid is killed — never its process
/// group, which fno shares with a child not placed in its own session.
fn run_opencode_db(sql: &str, timeout: Duration) -> Result<(bool, String), String> {
    use std::process::{Command, Stdio};
    let mut child = Command::new("opencode")
        .arg("db")
        .arg(sql)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("probe timed out after {timeout:?}"));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    ))
}

impl ProviderWithPty for OpencodeProvider {
    fn readiness_detector(&self) -> Box<dyn ReadinessDetector> {
        Box::new(OpencodeReadinessDetector)
    }

    fn envelope(&self) -> Box<dyn Envelope> {
        // Plain-text stdin into the TUI composer: no JSON wrapper.
        Box::new(NoEnvelope)
    }

    fn default_restart_policy(&self) -> RestartPolicy {
        RestartPolicy::default()
    }
}

/// Parse gemini's single JSON document. Gemini emits one blob at EOF (the
/// structural cleavage from codex), so this expects the COMPLETE document;
/// partial input parses as [`ParsedEvent::Unknown`].
///
/// A create-path blob can carry BOTH `session_id` and `response`. `parse_stream_event`
/// returns exactly one [`ParsedEvent`], so this surfaces the reply
/// ([`ParsedEvent::ReplyComplete`]) as the primary signal. On the create path
/// the daemon (Wave 3) captures the session id from [`CreateContext::session_id`]
/// (the daemon pre-assigns the UUID via `--session-id`, the design's default
/// flow) or, in the rare no-pre-assignment case, from this same raw blob via
/// [`gemini_session_id_from_blob`] before persisting the entry — so the id is
/// never lost despite the single-event return (Codex review P2).
fn parse_gemini_blob(blob: &str) -> ParsedEvent {
    let v: serde_json::Value = match serde_json::from_str(blob.trim()) {
        Ok(v) => v,
        Err(_) => {
            return ParsedEvent::Unknown {
                raw: blob.to_string(),
            }
        }
    };
    // A completed reply carries `response`; map to ReplyComplete with duration
    // summed from per-model API latency. snake_case `session_id` per US4-gemini
    // (NOT camelCase `sessionId`, which is gemini's internal storage shape).
    if let Some(resp) = v.get("response").and_then(|r| r.as_str()) {
        return ParsedEvent::ReplyComplete {
            text: resp.to_string(),
            duration_ms: gemini_total_latency_ms(&v),
        };
    }
    if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
        return ParsedEvent::SessionCreated {
            session_id: sid.to_string(),
        };
    }
    ParsedEvent::Unknown {
        raw: blob.to_string(),
    }
}

/// Sum `stats.models.<model>.api.totalLatencyMs` across all models. Returns 0
/// when the stats block is absent or shaped unexpectedly (degrade, don't fail).
fn gemini_total_latency_ms(v: &serde_json::Value) -> u64 {
    let Some(models) = v
        .get("stats")
        .and_then(|s| s.get("models"))
        .and_then(|m| m.as_object())
    else {
        return 0;
    };
    models
        .values()
        .filter_map(|m| m.get("api"))
        .filter_map(|api| api.get("totalLatencyMs"))
        .filter_map(|l| l.as_u64())
        .sum()
}

// ---------------------------------------------------------------------------
// Reachability helpers (HOME-relative; testable via $HOME override).
// ---------------------------------------------------------------------------

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Extract gemini's assigned `session_id` from a create-path JSON blob. The
/// daemon (Wave 3) calls this on the create path when it did NOT pre-assign a
/// UUID, so the session handle gemini chose is persisted even though
/// [`parse_stream_event`](Provider::parse_stream_event) surfaces the reply
/// rather than the id (single-event return; Codex review P2). Returns `None`
/// when the blob is malformed or carries no `session_id`.
pub fn gemini_session_id_from_blob(blob: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(blob.trim())
        .ok()?
        .get("session_id")?
        .as_str()
        .map(|s| s.to_string())
}

/// The provider roster: every provider name the Rust side can DISPATCH/host —
/// the spawn gates in `bin/client.rs` and [`for_name`] ride THIS list (x-51f6
/// US1: one source of truth, no per-site `matches!` copies).
///
/// NAMING SKEW (x-8dfc, Discretion 4 — commented, not lockstep-renamed, to keep
/// the diff small): this 5-name list mirrors Python's `READABLE_PROVIDERS` (the
/// spawn/pane read-tolerance roster), NOT Python's narrower 3-name
/// `KNOWN_PROVIDERS` (its dispatch set). A cli test pins this == READABLE.
/// It is NO LONGER a registry-LOAD gate: `client_verbs::load_registry_entries`
/// now shape-checks identity, so an alien harness reads without bricking; this
/// list gates only spawn/`for_name`. Every name here MUST have a [`for_name`]
/// arm (test-enforced).
pub const KNOWN_PROVIDERS: &[&str] = &["claude", "codex", "gemini", "agy", "opencode"];

/// The roster joined for error messages ("claude, codex, gemini, agy, opencode").
pub fn known_providers_csv() -> String {
    KNOWN_PROVIDERS.join(", ")
}

/// Resolve a provider impl by its stable name (`"claude"` / `"codex"` /
/// `"gemini"`). Returns `None` for an unknown provider so callers (e.g.
/// reconcile) can treat the probe as inconclusive rather than guessing. The
/// only place provider names map to impls — keeping provider knowledge in this
/// one file (the LD8 discipline).
pub fn for_name(name: &str) -> Option<Box<dyn Provider>> {
    match name {
        "claude" => Some(Box::new(ClaudeProvider)),
        "codex" => Some(Box::new(CodexProvider)),
        "gemini" => Some(Box::new(GeminiProvider)),
        "agy" => Some(Box::new(AgyProvider)),
        "opencode" => Some(Box::new(OpencodeProvider)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_ctx() -> CreateContext {
        CreateContext {
            name: "worker-A".into(),
            message: "build feature X".into(),
            cwd: PathBuf::from("/tmp/example-repo"),
            from_name: None,
            session_id: None,
            yolo: false,
            reasoning_effort: None,
            append_system_prompt: None,
        }
    }

    // ---- argv shapes ----

    #[test]
    fn claude_create_argv_uses_bg_not_print() {
        let argv = ClaudeProvider.create_argv(&create_ctx());
        assert_eq!(
            argv,
            vec!["claude", "--bg", "--name", "worker-A", "build feature X"]
        );
        assert!(!argv.iter().any(|a| a == "-p"), "LD38: never claude -p");
    }

    #[test]
    fn claude_resume_argv_is_resume_print() {
        let ctx = ResumeContext {
            session_id: "7c5dcf5d".into(),
            message: "follow up".into(),
            cwd: PathBuf::from("/x"),
            from_name: None,
            yolo: false,
        };
        assert_eq!(
            ClaudeProvider.resume_argv(&ctx),
            vec!["claude", "--resume", "7c5dcf5d", "--print", "follow up"]
        );
    }

    #[test]
    fn claude_stream_json_resume_argv_uses_p_and_full_uuid() {
        // The stream-json host lane resumes by the FULL UUID with -p +
        // stream-json IO (the only flags that yield a drivable bidirectional
        // pipe). -p here is the deliberate adoption lane (LD1), distinct from
        // the --bg create path (LD38).
        let argv = claude_stream_json_resume_argv("019e7157-4236-7bb1-b274-ebbac6040ace");
        assert_eq!(
            argv,
            vec![
                "claude",
                "-p",
                "--resume",
                "019e7157-4236-7bb1-b274-ebbac6040ace",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--include-partial-messages",
                "--replay-user-messages",
            ]
        );
    }

    #[test]
    fn codex_create_argv_defaults_to_workspace_write_sandbox() {
        let argv = CodexProvider.create_argv(&create_ctx());
        assert_eq!(
            argv,
            vec![
                "codex",
                "exec",
                "--json",
                "-C",
                "/tmp/example-repo",
                "--skip-git-repo-check",
                "--sandbox",
                "workspace-write",
                "build feature X"
            ]
        );
    }

    #[test]
    fn codex_create_argv_yolo_is_mutually_exclusive_with_sandbox() {
        let mut ctx = create_ctx();
        ctx.yolo = true;
        let argv = CodexProvider.create_argv(&ctx);
        assert!(argv.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
        assert!(!argv.iter().any(|a| a == "--sandbox"));
    }

    #[test]
    fn codex_create_argv_appends_reasoning_effort() {
        let mut ctx = create_ctx();
        ctx.reasoning_effort = Some("high".into());
        let argv = CodexProvider.create_argv(&ctx);
        assert!(argv
            .windows(2)
            .any(|w| w == ["-c", "model_reasoning_effort=high"]));
    }

    #[test]
    fn codex_resume_argv_omits_sandbox_unless_yolo() {
        let ctx = ResumeContext {
            session_id: "uuid-1".into(),
            message: "m".into(),
            cwd: PathBuf::from("/x"),
            from_name: None,
            yolo: false,
        };
        assert_eq!(
            CodexProvider.resume_argv(&ctx),
            vec![
                "codex",
                "exec",
                "resume",
                "uuid-1",
                "--json",
                "--skip-git-repo-check",
                "m"
            ]
        );
    }

    #[test]
    fn gemini_create_argv_passes_session_id_and_default_approval() {
        let mut ctx = create_ctx();
        ctx.session_id = Some("uuid-g".into());
        let argv = GeminiProvider.create_argv(&ctx);
        assert_eq!(
            argv,
            vec![
                "gemini",
                "--skip-trust",
                "-p",
                "build feature X",
                "--output-format",
                "json",
                "--approval-mode",
                "default",
                "--session-id",
                "uuid-g"
            ]
        );
    }

    #[test]
    fn gemini_resume_argv_uses_resume_flag() {
        let ctx = ResumeContext {
            session_id: "uuid-g".into(),
            message: "m".into(),
            cwd: PathBuf::from("/x"),
            from_name: None,
            yolo: true,
        };
        let argv = GeminiProvider.resume_argv(&ctx);
        assert_eq!(
            argv,
            vec![
                "gemini",
                "--skip-trust",
                "-p",
                "m",
                "--output-format",
                "json",
                "--yolo",
                "--resume",
                "uuid-g"
            ]
        );
    }

    // ---- interactive argv (host_mode=interactive): host + promote ----

    // ---- as_pty type-level routing ----

    #[test]
    fn claude_is_not_pty_managed_others_are() {
        // The shellout `--bg` claude stays non-PTY; the interactive claude (E1)
        // and codex/gemini are PTY-managed.
        assert!(ClaudeProvider.as_pty().is_none());
        assert!(ClaudeInteractiveProvider.as_pty().is_some());
        assert!(CodexProvider.as_pty().is_some());
        assert!(GeminiProvider.as_pty().is_some());
    }

    // ---- ClaudeInteractiveProvider (E1 keystone) ----

    // ---- OpencodeProvider (x-51f6) ----

    #[test]
    fn opencode_create_argv_is_headless_run_never_bare_tui() {
        // The trait's create path is the headless `opencode run` one-shot
        // (never-prompt via --dangerously-skip-permissions); the bare-`opencode`
        // TUI is the PANE form and lives in mux_spawn.build_pane_argv, not here.
        let argv = OpencodeProvider.create_argv(&create_ctx());
        assert_eq!(
            argv,
            vec![
                "opencode",
                "run",
                "--dangerously-skip-permissions",
                "build feature X"
            ]
        );
    }

    #[test]
    fn opencode_create_argv_routes_slash_command_via_command_flag() {
        // A rendered footnote slash command rides `--command <verb>` (opencode
        // expands the plugin command) with the rest as args - NOT a prose prompt
        // that `run` would run verbatim (x-de43 / codex P1).
        let mut ctx = create_ctx();
        ctx.message = "/fno:target no-merge x-abcd".into();
        assert_eq!(
            OpencodeProvider.create_argv(&ctx),
            vec![
                "opencode",
                "run",
                "--dangerously-skip-permissions",
                "--command",
                "fno:target",
                "no-merge x-abcd"
            ]
        );
    }

    #[test]
    fn opencode_run_tail_prose_through_and_bare_verb() {
        // Prose passes through unchanged; a bare verb has no args tail.
        assert_eq!(
            opencode_run_tail("build feature X"),
            vec!["build feature X"]
        );
        assert_eq!(opencode_run_tail("/fno:pr"), vec!["--command", "fno:pr"]);
    }

    #[test]
    fn opencode_resume_argv_uses_session_flag() {
        let ctx = ResumeContext {
            session_id: "ses_abc".into(),
            message: "m".into(),
            cwd: PathBuf::from("/x"),
            from_name: None,
            yolo: false,
        };
        assert_eq!(
            OpencodeProvider.resume_argv(&ctx),
            vec![
                "opencode",
                "run",
                "--dangerously-skip-permissions",
                "--session",
                "ses_abc",
                "m"
            ]
        );
    }

    #[test]
    fn opencode_is_pty_managed_and_inconclusive_to_probe() {
        assert!(OpencodeProvider.as_pty().is_some());
        // v1 never false-orphans a live opencode pane: probes are inconclusive.
        let entry = AgentEntry {
            name: "oc".into(),
            provider: "opencode".into(),
            session_id: None,
            cwd: PathBuf::from("/x"),
        };
        assert!(OpencodeProvider
            .reachability(&entry, Duration::from_secs(1))
            .is_err());
    }

    #[test]
    fn for_name_round_trips_every_known_provider() {
        // for_name is the LD8 single registration point; a copy-paste slip
        // (e.g. "codex" => GeminiProvider) would pass every other test, so
        // assert each name resolves to a provider reporting that same name.
        // Iterating KNOWN_PROVIDERS (x-51f6 US1 / AC1-FR) makes this the
        // roster-parity gate too: a name added to the const without a
        // for_name arm fails here, and the consolidation can never silently
        // narrow the roster (the old hardcoded list had already drifted —
        // it missed agy).
        for name in KNOWN_PROVIDERS.iter().copied() {
            let p = for_name(name).unwrap_or_else(|| panic!("for_name({name}) returned None"));
            assert_eq!(
                p.name(),
                name,
                "for_name({name}) resolved to wrong provider"
            );
        }
        assert!(for_name("nope").is_none(), "unknown provider must be None");
    }

    // ---- claude short-id parse ----

    #[test]
    fn claude_parses_short_id_from_bg_line() {
        let ev = ClaudeProvider.parse_stream_event("backgrounded · 7c5dcf5d · worker-A");
        assert_eq!(
            ev,
            ParsedEvent::SessionCreated {
                session_id: "7c5dcf5d".into()
            }
        );
    }

    #[test]
    fn claude_non_id_line_is_unknown() {
        assert!(matches!(
            ClaudeProvider.parse_stream_event("starting up"),
            ParsedEvent::Unknown { .. }
        ));
        // Uppercase hex is not a claude short-id (lowercase contract).
        assert!(matches!(
            ClaudeProvider.parse_stream_event("ABCDEF12"),
            ParsedEvent::Unknown { .. }
        ));
    }

    // ---- codex JSONL parse (pinned to fixture vocabulary) ----

    #[test]
    fn codex_thread_started_is_session_created() {
        let ev = parse_codex_line(
            r#"{"type":"thread.started","thread_id":"019e4958-80d1-7492-8054-2854dfda502c"}"#,
        );
        assert_eq!(
            ev,
            ParsedEvent::SessionCreated {
                session_id: "019e4958-80d1-7492-8054-2854dfda502c".into()
            }
        );
    }

    #[test]
    fn codex_agent_message_is_output_chunk() {
        let ev = parse_codex_line(
            r#"{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"hello"}}"#,
        );
        assert_eq!(
            ev,
            ParsedEvent::OutputChunk {
                text: "hello".into()
            }
        );
    }

    #[test]
    fn codex_error_item_is_provider_error() {
        let ev = parse_codex_line(
            r#"{"type":"item.completed","item":{"id":"item_0","type":"error","message":"boom"}}"#,
        );
        assert_eq!(
            ev,
            ParsedEvent::ProviderError {
                message: "boom".into()
            }
        );
    }

    #[test]
    fn codex_command_execution_is_tool_use() {
        let ev = parse_codex_line(
            r#"{"type":"item.started","item":{"id":"item_2","type":"command_execution","command":"echo hi"}}"#,
        );
        match ev {
            ParsedEvent::ToolUse { name, args } => {
                assert_eq!(name, "command_execution");
                assert_eq!(args.unwrap()["command"], "echo hi");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn codex_turn_completed_is_reply_complete_marker() {
        let ev = parse_codex_line(r#"{"type":"turn.completed","usage":{"output_tokens":91}}"#);
        assert_eq!(
            ev,
            ParsedEvent::ReplyComplete {
                text: String::new(),
                duration_ms: 0
            }
        );
    }

    #[test]
    fn codex_preamble_and_control_frames_are_unknown() {
        assert!(matches!(
            parse_codex_line("Reading additional input from stdin..."),
            ParsedEvent::Unknown { .. }
        ));
        assert!(matches!(
            parse_codex_line(r#"{"type":"turn.started"}"#),
            ParsedEvent::Unknown { .. }
        ));
    }

    // ---- gemini blob parse ----

    #[test]
    fn gemini_blob_response_is_reply_complete_with_latency() {
        let blob = r#"{
          "session_id": "abc",
          "response": "PONG",
          "stats": {"models": {"gemini-3.1-flash-lite": {"api": {"totalLatencyMs": 3359}}}}
        }"#;
        assert_eq!(
            parse_gemini_blob(blob),
            ParsedEvent::ReplyComplete {
                text: "PONG".into(),
                duration_ms: 3359
            }
        );
    }

    #[test]
    fn gemini_latency_sums_across_models() {
        let blob = r#"{
          "response": "ok",
          "stats": {"models": {
            "m1": {"api": {"totalLatencyMs": 100}},
            "m2": {"api": {"totalLatencyMs": 250}}
          }}
        }"#;
        assert_eq!(
            parse_gemini_blob(blob),
            ParsedEvent::ReplyComplete {
                text: "ok".into(),
                duration_ms: 350
            }
        );
    }

    #[test]
    fn gemini_session_only_blob_is_session_created() {
        let ev = parse_gemini_blob(r#"{"session_id":"xyz"}"#);
        assert_eq!(
            ev,
            ParsedEvent::SessionCreated {
                session_id: "xyz".into()
            }
        );
    }

    #[test]
    fn gemini_partial_or_garbage_is_unknown() {
        assert!(matches!(
            parse_gemini_blob(r#"{"session_id": "incomplete"#),
            ParsedEvent::Unknown { .. }
        ));
    }

    #[test]
    fn gemini_session_id_recoverable_from_create_blob_even_with_reply() {
        // parse_stream_event surfaces the reply (single-event return), but the
        // session id is still recoverable from the same blob for the create path.
        let blob = r#"{"session_id":"abc-123","response":"hi","stats":{}}"#;
        assert_eq!(
            parse_gemini_blob(blob),
            ParsedEvent::ReplyComplete {
                text: "hi".into(),
                duration_ms: 0
            }
        );
        assert_eq!(gemini_session_id_from_blob(blob), Some("abc-123".into()));
        assert_eq!(gemini_session_id_from_blob("not json"), None);
        assert_eq!(gemini_session_id_from_blob(r#"{"response":"x"}"#), None);
    }

    // ---- reachability tri-state (HOME-overridden) ----

    #[test]
    fn reachability_no_session_id_is_inconclusive() {
        let entry = AgentEntry {
            name: "a".into(),
            provider: "codex".into(),
            session_id: None,
            cwd: PathBuf::from("/x"),
        };
        let err = CodexProvider
            .reachability(&entry, Duration::from_millis(250))
            .unwrap_err();
        assert_eq!(err.provider, "codex");
    }

    fn codex_entry(session_id: &str) -> AgentEntry {
        AgentEntry {
            name: "a".into(),
            provider: "codex".into(),
            session_id: Some(session_id.into()),
            cwd: PathBuf::from("/x"),
        }
    }

    // -- opencode store probe (x-830c) ------------------------------------
    // Every case drives an injected runner, so the suite never shells out to a
    // real opencode binary or reads ~/.local/share/opencode.

    const OC_SES: &str = "ses_09679f284ffeJv7NdBAoLQLnLZ";

    fn opencode_entry(session_id: Option<&str>) -> AgentEntry {
        AgentEntry {
            name: "o".into(),
            provider: "opencode".into(),
            session_id: session_id.map(Into::into),
            cwd: PathBuf::from("/x"),
        }
    }

    #[test]
    fn opencode_reachable_when_store_returns_the_id() {
        // Leading plugin banner: opencode plugins print to stdout ahead of real
        // output, so the probe must tolerate garbage before the row.
        fn run(_sql: &str, _t: Duration) -> Result<(bool, String), String> {
            Ok((
                true,
                format!("[claude-mem] OpenCode plugin loading\nid\n{OC_SES}\n"),
            ))
        }
        assert_eq!(
            opencode_reachable_with(
                &opencode_entry(Some(OC_SES)),
                Duration::from_secs(2),
                &(run as OpencodeDbRunner)
            ),
            Ok(true)
        );
    }

    #[test]
    fn opencode_probe_embeds_only_the_validated_id_in_the_query() {
        use std::sync::{Mutex, OnceLock};
        static SEEN: OnceLock<Mutex<String>> = OnceLock::new();
        fn run(sql: &str, _t: Duration) -> Result<(bool, String), String> {
            *SEEN.get_or_init(Default::default).lock().unwrap() = sql.to_string();
            Ok((true, OC_SES.to_string()))
        }
        let _ = opencode_reachable_with(
            &opencode_entry(Some(OC_SES)),
            Duration::from_secs(2),
            &(run as OpencodeDbRunner),
        );
        assert_eq!(
            *SEEN.get_or_init(Default::default).lock().unwrap(),
            format!("select id from session where id='{OC_SES}'")
        );
    }

    #[test]
    fn opencode_clean_query_without_the_id_is_gone() {
        // Verified on v1.14.50: an absent id exits 0 with empty stdout.
        fn run(_sql: &str, _t: Duration) -> Result<(bool, String), String> {
            Ok((true, String::new()))
        }
        assert_eq!(
            opencode_reachable_with(
                &opencode_entry(Some(OC_SES)),
                Duration::from_secs(2),
                &(run as OpencodeDbRunner)
            ),
            Ok(false)
        );
    }

    #[test]
    fn opencode_infrastructure_failure_is_inconclusive_never_gone() {
        // Spawn failure (binary missing) and a nonzero exit (unopenable store)
        // must both stay Err, or a dead-pane pass would orphan a live pane.
        fn spawn_failed(_sql: &str, _t: Duration) -> Result<(bool, String), String> {
            Err("No such file or directory (os error 2)".into())
        }
        fn nonzero(_sql: &str, _t: Duration) -> Result<(bool, String), String> {
            Ok((false, String::new()))
        }
        for run in [
            spawn_failed as OpencodeDbRunner,
            nonzero as OpencodeDbRunner,
        ] {
            let err = opencode_reachable_with(
                &opencode_entry(Some(OC_SES)),
                Duration::from_secs(2),
                &run,
            )
            .unwrap_err();
            assert_eq!(err.provider, "opencode");
        }
    }

    #[test]
    fn opencode_malformed_id_never_reaches_the_subprocess() {
        fn run(_sql: &str, _t: Duration) -> Result<(bool, String), String> {
            panic!("probe must reject a malformed id before spawning");
        }
        for bad in ["ses_'; drop table session--", "ses_ x", "ses_", "not-a-ses"] {
            let err = opencode_reachable_with(
                &opencode_entry(Some(bad)),
                Duration::from_secs(2),
                &(run as OpencodeDbRunner),
            )
            .unwrap_err();
            assert!(err.reason.contains(bad), "reason should quote {bad:?}");
        }
    }

    #[test]
    fn opencode_missing_session_id_is_inconclusive() {
        fn run(_sql: &str, _t: Duration) -> Result<(bool, String), String> {
            panic!("no id means nothing to probe");
        }
        assert!(opencode_reachable_with(
            &opencode_entry(None),
            Duration::from_secs(2),
            &(run as OpencodeDbRunner)
        )
        .is_err());
    }

    #[test]
    fn opencode_probe_is_stateless_across_calls() {
        // AC1-FR: a transient failure poisons nothing; the next call reports the
        // true store verdict.
        fn failing(_sql: &str, _t: Duration) -> Result<(bool, String), String> {
            Err("binary briefly unavailable".into())
        }
        fn healthy(_sql: &str, _t: Duration) -> Result<(bool, String), String> {
            Ok((true, OC_SES.to_string()))
        }
        let entry = opencode_entry(Some(OC_SES));
        assert!(opencode_reachable_with(
            &entry,
            Duration::from_secs(2),
            &(failing as OpencodeDbRunner)
        )
        .is_err());
        assert_eq!(
            opencode_reachable_with(
                &entry,
                Duration::from_secs(2),
                &(healthy as OpencodeDbRunner)
            ),
            Ok(true)
        );
    }

    #[test]
    fn codex_reachable_when_id_in_session_index() {
        let tmp = tempdir();
        let idx = tmp.join(".codex").join("session_index.jsonl");
        std::fs::create_dir_all(idx.parent().unwrap()).unwrap();
        std::fs::write(
            &idx,
            "{\"id\":\"019e4958-80d1-7492-8054-2854dfda502c\",\"status\":\"live\"}\n",
        )
        .unwrap();
        with_home(&tmp, || {
            let entry = codex_entry("019e4958-80d1-7492-8054-2854dfda502c");
            assert_eq!(
                CodexProvider.reachability(&entry, Duration::from_secs(2)),
                Ok(true)
            );
        });
    }

    #[test]
    fn codex_index_present_id_absent_is_false() {
        // The id is NOT in the index -> the session ended (index drops it) ->
        // definitively orphaned, even if a historical session file still exists.
        let tmp = tempdir();
        let idx = tmp.join(".codex").join("session_index.jsonl");
        std::fs::create_dir_all(idx.parent().unwrap()).unwrap();
        std::fs::write(&idx, "{\"id\":\"some-other-uuid\"}\n").unwrap();
        with_home(&tmp, || {
            let entry = codex_entry("019e4958-80d1-7492-8054-2854dfda502c");
            assert_eq!(
                CodexProvider.reachability(&entry, Duration::from_secs(2)),
                Ok(false)
            );
        });
    }

    #[test]
    fn codex_index_absent_is_inconclusive() {
        let tmp = tempdir();
        with_home(&tmp, || {
            let entry = codex_entry("019e4958-80d1-7492-8054-2854dfda502c");
            // No session_index.jsonl (fresh install) -> inconclusive, never orphan.
            assert!(CodexProvider
                .reachability(&entry, Duration::from_secs(2))
                .is_err());
        });
    }

    fn gemini_entry(session_id: &str, cwd: &str) -> AgentEntry {
        AgentEntry {
            name: "g".into(),
            provider: "gemini".into(),
            session_id: Some(session_id.into()),
            cwd: PathBuf::from(cwd),
        }
    }

    const G_UUID: &str = "35624650-b11e-4300-ad85-0fc87baeb3af";

    #[test]
    fn gemini_reachability_is_cwd_pinned_and_verifies_full_uuid() {
        let tmp = tempdir();
        // Filename carries the 8-char short prefix; the FULL uuid must appear in
        // the file's first line for the probe to confirm (defeats collisions).
        let chats = tmp
            .join(".gemini")
            .join("tmp")
            .join("myproject")
            .join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(
            chats.join("session-35624650.json"),
            format!("{{\"sessionId\":\"{G_UUID}\",\"messages\":[]}}\n").as_bytes(),
        )
        .unwrap();
        with_home(&tmp, || {
            let entry = gemini_entry(G_UUID, "/work/myproject");
            assert_eq!(
                GeminiProvider.reachability(&entry, Duration::from_secs(2)),
                Ok(true)
            );
            // Same id but a DIFFERENT cwd must not find it (cwd-pinned).
            let other = gemini_entry(G_UUID, "/work/elsewhere");
            assert!(GeminiProvider
                .reachability(&other, Duration::from_secs(2))
                .is_err()); // chats dir for "elsewhere" absent -> inconclusive
        });
    }

    #[test]
    fn gemini_short_prefix_collision_without_full_uuid_is_false() {
        // A different session shares the 8-char prefix but the file's full uuid
        // differs -> the content-verification step rejects it (Codex P2).
        let tmp = tempdir();
        let chats = tmp.join(".gemini").join("tmp").join("proj").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(
            chats.join("session-35624650.json"),
            b"{\"sessionId\":\"35624650-ffff-ffff-ffff-ffffffffffff\"}\n",
        )
        .unwrap();
        with_home(&tmp, || {
            let entry = gemini_entry(G_UUID, "/x/proj");
            assert_eq!(
                GeminiProvider.reachability(&entry, Duration::from_secs(2)),
                Ok(false)
            );
        });
    }

    #[test]
    fn gemini_reachability_chats_present_no_match_is_false() {
        let tmp = tempdir();
        let chats = tmp.join(".gemini").join("tmp").join("proj").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(chats.join("session-deadbeef.json"), b"{}").unwrap();
        with_home(&tmp, || {
            let entry = gemini_entry("00000000-1111-2222-3333-444444444444", "/x/proj");
            assert_eq!(
                GeminiProvider.reachability(&entry, Duration::from_secs(2)),
                Ok(false)
            );
        });
    }

    #[test]
    fn gemini_reachability_short_session_id_is_inconclusive() {
        let entry = gemini_entry("uuid", "/x/proj");
        let err = GeminiProvider
            .reachability(&entry, Duration::from_millis(250))
            .unwrap_err();
        assert_eq!(err.provider, "gemini");
        assert!(err.reason.contains("too short"));
    }

    // ---- test helpers (no external tempfile dep) ----

    fn tempdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        let unique = format!(
            "fno-agents-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        p.push(unique);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Process-global lock serializing $HOME mutation. cargo runs tests in
    /// parallel threads within one process; HOME is process-global, so two
    /// `with_home` calls would race without this guard.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run `f` with $HOME set to `home`, restoring the prior value after. The
    /// reachability helpers read HOME on each call; the lock makes the
    /// set -> run -> restore window atomic across parallel test threads.
    fn with_home(home: &std::path::Path, f: impl FnOnce()) {
        // Poisoning is irrelevant here (the guarded data is unit); recover it.
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", home);
        f();
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}
