use super::resolve::{
    graph_session_rows_for, ledger_for_session, node_for_session, node_id_of, parse_pr,
    receipts_for, resolve_registry, row_is_live,
};
use super::{eq, push_string_unique, short_id, str_at, Receipt, Sources, TranscriptFacts};
use crate::agents_view;
use serde_json::Value;

pub(super) fn card(
    sid: &str,
    sources: &Sources,
    transcript: Option<&TranscriptFacts>,
) -> Vec<String> {
    let registry = resolve_registry(sources, sid);
    let live_registry = registry.is_some_and(row_is_live);
    let receipts = receipts_for(sources, sid);
    let newest_receipt = receipts.first().copied();
    let mut lines = vec![format!("session:    {sid}")];

    let spawned = spawned_name(sources, sid);
    let name = registry
        .and_then(|row| str_at(row, "name"))
        .or_else(|| newest_receipt.and_then(|r| str_at(&r.value, "row_name")))
        .or_else(|| transcript.and_then(|facts| facts.agent_name.as_deref()));
    let name_line = match name {
        Some(name) => match spawned.as_deref().filter(|spawned| !eq(spawned, name)) {
            Some(spawned) => format!("name:       {name} (spawned as {spawned})"),
            None => format!("name:       {name}"),
        },
        None => format!(
            "name:       unknown ({})",
            unknown_reason(registry.is_some(), newest_receipt.is_some(), "name")
        ),
    };
    lines.push(name_line);

    if let Some((node, _)) = node_for_session(sid, sources) {
        let title = str_at(node, "title").unwrap_or_default();
        let id = node_id_of(node).unwrap_or_default();
        lines.push(format!("node:       {id}  {title} (live graph row)"));
    } else if ledger_for_session(sid, sources)
        .iter()
        .any(|row| row["node_id_unrecoverable"].as_bool() == Some(true))
    {
        lines.push("node:       not recorded (this row says node_id_unrecoverable)".into());
    } else {
        lines.push(format!(
            "node:       unknown ({})",
            source_reason(&sources.graph, "no graph row names session")
        ));
    }

    let harness = registry
        .and_then(|row| str_at(row, "harness"))
        .or_else(|| spawned_harness(sources, sid));
    lines.push(match harness {
        Some(harness) => format!(
            "harness:    {harness} ({})",
            if registry.and_then(|r| str_at(r, "harness")).is_some() {
                "registry row"
            } else {
                "agent_spawned event"
            }
        ),
        None => format!(
            "harness:    unknown ({})",
            source_reason(
                &sources.registry,
                "no registry row or spawn event records a harness"
            )
        ),
    });

    lines.push(match registry.and_then(|row| str_at(row, "provider")) {
        Some(provider) => format!("provider:   {provider} (registry row)"),
        None if registry.is_some() => {
            "provider:   unknown (the registry row records no provider)".into()
        }
        None => format!(
            "provider:   unknown ({})",
            source_reason(&sources.registry, "no registry row")
        ),
    });
    lines.push(
        match registry.and_then(|row| str_at(row, "route_settings_path")) {
            Some(path) => format!("route:      {path} (registry row)"),
            None if registry.is_some() => {
                "route:      unknown (the registry row records no route_settings_path)".into()
            }
            None => format!(
                "route:      unknown ({})",
                source_reason(&sources.registry, "no registry row")
            ),
        },
    );

    let runs = transcript
        .map(|facts| facts.runs.as_slice())
        .unwrap_or_default();
    let (model, model_basis) = registry
        .and_then(|row| {
            str_at(row, "requested_model").map(|value| (value, "registry requested_model"))
        })
        .or_else(|| {
            registry.and_then(|row| str_at(row, "model").map(|value| (value, "registry model")))
        })
        .or_else(|| {
            runs.first()
                .map(|run| (run.model.as_str(), "first transcript turn"))
        })
        .unwrap_or(("unknown", "no registry request or transcript turn"));
    let observed = runs.last();
    let observed_text = observed.map_or_else(
        || "unknown (no transcript turn)".to_string(),
        |run| {
            format!(
                "{} (last turn {})",
                run.model,
                run.last_ts.as_deref().unwrap_or("time unknown")
            )
        },
    );
    lines.push(format!(
        "model:      spawn {model} ({model_basis}); observed {observed_text}"
    ));

    for pair in runs.windows(2) {
        if pair[0].model != pair[1].model {
            let from = pair[0].last_ts.as_deref().unwrap_or("time unknown");
            let to = pair[1].first_ts.as_deref().unwrap_or("time unknown");
            let between = event_rows(sources, sid)
                .into_iter()
                .filter(|event| {
                    event
                        .timestamp
                        .as_deref()
                        .is_some_and(|ts| ts > from && ts < to)
                })
                .map(|event| format!("{} {}", event.kind, event.timestamp.unwrap_or_default()))
                .collect::<Vec<_>>();
            let middle = if between.is_empty() {
                "none recorded".into()
            } else {
                between.join(", ")
            };
            lines.push(format!(
                "switch:     {} -> {}: last {} turn {from}, first {} turn {to}; between: {middle}",
                pair[0].model, pair[1].model, pair[0].model, pair[1].model
            ));
        }
    }
    if let Some(first) = runs.first() {
        if model != "unknown" && normalize_model(model) != normalize_model(&first.model) {
            lines.push(format!(
                "switch:     spawned asking for {model}, first turn answered as {} at {}",
                first.model,
                first.first_ts.as_deref().unwrap_or("time unknown")
            ));
        }
    }
    if runs.len() == 1 {
        lines.push(format!(
            "switch:     none ({} turns on one model)",
            runs[0].turns
        ));
    } else if runs.is_empty() {
        lines.push("switch:     unknown (no transcript turn records model history)".into());
    }

    let mut efforts = Vec::new();
    if let Some(row) = registry {
        for (key, basis) in [
            ("requested_effort", "registry requested_effort"),
            ("effort", "registry effort"),
        ] {
            if let Some(value) = str_at(row, key) {
                push_string_unique(&mut efforts, format!("{value} ({basis})"));
            }
        }
    }
    for row in graph_session_rows_for(sid, sources) {
        if let Some(value) = str_at(row, "effort") {
            push_string_unique(&mut efforts, format!("{value} (node session row)"));
        }
    }
    if efforts.is_empty() {
        lines.push(format!(
            "effort:     unknown ({})",
            source_reason(&sources.graph, "no effort recorded")
        ));
    } else {
        lines.extend(
            efforts
                .into_iter()
                .map(|effort| format!("effort:     {effort}")),
        );
    }

    let permission = transcript
        .and_then(
            |facts| match (facts.permissions.first(), facts.permissions.last()) {
                (Some(first), Some(last)) if first == last => Some(format!("{first} (transcript)")),
                (Some(first), Some(last)) => Some(format!("{first} -> {last} (transcript)")),
                _ => None,
            },
        )
        .or_else(|| {
            registry.and_then(|row| {
                str_at(row, "requested_permission_mode").map(|v| format!("{v} (registry row)"))
            })
        });
    lines.push(format!(
        "permission: {}",
        permission.unwrap_or_else(|| format!(
            "unknown ({})",
            source_reason(
                &sources.registry,
                "no transcript or registry permission mode"
            )
        ))
    ));

    for row in ordered_stage_rows(sid, sources) {
        lines.push(stage_line(row));
    }

    if let Some(row) = registry {
        let status = str_at(row, "status").unwrap_or("unknown");
        let exited = str_at(row, "exited_at")
            .map(|at| format!(" [exited {at}]"))
            .unwrap_or_default();
        lines.push(format!("status:     {status}{exited} (registry row)"));
    } else if let Some(receipt) = newest_receipt {
        lines.push(format!(
            "status:     reaped {} (receipt)",
            str_at(&receipt.value, "reaped_at").unwrap_or("time unknown")
        ));
    } else {
        lines.push(format!(
            "status:     unknown ({})",
            source_reason(&sources.registry, "no registry row and no receipt")
        ));
    }

    for event in event_rows(sources, sid) {
        if event.kind != "send" {
            let timestamp = event.timestamp.as_deref().unwrap_or("time unknown");
            let detail = if event.detail.is_empty() {
                String::new()
            } else {
                format!(" {}", event.detail)
            };
            lines.push(format!("event:      {timestamp} {}{detail}", event.kind));
        }
    }

    if receipts.is_empty() && live_registry {
        lines.push("receipt:    not recorded (row is live; reap receipt suppressed)".into());
    } else if receipts.is_empty() {
        let reason = match &sources.receipts {
            Ok(_) => "receipt matched no harness_session_id, short_id, row_name".to_string(),
            Err(err) => format!("receipt source unavailable: {err}"),
        };
        lines.push(format!("receipt:    not recorded ({reason})"));
    } else {
        for receipt in &receipts {
            lines.push(format!(
                "receipt:    {}, reaped {}",
                receipt.path.display(),
                str_at(&receipt.value, "reaped_at").unwrap_or("time unknown")
            ));
        }
    }

    for entry in ledger_for_session(sid, sources) {
        lines.push(ledger_line(entry));
    }
    if ledger_for_session(sid, sources).is_empty() {
        lines.push(format!(
            "ledger:     not recorded ({})",
            source_reason(&sources.ledger, "no ledger row names session")
        ));
    }

    lines.push(match transcript.and_then(|facts| facts.path.as_deref()) {
        Some(path) => format!("transcript: {} (this machine)", path.display()),
        None => match registry.and_then(|row| str_at(row, "transcript_path")) {
            Some(path) => {
                format!("transcript: {path} (registry row; not found on this machine)")
            }
            None => "transcript: not found on this machine (claude projects and codex sessions searched)"
                .into(),
        },
    });
    lines.push(
        match transcript
            .and_then(|facts| facts.path.as_deref())
            .and_then(|path| crate::session_origin::read_beside(path, sid))
        {
            Some(origin) => format!(
                "origin:     {}",
                origin.origin_text(&crate::session_origin::this_machine())
            ),
            None => "origin:     not recorded (no origin file beside the transcript; sessions started before this change, resumed sessions with a new id, and harnesses that report no transcript have none)"
                .into(),
        },
    );
    lines.push(format!(
        "resume:     {}",
        resume_line(sid, registry, newest_receipt, harness, &model)
    ));
    lines
}

struct DisplayEvent {
    timestamp: Option<String>,
    kind: String,
    detail: String,
}

fn event_rows(sources: &Sources, sid: &str) -> Vec<DisplayEvent> {
    let mut events = Vec::new();
    if let Some(rows) = sources.events.as_ref().ok() {
        for row in rows
            .iter()
            .filter(|row| event_sid(row).is_some_and(|value| eq(value, sid)))
        {
            let kind = match str_at(row, "type").unwrap_or_default() {
                "agent_spawned" => "spawn",
                "agent_resumed" => "resume",
                "agent_resume_failed" => "resume failed",
                "agent_row_reaped" => "reap",
                "agent_removed" => "removed",
                "agent_send_started" => "send",
                _ => continue,
            };
            let data = row.get("data").unwrap_or(row);
            let detail = if kind == "spawn" {
                let name = str_at(data, "name").unwrap_or_default();
                let substrate = str_at(data, "substrate")
                    .map(|value| format!(" (substrate {value})"))
                    .unwrap_or_default();
                let spawned_by = str_at(data, "spawned_by")
                    .map(short_id)
                    .map(|value| format!("; spawned by {value}"))
                    .unwrap_or_default();
                format!("{name}{substrate}{spawned_by}")
            } else if kind == "send" {
                str_at(data, "verb").unwrap_or_default().to_string()
            } else {
                String::new()
            };
            events.push(DisplayEvent {
                timestamp: event_time(row).map(str::to_string),
                kind: kind.to_string(),
                detail,
            });
        }
    }
    if let Some(row) =
        resolve_registry(sources, sid).filter(|row| str_at(row, "origin") == Some("adopted"))
    {
        if let Some(timestamp) = str_at(row, "created_at") {
            let detail = str_at(row, "adopted_by_session")
                .map(short_id)
                .map(|value| format!("adopted by {value}"))
                .unwrap_or_default();
            events.push(DisplayEvent {
                timestamp: Some(timestamp.to_string()),
                kind: "adopt".into(),
                detail,
            });
        }
    }
    for row in graph_session_rows_for(sid, sources) {
        if let (Some(end), Some(by)) = (str_at(row, "ended_at"), str_at(row, "ended_by")) {
            let kind = if by.to_ascii_lowercase().contains("reap") {
                "reap"
            } else {
                "removed"
            };
            events.push(DisplayEvent {
                timestamp: Some(end.to_string()),
                kind: kind.into(),
                detail: by.to_string(),
            });
        }
    }
    events.sort_by(|a, b| {
        a.timestamp
            .cmp(&b.timestamp)
            .then_with(|| a.kind.cmp(&b.kind))
    });
    events.dedup_by(|a, b| a.timestamp == b.timestamp && a.kind == b.kind);
    events
}

pub(super) fn event_sid(event: &Value) -> Option<&str> {
    str_at(event, "session_id")
        .or_else(|| str_at(event, "harness_session_id"))
        .or_else(|| str_at(&event["data"], "session_id"))
        .or_else(|| str_at(&event["data"], "harness_session_id"))
}

pub(super) fn event_time(event: &Value) -> Option<&str> {
    ["timestamp", "ts", "created_at"]
        .iter()
        .find_map(|key| str_at(event, key))
}

fn spawned_harness<'a>(sources: &'a Sources, sid: &str) -> Option<&'a str> {
    sources.events.as_ref().ok()?.iter().find_map(|event| {
        (str_at(event, "type") == Some("agent_spawned")
            && event_sid(event).is_some_and(|value| eq(value, sid)))
        .then(|| str_at(event.get("data").unwrap_or(event), "provider"))
        .flatten()
    })
}

fn spawned_name<'a>(sources: &'a Sources, sid: &str) -> Option<&'a str> {
    sources.events.as_ref().ok()?.iter().find_map(|event| {
        (str_at(event, "type") == Some("agent_spawned")
            && event_sid(event).is_some_and(|value| eq(value, sid)))
        .then(|| str_at(event.get("data").unwrap_or(event), "name"))
        .flatten()
    })
}
fn ordered_stage_rows<'a>(sid: &str, sources: &'a Sources) -> Vec<&'a Value> {
    let mut rows = graph_session_rows_for(sid, sources);
    rows.sort_by_key(|row| phase_order(str_at(row, "phase").unwrap_or_default()));
    rows
}

fn phase_order(phase: &str) -> usize {
    match phase {
        "think" => 0,
        "blueprint" => 1,
        "do" => 2,
        "review" => 3,
        "ship" => 4,
        _ => 5,
    }
}

fn stage_line(row: &Value) -> String {
    let phase = str_at(row, "phase").unwrap_or("unknown");
    let start = str_at(row, "started_at")
        .or_else(|| str_at(row, "claimed_at"))
        .unwrap_or("start unknown");
    let end = str_at(row, "ended_at").unwrap_or("open");
    let ended_by = str_at(row, "ended_by")
        .map(|value| format!(" [ended by {value}]"))
        .unwrap_or_default();
    let model = str_at(row, "observed_model")
        .map(|value| format!(" {value}"))
        .unwrap_or_default();
    let effort = str_at(row, "effort")
        .map(|value| format!(" effort {value}"))
        .unwrap_or_default();
    let grant = match row.get("merge_grant") {
        Some(grant) if grant["approved"].as_bool().is_some() => {
            let status = if grant["approved"].as_bool() == Some(true) {
                "approved"
            } else {
                "denied"
            };
            let source = str_at(grant, "source")
                .map(|value| format!(" ({value})"))
                .unwrap_or_default();
            format!(" merge grant {status}{source}")
        }
        Some(grant) if grant.as_str().is_some() => {
            format!(" merge grant {}", grant.as_str().unwrap_or_default())
        }
        _ => String::new(),
    };
    format!("stage:      {phase} {start} -> {end}{ended_by}{model}{effort}{grant}")
}
pub(super) fn ledger_line(entry: &Value) -> String {
    let node = str_at(entry, "graph_node_id")
        .or_else(|| str_at(entry, "node"))
        .unwrap_or("unknown");
    let pr = entry
        .get("pr_number")
        .map(|value| value.to_string())
        .unwrap_or_else(|| {
            str_at(entry, "pr_url")
                .and_then(parse_pr)
                .map(|number| number.to_string())
                .unwrap_or_else(|| "unknown".into())
        });
    let status = str_at(entry, "status").unwrap_or("unknown");
    let completed = str_at(entry, "completed").or_else(|| str_at(entry, "completed_at"));
    let mut line = format!("ledger:     {node} #{pr} {status}");
    if let Some(completed) = completed {
        line.push_str(&format!(" ({completed})"));
    }
    for (key, label) in [("plan_path", "plan"), ("root_path", "worktree")] {
        if let Some(value) = str_at(entry, key) {
            line.push_str(&format!(" [{label} {value}]"));
        }
    }
    line
}

fn resume_line(
    sid: &str,
    registry: Option<&Value>,
    receipt: Option<&Receipt>,
    harness: Option<&str>,
    spawn_model: &str,
) -> String {
    if let Some(resume) = receipt.and_then(|r| str_at(&r.value, "resume")) {
        return resume.to_string();
    }
    let provider = registry.and_then(|row| str_at(row, "provider"));
    if provider.is_none() {
        return "unknown (the provider is not recorded, so a plain resume could run the account default)".into();
    }
    if let Some(provider) = provider.filter(|provider| !provider.eq_ignore_ascii_case("anthropic"))
    {
        if spawn_model != "unknown" {
            return format!(
                "fno agents spawn --resume {sid} -P {provider} -m {}",
                shell_single_quote(spawn_model)
            );
        }
        return "unknown (the registry records a provider but no spawn model)".into();
    }
    if provider.is_some_and(|value| value.eq_ignore_ascii_case("anthropic")) {
        if let Some(harness) = harness {
            if let Some(form) = agents_view::resume_form(harness) {
                let short = registry
                    .and_then(|row| str_at(row, "short_id"))
                    .unwrap_or(sid);
                return form
                    .render(if form.id_kind == agents_view::IdKind::Short {
                        short
                    } else {
                        sid
                    })
                    .join(" ");
            }
            return format!("unknown (the {harness} harness declares no resume form)");
        }
    }
    "unknown (the harness has no declared resume form)".into()
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn normalize_model(model: &str) -> &str {
    model
        .strip_suffix(']')
        .and_then(|tagged| tagged.rsplit_once('[').map(|(base, _)| base))
        .unwrap_or(model)
}

fn source_reason<T>(source: &Result<T, String>, fallback: &str) -> String {
    match source {
        Ok(_) => fallback.into(),
        Err(error) => format!("source unavailable: {error}"),
    }
}

fn unknown_reason(has_registry: bool, has_receipt: bool, field: &str) -> String {
    match (has_registry, has_receipt) {
        (false, false) => format!("no registry row or receipt records {field}"),
        (false, true) => format!("receipt does not record {field}"),
        (true, false) => format!("registry row does not record {field}"),
        (true, true) => format!("registry row and receipt do not record {field}"),
    }
}
