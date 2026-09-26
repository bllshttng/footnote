use super::*;

#[test]
fn nudge_post_is_suppressed_by_the_escape_hatch() {
    // The test suite must never comment on a real PR. With the guard set,
    // post_nudge_comment returns false without spawning gh (a bogus bin here
    // would otherwise error, not silently succeed).
    std::env::set_var("FNO_LOOPCHECK_NO_COMMENT", "1");
    let posted = post_nudge_comment(
        "/nonexistent/gh",
        std::path::Path::new("/tmp"),
        618,
        "@codex review",
    );
    std::env::remove_var("FNO_LOOPCHECK_NO_COMMENT");
    assert!(!posted);
}

#[test]
fn unresponsive_bot_drives_the_giveup_message() {
    // AC13: the NoProgress message names the bot, the nudge count, and the
    // elapsed time instead of a bare fingerprint streak.
    let pr = bot_review_pr(
        "chatgpt-codex-connector",
        vec![bn(
            "chatgpt-codex-connector",
            NudgeClass::Unresponsive,
            3,
            20,
            47,
        )],
    );
    let n = unresponsive_bot(&pr).expect("an unresponsive bot");
    let msg = nudge_giveup_message(n);
    assert!(msg.contains("chatgpt-codex-connector"), "{msg}");
    assert!(msg.contains("3 nudges over 47m"), "{msg}");
    assert!(msg.contains("config.review.optional_apps"), "{msg}");
}

#[test]
fn no_giveup_for_an_awaiting_bot() {
    let pr = bot_review_pr(
        "chatgpt-codex-connector",
        vec![bn("chatgpt-codex-connector", NudgeClass::Awaiting, 1, 3, 3)],
    );
    assert!(unresponsive_bot(&pr).is_none());
}

fn nudge_cfg() -> NudgeConfig {
    NudgeConfig {
        login: "chatgpt-codex-connector".into(),
        review_handle: "@codex review".into(),
        wait_minutes: 15,
        ceiling: 3,
    }
}

#[test]
fn nudge_needs_nudge_when_never_mentioned() {
    let cfg = nudge_cfg();
    let b = classify_bot_nudge("chatgpt-codex-connector", &[], Some(&cfg), nudge_now());
    assert_eq!(b.class, NudgeClass::NeedsNudge);
    assert_eq!(b.nudges, 0);
    assert_eq!(b.review_handle, "@codex review");
}

#[test]
fn nudge_awaiting_within_window() {
    let cfg = nudge_cfg();
    let comments = vec![mention("@codex review", "2026-07-06T01:58:00Z")];
    let b = classify_bot_nudge(
        "chatgpt-codex-connector",
        &comments,
        Some(&cfg),
        nudge_now(),
    );
    assert_eq!(b.class, NudgeClass::Awaiting);
    assert_eq!(b.nudges, 1);
    assert!(b.newest_age_min <= 2);
}

#[test]
fn nudge_unresponsive_after_ceiling() {
    // AC3 building block: 3 mentions, newest older than wait_minutes.
    let cfg = nudge_cfg();
    let comments = vec![
        mention("@codex review", "2026-07-06T00:00:00Z"),
        mention("hey @codex review please", "2026-07-06T00:30:00Z"),
        mention("@codex review", "2026-07-06T01:00:00Z"),
    ];
    let b = classify_bot_nudge(
        "chatgpt-codex-connector",
        &comments,
        Some(&cfg),
        nudge_now(),
    );
    assert_eq!(b.class, NudgeClass::Unresponsive);
    assert_eq!(b.nudges, 3);
    assert!(b.span_min >= 120, "span was {}", b.span_min);
}

#[test]
fn nudge_reask_after_timeout_below_ceiling() {
    // One mention 60m ago, ceiling 3: the previous nudge timed out, ask again.
    let cfg = nudge_cfg();
    let comments = vec![mention("@codex review", "2026-07-06T01:00:00Z")];
    let b = classify_bot_nudge(
        "chatgpt-codex-connector",
        &comments,
        Some(&cfg),
        nudge_now(),
    );
    assert_eq!(b.class, NudgeClass::NeedsNudge);
    assert_eq!(b.nudges, 1);
}

#[test]
fn nudge_none_cfg_is_not_nudgeable() {
    // AC7: a peer-login sentinel classifies NotNudgeable.
    let b2 = classify_bot_nudge(SAME_MODEL_PEER_SENTINEL, &[], None, nudge_now());
    assert_eq!(b2.class, NudgeClass::NotNudgeable);
}

#[test]
fn nudge_malformed_created_at_is_needs_nudge() {
    // A mention with an unparseable createdAt must not push toward Unresponsive.
    let cfg = nudge_cfg();
    let comments = vec![mention("@codex review", "not-a-date")];
    let b = classify_bot_nudge(
        "chatgpt-codex-connector",
        &comments,
        Some(&cfg),
        nudge_now(),
    );
    assert_eq!(b.class, NudgeClass::NeedsNudge);
    assert_eq!(b.nudges, 1);
}

#[test]
fn resolved_nudge_configs_default_nudges_codex_only() {
    let cfgs = resolved_nudge_configs(&Settings::default());
    let codex = cfgs
        .iter()
        .find(|c| c.login == "chatgpt-codex-connector")
        .expect("codex nudgeable by default");
    assert_eq!(codex.review_handle, "@codex review");
    assert_eq!(codex.wait_minutes, 15);
    assert_eq!(codex.ceiling, 3);
    // gemini ships with an empty review_handle -> not nudgeable.
    assert!(cfgs.iter().all(|c| c.login != "gemini-code-assist"));
}

#[test]
fn nudge_override_sets_wait_and_ceiling_inheriting_handle() {
    let s = parse_settings(
        "[review.nudge]\n\"chatgpt-codex-connector\" = { wait_minutes = 30, ceiling = 5 }\n",
    );
    let cfgs = resolved_nudge_configs(&s);
    let codex = cfgs
        .iter()
        .find(|c| logins_correspond(&c.login, "chatgpt-codex-connector"))
        .unwrap();
    assert_eq!(codex.wait_minutes, 30);
    assert_eq!(codex.ceiling, 5);
    assert_eq!(codex.review_handle, "@codex review");
}

#[test]
fn nudge_override_disabled_removes_login() {
    let s = parse_settings("[review.nudge]\n\"chatgpt-codex-connector\" = { enabled = false }\n");
    let cfgs = resolved_nudge_configs(&s);
    assert!(cfgs
        .iter()
        .all(|c| !logins_correspond(&c.login, "chatgpt-codex-connector")));
}

#[test]
fn nudge_override_new_login() {
    let s = parse_settings(
            "[review.nudge]\n\"some-bot\" = { review_handle = \"@somebot review\", wait_minutes = 10, ceiling = 2 }\n",
        );
    let cfgs = resolved_nudge_configs(&s);
    let b = cfgs.iter().find(|c| c.login == "some-bot").unwrap();
    assert_eq!(b.review_handle, "@somebot review");
    assert_eq!(b.wait_minutes, 10);
    assert_eq!(b.ceiling, 2);
}

#[test]
fn nudge_malformed_override_degrades_to_non_nudgeable() {
    // AC8: a scalar, a list, and a non-integer wait_minutes each drop the
    // login to non-nudgeable without panicking.
    for body in [
        "[review.nudge]\n\"chatgpt-codex-connector\" = \"scalar\"\n",
        "[review.nudge]\n\"chatgpt-codex-connector\" = [1, 2]\n",
        "[review.nudge]\n\"chatgpt-codex-connector\" = { wait_minutes = \"soon\" }\n",
        // An absurd wait_minutes would overflow chrono::Duration::minutes and
        // panic the stop gate; it must degrade to non-nudgeable, not panic.
        "[review.nudge]\n\"chatgpt-codex-connector\" = { wait_minutes = 9999999999999999 }\n",
    ] {
        let s = parse_settings(body);
        let cfgs = resolved_nudge_configs(&s);
        assert!(
            cfgs.iter()
                .all(|c| !logins_correspond(&c.login, "chatgpt-codex-connector")),
            "malformed override must be non-nudgeable: {body}"
        );
    }
}
