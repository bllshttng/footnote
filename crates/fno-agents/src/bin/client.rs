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
use fno_agents::provider::{known_providers_csv, KNOWN_PROVIDERS};
use fno_agents::usage::{verb_usage, CLIENT_VERB_USAGE};
use serde_json::{json, Map, Value};
use std::io::IsTerminal;

const ALL_CLIENT_ACTIONS: &[&str] = &[
    "--emit-schema",
    "adopt",
    "ask",
    "attach",
    "bash-census",
    "claim",
    "codex-loaded-threads",
    "detect",
    "digest",
    "drive",
    "drive-authority",
    "finalize",
    "graph-get",
    "grid",
    "help",
    "host",
    "kill-check",
    "list",
    "logs",
    "loop",
    "loop-check",
    "mail-inject",
    "manifest-eval",
    "manifest-for-session",
    "needs",
    "ping",
    "pr-heal",
    "probe-run",
    "promote",
    "reap",
    "reconcile",
    "recover",
    "reentry-plan",
    "rename",
    "report",
    "review-coverage",
    "review-summary",
    "restart",
    "resume",
    "review-start",
    "rm",
    "session-start-bytes",
    "spawn",
    "status",
    "stop",
    "subscribe",
    "trace",
    "verify-evidence",
    "version",
    "wait",
];

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
        fno_agents::version::print_version(json);
        return 0;
    }

    // `mail-inject` is the one-shot LIVE-DELIVERY verb `fno agents mail send` calls to
    // inject a turn into a live `claude --bg` session over the daemon control.sock
    // (node x-1f23). Binary-direct (Python `_deliver_live` subprocess), NOT a
    // routable `fno agents` verb -- matched with `matches!` (like `version`) so the
    // parity guard (test_rust_client_verbs_match_client_rs) does not see it and it
    // stays out of CLIENT_VERB_USAGE / RUST_CLIENT_VERBS. Connects to an existing
    // daemon; never lazy-starts one.
    if matches!(verb, "mail-inject") {
        return fno_agents::mail_inject::run_mail_inject(&args[1..]).await;
    }

    if matches!(verb, "manifest-eval") {
        return fno_agents::manifest::run_manifest_eval(&args[1..]);
    }

    // `reentry-plan` is the INTERNAL machine resolver behind every
    // Claude re-entry door (x-d285): the Rust/Python attach+resume arms and
    // the mux gestures consume its verdict instead of each rebuilding a
    // provider argv. Matched with `matches!` (like `claim`/`detect`) so it
    // stays out of the routable-verb parity sets - it is not an `fno agents`
    // verb, and adding one needs the Python surface to grow with it.
    if matches!(verb, "reentry-plan") {
        return fno_agents::reentry::run_reentry_plan(&args[1..], &AgentsHome::from_env());
    }

    if matches!(verb, "manifest-for-session") {
        return fno_agents::manifest_lookup::run_manifest_for_session(&args[1..]);
    }

    // `review-summary` is the display-line author for a pre-push reviewed PR:
    // the /pr create worker runs it against the local ledger and appends its
    // stdout to the body. Same `matches!` treatment as `claim`/`detect` so the
    // routable-verb parity guard does not see it - it reads one file and
    // prints, it is not an `fno agents` verb.
    if matches!(verb, "review-summary") {
        return fno_agents::review_summary::run_review_summary(&args[1..]);
    }

    if matches!(verb, "codex-loaded-threads") {
        return fno_agents::codex_inject::run_loaded_thread_discovery().await;
    }

    // `review-start` is the hidden codex review-forcing verb (node x-c24d): the
    // app-server `review/start` RPC is the codex counterpart of claude's
    // `--raw /code-review` (the Python raw router sends exact review verbs here;
    // codex's turn/start lane still cannot parse arbitrary slash payloads).
    // Structured targets + an outcome receipt (a Turn + a reviewThreadId),
    // strictly better than keystroke faking. Same `matches!`
    // treatment as `mail-inject`/`codex-loaded-threads` so it stays out of
    // CLIENT_VERB_USAGE / RUST_CLIENT_VERBS and the parity guard - no advertised
    // fno verb is added. The socket round-trip needs the user's daemon.
    if matches!(verb, "review-start") {
        return fno_agents::codex_inject::run_review_start(&args[1..]).await;
    }

    // `claim` is the HIDDEN debug front over the native claims module
    // (`fno_agents::claims`): the cross-impl compatibility matrix drives the
    // Rust side of the lockfile protocol through it, and it doubles as an ops
    // escape hatch when the Python CLI is unavailable. Matched with `matches!`
    // (like `mail-inject`) so the routable-verb parity guard does not see it
    // and it stays out of CLIENT_VERB_USAGE / RUST_CLIENT_VERBS — `fno agents claim`
    // remains the only operator CLI for claims.
    if matches!(verb, "claim") {
        return fno_agents::client_verbs::run_claim(&args[1..]);
    }

    // `detect` is the HIDDEN debug front over the screen-manifest fallback
    // authority (`fno_agents::scrape`): `detect explain <agent>` prints which
    // rung of the badge lattice currently badges the agent. Same `matches!`
    // treatment as `claim` so it stays out of CLIENT_VERB_USAGE /
    // RUST_CLIENT_VERBS and the parity guard.
    if matches!(verb, "detect") {
        return fno_agents::scrape::run_detect(&args[1..]);
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

    // `--emit-schema` is a read-only introspection flag: prints the unified
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

    // `loop-check`: stop-hook decision verb (see loopcheck.rs module doc).
    // Direct dispatch; no daemon RPC.
    if verb == "loop-check" {
        return fno_agents::loopcheck::run_loop_check(&args[1..]);
    }

    // `probe-run`: see its own doc in loopcheck.rs. Direct dispatch.
    if verb == "probe-run" {
        return fno_agents::loopcheck::run_probe_run(&args[1..]);
    }

    // `review-coverage`: standalone review_coverage producer (see its own doc
    // in loopcheck.rs). Direct dispatch like loop-check; no daemon RPC.
    if verb == "review-coverage" {
        return fno_agents::loopcheck::run_review_coverage(&args[1..]);
    }

    // `loop run`: unified driver loop (see loop_target.rs doc). Direct
    // dispatch like loop-check; no daemon RPC.
    if verb == "loop" {
        return fno_agents::loop_target::run_loop_verb(&args[1..]);
    }

    // `finalize`: terminal-only side-effect WRITER (see finalize.rs doc). Direct
    // dispatch; no daemon RPC.
    if verb == "finalize" {
        return fno_agents::finalize::run_finalize(&args[1..]);
    }

    // `kill-check`: Rust port of scripts/lib/kill-criteria.sh (see
    // kill_criteria.rs doc). Direct dispatch; no daemon RPC.
    if verb == "kill-check" {
        return fno_agents::kill_criteria::run_kill_check(&args[1..]);
    }

    // `graph-get`/`bash-census`/`session-start-bytes` (x-997a): daemon-free reads, not routable `fno agents` verbs (same reasoning as kill-check).
    if verb == "graph-get" {
        return fno_agents::graph_get::run_graph_get(&args[1..]);
    }
    if verb == "bash-census" {
        return fno_agents::bash_census::run_bash_census(&args[1..]);
    }
    if verb == "session-start-bytes" {
        return fno_agents::session_start_bytes::run_session_start_bytes(&args[1..]);
    }

    // `verify-evidence`: Rust port of scripts/lib/verify-event-evidence.sh
    // (see verify_evidence.rs doc). Direct dispatch.
    if verb == "verify-evidence" {
        return fno_agents::verify_evidence::run_verify_evidence(&args[1..]);
    }

    // Retired at G4 (x-f54c): the grid, the WebSocket drive surface, and the
    // interactive daemon PTY hosting behind `host`/`promote` were deleted when
    // the mux became the agent-PTY substrate. Each prints a one-line pointer to
    // the mux and exits non-zero, never a silent no-op (AC5-EDGE).
    if let Some(pointer) = retired_verb_pointer(verb) {
        eprintln!("{pointer}");
        return 2;
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
    if verb == "adopt" {
        return fno_agents::client_verbs::run_adopt(&args[1..], &AgentsHome::from_env());
    }
    if verb == "attach" {
        return fno_agents::client_verbs::run_attach(&args[1..], &AgentsHome::from_env());
    }
    // `recover` (x-d285): hidden-but-invocable manual restoration of a recorded
    // session under its account/route, with explicit two-id selection. Reads
    // the registry and resolver directly, no daemon RPC.
    if verb == "recover" {
        return fno_agents::client_verbs::run_recover(&args[1..], &AgentsHome::from_env());
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
    // `wait`: block until an agent's registry row reaches a state. Reads
    // `registry.json` directly and polls (no daemon RPC), so it needs no running
    // daemon and dispatches here before build_request.
    if verb == "wait" {
        return fno_agents::wait::run_wait(&args[1..], &AgentsHome::from_env()).await;
    }

    // `pr-heal` classifies a red check and applies the mechanical fix. Binary-
    // direct behind `fno do pr heal`, like `kill-check`: it is NOT a routable
    // `fno agents` verb, so it stays out of CLIENT_VERB_USAGE / RUST_CLIENT_VERBS
    // (whose lengths the --help parity test asserts equal). `matches!` rather
    // than `verb == "..."` for the same reason `version` uses it: the Python
    // parity guard scrapes `verb == "..."` and would demand a RUST_CLIENT_VERBS
    // row for a verb that is not an `fno agents` verb. Daemon-free, so it
    // dispatches here before build_request.
    if matches!(verb, "pr-heal") {
        return fno_agents::heal::run_heal(&args[1..]);
    }
    // `subscribe`: follow the daemon's own `events.jsonl` and stream registry
    // state transitions + pane exits as NDJSON. File-follow, no daemon RPC, so it
    // dispatches here before build_request.
    if verb == "subscribe" {
        return fno_agents::subscribe::run_subscribe(&args[1..], &AgentsHome::from_env()).await;
    }

    // `digest` (x-4e2d): read-only "while you were gone" fold over events.jsonl +
    // ledger.json for a session. Never touches the daemon; exits 0 on empty.
    if verb == "digest" {
        return fno_agents::digest::run_digest(&args[1..], &AgentsHome::from_env()).await;
    }

    // `needs` (x-feec): read-only needs-me-queue fold over events.jsonl +
    // ledger.json across ALL sessions, emitting review_wedged / budget_stop
    // items. Never touches the daemon; exits 0 on empty. The mux client shells
    // this off-loop when the prefix+a overlay opens.
    if verb == "needs" {
        return fno_agents::needs::run_needs(&args[1..], &AgentsHome::from_env()).await;
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
    // `--force` (x-3498) is the break-glass variant: SIGKILL the lockfile's
    // holder BEFORE any probe, because a wedged holder is exactly what the
    // probe cannot see.
    if verb == "restart" {
        let force = match &args[1..] {
            [] => false,
            [f] if f == "--force" => true,
            _ => {
                eprintln!(
                    "fno-agents: restart takes no arguments besides --force (got: {})",
                    args[1..].join(" ")
                );
                return 2;
            }
        };
        return run_restart(force).await;
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
    // x-de10 (AC20): the codex-thread-target lookup moved INTO the agent.ask
    // block below, derived from the registry read already performed there. The
    // old spot loaded the registry for EVERY client verb and swallowed read
    // failures at `.ok()`.

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
        //
        // x-de10 (AC20): ONE registry read for the whole ask path. The
        // codex-thread-target lookup below derives from THIS read - the old
        // second load ran for EVERY client verb and swallowed failures at
        // `.ok()`.
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
        // A codex thread target falls through to the daemon ask below instead
        // of the unresolvable-create error.
        let codex_thread_target =
            fno_agents::codex_ask::is_codex_thread_target(&registry, &agent_name);
        {
            if registry.find_name_or_full_session_id(&agent_name).is_none() {
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
                    "unknown agent {}; spawn it first: fno agents spawn {} --harness <harness>",
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
        // clear error directing the caller to `spawn --harness agy --once`.
        if let Some(code) = maybe_run_agy_ask(&home, &params, &agent_name) {
            return code;
        }
        // Opencode `ask` is intercepted client-side (x-51f6): opencode is
        // pane-hosted only in v1, so a stateful resume is unsupported — this
        // surfaces a clear error directing the caller to drive the pane
        // directly, rather than the generic "provider required for new
        // agent" text an existing opencode row would otherwise hit below
        // (that text is both wrong - the agent already exists - and a dead
        // end, since retrying with --harness opencode reproduces it).
        if let Some(code) = maybe_run_opencode_ask(&home, &params, &agent_name) {
            return code;
        }
        // Unconditional flip (ab-73da4ac2): `ask` now auto-routes to this
        // client for every provider, so an ask that matched none of the four
        // provider hooks is a create with no/unknown `--provider`. Surface
        // Python's `select_provider` exit-2 error here rather than falling
        // through to the daemon RPC, whose `handle_ask` PTY screen is the wrong
        // shape for `ask` (Locked Decision 3). The daemon path below is now
        // unreachable for `agent.ask`.
        if !codex_thread_target {
            return unresolvable_ask_exit(&params, &agent_name);
        }
    }

    // Task 1.3a: intercept `spawn` (NOT host/promote, which also map to
    // agent.spawn) to route claude -> dispatch_claude_spawn, and
    // codex/gemini + --once -> dispatch_codex_once / dispatch_gemini_once.
    // `host` and `promote` must fall through to the daemon RPC unchanged.
    if method == "agent.spawn" && verb_owned == "spawn" {
        // 4a-G2: the `pane` substrate (the default) is mux-hosted now, and the
        // Python back half owns it (fno.agents.mux_spawn: front-half reuse +
        // `fno mux pane run` + the registry mux ref). The Python front door
        // already carves pane spawns out of the binary route (rust_runtime),
        // so this arm is only reached by a DIRECT `fno-agents spawn` call -
        // re-exec the Python CLI rather than falling through to the daemon
        // PTY host (retiring at G4; a silent daemon fallback is exactly what
        // AC1-ERR forbids). FNO_AGENTS_RUNTIME=python stops the front door
        // routing straight back here.
        let substrate = params
            .get("substrate")
            .and_then(|v| v.as_str())
            .unwrap_or("pane");
        // `thread` is the public substrate name. The lower-level dispatch arms
        // retain their historical `bg` selector until their wire contract moves.
        let substrate = if substrate == "thread" {
            "bg"
        } else {
            substrate
        };
        if let Err(message) = validate_spawn_placement(&params, substrate) {
            eprintln!("{message}");
            return 2;
        }
        if substrate == "pane" {
            use fno_agents::claude_ask::py_repr;
            use std::os::unix::process::CommandExt;
            // Provider parity with the optional-provider Python resolver: a
            // MISSING --provider is legal on the pane substrate (the Python
            // re-exec resolves it from the invoking harness), so let None fall
            // through. An UNKNOWN provider is still a client-side exit 2 even
            // where the `fno` front door is absent (CI), matching the resolver's
            // downstream substrate-aware rejection.
            match params.get("provider").and_then(|v| v.as_str()) {
                None => {}
                Some(p) if !KNOWN_PROVIDERS.contains(&p) => {
                    eprintln!(
                        "unknown provider {}; supported: {}",
                        py_repr(p),
                        known_providers_csv()
                    );
                    return 2;
                }
                Some(_) => {}
            }
            let err = std::process::Command::new("fno")
                .arg("agents")
                .args(&args[..])
                .env("FNO_AGENTS_RUNTIME", "python")
                .exec();
            eprintln!(
                "fno-agents: substrate 'pane' is mux-hosted via the Python CLI, \
                 but exec of 'fno agents spawn' failed: {err}. Install the fno \
                 front door or run `fno agents spawn ...` directly."
            );
            return 127;
        }
        // x-d012: an --account spawn on ANY substrate resolves its four-lane env
        // overlay in Python (fno.agents.account_env); re-exec the Python CLI here
        // rather than the native Rust bg spawn below, so the resolver + refusals
        // live in exactly one place (pane already re-exec'd above). Without this
        // the flag silently vanishes on the Rust-intercepted bg/headless path -
        // the known "two path gates for a new provider field" drift class.
        // FNO_AGENTS_RUNTIME=python stops the Python front door bouncing back.
        if params.get("account").and_then(|v| v.as_str()).is_some() {
            use std::os::unix::process::CommandExt;
            let err = std::process::Command::new("fno")
                .arg("agents")
                .args(&args[..])
                .env("FNO_AGENTS_RUNTIME", "python")
                .exec();
            eprintln!(
                "fno-agents: --account resolution runs in the Python CLI, but \
                 exec of 'fno agents spawn' failed: {err}. Run `fno agents \
                 spawn ...` directly."
            );
            return 127;
        }
        if let Some(code) = maybe_run_spawn(&home, &params, &agent_name) {
            if code == 0 {
                if let Err(detail) = place_thread_portal_after_spawn(&params, &agent_name) {
                    eprintln!("{detail}");
                    return 1;
                }
            }
            return code;
        }
        // No client-side handler matched: fall through to the daemon RPC below.
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
            // x-85fe: the default (no explicit --cwd, no --here) stamps the
            // canonical repo root instead of the caller cwd for daemon-bound
            // codex/gemini spawn -- the same inversion as the client-side path.
            // An explicit --cwd wins, so when params already carries one we
            // resolve nothing and emit no redirect note (it would falsely claim a
            // redirect that ensure_request_cwd's keep-explicit guard never
            // performs -- review MEDIUM 4); --here keeps the caller cwd. --fresh
            // is an accepted no-op alias. ensure_request_cwd then leaves the
            // explicit --cwd intact.
            let (_fresh, here) = fresh_here_flags(&params);
            let explicit_cwd = params.get("cwd").is_some();
            // Only spawn consumes the launch dir: an `agent.ask` follows its
            // registered session and takes cwd as `_cwd`, so it never takes the
            // canonical default nor the redirect note (a false diagnostic for a
            // non-consuming op -- x-85fe review). spawn keeps the inverted default.
            let stamp = if !explicit_cwd && !here && method == "agent.spawn" {
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
    // x-de10 (AC19): for a codex THREAD spawn the daemon RPC is what creates
    // the registry row, so the spawn gate is held across exactly this exchange
    // (acquire before the write, release after the response read) - one gate
    // evaluation per spawn, and the first spawn's row is counted before the
    // second is evaluated. Other verbs skip this entirely. Read before
    // `method`/`params` move into the request.
    //
    // Daemon-bound is DERIVED from the same capability contract the daemon
    // routes on (attach lane + a harness-owned server, x-b180). Both binaries
    // embed the same packaged table, so this predicate and the daemon's route
    // cannot disagree the way a name test here and a derived route there
    // could: the next attach-with-server harness arrives with its state_dirs
    // attached and its gate run without this line learning its name.
    //
    // The provider default MUST match the daemon's, which is `codex` when the
    // param is absent (`handle_spawn`). A predicate requiring an explicit
    // "codex" here reads false for a spawn with no `-H`, while the daemon still
    // routes it to the codex thread lane - so the grant was never attached and
    // the gate below never ran, on the exact lane this node exists to fix.
    // Green gate, mute worker. Keep the two defaults identical. An unreadable
    // contract answers false here: the daemon refuses the same spawn, so no
    // grant or gate is skipped for a spawn the daemon would have served.
    let spawn_provider = params
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("codex");
    let daemon_bound_thread_spawn = method == "agent.spawn"
        && params.get("substrate").and_then(|v| v.as_str()) == Some("thread")
        && fno_agents::harness_capabilities::HarnessContract::packaged()
            .and_then(|contract| {
                Ok(contract.thread_lane(spawn_provider)? == "attach"
                    && contract.attach_needs_server(spawn_provider)?)
            })
            .unwrap_or(false);
    // Hop 1 of the state-root grant (x-f22f). The client inherits
    // FNO_WORKER_ADD_DIRS from the Python seam across `os.execv`, so it reads
    // the ALREADY-RESOLVED set with the same reader every other lane uses -
    // one resolver, one published value, now three readers.
    //
    // It has to travel as a param rather than as environment because the
    // daemon on the other end is long-lived and SHARED: it does not inherit
    // this spawn's environment, so a `state_dirs_from_env()` call over there
    // would read the daemon's own env instead of ours.
    if daemon_bound_thread_spawn {
        let roots = fno_agents::claude_ask::state_dirs_from_env();
        if !roots.is_empty() {
            params["state_dirs"] = Value::from(roots);
        }
    }
    // Snapshot before `params` moves into the request: the relocated gate
    // honors the same spawn-control flags the shared construction reads.
    let daemon_gate_flags = gate_flags_from_params(&params);
    // (x-9b60) Same snapshot for the portal placement: it rides the
    // daemon's response, after the receipt.
    let thread_portal_params = if method == "agent.spawn"
        && params.get("substrate").and_then(|v| v.as_str()) == Some("thread")
    {
        Some(params.clone())
    } else {
        None
    };
    let req = Request::new(1, method, params);

    let daemon_spawn_gate = if daemon_bound_thread_spawn {
        let config_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        match fno_agents::spawn_gate::run_gate(
            &config_cwd,
            &home.registry_json(),
            &agent_name,
            "bg",
            daemon_gate_flags,
        ) {
            Ok(guard) => Some(guard),
            Err(code) => return code,
        }
    } else {
        None
    };

    let call_result = call(&home, &daemon_bin, &req).await;
    drop(daemon_spawn_gate);
    match call_result {
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
                // Drift warning on read/removal verbs, stderr-only so a
                // `--json` stdout consumer stays clean. These verbs already
                // ensured a daemon is up via `call`; a freshly lazy-started one
                // reads Fresh, so no false warning. A separate status probe keeps
                // this off every other verb's hot path.
                if warns_on_daemon_drift(&verb_owned) {
                    let state = check_daemon_drift(&home).await;
                    if let Some(w) = drift_warning(&state, None) {
                        eprintln!("{w}");
                    }
                }
                // (x-6678) A refused stop is not a success. The daemon answers
                // `stopped: false` over a turn whose interrupt never settled,
                // and the mux viewport's `run_agent_action` reads ONLY this
                // exit code. A 0 there prints "stopped <name>" over a worker
                // that is still running, which is the report the daemon arm
                // refuses to make. `Busy` is the shape: the turn holds the
                // thread, and a retry can still reach it.
                if verb_owned == "stop"
                    && result.get("stopped").and_then(Value::as_bool) == Some(false)
                {
                    return 18;
                }
                // (x-9b60) The daemon-bound thread lane (codex and every other
                // attach-with-server harness): the RPC created the row, so the
                // portal places here, after the receipt, exactly as the
                // client-side lanes do.
                if let Some(place_params) = &thread_portal_params {
                    if let Err(detail) = place_thread_portal_after_spawn(place_params, &agent_name)
                    {
                        eprintln!("{detail}");
                        return 1;
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
    let existing_provider = registry
        .find_name_or_full_session_id(name)
        .map(|e| e.harness_name().to_string());

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
        .unwrap_or("fno");
    // ask is a follow-up to an already-registered session: dispatch_claude_ask
    // takes the cwd as `_cwd` and never launches in it. So resolve only an
    // explicit --cwd (canonicalized, Python's `Path(cwd).resolve()`; empty is
    // absent) or the caller cwd -- NEVER the canonical default or the redirect
    // note, which would be a false diagnostic for an operation that does not
    // consume the launch dir (x-85fe review).
    let cwd = params
        .get("cwd")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(canonicalize_cwd)
        .unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        });
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

/// Invoking-harness env markers, highest priority first. Mirror of Python
/// `harness_identity.HARNESS_SESSION_MARKERS`; cross-language drift is caught by
/// the pytest `test_harness_markers_match_client_rs`, which reads this const from
/// source (the Rust unit test only guards Rust-internal edits).
#[allow(dead_code)]
const HARNESS_MARKERS: &[(&str, &str)] = &[
    ("CODEX_THREAD_ID", "codex"),
    ("CLAUDE_CODE_SESSION_ID", "claude"),
    ("CODEX_SESSION_ID", "codex"),
    ("GEMINI_SESSION_ID", "gemini"),
    ("OPENCODE_SESSION_ID", "opencode"),
];

/// Infer the dispatch provider when `--provider` is absent, mirroring Python
/// `infer_invoking_harness`: resolve when the present markers name exactly one
/// *distinct* harness. Multiple markers for the same harness (Codex's thread id
/// plus its legacy session id) agree; markers naming different harnesses, or
/// none, fall through to the builtin `claude`. Never guesses. `lookup` is
/// injectable so tests don't touch process-global env.
fn infer_dispatch_provider(lookup: impl Fn(&str) -> Option<String>) -> &'static str {
    match fno_agents::claims::resolve_harness_from(lookup).as_deref() {
        Some("claude") => "claude",
        Some("codex") => "codex",
        Some("gemini") => "gemini",
        Some("opencode") => "opencode",
        Some("agy") => "agy",
        _ => "claude",
    }
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

/// Route an opencode `ask` to the client-side pane-only guard (x-51f6).
/// opencode is hosted as a pane with no client-side stateful resume; this
/// returns `Some(2)` with a redirect error for an opencode target, else `None`.
fn maybe_run_opencode_ask(home: &AgentsHome, params: &Value, name: &str) -> Option<i32> {
    fno_agents::opencode_ask::maybe_run_opencode_ask(home, params, name)
}

fn validate_spawn_placement(params: &Value, substrate: &str) -> Result<(), String> {
    let squad = params.get("squad").and_then(Value::as_str);
    let split = params.get("split").and_then(Value::as_str);
    let at = params.get("at").and_then(Value::as_str);
    let tab = params.get("tab").and_then(Value::as_str);
    let portal = params.get("portal").and_then(Value::as_u64);

    if squad.is_some_and(|name| name.trim().is_empty()) {
        return Err("--workspace/-s needs a nonblank workspace name".into());
    }
    // (x-9b60) A portal is the pane a thread hosts: the placement flags are
    // legal for a thread WHEN --portal names it, refused for a bare thread
    // where they mean nothing. Mirrors Python's placement_refusal, the one
    // contract both runtimes read; the portal's 0-255 range is enforced at
    // the parse arm, so a bad index never reaches this check.
    if portal.is_some() && substrate != "bg" {
        return Err(
            "--portal applies only to --substrate thread; a pane hosts its \
             own geometry and headless hosts no session at all"
                .into(),
        );
    }
    if at.is_some() && substrate == "bg" {
        return Err("--at applies only to --substrate pane (a thread has no calling pane)".into());
    }
    let placement_requested = squad.is_some() || split.is_some() || at.is_some() || tab.is_some();
    if placement_requested && substrate == "bg" && portal.is_none() {
        return Err("--workspace/-s, --split/-x, and --tab on --substrate \
             thread need --portal N: a thread hosts no pane until a portal \
             opens one, so the placement has nothing to place"
            .into());
    }
    if placement_requested && portal.is_none() && substrate != "pane" {
        return Err(
            "--workspace/-s, --split/-x, --at, and --tab apply only to --substrate pane \
             (bg/headless have no pane geometry)"
                .into(),
        );
    }
    if split.is_some_and(|direction| !matches!(direction, "left" | "right" | "up" | "down")) {
        return Err(format!(
            "--split/-x must be left, right, up, or down (got {:?})",
            split.unwrap_or_default()
        ));
    }
    if tab.is_some_and(|selector| selector.trim().is_empty()) {
        return Err("--tab needs a nonblank selector or pane-group name".into());
    }
    Ok(())
}

fn effective_spawn_message(message: &str, substrate: &str) -> String {
    if substrate == "pane" {
        message.to_owned()
    } else {
        fno_agents::spawn_payload::enrich_spawn_payload(message)
    }
}

fn validate_effort_for_spawn(
    provider: &str,
    substrate: &str,
    effort: Option<&str>,
) -> Result<(), String> {
    if substrate == "pane" {
        return Ok(());
    }
    let Some(value) = effort else {
        return Ok(());
    };
    if value.is_empty() {
        return Err("--effort must be non-empty".to_string());
    }
    if matches!(provider, "gemini" | "agy") {
        return Err(format!(
            "harness {} has no reasoning-effort surface; omit --effort",
            provider
        ));
    }
    Ok(())
}

/// Route a `spawn` (NOT host/promote) to the appropriate client-side path.
///
/// x-2c27 names the session substrate as one axis with three values; this arm
/// routes the two non-default ones client-side and falls through for `pane`.
/// - `pane` (default): owned interactive daemon pane -> None (fall through).
/// - claude + `bg`: dispatch_claude_spawn (the detached `claude --bg` thread).
/// - claude + `headless`: dispatch_claude_headless (the `claude -p` one-shot).
/// - codex/gemini/agy/opencode + `headless`: dispatch_*_once (one-shot, client-side).
/// - opencode + `bg`: dispatch_opencode_serve (persistent session on a shared
///   `opencode serve`, x-d9f9; detached `run --attach` writer streams events).
/// - codex + `bg`: daemon-hosted app-server thread; gemini/agy + `bg`: hard error.
/// - no resolvable / unknown provider: stderr usage error + exit 2.
///
/// Returns `Some(exit_code)` when handled client-side, `None` to fall through.
/// (x-9b60) One-call portal placement on the Rust lanes, the twin of the
/// Python `thread_portal.place_thread_portal`: a thread spawn with
/// `--portal` ends with the portal open, through the same `fno mux thread`
/// reach a manual second command would type. The worker is already live, so
/// a placement failure is reported AFTER the spawn receipt and never
/// un-spawns anyone.
fn place_thread_portal_after_spawn(params: &Value, name: &str) -> Result<(), String> {
    let Some(portal) = params.get("portal").and_then(Value::as_u64) else {
        return Ok(());
    };
    let mut args = vec![
        "mux".to_string(),
        "thread".to_string(),
        name.to_string(),
        "--portal".to_string(),
        portal.to_string(),
    ];
    for (flag, key) in [
        ("--workspace", "squad"),
        ("--split", "split"),
        ("--at", "at"),
        ("--tab", "tab"),
    ] {
        if let Some(v) = params.get(key).and_then(|v| v.as_str()) {
            if !v.is_empty() {
                args.push(flag.to_string());
                args.push(v.to_string());
            }
        }
    }
    let out = std::process::Command::new("fno")
        .args(&args)
        .output()
        .map_err(|e| {
            format!(
                "portal {portal} placement failed: {e} (the worker is live; \
                 place it with 'fno mux thread {name} --portal {portal}')"
            )
        })?;
    if !out.status.success() {
        let raw = if out.stderr.is_empty() {
            &out.stdout
        } else {
            &out.stderr
        };
        let detail = String::from_utf8_lossy(raw).trim().to_string();
        let detail = if detail.is_empty() {
            "no output".to_string()
        } else {
            detail
        };
        return Err(format!(
            "portal {portal} placement failed: {detail} (the worker is live; \
             place it with 'fno mux thread {name} --portal {portal}')"
        ));
    }
    if !out.stdout.is_empty() {
        print!("{}", String::from_utf8_lossy(&out.stdout));
    }
    Ok(())
}

fn maybe_run_spawn(home: &AgentsHome, params: &Value, name: &str) -> Option<i32> {
    use fno_agents::agy_ask::dispatch_agy_once_with_effort;
    use fno_agents::claude_ask::{
        dispatch_claude_headless, dispatch_claude_spawn, py_repr, ClaudeHome,
    };
    use fno_agents::codex_ask::dispatch_codex_once;
    use fno_agents::gemini_ask::dispatch_gemini_once;
    use fno_agents::opencode_ask::dispatch_opencode_once;
    use fno_agents::state::load_registry;

    // x-9d11 refusal carrier, Rust lane: the verdict is computed HERE, in the
    // one owner of the grammar (`merge_posture`), so a direct `fno-agents
    // spawn` gets the same posture as a Python-fronted one. It is applied to
    // this process's env and every child env below inherits it (model_env_scrub
    // keeps TARGET_NO_MERGE as protected bookkeeping). Idempotent with the
    // Python lane's application (`harness_map.apply_merge_posture_env` in
    // `cmd_spawn`): both answer from the same table, so running twice
    // converges.
    if let Some(message) = params.get("message").and_then(|v| v.as_str()) {
        fno_agents::merge_posture::apply_env_from_message(message);
    }

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
    let substrate = if substrate == "thread" {
        "bg"
    } else {
        substrate
    };

    // unwrap_or_default is acceptable HERE (unlike the ask pre-check, which
    // must exit 12 on a corrupt registry): this collision check is advisory;
    // the authoritative read happens again under the per-agent lock inside
    // dispatch_claude_spawn / dispatch_*_once, which surface a corrupt
    // registry as exit 12.
    let registry = load_registry(&home.registry_json()).unwrap_or_default();
    let existing_provider = registry.find(name).map(|e| e.harness_name().to_string());

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

    // Resolve provider: explicit --provider > invoking-harness inference >
    // builtin `claude` (mirrors Python's resolve_dispatch_harness). A missing
    // flag no longer exits 2 -- that was the bg/headless split-brain vs pane,
    // which already infers via the Python re-exec.
    let provider = match provider_param {
        Some(p) => p,
        None => infer_dispatch_provider(|k| std::env::var(k).ok()),
    };

    // R3, the state-root grant gate. Placed HERE because every non-pane spawn
    // passes through this function exactly once, including the codex thread
    // spawn that returns None below and completes over the daemon RPC - one
    // gate evaluation per spawn, on both routes. `pane` is excluded because it
    // re-execs the Python CLI, whose own seam spends the grant.
    //
    // The public substrate name is what the capability table is keyed on; the
    // local `substrate` above has already been mapped to the historical `bg`
    // selector, so map it back rather than adding a second spelling to the
    // table.
    //
    // Skipped for a provider this binary does not know: the dispatch arms below
    // already refuse an unknown provider with exit 2 and the supported list,
    // which is the message that reader needs. Refusing first with grant advice
    // would shadow it with a diagnosis of the wrong problem.
    if substrate != "pane" && KNOWN_PROVIDERS.contains(&provider) {
        let declared_substrate = if substrate == "bg" {
            "thread"
        } else {
            substrate
        };
        let roots = fno_agents::claude_ask::state_dirs_from_env();
        if let Err(code) =
            fno_agents::spawn_gate::state_root_grant_gate(provider, declared_substrate, &roots)
        {
            return Some(code);
        }
    }

    let message = effective_spawn_message(
        params.get("message").and_then(|v| v.as_str()).unwrap_or(""),
        substrate,
    );
    let from_name = params
        .get("from_name")
        .and_then(|v| v.as_str())
        .unwrap_or("fno");
    // --cwd > --here (caller) > default canonical (x-85fe); resolve_dispatch_cwd
    // canonicalizes an explicit --cwd and shells to git on the default path
    // (no --cwd, no --here). Resolve only for CLIENT-SIDE spawns, which are the
    // non-`pane` substrates (bg + headless).
    // The `pane` substrate falls through to the daemon RPC below, which resolves
    // canonical itself; resolving here too would double the git call and the
    // redirect note (review MEDIUM 3).
    // x-85fe: `surface_cwd` is the move decision resolve_dispatch_cwd already
    // made (the note condition), so the receipt's cwd field couples to the note
    // with no second comparison. pane re-execs Python and resolves canonical
    // itself, so it neither resolves cwd here nor surfaces it.
    let (cwd, surface_cwd) = if substrate == "pane" {
        (std::path::PathBuf::new(), false)
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
    // Optional --model, forwarded to every provider's own --model (x-c772
    // wired codex/gemini/claude-headless; claude --bg was x-571f). Exact
    // passthrough appended to the worker argv.
    let model = params.get("model").and_then(|v| v.as_str());
    // x-dfa4: permission mode for the bg/headless lanes. The pane substrate
    // never reaches here (it re-execs the Python CLI, which owns pane mapping);
    // this arm handles the claude bg/headless lanes only.
    let permission_mode = params.get("permission_mode").and_then(|v| v.as_str());
    let effort = params.get("effort").and_then(|v| v.as_str());
    // x-b6e2: Tier-3 harness-native passthrough. add_dir has 3 real cells
    // (claude/codex/agy); agent/tools/deny_tools are claude-only on this
    // bg/headless lane. Every non-equivalent cell fails closed below (mirrors
    // --permission-mode / x-dfa4). The pane substrate re-execs the Python CLI,
    // which owns its own per-provider mapping + fail-closed for these.
    // Normalize empty-as-None once: an empty value is UNSET (the builders omit an
    // empty flag), so the guard below must not trip on `--add-dir=""` and the
    // bundle must carry None, not Some("").
    let empty_as_none = |k: &str| {
        params
            .get(k)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
    };
    let add_dir = empty_as_none("add_dir");
    let agent = empty_as_none("agent");
    let tools = empty_as_none("tools");
    let deny_tools = empty_as_none("deny_tools");

    // Validate the provider FIRST so an unknown provider is a client-side
    // error (exit 2) for every substrate, never a fall-through to the daemon.
    if !KNOWN_PROVIDERS.contains(&provider) {
        eprintln!(
            "unknown provider {}; supported: {}",
            py_repr(provider),
            known_providers_csv()
        );
        return Some(2);
    }

    // AC5-ERR: one knob at a time (pane enforces this in Python; here for
    // bg/headless).
    if permission_mode.is_some() && yolo {
        eprintln!("--permission-mode and --yolo are mutually exclusive; pass one");
        return Some(2);
    }
    // Fail-closed (Locked Decision 1/2): only claude's bg/headless lanes accept
    // a mapped --permission-mode. codex/gemini/agy one-shot lanes and the
    // opencode bg serve lane hardcode their own bypass form, so a mode here
    // can't be honored without a silent downgrade - reject it, pointing at the
    // pane substrate (which DOES map every provider's vocabulary).
    if permission_mode.is_some() && provider != "claude" {
        let remedy = if provider == "codex" {
            "drop --permission-mode and pass -Y/--yolo"
        } else {
            "use --substrate pane"
        };
        eprintln!(
            "--permission-mode is not supported for harness {} on --substrate bg/headless (its one-shot lane hardcodes its own bypass form); {remedy}",
            py_repr(provider),
        );
        return Some(2);
    }
    if let Err(reason) = validate_effort_for_spawn(provider, substrate, effort) {
        eprintln!("{reason}");
        return Some(2);
    }

    // x-b6e2 fail-closed matrix for the client-owned bg/headless lanes (pane
    // re-execs Python, which guards there). A flag with no equivalent for the
    // resolved provider is a hard error BEFORE launch - never a silent drop.
    // Message shape mirrors --permission-mode. (opencode's bg lane DOES reach
    // these checks now: `--add-dir`/`--agent`-shaped flags on an opencode bg
    // spawn refuse HERE - the serve lane grants writable dirs itself, through
    // the per-session permission rules.)
    if substrate != "pane" {
        // No "use --substrate pane" advice: unlike --permission-mode, pane does
        // NOT map these cells any wider than bg/headless does (gemini --add-dir,
        // codex --agent fail closed on pane too), so that advice would mislead.
        let unsupported = |flag: &str| {
            eprintln!(
                "{} is not supported for harness {}; drop it or use a harness that maps it",
                flag,
                py_repr(provider)
            );
        };
        if provider == "codex" && substrate == "bg" && add_dir.is_some() {
            unsupported("--add-dir");
            return Some(2);
        }
        // --add-dir: claude/codex/agy map it; gemini has no verified equivalent.
        if add_dir.is_some() && !matches!(provider, "claude" | "codex" | "agy") {
            unsupported("--add-dir");
            return Some(2);
        }
        // --agent / --tools / --deny-tools: claude-only on this lane.
        if agent.is_some() && provider != "claude" {
            unsupported("--agent");
            return Some(2);
        }
        if tools.is_some() && provider != "claude" {
            unsupported("--tools");
            return Some(2);
        }
        if deny_tools.is_some() && provider != "claude" {
            unsupported("--deny-tools");
            return Some(2);
        }
    }
    // fno's computed writable-dir set, published by the Python spawn seam. A
    // worker that cannot write ~/.fno takes no node claim, and the graph then
    // reads that node free while it works. Read once here so every lane below
    // carries it.
    let state_dirs = fno_agents::claude_ask::state_dirs_from_env();
    // The claude-only bundle, resolved once for both claude lanes.
    let claude_flags = fno_agents::claude_ask::HarnessFlags {
        add_dir,
        state_dirs: &state_dirs,
        agent,
        allowed_tools: tools,
        disallowed_tools: deny_tools,
    };

    // Spawn gate (x-c5cc): cap + RAM floor for the CLIENT-SIDE substrates only.
    // `pane` re-execs into the Python CLI whose mirrored gate is the sole gate
    // on that path (exactly one gate evaluation per spawn, LD1). The guard is
    // held across dispatch so the next waiter's count includes the newcomer
    // (bg: the mutex until the roster/registry row exists; headless: the
    // worker:<name> slot claim for the call duration), then dropped.
    // x-de10 (AC19): the codex THREAD spawn's registry row is created by the
    // DAEMON spawn RPC (the ("codex", "bg") arm below returns None), so a
    // guard held here drops before any row exists and two rapid thread spawns
    // both pass the cap. The gate moves to run(), wrapped around that RPC.
    let codex_thread_fallthrough = provider == "codex" && substrate == "bg";
    let mut gate_guard = if substrate == "pane" || codex_thread_fallthrough {
        None
    } else {
        let flags = gate_flags_from_params(&params);
        let config_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        match fno_agents::spawn_gate::run_gate(
            &config_cwd,
            &home.registry_json(),
            name,
            substrate,
            flags,
        ) {
            Ok(g) => Some(g),
            Err(code) => return Some(code),
        }
    };

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
        // pane (default): mux-hosted since 4a-G2. The caller intercepts pane
        // spawns BEFORE this fn and re-execs the Python CLI (mux_spawn back
        // half), so this arm is unreachable; None keeps the match total.
        (_, "pane") => None,

        // claude bg: the detached `claude --bg` thread (appears in `claude
        // agents`; attach/peek/reply; NOT a grid pane). claude-only by nature.
        ("claude", "bg") => {
            let claude_home = ClaudeHome::from_env();
            let daemon_receipt = match fno_agents::claude_ask::preflight_claude_daemon(&claude_home)
            {
                Ok(fno_agents::claude_ask::ClaudeDaemonPreflight::Ready(receipt)) => Some(receipt),
                Ok(fno_agents::claude_ask::ClaudeDaemonPreflight::NeedsBootstrap) => None,
                Err(error) => {
                    eprintln!(
                        "claude spawn refused: harness=claude observed=unreadable remedy=repair the Claude daemon roster: {error}"
                    );
                    if let Some(g) = gate_guard.as_mut() {
                        g.release();
                    }
                    return Some(13);
                }
            };
            // The refusal carrier rides the inherited env (see the x-8151 note
            // above), so extra_env stays empty: a worker that drops the flag
            // post-compaction still folds the refusal at init.
            let mut outcome = dispatch_claude_spawn(
                home,
                &claude_home,
                name,
                &message,
                from_name,
                &cwd,
                yolo,
                timeout,
                &[],
                model,
                permission_mode,
                effort,
                claude_flags,
                surface_cwd,
            );
            if outcome.exit_code == 0 {
                let daemon_receipt = match daemon_receipt {
                    Some(receipt) => Ok(receipt),
                    None => fno_agents::claude_ask::ensure_claude_daemon(&claude_home),
                };
                match daemon_receipt {
                    Ok(receipt) => {
                        fno_agents::claude_ask::stamp_daemon_receipt(&mut outcome, &receipt)
                    }
                    Err(error) => {
                        outcome.exit_code = 13;
                        outcome.stdout.clear();
                        outcome.stderr.push_str(&format!(
                            "claude spawn refused after create: harness=claude observed=unreadable remedy=inspect the Claude daemon roster: {error}\n"
                        ));
                    }
                }
            }
            if !outcome.stderr.is_empty() {
                eprint!("{}", outcome.stderr);
            }
            if !outcome.stdout.is_empty() {
                print!("{}", outcome.stdout);
            }
            // Flush the receipt BEFORE the bounded QoS roster poll so
            // line-parsing consumers never wait on the demotion (x-c5cc).
            use std::io::Write;
            let _ = std::io::stdout().flush();
            // The roster row exists once dispatch returned: release the gate
            // NOW so the ~10s demotion poll never serializes other spawns
            // behind the spawn-gate mutex (codex P2).
            if let Some(g) = gate_guard.as_mut() {
                g.release();
            }
            if outcome.exit_code == 0 {
                // The bg worker is claude's child (its exec can't be wrapped);
                // demote post-hoc via the roster pid. short_id from the JSON
                // receipt line (parsed, not string-split — gemini HIGH).
                let parsed: Option<serde_json::Value> =
                    serde_json::from_str(outcome.stdout.trim()).ok();
                if let Some(sid) = parsed
                    .as_ref()
                    .and_then(|v| v.get("short_id"))
                    .and_then(|s| s.as_str())
                    .filter(|s| !s.is_empty())
                {
                    let config_cwd =
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                    fno_agents::spawn_gate::qos_demote_bg_worker(&config_cwd, sid);
                }
            }
            Some(outcome.exit_code)
        }
        // claude headless: a truly headless `claude -p` one-shot (no thread, no
        // grid row; runs to completion and exits). The one place claude shells
        // `-p` (Locked Decision 4); ask/relay keep `--bg`.
        ("claude", "headless") => {
            let claude_home = ClaudeHome::from_env();
            emit!(dispatch_claude_headless(
                &claude_home,
                name,
                &message,
                from_name,
                &cwd,
                yolo,
                timeout,
                model,
                permission_mode,
                effort,
                claude_flags,
            ))
        }

        // codex/gemini/agy headless: the client-side one-shot (codex --exec /
        // gemini -p / agy -p). x-c772: --model is forwarded to each (exact
        // passthrough to the provider CLI's own --model).
        ("codex", "headless") => {
            // The daemon receipt is teardown telemetry for a one-shot spawn
            // (append_daemon_receipt only stamps stderr on success), never a
            // precondition: a codex whose app-server cannot boot (a stubbed
            // CLI on PATH, a partial install, CI) still runs `codex --exec`,
            // exactly like the Python lane that has no daemon step - refusing
            // here was exit-code drift against every such environment. The
            // daemon stays mandatory where it is load-bearing (inject/mail
            // ensure their own).
            let daemon_receipt = match fno_agents::codex_inject::ensure_codex_daemon() {
                Ok(result) => Some(result.receipt),
                Err(error) => {
                    eprintln!("spawn: harness=codex daemon-ensure degraded: {error}");
                    None
                }
            };
            let mut outcome = dispatch_codex_once(
                home, name, &message, from_name, &cwd, yolo, timeout, model, effort, add_dir,
            );
            if let Some(receipt) = daemon_receipt.as_ref() {
                fno_agents::codex_ask::append_daemon_receipt(&mut outcome, receipt);
            }
            emit!(outcome)
        }
        ("gemini", "headless") => emit!(dispatch_gemini_once(
            home, name, &message, from_name, &cwd, yolo, timeout, model,
        )),
        // opencode headless: the client-side one-shot `opencode run --auto`
        // (x-567d wires the documented lane; the bare `opencode` TUI stays the
        // `pane` form). Stateless plain-text, like agy.
        ("opencode", "headless") => emit!(dispatch_opencode_once(
            home, name, &message, from_name, &cwd, yolo, timeout, model, effort,
        )),

        // opencode bg: the serve-HTTP worker lane (x-d9f9). A shared
        // `opencode serve` hosts a persistent session bound to the worker
        // cwd; a detached `opencode run --attach` writer streams the turn's
        // JSON events to the log. The spawn returns immediately - the session
        // on the serve IS the worker (steering/mail over the API is a filed
        // follow-up).
        ("opencode", "bg") => emit!(fno_agents::opencode_serve::dispatch_opencode_serve(
            home, name, &message, from_name, &cwd, model, effort,
        )),

        ("agy", "headless") => {
            // agy is stateless (plain text, no session id): a one-shot `agy -p`.
            // It ignores `yolo` (headless create always passes
            // --dangerously-skip-permissions) and honors optional effort/model.
            emit!(dispatch_agy_once_with_effort(
                home, name, &message, from_name, &cwd, model, effort, timeout, add_dir,
            ))
        }

        // Codex thread is supervisor-hosted by the daemon. Returning `None`
        // preserves the request's `substrate=thread` so the daemon can own the
        // held app-server process and register its full thread identity.
        ("codex", "bg") => None,

        // The three arms above stay NAMED rather than lane-routed, because
        // they are three different ownership models and only two of them are
        // what the contract says they are: claude's thread is hosted by the
        // detached client itself, codex's by its own app-server, and
        // opencode's by a serve-hosted HTTP session. opencode is the
        // deliberate exception - its capability row declares
        // `interactive_attach` unsupported, so the derived lane reads
        // `keeper`, while its serve lane is built and working. Routing this
        // match on the derived lane would send a working path to a refusal;
        // the mismatch belongs to the capability row, not here.
        //
        // The REFUSAL is derived, because a provider name list goes stale the
        // moment a lane is built and then misdirects the reader it was meant
        // to help. Hard error pointing to headless; never a silent substrate
        // swap.
        (other, "bg") => {
            eprintln!("{}", bg_substrate_refusal(other));
            Some(2)
        }

        // Unreachable: provider is validated known above and substrate is
        // validated to pane|bg|headless in build_request.
        _ => None,
    }
}

/// The `--substrate bg` refusal, derived from the capability contract instead
/// of a provider name list.
///
/// A name list cannot say what is actually true. A harness whose keeper lane
/// is built and journey-proven, but whose spawn arm is not, is refused here
/// today; a list reading "claude + codex + opencode" tells that reader the
/// harness has no thread lane, which is the opposite of its situation. The
/// lane is what the reader needs, and the contract already knows it.
///
/// Mirrors the wording `resolve_dispatch` uses in `harness_map.py`, so both
/// runtimes name the same gap: it is in fno, never a harness limitation.
fn bg_substrate_refusal(harness: &str) -> String {
    use fno_agents::claude_ask::py_repr;

    // Name `thread`, not the `bg` selector this match arm is keyed on. `bg` is
    // the deprecated alias, so a user who typed `--substrate thread` was being
    // refused in a vocabulary they did not use and are being moved off.
    let head = format!(
        "substrate 'thread' (detached interactive session) is unavailable on harness {}",
        py_repr(harness)
    );
    let tail = "use --substrate headless for a one-shot";
    let lane = fno_agents::harness_capabilities::HarnessContract::packaged()
        .ok()
        .and_then(|contract| contract.thread_lane(harness).ok());
    match lane {
        // No resume form at all, so there is no lane for fno to build.
        Some("none") => {
            format!("{head}: it declares no resume form, so no thread lane exists for it - {tail}")
        }
        Some(lane) => format!(
            "{head}: fno has not built this harness's {lane} lane spawn arm yet, and that gap is \
             in fno, never a harness limitation - {tail}"
        ),
        // An unreadable table is its own diagnosis, and naming a lane we could
        // not resolve would be a guess wearing a verdict's clothes.
        None => format!(
            "{head}: its thread lane could not be resolved from the capability contract - {tail}"
        ),
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
    let provider_param = params.get("provider").and_then(|v| v.as_str());
    let msg = match provider_param {
        // `select_provider` validates the requested provider FIRST, so an
        // unknown `--provider` surfaces the "unknown provider" error.
        Some(p) if !KNOWN_PROVIDERS.contains(&p) => format!(
            "unknown provider {}; supported: {}",
            py_repr(p),
            known_providers_csv()
        ),
        // New agent with no resolvable provider.
        _ => format!(
            "provider is required for new agent {}; pass --provider one of: {}",
            py_repr(name),
            known_providers_csv()
        ),
    };
    eprintln!("{}", msg);
    2
}

/// One-line pointers for the verbs retired at G4 (x-f54c): the grid, the
/// WebSocket drive surface, and the interactive daemon PTY hosting behind
/// `host`/`promote` moved to the mux. Returns `None` for a live verb. Callers
/// print the pointer and exit non-zero so a script never reads a retired verb
/// as a silent success (AC5-EDGE).
fn retired_verb_pointer(verb: &str) -> Option<&'static str> {
    match verb {
        "grid" => Some(
            "fno agents grid was retired at G4: agent panes now live in the mux. \
             Open `fno mux`, or script panes with `fno mux pane ls|read|run|send|wait|kill`.",
        ),
        "drive" => Some(
            "fno agents drive was retired at G4: drive an agent pane in the mux. \
             Use `fno mux pane send <pane> --raw ...` for keystrokes (without --raw the payload is wrapped in an <fno_mail> envelope), or open `fno mux` and type into the pane.",
        ),
        "host" => Some(
            "fno agents host was retired at G4: spawn a mux-hosted agent pane with \
             `fno agents spawn --name <n> --substrate pane`.",
        ),
        "promote" => Some(
            "fno agents promote was retired at G4: the mux hosts agent panes; spawn one with \
             `fno agents spawn --name <n> --substrate pane`.",
        ),
        _ => None,
    }
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
/// the count removed and, for each row KEPT, the specific gate that kept it
/// (dirty/unprobed worktree, no positive corroboration yet - x-9de7 task 5 -
/// or the liveness re-check itself, x-98ab) so a stuck row is never silent
/// and invisible, and a zero-reap pass over a live fleet is never silent
/// about the rows it kept. The grace window is resolved
/// from `config.agents.dead_row_grace` exactly as the daemon does.
///
/// `--dry-run` runs the identical classification with no registry write and no
/// `agent_row_reaped` event - a reaper an operator cannot rehearse is one they
/// will not run.
fn run_reap(rest: &[String]) -> i32 {
    let json_out = rest.iter().any(|a| a == "--json" || a == "-J");
    let dry_run = rest.iter().any(|a| a == "--dry-run");
    let extras: Vec<&str> = rest
        .iter()
        .map(String::as_str)
        .filter(|a| *a != "--json" && *a != "-J" && *a != "--dry-run")
        .collect();
    if !extras.is_empty() {
        eprintln!(
            "fno-agents: reap takes no arguments other than --json/--dry-run (got: {})",
            extras.join(" ")
        );
        return 2;
    }
    let home = AgentsHome::from_env();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let grace_for_harness = |harness: &str| {
        std::time::Duration::from_secs(fno_agents::agents_config::dead_row_grace_secs(
            &cwd, harness,
        ))
    };
    let summary = if dry_run {
        fno_agents::daemon::gc_sweep_dry_run(&home, &grace_for_harness)
    } else {
        // Source "daemon" matches the event schema's declared source for
        // agent_row_reaped; the manual verb is the same operation as the tick.
        let emitter = fno_agents::events::EventEmitter::new(home.events_jsonl(), "daemon");
        fno_agents::daemon::gc_sweep(
            &home,
            &emitter,
            &grace_for_harness,
            fno_agents::agents_config::reap_receipt_retain_days(&cwd),
        )
    };

    print!(
        "{}",
        fno_agents::reap_render::render_reap(&summary, json_out, dry_run)
    );
    0
}

/// Render a restart outcome into (stdout line, optional stderr line, exit code).
/// Pure so the observable states (swapped / forced / was-down / failed) are unit
/// testable without spawning a daemon. A failure always carries a stderr line
/// and a nonzero code (Locked Decision: a failed restart is loud, never a silent
/// "restarted"); the forced arm records that a process was KILLED, not drained,
/// and a note (e.g. --force declining a recycled pid) rides on stderr at exit 0.
fn render_restart(
    outcome: &Result<RestartOutcome, RestartError>,
) -> (Option<String>, Option<String>, i32) {
    match outcome {
        Ok(RestartOutcome {
            old_pid: Some(old),
            new_pid,
            forced: false,
            note,
        }) => (
            Some(format!("restarted: pid {old} -> {new_pid}")),
            note.clone(),
            0,
        ),
        Ok(RestartOutcome {
            old_pid: Some(old),
            new_pid,
            forced: true,
            note,
        }) => (
            Some(format!("forced: killed pid {old} -> {new_pid}")),
            note.clone(),
            0,
        ),
        Ok(RestartOutcome {
            old_pid: None,
            new_pid,
            forced: _,
            note,
        }) => (
            Some(format!(
                "daemon was not running; started fresh (pid {new_pid})"
            )),
            note.clone(),
            0,
        ),
        Err(e) => (None, Some(format!("fno-agents: {e}")), 1),
    }
}

/// Dispatch `fno-agents restart`: swap a (possibly stale) daemon for one built
/// from the current binary. SIGTERM the running daemon (graceful drain; PTY
/// workers survive), wait for the socket to clear, lazy-start fresh. With
/// `force`, SIGKILL the lockfile holder first and lazy-start fresh (x-3498).
async fn run_restart(force: bool) -> i32 {
    let home = AgentsHome::from_env();
    let daemon_bin = resolve_daemon_bin();
    let outcome = restart_daemon(&home, &daemon_bin, force).await;
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
        "--harness",
        "--from",
        "--cwd",
        "--message",
        "--name",
        "--session-id",
        "--status",
        "--progress",
        "--from-name",
        "--timeout",
        "--model",
        "--mode",
        "--substrate",
        "--workspace",
        "--squad",
        "--split",
        "--portal",
        "--tab",
        "--at",
        "--permission-mode",
        "--effort",
        "--add-dir",
        "--audit-actor",
        "--audit-reason",
        "--audit-request-id",
        "--audit-reclaimed-bytes",
        "--agent",
        "--tools",
        "--deny-tools",
        "--account",
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
        // The bare `--` seed fence gets the same verbatim treatment: a
        // fenced `--timeout=5 do X` seed must reach the positional drain
        // as-is, not be split by the equals-form rewrite below.
        if tok == "--" {
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

    // x-6de8: three orthogonal axes. --harness/-H names the CLI binary,
    // --provider/-P the model VENDOR, --model the model at that vendor. The vendor
    // is held aside so a harness name typed there fails closed after the loop
    // (the historical confusion) rather than launching the wrong binary.
    let mut harness_val: Option<String> = None;
    let mut vendor_val: Option<String> = None;
    let mut it = normalized.into_iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            // --harness/-H is the CLI-binary axis, the --harness vocabulary the
            // rest of fno uses. -H no longer means headless (that is
            // --substrate headless / --headless / -p / --once now).
            "--harness" | "-H" => {
                harness_val = Some(it.next().ok_or("--harness needs a value")?);
            }
            // --provider/-P is the model-vendor axis. Capital P: -p is headless,
            // mirroring the harnesses' own one-shot short.
            "--provider" | "-P" => {
                vendor_val = Some(it.next().ok_or("--provider needs a value")?);
            }
            // Off `spawn`, -p was the provider short (the harness axis). That axis
            // is --harness/-H everywhere now, and -p/--headless is a spawn-only
            // one-shot, so -p is a loud tombstone here - never silently bound to
            // headless (the arm below) or to a harness. This arm must precede the
            // headless one, which also matches "-p".
            "-p" if verb != "spawn" => {
                return Err(format!(
                    "-p is not valid here; the one-shot short (--headless) is spawn-only, \
                     and the CLI binary is --harness/-H. \
                     (--provider/-p was split at the axis rename.)"
                ));
            }
            "--workspace" | "--squad" | "-s" => {
                params.insert("squad".into(), str_arg(&mut it, "-s/--workspace")?);
            }
            "--split" | "-x" => {
                params.insert("split".into(), str_arg(&mut it, "-x/--split")?);
            }
            // (x-9b60) The portal placement trio, same spellings the mux
            // thread verb uses. Parsed here so the default Rust runtime
            // accepts what the help advertises; the placement itself runs
            // after the spawn receipt (place_thread_portal_after_spawn).
            "--portal" => {
                let raw = str_arg(&mut it, "--portal")?;
                match raw.as_str().and_then(|s| s.parse::<u8>().ok()) {
                    Some(n) => {
                        params.insert("portal".into(), Value::from(n));
                    }
                    None => return Err("--portal takes an index 0-255".into()),
                }
            }
            "--tab" => {
                params.insert("tab".into(), str_arg(&mut it, "--tab")?);
            }
            "--at" => {
                params.insert("at".into(), str_arg(&mut it, "--at")?);
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
            // x-6de8: the agent name rides a flag, so the single positional can be
            // the prompt. The seam normalizer mints one when the caller omits it,
            // so a spawn reaching here normally carries --name; the positional
            // fallback below keeps a direct `fno-agents spawn <name>` working.
            "--name" => {
                params.insert("name".into(), str_arg(&mut it, "--name")?);
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
            "--status" => {
                params.insert("status".into(), str_arg(&mut it, "--status")?);
            }
            "--progress" => {
                params.insert("progress".into(), str_arg(&mut it, "--progress")?);
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
            "--no-wait" => {
                // Spawn-gate escape (x-c5cc): fail immediately at max_live
                // instead of queueing for a free slot. Client-side only.
                params.insert("no_wait".into(), Value::Bool(true));
            }
            "--model" | "-m" => {
                // Exact model name forwarded to the provider CLI's own --model:
                // claude --bg/-p, codex exec, gemini, agy (x-c772 wired the
                // headless one-shots; claude --bg was x-571f). -m is the mobile
                // short. No fuzzy resolution.
                params.insert("model".into(), str_arg(&mut it, "-m/--model")?);
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
            "--permission-mode" => {
                // x-dfa4: provider permission/approval mode. Parsed here so the
                // pane substrate (raw-arg re-exec to Python) is not blocked by an
                // unknown-flag error; bg/headless read it in maybe_run_spawn.
                // Mapping + fail-closed validation live at the spawn seam.
                params.insert(
                    "permission_mode".into(),
                    str_arg(&mut it, "--permission-mode")?,
                );
            }
            "--effort" => {
                params.insert("effort".into(), str_arg(&mut it, "--effort")?);
            }
            // x-b6e2: Tier-3 harness-native passthrough. Parsed here (space +
            // equals form via VALUE_FLAGS) so the pane re-exec is not blocked by
            // an unknown-flag error; the mapping + fail-closed live at the spawn
            // seam (maybe_run_spawn) and the Python pane builder.
            "--add-dir" => {
                params.insert("add_dir".into(), str_arg(&mut it, "--add-dir")?);
            }
            "--agent" => {
                params.insert("agent".into(), str_arg(&mut it, "--agent")?);
            }
            "--tools" => {
                params.insert("tools".into(), str_arg(&mut it, "--tools")?);
            }
            "--deny-tools" => {
                params.insert("deny_tools".into(), str_arg(&mut it, "--deny-tools")?);
            }
            "--account" => {
                // x-d012 per-spawn account selection. Parsed here so the spawn
                // arm is not blocked by an unknown-flag error; the four-lane
                // overlay resolution lives in Python (fno.agents.account_env), so
                // an account spawn re-execs the Python CLI on EVERY substrate (see
                // the spawn intercept) rather than duplicating the resolver here.
                params.insert("account".into(), str_arg(&mut it, "--account")?);
            }
            "--audit-actor" => {
                params.insert("audit_actor".into(), str_arg(&mut it, "--audit-actor")?);
            }
            "--audit-reason" => {
                params.insert("audit_reason".into(), str_arg(&mut it, "--audit-reason")?);
            }
            "--audit-request-id" => {
                params.insert(
                    "audit_request_id".into(),
                    str_arg(&mut it, "--audit-request-id")?,
                );
            }
            "--audit-reclaimed-bytes" => {
                let value = str_arg(&mut it, "--audit-reclaimed-bytes")?;
                let bytes = value
                    .as_str()
                    .and_then(|raw| raw.parse::<u64>().ok())
                    .ok_or("--audit-reclaimed-bytes needs a non-negative integer")?;
                params.insert("audit_reclaimed_bytes".into(), Value::from(bytes));
            }
            "--audit-worktree-touched" => {
                params.insert("audit_worktree_touched".into(), Value::Bool(true));
            }
            "--substrate" => {
                // The session-substrate selector (x-2c27): pane (owned-PTY,
                // default) | bg (claude --bg detached thread; opencode
                // serve-hosted session, x-d9f9) |
                // headless (claude -p / codex --exec / agy -p one-shot). The
                // sole routing key the spawn arm reads (replaces --once).
                let v = str_arg(&mut it, "--substrate")?;
                match v.as_str() {
                    Some("pane") | Some("thread") | Some("headless") => {
                        params.insert("substrate".into(), v);
                    }
                    Some("bg") => {
                        eprintln!(
                            "warning: substrate value 'bg' is deprecated; use 'thread' instead; the alias will be removed after one release"
                        );
                        params.insert("substrate".into(), Value::String("thread".into()));
                    }
                    other => {
                        return Err(format!(
                            "--substrate must be one of: pane, thread, headless (bg is a deprecated alias; got {})",
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
            "--headless" | "-p" => {
                // Ergonomic front for --substrate headless (x-c772). Same routing
                // key as --once; explicit --substrate already present wins. `-p`
                // mirrors the harnesses' own one-shot short; the vendor axis took
                // the capital -P so this letter could mean what it means in claude.
                params
                    .entry("substrate")
                    .or_insert_with(|| Value::String("headless".into()));
            }
            "--fresh" => {
                // Accepted no-op alias (x-85fe): the worker cwd already defaults
                // to the canonical repo root. Parsed for dispatcher compat.
                params.insert("fresh".into(), Value::Bool(true));
            }
            "--here" | "--in-place" => {
                // Explicit opt-in to the caller's cwd instead of the canonical
                // default (x-85fe): extend WIP right here.
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
            "--" => {
                // End-of-options: everything after is positional (the seed
                // fence, same contract as the Python CLI's click parser).
                for a in it.by_ref() {
                    positional.push(a);
                }
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown flag: {other}"));
            }
            _ => positional.push(a),
        }
    }

    // On `spawn` the two flags are different axes: --harness is the CLI binary,
    // --provider the model vendor. Off spawn the vendor axis routes nothing, so
    // --provider/-P (the retired harness spelling AND the vendor short) is a
    // tombstone: exit 2 with the axis map, never silently forwarded.
    if let Some(v) = vendor_val {
        let v = v.trim().to_string();
        if verb == "spawn" {
            // A harness name on the vendor axis is refused BY NAME. This lane
            // never re-execs Python cmd_spawn, so without this a `--provider
            // claude` reaches the daemon as a vendor it cannot resolve.
            if KNOWN_PROVIDERS.contains(&v.as_str()) || v == "agy" || v == "opencode" {
                return Err(format!(
                    "{v} is a harness, not a provider; use --harness {v}"
                ));
            }
            // The vendor axis only means anything alongside a materialized route,
            // and routing lives in the Python spawn path (the front door keeps
            // every --provider spawn there). Reaching here means the binary was
            // driven directly: say what to run instead of failing downstream.
            return Err(format!(
                "--provider {v} names a model vendor; routing is applied by the fno \
                 CLI (`fno agents spawn ... --provider {v} --model <m>`), not by \
                 fno-agents directly"
            ));
        }
        return Err(format!(
            "--provider/-P was split at the axis rename: the CLI binary is --harness/-H; \
             a model vendor is only routable at spawn \
             (`fno agents spawn --provider {v} --model <m>`)."
        ));
    }
    if let Some(v) = harness_val {
        params.insert("provider".into(), Value::String(v));
    }

    if let Some(av) = argv {
        params.insert(
            "argv".into(),
            Value::Array(av.into_iter().map(Value::String).collect()),
        );
    }

    let method = match verb {
        "spawn" => {
            // With --name the whole positional tail is the message; without it the
            // first positional is still the name (a direct `fno-agents spawn`
            // bypasses the seam normalizer that would have minted one).
            let msg_from = if params.contains_key("name") {
                0
            } else {
                let name = positional.first().ok_or("spawn needs a <name> or --name")?;
                params.insert("name".into(), Value::String(name.clone()));
                1
            };
            if !params.contains_key("message") && positional.len() > msg_from {
                params.insert(
                    "message".into(),
                    Value::String(positional[msg_from..].join(" ")),
                );
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
            let pty_capable = params
                .get("provider")
                .and_then(Value::as_str)
                .is_some_and(|p| KNOWN_PROVIDERS.contains(&p));
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
        // `host`/`promote` (interactive daemon PTY hosting) were retired at G4
        // (x-f54c) and intercepted with a mux pointer before build_request; they
        // never reach this match.
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
        "rename" => {
            fno_agents::rename::request(&mut params, &positional)?;
            "agent.rename"
        }
        "reconcile" => "agent.reconcile",
        other => {
            return Err(format!(
                "unknown verb: {other} (expected {})",
                ALL_CLIENT_ACTIONS.join("|")
            ))
        }
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

/// The spawn-control flags one gate evaluation honors: the `--force` and
/// `--no-wait` CLI flags land in the spawn params as booleans. Both gate
/// constructions (the daemon-bound codex-thread gate and the shared one)
/// read through this, so neither can drop a flag.
fn gate_flags_from_params(params: &Value) -> fno_agents::spawn_gate::GateFlags {
    fno_agents::spawn_gate::GateFlags {
        force: params
            .get("force")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        no_wait: params
            .get("no_wait")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

/// Pure cwd precedence for a spawn/ask dispatch: explicit `--cwd` > `--here`
/// (caller) > default canonical. x-85fe inverted the default: with no explicit
/// cwd source the worker lands on the canonical root, so the identical command
/// behaves the same regardless of where the launcher stands; `--here` is the
/// explicit opt-in to keep the caller's cwd. `--fresh` is an accepted no-op
/// alias (the default already resolves canonical). An unresolved canonical
/// (None) falls back to the caller cwd, the safe side. No git / env / IO, so the
/// precedence is unit-testable (Failure Modes > Invariants: `--cwd` is the
/// highest-priority cwd source and wins over everything).
fn effective_worker_cwd(
    explicit_cwd: Option<std::path::PathBuf>,
    _fresh: bool,
    here: bool,
    canonical: Option<std::path::PathBuf>,
    caller: std::path::PathBuf,
) -> std::path::PathBuf {
    if let Some(c) = explicit_cwd {
        return c; // explicit --cwd always wins
    }
    if here {
        return caller; // --here: explicit opt-in to the caller's cwd
    }
    canonical.unwrap_or(caller) // default: canonical; caller on resolution failure
}

/// One-line stderr note when the default (or `--fresh` alias) actually moves the
/// worker cwd off the caller's dir, so the redirect is never silent on any path,
/// default included (x-85fe Locked Decision 5; Failure Modes > Errors).
fn note_fresh_redirect(caller: &std::path::Path, chosen: &std::path::Path) {
    if chosen != caller {
        eprintln!(
            "fno-agents: dispatching from canonical main (default) ({}); pass --here to stay in this worktree",
            chosen.display()
        );
    }
}

/// Resolve the worker cwd for a client-side (claude/codex) spawn/ask dispatch,
/// honoring `--cwd` > `--here` (caller) > default canonical. Shells to git only
/// on the default path (no `--cwd`, no `--here`); emits the redirect note on an
/// actual move. Returns `(cwd, moved)` where `moved` is exactly the note
/// condition, so a caller surfacing `cwd` in a receipt couples to the note with
/// no second, divergent comparison (x-85fe; gemini review). Single source of cwd
/// truth for the two client-side dispatch blocks (claude `ask`, claude `spawn`).
fn resolve_dispatch_cwd(params: &Value) -> (std::path::PathBuf, bool) {
    let caller = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let explicit = params
        .get("cwd")
        .and_then(|v| v.as_str())
        // An empty --cwd is absent, never the empty-string path (Failure Modes >
        // Boundaries; Python's `if cwd:` twin). Without this, canonicalize_cwd("")
        // resolves to the caller dir and suppresses the canonical default -- the
        // exact worktree leak this change prevents (x-85fe review).
        .filter(|s| !s.is_empty())
        .map(canonicalize_cwd);
    let (fresh, here) = fresh_here_flags(params);
    // Default path (no explicit --cwd, no --here) resolves canonical; --fresh is
    // now a no-op alias since canonical IS the default (x-85fe).
    let default_path = explicit.is_none() && !here;
    let canonical = if default_path {
        fno_agents::paths::canonical_repo_root(&caller)
    } else {
        None
    };
    let chosen = effective_worker_cwd(explicit.clone(), fresh, here, canonical, caller.clone());
    let moved = default_path && chosen != caller;
    if moved {
        note_fresh_redirect(&caller, &chosen);
    }
    (chosen, moved)
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
/// - `rm`: names each surface the daemon proved removed or unverified.
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
                // A `reply: null` with `status: "in_flight"` is the codex
                // thread actor's bounded-ask receipt: the turn is still
                // driving, so say THAT instead of printing an empty line that
                // reads as an empty answer.
                if result.get("reply").is_none_or(Value::is_null)
                    && result.get("status").and_then(|v| v.as_str()) == Some("in_flight")
                {
                    let turn_id = result
                        .get("turn_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    return Some(format!(
                        "in flight: turn {turn_id} is still driving; the reply surfaces via \
                         agent_ask_done"
                    ));
                }
                let reply = result.get("reply").and_then(|v| v.as_str()).unwrap_or("");
                Some(reply.to_string())
            }
        }
        "stop" => {
            let outcome = result
                .get("interrupt")
                .and_then(|v| v.as_str())
                .filter(|outcome| *outcome != "no-turn");
            // The daemon REFUSED the stop: its interrupt never settled and the
            // turn is still driving in the worker's worktree. Saying "stopped"
            // here is the report-it-did-not-perform shape the daemon arm exists
            // to prevent, so the word never appears on this path.
            if result.get("stopped").and_then(Value::as_bool) == Some(false) {
                return Some(format!(
                    "stop refused: {name} is still running ({})",
                    outcome.unwrap_or("the interrupt never settled")
                ));
            }
            let mut line = match result.get("short_id").and_then(|v| v.as_str()) {
                Some(short_id) => format!("stopped: {name} ({short_id})"),
                None => format!("stopped: {name}"),
            };
            // A codex thread stop names what happened to the in-flight turn:
            // a bare "stopped" over an interrupt the daemon never confirmed is
            // the exact report-it-did-not-perform shape this field exists to
            // prevent. `no-turn` (nothing was driving) stays silent.
            if let Some(outcome) = outcome {
                line.push_str(&format!(" (turn {outcome})"));
            }
            Some(line)
        }
        "rm" => {
            let harness = result.get("harness").and_then(Value::as_str).unwrap_or("");
            let mut removed = vec!["fno"];
            let mut notes = Vec::new();
            // The harness row we just tore down IS the resume handle. The seam
            // warns BEFORE the reap; this names the reversal AFTER it, so a
            // direct `fno-agents rm` (which never passes the Python seam) is
            // not silent about the loss either.
            let mut adopt_hint: Option<String> = None;
            if !harness.is_empty() {
                let reason = result
                    .get("harness_reason")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let row_id = result
                    .get("harness_row_id")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                // Prefer the FULL session id over `harness_row_id`: that field
                // falls back to this id's first eight chars, which is not a
                // unique adopt key for a codex row (time-prefixed ids collide
                // across same-window sessions) and is not even hex for a
                // non-uuid id. Only name a handle that resolves back.
                let adopt_key = result
                    .get("harness_session_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .unwrap_or(row_id);
                match result.get("harness_removed").and_then(Value::as_bool) {
                    Some(true) => {
                        removed.push(harness);
                        if adopt_key != "unknown" {
                            adopt_hint = Some(format!(
                                "\nthe {harness} session record was the resume handle; \
                                 the transcript stays on disk.\nreverse it with: \
                                 fno agents adopt {adopt_key} --cross-project"
                            ));
                        }
                    }
                    Some(false) if reason.contains("already absent") => {
                        notes.push(format!("{harness} row already absent"))
                    }
                    Some(false) => notes.push(format!("{harness} row {row_id} survives: {reason}")),
                    None if harness == "claude" => {
                        notes.push("claude list unreadable, harness side unverified".to_string())
                    }
                    None if !reason.is_empty() => {
                        notes.push(format!("{harness} side unverified: {reason}"))
                    }
                    None => {}
                }
            }
            let pane_reason = result
                .get("pane_reason")
                .and_then(Value::as_str)
                .unwrap_or("");
            match result.get("pane_removed").and_then(Value::as_bool) {
                Some(true) => removed.push("mux"),
                Some(false) if pane_reason.contains("already absent") => {
                    notes.push("mux pane already absent".to_string())
                }
                Some(false) => {
                    let session = result
                        .get("pane_session")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let pane_id = result
                        .get("pane_id")
                        .and_then(Value::as_u64)
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    notes.push(format!(
                        "mux pane {session}:{pane_id} survives: {pane_reason}"
                    ));
                }
                None => {}
            }
            if result.get("event_written").and_then(Value::as_bool) == Some(false) {
                let reason = result
                    .get("event_reason")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                notes.push(format!("event record not written: {reason}"));
            }
            if result.get("worktree_outcome").and_then(Value::as_str) == Some("removed") {
                let bytes = result
                    .get("reclaimed_bytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                notes.push(format!(
                    "WARNING: worktree removed by guarded cleanup (reclaimed_bytes={bytes})"
                ));
            }
            if removed.len() == 1
                && notes.is_empty()
                && result.get("pane_removed").is_none_or(Value::is_null)
            {
                return Some(format!("removed: {name}"));
            }
            let has_survivor = notes
                .iter()
                .any(|note| note.contains("survives") || note.contains("unverified"));
            let surfaces = if removed.len() == 1 && has_survivor {
                "fno only".to_string()
            } else {
                removed.join(" + ")
            };
            let detail = if notes.is_empty() {
                surfaces
            } else {
                format!("{surfaces}; {}", notes.join("; "))
            };
            Some(format!(
                "removed: {name} ({detail}){}",
                adopt_hint.unwrap_or_default()
            ))
        }
        "rename" => fno_agents::rename::receipt(name, result),
        "list" => {
            let agents = &result["agents"];
            let fields_omitted = &result["fields_omitted"];
            let filters = result.get("filters_applied").cloned().unwrap_or_else(
                || json!({"cwd": null, "provider": null, "status": null, "progress": null}),
            );
            // ab-098967b4: merge the P1 host-local live-session lane. The Rust
            // client owns the rendered surface, so it shells out to the Python
            // helper (which has psutil's cross-platform reuse-safe liveness) and
            // folds the result in. Fail-open: an empty lane on any error.
            let discovered = if discover {
                fetch_discovered_sessions(
                    filters.get("cwd").and_then(|v| v.as_str()),
                    filters.get("provider").and_then(|v| v.as_str()),
                    filters.get("status").and_then(|v| v.as_str()),
                    filters.get("progress").and_then(|v| v.as_str()),
                )
            } else {
                Vec::new()
            };
            if json_flag || !is_tty {
                Some(render_list_json(
                    agents,
                    &filters,
                    fields_omitted,
                    &discovered,
                ))
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
            // Emit the SAME compact single-line JSON receipt that lane produces
            // ({"name","short_id","harness","status"}). The harness axis is
            // reported under `harness`, never under a `provider` key (a provider
            // key holding a harness literal is the axis defect). The in-repo
            // receipt parsers (skills/target/scripts/dispatch-node.sh and
            // backlog/advance.py) read only `short_id`, so the rename is safe.
            // serde_json::to_string (NOT _pretty) keeps it one line for the
            // line-by-line `json.loads` consumers. `--once` spawns are handled
            // client-side and never reach here.
            let short_id = result
                .get("short_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let harness = result.get("harness").and_then(|v| v.as_str()).unwrap_or("");
            let status = result
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("live");
            let session_id = result
                .get("harness_session_id")
                .or_else(|| result.get("session_id"))
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty());
            let mut receipt = json!({
                "name": name,
                "short_id": short_id,
                "harness": harness,
                "status": status,
            });
            if let Some(session_id) = session_id {
                receipt["session_id"] = json!(session_id);
                if result.get("harness_session_id").is_some() {
                    receipt["harness_session_id"] = json!(session_id);
                }
            }
            Some(serde_json::to_string(&receipt).unwrap_or_default())
        }
        _ => None,
    }
}

/// Render agents list as Python-matching JSON (Task 3.1; discovered lane
/// ab-098967b4; provider key restored x-f273).
///
/// Shape (schema_version 6): `{"agents": [...], "count": N,
/// "discovered_sessions": [...], "discovered_count": M, "fields_omitted":
/// [...], "filters_applied": {...}, "schema_version": 6}`. Stays
/// byte-shape-aligned with Python's `format.render_json`.
const LIST_JSON_SCHEMA_VERSION: u32 = 6;

fn render_list_json(
    agents: &Value,
    filters_applied: &Value,
    fields_omitted: &Value,
    discovered: &[Value],
) -> String {
    let count = agents.as_array().map(|a| a.len()).unwrap_or(0);
    let payload = json!({
        "agents": agents,
        "count": count,
        "discovered_sessions": discovered,
        "discovered_count": discovered.len(),
        "fields_omitted": fields_omitted,
        "filters_applied": filters_applied,
        "schema_version": LIST_JSON_SCHEMA_VERSION,
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
fn fetch_discovered_sessions(
    cwd_filter: Option<&str>,
    provider_filter: Option<&str>,
    status_filter: Option<&str>,
    progress_filter: Option<&str>,
) -> Vec<Value> {
    use std::process::Command;

    // No live-only early return. It encoded "a discovered session is live by
    // definition", which the shared reachability verdict retired: a discovered
    // row whose process is provably gone now comes back `orphaned`, so the
    // early return made `--status orphaned` drop through Rust a row that Python
    // prints -- one runtime-dependent answer to one question. The row-level
    // filter at the bottom is the single place status is applied.
    let mut cmd = Command::new("fno");
    cmd.args(["agents", "discovered-json"]);
    cmd.env("FNO_AGENTS_RUNTIME", "python");
    if let Some(c) = cwd_filter {
        cmd.args(["--cwd", c]);
    }
    // Without this the rendered surface disagrees with the Python one:
    // `--harness claude` would list every discovered codex/opencode session.
    // An empty value is "no filter" on the Python side, so forwarding it would
    // make the two runtimes disagree again in the other direction.
    if let Some(p) = provider_filter.filter(|p| !p.is_empty()) {
        cmd.args(["--harness", p]);
    }
    let output = match cmd.output() {
        Ok(o) if o.status.success() => o.stdout,
        _ => return Vec::new(),
    };
    let parsed: Value = match serde_json::from_slice(&output) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut rows: Vec<Value> = parsed
        .get("discovered_sessions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    retain_discovered_by_status(&mut rows, status_filter);
    retain_discovered_by_progress(&mut rows, progress_filter);
    rows
}

/// Keep only the discovered rows whose OWN verdict matches the requested one.
///
/// Split out so it is assertable without the shellout above. The test that
/// used to cover this pinned the early return (`a non-live filter yields
/// nothing`), and once that return was gone the same assertion still passed --
/// vacuously, because the subprocess answers with nothing under test. An
/// assertion that survives the behavior it describes is not covering it.
fn retain_discovered_by_status(rows: &mut Vec<Value>, status_filter: Option<&str>) {
    // A discovered session whose process is provably gone now comes back
    // `orphaned` (the shared reachability verdict), so a live-only run that
    // trusted the CALLER's filter alone would print an orphaned row under a
    // banner that says LIVE.
    if let Some(want) = status_filter {
        rows.retain(|r| r.get("status").and_then(Value::as_str) == Some(want));
    }
}

fn retain_discovered_by_progress(rows: &mut Vec<Value>, progress_filter: Option<&str>) {
    if let Some(want) = progress_filter {
        rows.retain(|r| r.get("progress").and_then(Value::as_str) == Some(want));
    }
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

/// Display cap for the LAST MESSAGE cell, kept in step with Python's
/// `_LAST_MESSAGE_WIDTH` in cli/src/fno/agents/format.py (the two tables are
/// functional parallels, not byte-exact, but the cap is the one value worth
/// holding together).
const LAST_MESSAGE_WIDTH: usize = 40;

/// Right-aligned ellipsis truncation, chars not bytes (mirrors Python's
/// `_truncate`), so a long transcript line cannot own the table.
fn truncate_cell(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else if width <= 1 {
        s.chars().take(width).collect()
    } else {
        let mut t: String = s.chars().take(width - 1).collect();
        t.push('…');
        t
    }
}

/// Render agents list as a human-readable table (Task 3.1; CHECKED/PID added by
/// plan ab-70faa65b, Architecture C).
///
/// Columns: NAME HARNESS STATUS CHECKED PID EVENT AGE LAST MESSAGE CWD. CHECKED
/// is the relative age since the last reconcile probe (`never` when unprobed);
/// it replaces the old always-`-` LIVE column (AC5-UI). PID is the worker pid
/// for a PTY agent (`-` for a one-shot ask, which has no managed process).
/// EVENT AGE is the relative age of the transcript's newest activity and LAST
/// MESSAGE the flattened last-turn text - beside the state column on
/// purpose, so a row claiming to be busy while its transcript is hours old
/// shows the disagreement instead of hiding it. This is a functional table;
/// byte-exact match with Python is not required (Python's table is
/// time-dependent via relative timestamps).
fn render_list_table(agents: &Value, discovered: &[Value]) -> String {
    // HARNESS, not PROVIDER: the column has always shown the harness, and the
    // old heading made a claude-hosted worker on a zai route read as running
    // on claude. Same rename on the Python renderer.
    // ADDRESS sits second, mirroring the Python renderer. `list` auto-routes
    // here whenever an installed binary is present, so a column added only to
    // the Python table would be missing from the surface nearly every reader
    // sees -- and the whole point of the column is that a reader with no
    // address copies NAME, whose durable write queues under a key no drain
    // reads. The value is read off the row (both projections emit `address`),
    // never re-derived, so the two tables cannot disagree.
    let headers = [
        "NAME",
        "ADDRESS",
        "HARNESS",
        "STATUS",
        "CHECKED",
        "PID",
        "EVENT AGE",
        "LAST MESSAGE",
        "CWD",
    ];
    let empty_arr = vec![];
    let rows = agents.as_array().unwrap_or(&empty_arr);
    let now = chrono::Utc::now();

    // Compute display values for each row
    let display: Vec<[String; 9]> = rows
        .iter()
        .map(|r| {
            let name = r["name"].as_str().unwrap_or("-").to_string();
            let address = r["address"].as_str().unwrap_or("-").to_string();
            let harness = r["harness"].as_str().unwrap_or("-").to_string();
            let status = r["status"].as_str().unwrap_or("-").to_string();
            let checked = render_checked(r["last_reconciled_at"].as_str(), now);
            let pid = r["pid"]
                .as_u64()
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string());
            // `unknown`, not `never`: no transcript was READ, which is an
            // absent reading, not a claim that no event ever happened.
            let event_age = match r["last_event_at"].as_str() {
                Some(ts) => render_checked(Some(ts), now),
                None => "unknown".to_string(),
            };
            // The transcript's LAST turn, not the registry timestamp this
            // column was wired to for its whole life: that field is null on
            // many rows while the worker is mid-sentence, so a "last message"
            // column that never showed a message. Capped so one long line
            // cannot blow out CWD.
            let last_msg = r["last_message"]
                .as_str()
                .map(|s| truncate_cell(s, LAST_MESSAGE_WIDTH))
                .unwrap_or_else(|| "-".to_string());
            let cwd = r["cwd"].as_str().unwrap_or("-").to_string();
            [
                name, address, harness, status, checked, pid, event_age, last_msg, cwd,
            ]
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
        headers[7].len(),
        headers[8].len(),
    ];
    for row in &display {
        for (i, cell) in row.iter().enumerate() {
            // Chars, not bytes: the `{:<width$}` pad below counts chars, so a
            // byte width on non-ASCII text (a CJK cwd, an emoji message) pads
            // past the intended column and shoves the rest of the row wide.
            widths[i] = widths[i].max(cell.chars().count());
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
/// distinct. Columns: ADDRESS (the mailbox) LABEL (friendly alias) STATUS
/// PROJECT CWD.
///
/// ADDRESS leads and the alias is demoted to LABEL, matching the Python
/// renderer. The alias led this table for its whole life, which made it the
/// leftmost thing a reader copied, and `<project>-<short8>` is not an address.
/// The value is read off the row rather than derived here: `to_row` resolves it
/// from the session's own harness, so this renderer and the Python one cannot
/// answer differently about the same session.
fn render_discovered_section(discovered: &[Value]) -> String {
    let headers = ["ADDRESS", "LABEL", "STATUS", "PROJECT", "CWD"];
    let display: Vec<[String; 5]> = discovered
        .iter()
        .map(|r| {
            let address = r["address"].as_str().unwrap_or("-").to_string();
            let label = r["handle"].as_str().unwrap_or("-").to_string();
            let status = r["status"].as_str().unwrap_or("-").to_string();
            let project = r["project"].as_str().unwrap_or("-").to_string();
            let cwd = r["cwd"].as_str().unwrap_or("-").to_string();
            [address, label, status, project, cwd]
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
            // Chars, not bytes: the `{:<width$}` pad below counts chars, so a
            // byte width on non-ASCII text (a CJK cwd, an emoji message) pads
            // past the intended column and shoves the rest of the row wide.
            widths[i] = widths[i].max(cell.chars().count());
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
        // Distinct from Internal on purpose: a caller racing daemon teardown
        // can retry, where a real Internal fault should not be retried blind.
        ErrorCode::ShuttingDown => 19,
        // Also distinct, and for the opposite reason: two fno builds disagree
        // about the registry schema, so retrying changes nothing. The repair is
        // a deploy or a redirect. A caller that folded this into Internal would
        // report a daemon fault, which is how the 2026-08-28 outage was
        // misdiagnosed twice.
        ErrorCode::SchemaMismatch => 20,
    }
}

fn warns_on_daemon_drift(verb: &str) -> bool {
    matches!(verb, "list" | "rm")
}

/// True when `--help`/`-h` appears in the verb's OWN options, i.e. before an
/// `--argv`/`--` payload boundary. A `--help` after that boundary belongs to a
/// spawned command's argv (e.g. `spawn wk --harness codex --argv -- tool
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

#[cfg(test)]
#[path = "../client_tests.rs"]
mod tests;
