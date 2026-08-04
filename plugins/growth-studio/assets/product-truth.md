# Product truth

The verified, shipped Footnote facts that growth-studio claims must stay within.
This file is the worked example and fallback bundled with the pack; an
installing project points `[context.artifacts] product-truth` at its own ledger
so a second project is never reviewed against Footnote's facts.
A draft that asserts something outside this record fails factual review.

## Delivery pipeline

Footnote is an autonomous delivery pipeline that takes a feature from idea to a
shipped pull request in five phases: think, plan, do, review, ship.
Source: `AGENTS.md`.

## Function-pack substrate

Footnote ships a function-pack substrate that describes business capability as
versioned data, declared in a pack manifest and verified before activation.
Source: `docs/architecture/function-pack-substrate.md`.

## Progressive exposure

A capability is reachable only for the role that declares it, and only with a
matching work-order-scoped capability fact.
Resolution refuses with `MISSING_CAPABILITY` or `MISSING_CONTEXT` rather than
degrading, so a guard on one path cannot read as protection on another.

## Founder approval

Consequential effects require founder approval.
Activating a pack grants nothing: it projects role definitions only and never
writes approval authority, mints a capability, or dispatches an effect.

## Resolver invariants

Role resolution is a frozen core.
`cli/src/fno/roles/registry.py` and `cli/src/fno/roles/resolver.py` resolve
every role through one unchanged path, so a packaged role is provably the same
kind of resolution as a built-in one.

## Worktree-first

Feature work lands in a dedicated worktree branched off `origin/main`, and the
canonical checkout stays unclogged until the pull request merges.
Source: `AGENTS.md`.

## Compatibility floor

The growth-studio pack declares a `footnote_compat` minimum of `0.3.0`.
A pack that declares a lower floor than the running install rejects at verify
time rather than activating against an unsupported runtime.
