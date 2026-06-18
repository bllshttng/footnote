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
from typing import TYPE_CHECKING, Any

import click
import typer
import typer.core
import typer.main

if TYPE_CHECKING:
    pass


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
            raise click.ClickException(
                f"Failed to import {self._import_path!r} for command "
                f"{self.name!r}: {exc}"
            ) from exc
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
            # path from ``fno executor resolve <args>`` to ``fno executor <args>``.
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
        lazy_subcommands: dict[str, tuple[str, str] | tuple[str, str, dict[str, Any]]] | None = None,
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
        return None

    def _make_stub(self, name: str) -> _LazyStub:
        entry = self._lazy[name]
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
        info_overrides = {k: v for k, v in options.items() if k != "hidden"}
        return _LazyStub(
            name=name,
            help=short_help,
            import_path=import_path,
            info_overrides=info_overrides,
            **stub_options,
        )

    def resolve_command(
        self, ctx: click.Context, args: list[str]
    ) -> tuple[str | None, click.Command | None, list[str]]:
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
    lazy_subcommands: dict[str, tuple[str, str] | tuple[str, str, dict[str, Any]]],
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
