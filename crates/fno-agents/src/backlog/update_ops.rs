//! The `field=value` / `field+=value` / `field-=value` operator grammar for
//! `fno backlog update`. Pure syntax and pure edits: nothing here reads the
//! graph store, so the module stays unit-testable without a fixture. The
//! grammar is closed: a name outside the tables refuses and names its door.

/// The operator a token carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Set,
    Add,
    Remove,
}

/// One parsed operator token: the field, the operator, the value after the
/// first `=`, and the token as typed (refusals quote it verbatim).
#[derive(Debug, Clone)]
pub struct FieldOp {
    pub field: String,
    pub op: Op,
    pub value: String,
    pub token: String,
}

/// The shapes the operators act on. The kind fixes which operators a field
/// takes, so a scalar slot is never half-written by an append and a record
/// list is never replaced whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// `=` sets (`null` clears), `+=` appends a paragraph.
    Text,
    /// `=` replaces, `+=` adds, `-=` removes.
    List,
    /// `=` only; `null` clears where the field clears.
    Scalar,
    /// `=` only, a boolean value.
    Flag,
    /// `+=` and `-=` only: one entry per call, never a whole-list `=`.
    Records,
}

/// Every field an operator can write, keyed by stored name (the same names
/// `--set` takes). `check` refuses an operator the kind does not take.
const OP_FIELDS: &[(&str, FieldKind)] = &[
    ("details", FieldKind::Text),
    ("dispatch_brief", FieldKind::Text),
    ("completion_note", FieldKind::Text),
    ("tags", FieldKind::List),
    ("blocked_by", FieldKind::List),
    ("related", FieldKind::List),
    ("additional_prs", FieldKind::Records),
    ("title", FieldKind::Scalar),
    ("domain", FieldKind::Scalar),
    ("size", FieldKind::Scalar),
    ("model", FieldKind::Scalar),
    ("type", FieldKind::Scalar),
    ("batch", FieldKind::Scalar),
    ("orphan_ok", FieldKind::Scalar),
    ("priority", FieldKind::Scalar),
    ("project", FieldKind::Scalar),
    ("cwd", FieldKind::Scalar),
    ("difficulty", FieldKind::Scalar),
    ("dispatch_verb", FieldKind::Scalar),
    ("locked_by", FieldKind::Scalar),
    ("plan_path", FieldKind::Scalar),
    ("parent", FieldKind::Scalar),
    ("caused_by", FieldKind::Scalar),
    ("pr_number", FieldKind::Scalar),
    ("pr_url", FieldKind::Scalar),
    ("collisions_acknowledged", FieldKind::Scalar),
    ("source_node_id", FieldKind::Scalar),
    ("fixes_pr", FieldKind::Scalar),
    ("public", FieldKind::Flag),
    ("reverted", FieldKind::Flag),
    ("has_brief", FieldKind::Flag),
];

/// Names an operator must refuse with the door that owns the write. Read
/// before `OP_FIELDS`: these stored names are written by other doors, and
/// the tombstones the refusals mirror retire the same spellings.
const RETIRED: &[(&str, &str)] = &[
    ("model_tier", "difficulty=<band>"),
    (
        "locked_by_harness",
        "fno agents claim acquire node:<id> --holder <id>",
    ),
    (
        "locked_by_harness_session",
        "fno agents claim acquire node:<id> --holder <id>",
    ),
    ("status", "--status <word>"),
];

/// Parse one argv token as an operator: no leading `-`, a field name shaped
/// `[a-z][a-z0-9_]*`, and an optional `+`/`-` directly before the FIRST `=`
/// (so `details=a=b` sets the value `a=b`). A node id never contains `=`, so
/// an id is never read as an operator. `None` means not operator-shaped and
/// the caller falls through to its positional checks.
pub fn parse_op_token(raw: &str) -> Option<FieldOp> {
    if raw.starts_with('-') {
        return None;
    }
    let (head, value) = raw.split_once('=')?;
    let (field, op) = match head.strip_suffix('+') {
        Some(f) => (f, Op::Add),
        None => match head.strip_suffix('-') {
            Some(f) => (f, Op::Remove),
            None => (head, Op::Set),
        },
    };
    let shaped = !field.is_empty()
        && field.starts_with(|c: char| c.is_ascii_lowercase())
        && field
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !shaped {
        return None;
    }
    Some(FieldOp {
        field: field.to_string(),
        op,
        value: value.to_string(),
        token: raw.to_string(),
    })
}

/// Validate one parsed op: a retired name refuses with its replacement, an
/// unknown field refuses with the operator list, and an operator the kind
/// does not take refuses with the operators it does.
pub fn check(op: &FieldOp) -> Result<FieldKind, String> {
    if let Some((_, replacement)) = RETIRED.iter().find(|(f, _)| *f == op.field) {
        return Err(format!(
            "Error: '{}' is retired. Use {}.",
            op.field, replacement
        ));
    }
    let Some((_, kind)) = OP_FIELDS.iter().find(|(f, _)| *f == op.field) else {
        let names: Vec<&str> = OP_FIELDS.iter().map(|(f, _)| *f).collect();
        return Err(format!(
            "Error: unknown field '{}'. Operator fields: {}. Other stored fields: \
             --set <field>=<value>.",
            op.field,
            names.join(", ")
        ));
    };
    match (*kind, op.op) {
        (FieldKind::Text, Op::Remove) => Err(format!(
            "Error: {} is text. It takes = and += only.",
            op.field
        )),
        (FieldKind::Text, Op::Add) if op.value.trim().is_empty() => {
            Err(format!("Error: {}+= needs text.", op.field))
        }
        (FieldKind::Scalar | FieldKind::Flag, Op::Set) => Ok(*kind),
        (FieldKind::Scalar | FieldKind::Flag, _) => {
            Err(format!("Error: {} takes = only.", op.field))
        }
        (FieldKind::Records, Op::Set) => Err(format!("Error: {} takes += and -= only.", op.field)),
        _ => Ok(*kind),
    }
}

/// The append join: the trimmed `existing`, two newlines, then `add`; `add`
/// alone when there is nothing to join; `None` when the trimmed existing
/// text already ends with the trimmed `add` (the retry-safe no-op). The
/// literal `null` is data here; only `=` treats it as clear.
pub fn append_text(existing: Option<&str>, add: &str) -> Option<String> {
    match existing {
        Some(text) if !text.trim().is_empty() => {
            if text.trim().ends_with(add.trim()) {
                None
            } else {
                Some(format!("{}\n\n{}", text.trim_end(), add))
            }
        }
        _ => Some(add.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_splits_on_the_first_equals() {
        let op = parse_op_token("details+=a=b").expect("operator");
        assert_eq!(op.field, "details");
        assert_eq!(op.op, Op::Add);
        assert_eq!(op.value, "a=b");
        assert_eq!(op.token, "details+=a=b");
    }

    #[test]
    fn non_operator_shapes_return_none() {
        assert!(parse_op_token("--title=T").is_none());
        assert!(parse_op_token("-d=x").is_none());
        assert!(parse_op_token("plain").is_none());
        assert!(parse_op_token("x-aaaa1111").is_none());
        assert!(parse_op_token("Bad_Field=1").is_none());
        assert!(parse_op_token("=value").is_none());
    }

    #[test]
    fn kinds_refuse_the_operators_they_do_not_take() {
        let scalar = parse_op_token("title+=x").expect("operator");
        assert_eq!(check(&scalar).unwrap_err(), "Error: title takes = only.");
        let records = parse_op_token("additional_prs=1").expect("operator");
        assert_eq!(
            check(&records).unwrap_err(),
            "Error: additional_prs takes += and -= only."
        );
        let retired = parse_op_token("status=ready").expect("operator");
        assert_eq!(
            check(&retired).unwrap_err(),
            "Error: 'status' is retired. Use --status <word>."
        );
    }
}
