# Agent field coverage

`fno doctor lint field-coverage` checks that every field declared on `AgentEntry` has an explicit projection disposition. A field must be projected through `required`, intentionally excluded through `storage_only`, covered by another declared schema block, or recorded under `known_gaps` with an owning node. CI runs this source-only lane because it needs no registry or built Rust binary.

`fno doctor lint field-coverage --live` adds two operator-run population readings. The persisted reading counts declared `AgentEntry` fields on real registry rows. The projected reading serializes those same rows through `fno.agents.format.serialize_entry` and counts emitted keys. The readings stay separate because a missing writer and a missing projection have different fixes. Empty registries and incomplete anchor fields are `UNMEASURED`, never clean.

A zero population on a snapshot is a point-in-time fact, not a verdict. The schema's `population_contract` block names the fields whose zero is valid, because a writer fires only under a condition or while a runtime state lasts:

- `forked_from_session_id`, `predecessor_session_ids`: conditional, after succession or branch classification.
- `live_status`, `live_status_basis`: conditional, from Claude supervisor enrichment and fired falsifiers.
- `delivery_policy`: transient, only while a busy-mode hold is active.

Each entry carries the mode, the measured surface, the writer, and the behavior test. The live evaluator partitions contract-covered zeroes into `conditional_zero` and `transient_zero` reports with their measured counts, and keeps them out of both `dead_fields` lists. A field absent from the contract stays a dead-field finding. An entry with malformed or incomplete metadata becomes a contract error. A contract never silently accepts a zero. Do not synthesize values into rows to move a count.

The 2026-09-01 acceptance baseline is a positive control. The persisted and projected findings must rediscover `crown`, `crown_grantor`, `crown_level`, and `crown_scope`, which have no population contract. The five contract fields must instead be classified, never dead. A regression fixture removes `node` from the projection contract. The source lane must rediscover that historical omission by name. The live contract now projects `node` in both serializers. The decisive-field lane must prove that the consolidation receipt exposes a candidate's `superseded_by`. Previously, the gate read `graph.closure` only for the resolved node. It then presented a superseded candidate as live.

A node that states a measurement in its details reruns that measurement at closure. `bash scripts/ci/check-agent-field-population-contract.sh` is that closure probe for this contract. It reruns the live JSON report and keeps the evaluator's real exit code. When the five fields are classified by the schema contract and absent from `dead_fields`, it prints `agent-field-population-contract: verified`. Dead fields outside the contract stay the evaluator's own finding and do not block the marker. It fails closed on missing JSON, an unmeasured registry, contract errors, or schema drift. A node whose closure depends on a measured population declares its own probe the same way.

Resolve a finding with one of four actions: fix its writer, project it through `required`, record it in `storage_only`, or add it to `known_gaps`. Do not populate an authority field such as crown until the operator decides whether that data must exist.
