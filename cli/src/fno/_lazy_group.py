"""Click LazyGroup for deferred sub-app imports.

Implements the canonical Click lazy-loading-subcommands pattern adapted for
Typer 0.9+, which requires the cls to be a TyperGroup subclass.

Architecture
------------
``_LazyStub`` is a lightweight placeholder returned by ``get_command()`` for
every lazy entry.  It carries only the stored name, help text, hidden flag,
and import path -- no sub-app is imported.  Click and Typer's rich formatter
use the stub for help display (name + short help) without touching the
underlying module.

When the command is actually *invoked*, Click calls ``stub.make_context()``.
At that point the stub imports the real module, gets the attribute, converts
it to a Click command if needed, and delegates ``make_context`` to the real
command.  Click's invocation loop then calls ``sub_ctx.command.invoke(sub_ctx)``
where ``sub_ctx.command`` is the real command set by ``make_context``.

Usage (via the factory)
-----------------------
    from fno._lazy_group import make_lazy_group_cls
    import typer

    LAZY = {
        "state":   ("fno.state.cli:cli",    "manage state files"),
        "backlog": ("fno.graph.cli:cli",     "feature graph"),
    }

    app = typer.Typer(cls=make_lazy_group_cls(LAZY), ...)
"""

from __future__ import annotations

import importlib
import sys
from types import MethodType
from typing import TYPE_CHECKING, Any, Mapping

import click
import typer
import typer.core
import typer.main

# One implementation of the reinstall-window vocabulary, shared with the
# meta-path finder that covers the function-level imports this group cannot see.
from fno import _is_fno_module, _module_is_now_on_disk, _reinstall_hint

if TYPE_CHECKING:
    pass


class _CollapsedForward(click.Command):
    """Unregistered action adapter that preserves the original Click command."""

    def __init__(self, action: click.Command) -> None:
        super().__init__(name=action.name, hidden=True)
        self._action = action

    def make_context(
        self,
        info_name: str | None,
        args: list[str],
        parent: click.Context | None = None,
        **extra: Any,
    ) -> click.Context:
        return self._action.make_context(info_name, args, parent=parent, **extra)

    def invoke(self, ctx: click.Context) -> Any:
        return self._action.invoke(ctx)


def collapse_click_group(group: click.Group, *, keep: set[str]) -> click.Group:
    """Collapse non-KEEP children into one argument-dispatching group verb.

    KEEP children remain registered leaves. Every other first token is resolved
    against the original group and forwarded to that command's own parser, so
    its options, validation, help, callback, and nested actions stay unchanged.
    The marker is consumed by the verb-ratchet walker, which counts the group
    dispatcher once plus the deliberately retained children.
    """
    if getattr(group, "_fno_collapsed_dispatcher", False):
        return group

    original_list_commands = group.list_commands
    original_get_command = group.get_command
    original_resolve_command = group.resolve_command
    original_format_commands = group.format_commands
    original_shell_complete = group.shell_complete
    forward: dict[str, _CollapsedForward] = {}

    def list_commands(self: click.Group, ctx: click.Context) -> list[str]:
        return [name for name in original_list_commands(ctx) if name in keep]

    def resolve_command(
        self: click.Group, ctx: click.Context, args: list[str]
    ) -> tuple[str | None, click.Command | None, list[str]]:
        if args and args[0] not in keep:
            action = original_get_command(ctx, args[0])
            if action is not None:
                adapter = forward.setdefault(args[0], _CollapsedForward(action))
                return args[0], adapter, args[1:]
        return original_resolve_command(ctx, args)

    def format_commands(
        self: click.Group,
        ctx: click.Context,
        formatter: click.HelpFormatter,
    ) -> None:
        # Help is a user-facing catalog of ACTION arguments, so keep showing the
        # full original set even though only the selected children are leaves.
        saved = self.list_commands
        self.list_commands = original_list_commands  # type: ignore[method-assign]
        try:
            original_format_commands(ctx, formatter)
        finally:
            self.list_commands = saved  # type: ignore[method-assign]

    def shell_complete(
        self: click.Group,
        ctx: click.Context,
        incomplete: str,
    ) -> list[click.shell_completion.CompletionItem]:
        saved = self.list_commands
        self.list_commands = original_list_commands  # type: ignore[method-assign]
        try:
            return original_shell_complete(ctx, incomplete)
        finally:
            self.list_commands = saved  # type: ignore[method-assign]

    group.list_commands = MethodType(list_commands, group)  # type: ignore[method-assign]
    group.resolve_command = MethodType(resolve_command, group)  # type: ignore[method-assign]
    group.format_commands = MethodType(format_commands, group)  # type: ignore[method-assign]
    group.shell_complete = MethodType(shell_complete, group)  # type: ignore[method-assign]
    group._fno_collapsed_dispatcher = True  # type: ignore[attr-defined]
    group._fno_collapsed_keep = frozenset(keep)  # type: ignore[attr-defined]
    group._fno_collapsed_original_list_commands = original_list_commands  # type: ignore[attr-defined]
    group._fno_collapsed_original_get_command = original_get_command  # type: ignore[attr-defined]
    return group


def _import_failure_hint(exc: ImportError) -> str:
    """The reinstall-window hint for an ImportError, or "" for anything else.

    Exception-shaped adapter over ``fno._reinstall_hint``, which is where the
    text and the "is it ours?" gate actually live so the meta-path finder in
    ``fno/__init__.py`` and this path cannot drift into two different messages.

    Sharing one text is exactly why this has to check for it first: the finder
    raises with the hint ALREADY in its message, and that error is what arrives
    here, so appending unconditionally prints the same parenthetical twice.
    """
    hint = _reinstall_hint(getattr(exc, "name", None) or "")
    return "" if hint and hint in str(exc) else hint


# ---------------------------------------------------------------------------
# _LazyStub
# ---------------------------------------------------------------------------


class _LazyStub(click.Group):
    """Placeholder returned by LazyTypeGroup.get_command() for lazy entries.

    Carries only the stored name, short help, hidden flag, and import path.
    No import is triggered until ``make_context()`` is called (i.e. actual
    invocation).  Click's rich/plain help formatters only need the name and
    short help, so ``--help`` completes without importing any sub-app.

    After ``make_context()`` loads the real command, Click invokes the
    returned context via ``sub_ctx.command.invoke(sub_ctx)`` where
    ``sub_ctx.command`` is the real command -- not this stub.
    """

    def __init__(
        self,
        *,
        name: str,
        help: str,
        import_path: str,
        hidden: bool = False,
        info_overrides: dict[str, Any] | None = None,
        collapse_keep: set[str] | None = None,
    ) -> None:
        super().__init__(name=name, help=help, hidden=hidden)
        self._import_path = import_path
        # ``info_overrides``: kwargs forwarded to ``TyperInfo`` when the loaded
        # attr is a ``typer.Typer`` instance.  Used to preserve options that
        # ``app.add_typer(sub, help=..., invoke_without_command=True, ...)``
        # passed at the registration site.  Without this, the real command's
        # help / behavior reverts to whatever the Typer instance itself
        # defines and the parent-side override is lost.
        self._info_overrides: dict[str, Any] = dict(info_overrides or {})
        self._collapse_keep = collapse_keep
        self._real: click.Command | None = None

    def _load_real(self) -> click.Command:
        if self._real is not None:
            return self._real
        module_path, _, attr_name = self._import_path.rpartition(":")
        if not module_path:
            raise click.ClickException(
                f"Bad lazy entry for {self.name!r}: expected 'module:attr', "
                f"got {self._import_path!r}"
            )
        try:
            module = importlib.import_module(module_path)
        except ImportError as exc:
            # Imports here happen at INVOCATION time, so `uv tool install
            # --reinstall` (what `fno update` runs) can delete and rewrite this
            # package between process start and this line. On a box with several
            # launchd agents and live sessions, some `fno` process is nearly
            # always mid-flight during that window, so this is routine rather
            # than exotic.
            #
            # Retry ONCE, and only after confirming on disk that what was
            # missing is present now. That check is the whole difference between
            # this and a hopeful sleep-retry: a genuinely stale or broken
            # install still answers "absent" and still fails below with the same
            # message, so nothing is masked. A second failure is reported
            # normally rather than retried again.
            # ModuleNotFoundError specifically, not ImportError generally: only
            # that subclass proves a module was ABSENT. A plain "cannot import
            # name X from Y" names a module that exists, so the on-disk check
            # below would say yes and we would re-execute the whole module tree
            # for nothing, running every import-time side effect a second time.
            name = getattr(exc, "name", None) or ""
            module = None
            failure: ImportError = exc
            if (
                isinstance(exc, ModuleNotFoundError)
                and _is_fno_module(name)
                and _module_is_now_on_disk(name)
            ):
                try:
                    module = importlib.import_module(module_path)
                except ImportError as retry_exc:
                    # Report what the RETRY hit, never the stale first failure.
                    # The retry gets further through the tree, so it can surface
                    # a different and more truthful cause (a genuinely missing
                    # third-party dependency, say). Reporting the original would
                    # bury that under an fno reinstall hint and send the operator
                    # to `fno update` for a problem `fno update` cannot fix.
                    failure = retry_exc
                    module = None
            if module is None:
                raise click.ClickException(
                    f"Failed to import {self._import_path!r} for command "
                    f"{self.name!r}: {failure}{_import_failure_hint(failure)}"
                ) from failure
        attr = getattr(module, attr_name, None)
        if attr is None:
            raise click.ClickException(
                f"Module {module_path!r} has no attribute {attr_name!r} "
                f"(lazy entry for {self.name!r})"
            )
        if isinstance(attr, typer.Typer):
            # Preserve group structure even for single-command apps.  Without
            # this, ``typer.main.get_command(attr)`` collapses a one-command
            # Typer app into a bare TyperCommand, which changes the invocation
            # path from ``fno <group> <sub> <args>`` to ``fno <group> <args>``.
            # ``get_group_from_info`` keeps the group + subcommand shape that
            # ``app.add_typer`` produced under the eager-load model.
            from typer.models import TyperInfo

            info = TyperInfo(attr, **self._info_overrides)
            self._real = typer.main.get_group_from_info(
                info,
                pretty_exceptions_short=True,
                rich_markup_mode=None,
                suggest_commands=True,
            )
            if self._collapse_keep is not None:
                self._real = collapse_click_group(
                    self._real,
                    keep=self._collapse_keep,
                )
        elif isinstance(attr, click.Command):
            self._real = attr
        else:
            # Plain function with Typer-style params -- wrap as single-command
            # Typer app.  get_command() returns a TyperCommand (click.Command),
            # not a group, when the Typer app has exactly one command.  That
            # is the right shape here because plain-function entries (e.g.
            # ``done``, ``find``, ``new``, ``update``) were registered via
            # ``app.command()`` originally -- top-level commands, not groups.
            sub = typer.Typer(add_completion=False)
            sub.command(name=self.name)(attr)
            self._real = typer.main.get_command(sub)
        return self._real

    def make_context(
        self,
        info_name: str | None,
        args: list[str],
        parent: click.Context | None = None,
        **extra: Any,
    ) -> click.Context:
        return self._load_real().make_context(info_name, args, parent=parent, **extra)

    def invoke(self, ctx: click.Context) -> Any:
        # Defense-in-depth: although Click 8.x's ``Group.invoke`` calls
        # ``sub_ctx.command.invoke(sub_ctx)`` (which resolves to the real
        # command after ``make_context`` set it), older Click versions and
        # some intermediate call paths use the stub reference directly.
        # ``_load_real()`` is memoized, so this is free after ``make_context``
        # already ran.
        return self._load_real().invoke(ctx)

    # ``get_short_help_str`` is inherited from ``click.Command`` and reads
    # ``self.help``, so no override needed -- the stored help text is used
    # directly during ``--help`` rendering without any import.


# ---------------------------------------------------------------------------
# LazyTypeGroup
# ---------------------------------------------------------------------------


class LazyTypeGroup(typer.core.TyperGroup):
    """TyperGroup subclass that defers sub-app imports until invocation.

    Each entry in ``lazy_subcommands`` maps a command name to a 2-tuple
    ``(import_path, short_help)`` where:

    - ``import_path`` is ``"module.path:attr"``
    - ``short_help`` is a short one-line description shown in help output

    Optionally a 3-tuple ``(import_path, short_help, options)`` where
    ``options`` is a dict (currently supports ``{"hidden": True}``).

    ``list_commands()`` returns the lazy keys immediately with no import.
    ``get_command()`` returns a ``_LazyStub`` that loads the module only
    when the command is actually invoked.
    """

    def __init__(
        self,
        *args: Any,
        lazy_subcommands: dict[str, tuple[str, str] | tuple[str, str, dict[str, Any]]]
        | None = None,
        **kwargs: Any,
    ) -> None:
        super().__init__(*args, **kwargs)
        self._lazy: dict[str, tuple[str, str] | tuple[str, str, dict[str, Any]]] = (
            lazy_subcommands or {}
        )

    def list_commands(self, ctx: click.Context) -> list[str]:
        base = super().list_commands(ctx)
        seen = set(base)
        result = list(base)
        for name in self._lazy:
            if name not in seen:
                result.append(name)
        return result

    def get_command(self, ctx: click.Context, cmd_name: str) -> click.Command | None:
        cmd = super().get_command(ctx, cmd_name)
        if cmd is not None:
            return cmd
        if cmd_name in self._lazy:
            return self._make_stub(cmd_name)
        # A removed top-level verb refuses BY NAME. The root group is a
        # different class from TombstoneGroup (it owns lazy loading), so the
        # check has to live on both or removals are only taught one level down
        # - a guard on one of two reachable paths.
        from fno.tombstones import refuse, tombstone_for

        # The root's children ARE the top-level path, so the lookup is exact.
        if tombstone_for(cmd_name) is not None:
            raise refuse(cmd_name)
        return None

    def _make_stub(self, name: str) -> _LazyStub:
        entry = self._lazy[name]
        options: dict
        if isinstance(entry, str):
            import_path, short_help, options = entry, "", {}
        elif isinstance(entry, tuple) and len(entry) == 2:
            import_path, short_help = entry
            options = {}
        elif isinstance(entry, tuple) and len(entry) == 3:
            import_path, short_help, options = entry  # type: ignore[misc]
        else:
            raise TypeError(
                f"lazy_subcommands entry for {name!r} must be 'module:attr' "
                f"or (import_path, help) or (import_path, help, options); "
                f"got {type(entry).__name__}: {entry!r}"
            )
        # Stub-level options vs TyperInfo-level options:
        #   ``hidden`` lives on the stub itself so the parent's help listing
        #   filters it out.  Everything else (``help``, ``invoke_without_command``,
        #   ``rich_help_panel``, etc.) is forwarded to ``TyperInfo`` so it
        #   takes effect when the real Typer instance is converted to a Click
        #   group at invocation time.
        stub_options = {"hidden": bool(options.get("hidden", False))}
        info_overrides = {k: v for k, v in options.items() if k not in {"hidden", "collapse_keep"}}
        collapse_keep = options.get("collapse_keep")
        return _LazyStub(
            name=name,
            help=short_help,
            import_path=import_path,
            info_overrides=info_overrides,
            collapse_keep=set(collapse_keep) if collapse_keep is not None else None,
            **stub_options,
        )

    def resolve_command(
        self, ctx: click.Context, args: list[str]
    ) -> tuple[str | None, click.Command | None, list[str]]:
        # A moved top-level spelling forwards here, before any resolution:
        # rewriting the argv and falling through to the normal path makes the
        # destination parse its own arguments exactly as a direct invocation
        # would, so stdout is byte-identical. The forward is gated on the
        # destination root being registered, so a wave can seed VERB_MOVES
        # before it mints the destination; until then the OLD registration
        # serves the call without advertising a command that cannot resolve.
        # The gate is a
        # registry membership test, not get_command: a probe would import
        # fno.tombstones and build a stub only to throw it away, on the hot
        # leaf path, and would raise the tombstone refusal should a
        # destination name ever be removed.
        if args:
            from fno.verb_moves import (
                deprecation_line,
                destination_is_registered,
                forwarding_args,
                move_for,
            )

            move = move_for(args[0])
            roots = {*self.commands, *self._lazy}
            if move is not None:
                forwarded = forwarding_args(args[1:], move)
                if forwarded is None:
                    leaf = f" {args[1]}" if len(args) > 1 else ""
                    raise click.UsageError(
                        f"fno {args[0]}{leaf} was removed; use fno {move.to}{leaf}"
                    )
                if destination_is_registered(move, roots):
                    line = deprecation_line(args[0], args[1:], move)
                    if line is not None:
                        print(line, file=sys.stderr)
                    args = forwarded
        try:
            return super().resolve_command(ctx, args)
        except click.UsageError as exc:
            # TyperGroup.resolve_command already appends a "Did you mean ..."
            # hint when the typo matches one of the eagerly-registered
            # commands (i.e. those in ``self.commands``).  In that case we
            # do nothing -- appending again would produce a duplicated
            # message like "Did you mean 'help'?. Did you mean 'help'?"
            # (Codex P2 finding on PR #269).
            if "Did you mean" in (exc.message or ""):
                raise
            # A tombstone refusal already names the replacement. Appending a
            # fuzzy "Did you mean 'loops'?" to it offers a second, wrong answer
            # next to the right one.
            if "was removed" in (exc.message or ""):
                raise
            if self.suggest_commands:
                # Only the lazy keys are missing from the parent's
                # suggestion pool, so restrict the candidate list to those.
                if self._lazy and args:
                    from difflib import get_close_matches

                    matches = get_close_matches(args[0], list(self._lazy))
                    if matches:
                        suggestions = ", ".join(f"{m!r}" for m in matches)
                        message = exc.message.rstrip(".")
                        exc.message = f"{message}. Did you mean {suggestions}?"
            raise


# ---------------------------------------------------------------------------
# Factory
# ---------------------------------------------------------------------------


def make_lazy_group_cls(
    # Mapping, not dict: dict is invariant in its value type, so a caller whose
    # literal infers as `dict[str, tuple[str, str]]` (every entry a 2-tuple, no
    # options) fails to type-check against the union. The map is copied below
    # and never mutated, so the covariant read-only type is the honest one.
    lazy_subcommands: Mapping[str, tuple[str, str] | tuple[str, str, dict[str, Any]]],
) -> type[LazyTypeGroup]:
    """Return a LazyTypeGroup subclass with the given lazy map baked in.

    Typer's ``cls=`` parameter instantiates the class with only the kwargs
    that Typer knows about.  Using a closure lets us attach the map without
    touching Typer's internals.
    """
    _map = dict(lazy_subcommands)

    class _LazyGroupCls(LazyTypeGroup):
        def __init__(self, *args: Any, **kwargs: Any) -> None:
            super().__init__(*args, lazy_subcommands=_map, **kwargs)

    return _LazyGroupCls
