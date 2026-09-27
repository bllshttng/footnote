"""`fno inbox day start|end`: relay the native `day` fold, which commits the row."""
import subprocess

import typer

day_app = typer.Typer(name="day", help="Read and record a daily boundary.")


def _make(kind: str):
    def command() -> None:
        from fno import paths
        from fno.agents.rust_runtime import refuse_without_binary
        from fno.rust_binary import resolve_binary

        binary = resolve_binary() or refuse_without_binary("day")
        argv = [str(binary), "day", "--kind", kind, "--commit"]
        argv += [x for p in paths.event_journals() for x in ("--events-path", str(p))]
        raise typer.Exit(code=subprocess.run(argv).returncode)

    return command


day_app.command("start")(_make("start"))
day_app.command("end")(_make("end"))
