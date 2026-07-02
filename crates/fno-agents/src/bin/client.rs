//! `fno-agents` client entrypoint (Wave 3). Parses a verb + flags into a
//! JSON-RPC request, lazy-starts the daemon, forwards the request, prints the
//! result, and maps the daemon's error code to a process exit code.
//!
//! This is the thin Rust client the Python `fno agents <verb>` wrapper (Wave 6)
//! will exec. Power users can call it directly. The argv surface here is the
//! minimum that exercises every Wave 3 daemon verb end-to-end; the rich flag
//! surface (`--stream`, `--watch`, ...) lands with its verbs in later waves.

use fno_agents::client::resolve_daemon_bin;
use fno_agents::client::{
    call, call_if_running, check_daemon_drift, drift_from_status, restart_daemon, ClientError,
    RestartError, RestartOutcome,
};
use fno_agents::drift::drift_warning;
use fno_agents::paths::AgentsHome;
use fno_agents::protocol::{ErrorCode, Request, ResponsePayload};
use serde_json::{json, Map, Value};
use std::io::IsTerminal;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");
    let code = rt.block_on(run(args));
    std::process::exit(code);
}

async fn run(args: Vec<String>) -> i32 {
    if args.is_empty() {
        print_help();
        return 0;
    }
    let verb = args[0].as_str();
    if matches!(verb, "-h" | "--help" | "help") {
        print_help();
        return 0;
    }

    // `version` / `-V` / `--version`: report which commit this binary was built
    // from (ab-24a59d50) -- the prerequisite for Rust-side `fno doctor`
    // staleness. `--json` emits the machine surface `fno doctor` reads off the
    // resolved binary path. Side-effect-free, like `--emit-schema`/`help`: it
    // never starts the daemon and is NOT a routable daemon verb, so it stays out
    // of CLIENT_VERB_USAGE / RUST_CLIENT_VERBS (callers invoke the binary
    // directly). Matched here rather than as a dispatch arm so the routable-verb
    // parity guard (test_rust_client_verbs_match_client_rs) does not see it.
    if matches!(verb, "version" | "-V" | "--version") {
        let json = args[1..].iter().any(|a| a == "--json");
        print_version(json);
        return 0;
    }

    // `mail-inject` is the one-shot LIVE-DELIVERY verb `fno mail send` calls to
    // inject a turn into a live `claude --bg` session over the daemon control.sock
    // (node x-1f23). Binary-direct (Python `_deliver_live` subprocess), NOT a
    // routable `fno agents` verb -- matched with `matches!` (like `version`) so the
    // parity guard (test_rust_client_verbs_match_client_rs) does not see it and it
    // stays out of CLIENT_VERB_USAGE / RUST_CLIENT_VERBS. Connects to an existing
    // daemon; never lazy-starts one.
    if matches!(verb, "mail-inject") {
        return fno_agents::mail_inject::run_mail_inject(&args[1..]);
    }

    // `claim` is the HIDDEN debug front over the native claims module
    // (`fno_agents::claims`): the cross-impl compatibility matrix drives the
    // Rust side of the lockfile protocol through it, and it doubles as an ops
    // escape hatch when the Python CLI is unavailable. Matched with `matches!`
    // (like `mail-inject`) so the routable-verb parity guard does not see it
    // and it stays out of CLIENT_VERB_USAGE / RUST_CLIENT_VERBS — `fno claim`
    // remains the only operator CLI for claims.
    if matches!(verb, "claim") {
        return fno_agents::client_verbs::run_claim(&args[1..]);
    }

    // Per-verb help: `fno agents <verb> --help` prints that verb's usage line
    // and exits 0, instead of the verb's arg parser erroring "unknown flag:
    // --help" / "takes no arguments" (ab-351427cb). Only fires for a recognized
    // verb; an unknown verb falls through to its normal error path. The scan
    // stops at an `--argv`/`--` boundary so a `--help` inside a spawn/host argv
    // payload reaches the spawned command instead of being captured here.
    if is_help_request(&args[1..]) {
        if let Some(usage) = verb_usage(verb) {
            println!("usage: fno-agents {usage}");
            return 0;
        }
    }

    // `--emit-schema` is a read-only introspection flag: prints the Branch B
    // envelope + status-v1 schema + known event kinds as JSON to stdout, then
    // exits 0. Used by scripts/check-event-schema-parity.sh. Must not start
    // the daemon or read any runtime state (AC2-HP: side-effect-free).
    if verb == "--emit-schema" {
        let schema = fno_agents::emit_schema_json();
        match serde_json::to_string_pretty(&schema) {
            Ok(s) => {
                println!("{s}");
                return 0;
            }
            Err(e) => {
                eprintln!("fno-agents --emit-schema: serialization error: {e}");
                return 1;
            }
        }
    }

    // `loop-check` is the stop-hook decision verb (Task 1.1, ab-d0337fbc).
    // It reads external state (git, gh, manifest, transcript, events, ledger)
    // and returns a single JSON decision object. Direct dispatch; no daemon RPC.
    if verb == "loop-check" {
        return fno_agents::loopcheck::run_loop_check(&args[1..]);
    }

    // `loop run` is the unified driver loop (step 5, ab-781b6d17). Direct
    // dispatch like loop-check; no daemon RPC.
    if verb == "loop" {
        return fno_agents::loop_target::run_loop_verb(&args[1..]);
    }

    // `finalize` is the terminal-only side-effect WRITER (step 6, ab-f8e5f214).
    // The stop-hook shim invokes it on a terminal-allow loop-check decision to
    // re-home the ledger record + plan stamp/graduate + handoff artifact. Direct
    // dispatch; no daemon RPC. loop-check stays the read-only decision verb.
    if verb == "finalize" {
        return fno_agents::finalize::run_finalize(&args[1..]);
    }

    // `kill-check` is the Rust port of scripts/lib/kill-criteria.sh
    // (packaging EPIC ab-8bdb4642). It evaluates a plan's kill_criteria
    // predicates against target-state + git, printing the single
    // `KILL_CRITERIA_FIRED <name>|<reason>` line (exit 1) when one fires, else
    // empty stdout (exit 0). Direct dispatch; no daemon RPC.
    if verb == "kill-check" {
        return fno_agents::kill_criteria::run_kill_check(&args[1..]);
    }

    // `verify-evidence` is the Rust port of scripts/lib/verify-event-evidence.sh
    // (packaging EPIC ab-8bdb4642). It dispatches on a leading sub-token
    // (event | child-promise | has-nonclaude) and reproduces the bash exit
    // codes + stdout diagnostic kinds + stderr warnings. Direct dispatch.
    if verb == "verify-evidence" {
        return fno_agents::verify_evidence::run_verify_evidence(&args[1..]);
    }

    // `drive` does not fit the one-shot request/response path: it upgrades to a
    // long-lived WebSocket and takes over the terminal. Dispatch it directly.
    if verb == "drive" {
        return run_drive(&args[1..]).await;
    }

    // `grid` is the multi-pane sibling of `drive`: one watcher WebSocket per
    // named agent, tiled in a TUI compositor, with the focused pane promotable
    // to a driver on Enter (ab-3c063856). Like `drive`, it does not fit the
    // one-shot RPC path; the runtime lives in `fno_agents::grid`.
    if verb == "grid" {
        return fno_agents::grid::run_grid(&args[1..], &AgentsHome::from_env()).await;
    }

    // Python-only verbs ported to the Rust client: these read state/registry/
    // event files directly (or print a stub) without a daemon RPC, so they
    // dispatch here before build_request. Byte-for-byte parity with the Python
    // implementations is the contract; see `fno_agents::client_verbs`.
    if verb == "drive-authority" {
        return fno_agents::client_verbs::run_drive_authority(&args[1..], &AgentsHome::from_env());
    }
    if verb == "trace" {
        return fno_agents::client_verbs::run_trace(&args[1..], &AgentsHome::from_env());
    }
    if verb == "ping" {
        return fno_agents::client_verbs::run_ping(&args[1..]);
    }
    if verb == "resume" {
        return fno_agents::client_verbs::run_resume(&args[1..], &AgentsHome::from_env());
    }
    if verb == "attach" {
        return fno_agents::client_verbs::run_attach(&args[1..], &AgentsHome::from_env());
    }
    if verb == "logs" {
        return fno_agents::client_verbs::run_logs(&args[1..], &AgentsHome::from_env()).await;
    }
    // Inside-leg state push (E3.2): a per-turn hook reports {working|blocked|done}.
    // Dispatched here (no build_request) because it sends to an ALREADY-RUNNING
    // daemon and must never lazy-start one.
    if verb == "report" {
        return fno_agents::client_verbs::run_report(&args[1..], &AgentsHome::from_env()).await;
    }

    // `status` reports on a *running* daemon: it must NOT lazy-start one just to
    // describe it as up. A down daemon is exit 13 (AC10-ERR).
    if verb == "status" {
        // status takes no further args; reject extras rather than silently
        // ignoring a mistyped flag the way other verbs would not (Codex P3).
        if args.len() > 1 {
            eprintln!(
                "fno-agents: status takes no arguments (got: {})",
                args[1..].join(" ")
            );
            return 2;
        }
        return run_status().await;
    }

    // `restart` swaps a stale daemon for one built from the current binary
    // (ab-1891cdff): SIGTERM the running daemon (graceful drain; PTY workers
    // survive), wait for the socket to clear, lazy-start fresh. Like `status`,
    // it does not fit the one-shot build_request path and dispatches here.
    if verb == "restart" {
        if args.len() > 1 {
            eprintln!(
                "fno-agents: restart takes no arguments (got: {})",
                args[1..].join(" ")
            );
            return 2;
        }
        return run_restart().await;
    }

    // `reap` is the manual dead-row GC (x-b1aa): the SAME sweep the daemon runs
    // on its idle tick, on demand. It operates on the registry directly under the
    // shared flock, so it needs no running daemon and dispatches here before
    // build_request.
    if verb == "reap" {
        return run_reap(&args[1..]);
    }

    // Capture the verb name so format_success can use it at the print site
    // without threading it through the protocol layer.
    let verb_owned = verb.to_string();

    // Task 3.1: capture --json before build_request strips it, and detect TTY.
    // --json is a client-side rendering flag and must NOT be forwarded to the daemon.
    // Stop scanning at `--argv`: a `--json`/`-J` in the spawned process's argv
    // payload must not trip client-side JSON rendering (gemini review, PR #431).
    let json_flag = args[1..]
        .iter()
        .take_while(|a| a.as_str() != "--argv")
        .any(|a| a == "--json" || a == "-J");
    let is_tty = std::io::stdout().is_terminal();
    // ab-098967b4: the P1 discovered-live-sessions lane is on by default for
    // `list`; --no-discovered opts out of the ~/.claude/sessions scan.
    let discover_flag = !args[1..]
        .iter()
        .take_while(|a| a.as_str() != "--argv")
        .any(|a| a == "--no-discovered");

    let (method, mut params) = match build_request(verb, &args[1..]) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("fno-agents: {msg}");
            return 2;
        }
    };

    // Resolve the agent name from the PARSED params, not args.get(1): build_request
    // strips leading flags (and their values) when collecting positionals, so
    // `fno agents stop --force worker-A` yields name="worker-A". Reading args.get(1)
    // would capture "--force" and print the wrong success line (gemini-code-assist
    // high on PR #361). Falls back to the raw first positional for verbs that don't
    // set params.name (none of the formatted verbs hit that path today).
    let agent_name = params
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_default();

    let home = AgentsHome::from_env();

    // Claude `ask` is handled entirely client-side (ab-cc926b4e): claude is a
    // `claude --bg` shellout, not a daemon-PTY agent, so it bypasses the daemon
    // RPC. Only claude targets take this path; codex/gemini ask still routes to
    // the daemon below. Resolution: an existing registry row's provider, else
    // the `--provider` flag on first contact.
    if method == "agent.ask" {
        // Task 1.3a: ask never creates. Pre-check the registry before provider
        // resolution: if no row exists for the name, surface the unknown-agent
        // error (exit 16) regardless of --provider. This mirrors Python's
        // dispatch_ask after Task 1.1 (unknown-name check precedes provider
        // selection). Provider-mismatch logic (inside maybe_run_claude_ask) still
        // applies for existing rows.
        {
            use fno_agents::claude_ask::{emit_event, py_repr};
            use fno_agents::state::load_registry;
            // A corrupt/unreadable registry must surface as exit 12 ("registry
            // read failed"), NOT degrade to an empty registry where every name
            // looks unknown (exit 16 + a forensically wrong unknown-name
            // event). Python parity: dispatch_ask raises exit 12 on
            // (OSError, ValueError, RegistryVersionError); the lib dispatch
            // fns do the same. A MISSING file is not an error (load_registry
            // returns the default). Sigma-review finding, this PR.
            let registry = match load_registry(&home.registry_json()) {
                Ok(r) => r,
                Err(e) => {
                    emit_event(
                        &home.events_jsonl(),
                        "agent_ask_failed",
                        &[
                            ("stage", "registry-read".into()),
                            ("name", agent_name.clone().into()),
                            ("error", e.to_string().into()),
                        ],
                    );
                    eprintln!("registry read failed: {e}");
                    return 12;
                }
            };
            if registry.find(&agent_name).is_none() {
                // Event parity: Python's dispatch_ask emits agent_ask_failed
                // stage="unknown-name" before raising; this pre-check is the
                // only emitter on the Rust CLI path (the lib None-arms are
                // unreachable from here once this fires).
                emit_event(
                    &home.events_jsonl(),
                    "agent_ask_failed",
                    &[
                        ("stage", "unknown-name".into()),
                        ("name", agent_name.clone().into()),
                    ],
                );
                eprintln!(
                    "unknown agent {}; spawn it first: fno agents spawn {} -p <provider>",
                    py_repr(&agent_name),
                    agent_name
                );
                return 16;
            }
        }

        if let Some(code) = maybe_run_claude_ask(&home, &params, &agent_name) {
            return code;
        }
        // Codex `ask` is handled client-side (ab-0429c6e1): codex is a
        // one-shot `codex exec --json` subprocess, not a PTY agent, so it
        // bypasses the daemon RPC. Same Option<i32> contract as claude.
        if let Some(code) = maybe_run_codex_ask(&home, &params, &agent_name) {
            return code;
        }
        // Gemini `ask` is handled client-side (ab-73da4ac2): gemini is a
        // one-shot `gemini -p --output-format json` subprocess. Same contract.
        if let Some(code) = maybe_run_gemini_ask(&home, &params, &agent_name) {
            return code;
        }
        // Agy `ask` is intercepted client-side (Phase C): agy is plain-text with
        // no session id, so a stateful resume is unsupported — this surfaces a
        // clear error directing the caller to `spawn --provider agy --once`.
        if let Some(code) = maybe_run_agy_ask(&home, &params, &agent_name) {
            return code;
        }
        // Unconditional flip (ab-73da4ac2): `ask` now auto-routes to this
        // client for every provider, so an ask that matched none of the three
        // provider hooks is a create with no/unknown `--provider`. Surface
        // Python's `select_provider` exit-2 error here rather than falling
        // through to the daemon RPC, whose `handle_ask` PTY screen is the wrong
        // shape for `ask` (Locked Decision 3). The daemon path below is now
        // unreachable for `agent.ask`.
        return unresolvable_ask_exit(&params, &agent_name);
    }

    // Task 1.3a: intercept `spawn` (NOT host/promote, which also map to
    // agent.spawn) to route claude -> dispatch_claude_spawn, and
    // codex/gemini + --once -> dispatch_codex_once / dispatch_gemini_once.
    // `host` and `promote` must fall through to the daemon RPC unchanged.
    if method == "agent.spawn" && verb_owned == "spawn" {
        if let Some(code) = maybe_run_spawn(&home, &params, &agent_name) {
            return code;
        }
        // No client-side handler matched (codex/gemini without --once):
        // fall through to the daemon RPC below.
    }

    let daemon_bin = resolve_daemon_bin();
    // Forward the caller's cwd so a spawned worker launches in the user's
    // project, not the daemon's frozen home dir (fix/agents-host-cwd). Only
    // daemon-bound requests remain here; claude/codex `ask` already returned
    // above. On the rare current_dir() failure we leave params as-is and warn:
    // the daemon then uses its hardened temp-dir fallback (an obviously-wrong
    // /tmp launch) rather than silently adopting its own start dir.
    match std::env::current_dir() {
        Ok(caller) => {
            // --fresh (without --here) stamps the canonical repo root instead of
            // the caller cwd for daemon-bound codex/gemini spawn. An explicit
            // --cwd wins, so when params already carries one we resolve nothing
            // and emit no redirect note (it would falsely claim a redirect that
            // ensure_request_cwd's keep-explicit guard never performs -- review
            // MEDIUM 4); ensure_request_cwd then leaves the explicit --cwd intact.
            let (fresh, here) = fresh_here_flags(&params);
            let explicit_cwd = params.get("cwd").is_some();
            let stamp = if fresh && !here && !explicit_cwd {
                match fno_agents::paths::canonical_repo_root(&caller) {
                    Some(canon) => {
                        note_fresh_redirect(&caller, &canon);
                        canon
                    }
                    None => caller,
                }
            } else {
                caller
            };
            ensure_request_cwd(&method, &mut params, &stamp);
        }
        Err(e) => eprintln!(
            "fno-agents: could not resolve current dir ({e}); daemon will pick a fallback cwd"
        ),
    }
    let req = Request::new(1, method, params);

    match call(&home, &daemon_bin, &req).await {
        Ok(resp) => match resp.payload {
            ResponsePayload::Err(err) => {
                eprintln!("fno-agents: {}", err.message);
                exit_code_for(err.code)
            }
            ResponsePayload::Ok(result) => {
                if let Some(line) = format_success(
                    &verb_owned,
                    &agent_name,
                    &result,
                    json_flag,
                    is_tty,
                    discover_flag,
                ) {
                    // ask FOLLOW-UP prints the reply verbatim with no added
                    // newline, matching Python `sys.stdout.write(result.reply or "")`
                    // (Codex P2 on PR #361 — relevant under FNO_AGENTS_RUNTIME=rust,
                    // the only path that routes ask to this client). Every other
                    // formatted output (ask create short_id, stop/rm/list/reconcile)
                    // keeps the trailing newline.
                    let ask_followup = verb_owned == "ask"
                        && !result
                            .get("created")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                    if ask_followup {
                        print!("{line}");
                    } else {
                        println!("{line}");
                    }
                } else {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&result).unwrap_or_default()
                    );
                }
                // Drift warning on `list` (ab-1891cdff), stderr-only so a
                // `list --json` stdout consumer stays clean (US4). `list` already
                // ensured a daemon is up via `call`; a freshly lazy-started one
                // reads Fresh, so no false warning. A separate status probe keeps
                // this off every other verb's hot path.
                if verb_owned == "list" {
                    let state = check_daemon_drift(&home).await;
                    if let Some(w) = drift_warning(&state, None) {
                        eprintln!("{w}");
                    }
                }
                0
            }
        },
        Err(e) => {
            eprintln!("fno-agents: {e}");
            1
        }
    }
}

/// Route a claude `ask` to the client-side `claude --bg` path, bypassing the
/// daemon (ab-cc926b4e). Returns `Some(exit_code)` when the target is claude
/// (resolved from an existing registry row, else the `--provider` flag), or
/// `None` to fall through to the daemon RPC for codex/gemini.
fn maybe_run_claude_ask(home: &AgentsHome, params: &Value, name: &str) -> Option<i32> {
    use fno_agents::claude_ask::{dispatch_claude_ask, ClaudeHome};
    use fno_agents::state::load_registry;

    let provider_param = params.get("provider").and_then(|v| v.as_str());
    let registry = load_registry(&home.registry_json()).unwrap_or_default();
    let existing_provider = registry.find(name).map(|e| e.provider.clone());

    // Provider mismatch: an existing claude agent plus a conflicting --provider
    // flag. Python's select_provider rejects this as a mismatch; without the
    // check the registry value silently wins and the message is delivered to
    // the wrong provider/session on a stale or mistyped flag (Codex P2).
    if let (Some(ep), Some(pp)) = (existing_provider.as_deref(), provider_param) {
        if ep == "claude" && pp != "claude" {
            eprintln!(
                "fno-agents: agent {name:?} already exists with provider 'claude'; refusing to override with --provider {pp}"
            );
            return Some(2);
        }
    }

    let resolved = existing_provider.as_deref().or(provider_param);
    if resolved != Some("claude") {
        return None; // not a claude target; the daemon path handles it
    }

    let message = params.get("message").and_then(|v| v.as_str()).unwrap_or("");
    let from_name = params
        .get("from_name")
        .and_then(|v| v.as_str())
        .unwrap_or("abilities");
    // Resolve cwd to an absolute path before it reaches the registry row, so a
    // relative --cwd (e.g. ".") isn't later re-normalized against a *listing*
    // process's directory and mis-bucketed under the wrong project (Codex P2).
    // resolve_dispatch_cwd canonicalizes an explicit --cwd (Python's
    // `Path(cwd).resolve()`) and honors --fresh/--here (AC6): --cwd > --fresh >
    // caller cwd.
    let cwd = resolve_dispatch_cwd(params);
    let timeout = params
        .get("timeout")
        .and_then(|v| v.as_u64())
        .map(std::time::Duration::from_secs);
    let yolo = params
        .get("yolo")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let claude_home = ClaudeHome::from_env();
    let outcome = dispatch_claude_ask(
        home,
        &claude_home,
        name,
        message,
        from_name,
        &cwd,
        yolo,
        timeout,
        &[],
    );
    // stderr/stdout carry exact bytes (newlines baked in); write verbatim.
    if !outcome.stderr.is_empty() {
        eprint!("{}", outcome.stderr);
    }
    if !outcome.stdout.is_empty() {
        print!("{}", outcome.stdout);
    }
    Some(outcome.exit_code)
}

/// Route a codex `ask` to the client-side `codex exec` path, bypassing the
/// daemon (ab-0429c6e1). Returns `Some(exit_code)` when the target is codex
/// (resolved from an existing registry row, else the `--provider` flag), or
/// `None` to fall through to the next provider hook.
fn maybe_run_codex_ask(home: &AgentsHome, params: &Value, name: &str) -> Option<i32> {
    fno_agents::codex_ask::maybe_run_codex_ask(home, params, name)
}

/// Route a gemini `ask` to the client-side `gemini -p` path, bypassing the
/// daemon (ab-73da4ac2). Returns `Some(exit_code)` when the target is gemini,
/// or `None` to fall through to the unresolvable-`ask` surface.
fn maybe_run_gemini_ask(home: &AgentsHome, params: &Value, name: &str) -> Option<i32> {
    fno_agents::gemini_ask::maybe_run_gemini_ask(home, params, name)
}

/// Route an agy `ask` to the client-side stateless guard (Phase C). agy is
/// plain-text with no session id, so a stateful resume is unsupported; this
/// returns `Some(2)` with a redirect error for an agy target, else `None`.
fn maybe_run_agy_ask(home: &AgentsHome, params: &Value, name: &str) -> Option<i32> {
    fno_agents::agy_ask::maybe_run_agy_ask(home, params, name)
}

/// Route a `spawn` (NOT host/promote) to the appropriate client-side path.
///
/// x-2c27 names the session substrate as one axis with three values; this arm
/// routes the two non-default ones client-side and falls through for `pane`.
/// - `pane` (default): owned interactive daemon pane -> None (fall through).
/// - claude + `bg`: dispatch_claude_spawn (the detached `claude --bg` thread).
/// - claude + `headless`: dispatch_claude_headless (the `claude -p` one-shot).
/// - codex/gemini/agy + `headless`: dispatch_*_once (one-shot, client-side).
/// - codex/gemini/agy + `bg`: hard error (bg is claude-only -> use headless).
/// - no resolvable / unknown provider: stderr usage error + exit 2.
///
/// Returns `Some(exit_code)` when handled client-side, `None` to fall through.
fn maybe_run_spawn(home: &AgentsHome, params: &Value, name: &str) -> Option<i32> {
    use fno_agents::agy_ask::dispatch_agy_once;
    use fno_agents::claude_ask::{
        dispatch_claude_headless, dispatch_claude_spawn, py_repr, ClaudeHome,
    };
    use fno_agents::codex_ask::dispatch_codex_once;
    use fno_agents::gemini_ask::dispatch_gemini_once;
    use fno_agents::state::load_registry;

    let provider_param = params.get("provider").and_then(|v| v.as_str());
    // `substrate` is a CLIENT-ONLY routing key: build_request validates and
    // inserts it (default `pane`) for the spawn verb and this is its sole
    // consumer. It is never forwarded in a daemon-bound request (the `pane`
    // fall-through below sends params WITHOUT it mattering; the daemon ignores
    // unknown params).
    let substrate = params
        .get("substrate")
        .and_then(|v| v.as_str())
        .unwrap_or("pane");

    // unwrap_or_default is acceptable HERE (unlike the ask pre-check, which
    // must exit 12 on a corrupt registry): this collision check is advisory;
    // the authoritative read happens again under the per-agent lock inside
    // dispatch_claude_spawn / dispatch_*_once, which surface a corrupt
    // registry as exit 12.
    let registry = load_registry(&home.registry_json()).unwrap_or_default();
    let existing_provider = registry.find(name).map(|e| e.provider.clone());

    // Collision check: name already exists -> error.
    // Python: f"agent {name!r} already exists; ..." -> py_repr, not {:?}.
    if existing_provider.is_some() {
        eprintln!(
            "agent {} already exists; use 'fno agents rm {}' first or pick another name",
            py_repr(name),
            name
        );
        return Some(2);
    }

    // Resolve provider from --provider flag (no existing row since collision check passed).
    let provider = match provider_param {
        Some(p) => p,
        None => {
            eprintln!(
                "provider is required to spawn a new agent {}; pass --provider one of: claude, codex, gemini, agy",
                py_repr(name)
            );
            return Some(2);
        }
    };

    let message = params.get("message").and_then(|v| v.as_str()).unwrap_or("");
    let from_name = params
        .get("from_name")
        .and_then(|v| v.as_str())
        .unwrap_or("abilities");
    // --cwd > --fresh > caller cwd (AC6); resolve_dispatch_cwd canonicalizes an
    // explicit --cwd and shells to git only when --fresh && !--here. Resolve only
    // for CLIENT-SIDE spawns, which are the non-`pane` substrates (bg + headless).
    // The `pane` substrate falls through to the daemon RPC below, which resolves
    // canonical itself; resolving here too would double the git call and the
    // redirect note (review MEDIUM 3).
    let cwd = if substrate == "pane" {
        std::path::PathBuf::new()
    } else {
        resolve_dispatch_cwd(params)
    };
    let timeout = params
        .get("timeout")
        .and_then(|v| v.as_u64())
        .map(std::time::Duration::from_secs);
    let yolo = params
        .get("yolo")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // agy honors an optional --model (exact agy model name); other providers ignore it.
    let model = params.get("model").and_then(|v| v.as_str());

    // Validate the provider FIRST so an unknown provider is a client-side
    // error (exit 2) for every substrate, never a fall-through to the daemon.
    if !matches!(provider, "claude" | "codex" | "gemini" | "agy") {
        eprintln!(
            "unknown provider {}; supported: claude, codex, gemini, agy",
            py_repr(provider)
        );
        return Some(2);
    }

    // Each provider module defines its OWN AskOutcome struct (nominally
    // distinct types), so `emit!` prints+returns inline per arm rather than via
    // one shared closure that could not name all four types.
    macro_rules! emit {
        ($outcome:expr) => {{
            let outcome = $outcome;
            if !outcome.stderr.is_empty() {
                eprint!("{}", outcome.stderr);
            }
            if !outcome.stdout.is_empty() {
                print!("{}", outcome.stdout);
            }
            Some(outcome.exit_code)
        }};
    }

    match (provider, substrate) {
        // pane (default): plain `spawn` is an owned interactive daemon pane
        // (host_mode=interactive set in build_request) -> fall through (None) to
        // the daemon RPC, exactly like host. The x-3ab8 default; unchanged.
        (_, "pane") => None,

        // claude bg: the detached `claude --bg` thread (appears in `claude
        // agents`; attach/peek/reply; NOT a grid pane). claude-only by nature.
        ("claude", "bg") => {
            let claude_home = ClaudeHome::from_env();
            emit!(dispatch_claude_spawn(
                home,
                &claude_home,
                name,
                message,
                from_name,
                &cwd,
                yolo,
                timeout,
                &[],
            ))
        }
        // claude headless: a truly headless `claude -p` one-shot (no thread, no
        // grid row; runs to completion and exits). The one place claude shells
        // `-p` (Locked Decision 4); ask/relay keep `--bg`.
        ("claude", "headless") => {
            let claude_home = ClaudeHome::from_env();
            emit!(dispatch_claude_headless(
                &claude_home,
                name,
                message,
                from_name,
                &cwd,
                yolo,
                timeout,
            ))
        }

        // codex/gemini/agy headless: the client-side one-shot (codex --exec /
        // gemini -p / agy -p).
        ("codex", "headless") => emit!(dispatch_codex_once(
            home, name, message, from_name, &cwd, yolo, timeout,
        )),
        ("gemini", "headless") => emit!(dispatch_gemini_once(
            home, name, message, from_name, &cwd, yolo, timeout,
        )),
        ("agy", "headless") => {
            // agy is stateless (plain text, no session id): a one-shot `agy -p`.
            // It ignores `yolo` (headless create always passes
            // --dangerously-skip-permissions) and honors an optional --model.
            emit!(dispatch_agy_once(
                home, name, message, from_name, &cwd, model, timeout,
            ))
        }

        // bg is claude-only (Locked Decision 2): codex/gemini/agy have no
        // detached-interactive substrate. Hard error pointing to headless;
        // never a silent substrate swap.
        (other, "bg") => {
            eprintln!(
                "substrate 'bg' (detached interactive thread) is claude-only; provider {} has no detached-thread substrate - use --substrate headless for a one-shot",
                py_repr(other)
            );
            Some(2)
        }

        // Unreachable: provider is validated known above and substrate is
        // validated to pane|bg|headless in build_request.
        _ => None,
    }
}

/// Surface for an `ask` that resolved to no known provider: a create with no
/// `--provider` (or an unknown one). Reproduces Python's `select_provider`
/// exit-2 error text byte-for-byte (`dispatch.py` wraps both the
/// `_check_known_provider` ValueError and the "provider is required for new
/// agent" ValueError as `DispatchAskError(..., exit_code=2)`, which `cmd_ask`
/// prints to stderr verbatim). Never routes to the daemon (Locked Decision 3).
fn unresolvable_ask_exit(params: &Value, name: &str) -> i32 {
    use fno_agents::claude_ask::py_repr;
    const KNOWN: [&str; 4] = ["claude", "codex", "gemini", "agy"];
    let provider_param = params.get("provider").and_then(|v| v.as_str());
    let msg = match provider_param {
        // `select_provider` validates the requested provider FIRST, so an
        // unknown `--provider` surfaces the "unknown provider" error.
        Some(p) if !KNOWN.contains(&p) => format!(
            "unknown provider {}; supported: claude, codex, gemini, agy",
            py_repr(p)
        ),
        // New agent with no resolvable provider.
        _ => format!(
            "provider is required for new agent {}; pass --provider one of: claude, codex, gemini, agy",
            py_repr(name)
        ),
    };
    eprintln!("{}", msg);
    2
}

/// Dispatch `fno-agents drive <name> [--watch|--step|--paranoid]`.
async fn run_drive(rest: &[String]) -> i32 {
    use fno_agents::drive::DriveMode;
    let mut name: Option<String> = None;
    let mut mode = DriveMode::Interactive;
    for a in rest {
        match a.as_str() {
            "--watch" => mode = DriveMode::Watch,
            "--step" => mode = DriveMode::Step,
            "--paranoid" => mode = DriveMode::Paranoid,
            other if other.starts_with("--") => {
                eprintln!("fno-agents: unknown drive flag: {other}");
                return 2;
            }
            n => {
                // Reject a second positional name rather than silently retargeting
                // (Codex P2): `drive a b` must error, not drive `b`.
                if name.is_some() {
                    eprintln!("fno-agents: drive takes exactly one <name> (got extra: {n})");
                    return 2;
                }
                name = Some(n.to_string());
            }
        }
    }
    let Some(name) = name else {
        eprintln!("fno-agents: drive needs a <name>");
        return 2;
    };
    let home = AgentsHome::from_env();
    let daemon_bin = resolve_daemon_bin();
    fno_agents::drive_client::drive(&home, &daemon_bin, &name, mode).await
}

/// Dispatch `fno-agents status`: probe an already-running daemon and print its
/// `status-v1.json`. Exit 13 when the daemon is down (no lazy-start).
async fn run_status() -> i32 {
    let home = AgentsHome::from_env();
    let req = Request::new(1, "agent.status", Value::Object(Map::new()));
    match call_if_running(&home, &req).await {
        Ok(resp) => match resp.payload {
            ResponsePayload::Err(err) => {
                eprintln!("fno-agents: {}", err.message);
                exit_code_for(err.code)
            }
            ResponsePayload::Ok(result) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).unwrap_or_default()
                );
                // Drift warning (ab-1891cdff), stderr-only so --json/automation
                // consumers of stdout are never contaminated. We already hold the
                // status payload, so classify from it without a second RPC.
                let pid = result
                    .get("daemon")
                    .and_then(|d| d.get("pid"))
                    .and_then(Value::as_u64)
                    .map(|p| p as u32);
                if let Some(w) = drift_warning(&drift_from_status(&result), pid) {
                    eprintln!("{w}");
                }
                0
            }
        },
        Err(ClientError::DaemonNotRunning) => {
            eprintln!("fno-agents: daemon not running");
            13
        }
        Err(e) => {
            eprintln!("fno-agents: {e}");
            1
        }
    }
}

/// `fno agents reap`: manual dead-row garbage collection (x-b1aa). Runs the same
/// `gc_sweep` the daemon runs on its idle tick, operating on the registry
/// directly under the shared flock (no daemon required), and reports what it did:
/// the count removed and, for each row KEPT because its worktree is dirty, the
/// worktree path so the operator can commit/clean it (AC1-UI). The grace window
/// is resolved from `config.agents.dead_row_grace` exactly as the daemon does.
fn run_reap(rest: &[String]) -> i32 {
    let json_out = rest.iter().any(|a| a == "--json" || a == "-J");
    let extras: Vec<&str> = rest
        .iter()
        .map(String::as_str)
        .filter(|a| *a != "--json" && *a != "-J")
        .collect();
    if !extras.is_empty() {
        eprintln!(
            "fno-agents: reap takes no arguments (got: {})",
            extras.join(" ")
        );
        return 2;
    }
    let home = AgentsHome::from_env();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let grace =
        std::time::Duration::from_secs(fno_agents::agents_config::dead_row_grace_secs(&cwd));
    // Source "daemon" matches the event schema's declared source for
    // agent_row_reaped; the manual verb is the same operation as the tick.
    let emitter = fno_agents::events::EventEmitter::new(home.events_jsonl(), "daemon");
    let summary = fno_agents::daemon::gc_sweep(&home, &emitter, grace);

    if json_out {
        let kept: Vec<Value> = summary
            .kept_dirty
            .iter()
            .map(|(id, path)| json!({"id": id, "worktree": path}))
            .collect();
        println!("{}", json!({"reaped": summary.reaped, "kept_dirty": kept}));
    } else {
        println!("reaped {} row(s)", summary.reaped.len());
        for id in &summary.reaped {
            println!("  reaped {id}");
        }
        for (id, path) in &summary.kept_dirty {
            println!("  kept {id} (dirty worktree: {path})");
        }
    }
    0
}

/// Render a restart outcome into (stdout line, optional stderr line, exit code).
/// Pure so the three observable states (swapped / was-down / failed) are unit
/// testable without spawning a daemon. A failure always carries a stderr line
/// and a nonzero code (Locked Decision: a failed restart is loud, never a silent
/// "restarted").
fn render_restart(
    outcome: &Result<RestartOutcome, RestartError>,
) -> (Option<String>, Option<String>, i32) {
    match outcome {
        Ok(RestartOutcome {
            old_pid: Some(old),
            new_pid,
        }) => (Some(format!("restarted: pid {old} -> {new_pid}")), None, 0),
        Ok(RestartOutcome {
            old_pid: None,
            new_pid,
        }) => (
            Some(format!(
                "daemon was not running; started fresh (pid {new_pid})"
            )),
            None,
            0,
        ),
        Err(e) => (None, Some(format!("fno-agents: {e}")), 1),
    }
}

/// Dispatch `fno-agents restart`: swap a (possibly stale) daemon for one built
/// from the current binary. SIGTERM the running daemon (graceful drain; PTY
/// workers survive), wait for the socket to clear, lazy-start fresh.
async fn run_restart() -> i32 {
    let home = AgentsHome::from_env();
    let daemon_bin = resolve_daemon_bin();
    let outcome = restart_daemon(&home, &daemon_bin).await;
    let (out, err, code) = render_restart(&outcome);
    if let Some(line) = out {
        println!("{line}");
    }
    if let Some(line) = err {
        eprintln!("{line}");
    }
    code
}

/// Mint a random UUID (RFC-4122 v4) to pin an interactive claude `--session-id`.
/// The daemon refuses an interactive claude host without a pinned session id
/// (the single-writer claim + transcript discovery key on it); a fresh host
/// supplies one client-side.
// ponytail: v4 from getrandom (the OS CSPRNG), not the `uuid` crate.
// `--session-id` only needs a unique, well-formed UUID -- v7's time-ordering
// buys nothing for a session pin. getrandom is already in the tree, so this
// adds no compile cost and is cross-platform (unlike a `/dev/urandom` read).
fn mint_session_uuid() -> String {
    let mut b = [0u8; 16];
    if getrandom::fill(&mut b).is_err() {
        // Never panic: mix wall-clock nanos with the pid. Collision is
        // implausible for a session pin and getrandom is the real path.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mix = nanos ^ ((std::process::id() as u128) << 96);
        b = mix.to_be_bytes();
    }
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // RFC-4122 variant
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Build (method, params) from a verb and its flags.
/// Apply the owned-interactive (drivable grid pane) defaults to a spawn/host
/// request. Sets `host_mode=interactive`; for claude additionally defaults the
/// PTY lane (`mode=interactive`) and mints a `session_id` when none is pinned or
/// resumed (the daemon's single-writer claim + transcript discovery key on it).
///
/// Shared by `host` (always interactive) and `spawn` (default for PTY providers
/// unless `--once`) so the claude mint lives in exactly ONE place (x-3ab8). An
/// explicit `--mode` wins, so `--mode stream_json` opts a claude spawn back out
/// of the PTY lane. Non-claude providers get only `host_mode`; their create argv
/// stays byte-unchanged (the mint is claude-only, mirroring the host contract).
fn apply_interactive_defaults(params: &mut Map<String, Value>) {
    params.insert(
        "host_mode".into(),
        Value::String(fno_agents::state::HOST_MODE_INTERACTIVE.into()),
    );
    if params.get("provider").and_then(Value::as_str) == Some("claude") {
        // claude has two interactive lanes; default the owned-PTY pane unless the
        // caller explicitly picked one via --mode.
        if !params.contains_key("mode") {
            params.insert(
                "mode".into(),
                Value::String(fno_agents::state::CLAUDE_MODE_INTERACTIVE.into()),
            );
        }
        let is_pty_lane = params.get("mode").and_then(Value::as_str)
            == Some(fno_agents::state::CLAUDE_MODE_INTERACTIVE);
        if is_pty_lane && !params.contains_key("session_id") && !params.contains_key("resume_id") {
            params.insert("session_id".into(), Value::String(mint_session_uuid()));
        }
    }
}

fn build_request(verb: &str, rest: &[String]) -> Result<(String, Value), String> {
    let mut params = Map::new();
    let mut positional: Vec<String> = Vec::new();
    let mut argv: Option<Vec<String>> = None;

    // Click/Typer accepts `--flag=value` for every string option; the Python
    // path forwards e.g. `fno agents ask <name> <msg> --cwd=/repo --timeout=30
    // --from-name=bot --provider=codex` verbatim. Since `ask` now auto-routes to
    // this client for EVERY provider (ab-73da4ac2), the binary must accept the
    // equals form for ALL value-carrying flags, not just --provider/--from --
    // otherwise a routed `--cwd=...` / `--timeout=...` / `--from-name=...`
    // regresses to "unknown flag" instead of reaching the dispatch (Codex P2 on
    // PR #379; same regression class as PR #371's --provider=). Normalize
    // `--flag=value` into two tokens up front so the space-form match arms below
    // handle both syntaxes uniformly.
    const VALUE_FLAGS: &[&str] = &[
        "--provider",
        "--from",
        "--cwd",
        "--message",
        "--session-id",
        "--cc-session-id",
        "--channel-id",
        "--status",
        "--from-name",
        "--timeout",
        "--model",
        "--mode",
        "--substrate",
    ];
    let mut normalized: Vec<String> = Vec::with_capacity(rest.len());
    let mut rest_iter = rest.iter();
    while let Some(tok) = rest_iter.next() {
        // Everything after a bare `--argv` is the provider command line, which
        // the `--argv` match arm below collects verbatim. Do NOT normalize
        // equals-form tokens in that payload -- a downstream tool's
        // `--timeout=5` must survive untouched (the prior per-token splitting
        // never reached the payload because `--argv` drained the iterator
        // first; the up-front pass would otherwise corrupt it). Copy the rest
        // verbatim and stop.
        if tok == "--argv" {
            normalized.push(tok.clone());
            normalized.extend(rest_iter.cloned());
            break;
        }
        // ab-3ff64151: the equals-form split is for LONG flags only. The short
        // value flags (-p/-c/-t) take a space-separated value (`-p claude`),
        // matching Click/Typer's short-option convention; the `-p=value` form is
        // intentionally not normalized here. The phone-motivating surface types
        // the space form, and shorts are additive aliases, not a new syntax.
        if tok.starts_with("--") {
            if let Some(eq) = tok.find('=') {
                if VALUE_FLAGS.contains(&&tok[..eq]) {
                    normalized.push(tok[..eq].to_string());
                    normalized.push(tok[eq + 1..].to_string());
                    continue;
                }
            }
        }
        normalized.push(tok.clone());
    }

    let mut it = normalized.into_iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--provider" | "-p" => {
                params.insert("provider".into(), str_arg(&mut it, "--provider")?);
            }
            "--from" => {
                // `promote <name> --from <session-uuid>`: the session to resume
                // interactively. Forwarded as `resume_id` (the daemon infers the
                // provider from the source row).
                params.insert("resume_id".into(), str_arg(&mut it, "--from")?);
            }
            "--cwd" | "-c" => {
                params.insert("cwd".into(), str_arg(&mut it, "--cwd")?);
            }
            "--message" => {
                params.insert("message".into(), str_arg(&mut it, "--message")?);
            }
            "--session-id" => {
                params.insert("session_id".into(), str_arg(&mut it, "--session-id")?);
            }
            "--mode" => {
                // Disambiguates claude's two interactive-host lanes: `interactive`
                // (PTY pane, subscription-billed) vs the default stream-json adopt.
                // The daemon reads `mode`; codex/gemini ignore it. (`drive --mode`
                // is a different parser and never reaches build_request.)
                params.insert("mode".into(), str_arg(&mut it, "--mode")?);
            }
            "--cc-session-id" => {
                params.insert("cc_session_id".into(), str_arg(&mut it, "--cc-session-id")?);
            }
            "--channel-id" => {
                params.insert("mcp_channel_id".into(), str_arg(&mut it, "--channel-id")?);
            }
            "--status" => {
                params.insert("status".into(), str_arg(&mut it, "--status")?);
            }
            "--json" | "-J" => {
                // Task 3.1: --json is a client-side rendering flag. We recognize it
                // here so it is not rejected as "unknown flag". It is NOT forwarded
                // to the daemon as a param. The caller captures it separately.
                // ab-3ff64151: -J is the global-register short for --json.
            }
            "--all" | "-A" => {
                params.insert("all".into(), Value::Bool(true));
            }
            "--discovered" | "--no-discovered" => {
                // ab-098967b4: client-side rendering flags for the `list`
                // discovered-live-sessions lane. Recognized here so they are not
                // rejected as unknown; captured separately at the call site and
                // never forwarded to the daemon.
            }
            "--force" | "-F" => {
                params.insert("force".into(), Value::Bool(true));
            }
            "--model" => {
                // agy honors an exact model name (`agy models`); other providers
                // ignore the param. Forwarded so `spawn --provider agy --once
                // --model <name>` reaches dispatch_agy_once (codex P2).
                params.insert("model".into(), str_arg(&mut it, "--model")?);
            }
            "--from-name" => {
                // NOTE: --from-name is accepted and forwarded to the daemon, but
                // the daemon's handle_ask currently ignores it (PTY path does not
                // apply the envelope wrapper yet). Accepted without error for
                // Python flag-parity; the daemon will wire it when the envelope
                // lands (Wave 5/6 follow-up).
                params.insert("from_name".into(), str_arg(&mut it, "--from-name")?);
            }
            "--yolo" | "-Y" => {
                // NOTE: --yolo is accepted and forwarded; daemon ignores it for now.
                params.insert("yolo".into(), Value::Bool(true));
            }
            "--substrate" => {
                // The session-substrate selector (x-2c27): pane (owned-PTY,
                // default) | bg (claude --bg detached thread, claude-only) |
                // headless (claude -p / codex --exec / agy -p one-shot). The
                // sole routing key the spawn arm reads (replaces --once).
                let v = str_arg(&mut it, "--substrate")?;
                match v.as_str() {
                    Some("pane") | Some("bg") | Some("headless") => {
                        params.insert("substrate".into(), v);
                    }
                    other => {
                        return Err(format!(
                            "--substrate must be one of: pane, bg, headless (got {})",
                            other.unwrap_or("")
                        ));
                    }
                }
            }
            "--once" | "-o" => {
                // Back-compat alias: every live `--once` caller is a codex/gemini
                // one-shot, i.e. headless. Map it to --substrate headless so old
                // callers keep working without the conflated `once` boolean. An
                // explicit --substrate already present wins.
                params
                    .entry("substrate")
                    .or_insert_with(|| Value::String("headless".into()));
            }
            "--fresh" => {
                // Resolve the worker cwd to the canonical repo root (main
                // checkout) regardless of caller cwd. Opt-in; --cwd still wins.
                params.insert("fresh".into(), Value::Bool(true));
            }
            "--here" | "--in-place" => {
                // Explicit opt-out of --fresh: keep the worker in the caller's
                // cwd (the policy layer passes this to override a default --fresh).
                params.insert("here".into(), Value::Bool(true));
            }
            "--timeout" | "-t" => {
                let val = str_arg(&mut it, "--timeout")?;
                let secs: u64 = val
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| "--timeout needs a numeric value")?;
                params.insert("timeout".into(), Value::Number(secs.into()));
            }
            "--argv" => {
                // Everything after --argv is the provider command line. The
                // documented syntax is `--argv -- <cmd...>`; strip a single
                // leading `--` separator so the worker does not try to exec
                // the literal "--" as argv[0] (Codex P1).
                let mut rest: Vec<String> = it.by_ref().collect();
                if rest.first().map(|s| s == "--").unwrap_or(false) {
                    rest.remove(0);
                }
                argv = Some(rest);
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown flag: {other}"));
            }
            _ => positional.push(a),
        }
    }

    if let Some(av) = argv {
        params.insert(
            "argv".into(),
            Value::Array(av.into_iter().map(Value::String).collect()),
        );
    }

    let method = match verb {
        "spawn" => {
            let name = positional.first().ok_or("spawn needs a <name>")?;
            params.insert("name".into(), Value::String(name.clone()));
            // Remaining positionals after name form the message if not flagged.
            if !params.contains_key("message") && positional.len() > 1 {
                params.insert("message".into(), Value::String(positional[1..].join(" ")));
            }
            // x-3ab8/x-2c27: spawn defaults to an owned interactive pane (the
            // `pane` substrate) for PTY-capable providers. Only `pane` gets the
            // interactive host_mode/mint; `bg` (claude --bg) and `headless`
            // (-p/--exec) are client-side one-shots that never touch the daemon
            // (byte-unchanged: no host_mode, no mint). An unknown provider keeps
            // today's behavior (the daemon's provider_for_pty errors as before).
            let substrate = params
                .get("substrate")
                .and_then(Value::as_str)
                .unwrap_or("pane");
            let pty_capable = matches!(
                params.get("provider").and_then(Value::as_str),
                Some("claude") | Some("codex") | Some("gemini") | Some("agy")
            );
            if substrate == "pane" && pty_capable {
                apply_interactive_defaults(&mut params);
            }
            "agent.spawn"
        }
        "ask" => {
            let name = positional.first().ok_or("ask needs a <name>")?;
            params.insert("name".into(), Value::String(name.clone()));
            if !params.contains_key("message") && positional.len() > 1 {
                params.insert("message".into(), Value::String(positional[1..].join(" ")));
            }
            "agent.ask"
        }
        "host" => {
            // `host` is now a thin alias for `spawn --mode interactive` (x-3ab8):
            // `host <name> --provider codex|gemini|claude|agy ["<task>"]`. It always
            // forces the owned interactive lane via the SAME helper `spawn` uses, so
            // the two verbs produce identical params (no second mint path). Kept as a
            // distinct routable verb for callers/docs/muscle-memory; not deprecated.
            // Empty task -> bare interactive session.
            let name = positional.first().ok_or("host needs a <name>")?;
            params.insert("name".into(), Value::String(name.clone()));
            if !params.contains_key("message") && positional.len() > 1 {
                params.insert("message".into(), Value::String(positional[1..].join(" ")));
            }
            apply_interactive_defaults(&mut params);
            "agent.spawn"
        }
        "promote" => {
            // Promote an existing session to a live interactive host:
            // `promote <name> --from <session-uuid>`. The daemon infers the
            // provider from the source row, so --provider is not required.
            let name = positional.first().ok_or("promote needs a <name>")?;
            params.insert("name".into(), Value::String(name.clone()));
            if !params.contains_key("resume_id") {
                return Err("promote needs --from <session-uuid>".into());
            }
            params.insert(
                "host_mode".into(),
                Value::String(fno_agents::state::HOST_MODE_INTERACTIVE.into()),
            );
            "agent.spawn"
        }
        "list" => "agent.list",
        "status" => "agent.status",
        "stop" => {
            let name = positional.first().ok_or("stop needs a <name>")?;
            params.insert("name".into(), Value::String(name.clone()));
            "agent.stop"
        }
        "rm" => {
            let name = positional.first().ok_or("rm needs a <name>")?;
            params.insert("name".into(), Value::String(name.clone()));
            "agent.rm"
        }
        "reconcile" => "agent.reconcile",
        "register-channel" => {
            // Help advertises `register-channel --cc-session-id <id> [<name>]`;
            // map the optional positional name so the daemon can resolve the
            // target agent by name on first registration (Codex P2).
            if let Some(name) = positional.first() {
                params.insert("name".into(), Value::String(name.clone()));
            }
            "channel.register_channel"
        }
        "unregister-channel" => "channel.unregister_channel",
        "push-channel" => "channel.push_to_channel",
        other => return Err(format!("unknown verb: {other}")),
    };

    Ok((method.to_string(), Value::Object(params)))
}

/// Stamp the caller's working directory into daemon-bound spawn/ask requests.
///
/// The `fno-agents` daemon is a single long-lived process shared across every
/// project, so its own `std::env::current_dir()` is frozen to wherever it was
/// first lazy-started; it cannot stand in for "the directory the user ran the
/// command from". Only the client sits in the user's directory, so the client
/// must forward `cwd`; otherwise a worker spawned from project A lands in the
/// daemon's home project B (e.g. `fno agents host` opening codex in the wrong
/// repo). An explicit `--cwd` already in `params` always wins.
///
/// `agent.spawn` covers `spawn`/`host`/`promote`; `agent.ask` covers gemini's
/// first-contact auto-spawn (claude/codex `ask` resolve cwd client-side before
/// reaching this send path, so they never depend on it).
fn ensure_request_cwd(method: &str, params: &mut Value, cwd: &std::path::Path) {
    if method != "agent.spawn" && method != "agent.ask" {
        return;
    }
    // build_request always returns Value::Object for these methods; assert it
    // so a future caller passing a non-object is caught in debug rather than
    // silently skipping the cwd stamp.
    debug_assert!(params.is_object(), "spawn/ask params must be a JSON object");
    if let Some(obj) = params.as_object_mut() {
        if !obj.contains_key("cwd") {
            obj.insert(
                "cwd".to_string(),
                Value::String(cwd.to_string_lossy().into_owned()),
            );
        }
    }
}

/// Canonicalize a `--cwd` string to an absolute path, matching Python's
/// `Path(cwd).resolve()`: prefer `std::fs::canonicalize`, falling back to a
/// join against the caller cwd for a relative path that does not exist yet.
/// Extracted from the previously-duplicated claude-ask / spawn cwd blocks.
fn canonicalize_cwd(c: &str) -> std::path::PathBuf {
    std::fs::canonicalize(c).unwrap_or_else(|_| {
        let p = std::path::PathBuf::from(c);
        if p.is_absolute() {
            p
        } else {
            std::env::current_dir().map(|d| d.join(&p)).unwrap_or(p)
        }
    })
}

/// Read the `fresh` / `here` booleans a caller set via `--fresh` /
/// `--here`(`--in-place`). Both default to false: `--fresh` is an opt-in
/// mechanism, never on by default at the client layer (the policy layer decides
/// when to pass it -- AC3 keeps non-target verbs on caller cwd unless asked).
fn fresh_here_flags(params: &Value) -> (bool, bool) {
    let fresh = params
        .get("fresh")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let here = params
        .get("here")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    (fresh, here)
}

/// Pure cwd precedence for a spawn/ask dispatch: explicit `--cwd` > `--fresh`
/// (canonical) > caller cwd. `--here` suppresses `--fresh`; an unresolved
/// canonical (None) falls back to the caller cwd, the safe side. No git / env /
/// IO, so the precedence is unit-testable (AC6; Failure Modes > Invariants:
/// `--cwd` is the highest-priority cwd source and wins over `--fresh`).
fn effective_worker_cwd(
    explicit_cwd: Option<std::path::PathBuf>,
    fresh: bool,
    here: bool,
    canonical: Option<std::path::PathBuf>,
    caller: std::path::PathBuf,
) -> std::path::PathBuf {
    if let Some(c) = explicit_cwd {
        return c; // explicit --cwd always wins
    }
    if fresh && !here {
        return canonical.unwrap_or(caller);
    }
    caller
}

/// One-line stderr note when `--fresh` actually moves the worker cwd off the
/// caller's dir, so the redirect is never silent (Failure Modes > Errors).
fn note_fresh_redirect(caller: &std::path::Path, chosen: &std::path::Path) {
    if chosen != caller {
        eprintln!(
            "fno-agents: --fresh: dispatching from canonical main ({}); pass --here to stay in this worktree",
            chosen.display()
        );
    }
}

/// Resolve the worker cwd for a client-side (claude/codex) spawn/ask dispatch,
/// honoring `--cwd` > `--fresh` > caller-cwd. Shells to git only when `--fresh`
/// is set without `--here` and no explicit `--cwd`; emits the redirect note on an
/// actual move. Single source of cwd truth for the two client-side dispatch
/// blocks (claude `ask`, claude `spawn`).
fn resolve_dispatch_cwd(params: &Value) -> std::path::PathBuf {
    let caller = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let explicit = params
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(canonicalize_cwd);
    let (fresh, here) = fresh_here_flags(params);
    let canonical = if explicit.is_none() && fresh && !here {
        fno_agents::paths::canonical_repo_root(&caller)
    } else {
        None
    };
    let chosen = effective_worker_cwd(explicit.clone(), fresh, here, canonical, caller.clone());
    if explicit.is_none() && fresh && !here {
        note_fresh_redirect(&caller, &chosen);
    }
    chosen
}

fn str_arg(
    it: &mut std::iter::Peekable<impl Iterator<Item = String>>,
    flag: &str,
) -> Result<Value, String> {
    it.next()
        .map(Value::String)
        .ok_or_else(|| format!("{flag} needs a value"))
}

/// Format a successful daemon response for human-readable stdout.
///
/// Returns `Some(line)` for verbs with a defined output contract, `None` for
/// verbs that still use the generic `serde_json::to_string_pretty` fallback.
///
/// - `stop`: prints `stopped: <name> (<short_id>)` using the `short_id` the
///   daemon now includes in every stop success payload. Falls back to
///   `stopped: <name>` when `short_id` is absent (e.g. an old daemon).
/// - `rm`: prints `removed: <name>` (the client already has the name as the
///   positional arg; no field from the daemon payload is needed).
/// - `list`: Task 3.1 — JSON when `json_flag` or not a TTY; table otherwise.
/// - `reconcile`: Task 3.1 — JSON when `json_flag` or not a TTY; human summary otherwise.
fn format_success(
    verb: &str,
    name: &str,
    result: &Value,
    json_flag: bool,
    is_tty: bool,
    discover: bool,
) -> Option<String> {
    match verb {
        "ask" => {
            // Create path (first contact): daemon returns {created: true, short_id: "..."}.
            // Python prints exactly `<short_id>\n` (no banner).
            if result
                .get("created")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                let short_id = result
                    .get("short_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                Some(short_id.to_string())
            } else {
                // Follow-up path: print the reply verbatim (no added newline; println!
                // in the caller adds the newline, matching Python's behaviour).
                let reply = result.get("reply").and_then(|v| v.as_str()).unwrap_or("");
                Some(reply.to_string())
            }
        }
        "stop" => {
            if let Some(short_id) = result.get("short_id").and_then(|v| v.as_str()) {
                Some(format!("stopped: {name} ({short_id})"))
            } else {
                Some(format!("stopped: {name}"))
            }
        }
        "rm" => Some(format!("removed: {name}")),
        "list" => {
            let agents = &result["agents"];
            let filters = result
                .get("filters_applied")
                .cloned()
                .unwrap_or_else(|| json!({"cwd": null, "provider": null, "status": null}));
            // ab-098967b4: merge the P1 host-local live-session lane. The Rust
            // client owns the rendered surface, so it shells out to the Python
            // helper (which has psutil's cross-platform reuse-safe liveness) and
            // folds the result in. Fail-open: an empty lane on any error.
            let discovered = if discover {
                fetch_discovered_sessions(filters.get("cwd").and_then(|v| v.as_str()))
            } else {
                Vec::new()
            };
            if json_flag || !is_tty {
                Some(render_list_json(agents, &filters, &discovered))
            } else {
                Some(render_list_table(agents, &discovered))
            }
        }
        "reconcile" => {
            if json_flag || !is_tty {
                Some(render_reconcile_json(result))
            } else {
                Some(render_reconcile_human(result))
            }
        }
        "spawn" => {
            // x-3ab8: PTY-provider spawns now route through the daemon (owned
            // interactive pane) instead of the client-side claude `--bg` lane.
            // Emit the SAME compact single-line JSON receipt that lane produced
            // ({"name","short_id","provider","status"}) so receipt parsers
            // (dispatch-node.sh, backlog/advance.py) keep working across the move.
            // serde_json::to_string (NOT _pretty) keeps it one line for the
            // line-by-line `json.loads` consumers. `--once` spawns are handled
            // client-side and never reach here.
            let short_id = result
                .get("short_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let provider = result
                .get("provider")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let status = result
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("live");
            Some(
                serde_json::to_string(&json!({
                    "name": name,
                    "short_id": short_id,
                    "provider": provider,
                    "status": status,
                }))
                .unwrap_or_default(),
            )
        }
        "host" | "promote" => {
            // AC1-UI: a live spawn outcome is visible -- name, short_id, provider,
            // status, and how to drive it. (A spawn-failed worker returns
            // Response::Err, handled by the caller's nonzero-exit path, so this
            // arm only renders the success case.)
            let short_id = result
                .get("short_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let provider = result
                .get("provider")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let status = result
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("live");
            if json_flag || !is_tty {
                Some(
                    serde_json::to_string_pretty(&json!({
                        "name": name,
                        "short_id": short_id,
                        "provider": provider,
                        "status": status,
                        "host_mode": fno_agents::state::HOST_MODE_INTERACTIVE,
                    }))
                    .unwrap_or_default(),
                )
            } else if provider == "claude" {
                // claude is the stream-json lane (no PTY): observe with `watch`,
                // drive a turn with `send`, never `drive`/`grid`.
                Some(format!(
                    "adopted claude session as stream thread {name} ({short_id}), status={status}\n\
                     observe with: fno agents watch {name}   (drive a turn: fno mail send {name} \"...\")"
                ))
            } else {
                Some(format!(
                    "hosted interactive {provider} agent {name} ({short_id}), status={status}\n\
                     drive with: fno agents grid {name}   (or: fno agents drive {name} --mode interactive)"
                ))
            }
        }
        _ => None,
    }
}

/// Render agents list as Python-matching JSON (Task 3.1; discovered lane
/// ab-098967b4).
///
/// Shape (schema_version 2): `{"agents": [...], "count": N,
/// "discovered_sessions": [...], "discovered_count": M, "filters_applied":
/// {...}, "schema_version": 2}`. Stays byte-shape-aligned with Python's
/// `format.render_json`.
fn render_list_json(agents: &Value, filters_applied: &Value, discovered: &[Value]) -> String {
    let count = agents.as_array().map(|a| a.len()).unwrap_or(0);
    let payload = json!({
        "agents": agents,
        "count": count,
        "discovered_sessions": discovered,
        "discovered_count": discovered.len(),
        "filters_applied": filters_applied,
        "schema_version": 2,
    });
    serde_json::to_string_pretty(&payload).unwrap_or_default()
}

/// Shell out to the Python `fno agents discovered-json` helper for the P1
/// discovered-live-sessions lane and return the rows (ab-098967b4).
///
/// The Rust client owns the `list` rendered surface, but discovery lives in
/// Python (it needs psutil's cross-platform process create-time for the
/// reuse-safe liveness the design requires; the Rust-native liveness degrades
/// to existence-only on macOS). Fail-open by contract: a missing `fno`, a
/// non-zero exit, or unparseable output yields an empty lane so `list` is
/// never broken by discovery (US5). `FNO_AGENTS_RUNTIME=python` pins the child
/// to the Python dispatch so it cannot recurse back into this binary.
fn fetch_discovered_sessions(cwd_filter: Option<&str>) -> Vec<Value> {
    use std::process::Command;

    let mut cmd = Command::new("fno");
    cmd.args(["agents", "discovered-json"]);
    cmd.env("FNO_AGENTS_RUNTIME", "python");
    if let Some(c) = cwd_filter {
        cmd.args(["--cwd", c]);
    }
    let output = match cmd.output() {
        Ok(o) if o.status.success() => o.stdout,
        _ => return Vec::new(),
    };
    let parsed: Value = match serde_json::from_slice(&output) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    parsed
        .get("discovered_sessions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Compact single-unit age for the CHECKED column: the largest whole unit of
/// the elapsed seconds -- `3s`, `4m`, `18h`, `2d` (plan ab-70faa65b, AC2-EDGE).
/// Negative input (a row reconciled in the "future" via clock skew) clamps to
/// `0s` rather than rendering a misleading negative age.
fn format_age_secs(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86400)
    }
}

/// Render `last_reconciled_at` (raw RFC3339, or None) as the CHECKED cell:
/// `never` when never probed, the compact age otherwise, or `?` when the stored
/// timestamp cannot be parsed (explicit, never blank -- Silent-Failure check).
fn render_checked(last_reconciled_at: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> String {
    match last_reconciled_at {
        None => "never".to_string(),
        Some(ts) => match chrono::DateTime::parse_from_rfc3339(ts) {
            Ok(then) => format_age_secs((now - then.with_timezone(&chrono::Utc)).num_seconds()),
            Err(_) => "?".to_string(),
        },
    }
}

/// Render agents list as a human-readable table (Task 3.1; CHECKED/PID added by
/// plan ab-70faa65b, Architecture C).
///
/// Columns: NAME PROVIDER STATUS CHECKED PID LAST MESSAGE CWD. CHECKED is the
/// relative age since the last reconcile probe (`never` when unprobed); it
/// replaces the old always-`-` LIVE column (AC5-UI). PID is the worker pid for a
/// PTY agent (`-` for a one-shot ask, which has no managed process). This is a
/// functional table; byte-exact match with Python is not required (Python's
/// table is time-dependent via relative timestamps).
fn render_list_table(agents: &Value, discovered: &[Value]) -> String {
    let headers = [
        "NAME",
        "PROVIDER",
        "STATUS",
        "CHECKED",
        "PID",
        "LAST MESSAGE",
        "CWD",
    ];
    let empty_arr = vec![];
    let rows = agents.as_array().unwrap_or(&empty_arr);
    let now = chrono::Utc::now();

    // Compute display values for each row
    let display: Vec<[String; 7]> = rows
        .iter()
        .map(|r| {
            let name = r["name"].as_str().unwrap_or("-").to_string();
            let provider = r["provider"].as_str().unwrap_or("-").to_string();
            let status = r["status"].as_str().unwrap_or("-").to_string();
            let checked = render_checked(r["last_reconciled_at"].as_str(), now);
            let pid = r["pid"]
                .as_u64()
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string());
            let last_msg = r["last_message_at"].as_str().unwrap_or("-").to_string();
            let cwd = r["cwd"].as_str().unwrap_or("-").to_string();
            [name, provider, status, checked, pid, last_msg, cwd]
        })
        .collect();

    // Column widths: max of header and data
    let mut widths = [
        headers[0].len(),
        headers[1].len(),
        headers[2].len(),
        headers[3].len(),
        headers[4].len(),
        headers[5].len(),
        headers[6].len(),
    ];
    for row in &display {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let mut lines = Vec::new();
    // Header row
    let header_line = headers
        .iter()
        .enumerate()
        .map(|(i, h)| format!("{:width$}", h, width = widths[i]))
        .collect::<Vec<_>>()
        .join(" ");
    lines.push(header_line.trim_end().to_string());
    // Data rows
    for row in &display {
        let data_line = row
            .iter()
            .enumerate()
            .map(|(i, cell)| format!("{:width$}", cell, width = widths[i]))
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(data_line.trim_end().to_string());
    }
    let mut out = lines.join("\n") + "\n";
    if !discovered.is_empty() {
        out.push_str(&render_discovered_section(discovered));
    }
    out
}

/// Render the host-local discovered-live-sessions lane below the registry
/// table (ab-098967b4, AC1-UI). A blank line + banner make it visually
/// distinct. Columns: HANDLE (friendly alias) STATUS PROJECT HEX CWD.
fn render_discovered_section(discovered: &[Value]) -> String {
    let headers = ["HANDLE", "STATUS", "PROJECT", "HEX", "CWD"];
    let display: Vec<[String; 5]> = discovered
        .iter()
        .map(|r| {
            let handle = r["handle"].as_str().unwrap_or("-").to_string();
            let status = r["status"].as_str().unwrap_or("-").to_string();
            let project = r["project"].as_str().unwrap_or("-").to_string();
            let hex = r["short_id"].as_str().unwrap_or("-").to_string();
            let cwd = r["cwd"].as_str().unwrap_or("-").to_string();
            [handle, status, project, hex, cwd]
        })
        .collect();

    let mut widths = [
        headers[0].len(),
        headers[1].len(),
        headers[2].len(),
        headers[3].len(),
        headers[4].len(),
    ];
    for row in &display {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let mut lines = Vec::new();
    lines.push(String::new()); // blank separator line
    lines.push(format!(
        "DISCOVERED LIVE SESSIONS ({}, host-local)",
        display.len()
    ));
    lines.push(
        headers
            .iter()
            .enumerate()
            .map(|(i, h)| format!("{:width$}", h, width = widths[i]))
            .collect::<Vec<_>>()
            .join(" ")
            .trim_end()
            .to_string(),
    );
    for row in &display {
        lines.push(
            row.iter()
                .enumerate()
                .map(|(i, cell)| format!("{:width$}", cell, width = widths[i]))
                .collect::<Vec<_>>()
                .join(" ")
                .trim_end()
                .to_string(),
        );
    }
    lines.join("\n") + "\n"
}

/// Render reconcile result as Python-matching JSON (Task 3.1).
///
/// Shape: `{"scanned": N, "orphaned": [...], "recovered": [...], "skipped": [...], "errors": [...]}`
/// Matches Python cmd_reconcile's JSON payload exactly.
fn render_reconcile_json(result: &Value) -> String {
    // The daemon now returns scanned/orphaned/recovered/skipped/errors directly.
    let payload = json!({
        "scanned": result.get("scanned").cloned().unwrap_or(Value::Null),
        "orphaned": result.get("orphaned").cloned().unwrap_or_else(|| json!([])),
        "recovered": result.get("recovered").cloned().unwrap_or_else(|| json!([])),
        "skipped": result.get("skipped").cloned().unwrap_or_else(|| json!([])),
        "errors": result.get("errors").cloned().unwrap_or_else(|| json!([])),
    });
    serde_json::to_string(&payload).unwrap_or_default() + "\n"
}

/// Render reconcile result as human-readable summary (Task 3.1).
fn render_reconcile_human(result: &Value) -> String {
    let scanned = result["scanned"].as_u64().unwrap_or(0);
    let orphaned = result["orphaned"].as_array().map(|a| a.len()).unwrap_or(0);
    let recovered = result["recovered"].as_array().map(|a| a.len()).unwrap_or(0);
    let skipped = result["skipped"].as_array().map(|a| a.len()).unwrap_or(0);
    let errors = result["errors"].as_array().map(|a| a.len()).unwrap_or(0);
    format!(
        "scanned: {scanned}  orphaned: {orphaned}  recovered: {recovered}  skipped: {skipped}  errors: {errors}\n"
    )
}

/// Map a daemon error code to the design's verb exit codes.
fn exit_code_for(code: ErrorCode) -> i32 {
    match code {
        ErrorCode::AgentNotFound | ErrorCode::AgentExists | ErrorCode::InvalidStatus => 13,
        ErrorCode::SpawnFailed => 14,
        ErrorCode::LockTimeout => 15,
        ErrorCode::Busy => 18,
        ErrorCode::InvalidParams | ErrorCode::MalformedFrame | ErrorCode::UnknownMethod => 2,
        ErrorCode::ChannelUnknown => 13,
        ErrorCode::Internal => 1,
    }
}

/// Usage line per dispatchable verb; the leading token is the verb name and the
/// slice order is the `--help` display order. This MUST cover every routable
/// verb (the `build_request` match arms plus the directly-dispatched specials).
/// `test_rust_client_verbs_match_client_rs` (Python) guards client.rs<->router
/// parity; `print_help_lists_every_routable_verb` (below) guards this display
/// list against that set, so a new verb cannot land without a `--help` entry
/// (ab-351427cb).
const CLIENT_VERB_USAGE: &[&str] = &[
    "spawn <name> --provider <p> [--substrate pane|bg|headless] [--cwd <dir>|--fresh|--here] --argv -- <cmd...>",
    "ask <name> <message> [--cwd <dir>|--fresh|--here]",
    "list [--all]",
    "status",
    "restart",
    "reap [--json]",
    "stop <name> [--force]",
    "rm <name> [--force]",
    "drive <name> [--watch|--step|--paranoid]",
    "grid [<name>...] [--all]",
    "loop-check --state <target-state.md> --transcript <transcript.jsonl> --cwd <project-root> [--events <events.jsonl>] [--global-events <global.jsonl>] [--settings <settings.yaml>] [--ledger <ledger.json>] [--now <rfc3339>] [--gh-bin <path>] [--git-bin <path>]",
    "finalize --state <target-state.md> --cwd <project-root> --reason <TerminationReason> [--transcript <transcript.jsonl>]",
    "reconcile",
    "register-channel --cc-session-id <id> [<name>]",
    "unregister-channel --channel-id <id>",
    "push-channel --channel-id <id>",
    // ab-351427cb: these verbs are dispatchable in build_request / as specials
    // but were missing from --help, and `<verb> --help` errored instead of
    // printing usage.
    "host <name> --provider codex|gemini|claude [--mode interactive] [<task>]",
    "promote <name> --from <session-uuid> [--provider claude]",
    "drive-authority [--json]",
    "trace [options]",
    "ping",
    "resume <name> [--print-command]",
    "attach <name>",
    "logs <name> [--follow] [options]",
    "loop run --driver target|megawalk [options]",
    "report --session-id <uuid> --seq <n> --state working|blocked|done [--reason <text>] [--ttl-ms <n>]",
];

/// Return the usage line for `verb` (matched on the leading token), or `None`
/// for an unrecognized verb.
fn verb_usage(verb: &str) -> Option<&'static str> {
    CLIENT_VERB_USAGE
        .iter()
        .copied()
        .find(|usage| usage.split_whitespace().next() == Some(verb))
}

/// True when `--help`/`-h` appears in the verb's OWN options, i.e. before an
/// `--argv`/`--` payload boundary. A `--help` after that boundary belongs to a
/// spawned command's argv (e.g. `spawn wk --provider codex --argv -- tool
/// --help`) and must not be captured as our per-verb help request
/// (ab-351427cb review: gemini HIGH / codex P2).
fn is_help_request(opts: &[String]) -> bool {
    let boundary = opts
        .iter()
        .position(|a| a == "--" || a == "--argv")
        .unwrap_or(opts.len());
    opts[..boundary].iter().any(|a| a == "--help" || a == "-h")
}

fn print_help() {
    println!(
        "{}",
        json!({
            "binary": "fno-agents",
            "verbs": CLIENT_VERB_USAGE,
        })
    );
}

/// Print the binary's embedded version: package version + the source git rev it
/// was built from + a dirty flag, all baked in by `build.rs` (ab-24a59d50). The
/// `--json` form is the machine surface `fno doctor` reads off the resolved
/// binary to decide Rust-side staleness without an external marker.
/// The machine-readable version payload `fno doctor` reads off the resolved
/// binary (`version --json`). `crates_rev` is the crates/ subtree rev the
/// rust-staleness verdict keys on (ab-716cd330) -- the same quantity Python's
/// `update._rust_subtree_rev` computes, so the comparison is apples-to-apples.
/// `git_rev` stays the full HEAD the binary was built from (identity).
fn version_json() -> serde_json::Value {
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    json!({
        "package": env!("CARGO_PKG_VERSION"),
        "git_rev": env!("FNO_AGENTS_GIT_REV"),       // full sha, or the literal "unknown"
        "crates_rev": env!("FNO_AGENTS_CRATES_REV"), // crates/ subtree rev, or "unknown"
        "dirty": env!("FNO_AGENTS_GIT_DIRTY") == "1",
        "profile": profile,
    })
}

fn print_version(json_out: bool) {
    let pkg = env!("CARGO_PKG_VERSION");
    let rev = env!("FNO_AGENTS_GIT_REV"); // full sha, or the literal "unknown"
    let dirty = env!("FNO_AGENTS_GIT_DIRTY") == "1";
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    if json_out {
        println!("{}", version_json());
    } else {
        let short = if rev.len() >= 12 { &rev[..12] } else { rev };
        let suffix = if dirty { "-dirty" } else { "" };
        println!("fno-agents {pkg} ({short}{suffix}, {profile})");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fno_agents::{emit_schema_json, state::AgentState, AgentStatus, KNOWN_EVENT_KINDS};
    use std::path::Path;

    // -----------------------------------------------------------------------
    // ab-716cd330: version --json carries the crates/ subtree rev fno doctor reads
    // -----------------------------------------------------------------------

    #[test]
    fn version_json_carries_crates_rev_and_git_rev() {
        // build.rs always sets both env vars (falling back to "unknown"), so the
        // keys are present for `fno doctor` to read the rust-staleness signal
        // off the resolved binary without an external marker.
        let v = version_json();
        assert!(v.get("crates_rev").and_then(|x| x.as_str()).is_some());
        assert!(v.get("git_rev").and_then(|x| x.as_str()).is_some());
    }

    // -----------------------------------------------------------------------
    // ab-351427cb: --help parity (top-level verb list + per-verb usage)
    // -----------------------------------------------------------------------

    #[test]
    fn print_help_lists_every_routable_verb() {
        // Mirror of RUST_CLIENT_VERBS (cli/src/fno/agents/rust_runtime.py).
        // The Python parity test guards client.rs<->router; this guards the
        // `--help` display list so a routable verb can't be missing from it.
        let expected = [
            "spawn",
            "ask",
            "list",
            "status",
            "restart",
            "reap",
            "stop",
            "rm",
            "reconcile",
            "drive",
            "grid",
            "register-channel",
            "unregister-channel",
            "push-channel",
            "drive-authority",
            "trace",
            "ping",
            "resume",
            "attach",
            "logs",
            "host",
            "promote",
            "loop-check",
            "loop",
            "finalize",
            "report",
        ];
        let listed: std::collections::HashSet<&str> = CLIENT_VERB_USAGE
            .iter()
            .map(|u| u.split_whitespace().next().expect("usage has a verb token"))
            .collect();
        for verb in expected {
            assert!(
                listed.contains(verb),
                "verb {verb} missing from --help (CLIENT_VERB_USAGE)"
            );
        }
        assert_eq!(
            listed.len(),
            expected.len(),
            "CLIENT_VERB_USAGE has an extra or duplicate verb vs RUST_CLIENT_VERBS"
        );
    }

    #[test]
    fn verb_usage_resolves_known_and_rejects_unknown() {
        // The nine verbs that ab-351427cb added must each resolve a usage line.
        for verb in [
            "host",
            "promote",
            "trace",
            "ping",
            "resume",
            "attach",
            "logs",
            "drive-authority",
            "loop",
        ] {
            assert!(
                verb_usage(verb).is_some(),
                "verb_usage({verb}) should resolve"
            );
        }
        // `loop` and `loop-check` are distinct leading tokens (no prefix collision).
        assert!(verb_usage("loop").unwrap().starts_with("loop run"));
        assert!(verb_usage("loop-check").unwrap().starts_with("loop-check"));
        assert!(verb_usage("definitely-not-a-verb").is_none());
    }

    #[test]
    fn help_request_respects_argv_boundary() {
        // ab-351427cb review (gemini HIGH / codex P2): a `--help` in the verb's
        // own options is a help request; a `--help` after an `--argv`/`--`
        // boundary belongs to the spawned command and must NOT be captured.
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();

        // Verb's own --help / -h -> help request.
        assert!(is_help_request(&s(&["wk", "--help"])));
        assert!(is_help_request(&s(&["--help"])));
        assert!(is_help_request(&s(&["-h"])));

        // --help inside a spawn/host argv payload -> NOT a help request.
        assert!(!is_help_request(&s(&[
            "wk",
            "--provider",
            "codex",
            "--argv",
            "--",
            "tool",
            "--help"
        ])));
        assert!(!is_help_request(&s(&["wk", "--argv", "tool", "--help"])));

        // --help after a bare `--` end-of-options separator -> NOT a help request.
        assert!(!is_help_request(&s(&["wk", "--", "--help"])));

        // No help flag at all.
        assert!(!is_help_request(&s(&["wk", "--provider", "codex"])));
    }

    // -----------------------------------------------------------------------
    // W7: --emit-schema unit tests (struct-drift guard + JSON parse check)
    // -----------------------------------------------------------------------

    /// AC2-HP: emit_schema_json() must produce valid JSON containing the
    /// required top-level keys (envelope, status, event_kinds).
    #[test]
    fn emit_schema_json_has_required_keys() {
        let schema = emit_schema_json();
        assert!(schema.get("envelope").is_some(), "missing 'envelope' key");
        assert!(schema.get("status").is_some(), "missing 'status' key");
        assert!(
            schema.get("event_kinds").is_some(),
            "missing 'event_kinds' key"
        );
    }

    /// AC2-HP: The emitted schema must serialize to valid JSON (round-trip check).
    #[test]
    fn emit_schema_round_trips_as_json() {
        let schema = emit_schema_json();
        let s = serde_json::to_string(&schema).expect("schema must serialize");
        let back: serde_json::Value = serde_json::from_str(&s).expect("re-parse must succeed");
        assert_eq!(schema, back);
    }

    /// Bidirectional struct-drift guard for AgentState + PtyStateWire.
    ///
    /// Direction 1 (schema ⊆ struct): every property key in the emitted
    /// status schema must exist as a serialized AgentState field. A property
    /// added to emit_schema_json() without a corresponding struct field is
    /// caught here.
    ///
    /// Direction 2 (struct ⊆ schema): every serialized AgentState field must
    /// appear in the emitted status schema properties. A new AgentState field
    /// forgotten in emit_schema_json() is caught here.
    ///
    /// The same bidirectional check is applied to the pty sub-object vs the
    /// on-disk PtyStateWire flat fields (active, drive_active, drive_session_id,
    /// drive_mode, last_heartbeat_at_monotonic_ns).
    #[test]
    fn emit_schema_status_properties_match_agent_state_fields() {
        use fno_agents::state::PtyState;

        // --- AgentState (pty: None) ---
        let sample = AgentState {
            schema_version: 1,
            short_id: "wkA".into(),
            status: AgentStatus::Ready,
            ready: true,
            last_message_at: Some("2026-01-01T00:00:00Z".into()),
            last_reply: Some("hello".into()),
            restart_count: 2,
            last_restart_at: Some("2026-01-01T00:00:01Z".into()),
            pty: None,
        };
        let serialized = serde_json::to_value(&sample).expect("AgentState must serialize");
        let struct_keys: std::collections::HashSet<String> = serialized
            .as_object()
            .expect("must be object")
            .keys()
            .cloned()
            .collect();

        let schema = emit_schema_json();
        let schema_props = schema["status"]["properties"]
            .as_object()
            .expect("status.properties must be object");
        let schema_keys: std::collections::HashSet<String> = schema_props.keys().cloned().collect();

        // Direction 1: schema_props ⊆ struct_keys
        for key in &schema_keys {
            assert!(
                struct_keys.contains(key.as_str()),
                "emitted status schema has property {key:?} not in serialized AgentState: {struct_keys:?}"
            );
        }

        // Direction 2: struct_keys ⊆ schema_props
        for key in &struct_keys {
            assert!(
                schema_keys.contains(key.as_str()),
                "AgentState field {key:?} not in emitted status schema properties: {schema_keys:?}"
            );
        }

        // --- PtyState / PtyStateWire bidirectional check ---
        // Serialize a PtyState WITH drive active so all optional wire fields
        // (drive_session_id, drive_mode, last_heartbeat_at_monotonic_ns) are
        // present in the output. Using default() (no drive) omits them via
        // skip_serializing_if, which would make Direction 1 trivially pass
        // while hiding that the schema has properties absent from the struct.
        use fno_agents::state::DriveWindow;
        let pty_sample = PtyState {
            active: true,
            drive: Some(DriveWindow {
                session_id: Some("sess-1".into()),
                mode: Some("interactive".into()),
                last_heartbeat_at_monotonic_ns: Some(123_456_789),
            }),
        };
        let pty_json = serde_json::to_value(&pty_sample).expect("PtyState must serialize");
        let pty_struct_keys: std::collections::HashSet<String> = pty_json
            .as_object()
            .expect("pty must be object")
            .keys()
            .cloned()
            .collect();

        // The emitted pty schema is the second branch of the oneOf (type: object).
        let pty_schema_obj = schema["status"]["properties"]["pty"]["oneOf"]
            .as_array()
            .expect("pty oneOf must be array")
            .iter()
            .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("object"))
            .expect("pty oneOf must have an object branch");
        let pty_schema_props = pty_schema_obj["properties"]
            .as_object()
            .expect("pty object branch must have properties");
        let pty_schema_keys: std::collections::HashSet<String> =
            pty_schema_props.keys().cloned().collect();

        // Direction 1: pty schema_props ⊆ pty wire keys
        for key in &pty_schema_keys {
            assert!(
                pty_struct_keys.contains(key.as_str()),
                "emitted pty schema has property {key:?} not in serialized PtyState wire: {pty_struct_keys:?}"
            );
        }

        // Direction 2: pty wire keys ⊆ pty schema_props
        for key in &pty_struct_keys {
            assert!(
                pty_schema_keys.contains(key.as_str()),
                "PtyState wire field {key:?} not in emitted pty schema properties: {pty_schema_keys:?}"
            );
        }
    }

    /// AC2-HP: KNOWN_EVENT_KINDS must be non-empty and contain the canonical kinds.
    #[test]
    fn known_event_kinds_are_non_empty_and_contain_canonical() {
        assert!(!KNOWN_EVENT_KINDS.is_empty());
        assert!(KNOWN_EVENT_KINDS.contains(&"agent_spawned"));
        assert!(KNOWN_EVENT_KINDS.contains(&"daemon_started"));
        assert!(KNOWN_EVENT_KINDS.contains(&"event_payload_too_large"));
    }

    // -----------------------------------------------------------------------
    // Task 2.1: format_success per-verb output (stop/rm stdout parity)
    // -----------------------------------------------------------------------

    /// AC1-HP: stop with short_id in result -> "stopped: <name> (<short_id>)"
    #[test]
    fn format_success_stop_with_short_id() {
        let result = json!({"stopped": true, "short_id": "fo-1a2b"});
        let out = format_success("stop", "foo", &result, false, true, false);
        assert_eq!(out, Some("stopped: foo (fo-1a2b)".to_string()));
    }

    /// AC1-HP: stop fallback when short_id absent -> "stopped: <name>"
    #[test]
    fn format_success_stop_without_short_id() {
        let result = json!({"stopped": true});
        let out = format_success("stop", "foo", &result, false, true, false);
        assert_eq!(out, Some("stopped: foo".to_string()));
    }

    /// AC1-HP: rm -> "removed: <name>"
    #[test]
    fn format_success_rm() {
        let result = json!({"removed": true, "was_orphaned": false});
        let out = format_success("rm", "bar-agent", &result, false, true, false);
        assert_eq!(out, Some("removed: bar-agent".to_string()));
    }

    /// AC2-HP: unknown verb returns None (falls back to pretty-print).
    /// `spawn` is NOT unknown post-x-3ab8 (it renders a receipt, covered by
    /// `format_success_spawn_emits_compact_receipt`); use a truly unhandled verb.
    #[test]
    fn format_success_unknown_verb_returns_none() {
        let result = json!({"spawned": true});
        assert_eq!(
            format_success("bogus-verb", "worker", &result, false, true, false),
            None
        );
        // list and reconcile now have their own rendering (not None)
        assert_eq!(
            format_success("status", "worker", &result, false, true, false),
            None
        );
    }

    // -----------------------------------------------------------------------
    // ab-1891cdff: `restart` outcome rendering (AC2-HP / AC2-EDGE / AC2-FR)
    // -----------------------------------------------------------------------

    #[test]
    fn render_restart_reports_old_to_new() {
        // AC2-HP: a swap reports `restarted: pid OLD -> NEW` on stdout, exit 0.
        let (out, err, code) = render_restart(&Ok(RestartOutcome {
            old_pid: Some(91627),
            new_pid: 91999,
        }));
        assert_eq!(out.as_deref(), Some("restarted: pid 91627 -> 91999"));
        assert_eq!(err, None);
        assert_eq!(code, 0);
    }

    #[test]
    fn render_restart_reports_fresh_when_down() {
        // AC2-EDGE: no daemon was running -> started fresh, no error, exit 0.
        let (out, err, code) = render_restart(&Ok(RestartOutcome {
            old_pid: None,
            new_pid: 42,
        }));
        assert_eq!(
            out.as_deref(),
            Some("daemon was not running; started fresh (pid 42)")
        );
        assert_eq!(err, None);
        assert_eq!(code, 0);
    }

    #[test]
    fn render_restart_failure_is_loud() {
        // AC2-FR: a SIGTERM failure carries a stderr line naming the pid + reason
        // and a nonzero exit; no false "restarted" on stdout.
        let (out, err, code) = render_restart(&Err(RestartError::SigtermFailed {
            pid: 91627,
            reason: "Operation not permitted (os error 1)".to_string(),
        }));
        assert_eq!(out, None);
        let err = err.expect("failure has a stderr line");
        assert!(err.contains("91627"), "names the pid");
        assert!(err.contains("SIGTERM"), "names the failure");
        assert_ne!(code, 0, "failure exits nonzero");

        // A did-not-exit timeout is equally loud and names the pid.
        let (_o, err2, code2) = render_restart(&Err(RestartError::DidNotExit { pid: 5, secs: 5 }));
        assert!(err2.unwrap().contains("did not exit"));
        assert_ne!(code2, 0);
    }

    // -----------------------------------------------------------------------
    // Task 4.1: ask create-on-first-contact + follow-up output parity
    // -----------------------------------------------------------------------

    /// AC1-HP (create): ask with created=true in result prints "<short_id>\n" only.
    #[test]
    fn format_success_bg_create_prints_short_id() {
        let result = json!({"created": true, "short_id": "cx-1a2b3c"});
        let out = format_success("ask", "myagent", &result, false, true, false);
        assert_eq!(out, Some("cx-1a2b3c".to_string()));
    }

    /// AC1-HP (follow-up): ask without created prints the reply verbatim (no added newline).
    #[test]
    fn format_success_ask_followup_prints_reply_verbatim() {
        let reply = "Here is my answer to your question.";
        let result = json!({"reply": reply, "backend": "pty"});
        let out = format_success("ask", "myagent", &result, false, true, false);
        assert_eq!(out, Some(reply.to_string()));
    }

    /// AC2-ERR: ask follow-up with empty reply prints empty string (not None).
    #[test]
    fn format_success_ask_followup_empty_reply() {
        let result = json!({"reply": "", "backend": "pty"});
        let out = format_success("ask", "myagent", &result, false, true, false);
        assert_eq!(out, Some(String::new()));
    }

    /// AC3-HP: build_request accepts --from-name, --yolo, --timeout without error.
    /// These flags are forwarded to the daemon so `ask` can be called with full
    /// Python-parity flag surface without exit 2 (unknown flag).
    #[test]
    fn ask_accepts_from_name_yolo_timeout_flags() {
        let args = vec![
            "myagent".to_string(),
            "hello there".to_string(),
            "--from-name".to_string(),
            "abilities".to_string(),
            "--yolo".to_string(),
            "--timeout".to_string(),
            "30".to_string(),
        ];
        let result = build_request("ask", &args);
        assert!(
            result.is_ok(),
            "build_request must accept --from-name/--yolo/--timeout: {result:?}"
        );
        let (method, params) = result.unwrap();
        assert_eq!(method, "agent.ask");
        assert_eq!(params["name"], "myagent");
        assert_eq!(params["from_name"], "abilities");
        assert_eq!(params["yolo"], true);
        assert_eq!(params["timeout"], 30u64);
    }

    /// Codex P2 (PR #379): with `ask` unconditionally auto-routed, the binary
    /// must accept the Click/Typer `--flag=value` equals form for EVERY
    /// value-carrying option, not just --provider/--from. Without the
    /// normalization, `--cwd=/repo` / `--timeout=30` / `--from-name=bot` would
    /// regress to "unknown flag" instead of reaching the dispatch.
    #[test]
    fn ask_accepts_equals_form_for_all_value_flags() {
        let args = vec![
            "myagent".to_string(),
            "hello there".to_string(),
            "--provider=gemini".to_string(),
            "--cwd=/repo".to_string(),
            "--timeout=30".to_string(),
            "--from-name=bot".to_string(),
        ];
        let result = build_request("ask", &args);
        assert!(
            result.is_ok(),
            "equals-form ask flags must parse: {result:?}"
        );
        let (method, params) = result.unwrap();
        assert_eq!(method, "agent.ask");
        assert_eq!(params["name"], "myagent");
        assert_eq!(params["provider"], "gemini");
        assert_eq!(params["cwd"], "/repo");
        assert_eq!(params["timeout"], 30u64);
        assert_eq!(params["from_name"], "bot");
        // A value containing '=' (e.g. a path) splits only on the first '='.
        let (_m, p2) = build_request(
            "ask",
            &["a".to_string(), "m".to_string(), "--cwd=/a=b".to_string()],
        )
        .unwrap();
        assert_eq!(p2["cwd"], "/a=b");
    }

    /// codex P2 (PR #73): `--model` must reach the request, else
    /// `spawn --provider agy --once --model <name>` fails with "unknown flag"
    /// before dispatch_agy_once sees it. Both space- and equals-form parse.
    #[test]
    fn spawn_forwards_model_flag() {
        let (_m, space) = build_request(
            "spawn",
            &[
                "wk".to_string(),
                "--provider".to_string(),
                "agy".to_string(),
                "--once".to_string(),
                "--model".to_string(),
                "Gemini 3.5 Flash (High)".to_string(),
            ],
        )
        .expect("--model must parse");
        assert_eq!(space["model"], "Gemini 3.5 Flash (High)");
        let (_m2, eq) = build_request("spawn", &["wk".to_string(), "--model=pro".to_string()])
            .expect("--model= must parse");
        assert_eq!(eq["model"], "pro");
    }

    /// ab-3ff64151 AC1 (Rust-path parity): `agents ask` accepts the phone shorts
    /// `-p`/`-c`/`-t` and the global `-Y`, building the byte-identical request
    /// the long flags would. This is the cross-language parity guard from the
    /// design's highest-risk task: short flags MUST reach dispatch on the
    /// Rust-routed `ask`, not just the Python `typer.Option` path.
    #[test]
    fn ask_accepts_phone_short_flags() {
        let short = build_request(
            "ask",
            &[
                "myagent".to_string(),
                "hi".to_string(),
                "-p".to_string(),
                "claude".to_string(),
                "-c".to_string(),
                "/repo".to_string(),
                "-t".to_string(),
                "30".to_string(),
                "-Y".to_string(),
            ],
        )
        .expect("short flags must parse on the Rust ask path");
        let long = build_request(
            "ask",
            &[
                "myagent".to_string(),
                "hi".to_string(),
                "--provider".to_string(),
                "claude".to_string(),
                "--cwd".to_string(),
                "/repo".to_string(),
                "--timeout".to_string(),
                "30".to_string(),
                "--yolo".to_string(),
            ],
        )
        .expect("long flags must parse");
        assert_eq!(
            short, long,
            "short flags must build the same request as long flags"
        );
        let (method, params) = short;
        assert_eq!(method, "agent.ask");
        assert_eq!(params["name"], "myagent");
        assert_eq!(params["provider"], "claude");
        assert_eq!(params["cwd"], "/repo");
        assert_eq!(params["timeout"], 30u64);
        assert_eq!(params["yolo"], true);
    }

    /// ab-3ff64151 AC2 (Rust-path parity): the global-register boolean shorts
    /// the client recognizes (`-A` --all, `-F` --force) parse identically to the
    /// long forms on the verbs that use them. `-J` --json is client-side (not a
    /// build_request param) and is covered by the json-detection path.
    #[test]
    fn global_register_boolean_shorts_parse() {
        let (_m, all_params) = build_request("list", &["-A".to_string()]).expect("-A must parse");
        assert_eq!(all_params["all"], true);
        let (_m, force_params) =
            build_request("rm", &["myagent".to_string(), "-F".to_string()]).expect("-F must parse");
        assert_eq!(force_params["force"], true);
    }

    /// AC4-HP: spawn with provider and no --argv succeeds (uses provider-derived argv).
    #[test]
    fn spawn_without_argv_with_known_provider_succeeds() {
        // After Task 4.1, spawn with a known --provider and no --argv should
        // build the request without error (the daemon resolves argv from the provider).
        let args = vec![
            "myagent".to_string(),
            "--provider".to_string(),
            "codex".to_string(),
        ];
        let result = build_request("spawn", &args);
        assert!(
            result.is_ok(),
            "spawn with --provider (no --argv) must not error: {result:?}"
        );
        let (method, params) = result.unwrap();
        assert_eq!(method, "agent.spawn");
        assert_eq!(params["name"], "myagent");
        assert_eq!(params["provider"], "codex");
        // argv must be absent from params so the daemon knows to use provider-derived argv.
        assert!(
            params.get("argv").is_none(),
            "argv must be absent when using provider-derived argv"
        );
    }

    fn argv_of(params: &Value) -> Vec<String> {
        params["argv"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn spawn_strips_leading_double_dash_from_argv() {
        // Documented syntax: `spawn worker --argv -- sleep 60`. The `--`
        // separator must not become argv[0] (Codex P1).
        let args = vec![
            "worker".to_string(),
            "--argv".to_string(),
            "--".to_string(),
            "sleep".to_string(),
            "60".to_string(),
        ];
        let (method, params) = build_request("spawn", &args).unwrap();
        assert_eq!(method, "agent.spawn");
        assert_eq!(argv_of(&params), vec!["sleep", "60"]);
        assert_eq!(params["name"], "worker");
    }

    #[test]
    fn spawn_argv_without_separator_is_unchanged() {
        let args = vec![
            "worker".to_string(),
            "--argv".to_string(),
            "codex".to_string(),
            "exec".to_string(),
        ];
        let (_m, params) = build_request("spawn", &args).unwrap();
        assert_eq!(argv_of(&params), vec!["codex", "exec"]);
    }

    /// Sigma-review (PR #379): the equals-form normalization must NOT touch the
    /// `--argv` payload. A downstream tool's `--timeout=5` in the provider
    /// command line must survive verbatim, not get split into `--timeout 5`.
    #[test]
    fn spawn_argv_payload_equals_form_survives_normalization() {
        let args = vec![
            "worker".to_string(),
            "--argv".to_string(),
            "--".to_string(),
            "mytool".to_string(),
            "--timeout=5".to_string(),
            "--cwd=/x".to_string(),
        ];
        let (_m, params) = build_request("spawn", &args).unwrap();
        assert_eq!(argv_of(&params), vec!["mytool", "--timeout=5", "--cwd=/x"]);
        // The verb's own --timeout/--cwd are NOT set from the payload tokens.
        assert!(params.get("timeout").is_none());
        assert!(params.get("cwd").is_none());
    }

    #[test]
    fn register_channel_maps_positional_name() {
        let args = vec![
            "--cc-session-id".to_string(),
            "cc-1".to_string(),
            "worker-A".to_string(),
        ];
        let (method, params) = build_request("register-channel", &args).unwrap();
        assert_eq!(method, "channel.register_channel");
        assert_eq!(params["name"], "worker-A");
        assert_eq!(params["cc_session_id"], "cc-1");
    }

    // -----------------------------------------------------------------------
    // Interactive-drive verbs (host / promote): build_request dispatch
    // -----------------------------------------------------------------------

    #[test]
    fn host_builds_interactive_spawn_with_message() {
        let args = vec![
            "worker".to_string(),
            "--provider".to_string(),
            "codex".to_string(),
            "do".to_string(),
            "the".to_string(),
            "thing".to_string(),
        ];
        let (method, params) = build_request("host", &args).unwrap();
        assert_eq!(method, "agent.spawn");
        assert_eq!(params["name"], "worker");
        assert_eq!(params["provider"], "codex");
        assert_eq!(params["message"], "do the thing");
        assert_eq!(params["host_mode"], "interactive");
    }

    #[test]
    fn host_empty_task_is_bare_interactive() {
        let args = vec![
            "worker".to_string(),
            "--provider".to_string(),
            "gemini".to_string(),
        ];
        let (method, params) = build_request("host", &args).unwrap();
        assert_eq!(method, "agent.spawn");
        assert_eq!(params["host_mode"], "interactive");
        assert!(
            params.get("message").is_none(),
            "no message -> bare session"
        );
    }

    #[test]
    fn host_needs_a_name() {
        let args = vec!["--provider".to_string(), "codex".to_string()];
        assert!(build_request("host", &args).is_err());
    }

    #[test]
    fn mint_session_uuid_is_well_formed_v4() {
        let u = mint_session_uuid();
        let parts: Vec<&str> = u.split('-').collect();
        assert_eq!(parts.len(), 5, "uuid has five dash-separated groups: {u}");
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12],
            "uuid group widths: {u}"
        );
        assert!(
            u.chars().all(|c| c == '-' || c.is_ascii_hexdigit()),
            "uuid is hex + dashes: {u}"
        );
        // version nibble (group 3, first char) is '4'; variant nibble (group 4,
        // first char) is one of 8/9/a/b.
        assert_eq!(parts[2].chars().next().unwrap(), '4', "v4 version: {u}");
        assert!(
            matches!(parts[3].chars().next().unwrap(), '8' | '9' | 'a' | 'b'),
            "rfc-4122 variant: {u}"
        );
        assert_ne!(mint_session_uuid(), u, "two mints differ");
    }

    #[test]
    fn host_claude_interactive_mints_session_id() {
        let args = vec![
            "wk-c".to_string(),
            "--provider".to_string(),
            "claude".to_string(),
            "--mode".to_string(),
            "interactive".to_string(),
        ];
        let (method, params) = build_request("host", &args).unwrap();
        assert_eq!(method, "agent.spawn");
        assert_eq!(params["provider"], "claude");
        assert_eq!(params["mode"], "interactive");
        assert_eq!(params["host_mode"], "interactive");
        let sid = params["session_id"].as_str().expect("minted session_id");
        assert_eq!(sid.split('-').count(), 5, "minted a uuid: {sid}");
    }

    #[test]
    fn host_claude_interactive_accepts_equals_form_mode() {
        // codex P2 PR#88: `--mode=interactive` must normalize like the other
        // value flags, not fall through to "unknown flag".
        let args = vec![
            "wk-c".to_string(),
            "--provider".to_string(),
            "claude".to_string(),
            "--mode=interactive".to_string(),
        ];
        let (_m, params) = build_request("host", &args).unwrap();
        assert_eq!(params["mode"], "interactive");
        assert!(
            params["session_id"].as_str().is_some(),
            "equals-form mode still mints the session id"
        );
    }

    #[test]
    fn host_claude_interactive_keeps_explicit_session_id() {
        let args = vec![
            "wk-c".to_string(),
            "--provider".to_string(),
            "claude".to_string(),
            "--mode".to_string(),
            "interactive".to_string(),
            "--session-id".to_string(),
            "019e7157-4236-7bb1-b274-ebbac6040ace".to_string(),
        ];
        let (_m, params) = build_request("host", &args).unwrap();
        assert_eq!(params["session_id"], "019e7157-4236-7bb1-b274-ebbac6040ace");
    }

    #[test]
    fn host_codex_does_not_mint_session_id() {
        // The mint is claude+interactive only; codex must be byte-unchanged.
        let args = vec![
            "wk-x".to_string(),
            "--provider".to_string(),
            "codex".to_string(),
            "--mode".to_string(),
            "interactive".to_string(),
        ];
        let (_m, params) = build_request("host", &args).unwrap();
        assert!(
            params.get("session_id").is_none(),
            "no mint for codex interactive host"
        );
    }

    // -----------------------------------------------------------------------
    // spawn defaults to an owned interactive pane (x-3ab8)
    // -----------------------------------------------------------------------

    #[test]
    fn spawn_defaults_interactive_for_pty_providers() {
        // AC1-HP: spawn --provider <pty> (no --once) -> host_mode=interactive.
        // codex/gemini/agy never mint a session id (claude-only).
        for provider in ["codex", "gemini", "agy"] {
            let args = vec![
                "wk".to_string(),
                "--provider".to_string(),
                provider.to_string(),
            ];
            let (method, params) = build_request("spawn", &args).unwrap();
            assert_eq!(method, "agent.spawn");
            assert_eq!(
                params["host_mode"], "interactive",
                "{provider} default-interactive"
            );
            assert!(params.get("session_id").is_none(), "{provider} never mints");
        }
    }

    #[test]
    fn spawn_claude_default_is_pty_lane_with_minted_session() {
        // claude default -> PTY lane (mode=interactive) + a minted session id.
        let args = vec![
            "wk".to_string(),
            "--provider".to_string(),
            "claude".to_string(),
        ];
        let (_m, params) = build_request("spawn", &args).unwrap();
        assert_eq!(params["host_mode"], "interactive");
        assert_eq!(params["mode"], "interactive");
        let sid = params["session_id"].as_str().expect("minted session_id");
        assert_eq!(sid.split('-').count(), 5, "minted a uuid: {sid}");
    }

    #[test]
    fn spawn_once_is_headless_byte_unchanged() {
        // AC1-EDGE: --once is the back-compat alias for --substrate headless ->
        // no host_mode, no mint, for EVERY provider; substrate=headless.
        for provider in ["claude", "codex", "gemini", "agy"] {
            let args = vec![
                "wk".to_string(),
                "--provider".to_string(),
                provider.to_string(),
                "--once".to_string(),
            ];
            let (_m, params) = build_request("spawn", &args).unwrap();
            assert_eq!(
                params.get("substrate").and_then(|v| v.as_str()),
                Some("headless"),
                "{provider} --once aliases to substrate=headless"
            );
            assert!(
                params.get("host_mode").is_none(),
                "{provider} --once: no host_mode"
            );
            assert!(
                params.get("session_id").is_none(),
                "{provider} --once: no mint"
            );
        }
    }

    #[test]
    fn spawn_substrate_pane_is_default_and_interactive() {
        // AC1-UI: no --substrate -> pane -> interactive defaults applied (the
        // x-3ab8 owned-PTY behavior is the strictly-additive default).
        let args = vec![
            "wk".to_string(),
            "--provider".to_string(),
            "claude".to_string(),
        ];
        let (_m, params) = build_request("spawn", &args).unwrap();
        assert!(
            params.get("substrate").is_none(),
            "no substrate key when omitted"
        );
        assert_eq!(params["host_mode"], "interactive");
        // Explicit --substrate pane is identical (interactive defaults applied).
        let args = vec![
            "wk".to_string(),
            "--provider".to_string(),
            "claude".to_string(),
            "--substrate".to_string(),
            "pane".to_string(),
        ];
        let (_m, params) = build_request("spawn", &args).unwrap();
        assert_eq!(params["substrate"], "pane");
        assert_eq!(params["host_mode"], "interactive");
    }

    #[test]
    fn spawn_substrate_bg_and_headless_suppress_interactive() {
        // bg + headless are client-side one-shots: no host_mode, no mint.
        for sub in ["bg", "headless"] {
            let args = vec![
                "wk".to_string(),
                "--provider".to_string(),
                "claude".to_string(),
                "--substrate".to_string(),
                sub.to_string(),
            ];
            let (_m, params) = build_request("spawn", &args).unwrap();
            assert_eq!(params["substrate"], sub);
            assert!(params.get("host_mode").is_none(), "{sub}: no host_mode");
            assert!(params.get("session_id").is_none(), "{sub}: no mint");
        }
    }

    #[test]
    fn spawn_substrate_rejects_unknown_value() {
        let args = vec![
            "wk".to_string(),
            "--provider".to_string(),
            "claude".to_string(),
            "--substrate".to_string(),
            "detached".to_string(),
        ];
        let err = build_request("spawn", &args).unwrap_err();
        assert!(err.contains("--substrate must be one of"), "got: {err}");
    }

    #[test]
    fn spawn_explicit_substrate_wins_over_once_alias() {
        // --substrate set explicitly is not clobbered by a trailing --once.
        let args = vec![
            "wk".to_string(),
            "--provider".to_string(),
            "claude".to_string(),
            "--substrate".to_string(),
            "bg".to_string(),
            "--once".to_string(),
        ];
        let (_m, params) = build_request("spawn", &args).unwrap();
        assert_eq!(params["substrate"], "bg");
    }

    #[test]
    fn spawn_unknown_provider_does_not_force_interactive() {
        // AC1-EDGE (Boundaries): an unknown provider keeps today's behavior; the
        // daemon's provider_for_pty errors on it as before, so we must NOT force
        // host_mode (which would change the error surface).
        let args = vec![
            "wk".to_string(),
            "--provider".to_string(),
            "opencode".to_string(),
        ];
        let (_m, params) = build_request("spawn", &args).unwrap();
        assert!(
            params.get("host_mode").is_none(),
            "unknown provider: interactive not forced"
        );
    }

    #[test]
    fn host_equals_spawn_params_without_once() {
        // Change 2 AC-HP: identical args via host vs spawn (no --once) produce
        // equal params. The claude mint is a fresh uuid per call, so for claude we
        // assert both mint then drop the id before the structural equality check.
        for provider in ["codex", "gemini", "agy", "claude"] {
            let args = vec![
                "wk".to_string(),
                "--provider".to_string(),
                provider.to_string(),
                "do".to_string(),
                "it".to_string(),
            ];
            let (m_spawn, mut p_spawn) = build_request("spawn", &args).unwrap();
            let (m_host, mut p_host) = build_request("host", &args).unwrap();
            assert_eq!(m_spawn, m_host, "{provider}: same method");
            if provider == "claude" {
                assert!(
                    p_spawn["session_id"].as_str().is_some()
                        && p_host["session_id"].as_str().is_some(),
                    "both mint a claude session id"
                );
                p_spawn.as_object_mut().unwrap().remove("session_id");
                p_host.as_object_mut().unwrap().remove("session_id");
            }
            assert_eq!(p_spawn, p_host, "{provider}: host == spawn params");
        }
    }

    #[test]
    fn format_success_spawn_emits_compact_receipt() {
        // x-3ab8: a daemon-routed spawn must emit the one-line JSON receipt that
        // advance.py / dispatch-node.sh parse for short_id (line-by-line
        // json.loads needs it compact, not pretty-printed).
        let result = json!({
            "short_id": "ab12cd34",
            "provider": "claude",
            "status": "live",
            "extra": "ignored"
        });
        let line = format_success("spawn", "wk", &result, false, false, false).unwrap();
        assert!(!line.contains('\n'), "receipt must be one line: {line}");
        let parsed: Value = serde_json::from_str(&line).expect("valid JSON receipt");
        assert_eq!(parsed["name"], "wk");
        assert_eq!(parsed["short_id"], "ab12cd34");
        assert_eq!(parsed["provider"], "claude");
        assert_eq!(parsed["status"], "live");
    }

    #[test]
    fn promote_builds_interactive_spawn_with_resume_id() {
        let args = vec![
            "bot2".to_string(),
            "--from".to_string(),
            "019e7157-4236-7bb1-b274-ebbac6040ace".to_string(),
        ];
        let (method, params) = build_request("promote", &args).unwrap();
        assert_eq!(method, "agent.spawn");
        assert_eq!(params["name"], "bot2");
        assert_eq!(params["resume_id"], "019e7157-4236-7bb1-b274-ebbac6040ace");
        assert_eq!(params["host_mode"], "interactive");
        // provider is intentionally absent: the daemon infers it from the source.
        assert!(params.get("provider").is_none());
    }

    #[test]
    fn promote_from_equals_form_is_accepted() {
        let args = vec!["bot2".to_string(), "--from=uuid-xyz".to_string()];
        let (_m, params) = build_request("promote", &args).unwrap();
        assert_eq!(params["resume_id"], "uuid-xyz");
    }

    #[test]
    fn promote_without_from_is_rejected() {
        let args = vec!["bot2".to_string()];
        let err = build_request("promote", &args).unwrap_err();
        assert!(err.contains("--from"));
    }

    // -----------------------------------------------------------------------
    // cwd forwarding (fix/agents-host-cwd): the daemon is a shared long-lived
    // process whose own current_dir is frozen to wherever it was first started,
    // so the client must stamp the caller's cwd into daemon-bound spawn/ask
    // requests. Without this, `fno agents host` from project A opens the
    // provider in the daemon's home project B.
    // -----------------------------------------------------------------------

    #[test]
    fn ensure_request_cwd_stamps_caller_dir_for_spawn() {
        // host and promote also resolve to "agent.spawn" in build_request, so
        // this case covers all three spawn-bearing verbs.
        let mut params = json!({"name": "w", "provider": "codex"});
        ensure_request_cwd("agent.spawn", &mut params, Path::new("/work/proj"));
        assert_eq!(params["cwd"], "/work/proj");
    }

    #[test]
    fn ensure_request_cwd_explicit_cwd_wins() {
        // An explicit --cwd (already in params) must never be overwritten.
        let mut params = json!({"name": "w", "provider": "codex", "cwd": "/explicit"});
        ensure_request_cwd("agent.spawn", &mut params, Path::new("/work/proj"));
        assert_eq!(params["cwd"], "/explicit");
    }

    #[test]
    fn ensure_request_cwd_covers_ask_first_contact() {
        // gemini `ask` falls through to the daemon's auto-spawn path, which has
        // the same cwd fallback; the client must forward cwd for agent.ask too.
        let mut params = json!({"name": "g", "provider": "gemini"});
        ensure_request_cwd("agent.ask", &mut params, Path::new("/work/proj"));
        assert_eq!(params["cwd"], "/work/proj");
    }

    #[test]
    fn ensure_request_cwd_skips_non_spawn_methods() {
        // list/stop/rm carry no worker launch; leave params untouched so a
        // `--cwd` *filter* on list is the only thing that sets cwd there.
        let mut params = json!({"status": "live"});
        ensure_request_cwd("agent.list", &mut params, Path::new("/work/proj"));
        assert!(params.get("cwd").is_none());
    }

    // -----------------------------------------------------------------------
    // ab-77b691dc: --fresh / --here canonical-dispatch mechanism
    //
    // effective_worker_cwd encodes the AC6 precedence (--cwd > --fresh >
    // caller cwd; --here suppresses --fresh; unresolved canonical -> caller,
    // the safe side). It is pure so the precedence is provable without git.
    // -----------------------------------------------------------------------

    fn pb(s: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(s)
    }

    #[test]
    fn effective_cwd_no_flags_keeps_caller() {
        // No --fresh: behavior is unchanged (backward compatible).
        let got = effective_worker_cwd(None, false, false, Some(pb("/canon")), pb("/wt"));
        assert_eq!(got, pb("/wt"));
    }

    #[test]
    fn effective_cwd_fresh_resolves_canonical() {
        // --fresh moves the worker to the canonical root (AC1 / AC6).
        let got = effective_worker_cwd(None, true, false, Some(pb("/canon")), pb("/wt"));
        assert_eq!(got, pb("/canon"));
    }

    #[test]
    fn effective_cwd_here_suppresses_fresh() {
        // --here opt-out keeps the worker in the caller's worktree (AC2).
        let got = effective_worker_cwd(None, true, true, Some(pb("/canon")), pb("/wt"));
        assert_eq!(got, pb("/wt"));
    }

    #[test]
    fn effective_cwd_unresolved_canonical_falls_back_to_caller() {
        // Ambiguous / git-missing canonical resolution -> caller cwd, the safe
        // side (Failure Modes > Boundaries: never guess canonical).
        let got = effective_worker_cwd(None, true, false, None, pb("/wt"));
        assert_eq!(got, pb("/wt"));
    }

    #[test]
    fn effective_cwd_explicit_cwd_wins_over_fresh() {
        // --cwd is the highest-priority cwd source and wins over --fresh (AC6;
        // Failure Modes > Invariants).
        let got = effective_worker_cwd(
            Some(pb("/explicit")),
            true,
            false,
            Some(pb("/canon")),
            pb("/wt"),
        );
        assert_eq!(got, pb("/explicit"));
    }

    #[test]
    fn build_request_parses_fresh_and_here_flags() {
        // --fresh / --here / --in-place are plumbed into params for spawn/ask.
        let (_m, p) = build_request(
            "spawn",
            &[
                "w".into(),
                "--provider".into(),
                "claude".into(),
                "--fresh".into(),
            ],
        )
        .unwrap();
        assert_eq!(p["fresh"], Value::Bool(true));
        assert!(p.get("here").is_none());

        let (_m, p) = build_request("ask", &["w".into(), "hi".into(), "--here".into()]).unwrap();
        assert_eq!(p["here"], Value::Bool(true));

        let (_m, p) =
            build_request("ask", &["w".into(), "hi".into(), "--in-place".into()]).unwrap();
        assert_eq!(p["here"], Value::Bool(true));
    }

    // (canonical_repo_root unit tests live in src/paths.rs, where the shared
    // resolver now lives -- ab-77b691dc.)

    // -----------------------------------------------------------------------
    // Task 3.1: list/reconcile JSON parity + flag parsing
    // -----------------------------------------------------------------------

    /// AC1-HP: list --status is parsed into daemon params (not rejected as unknown)
    #[test]
    fn list_status_flag_is_parsed() {
        let args = vec!["--status".to_string(), "live".to_string()];
        let (method, params) = build_request("list", &args).unwrap();
        assert_eq!(method, "agent.list");
        assert_eq!(params["status"], Value::String("live".to_string()));
    }

    /// AC1-HP: list --cwd and --provider are forwarded to daemon params
    #[test]
    fn list_filter_flags_are_forwarded() {
        let args = vec![
            "--cwd".to_string(),
            "/tmp/myproject".to_string(),
            "--provider".to_string(),
            "codex".to_string(),
        ];
        let (_method, params) = build_request("list", &args).unwrap();
        assert_eq!(params["cwd"], Value::String("/tmp/myproject".to_string()));
        assert_eq!(params["provider"], Value::String("codex".to_string()));
    }

    /// AC1-HP: --json is NOT forwarded to daemon params (it is a client-side rendering flag)
    #[test]
    fn list_json_flag_is_not_forwarded_to_daemon() {
        // --json must be captured by build_request as a recognized flag
        // but NOT appear in the daemon params object.
        // build_request itself should not error on --json.
        let args = vec!["--json".to_string()];
        let result = build_request("list", &args);
        // Must succeed (not return Err "unknown flag: --json")
        assert!(result.is_ok(), "build_request must accept --json for list");
        let (_method, params) = result.unwrap();
        // --json must NOT be forwarded to the daemon
        assert!(
            params.get("json").is_none(),
            "--json must not appear in daemon params"
        );
    }

    /// AC2-HP: render_list_json produces the Python-matching shape with correct keys
    #[test]
    fn render_list_json_shape_matches_python_contract() {
        // Simulate daemon returning agents list in the new 10-key serialize_entry shape
        let agents = json!([
            {
                "name": "worker-a",
                "provider": "claude",
                "short_id": "cl-abc123",
                "session_id": "cl-abc123",
                "cwd": "/home/user/project",
                "created_at": "2026-05-25T00:00:00Z",
                "last_message_at": "2026-05-25T01:00:00Z",
                "status": "live",
                "live_status": null,
                "pid": 4242,
                "last_reconciled_at": "2026-05-25T00:30:00Z",
                "log_path": null,
            }
        ]);
        let filters = json!({"cwd": null, "provider": null, "status": null});
        let output = render_list_json(&agents, &filters, &[]);

        let parsed: Value = serde_json::from_str(&output).expect("must be valid JSON");
        // Top-level keys must match Python's render_json shape
        assert!(parsed.get("agents").is_some(), "missing 'agents' key");
        assert!(parsed.get("count").is_some(), "missing 'count' key");
        assert!(
            parsed.get("filters_applied").is_some(),
            "missing 'filters_applied' key"
        );
        assert!(
            parsed.get("schema_version").is_some(),
            "missing 'schema_version' key"
        );
        // ab-098967b4: discovered lane keys are additive; schema bumped to 2.
        assert!(
            parsed.get("discovered_sessions").is_some(),
            "missing 'discovered_sessions' key"
        );
        assert_eq!(parsed["discovered_count"], 0);
        assert_eq!(parsed["schema_version"], 2);
        assert_eq!(parsed["count"], 1);

        // The client passes rows through verbatim: the 10 Python parity keys
        // (incl. live_status, retained for back-compat -- AC4-FR) plus the
        // additive Architecture C keys pid + last_reconciled_at (AC4-HP) survive.
        let row = &parsed["agents"][0];
        for key in &[
            "name",
            "provider",
            "short_id",
            "session_id",
            "cwd",
            "created_at",
            "last_message_at",
            "status",
            "live_status",
            "log_path",
            "pid",
            "last_reconciled_at",
        ] {
            assert!(row.get(*key).is_some(), "row missing key: {key}");
        }
        assert_eq!(row["pid"], 4242, "pid passes through");
        assert!(row["live_status"].is_null(), "live_status retained as null");
    }

    /// ab-098967b4: render_list_json folds in the discovered lane (additive
    /// keys, schema 2); render_list_table appends a distinct DISCOVERED section.
    #[test]
    fn render_list_with_discovered_lane() {
        let agents = json!([]);
        let filters = json!({"cwd": null, "provider": null, "status": null});
        let discovered = vec![json!({
            "handle": "abilities-aaaa1111",
            "short_id": "aaaa1111",
            "session_id": "uuid-1",
            "pid": 4242,
            "cwd": "/Users/x/code/proj",
            "project": "abilities",
            "status": "busy",
            "agent": "claude",
        })];
        let out = render_list_json(&agents, &filters, &discovered);
        let parsed: Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(parsed["discovered_count"], 1);
        assert_eq!(
            parsed["discovered_sessions"][0]["handle"],
            "abilities-aaaa1111"
        );
        assert_eq!(parsed["schema_version"], 2);

        let table = render_list_table(&agents, &discovered);
        assert!(table.contains("DISCOVERED LIVE SESSIONS (1, host-local)"));
        assert!(table.contains("HANDLE"));
        assert!(table.contains("abilities-aaaa1111"));
        assert!(table.contains("busy"));
    }

    /// AC5-UI: render_list_table drops LIVE and adds CHECKED + PID; AC2-UI: a
    /// never-reconciled row renders `never`; AC4: a PTY pid is shown, an ask
    /// row's null pid renders `-`.
    #[test]
    fn render_list_table_has_checked_and_pid_columns_not_live() {
        let agents = json!([
            {
                "name": "pty-worker",
                "provider": "codex",
                "short_id": "wk1",
                "session_id": null,
                "cwd": "/home/user/project",
                "created_at": "2026-05-25T00:00:00Z",
                "last_message_at": null,
                "status": "live",
                "live_status": null,
                "pid": 4242,
                "last_reconciled_at": "2026-05-25T00:00:00Z",
                "log_path": null,
            },
            {
                "name": "ask-row",
                "provider": "claude",
                "short_id": null,
                "session_id": "cl-xyz",
                "cwd": "/home/user/other",
                "created_at": "2026-05-25T00:00:00Z",
                "last_message_at": null,
                "status": "exited",
                "live_status": null,
                "pid": null,
                "last_reconciled_at": null,
                "log_path": null,
            }
        ]);
        let table = render_list_table(&agents, &[]);
        let lines: Vec<&str> = table.lines().collect();
        // AC5-UI: header shows STATUS + CHECKED + PID, and LIVE is gone.
        assert!(
            lines[0].contains("NAME")
                && lines[0].contains("PROVIDER")
                && lines[0].contains("STATUS")
                && lines[0].contains("CHECKED")
                && lines[0].contains("PID")
                && lines[0].contains("CWD"),
            "header must contain the new column set, got: {:?}",
            lines[0]
        );
        assert!(
            !lines[0].contains("LIVE"),
            "LIVE column must be removed, got: {:?}",
            lines[0]
        );
        // header + 2 data rows
        assert!(lines.len() >= 3, "got {} lines", lines.len());
        // PTY row shows its worker pid (AC4-HP at the table surface).
        let pty_line = lines.iter().find(|l| l.contains("pty-worker")).unwrap();
        assert!(pty_line.contains("4242"), "PTY pid in table: {pty_line}");
        // Never-reconciled ask row renders `never` (AC2-UI), not `0s`/blank.
        let ask_line = lines.iter().find(|l| l.contains("ask-row")).unwrap();
        assert!(
            ask_line.contains("never"),
            "unprobed row shows never: {ask_line}"
        );
    }

    #[test]
    fn format_age_secs_compact_units() {
        // AC2-EDGE: compact single-unit ages across the threshold boundaries.
        assert_eq!(format_age_secs(3), "3s");
        assert_eq!(format_age_secs(59), "59s");
        assert_eq!(format_age_secs(240), "4m");
        assert_eq!(format_age_secs(3599), "59m");
        assert_eq!(format_age_secs(64800), "18h"); // 18 * 3600
        assert_eq!(format_age_secs(86400), "1d");
        // Clock skew (future timestamp) clamps to 0s, never negative.
        assert_eq!(format_age_secs(-5), "0s");
    }

    #[test]
    fn render_checked_handles_never_and_unparseable() {
        let now = chrono::Utc::now();
        // AC2-UI: never reconciled.
        assert_eq!(render_checked(None, now), "never");
        // A parseable recent timestamp yields a small age (seconds bucket).
        let recent = (now - chrono::Duration::seconds(5)).to_rfc3339();
        assert_eq!(render_checked(Some(&recent), now), "5s");
        // An unparseable stored value is explicit `?`, never blank or a panic.
        assert_eq!(render_checked(Some("not-a-timestamp"), now), "?");
    }

    /// AC4-HP: render_reconcile_json produces the Python-matching key set
    #[test]
    fn render_reconcile_json_shape_matches_python_contract() {
        let daemon_result = json!({
            "scanned": 3,
            "orphaned": [{"name": "gone-agent", "provider": "claude", "id": "cl-123"}],
            "recovered": [],
            "skipped": [],
            "errors": [],
        });
        let output = render_reconcile_json(&daemon_result);
        let parsed: Value = serde_json::from_str(&output).expect("must be valid JSON");
        // Must have exactly the Python ReconcileResult keys
        for key in &["scanned", "orphaned", "recovered", "skipped", "errors"] {
            assert!(
                parsed.get(*key).is_some(),
                "reconcile JSON missing key: {key}"
            );
        }
        assert_eq!(parsed["scanned"], 3);
    }
}
