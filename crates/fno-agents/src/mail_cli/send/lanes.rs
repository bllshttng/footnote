// Ported from the single-file draft of send.rs; split by question.
use super::engine::*;
use super::resolve::*;
use super::verify::*;
use super::*;

// ---------------------------------------------------------------------------
// The name lane (cli.py _name_lane_send)
// ---------------------------------------------------------------------------

/// One bus row append through the shared announce door, staging and audit
/// records included.
pub(super) fn bus_append_row(row: &Value) -> Result<(), String> {
    let live = bus_log_path();
    let chats_dir = live
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    crate::announce::append_line_open(&live, row, &chats_dir)
}

/// Python `stage_before_live`'s record shape: the caller's fields with Nones
/// dropped, minting nothing.
#[allow(clippy::too_many_arguments)]
pub(super) fn stage_before_live(
    msg_id: &str,
    sender: &str,
    recipient: &str,
    body: &str,
    from_harness: Option<&str>,
    to_harness: Option<&str>,
    in_reply_to: Option<&str>,
    from_session: Option<&str>,
    from_model: Option<&str>,
    to_kind: Option<&str>,
    word_count: Option<i64>,
    subject: Option<&str>,
) -> Result<(), String> {
    let row = bus_envelope_row(
        msg_id,
        msg_id,
        sender,
        recipient,
        "send",
        body,
        &now_iso(),
        in_reply_to,
        json!({}),
        from_harness,
        to_harness,
        from_session,
        from_model,
        to_kind,
        word_count,
        None,
        subject,
    );
    bus_append_row(&row)
}

/// Python `record_hosted_delivery`: the audit row naming the lane actually
/// injected into.
#[allow(clippy::too_many_arguments)]
pub(super) fn record_hosted_delivery(
    record: &Value,
    to_harness: Option<&str>,
    to_session: Option<&str>,
) -> Result<(), String> {
    let mut row = record.clone();
    let obj = row.as_object_mut().ok_or("record not an object")?;
    obj.insert("delivery".to_string(), json!("hosted"));
    if let Some(h) = to_harness.filter(|h| !h.is_empty()) {
        obj.insert("to_harness".to_string(), json!(h));
    }
    let mut meta = serde_json::Map::new();
    if let Some(s) = to_session.filter(|s| !s.is_empty()) {
        meta.insert("to_session".to_string(), json!(s));
    }
    if let Some(h) = to_harness.filter(|h| !h.is_empty()) {
        meta.insert("to_harness".to_string(), json!(h));
    }
    if !meta.is_empty() {
        obj.insert("meta".to_string(), Value::Object(meta));
    }
    bus_append_row(&row)
}

/// Python `durable_leg_story` caller: the positive wording or None.
pub(super) fn durable_leg_story_or_none(
    reason: Option<&str>,
    recipient: Option<&str>,
) -> Option<String> {
    if !crate::mail_receipt::is_live_lane_failure(reason) {
        return None;
    }
    let suffix = recipient.map(live_miss_age_suffix);
    crate::mail_receipt::durable_leg_story(reason, suffix.as_deref(), recipient)
}

/// Python `_warn_deferred`: the stderr recovery ladder by arm.
pub(super) fn warn_deferred(target: &str, project: bool, reason: Option<&str>) {
    let arm = if project {
        "project"
    } else if reason == Some(LOCK_TIMEOUT_REASON) {
        "lock"
    } else if crate::mail_receipt::is_live_lane_failure(reason) {
        "live"
    } else {
        "plain"
    };
    let head = if arm == "plain" {
        deferred_liveness_head(target)
    } else {
        None
    };
    let rendered = crate::mail_receipt::warn_deferred(target, arm, reason, head.as_deref());
    eprintln!("{rendered}");
}

/// Python `bounce_reason`: the hold's clock receipt, or None when no hold
/// runs.
pub(super) fn bounce_reason(recipient: &str) -> Option<String> {
    let now = chrono::Utc::now();
    let verdict = crate::mail_hold::gate(recipient, None, now);
    if verdict.deliver {
        return None;
    }
    verdict.receipt.filter(|r| !r.is_empty())
}

/// The bus record the name lane stages and receipts with.
#[allow(clippy::too_many_arguments)]
pub(super) fn name_lane_record(
    msg_id: &str,
    sender: &str,
    recipient: &str,
    wrapped: &str,
    from_harness: Option<&str>,
    in_reply_to: Option<&str>,
    from_session: Option<&str>,
    from_model: Option<&str>,
    word_count: Option<i64>,
    subject: Option<&str>,
) -> Value {
    bus_envelope_row(
        msg_id,
        msg_id,
        sender,
        recipient,
        "send",
        wrapped,
        &now_iso(),
        in_reply_to,
        json!({}),
        from_harness,
        None,
        from_session,
        from_model,
        Some("name"),
        word_count,
        None,
        subject,
    )
}

/// Python `_name_lane_send`'s outcomes.
pub(super) enum NameLane {
    /// A receipt was printed; the caller's flow ends.
    Done,
    /// The token resolved nowhere; the caller owns the exit-16 refusal.
    UnreachableToken,
    AmbiguousToken(Vec<String>),
    UnavailableToken {
        failed: Vec<String>,
        candidates: Vec<String>,
    },
}

pub(super) struct NameLaneArgs<'a> {
    message: &'a str,
    from_name: Option<&'a str>,
    resolved: Option<ReachableSession>,
    recipient: Option<&'a str>,
    provider: Option<&'a str>,
    reply_to: Option<&'a str>,
    token: Option<&'a str>,
    style_exception: Option<&'a str>,
    force: bool,
    origin: Option<&'a str>,
    subject: Option<&'a str>,
}

/// Python `_name_lane_send`: the ONE choke point every delivery ladder rung
/// lives in. Returns the lane outcome; the caller prints the exit refusals.
#[allow(clippy::too_many_lines)]
pub(super) fn name_lane_send(args: NameLaneArgs) -> Result<NameLane, i32> {
    let mut recipient: Option<String> = args.recipient.map(str::to_string);
    let mut provider = args.provider.map(str::to_string);
    let mut recipient_session: Option<String> = None;
    let mut self_send = false;
    let mut token_reachable: Option<ReachableSession> = None;
    let mut token_lane: Option<String> = None;
    if let Some(resolved) = &args.resolved {
        recipient_session = Some(resolved.session_id.clone());
        recipient = Some(crate::identity::canonical_handle(&resolved.session_id));
        provider = Some(resolved.agent.clone());
        self_send = self_recipient(
            recipient.as_deref().unwrap_or_default(),
            Some(&resolved.session_id),
            false,
        )
        .is_some();
    } else if let Some(token) = args.token {
        // Resolve BEFORE addressing; a clean miss may still be this session.
        let mut self_recipient_hit: Option<String> = None;
        match resolve_token(token) {
            Ok((reachable, lane)) => {
                token_reachable = reachable;
                token_lane = lane;
            }
            Err(TokenFailure::Ambiguous { candidates }) => {
                return Ok(NameLane::AmbiguousToken(candidates));
            }
            Err(TokenFailure::Unavailable { failed, candidates }) => {
                if !is_full_session_id(token) {
                    return Ok(NameLane::UnavailableToken { failed, candidates });
                }
                token_lane = Some(format!("wake=stores-unreadable({})", failed.join(",")));
            }
        }
        if token_reachable.is_none() {
            self_recipient_hit = self_recipient(token, None, token_lane.is_some());
            if self_recipient_hit.is_some() {
                token_lane = Some("self-send".to_string());
            }
        }
        if let Some(reach) = &token_reachable {
            self_recipient_hit = self_recipient(token, Some(&reach.session_id), false);
            if self_recipient_hit.is_some() {
                token_reachable = None;
                token_lane = Some("self-send".to_string());
            }
        }
        self_send = token_lane.as_deref() == Some("self-send");
        if let Some(hit) = self_recipient_hit {
            recipient = Some(hit);
        } else if is_full_session_id(token) {
            recipient = Some(crate::spawn_context::session_identity_key(token));
        } else if let Some(reach) = &token_reachable {
            recipient = Some(crate::identity::canonical_handle(&reach.session_id));
        } else if !is_session_shaped(token) {
            // A non-id token is not a mail address and would strand; --force
            // is the exception because it writes no durable row.
            let forced_entry = if args.force {
                resolve_pane_entry(None, None, Some(token))
            } else {
                None
            };
            let forced_session = forced_entry.as_ref().and_then(|entry| {
                opt_str_field(entry, "harness_session_id")
                    .or_else(|| opt_str_field(entry, "session_id"))
            });
            match forced_session {
                Some(sid) => {
                    recipient = Some(crate::identity::canonical_handle(&sid));
                    provider = forced_entry
                        .as_ref()
                        .and_then(|e| opt_str_field(e, "harness"))
                        .or(provider);
                    recipient_session = Some(sid);
                }
                None => return Ok(NameLane::UnreachableToken),
            }
        } else {
            recipient = Some(token.to_string());
        }
        if recipient_session.is_none() {
            recipient_session = token_reachable
                .as_ref()
                .map(|r| r.session_id.clone())
                .or_else(|| {
                    is_full_session_id(token)
                        .then(|| crate::spawn_context::session_identity_key(token))
                });
        }
        provider = Some(
            token_reachable
                .as_ref()
                .map(|r| r.agent.clone())
                .or(provider)
                .unwrap_or_else(|| "claude".to_string()),
        );
    }
    let Some(recipient) = recipient else {
        return Ok(NameLane::UnreachableToken);
    };

    // Mint the msg-id ONCE, before wrapping.
    let msg_id = generate_msg_id();
    let sender = stamp_from(args.from_name);
    let reservation = reserve_send_budget(&sender, &recipient, args.message, &msg_id);
    if let Err((msg, code)) = &reservation {
        eprintln!("{msg}");
        return Err(*code);
    }
    let reservation = reservation.expect("checked above");
    let authored_words = crate::backlog::style_check::word_count(args.message);
    let sender_harness = resolve_self_harness();
    let sender_model = resolve_self_model();
    let sender_session = reply_session_for(args.from_name);

    let envelope_input = |to_session: Option<&str>| {
        json!({
            "mode": "wrap",
            "body": args.message,
            "from": sender,
            "harness": sender_harness,
            "to": recipient,
            "id": msg_id,
            "reply_to": args.reply_to,
            "from_session": sender_session,
            "origin": args.origin,
            "to_session": to_session,
            "subject": args.subject,
        })
    };
    let registry_path = AgentsHome::shared_registry_json();
    let wrap = |to_session: Option<&str>| -> String {
        crate::mail_envelope::render_at(&envelope_input(to_session), &registry_path)
            .unwrap_or_else(|_| args.message.to_string())
    };
    let wrapped = wrap(recipient_session.as_deref());

    if args.force {
        if self_send {
            release_budget(&reservation);
            eprintln!(
                "error: --force cannot type into this session's own prompt. Use --to-self --raw to self-inject a verb, or send without --force to leave yourself a durable note."
            );
            return Err(2);
        }
        let entry = resolve_pane_entry(args.resolved.as_ref(), Some(&recipient), args.token);
        let Some(entry) = entry else {
            release_budget(&reservation);
            eprintln!(
                "error: --force needs a registry row naming the recipient's pane, and none resolves for {}. Send without --force to reach the durable bus.",
                py_repr(&recipient)
            );
            return Err(1);
        };
        return forced_pane_send(ForcedPane {
            wrapped: &wrapped,
            entry: &entry,
            recipient: &recipient,
            sender: &sender,
            msg_id: &msg_id,
            reply_to: args.reply_to,
            provider: provider.as_deref(),
            sender_harness: sender_harness.as_deref(),
            sender_session: sender_session.as_deref(),
            sender_model: &sender_model,
            authored_words,
            reservation,
            subject: args.subject,
        });
    }

    let record = name_lane_record(
        &msg_id,
        &sender,
        &recipient,
        &wrapped,
        sender_harness.as_deref(),
        args.reply_to,
        sender_session.as_deref(),
        Some(sender_model.as_str()),
        Some(authored_words as i64),
        args.subject,
    );
    let mut live_reason: Option<String> = None;
    let mut staged = false;
    if !self_send && (args.resolved.is_some() || token_reachable.is_some()) {
        match stage_before_live(
            &msg_id,
            &sender,
            &recipient,
            &wrapped,
            sender_harness.as_deref(),
            None,
            args.reply_to,
            sender_session.as_deref(),
            Some(&sender_model),
            Some("name"),
            Some(authored_words as i64),
            args.subject,
        ) {
            Ok(()) => staged = true,
            Err(exc) => {
                eprintln!("body row not staged; skipping live delivery: {exc}");
                live_reason = Some("stage-failed".to_string());
            }
        }
    }

    let mut injected = false;
    let mut woken_as: Option<String> = None;
    let mut lanes: Vec<String> = Vec::new();
    let mut to_session = recipient_session.clone();
    let mut to_harness: Option<String> = args.resolved.as_ref().map(|r| r.agent.clone());

    if args.resolved.is_none() && args.token.is_some() {
        // The ladder below the discovery miss.
        if token_lane.as_deref() == Some("self-send") {
            lanes.push("self-send".to_string());
        } else if live_reason.as_deref() == Some("stage-failed") {
            lanes.push("stage-failed".to_string());
        } else {
            let probe_target = token_reachable
                .as_ref()
                .map(|r| r.session_id.clone())
                .unwrap_or_else(|| args.token.expect("checked").to_string());
            let probe_agent = token_reachable.as_ref().map(|r| r.agent.clone());
            to_session = Some(probe_target.clone());
            let mut probe_reasons: Vec<String> = Vec::new();
            if probe_agent.as_deref() == Some("codex") {
                injected =
                    mail_inject_codex(&probe_target, &wrapped, Some(&mut probe_reasons), None);
                if injected {
                    to_harness = Some("codex".to_string());
                }
                if !injected && !probe_reasons.is_empty() {
                    live_reason = Some(probe_reasons.join(";"));
                }
            } else {
                let mut reasons: Vec<String> = Vec::new();
                injected = mail_inject_claude(&probe_target, &wrapped, Some(&mut reasons), None);
                if injected {
                    to_harness = Some("claude".to_string());
                }
                if !injected && !reasons.is_empty() {
                    live_reason = Some(reasons.join(";"));
                }
                if !injected && probe_agent.is_none() {
                    let mut both: Vec<String> = Vec::new();
                    injected = mail_inject_codex(&probe_target, &wrapped, Some(&mut both), None);
                    if injected {
                        to_harness = Some("codex".to_string());
                    }
                    if !injected && !both.is_empty() {
                        live_reason = Some(both.join(";"));
                    }
                }
            }
            if !injected {
                lanes.push("inject=not-delivered".to_string());
                if token_reachable.is_none() {
                    if let Some(lane) = &token_lane {
                        lanes.push(lane.clone());
                    } else {
                        release_budget(&reservation);
                        return Ok(NameLane::UnreachableToken);
                    }
                } else if let Some(lane) = &token_lane {
                    lanes.push(lane.clone());
                } else if live_reason.as_deref() == Some(BUS_ONLY_POLICY) {
                    // bus-only declines the wake rung by policy.
                } else {
                    let reach = token_reachable.as_ref().expect("checked");
                    let (ok, woken, wake_lane) = wake_rung(reach, &wrapped);
                    injected = ok;
                    if injected {
                        woken_as = woken;
                    }
                    if let Some(lane) = wake_lane {
                        lanes.push(lane);
                    }
                }
            }
        }
    }

    if args.resolved.is_some() && self_send {
        lanes.push("self-send".to_string());
    } else if staged && args.resolved.is_some() {
        let resolved = args.resolved.as_ref().expect("checked");
        let lane_harness = resolved.agent.clone();
        let keeper_recipient = crate::harness_capabilities::HarnessContract::packaged()
            .ok()
            .and_then(|c| c.thread_lane(&lane_harness).ok())
            .map(|lane| lane == "keeper")
            .unwrap_or(false);
        let keeper_thread_row = keeper_recipient && row_hosts_keeper_thread(&resolved.session_id);
        if provider.as_deref() == Some("claude") {
            let mut reasons = Vec::new();
            injected = mail_inject_claude(&resolved.session_id, &wrapped, Some(&mut reasons), None);
            if !injected && !reasons.is_empty() {
                live_reason = Some(reasons.join(";"));
            }
        } else if provider.as_deref() == Some("codex") {
            let mut reasons = Vec::new();
            injected = mail_inject_codex(&resolved.session_id, &wrapped, Some(&mut reasons), None);
            if !injected && !reasons.is_empty() {
                live_reason = Some(reasons.join(";"));
            }
        } else if keeper_thread_row {
            let mut reasons = Vec::new();
            injected = mail_inject_keeper(
                &resolved.session_id,
                &wrapped,
                &lane_harness,
                Some(&mut reasons),
            );
            if !injected && !reasons.is_empty() {
                live_reason = Some(reasons.join(";"));
            }
        }
        if !injected && !keeper_thread_row {
            // A send addressed by session id never consults the roster... but a
            // MUX-hosted session of any provider would demote to durable with a
            // live pane right there. Gate on live status like the registered
            // path does: an exited row keeps its mux ref, and pane ids are
            // reused across a mux restart, so sending on a stale ref types
            // into an unrelated pane.
            if let Some(entry) = resolve_pane_entry(Some(resolved), Some(&recipient), args.token) {
                if str_field(&entry, "status") == "live" {
                    let pane = mux_pane_send(&entry, &wrapped);
                    injected = pane;
                }
            }
        }
    }

    let live = match args.resolved.as_ref() {
        Some(resolved) => format!(
            " [live {} session {}]",
            resolved.agent,
            crate::identity::canonical_handle(&resolved.session_id)
        ),
        None => String::new(),
    };
    let corr = args
        .reply_to
        .map(|r| format!(" re:{r}"))
        .unwrap_or_default();
    if injected {
        let row = record.clone();
        if let Err(exc) = record_hosted_delivery(&row, to_harness.as_deref(), to_session.as_deref())
        {
            eprintln!(
                "delivery succeeded; receipt row failed; the staged bus row holds the body; do not retry: {exc}"
            );
        }
    }
    if injected && woken_as.is_some() {
        println!(
            "{}",
            crate::mail_receipt::json_line(
                &msg_id,
                args.subject,
                &recipient,
                &format!(
                    "delivered (woken) [revived as bg thread {}]",
                    woken_as.expect("checked")
                ),
            )
        );
        if !corr.is_empty() {
            println!("{}", corr.trim());
        }
        return Ok(NameLane::Done);
    }
    if injected {
        println!(
            "{}",
            crate::mail_receipt::json_line(
                &msg_id,
                args.subject,
                &recipient,
                &format!("delivered (hosted){live}{corr}"),
            )
        );
        return Ok(NameLane::Done);
    }

    if !lanes.is_empty() {
        eprintln!("lanes tried: {}", lanes.join(", "));
    }
    let live_attempted = (args.resolved.is_some() || args.token.is_some()) && !self_send;
    let recipient_live =
        self_send || (args.resolved.is_some() && family1_probe_reachable(&recipient));
    let owner = classify_durable_owner(false, recipient_live, !recipient_live);
    let bus_only = live_reason.as_deref() == Some(BUS_ONLY_POLICY);
    let durable_body = wrap(None);
    let write = write_durable_thread(
        &recipient,
        &stamp_from(args.from_name),
        "send",
        &durable_body,
        &msg_id,
        Some("name"),
        provider.as_deref(),
        sender_harness.as_deref(),
        sender_session.as_deref(),
        Some(&sender_model),
        owner,
        Some(authored_words as i64),
        args.origin,
        args.subject,
        args.reply_to,
    );
    let thread_id = match write {
        Ok(_path) => msg_id.clone(),
        Err(exc) => {
            if !injected {
                release_budget(&reservation);
            }
            eprintln!(
                "durable envelope write failed for {}: {exc}",
                py_repr(&recipient)
            );
            return Err(12);
        }
    };
    if !bus_only {
        warn_deferred(&recipient, false, live_reason.as_deref());
    }
    let hold_note = if bus_only {
        bounce_reason(&recipient)
    } else {
        None
    };
    let reason = if bus_only {
        hold_note.clone().unwrap_or_else(|| {
            "DND (bus-only): recipient polls the bus at each turn boundary".to_string()
        })
    } else {
        let story = durable_leg_story_or_none(live_reason.as_deref(), Some(&recipient));
        let mut reason = story.unwrap_or_else(|| {
            if self_send {
                "self-send".to_string()
            } else {
                live_reason
                    .clone()
                    .unwrap_or_else(|| "live-miss".to_string())
            }
        });
        if reason == "live-miss" {
            reason = format!("live-miss{}", live_miss_age_suffix(&recipient));
        }
        reason
    };
    let window_tail = crate::mail_receipt::window_clause(owner_ttl_hours(owner));
    let mut hint = String::new();
    if hold_note.is_some() {
        hint = format!(" `fno agents mail withdraw {msg_id}` retracts it.");
    }
    if !self_send
        && !bus_only
        && provider.as_deref() == Some("codex")
        && codex_daemon_socket_absent()
    {
        hint = " codex app-server daemon not running: run `codex app-server daemon start`, then restart the session (the socket must exist before the codex TUI starts)".to_string();
    }
    println!(
        "{}",
        crate::mail_receipt::json_line(
            &thread_id,
            args.subject,
            &recipient,
            &format!("queued (durable){live}{corr} [{reason}]{window_tail}"),
        )
    );
    if !hint.is_empty() {
        println!("{}", hint.trim());
    }
    let attended = recipient_is_attended(&recipient);
    let resolved_reachable = args.resolved.is_some() && recipient_live;
    if live_attempted && !bus_only && (attended || resolved_reachable) {
        let esc_reason = if attended {
            "attended-miss"
        } else {
            "reachable-miss"
        };
        if escalate_to_human(
            &stamp_from(args.from_name),
            &recipient,
            args.message,
            esc_reason,
            Some(&msg_id),
        ) == "escalated"
        {
            eprintln!("escalated to human ({recipient}) [{esc_reason}]");
        }
    }
    if !bus_only && !self_send {
        if !report_landing(
            &msg_id,
            &recipient,
            Some(recipient.as_str()),
            to_harness.as_deref().or(provider.as_deref()),
            to_session.as_deref(),
        ) {
            return Err(NOT_LANDED_EXIT);
        }
    }
    Ok(NameLane::Done)
}

// ---------------------------------------------------------------------------
// The lead lane (cli.py _resolve_to_lead_address twin)
// ---------------------------------------------------------------------------

/// Python `role.resolve_to_lead`: every live row holding the role right now,
/// by name, sorted. Territory equality; the alias widening is a documented
/// simplification in this port.
pub(super) fn resolve_to_lead(scope: &str) -> Result<Vec<String>, (String, i32)> {
    let registry = AgentsHome::shared_registry_json();
    let rows = crate::client_verbs::load_registry_entries(&registry).map_err(|exc| {
        (
            format!("error: --to-lead {scope:?}: registry unreadable: {exc}"),
            12,
        )
    })?;
    let mut holders: Vec<String> = Vec::new();
    for row in rows.iter() {
        let role_level = row.get("role_level").is_some_and(Value::is_i64);
        if !role_level {
            continue;
        }
        let role_scope = opt_str_field(row, "role_scope").unwrap_or_default();
        if role_scope.trim() != scope.trim() || scope.trim().is_empty() {
            continue;
        }
        if crate::announce::row_terminal(row) {
            continue;
        }
        holders.push(str_field(row, "name").to_string());
    }
    holders.sort();
    holders.dedup();
    Ok(holders)
}

/// Python `_resolve_to_lead_address`: resolve --to-lead to (holder, body),
/// or refuse. A refusal queues nothing.
pub(super) fn resolve_to_lead_address(
    scope: &str,
    name: Option<String>,
    message: Option<String>,
    conflict: Option<&str>,
) -> Result<(String, String), i32> {
    if conflict.is_some() || message.is_some() {
        let clash = conflict
            .map(str::to_string)
            .unwrap_or_else(|| "a second positional (the message is the only one)".to_string());
        eprintln!(
            "error: --to-lead and {clash} are mutually exclusive. Run `fno agents team` and address that handle for the other lane."
        );
        return Err(2);
    }
    let Some(name) = name else {
        eprintln!("usage: fno agents mail send --to-lead <scope> <message>");
        return Err(2);
    };
    let holders = resolve_to_lead(scope).map_err(|(msg, code)| {
        eprintln!("{msg}");
        code
    })?;
    if holders.is_empty() {
        eprintln!(
            "--to-lead {scope:?} refused: no live row holds this role right now, and nothing was queued. Run `fno agents team` to see who does, or address a specific handle."
        );
        return Err(16);
    }
    if holders.len() > 1 {
        eprintln!(
            "--to-lead {scope:?} is a split role: {} live rows hold it ({}), so it went to none. Run `fno agents team` and resolve it, or address one by name.",
            holders.len(),
            holders.join(", ")
        );
        return Err(17);
    }
    eprintln!("--to-lead {scope}: resolved to {}", holders[0]);
    Ok((holders[0].clone(), name))
}

// ---------------------------------------------------------------------------
// The kind lane, the job lane, the project lane printer
// ---------------------------------------------------------------------------

/// Python `dispatch_send_to_project`: one live peer delivers live by name,
/// none queues durable to the project, many refuse (exit 17) unless --any.
pub(super) fn dispatch_send_to_project(
    project: &str,
    message: &str,
    provider: Option<&str>,
    from_name: &str,
    any_flag: bool,
    origin: Option<&str>,
    subject: Option<&str>,
) -> Result<(DispatchSendResult, Option<String>), (String, i32)> {
    // (result, resolved recipient when a live peer took it)
    validate_send_inputs("placeholder", message, from_name)?;
    if message.len() > 1024 * 1024 {
        return Err((
            format!(
                "message body exceeds maximum size (1 MiB); got {} bytes",
                message.len()
            ),
            2,
        ));
    }
    let (recipient, _candidates, durable) = resolve_to_project(project, any_flag)?;
    if let Some(recipient) = recipient {
        let result = dispatch_send(
            &recipient,
            message,
            provider,
            DEFAULT_LOCK_TIMEOUT,
            from_name,
            origin,
            subject,
        )?;
        return Ok((result, Some(recipient)));
    }
    if !durable {
        unreachable!("resolve_to_project returned neither recipient nor durable");
    }
    let msg_id = generate_msg_id();
    let reservation = reserve_send_budget(from_name, project, message, &msg_id);
    if let Err((msg, code)) = &reservation {
        return Err((msg.clone(), *code));
    }
    let reservation = reservation.expect("checked above");
    let mut from_harness: Option<String> = None;
    let mut from_session: Option<String> = None;
    let registry = AgentsHome::shared_registry_json();
    if let Ok(rows) = crate::client_verbs::load_registry_entries(&registry) {
        for row in rows.iter() {
            if str_field(row, "name") == from_name {
                from_harness = opt_str_field(row, "harness");
                from_session = opt_str_field(row, "harness_session_id")
                    .or_else(|| opt_str_field(row, "short_id"));
            }
        }
    }
    let words = crate::backlog::style_check::word_count(message) as i64;
    let write = write_durable_thread(
        project,
        from_name,
        "send",
        message,
        &msg_id,
        Some("project"),
        None,
        from_harness.as_deref(),
        from_session.as_deref(),
        None,
        "inbox-drain",
        Some(words),
        origin,
        subject,
        None,
    );
    let thread_id = match write {
        Ok(_) => msg_id,
        Err(exc) => {
            release_budget(&reservation);
            emit_event(
                "agent_send_failed",
                json!({"stage": "durable-write", "name": project}),
            );
            return Err((
                format!("durable envelope write failed for project {project:?}: {exc}"),
                12,
            ));
        }
    };
    emit_event(
        "agent_send_done",
        json!({"name": project, "provider": provider.unwrap_or(""), "msg_id": thread_id, "delivery": "durable"}),
    );
    Ok((
        DispatchSendResult {
            msg_id: thread_id,
            delivery: "durable".to_string(),
            reason: None,
            durable_owner: Some("inbox-drain".to_string()),
            to: None,
            to_harness: None,
            to_session: None,
        },
        None,
    ))
}

/// Python `receipts.print_project_demotion`: the stdout receipt(s) for a
/// --to-project send that wrote durable.
pub(super) fn print_project_demotion(
    result: &DispatchSendResult,
    to_project: &str,
    subject: Option<&str>,
) {
    let window_tail = crate::mail_receipt::window_clause(24.0);
    let Some(resolved) = result.to.as_ref() else {
        // No resolved peer: the queue went to the project inbox itself.
        warn_deferred(to_project, true, None);
        println!(
            "{}",
            crate::mail_receipt::json_line(
                &result.msg_id,
                subject,
                to_project,
                &format!("queued (durable) [param-forced: --to-project]{window_tail}"),
            )
        );
        return;
    };
    if result.reason.as_deref() == Some(BUS_ONLY_POLICY) {
        let note = bounce_reason(resolved);
        println!(
            "{}",
            crate::mail_receipt::json_line(
                &result.msg_id,
                subject,
                resolved,
                &format!(
                    "queued (durable) [project {to_project}] [{}]",
                    note.clone().unwrap_or_else(|| {
                        "DND (bus-only): recipient polls the bus at each turn boundary".to_string()
                    })
                ),
            )
        );
        if note.is_some() {
            println!("`fno agents mail withdraw {}` retracts it.", result.msg_id);
        }
        return;
    }
    warn_deferred(resolved, false, result.reason.as_deref());
    let age_target = resolved;
    let suffix = live_miss_age_suffix(age_target);
    let ttl = owner_ttl_hours(result.durable_owner.as_deref().unwrap_or("wake-daemon"));
    let mut door: Vec<String> = vec![
        "demotion".to_string(),
        "--msg-id".to_string(),
        result.msg_id.clone(),
    ];
    if let Some(reason) = &result.reason {
        door.push("--reason".to_string());
        door.push(reason.clone());
    }
    door.push("--ttl-hours".to_string());
    door.push(format!("{ttl:?}"));
    door.push("--target".to_string());
    door.push(resolved.to_string());
    door.push("--project".to_string());
    door.push(to_project.to_string());
    door.push("--age-of".to_string());
    door.push(age_target.to_string());
    door.push("--suffix".to_string());
    door.push(suffix);
    if let Some(sub) = subject {
        door.push("--subject".to_string());
        door.push(sub.to_string());
    }
    let refs: Vec<&str> = door.iter().map(String::as_str).collect();
    match render_receipt(&refs) {
        Ok(text) => println!("{text}"),
        Err(exc) => println!("{exc}"),
    }
}

// ---------------------------------------------------------------------------
// The kind lane (cli.py cmd_send's --kind branch twin)
// ---------------------------------------------------------------------------

/// Python `inbox.store.resolve_project` at the send verb's working dir, or
/// the caller's --from-name. The error text names the real flags.
pub(super) fn resolve_kind_sender(
    from_name: Option<&str>,
    cwd: &Path,
) -> Result<String, (String, i32)> {
    if let Some(name) = from_name {
        return Ok(name.to_string());
    }
    let mut search = cwd.to_path_buf();
    loop {
        for name in [".fno/config.toml", ".fno/settings.yaml"] {
            let candidate = search.join(name);
            if candidate.is_file() {
                if let Ok(text) = std::fs::read_to_string(&candidate) {
                    if let Some(pid) = project_id_from_settings_text(&text) {
                        return Ok(pid);
                    }
                }
            }
        }
        if !search.pop() {
            break;
        }
    }
    Err((
        "error: set 'project' in .fno/config.toml or pass --from-name/--from-self".to_string(),
        2,
    ))
}

/// The kind lane: heads-up / question / fyi are durable project-inbox notes.
/// They ALWAYS queue durable; a question escalates to the human, a heads-up
/// wakes an asleep claude session to drain.
#[allow(clippy::too_many_lines)]
pub(super) fn kind_lane_send(
    args: &SendArgs,
    content: &str,
    recipient_in: &str,
    subject: Option<&str>,
    mail_origin: Option<&str>,
) -> Result<i32, i32> {
    let kind = args.kind.as_deref().unwrap_or("");
    let known = ["heads-up", "question", "fyi"];
    if !known.contains(&kind) {
        if let Some((_, replacement)) = DEPRECATED_KINDS.iter().find(|(k, _)| *k == kind) {
            eprintln!(
                "error: kind {kind:?} was removed in the 2026-05 inbox redesign. Use --kind {replacement} instead."
            );
        } else {
            eprintln!(
                "error: --kind must be one of {} (got {kind:?})",
                known.join(", ")
            );
        }
        return Err(2);
    }
    // US10: question/fyi to a session handle has no drain that reads it.
    if args.to_project.is_none() && (kind == "question" || kind == "fyi") {
        eprintln!(
            "error: --kind {kind} to a session handle ({recipient_in}) has no drain that reads it. Drop --kind to inject it live, or add --to-project <project> to file it as a durable {kind} note."
        );
        return Err(2);
    }
    let mut persist_to_memory = false;
    if let Some(persist) = args.persist.as_deref() {
        if persist != "memory" {
            eprintln!("error: --persist only accepts 'memory' (got {persist:?})");
            return Err(2);
        }
        persist_to_memory = true;
    }
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let recipient = if args.to_project.is_none() {
        // An agent-scoped heads-up resolves to a canonical session handle.
        let reach = resolve_reachable(recipient_in);
        let Some(resolved) = reach.hit else {
            eprintln!("unknown agent or live-session handle: {recipient_in:?}.");
            return Err(UNKNOWN_AGENT_EXIT);
        };
        crate::identity::canonical_handle(&resolved.session_id)
    } else {
        recipient_in.to_string()
    };
    let sender = match resolve_kind_sender(args.from_name.as_deref(), &workdir) {
        Ok(sender) => sender,
        Err((msg, code)) => {
            eprintln!("{msg}");
            return Err(code);
        }
    };
    let msg_id = generate_msg_id();
    let reservation = reserve_send_budget(&sender, &recipient, content, &msg_id);
    if let Err((msg, code)) = &reservation {
        eprintln!("{msg}");
        return Err(*code);
    }
    let refs = json!({
        "ref_pr": args.ref_pr,
        "ref_node": args.ref_node,
        "ref_gate": args.ref_gate,
    });
    let write = match args.reply_to.as_deref() {
        Some(reply_to) => match find_thread_by_msg_id(&recipient, reply_to) {
            Some(thread_path) => append_to_kind_thread(
                &thread_path,
                &sender,
                content,
                &msg_id,
                Some(crate::backlog::style_check::word_count(content) as i64),
                mail_origin,
            ),
            None => write_durable_thread(
                &recipient,
                &sender,
                kind,
                content,
                &msg_id,
                None,
                None,
                None,
                None,
                None,
                "inbox-drain",
                Some(crate::backlog::style_check::word_count(content) as i64),
                mail_origin,
                subject,
                Some(reply_to),
            ),
        },
        None => write_durable_thread(
            &recipient,
            &sender,
            kind,
            content,
            &msg_id,
            None,
            None,
            None,
            None,
            None,
            "inbox-drain",
            Some(crate::backlog::style_check::word_count(content) as i64),
            mail_origin,
            subject,
            None,
        ),
    };
    let thread_id = match write {
        Ok(_) => msg_id,
        Err(exc) => {
            if let Ok(res) = &reservation {
                release_budget(res);
            }
            eprintln!("error: {exc}");
            return Err(2);
        }
    };
    if kind == "question" {
        let esc = escalate_to_human(&sender, &recipient, content, "question", Some(&thread_id));
        if esc == "escalated" {
            eprintln!("escalated to human ({recipient})");
        }
    } else if kind == "heads-up" {
        let policy = crate::mail_hold::gate(&recipient, None, chrono::Utc::now());
        if policy.deliver {
            let reach = resolve_reachable(&recipient);
            if let Some(resolved) = reach.hit {
                if resolved.agent == "claude" {
                    let cwd = resolved.cwd.clone().map(PathBuf::from);
                    let (woke, short) = match cwd {
                        Some(dir) => wake_and_deliver(&resolved.session_id, "", dir.to_str()),
                        None => wake_and_deliver(&resolved.session_id, "", None),
                    };
                    if woke {
                        eprintln!("woke {recipient} to drain (bg thread {short})");
                    }
                }
            }
        }
    }
    if args.json_out {
        let inbox = inbox_dir_for(&recipient).unwrap_or_default();
        println!(
            "{}",
            json!({
                "msg_id": thread_id,
                "thread_path": inbox.to_string_lossy(),
                "appended": true,
            })
        );
    } else {
        let window_tail = crate::mail_receipt::window_clause(24.0);
        println!(
            "{}",
            crate::mail_receipt::json_line(
                &thread_id,
                subject,
                &recipient,
                &format!("queued (durable) [param-forced: --kind {kind}]{window_tail}"),
            )
        );
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// The job lane (job_lane.py + job_address.py twins)
// ---------------------------------------------------------------------------

pub(super) struct JobHolder {
    address: String,
    state: String,
    session_id: Option<String>,
    harness: Option<String>,
    note: Option<String>,
}

impl JobHolder {
    fn has_holder(&self) -> bool {
        (self.state == "live" || self.state == "suspect") && self.session_id.is_some()
    }
}

/// Python `job_address.resolve_job_address`: node:<id> / pr:<n> to the
/// current claim holder over the global claims root.
pub(super) fn resolve_job_address(token: &str) -> Option<JobHolder> {
    if let Some(node_id) = token.strip_prefix("node:") {
        let node_id = node_id.trim();
        if node_id.is_empty() {
            return None;
        }
        return Some(resolve_node_holder(node_id));
    }
    if let Some(raw) = token.strip_prefix("pr:") {
        let raw = raw.trim();
        if raw.is_empty() || !raw.chars().all(|c| c.is_ascii_digit()) {
            return Some(JobHolder {
                address: token.to_string(),
                state: "free".to_string(),
                session_id: None,
                harness: None,
                note: Some(format!("pr address must be pr:<number> (got {token:?})")),
            });
        }
        let candidates = node_ids_for_pr(raw);
        if candidates.is_empty() {
            return Some(JobHolder {
                address: token.to_string(),
                state: "free".to_string(),
                session_id: None,
                harness: None,
                note: Some(format!("no backlog node carries PR {raw}")),
            });
        }
        if candidates.len() > 1 {
            return Some(JobHolder {
                address: token.to_string(),
                state: "free".to_string(),
                session_id: None,
                harness: None,
                note: Some(format!(
                    "PR {raw} is ambiguous across nodes {}; use node:<id> to pick one",
                    candidates.join(", ")
                )),
            });
        }
        let mut holder = resolve_node_holder(&candidates[0]);
        holder.note = Some(format!("pr:{raw} -> {}", holder.address));
        return Some(holder);
    }
    None
}

/// Python `_resolve_node`: the claim status read over the claims root.
pub(super) fn resolve_node_holder(node_id: &str) -> JobHolder {
    let key = format!("node:{node_id}");
    let (state, record) = crate::claims::status(&key, None);
    let state_text = match state {
        crate::claims::ClaimState::Live => "live",
        crate::claims::ClaimState::Suspect => "suspect",
        crate::claims::ClaimState::Free => "free",
        _ => "stale",
    };
    // The record's own session identity is authoritative ("the holder string
    // is a published credential and is never parsed for identity"); the
    // holder-prefix parse is the legacy fallback for pre-change records.
    let session_id = record
        .as_ref()
        .and_then(|rec| rec.session_id.clone())
        .or_else(|| {
            record.as_ref().and_then(|rec| {
                rec.holder
                    .strip_prefix("target-session:")
                    .map(str::to_string)
            })
        });
    let harness = record.as_ref().and_then(|rec| rec.harness.clone());
    JobHolder {
        address: key,
        state: state_text.to_string(),
        session_id,
        harness,
        note: None,
    }
}

/// Python `_node_ids_for_pr`: every backlog node carrying the PR (primary or
/// additional), scanned off the graph store.
pub(super) fn node_ids_for_pr(pr: &str) -> Vec<String> {
    let pr_num: i64 = match pr.parse() {
        Ok(n) => n,
        Err(_) => return Vec::new(),
    };
    let store_path = crate::org_board::scope::graph_json_path(Path::new("."));
    let store = crate::backlog::api::Store::new(&store_path);
    let rows = match crate::backlog::api::rows(&store) {
        Ok(rows) => rows,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for row in rows.iter() {
        let primary = row.get("pr_number").and_then(Value::as_i64);
        let additional = row
            .get("additional_prs")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|p| p.get("number").and_then(Value::as_i64))
                    .any(|n| n == pr_num)
            })
            .unwrap_or(false);
        if primary == Some(pr_num) || additional {
            if let Some(id) = crate::graph_store::entry_id(row) {
                out.push(id.to_string());
            }
        }
    }
    out
}

/// Python `job_lane_send`: deliver to the WORK, resolved to whoever holds
/// the claim right now. A holder live-injects; a miss floors durable to the
/// job address; no holder refuses (exit 16) and queues nothing.
#[allow(clippy::too_many_lines)]
pub(super) fn job_lane_send(
    payload: &str,
    token: &str,
    from_name: Option<&str>,
    _style_exception: Option<&str>,
    origin: Option<&str>,
    subject: Option<&str>,
) -> Result<(), i32> {
    let Some(job) = resolve_job_address(token) else {
        eprintln!("error: not a job address: {token:?}");
        return Err(2);
    };
    if !job.has_holder() {
        let note = job
            .note
            .as_deref()
            .map(|n| format!(" ({n})"))
            .unwrap_or_default();
        eprintln!(
            "mail: {token}{note} has no live holder (claim state: {}); not queued.\n  a job address with no holder would strand. Retry when a /target session holds {}.",
            job.state, job.address
        );
        return Err(16);
    }
    let session_id = job.session_id.clone().unwrap_or_default();
    let msg_id = generate_msg_id();
    let recipient = job.address.clone();
    let sender = stamp_from(from_name);
    let reservation = reserve_send_budget(&sender, &recipient, payload, &msg_id);
    if let Err((msg, code)) = &reservation {
        eprintln!("{msg}");
        return Err(*code);
    }
    let reservation = reservation.expect("checked above");
    let sender_harness = resolve_self_harness();
    let sender_model = resolve_self_model();
    let sender_session = reply_session_for(from_name);
    let registry_path = AgentsHome::shared_registry_json();
    let envelope = |to_session: Option<&str>| {
        crate::mail_envelope::render_at(
            &json!({
                "mode": "wrap",
                "body": payload,
                "from": sender,
                "harness": sender_harness,
                "to": recipient,
                "node": job_node_of(&job.address),
                "id": msg_id,
                "from_session": sender_session,
                "origin": origin,
                "to_session": to_session,
                "subject": subject,
            }),
            &registry_path,
        )
        .unwrap_or_else(|_| payload.to_string())
    };
    let wrapped = envelope(Some(session_id.as_str()));
    let provider = job.harness.clone().unwrap_or_else(|| "claude".to_string());
    let mut reasons: Vec<String> = Vec::new();
    let injected = if provider == "codex" {
        mail_inject_codex(&session_id, &wrapped, Some(&mut reasons), None)
    } else {
        mail_inject_claude(&session_id, &wrapped, Some(&mut reasons), None)
    };
    let bus_only = !injected && reasons.iter().any(|r| r == BUS_ONLY_POLICY);
    let holder_tag = format!(
        " [holder {provider} {}]",
        session_id.chars().take(8).collect::<String>()
    );
    if injected {
        let record = bus_envelope_row(
            &msg_id,
            &msg_id,
            &sender,
            &recipient,
            "send",
            &wrapped,
            &now_iso(),
            None,
            json!({}),
            sender_harness.as_deref(),
            Some(provider.as_str()),
            sender_session.as_deref(),
            Some(&sender_model),
            Some("node"),
            Some(crate::backlog::style_check::word_count(payload) as i64),
            Some(session_id.as_str()),
            subject,
        );
        if let Err(exc) = record_hosted_delivery(&record, Some(&provider), Some(&session_id)) {
            eprintln!("delivery succeeded; outbox record failed; do not retry: {exc}");
        }
        println!(
            "{}",
            crate::mail_receipt::json_line(
                &msg_id,
                subject,
                &recipient,
                &format!("delivered (hosted){holder_tag}"),
            )
        );
        return Ok(());
    }
    let owner = "wake-daemon";
    let write = write_durable_thread(
        &recipient,
        &stamp_from(from_name),
        "send",
        &envelope(None),
        &msg_id,
        Some("node"),
        Some(&provider),
        sender_harness.as_deref(),
        sender_session.as_deref(),
        Some(&sender_model),
        owner,
        Some(crate::backlog::style_check::word_count(payload) as i64),
        origin,
        subject,
        None,
    );
    let thread_id = match write {
        Ok(_) => msg_id,
        Err(exc) => {
            release_budget(&reservation);
            eprintln!("durable envelope write failed for {recipient:?}: {exc}");
            return Err(12);
        }
    };
    if bus_only {
        eprintln!(
            "mail: holder is DND (bus-only by delivery policy); queued durable until a holder drains"
        );
        println!(
            "{}",
            crate::mail_receipt::json_line(
                &thread_id,
                subject,
                &recipient,
                &format!(
                    "queued (durable) [bus-only: a holder drains it by policy]{holder_tag}{}",
                    crate::mail_receipt::window_clause(owner_ttl_hours(owner))
                ),
            )
        );
        return Ok(());
    }
    eprintln!("mail: {recipient} live-inject missed; durable until a holder drains");
    let suffix = live_miss_age_suffix(&recipient);
    println!(
        "{}",
        crate::mail_receipt::json_line(
            &thread_id,
            subject,
            &recipient,
            &format!(
                "queued (durable) [job-live-miss{suffix}]{holder_tag}{}",
                crate::mail_receipt::window_clause(owner_ttl_hours(owner))
            ),
        )
    );
    Ok(())
}

pub(super) fn job_node_of(address: &str) -> String {
    address.strip_prefix("node:").unwrap_or("").to_string()
}

/// The --force name lane: resolve through discovery, then force the pane.
pub(super) fn run_name_lane_force(
    args: &SendArgs,
    body: &str,
    token: &str,
    from_name: Option<&str>,
    origin: Option<&str>,
    subject: Option<&str>,
) -> i32 {
    let reach = resolve_reachable(token);
    let suggestions: Vec<String> = Vec::new();
    let lane_args = NameLaneArgs {
        message: body,
        from_name,
        resolved: reach.hit.clone(),
        recipient: None,
        provider: args.harness.as_deref(),
        reply_to: args.reply_to.as_deref(),
        token: if reach.hit.is_none() {
            Some(token)
        } else {
            None
        },
        style_exception: args.style_exception.as_deref(),
        force: true,
        origin,
        subject,
    };
    match name_lane_send(lane_args) {
        Ok(NameLane::Done) => 0,
        Ok(NameLane::UnreachableToken) => {
            unreachable_token_exit(token, &suggestions, UNKNOWN_AGENT_EXIT)
        }
        Ok(NameLane::AmbiguousToken(candidates)) => ambiguous_token_exit(token, &candidates),
        Ok(NameLane::UnavailableToken { failed, candidates }) => {
            unavailable_token_exit(token, &failed, &candidates)
        }
        Err(code) => code,
    }
}

/// The registered-name lane: dispatch_send first; its unknown-name refusal
/// falls into the shared name-lane choke point carrying the raw token, where
/// exit 16 lives at the BOTTOM of the ladder.
pub(super) fn run_dispatch_then_name_lane(
    args: &SendArgs,
    body: &str,
    token: &str,
    from_name: Option<&str>,
    origin: Option<&str>,
    subject: Option<&str>,
) -> i32 {
    let sender = stamp_from(from_name);
    let dispatched = dispatch_send(
        token,
        body,
        args.harness.as_deref(),
        DEFAULT_LOCK_TIMEOUT,
        &sender,
        origin,
        subject,
    );
    match dispatched {
        Ok(result) => {
            if result.delivery == "hosted" {
                println!(
                    "{}",
                    crate::mail_receipt::json_line(
                        &result.msg_id,
                        subject,
                        token,
                        "delivered (hosted)",
                    )
                );
                return 0;
            }
            if result.reason.as_deref() == Some(BUS_ONLY_POLICY) {
                let note = bounce_reason(token);
                println!(
                    "{}",
                    crate::mail_receipt::json_line(
                        &result.msg_id,
                        subject,
                        token,
                        &format!(
                            "queued (durable) [{}]",
                            note.clone().unwrap_or_else(|| {
                                "DND (bus-only): recipient polls the bus at each turn boundary"
                                    .to_string()
                            })
                        ),
                    )
                );
                if note.is_some() {
                    println!("`fno agents mail withdraw {}` retracts it.", result.msg_id);
                }
                return 0;
            }
            warn_deferred(token, false, result.reason.as_deref());
            let age_target = token;
            let suffix = live_miss_age_suffix(age_target);
            let ttl = owner_ttl_hours(result.durable_owner.as_deref().unwrap_or("wake-daemon"));
            let mut door: Vec<String> = vec![
                "demotion".to_string(),
                "--msg-id".to_string(),
                result.msg_id.clone(),
            ];
            if let Some(reason) = &result.reason {
                door.push("--reason".to_string());
                door.push(reason.clone());
            }
            door.push("--ttl-hours".to_string());
            door.push(format!("{ttl:?}"));
            door.push("--target".to_string());
            door.push(token.to_string());
            door.push("--age-of".to_string());
            door.push(age_target.to_string());
            door.push("--suffix".to_string());
            door.push(suffix);
            if let Some(sub) = subject {
                door.push("--subject".to_string());
                door.push(sub.to_string());
            }
            let refs: Vec<&str> = door.iter().map(String::as_str).collect();
            match render_receipt(&refs) {
                Ok(text) => println!("{text}"),
                Err(exc) => println!("{exc}"),
            }
            if self_recipient(token, None, false).is_none()
                && !report_landing(
                    &result.msg_id,
                    token,
                    result.to.as_deref(),
                    result.to_harness.as_deref(),
                    result.to_session.as_deref(),
                )
            {
                return NOT_LANDED_EXIT;
            }
            0
        }
        Err((msg, code)) => {
            if code != UNKNOWN_AGENT_EXIT {
                eprintln!("{msg}");
                return code;
            }
            // Discovery is a liveness-gated LISTING, so unknown means "not
            // listed", not "not reachable": fall into the name lane.
            let reach = resolve_reachable(token);
            let suggestions: Vec<String> = Vec::new();
            let lane_args = NameLaneArgs {
                message: body,
                from_name,
                resolved: reach.hit.clone(),
                recipient: None,
                provider: args.harness.as_deref(),
                reply_to: args.reply_to.as_deref(),
                token: if reach.hit.is_none() {
                    Some(token)
                } else {
                    None
                },
                style_exception: args.style_exception.as_deref(),
                force: false,
                origin,
                subject,
            };
            match name_lane_send(lane_args) {
                Ok(NameLane::Done) => 0,
                Ok(NameLane::UnreachableToken) => unreachable_token_exit(token, &suggestions, code),
                Ok(NameLane::AmbiguousToken(candidates)) => {
                    ambiguous_token_exit(token, &candidates)
                }
                Ok(NameLane::UnavailableToken { failed, candidates }) => {
                    unavailable_token_exit(token, &failed, &candidates)
                }
                Err(lane_code) => lane_code,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The --force pane lane (cli.py _forced_pane_send twin)
// ---------------------------------------------------------------------------

/// The force arm's bundle, threaded from the name lane in one struct.
struct ForcedPane<'a> {
    wrapped: &'a str,
    entry: &'a Value,
    recipient: &'a str,
    sender: &'a str,
    msg_id: &'a str,
    reply_to: Option<&'a str>,
    provider: Option<&'a str>,
    sender_harness: Option<&'a str>,
    sender_session: Option<&'a str>,
    sender_model: &'a str,
    authored_words: usize,
    reservation: Option<BudgetReservation>,
    subject: Option<&'a str>,
}

/// Python `bus.log.record_typed_delivery`: the audit row naming the pane the
/// bytes were typed into.
fn record_typed_delivery(
    record: &Value,
    pane_id: &str,
    mux_session: Option<&str>,
) -> Result<(), String> {
    let mut row = record.clone();
    let obj = row.as_object_mut().ok_or("record not an object")?;
    obj.insert("delivery".to_string(), json!("typed"));
    if let Some(obj) = row.as_object_mut() {
        let mut meta = serde_json::Map::new();
        meta.insert("pane_id".to_string(), json!(pane_id));
        if let Some(session) = mux_session {
            meta.insert("mux_session".to_string(), json!(session));
        }
        obj.insert("meta".to_string(), Value::Object(meta));
    }
    bus_append_row(&row)
}

/// Python `_forced_pane_send`: --force changes only the TRANSPORT; every
/// mail semantic is kept. The receipt says `typed`, never `delivered`.
/// Simplification documented for this port: a thread-substrate row (no
/// mux/pane ref) refuses here rather than resolving a retask viewport.
#[allow(clippy::too_many_lines)]
fn forced_pane_send(pane: ForcedPane) -> Result<NameLane, i32> {
    let ForcedPane {
        wrapped,
        entry,
        recipient,
        sender,
        msg_id,
        reply_to,
        provider,
        sender_harness,
        sender_session,
        sender_model,
        authored_words,
        reservation,
        subject,
    } = pane;
    let provider = opt_str_field(entry, "harness")
        .or_else(|| provider.map(str::to_string))
        .unwrap_or_else(|| "claude".to_string());
    let mux_session = entry
        .pointer("/mux/session")
        .and_then(Value::as_str)
        .map(str::to_string);
    let pane_id = entry.pointer("/mux/pane_id").and_then(Value::as_i64);
    // Bus-only FIRST: a policy on the row, not an outcome of trying.
    if crate::mail_hold::gate(recipient, Some(wrapped), chrono::Utc::now()).deliver {
        // deliverable: fall through
    } else {
        release_budget(&reservation);
        eprintln!(
            "error: {} is DND (bus-only by policy), so no pane paste is allowed and --force has nothing it may do. Send without --force: that queues durable and the recipient drains it.",
            py_repr(recipient)
        );
        return Err(1);
    }
    if crate::announce::row_terminal(entry) {
        let status = str_field(entry, "status");
        release_budget(&reservation);
        eprintln!(
            "error: --force types at a live prompt and {} is {status}. Its pane id may already belong to another session. Send without --force to reach the durable bus.",
            py_repr(recipient)
        );
        return Err(1);
    }
    let Some(pane_num) = pane_id else {
        release_budget(&reservation);
        eprintln!(
            "error: --force types into a mux pane and {} has none. Send without --force (it queues durable), or spawn the recipient with --substrate pane.",
            py_repr(recipient)
        );
        return Err(1);
    };
    let record = bus_envelope_row(
        msg_id,
        msg_id,
        sender,
        recipient,
        "send",
        wrapped,
        &now_iso(),
        reply_to,
        json!({}),
        sender_harness,
        Some(provider.as_str()),
        sender_session,
        Some(sender_model),
        Some("name"),
        Some(authored_words as i64),
        None,
        subject,
    );
    if let Err(exc) = stage_before_live(
        msg_id,
        sender,
        recipient,
        wrapped,
        sender_harness,
        Some(&provider),
        reply_to,
        sender_session,
        Some(sender_model),
        Some("name"),
        Some(authored_words as i64),
        subject,
    ) {
        release_budget(&reservation);
        eprintln!(
            "error: body row not staged before the forced send; nothing was typed. Send without --force to queue durable. ({exc})"
        );
        return Err(1);
    }
    if !mux_pane_send(entry, wrapped) {
        release_budget(&reservation);
        eprintln!(
            "error: --force did not confirm a send into pane {pane_num}. The payload may still be sitting unsent in the recipient's composer, and the staged body row will deliver by drain, so check the pane before retrying. The refusal is on stderr above."
        );
        return Err(1);
    }
    if let Err(exc) = record_typed_delivery(&record, &pane_num.to_string(), mux_session.as_deref())
    {
        eprintln!(
            "typed; receipt row failed; the staged bus row holds the body; do not retry: {exc}"
        );
    }
    let corr = pane
        .reply_to
        .map(|r| format!(" re:{r}"))
        .unwrap_or_default();
    println!(
        "{}",
        crate::mail_receipt::json_line(
            msg_id,
            subject,
            recipient,
            &format!("typed (pane {pane_num})"),
        )
    );
    if !corr.is_empty() {
        println!("{}", corr.trim());
    }
    Ok(NameLane::Done)
}
