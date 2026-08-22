"""``fno do target status`` -- resolved orientation report (x-a7be, change A).

A cold or compacted agent reconstructs its situation -- node lifecycle,
attended state, worktree path, repo test command, plan delta, done-condition --
from scattered ``fno`` / ``git`` / ``gh`` calls plus per-agent memory, the layer
that does not cross to OSS users or weaker models. This builds that situation
ONCE as a resolved fact block.

Contract (the invariants the report must keep):
  * Strictly READ-ONLY. Never mutates the graph, the manifest, or a claim.
  * Each line resolves INDEPENDENTLY. An unresolvable line prints ``unknown``
    plus the single command that resolves it -- never a stack trace, never an
    abort. A degraded ``gh``/``git``/graph never blocks the whole report.

This is the introspection family (``fno whoami`` / ``fno whoami status``), reusing
``load_agent_context`` for the manifest read rather than a parallel surface.
"""
from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, List, Optional

from fno.plan.reconcile import reconcile_plan


@dataclass(frozen=True)
class OrientLine:
    label: str
    value: str


# --- git helpers (self-contained so orient never imports target_cli; that
#     module imports orient for the `status` command + init print) -----------

# The one git-out helper lives in review_capability (moved there with the
# diff-sizing path); importing it keeps this module's callers (and the tests
# that patch `orient._git_out`) on the same name while deleting the copy.
from fno.review_capability import _git_out  # noqa: E402  (re-export seam)


def _is_linked_worktree(cwd: Path) -> bool:
    """True if ``cwd`` is inside a git LINKED worktree (git-dir != common-dir).

    Mirrors target_cli's location verdict in pure git terms: a linked worktree
    means we are already isolated.
    """
    gdir = _git_out(cwd, "rev-parse", "--git-dir")
    common = _git_out(cwd, "rev-parse", "--git-common-dir")
    if not gdir or not common:
        return False

    def _abs(p: str) -> Path:
        path = Path(p)
        return (path if path.is_absolute() else cwd / path).resolve()

    return _abs(gdir) != _abs(common)


# --- per-line resolvers (each fail-safe to `unknown ... | resolve: <cmd>`) ---

def _graph_entry(node_id: str, project_root: Path) -> Optional[Dict[str, Any]]:
    """The graph entry for ``node_id``, or None when absent. Raises on a real
    graph load error so the node line can degrade distinctly (not-in-graph vs
    unreadable)."""
    from fno.graph.load import load_graph
    from fno.paths import graph_json

    data = load_graph(graph_json())
    entries = data if isinstance(data, list) else []
    low = node_id.lower()
    for e in entries:
        if isinstance(e, dict) and str(e.get("id", "")).lower() == low:
            return e
    return None


def _node_line(
    node_id: Optional[str],
    project_root: Path,
    manifest_raw: Optional[Dict[str, Any]] = None,
) -> str:
    if not node_id:
        return "fresh (no node bound)"
    resolve = f"resolve: fno backlog get {node_id}"
    try:
        entry = _graph_entry(node_id, project_root)
    except Exception as exc:  # noqa: BLE001 - degrade, never abort the report
        return f"unknown (graph unreadable: {exc}) | {resolve}"
    if entry is None:
        return f"unknown (not in graph) | {resolve}"
    status = str(entry.get("status") or "").strip()
    pr = entry.get("pr_number")
    # `done` is terminal FIRST, before any PR-metadata branch: an advisory /
    # no-ship / manually-completed node is `done` without a PR, and must not
    # fall through to claim/fresh and misorient a resumed agent toward rework.
    if status == "done":
        if not pr:
            return "done (no PR)"
        # Only `merge_status` evidences a merge. Deriving "merged" from
        # done + pr_number asserted a merge nothing had checked, so a node
        # closed early read as shipped while the PR was still open.
        if entry.get("merge_status") == "merged":
            return f"shipped (PR #{pr} merged)"
        return f"shipped (PR #{pr}, awaiting merge)"
    if pr:
        return f"half-done (PR #{pr})"
    # In-progress: the current manifest itself holds this node's claim. (A
    # foreign worker's claim is not reliably a file on disk -- `fno do target init`
    # already refused the loser, so graph status orients them; this surfaces the
    # holder we DO know.)
    raw = manifest_raw or {}
    if str(raw.get("target_claim_key") or "") == f"node:{node_id}":
        holder = str(raw.get("target_claim_holder") or "this session")
        return f"in-progress (claim: {holder})"
    if status == "blocked":
        return "blocked (open dependency)"
    return f"fresh ({status or 'ready'})"


# --- live-manifest predicate (x-4af4) ---------------------------------------
#
# ONE liveness truth, two consumers: `_attended_line` (so a DEAD manifest reads
# attended, restoring /think's question flow) and the session-start GC hook
# (which shells `fno do target status --json` and archives a DEAD manifest). The
# hook must NOT re-implement pid/claim logic in bash -- it reads `manifest-live`.


def _pid_alive(pid_val: Any) -> bool:
    """Best-effort: is ``pid_val`` a running process on THIS host?

    Biased toward LIVE on any uncertainty: a false-live costs one autonomous
    /think, a false-dead would archive a still-running session's manifest.
    """
    try:
        pid = int(str(pid_val).strip())
    except (TypeError, ValueError):
        return False
    if pid <= 0:
        return False
    try:
        import psutil

        return bool(psutil.pid_exists(pid))
    except Exception:  # noqa: BLE001 - psutil missing/erroring -> os.kill fallback
        try:
            os.kill(pid, 0)
            return True
        except ProcessLookupError:
            return False
        except (PermissionError, OSError):
            return True  # exists-but-not-ours / uncertain -> biased live


def _claim_state(claim_key: str) -> Optional[str]:
    """The claim lockfile state for ``claim_key`` (free|live|suspect|stale|
    corrupted), or None on a read error. None means "claim signal unavailable"
    -- the caller must NOT treat it as confirmed-dead."""
    try:
        from fno.claims.core import claim_status
        from fno.claims.io import claims_root_for

        # node:/dispatch:/... keys live at the GLOBAL claims root, not the
        # per-repo default; route there (the same helper `fno agents claim status` uses)
        # or a node claim always reads `free` from a worktree checkout.
        state = claim_status(claim_key, root=claims_root_for(claim_key)).get("state")
        return str(state or "") or None
    except Exception:  # noqa: BLE001 - unreadable claim -> None (not confirmed dead)
        return None


def _manifest_liveness(manifest_raw: Optional[Dict[str, Any]]) -> tuple[str, str]:
    """``(state, reason)`` where state is ``live`` | ``dead`` | ``none``.

    The node claim is the ONLY durable liveness signal (x-ba4b: session-pid
    anchored + TTL-protected). ``owner_pid`` is the TRANSIENT ``fno do target init``
    wrapper pid (init-target-state.sh:525) that dies seconds after init, so it can
    only ever PROVE life (a live pid), never death. DEAD is asserted solely from a
    claim confirmed absent/expired:

      * claim held (live/suspect)          -> LIVE
      * claim absent/expired (free/stale)  -> DEAD (the durable anchor is gone)
      * claim unreadable (corrupted/error) -> LIVE (cannot confirm death)
      * NO claim key -> LIVE unless owner_pid still proves life

    The no-claim-key bias is load-bearing: a live NON-node target (free-text or a
    plan input writes graph_node_id:null and no claim) has a dead transient
    owner_pid post-init, so concluding DEAD from owner_pid there would archive a
    running session and flip /think to attended mid-run. With no durable death
    signal we bias LIVE (a false-live costs one autonomous /think; a false-dead
    archives a live session).
    """
    raw = manifest_raw or {}
    if not raw:
        return "none", "no manifest"

    claim_key = str(raw.get("target_claim_key") or "").strip()
    if claim_key:
        state = _claim_state(claim_key)
        if state in {"live", "suspect"}:
            return "live", f"claim {claim_key} {state}"
        if state in {"free", "stale"}:
            return "dead", f"claim {claim_key} {state}"
        # corrupted / unreadable -> claim signal unavailable, cannot confirm death
        return "live", f"claim {claim_key} unreadable (biased live)"

    if _pid_alive(raw.get("owner_pid")):
        return "live", "owner_pid alive"
    return "live", "no claim key; owner_pid transient (biased live)"


def _authority_granted(raw: Optional[Dict[str, Any]]) -> bool:
    """Authority fails CLOSED: it requires a LIVE CLAIM, and nothing else.

    Two properties have to hold at once, and only a claim delivers both. The
    grant must be live now, and it must stay readable after this process exits.
    ``owner_pid`` gives the first without the second: it is alive for every
    session at init time, claimless ones included, so a pid-based check reads
    granted at init and then silently evaporates minutes later - the operator
    walks away believing they have a grant they no longer hold.

    ``_manifest_liveness``'s bias toward live is right for ``attended`` (worst
    case you get asked) and wrong here, where a stale grant silently un-prompts
    every session that reads it (x-4af4: a defunct manifest once auto-locked an
    attended /think for ten days). So: no claim, no authority - which is also
    why a free-text run cannot hold one.
    """
    if not raw or str(raw.get("authority", "")).strip().lower() != "full":
        return False
    claim_key = str(raw.get("target_claim_key") or "").strip()
    return bool(claim_key) and _claim_state(claim_key) in {"live", "suspect"}


def _attended_line(manifest_raw: Optional[Dict[str, Any]]) -> str:
    state, reason = _manifest_liveness(manifest_raw)
    # A DEAD manifest (x-4af4) means the owning session is gone -- resolve to
    # ATTENDED regardless of the stale stamped value, and NAME it so the posture
    # is not silently changed (the original bug was a silent autonomous switch).
    # This branch also denies a dead manifest's authority grant.
    if state == "dead":
        return f"true (dead manifest: {reason}; attended)"
    if manifest_raw and "attended" in manifest_raw:
        val = str(manifest_raw["attended"]).strip().lower()
        line = f"{val} (manifest, live: {reason})"
        if _authority_granted(manifest_raw):
            line += "; authority: full (beastmode)"
        return line
    # No manifest yet: resolve from the substrate, mirroring init-target-state.sh
    # and the spawn_think precedent -- FNO_AGENT_SELF (injected into EVERY spawned
    # worker) is the reliable "not an operator at the keyboard" signal.
    if (
        os.environ.get("FNO_AGENT_SELF")
        or os.environ.get("FNO_BG")
        or os.environ.get("TARGET_UNATTENDED") == "1"
    ):
        return "false (substrate: spawned/bg worker)"
    return "true (substrate: operator session)"


def _manifest_live_line(manifest_raw: Optional[Dict[str, Any]]) -> str:
    """The machine-read liveness field the session-start GC keys on. A ``dead``
    value carries the archive command (the module's "unknown line names its one
    resolving command" idiom)."""
    state, reason = _manifest_liveness(manifest_raw)
    if state == "dead":
        return (
            f"dead ({reason}) | archive: "
            "fno do state archive --path .fno/target-state.md --type target"
        )
    if state == "none":
        return "none (no manifest)"
    return f"live ({reason})"


def _worktree_line(project_root: Path, node_id: Optional[str]) -> str:
    try:
        if _is_linked_worktree(project_root):
            return str(project_root)
    except Exception as exc:  # noqa: BLE001
        return f"unknown (git error: {exc}) | resolve: git rev-parse --git-dir"
    hint = node_id or "<node>"
    return f"on canonical main -- create with: fno do target start {hint}"


def _tests_line(project_root: Path) -> str:
    """The repo's test command(s), detected from project markers."""
    cmds: List[str] = []
    if (project_root / "pyproject.toml").exists() or (
        project_root / "cli" / "pyproject.toml"
    ).exists():
        cmds.append("pytest")
    if (project_root / "Cargo.toml").exists() or (
        project_root / "crates"
    ).is_dir():
        cmds.append("cargo test")
    if (project_root / "package.json").exists():
        cmds.append("npm test")
    if not cmds:
        return "unknown | resolve: set your repo's test command"
    return " | ".join(cmds)


def _peer_entry_identity(peer: object, shared: Optional[str]) -> Optional[str]:
    """The login one `config.review.peers` entry posts under, or None.

    Mirrors loop-check's rule (`resolved_required_bots_for_author`): a per-entry
    `identity` wins, else the shared `config.review.peer_identity`. BOTH are read
    for truthiness, not presence: loop-check tests `is_some()` only AFTER its
    parser has dropped an empty string to `None`
    (`scalar_string(v).filter(|s| !s.is_empty())`, loopcheck.rs), so
    `peer_identity = ""` is an UNSET carrier over there and the identity-free
    local `peer` gate is live. Reading it as configured here would announce
    `none (PR + CI only)` for an armed gate -- the exact wedge this file closes.
    `resolve_local_peers` and `fno do pr`'s reviewer read already agree.
    """
    if isinstance(peer, dict):
        own = peer.get("identity")
        if own:
            return str(own)
    return shared or None


def _peer_provider(peer: object) -> str:
    """The provider name of a `config.review.peers` entry (scalar or mapping)."""
    if isinstance(peer, dict):
        return str(peer.get("provider") or "").strip()
    return str(peer).strip()


def _required_bots(review: Any) -> List[str]:
    """The must-have-reviewed login list: None/[] -> no gate (cv-6537099f).

    `config.review.github_apps` (the legacy required_bots aliases it) UNION the
    posting identity of every identity-BACKED peer, which is exactly what
    loop-check's `resolved_required_bots_for_author` requires. Reading only the
    first half announced `none (PR + CI only)` for a repo whose `peer_identity`
    gate was live -- the same wedge the local-attestation half of this file
    closes, left open on the sibling carrier.

    A peer-contributed login carries its producer, because it is NOT an App bot
    that posts on its own: nothing appears under `peer_identity` unless the
    session runs `/review peer <pr#> <provider> --post`. Rendered bare beside
    `chatgpt-codex-connector` it reads as self-posting, and the session waits
    for a review that never arrives - the wedge this file exists to close,
    wearing the other carrier's clothes.

    The effective default matches the Rust loop-check: absent == [] == no review
    gate (PR + CI only), not the old ["chatgpt-codex-connector"].

    Dedup is on the RAW login, never on the formatted entry: once an entry
    carries a producer suffix, a prefix test drops a distinct login whose name
    is a prefix of one already present (`bot` behind `bot-extra`), silently
    omitting a gate loop-check enforces.

    The `cross-model only` clause is the honest form of a claim this file cannot
    verify. When the peer's model family matches the author's, loop-check swaps
    the login for an unmatchable sentinel (`apply_same_model_guard`), so a review
    posted under it can never clear the gate - and `fno do target init` does not
    refuse that case, because `resolve_local_peers` skips identity-backed
    entries. Deciding WHICH login is affected needs the author harness, the
    session dependency this file declines. So the line narrows its claim to what
    config alone supports rather than computing the answer: it names the login,
    names the producer, and names the condition, instead of asserting a
    clearability it has no evidence for.
    """
    from fno.review.provider_resolution import DISPATCHABLE_PROVIDERS

    apps: List[str] = list(review.github_apps) if review.github_apps else []
    # Aggregate providers PER LOGIN before rendering. First-seen-wins dropped
    # information twice: a peer identity colliding with a `github_apps` login
    # was skipped entirely, so its producer and condition vanished and the
    # entry read as an App that posts itself; and under a shared
    # `peer_identity` only the first provider survived, so a config whose
    # second entry is the only drivable one advertised the undrivable first.
    # The carrier SOURCE travels with the login, not just its providers.
    # `--post` posts under the SHARED `config.review.peer_identity` and reads its
    # PAT from `peer_token_env`; it cannot select a per-entry `identity`
    # (peer.md preconditions). So a per-entry login has no posting runner at all,
    # and printing `--post` for it advertises a command whose helper stops on a
    # missing precondition - the same unrunnable-producer wedge, one carrier over.
    shared = review.peer_identity or None
    by_login: Dict[str, List[str]] = {}
    per_entry: set[str] = set()
    for peer in review.peers or []:
        login = _peer_entry_identity(peer, review.peer_identity)
        if not login:
            continue
        by_login.setdefault(login, [])
        if login != shared:
            per_entry.add(login)
        provider = _peer_provider(peer)
        if provider and provider not in by_login[login]:
            by_login[login].append(provider)

    def _post_clause(login: str, providers: List[str]) -> str:
        if login in per_entry:
            return (
                "no --post runner: per-entry identity cannot be posted under | "
                "resolve: set config.review.peer_identity + peer_token_env, or "
                "drop the per-entry identity to use the local peer gate"
            )
        # Same drivability rule as the identity-free producer: a name
        # `/review peer` cannot drive is not a producer, and printing one as
        # `--post` is the wedge with the other carrier's label on it.
        drivable = [p for p in providers if p.lower() in DISPATCHABLE_PROVIDERS]
        if not drivable:
            named = ", ".join(providers) if providers else "<provider>"
            return f"no /fno:review peer runner for [{named}]"
        # NEVER `<a|b>`: the peer arg parser matches a known provider name, so an
        # alternation is discarded and the provider falls back to codex - which
        # can refuse a gate a configured gemini would have cleared. Same reason
        # the local-attestation branch prints a placeholder plus the set.
        if len(drivable) == 1:
            return (
                f"post: /fno:review peer <pr#> {drivable[0]} --post; "
                f"cross-model only"
            )
        return (
            f"post: /fno:review peer <pr#> <provider> --post; cross-model only, "
            f"configured: {', '.join(drivable)}"
        )

    rendered: List[str] = []
    for app in apps:
        rendered.append(
            f"{app} ({_post_clause(app, by_login.pop(app))})" if app in by_login else app
        )
    for login, providers in by_login.items():
        rendered.append(f"{login} ({_post_clause(login, providers)})")
    return rendered


def _optional_bots(review: Any) -> List[str]:
    """Honored-if-present reviewer logins (config.review.optional_apps)."""
    return list(review.optional_apps)


# The peers-derived requirement is synthesized by loop-check
# (`resolved_local_peer_reviewers_for_author`), not named in `reviewers`, so it
# has no descriptor in `_RESOLVABLE_REVIEWERS` and its producer lives here.
# The provider is NOT optional in the printed form: `/review peer` defaults a
# missing provider to `codex` and then REFUSES a provider matching the invoking
# harness, so a bare producer is unrunnable on a codex-authored session whose
# only peer is something else.
#
# With ONE runnable provider we name it and the command is pasteable. With
# several we must NOT print `<a|b>`: `/review peer` resolves its provider by
# matching a known name, so an alternation is discarded as unrecognized and the
# provider silently falls back to `codex` -- a command that looks pasteable and
# is not. Print a visible placeholder plus the configured set instead, so the
# reader picks rather than pastes.
#
# A name `/review peer` cannot DRIVE (`opencode`, `hermes`) hits that same
# fallback, so it is filtered out of the printed set rather than offered: the
# config is legal and arms the composite gate, but pasting the name reviews on
# codex instead, and on a codex-authored session that fallback is then refused
# as same-model and the gate never clears. Drivability is STATIC (the review
# runners in `provider_resolution`), so unlike model-family ELIGIBILITY it costs
# no author harness -- that second gap is the one the docstring below records
# for the identity-backed carrier, and it stays open here.
_EMIT_ATTESTATION = "bash skills/review/scripts/emit-attestation.sh"


def _local_review_gates(review: Any) -> List[str]:
    """Local-attestation gates loop-check holds the loop on, `name -> producer`.

    Two sources, both invisible to `_required_bots`: `config.review.reviewers`
    names them directly, and identity-free `config.review.peers` collapse into
    ONE composite `peer` requirement. No GitHub reviewer ever posts either -- a
    head-pinned `review_attestation` is the only evidence -- so a session that
    is not told they exist ships, promises, and then blocks on an attestation
    nothing in its plan produced (x-0322). Naming the producer alongside the
    gate is the point: the gate alone is a puzzle.

    Deliberately config-only, with no `detect_session()` call: the one case
    where the printed producer would not clear the gate -- every identity-free
    peer sharing the author's model family -- is already refused up front by
    `fno do target init` (`local_peers_refusal_message`), so buying it here would
    cost env-dependent output for a state a real run cannot be in.

    That argument covers the identity-FREE half only. The identity-BACKED
    same-model case is handled in `_required_bots` by narrowing the claim rather
    than computing it: the login is printed with a `cross-model only` condition
    instead of as unconditionally clearable, which costs no author harness and
    stops the line asserting something it cannot check.
    """
    from fno.config import resolvable_reviewers
    from fno.review.provider_resolution import DISPATCHABLE_PROVIDERS

    known = resolvable_reviewers(review.reviewer_registry)
    # Built-ins carry their own emit (sigma/declare auto-emit on a clean pass;
    # code-review bakes the helper into its invocation string). A project-
    # registered reviewer does not, so its printed producer must append the emit
    # or a session that follows it verbatim reviews and still leaves the gate
    # unmet -- the exact failure this line exists to prevent.
    builtin = resolvable_reviewers()
    gates: List[str] = []
    for name in review.reviewers or []:
        descriptor = known.get(str(name))
        if descriptor is None:
            gates.append(str(name))
            continue
        producer = descriptor.invocation
        if str(name) not in builtin:
            producer = f"{producer}, then {_EMIT_ATTESTATION} {name}"
        # What the rung ASSERTS travels with it. loop-check's own block reason
        # marks a self-cert `[self-cert: asserts no review evidence]`, and an
        # `invocation` reviewer attests only that its skill ran - footnote never
        # reads its output. Printing every rung as an undifferentiated "local
        # attestation" makes `declare` look like `sigma`, which is the trust
        # spectrum collapsing in the one line whose job is to show the gate.
        asserts = str(getattr(descriptor, "asserts", "") or "")
        if asserts == "self-cert":
            producer += " [self-cert: asserts no review evidence]"
        elif asserts == "invocation":
            producer += " [asserts invocation only: output is not read]"
        gates.append(f"{name} -> {producer}")
    # Only an identity-free entry contributes to the local composite gate; an
    # entry with its own `identity` (or any entry under a shared peer_identity)
    # is a posted-review login and is counted by `_required_bots` instead.
    # A provider-less entry is not a gate: loop-check's parser drops an entry
    # with an empty provider and no identity outright (`value_as_peers`), so
    # printing one would announce a gate nothing holds AND a command with no
    # provider to run.
    named = [
        provider
        for peer in (review.peers or [])
        if _peer_entry_identity(peer, review.peer_identity) is None
        and (provider := _peer_provider(peer))
    ]
    if named:
        unique = list(dict.fromkeys(named))
        runnable = [p for p in unique if p.lower() in DISPATCHABLE_PROVIDERS]
        # `cross-model only` for the same reason the identity-backed carrier
        # carries it: `/review peer` refuses a provider matching the invoking
        # harness, and WHICH provider that is needs the author harness this file
        # declines to resolve. Drivability is static and filtered above;
        # eligibility is not, so it is stated as a condition rather than
        # asserted away. Without it, a codex-authored session whose only
        # drivable peer is codex reads a pasteable command that refuses.
        if len(runnable) == 1:
            gates.append(
                f"peer -> /fno:review peer {runnable[0]} --attest "
                f"(cross-model only)"
            )
        elif runnable:
            gates.append(
                f"peer -> /fno:review peer <provider> --attest "
                f"(cross-model only, configured: {', '.join(runnable)})"
            )
        else:
            # The gate is armed and nothing configured can clear it. Announcing
            # a producer here would be the wedge in its loudest form: a command
            # that runs, reviews on the wrong model, and still leaves the gate
            # to be discovered after the promise.
            gates.append(
                f"peer -> no /fno:review peer runner for "
                f"[{', '.join(unique)}] | resolve: configure a codex or "
                f"gemini peer"
            )
    return gates


def _self_review_clause(project_root: Optional[Path] = None) -> str:
    """The self-review verb + fallback clause, or "".

    Names this harness's self-review verb and the ``--to-self --raw`` fallback so
    a session can satisfy the code-payload review gate itself rather than ask an
    epic leader. The level is sized from the branch's actual diff when one
    exists; pre-diff the invocation keeps its `<level>` placeholder instead of
    baking in one concrete level. The fallback is a prompt-line injection, so it
    is omitted on a headless substrate (no prompt line exists there). Never
    raises."""
    try:
        from fno.review_capability import (
            detect_session,
            diff_review_level,
            harness_can_self_review,
            self_review_invocation,
        )

        s = detect_session()
        if not harness_can_self_review(s.harness):
            return ""
        verb = self_review_invocation(s.harness, level=diff_review_level(project_root))
    except Exception:  # noqa: BLE001 - advisory; the stop gate is the backstop
        return ""
    harness = s.harness or "unknown"
    clause = (
        f"self-review required for code ({harness}): run `{verb}`, then "
        "`bash skills/review/scripts/emit-attestation.sh code-review`"
    )
    if s.substrate != "headless":
        clause += f"; refused? fno agents mail send '{verb}' --to-self --raw"
    return clause


def _done_when_line(manifest_raw: Optional[Dict[str, Any]], project_root: Path) -> str:
    raw = manifest_raw or {}

    # `or ""` would collapse a YAML-parsed bool False to "" -- read the value
    # straight so `attended: false` / `no_ship: false` are detected correctly.
    def _is(key: str, want: str) -> bool:
        return str(raw.get(key)).strip().lower() == want

    if _is("no_ship", "true") or _is("advisory", "true"):
        return "advisory: written + eval-green (no PR)"
    try:
        # ONE settings load for all three readers. `load_settings_for_repo` is
        # documented uncached and calls `_ensure_migrated`, which can WRITE, and
        # this line runs on `fno do target start`, `fno do target status`, and init -
        # so a per-reader load paid three full parse+validate passes, and three
        # chances to migrate, every time the orienter rendered.
        from fno.config import load_settings_for_repo

        review = load_settings_for_repo(project_root).review
        bots = _required_bots(review)
        optional = _optional_bots(review)
        local = _local_review_gates(review)
    except Exception:  # noqa: BLE001 - report unknown, never assert no-gate
        # NOT "none (PR + CI only)": loop-check reads the same keys out of the
        # same file and holds whatever gate they declare, so a config this side
        # cannot parse leaves the gate UNKNOWN, not absent. A reviewers typo
        # raises here and still gates over there - announcing no gate would be
        # the same lie the rest of this function exists to stop. The PR + CI
        # half stays on the line because it holds unconditionally: only the
        # REVIEW half is unknown, and dropping the prefix would understate the
        # gate in the other direction.
        line = (
            "PR + CI green + reviewed by [unknown (config.review unreadable) "
            "| resolve: fno config doctor]"
        )
    else:
        # `no_external` skips loop-check's GitHub-login reads entirely
        # (`login_skipped = no_external || !login_gate_active`), so EVERY login
        # gate - App bots, optional apps, and identity-backed peer posts - stops
        # being enforced for this session. Announcing them anyway sends the
        # session off to satisfy reviews nothing is waiting on. Local
        # attestations are unaffected: `reviewers_ok` is computed independently
        # of that skip, which is why they stay on the line.
        if _is("no_external", "true"):
            bots, optional = [], []
        if bots:
            bots_str = ", ".join(bots)
        elif _is("no_external", "true"):
            bots_str = "none (--no-external skips every login gate)"
        else:
            # "PR + CI only" is false whenever a local gate is armed; say which
            # half is empty instead of announcing a gate that does not exist.
            bots_str = "no App bot" if local else "none (PR + CI only)"
        line = f"PR + CI green + reviewed by [{bots_str}]"
        if local:
            line += f" + local attestation [{'; '.join(local)}]"
        if optional:
            line += f" (optional if present: [{', '.join(optional)}])"
        # Self-review floor: when the obligation is on and no local lane is
        # configured, name this harness's verb and the --to-self fallback so a
        # session serves itself instead of asking an epic leader. A configured
        # local lane already names the reviewer in the line above.
        if not local and getattr(review, "self_review_required", True):
            clause = _self_review_clause(project_root)
            if clause:
                line += f"; {clause}"
    if _is("attended", "false"):
        line += "; bg -> hand off the merge"
    return line


def _plan_line(plan_path: Optional[str], project_root: Path) -> str:
    if not plan_path:
        return "none (no plan bound)"
    return reconcile_plan(plan_path, project_root).summary()


def _render_boundary(verdicts: list) -> str:
    """Collapse per-blocker verdicts to one line. STALE > unknown > reconciled >
    fresh -- a single stale blocker is the actionable signal Step 0 keys on."""
    if not verdicts:
        # empty covers both "no blockers" and "blockers all skipped as not-stale"
        return "fresh (no landed blocker to reconcile)"
    stale = [v for v in verdicts if v.verdict == "stale"]
    unknown = [v for v in verdicts if v.verdict == "unknown"]
    reconciled = [v for v in verdicts if v.verdict == "reconciled"]
    if stale:
        clauses = [
            f"{v.blocker_id} ("
            f"{('PR #' + str(v.pr_number)) if v.pr_number else 'no PR'}"
            f"{', merged ' + v.completed_at[:10] if v.completed_at else ''})"
            for v in stale
        ]
        return "STALE vs " + ", ".join(clauses) + " - Step 0 required"
    if unknown:
        return "unknown (" + "; ".join(f"{v.blocker_id}: {v.reason}" for v in unknown) + ")"
    if reconciled:
        return "reconciled (" + ", ".join(f"{v.blocker_id} marker present" for v in reconciled) + ")"
    return "fresh (no done blocker newer than plan)"


def _boundary_line(
    node_id: Optional[str], plan_path: Optional[str], project_root: Path
) -> str:
    """Boundary-reconcile verdict for the report (x-d0ad). Advisory: the /target
    spine's Step 0 is what mandates acting on STALE. Never raises."""
    if not node_id:
        return "fresh (no node bound)"
    try:
        entry = _graph_entry(node_id, project_root)
    except Exception as exc:  # noqa: BLE001 - degrade, never abort the report
        return f"unknown (graph unreadable: {exc})"
    if entry is None:
        return "unknown (not in graph)"
    try:
        from fno.graph.load import load_graph
        from fno.paths import graph_json
        from fno.plan.boundary import boundary_reconcile

        verdicts = boundary_reconcile(entry, plan_path, load_graph(graph_json()))
        return _render_boundary(verdicts)
    except Exception as exc:  # noqa: BLE001 - render inside the try so the
        # module's "each line resolves independently, never abort" contract holds
        # even if _render_boundary itself raises on malformed verdict data.
        return f"unknown ({exc})"


# --- assembly + render -------------------------------------------------------

def build_report(
    project_root: Path,
    *,
    node_id: Optional[str] = None,
    plan_path: Optional[str] = None,
    manifest_raw: Optional[Dict[str, Any]] = None,
) -> List[OrientLine]:
    """Resolve all seven orientation lines. Read-only; never raises."""
    return [
        OrientLine("node", _node_line(node_id, project_root, manifest_raw)),
        OrientLine("attended", _attended_line(manifest_raw)),
        OrientLine("worktree", _worktree_line(project_root, node_id)),
        OrientLine("tests", _tests_line(project_root)),
        OrientLine("plan", _plan_line(plan_path, project_root)),
        OrientLine("boundary-reconcile", _boundary_line(node_id, plan_path, project_root)),
        OrientLine("manifest-live", _manifest_live_line(manifest_raw)),
        OrientLine("done-when", _done_when_line(manifest_raw, project_root)),
    ]


def render(lines: List[OrientLine]) -> str:
    width = max((len(ln.label) for ln in lines), default=0) + 1  # +1 for ':'
    return "\n".join(f"{(ln.label + ':'):<{width + 1}} {ln.value}" for ln in lines)


def load_orientation(
    project_root: Path,
    *,
    node_id: Optional[str] = None,
    plan_path: Optional[str] = None,
) -> List[OrientLine]:
    """Build the report by reading the session manifest (best-effort).

    Resolves node_id / plan_path / manifest_raw from ``target-state.md`` when it
    exists; degrades to a manifest-less report (substrate-resolved attended, no
    node) otherwise. Explicit ``node_id`` / ``plan_path`` override the manifest
    values (for ``fno do target status <node>``). Never raises.
    """
    manifest_raw = _read_manifest(project_root)
    if node_id is None:
        nid = str((manifest_raw or {}).get("graph_node_id") or "").strip()
        if nid and nid != "null":
            node_id = nid
    if plan_path is None:
        pp = str((manifest_raw or {}).get("plan_path") or "").strip().strip("\"'")
        if pp and pp != "null":
            plan_path = pp
    return build_report(
        project_root, node_id=node_id, plan_path=plan_path, manifest_raw=manifest_raw
    )


# Body keys appended below the frontmatter (init-target-state.sh writes them as
# `key: value` lines, NOT YAML frontmatter), so load_agent_context (frontmatter
# only) never sees them. The shared reader in target.manifest owns this set and
# the merge; orient keeps a thin wrapper so existing callers are unchanged.
_BODY_KEYS = ("graph_node_id", "target_claim_key", "target_claim_holder")


def _read_manifest(project_root: Path) -> Optional[Dict[str, Any]]:
    """Merged session manifest via the shared :mod:`fno.target.manifest` reader.

    Thin wrapper kept so existing orient callers are unchanged; the body-key set
    and the frontmatter+body merge live in one place now (x-2ccd), so the
    resume-bind primitive and the orienter share one contract.
    """
    from fno.target.manifest import read_target_manifest

    return read_target_manifest(project_root)


def _self_check() -> None:
    import tempfile

    with tempfile.TemporaryDirectory() as d:
        root = Path(d)
        # manifest-less, no node -> a fresh, operator-session report
        os.environ.pop("FNO_AGENT_SELF", None)
        os.environ.pop("FNO_BG", None)
        os.environ.pop("TARGET_UNATTENDED", None)
        lines = build_report(root, node_id=None, plan_path=None, manifest_raw=None)
        assert [ln.label for ln in lines] == [
            "node", "attended", "worktree", "tests", "plan",
            "boundary-reconcile", "manifest-live", "done-when",
        ], lines
        by = {ln.label: ln.value for ln in lines}
        assert by["node"].startswith("fresh"), by
        assert by["attended"].startswith("true"), by
        assert by["manifest-live"].startswith("none"), by
        assert "fno do target start" in by["worktree"], by
        out = render(lines)
        assert "node:" in out and "done-when:" in out, out
    print("orient self-check OK")


if __name__ == "__main__":
    _self_check()
