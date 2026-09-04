//! x-cd67 US4 resolve_branch family: moved verbatim out of agents_view.rs
//! (file budget shrink). Parent helpers resolve through the glob.
use super::*;

// ---- resolve_branch (x-cd67 US4) ----

fn branch_tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fno-branch-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn resolve_branch_reads_plain_checkout_head() {
    let cwd = branch_tmp("plain");
    std::fs::create_dir_all(cwd.join(".git")).unwrap();
    std::fs::write(cwd.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    assert_eq!(resolve_branch(&cwd), Some("main".into()));
    // A slash-bearing branch keeps only its leaf name.
    std::fs::write(cwd.join(".git/HEAD"), "ref: refs/heads/feature/x-cd67\n").unwrap();
    assert_eq!(resolve_branch(&cwd), Some("x-cd67".into()));
    std::fs::remove_dir_all(&cwd).unwrap();
}

#[test]
fn resolve_branch_walks_up_to_repo_root_from_subdir() {
    // codex review: a pane started in a subdirectory resolves the repo's
    // branch by walking up to the nearest `.git`, not degrading to the tail.
    let root = branch_tmp("subdir");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    let sub = root.join("crates/fno");
    std::fs::create_dir_all(&sub).unwrap();
    assert_eq!(resolve_branch(&sub), Some("main".into()));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn resolve_branch_detached_head_shortens_sha() {
    let cwd = branch_tmp("detached");
    std::fs::create_dir_all(cwd.join(".git")).unwrap();
    std::fs::write(
        cwd.join(".git/HEAD"),
        "0123456789abcdef0123456789abcdef01234567\n",
    )
    .unwrap();
    assert_eq!(resolve_branch(&cwd), Some("01234567".into()));
    std::fs::remove_dir_all(&cwd).unwrap();
}

#[test]
fn resolve_branch_follows_worktree_gitdir_redirect() {
    // AC3-EDGE: a linked-worktree `.git` FILE points at the real gitdir.
    let root = branch_tmp("wt");
    let real_gitdir = root.join(".git/worktrees/x-cd67");
    std::fs::create_dir_all(&real_gitdir).unwrap();
    std::fs::write(real_gitdir.join("HEAD"), "ref: refs/heads/x-cd67\n").unwrap();
    let cwd = root.join("checkout");
    std::fs::create_dir_all(&cwd).unwrap();
    // A relative gitdir pointer resolves against cwd.
    std::fs::write(cwd.join(".git"), "gitdir: ../.git/worktrees/x-cd67\n").unwrap();
    assert_eq!(resolve_branch(&cwd), Some("x-cd67".into()));
    // Leading whitespace before the `gitdir:` key is tolerated (gemini review).
    std::fs::write(cwd.join(".git"), "  \tgitdir: ../.git/worktrees/x-cd67\n").unwrap();
    assert_eq!(resolve_branch(&cwd), Some("x-cd67".into()));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn resolve_branch_degrades_on_no_git_and_malformed_head() {
    // AC1-ERR: a plain dir with no .git -> None (poll must not error).
    let cwd = branch_tmp("nogit");
    assert_eq!(resolve_branch(&cwd), None);
    // A malformed HEAD (neither ref: nor 40-hex) -> None.
    std::fs::create_dir_all(cwd.join(".git")).unwrap();
    std::fs::write(cwd.join(".git/HEAD"), "garbage not a ref\n").unwrap();
    assert_eq!(resolve_branch(&cwd), None);
    // A worktree pointer whose gitdir target vanished -> None (pruned wt).
    std::fs::write(cwd.join(".git/HEAD.tmp"), "x").unwrap(); // noise
    let dangling = branch_tmp("dangling");
    std::fs::write(dangling.join(".git"), "gitdir: /nonexistent/gitdir\n").unwrap();
    assert_eq!(resolve_branch(&dangling), None);
    std::fs::remove_dir_all(&cwd).unwrap();
    std::fs::remove_dir_all(&dangling).unwrap();
}

// -- x-132c: the lineage forest the sideline orders and indents by --------

struct LRow {
    name: &'static str,
    id: Option<&'static str>,
    parent: Option<&'static str>,
}

fn layout(rows: &[LRow]) -> (Vec<usize>, Vec<usize>) {
    lineage_layout(rows, |r| r.id, |r| r.parent)
}

/// Display labels for the returned index order.
fn ordered_names(rows: &[LRow], order: &[usize]) -> Vec<&'static str> {
    order.iter().map(|&i| rows[i].name).collect()
}

#[test]
fn derive_rows_reads_the_spawned_by_edge_tolerantly() {
    // The lineage join key parses like every other registry field: present
    // and non-empty -> Some, absent/blank/null -> None, never a parse
    // failure for the whole row.
    let raw = reg(
        r#"{"name":"child","cwd":"/w","status":"live","harness":"claude",
                 "harness_session_id":"sid-c","spawned_by_session":"sid-p"}"#,
    );
    let rows = derive_rows(&raw, NOW).unwrap();
    assert_eq!(
        rows[0].spawned_by_session.as_deref(),
        Some("sid-p"),
        "a stamped parent edge must reach the reader"
    );
    let blank = reg(
        r#"{"name":"root","cwd":"/w","status":"live","harness":"claude",
                 "harness_session_id":"sid-r","spawned_by_session":""}"#,
    );
    let rows = derive_rows(&blank, NOW).unwrap();
    assert_eq!(rows[0].spawned_by_session, None);
}

#[test]
fn lineage_child_renders_beneath_its_parent() {
    let rows = [
        LRow {
            name: "king",
            id: Some("sid-k"),
            parent: None,
        },
        LRow {
            name: "worker",
            id: Some("sid-w"),
            parent: Some("sid-k"),
        },
    ];
    let (order, depths) = layout(&rows);
    assert_eq!(ordered_names(&rows, &order), vec!["king", "worker"]);
    assert_eq!(depths, vec![0, 1]);
}

#[test]
fn lineage_grandchild_nests_before_later_siblings() {
    // Pre-order: the grandchild renders beneath ITS parent, ahead of the
    // parent's name-later sibling.
    let rows = [
        LRow {
            name: "king",
            id: Some("k"),
            parent: None,
        },
        LRow {
            name: "a-child",
            id: Some("a"),
            parent: Some("k"),
        },
        LRow {
            name: "a-grand",
            id: Some("g"),
            parent: Some("a"),
        },
        LRow {
            name: "b-child",
            id: Some("b"),
            parent: Some("k"),
        },
    ];
    let (order, depths) = layout(&rows);
    assert_eq!(
        ordered_names(&rows, &order),
        vec!["king", "a-child", "a-grand", "b-child"]
    );
    assert_eq!(depths, vec![0, 1, 2, 1]);
}

#[test]
fn lineage_missing_parent_is_a_root_never_an_error() {
    let rows = [
        LRow {
            name: "orphan",
            id: Some("o"),
            parent: Some("gone"),
        },
        LRow {
            name: "plain",
            id: Some("p"),
            parent: None,
        },
    ];
    let (order, depths) = layout(&rows);
    assert_eq!(ordered_names(&rows, &order), vec!["orphan", "plain"]);
    assert_eq!(depths, vec![0, 0]);
}

#[test]
fn lineage_two_row_cycle_terminates() {
    // Ambient-captured parent values are never validated, so a cycle must
    // lay out (entry rooted, the other member beneath it), not hang.
    let rows = [
        LRow {
            name: "a",
            id: Some("id-a"),
            parent: Some("id-b"),
        },
        LRow {
            name: "b",
            id: Some("id-b"),
            parent: Some("id-a"),
        },
    ];
    let (order, depths) = layout(&rows);
    assert_eq!(
        ordered_names(&rows, &order),
        vec!["a", "b"],
        "every cycle member renders exactly once"
    );
    assert_eq!(
        depths,
        vec![0, 1],
        "the cycle entry roots, the member nests"
    );
}

#[test]
fn lineage_self_edge_roots() {
    let rows = [LRow {
        name: "self",
        id: Some("s"),
        parent: Some("s"),
    }];
    let (order, depths) = layout(&rows);
    assert_eq!(ordered_names(&rows, &order), vec!["self"]);
    assert_eq!(depths, vec![0]);
}

#[test]
fn lineage_depth_is_capped_for_rendering() {
    let mut rows = vec![LRow {
        name: "r0",
        id: Some("i0"),
        parent: None,
    }];
    for d in 1..=14 {
        rows.push(LRow {
            name: Box::leak(format!("r{d}").into_boxed_str()),
            id: Some(Box::leak(format!("i{d}").into_boxed_str())),
            parent: Some(Box::leak(format!("i{}", d - 1).into_boxed_str())),
        });
    }
    let (order, depths) = layout(&rows);
    assert_eq!(order.len(), 15);
    assert!(
        depths.iter().all(|&d| d <= MAX_LINEAGE_DEPTH),
        "no row indents past the cap: {depths:?}"
    );
    assert_eq!(*depths.last().unwrap(), MAX_LINEAGE_DEPTH);
}

#[test]
fn lineage_flat_set_is_byte_identical_to_input_order() {
    let rows = [
        LRow {
            name: "b",
            id: Some("b"),
            parent: None,
        },
        LRow {
            name: "a",
            id: Some("a"),
            parent: None,
        },
    ];
    let (order, depths) = layout(&rows);
    assert_eq!(ordered_names(&rows, &order), vec!["b", "a"]);
    assert_eq!(depths, vec![0, 0]);
}
