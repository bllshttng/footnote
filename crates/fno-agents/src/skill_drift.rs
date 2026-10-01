//! Reads the skill bodies the session still carries from before its last
//! compaction and names the ones whose file no longer matches. A step the
//! session remembers from a stale body may already be retired on disk.

use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use serde_json::{json, Value};

const SKILL_PREFIX: &str = "Base directory for this skill: ";
const TRUNCATED: &str = "\n\n[... skill content truncated for compaction";

pub(crate) fn reading() -> Result<Value, String> {
    let path = crate::lead_checkin::own_claude_transcript()?;
    fold(&path)
}

fn fold(path: &Path) -> Result<Value, String> {
    // ponytail: reads the whole transcript each beat, as repeated_asks does; seek to the newest invoked_skills row if the check-in's reader time matters.
    let file = fs::File::open(path).map_err(|e| format!("transcript unreadable: {e}"))?;
    let mut carried: Vec<(String, String)> = Vec::new();
    let mut reinvoked: Vec<String> = Vec::new();
    let mut compacted_at: Option<String> = None;
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|e| format!("transcript unreadable: {e}"))?;
        if line.contains("invoked_skills") {
            let Ok(row) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let skills = row
                .get("attachment")
                .filter(|a| a.get("type").and_then(Value::as_str) == Some("invoked_skills"))
                .and_then(|a| a.get("skills"))
                .and_then(Value::as_array);
            let Some(skills) = skills else {
                continue;
            };
            carried = skills
                .iter()
                .filter_map(|s| {
                    let name = s.get("name")?.as_str()?.to_string();
                    let content = s.get("content")?.as_str()?.to_string();
                    Some((name, content))
                })
                .collect();
            compacted_at = row
                .get("timestamp")
                .and_then(Value::as_str)
                .map(str::to_string);
            reinvoked.clear();
            continue;
        }
        if !line.contains("\"tool_use\"") {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if row.get("type").and_then(Value::as_str) != Some("assistant")
            || row.get("isSidechain").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let empty: Vec<Value> = Vec::new();
        let blocks = row
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("tool_use")
                && block.get("name").and_then(Value::as_str) == Some("Skill")
            {
                if let Some(skill) = block
                    .get("input")
                    .and_then(|i| i.get("skill"))
                    .and_then(Value::as_str)
                {
                    reinvoked.push(skill.to_string());
                }
            }
        }
    }
    let mut checked = 0usize;
    let mut stale: Vec<Value> = Vec::new();
    for (name, content) in &carried {
        if reinvoked.iter().any(|s| s == name) {
            continue;
        }
        let Some(rest) = content.strip_prefix(SKILL_PREFIX) else {
            continue;
        };
        let Some((dir_line, text)) = rest.split_once("\n\n") else {
            continue;
        };
        let file = PathBuf::from(dir_line).join("SKILL.md");
        checked += 1;
        let Some(reason) = drift(&file, text) else {
            continue;
        };
        stale.push(json!({
            "name": name,
            "file": file.display().to_string(),
            "reason": reason,
        }));
    }
    Ok(json!({
        "compacted_at": compacted_at,
        "carried": checked,
        "stale": stale,
    }))
}

/// None when the file's current body still matches the text the session
/// carries; otherwise the reason it reads stale. A truncated carried body
/// only holds a head, so it is fresh when the body starts with that head; a
/// whole carried body is fresh when it starts with the body, since Claude
/// may append an arguments line. A missing file reads stale, never fresh.
fn drift(file: &Path, carried_text: &str) -> Option<String> {
    let Ok(raw) = fs::read_to_string(file) else {
        return Some("file gone".into());
    };
    let body = body_of(&raw);
    let head = match carried_text.find(TRUNCATED) {
        Some(cut) => carried_text[..cut].trim(),
        None => carried_text.trim(),
    };
    if body.starts_with(head) {
        None
    } else {
        Some("text drift".into())
    }
}

/// The SKILL.md body: everything after the leading `---` frontmatter block,
/// trimmed. A file with no frontmatter is all body.
fn body_of(raw: &str) -> &str {
    let Some(rest) = raw.strip_prefix("---\n") else {
        return raw;
    };
    let Some(cut) = rest.find("\n---") else {
        return raw;
    };
    let after = &rest[cut + 4..];
    let after = after.strip_prefix('\n').unwrap_or(after);
    after.trim()
}

pub(crate) fn lines(readings: &[crate::lead_checkin::Reading]) -> Vec<String> {
    let Some(reading) = readings.iter().find(|r| r.name == "skill_drift") else {
        return Vec::new();
    };
    if !reading.ok {
        return vec![format!("READER FAILED skill_drift: {}", reading.error)];
    }
    let Some(ts) = reading.value.get("compacted_at").and_then(Value::as_str) else {
        return vec!["skill drift: none (no compaction in this session)".into()];
    };
    let stale = reading
        .value
        .get("stale")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if stale.is_empty() {
        let carried = reading
            .value
            .get("carried")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        return vec![format!(
            "skill drift: none ({carried} skill bodies carried since {ts} match their files)"
        )];
    }
    let mut lines = Vec::new();
    for entry in stale {
        let name = entry.get("name").and_then(Value::as_str).unwrap_or("");
        let file = entry.get("file").and_then(Value::as_str).unwrap_or("");
        lines.push(format!(
            "skill drift: {name} text carried since the {ts} compaction no longer matches {file}"
        ));
        lines.push(format!(
            "  remedy: run the Skill tool with {name} again before you act on its steps; a step you remember from it may be retired."
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lead_checkin::Reading;

    #[test]
    fn fold_names_the_stale_carried_body_and_lines_render_the_remedy() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join("skills");
        std::fs::create_dir_all(skills.join("review")).unwrap();
        std::fs::create_dir_all(skills.join("lead")).unwrap();
        std::fs::write(
            skills.join("review").join("SKILL.md"),
            "---\nname: review\ndescription: \"Retired spelling of review.\"\n---\n`review` is the retired spelling of `review`, kept for one release. Run the Skill tool with `fno:review` and the same arguments now.\n",
        )
        .unwrap();
        let lead_body =
            "Team mail delivers live between beats. Read the team doc before you act.";
        std::fs::write(
            skills.join("lead").join("SKILL.md"),
            format!("---\nname: lead\ndescription: \"Beats.\"\n---\n{lead_body}\n"),
        )
        .unwrap();

        // A stale carried body: the head names steps the on-disk stub body
        // no longer carries.
        let review_carried = format!(
            "Base directory for this skill: {}\n\nTeam mail delivers live between beats.\n\nLift the mail hold first, every beat.\n\n[... skill content truncated for compaction; use Read on the skill path if you need the full text]",
            skills.join("review").display()
        );
        let lead_head = &lead_body[..lead_body.find(" Read").unwrap()];
        // A fresh carried body: the truncated head still matches the file.
        let lead_carried = format!(
            "Base directory for this skill: {}\n\n{lead_head}\n\n[... skill content truncated for compaction; use Read on the skill path if you need the full text]",
            skills.join("lead").display()
        );
        let rows = [
            json!({"type":"user", "timestamp":"2026-09-30T00:30:00Z",
            "attachment":{"type":"invoked_skills","skills":[
                {"name":"fno:old","content":format!("Base directory for this skill: {}\n\nOld text.", skills.join("gone").display())}
            ]}}),
            json!({"type":"user", "timestamp":"2026-09-30T01:35:00Z",
            "attachment":{"type":"invoked_skills","skills":[
                {"name":"bundled:loop","content":"# /loop - schedule a recurring or self-paced prompt"},
                {"name":"fno:review","content":review_carried},
                {"name":"fno:lead","content":lead_carried},
                {"name":"fno:other","content":format!("Base directory for this skill: {}\n\nOld body.", skills.join("other").display())}
            ]}}),
            json!({"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[
                {"type":"tool_use","name":"Skill","input":{"skill":"fno:other"}}
            ]}}),
        ];
        let transcript = dir.path().join("t.jsonl");
        let body = rows
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&transcript, body).unwrap();

        let folded = fold(&transcript).unwrap();
        assert_eq!(folded["compacted_at"], json!("2026-09-30T01:35:00Z"));
        assert_eq!(folded["carried"], json!(2));
        assert_eq!(
            folded["stale"],
            json!([{
                "name": "fno:review",
                "file": skills.join("review").join("SKILL.md").display().to_string(),
                "reason": "text drift",
            }])
        );

        let rendered = lines(&[Reading::took("skill_drift", folded)]);
        assert_eq!(
            rendered,
            vec![
                format!(
                    "skill drift: fno:review text carried since the 2026-09-30T01:35:00Z compaction no longer matches {}",
                    skills.join("review").join("SKILL.md").display()
                ),
                "  remedy: run the Skill tool with fno:review again before you act on its steps; a step you remember from it may be retired.".to_string(),
            ]
        );

        assert_eq!(
            lines(&[Reading::took(
                "skill_drift",
                json!({"compacted_at":"2026-09-30T01:35:00Z","carried":2,"stale":[]})
            )]),
            vec!["skill drift: none (2 skill bodies carried since 2026-09-30T01:35:00Z match their files)".to_string()]
        );
        assert_eq!(
            lines(&[Reading::took(
                "skill_drift",
                json!({"compacted_at":null,"carried":0,"stale":[]})
            )]),
            vec!["skill drift: none (no compaction in this session)".to_string()]
        );
        assert_eq!(
            lines(&[Reading::failed(
                "skill_drift",
                "transcript unreadable".into()
            )]),
            vec!["READER FAILED skill_drift: transcript unreadable".to_string()]
        );
        assert!(lines(&[]).is_empty());
    }
}
