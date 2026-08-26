"""``fno do`` - delivery-work command group.

The child apps are the same objects used by their one-release top-level
compatibility spellings.  Nesting them here changes only routing: each command
keeps its existing parser, output, and implementation.
"""

from __future__ import annotations

import typer

from fno.delivery.cli import delivery_app
from fno.loops import loops_app
from fno.phase import phase_app
from fno.plan import plan_app
from fno.pr import pr_app
from fno.pr_watch.cli import cli as pr_watch_app
from fno.provenance.cli import think_app
from fno.research import research_command
from fno.resume.cli import cli as resume_app
from fno.state.cli import cli as state_app
from fno.stub_manifest import stub_manifest_app
from fno.target_cli import target_app


do_app = typer.Typer(
    name="do",
    help="Delivery work: plans, targets, reviews, PRs, state, and supporting operations.",
    no_args_is_help=True,
)

# These two former roots are PR lifecycle operations, so they live below the
# existing PR group instead of minting one-child intermediate groups. Copy the
# registrations rather than mutating ``pr_app``: the old top-level PR app is a
# compatibility spelling and its collapse allocation must stay unchanged.
do_pr_app = typer.Typer(
    name="pr",
    help=pr_app.info.help,
    no_args_is_help=True,
    add_completion=False,
)
do_pr_app.registered_commands.extend(pr_app.registered_commands)
do_pr_app.registered_groups.extend(pr_app.registered_groups)
do_pr_app.add_typer(pr_watch_app, name="watch")
do_pr_app.add_typer(stub_manifest_app, name="stub-manifest")

do_app.add_typer(delivery_app, name="delivery")
do_app.add_typer(loops_app, name="loops")
do_app.add_typer(phase_app, name="phase")
do_app.add_typer(plan_app, name="plan")
do_app.add_typer(do_pr_app, name="pr")
do_app.command("research")(research_command)
do_app.add_typer(resume_app, name="resume")
do_app.add_typer(state_app, name="state")
do_app.add_typer(target_app, name="target")
do_app.add_typer(think_app, name="think")


def _register_review() -> None:
    # ``review`` is an eager command on fno.cli rather than its own sub-app.
    # Importing it here is safe because this module is loaded lazily only after
    # the root CLI has finished initialization. Under ``do`` it doubles as the
    # default callback of the review GROUP, so ``fno do review`` still runs the
    # panel unchanged while ``fno do review classify`` reaches the subcommand;
    # the guard at the top of the panel body keeps a subcommand invocation from
    # also running the panel.
    from fno.cli import review
    from fno.review.cli import review_app

    review_app.callback()(review)
    do_app.add_typer(review_app, name="review")


_register_review()
