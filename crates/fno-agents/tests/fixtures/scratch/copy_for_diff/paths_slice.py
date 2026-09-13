    try:
        result = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True,
            text=True,
            check=False,
            cwd=cwd,
        )
        if result.returncode == 0 and result.stdout.strip():
            return Path(result.stdout.strip()).resolve()
    except (FileNotFoundError, OSError):
        pass
    return Path(cwd)


def resolve_repo_root() -> Path:
    """Resolve the repo root for state + artifact path resolution.

    Uncached wrapper: reads the process declaration (``os.getcwd()`` and
    ``FNO_REPO_ROOT``) at call time and delegates to the keyed
