---
created: 2026-08-03T00:00
status: approved
---

# Function-pack Substrate

## Overview

A function pack ships business capability as versioned, verifiable data rather
than as core code. The substrate lets a pack declare roles, skills, workflows,
adapters, evaluators, assets, a maximum-expected effect ceiling, and benchmark
scenarios in one manifest; verify the pack before it is installed; and activate
it so its roles become resolvable through the existing role resolver.

The substrate grants nothing. A pack declares effects and capabilities; it never
grants them. Activation projects packaged roles into the `RoleLayer.PLUGIN`
directory the role registry already walks, and that is the whole integration:
`cli/src/fno/roles/registry.py` and `cli/src/fno/roles/resolver.py` are unchanged
by this work, which is what makes "activation changed no resolution behavior" a
provable claim rather than an assertion.

## Components

```text
cli/src/fno/plugins/manifest.py   frozen PackManifest and component declarations
cli/src/fno/plugins/verify.py     pure two-axis verification
cli/src/fno/plugins/registry.py   installed packs, digests, activation receipts
cli/src/fno/plugins/activate.py   projection into the plugin role layer
cli/src/fno/plugins/cli.py        hidden `fno config plugins` administrative surface
plugins/growth-studio/            the first pack (content, not code)
```

`manifest.py` and `verify.py` are pure over their inputs. `registry.py` and
`activate.py` are the only modules that touch the filesystem outside a read.

## Identity reuse

`PackManifest.roles` carries full `RoleManifest` objects, reused unchanged. When
activation projects a role into the plugin layer it writes a
`RoleDefinitionSource` whose `manifest` is exactly the `RoleManifest` the
resolver already digests. A divergent role or function identity type would
compute a different digest and break the identity and overlay checks
`resolve_role` performs, so the pack reuses `RoleRef`, `FunctionRef`, and the
effect-class and destination vocabulary that `classify_effect` and the approval
authority consult.

The effect ceiling (`permissions`) is a declaration, not a grant. A pack cannot
know the work order and attempt a concrete `EffectRef` is bound to, so the
ceiling carries only the effect class and destination the review and authority
surfaces read. Activation never acts on it.

## Two verification axes

`fno config plugins verify` reports every condition twice on independent axes:

- `checked` or `unchecked`: was the condition evaluated at all?
- `passed`, `failed`, `blocked`, or `unknown`: the result, reusing
  `EvidenceResult`.

The axes are separate on purpose. An unchecked condition reporting `unknown` is
the honest shape for a check that could not run; collapsing the axes into one
boolean is how a verifier starts reporting green for a condition it never
evaluated. The command exits zero only when every condition is checked and
passed, so an unverified pack is never mistaken for a verified one.

The compatibility family goes beyond schema: it imports the closed topology
vocabulary from `fno.company.topology` and runs `resolve_role` against a
maximally permissive synthetic work order, so a manifest that would fail
resolution is caught before activation, not at execution.

A corrupt manifest reports `blocked` naming the file and the parse error and
never reports the pack as absent, mirroring the role registry's rule that a
corrupt source blocks rather than reading as missing.

## The grant boundary

Activation makes a role resolvable and grants nothing else. This is enforceable,
not aspirational, because the merged prerequisites are already built that way:

- A `RoleManifest` names `required_capabilities` and never grants them;
  capabilities arrive independently as work-order-scoped `CapabilityFact` values.
- Approval authority is the injected `Authority` plus
  `config.approvals.authorized_principals`, which activation does not write.
- Naming a `ContextReference` does not grant access to its content; the read
  boundary still enforces access.

An activated pack that declares an external-publication effect still cannot
dispatch one, because the authority is unchanged and `classify_effect` still
requires approval for the class. Progressive capability exposure is scoped, not
global: a pack-contributed capability is reachable only for a role whose manifest
declares it and only when a matching work-order-scoped fact exists. This reuses
the scoping rule `resolve_role` already enforces rather than adding a second
gate, and it is a security boundary, not an ergonomics one: untrusted retrieved
text reaching an agent holding every installed capability is a materially larger
blast radius than the same text reaching a bounded one.

## Registry and deactivation

The registry persists installed packs (id, version, digest, declared effect
ceiling) and activation receipts naming the exact role-layer paths each
activation wrote. The receipt's written-path list is what makes deactivation
surgical: `fno config plugins deactivate` removes only the paths this pack's receipt
recorded, leaving a hand-written definition in the same plugin layer untouched.

Adapter conformance is attributed to its pack digest. The approval store cannot
verify adapter conformance, so attribution makes a false declaration traceable
after the fact. Concurrent activations take a file lock and refuse on digest
ownership rather than overwriting.

## Ownership treaty

This substrate shares an epic with the company coordinator. The split is the
ownership treaty:

- The coordinator owns objective classification, campaign decomposition,
  coordination, joins, and execution-topology selection.
- The function-pack substrate owns pack manifests, verification, registry,
  activation, packaged roles, workflows, evaluators, assets, and progressive
  capability exposure.

The single shared seam is the closed topology vocabulary in
`fno.company.topology`, which the substrate imports and never redefines.

## Function-agnostic invariant

`cli/src/fno/plugins/` contains no branch on a pack id or a function name. The
first pack is discovered by path and validated by schema.
`scripts/ci/check-plugins-function-agnostic.sh` AST-scans the package for the
pack id and function-name literals in code (excluding docstrings), failing
loudly with file:line when one appears. The scan carries a positive control so
an empty result is never trusted.

The broader module-layer map, pack-projection rule, and extraction threshold are recorded in [Company Boundaries](company-boundaries.md).

## Scope of the first pack

The growth-studio pack ships four roles (marketing, communications, design,
social) reusing all four topology literals, each at `approval_floor: founder`
for external publication and an `authority_ceiling` no higher than `internal` for
drafting. It declares workflows, evaluators, assets, a maximum-expected
publication ceiling, and a benchmark scenario.

Pack skills, agents, and the invokable faucet are bundled at build time from the
pack manifest, not re-declared in `skill-bundles.yaml`. The parser globs
`plugins/*/plugin.yaml` and emits `pack-skill` and `pack-agent` rows whose
destinations are root-relative (`skills/<id>`, `agents/<id>.md`); the generator
copies them and the freshness gate audits them with no second declaration to
drift. The growth-studio pack ships four role subagents and the
`/fno:growth-launch` orchestrator this way.

The split the phrase "activation extends the toolkit" hides is now delivered:
presence is build time (skills and agents are copied into `skills/` and `agents/`
and read at session start, unconditionally), and permission is runtime (a
packaged role is projected into the plugin layer only on activation, and
`/fno:growth-launch` refuses at step one when the pack is not active). Three
lifetimes: build time bundles, activation permits, run time scopes. `fno doctor bundle
check` reports the bundles fresh with no drift.

The company-conformance pack adds internal support, sales, and recurring-operations scenarios, not products or independent consumers; full function packs remain out of scope here.

## Boundary model (recorded design stance)

The founder-approval boundary is the agent's tool list, not gates at every
dispatch seam. A packaged agent that holds only `Read`, `Write`, `Edit`, `Glob`,
and `Grep` cannot publish or delegate, on any reachable path: a direct
`@fno:<agent>` invocation runs the same bounded frontmatter the orchestrator
runs, exactly as a Codex subagent is bounded by its inherited `sandbox_mode`
rather than by per-spawn gates. The orchestrator's activation and
role-resolution gates are for the *composed* launch (a reviewed campaign
bundle); an individual agent invoked directly can still only produce a draft.

`growth-launch` is a Claude-plugin surface: it dispatches the role subagents
through Claude's Task tool. The Codex agent TOMLs for these agents are
intentionally not generated, because Codex's coarse sandbox (read-only or
workspace-write) cannot express the bounded-tool allowlist the Claude harness
enforces; advertising the skill on a harness that cannot bound its agents would
be the defect, not the omission.

The role resolver's candidate sort assumes one context reference per
identifier, so an unreadable observation carries no `content_digest`
(`None`) without a sort clash; the catalog producer guarantees that
one-per-identifier invariant.
