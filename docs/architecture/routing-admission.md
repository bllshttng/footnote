# Routing admission: reserve shared account capacity before launching

Opt-in. `config.routing.admission.enabled = false` is the default, and a fresh install reserves nothing: dispatch behaves exactly as it did before this block existed.

## The question this answers

Several dispatchers can each see the same remaining account allowance and all launch. Between the moment one dispatcher reads "40 percent remains" and the moment its worker starts spending, other launches can spend the same allowance. Admission closes that gap with a reservation taken under the provider runtime state's file lock. The check and the write are one decision, so two concurrent dispatches cannot both promise the same remaining percent.

## What a reservation is, and is not

A reservation is an admission estimate recorded before launch. It is not a provider-enforced spending cap, and it is not a guarantee that an unbounded session will finish. The harness owns the session's actual model calls. `max_inflight_per_pool` (default 3) bounds how many reservations one pool can hold at once, so a bad estimate cannot claim unbounded capacity.

## Units

This first implementation speaks one unit: percent of a subscription quota window. `demand_pct` estimates what one dispatch can consume. `reserve_pct` protects a share of the window for difficult work and reviews. A dollar amount supplied as a percentage is refused with the exact field and unit error. API spend forecasting is unsupported and labeled so in config. API accounts participate through `max_inflight_per_pool` only, and the existing API account and cooldown guards are unchanged.

## The decision

For each fresh binding window of the account, admission requires:

    remaining_pct - outstanding_reserved_demand - this_demand >= reserve_pct

Windows are conjunctive: every active applicable window must admit the work. They are never averaged, and percentages from different windows are never added. A configured priority exception (a p0 dispatch, or an explicit `--force` at the spawn gate) can consume the protected reserve. It never bypasses known exhaustion, the inflight cap, an unprovable identity, or a tainted policy.

### Sample policy

```toml
[routing.admission]
enabled = true
max_inflight_per_pool = 3
reservation_ttl_seconds = 900

[routing.admission.demand_pct.do]
default = 10
high = 20

[routing.admission.reserve_pct.default]
default = 10
```

With this policy a `do` dispatch at high difficulty needs 20 percent of headroom above the outstanding reservations on every binding window. Ten percent of every window stays protected. Difficulty lookups take the most specific declared row at or below the node's band and fall back to the `default` verb. Known limits: the demand estimate is configured, not measured. A partial window, or one older than the probe TTL, admits nothing. It defers to the lane's existing low/unknown policy instead.

## Pool identity

Accounts that share a provider budget must not behave like independent budgets. Resolution order:

1. A record's declared `quota_pool` key wins. Credentials the operator knows share a budget are grouped by that key alone.
2. Otherwise a claude oauth record uses its observed principal, the proven canonical account identity. Never the alias or the directory basename. The principal is re-derived at reserve time. A manual canonical-account switch between selection and launch therefore attributes the reservation to the pool proven at launch, or refuses. A launch's reservation is never moved to another account by admission.
3. API-key records and non-claude records stay separate by record id.

When admission is armed, an unprovable claude principal refuses with `unknown_identity`: unknown usage cannot imply 100 percent available. An operator who cannot or does not want the proof can declare `quota_pool` keys instead.

## Where the reservation lives

Inside `provider-runtime-state.json`, under the existing document and update lock. There is no new state-root file and no second quota cache. Every existing writer of that document parses and re-persists the `reservations` block, so an unrelated health or usage write never eats a live worker's reservation. Rows expire after `routing.admission.reservation_ttl_seconds`. A committed worker is revalidated through refresh, never refunded merely because its TTL elapsed. Idempotency keys on the dispatch identity: a re-request by the same dispatch returns its held reservation, and two different dispatches never share a token. Releasing requires the holding dispatch's identity.

The owner of the math and the disk is the `fno-agents admission` verb (`crates/fno-agents/src/admission.rs`), beside the other runtime-state readers. Python (`cli/src/fno/adapters/providers/admission.py`) is the transport plus the one thing that cannot move: the budget identity, whose proof reads the operator's Keychain. Python sends one JSON payload; the verb decides, locks, and persists.

## Who calls it

The spawn gate reserves at its admit seams, as the last conjunct after cap, RAM, load, king share, and schema. A typed refusal exits with the provider-cap code and an `account_admission_refused` receipt naming the status, pool, binding window, and reset hint.

The autonomous route previews the same admission before answering `stay`. A refusal defers with an `admission:<status>` reason and the reset hint, which the backlog advance queue records on its typed wait. On the next tick, after fresh evidence, the launch runs again and the gate's reservation is the matching receipt.

`fno route admission` and the explain report render the pure read half, marked preview. A preview never reserves, commits, or releases, and never mutates the state document: byte-identical repeat renders are the contract.

## Reviewing and promoting thresholds

The numbers live entirely in config, and there is no daemon to tune. Start from the sample policy, run the fleet, and read `fno route admission` alongside the explain report. They show remaining, reserved, protected reserve, demand estimate, inflight count, and evidence age with units. If refusals name windows that later reset unspent, lower `reserve_pct` or the demand rows. If a pool keeps hitting the inflight cap before the percentages bind, raise `max_inflight_per_pool`. If the provider exposes only coarse quota data, the preview labels the estimate accordingly, and the evidence age tells you how much to trust it.

## Out of scope, deliberately

No new daemon, scheduler, or polling loop: this is a launch-time admission change only. No automatic login, aliasing, credential maintenance, or account switching. The canonical account stays a manual operator action, and admission reads the proof instead of making it. No per-dispatch usage forecasting from model names. No direct comparison between a dollar amount and a percentage, and no adding percentages that belong to different quota windows. Process concurrency caps and repo-local worker slots remain separate conjuncts owned where they are today.
