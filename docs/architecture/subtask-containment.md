# Subtask containment

`contained_in` is the field that says: this node ships inside another node's PR. It has no PR of its own and never dispatches alone. When its owner's PR merges, the row closes with it. A row with declared surfaces closes only with merge evidence (see Surfaces). Without that evidence the containment releases and the row stays open.

## Readers

Three surfaces read the field, and all three refuse or hide the contained node:

- `selection_guards` (`cli/src/fno/backlog/advance.py`) reads it before every ancestor walk and returns `contained:<owner>`, which holds the node out of autonomous dispatch.
- `_redirect_if_contained` (`cli/src/fno/target_cli.py`) is the named-dispatch half: naming a contained node exits 2 with `ships inside <owner>'s PR` and claims nothing.
- When the owner's PR merges, `_cascade_close_contained` (`cli/src/fno/graph/cli.py`) closes every contained node, stamping `shipped inside <owner>` on each.

A fourth rule rides the same field. Only a child that is NOT contained in its parent makes that parent a box, in `_container_ids`. An owner whose every child is contained is a delivery unit, so it drains instead of sitting undispatchable forever.

## Writers

Two verbs stamp the field, both through the shared guards in `cli/src/fno/graph/_contain.py`:

- `fno backlog decompose <epic> --groups '[{adopt: [...]}]'` folds named nodes into a group child while authoring its plan fragment. A plan-less epic is refused there and pointed at the direct verb.
- `fno backlog contain <owner> <id>...` folds existing nodes into any not-done, live owner, with no plan and no group scaffolding. It stamps `contained_in` and `parent` in one locked mutation, atomically across the batch. The verb name matches the field: `contain` stamps `contained_in`. An earlier `adopt` verb was a retired alias for `backlog intake`, so the name stays retired.

The inverse is `fno backlog update <id> --parent null`, which un-contains a node as it moves it away. The release also drops the PR refs the containment bind inherited from the owner and records `released_from`. The owner's `Fixes` line then no longer closes the node. Containing it again clears the marker.

## Surfaces

`fno backlog contain <owner> <id>... --surface <repo-relative-path>` stamps `containment_surfaces` on each contained child. Pass the flag once per path. A child with declared surfaces closes on its owner's merge in two cases only. The first: the merged PR changes a file that matches a declared surface. The second: the owner's PR body binds the child through a closure line. With neither, the cascade releases the containment. The row drops `contained_in`, drops the owner's PR refs, keeps `parent`, and records `released_from`. The row stays open and dispatches again. A child with no declared surfaces keeps the unconditional close. The stranded sweep releases the declared children of a done owner. It cannot close them, because no merged-PR file set exists to prove their work shipped.

## Guards

`contain_into` runs its guards in order. A live worker on the target refuses the fold: a node someone is building is a delivery unit in practice. Then a cycle check. Then the one-level check: a target with children leaves those children dispatchable while their parent closes. Then the mid-flight check: an open PR or accrued cost is independent delivery evidence, and a node carrying either is refused while not done. A done node with a PR or cost is parented but containment is withheld, loudly: it already shipped on its own.

A dead owner, deferred or superseded, is refused outright by `refuse_dead_owner`: containment under a dead unit has no verb that ever releases it.

## Contain, not defer

Defer and containment answer different questions and are released by different hands. Defer is a judgement any groom pass can make, it hides the row from the board, and a contained row stays visible. Containment is released only by the owner's merge cascade or by moving the node away. Never defer a node to mean "this ships with that PR": contain it, then undefer, so the node is never armed for dispatch in between.
