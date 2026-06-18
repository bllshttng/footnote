# Flag Migration

Old flags still work. This reference maps them to the size system for backwards compatibility.

## Presets

| Old | New Equivalent | Notes |
|-----|---------------|-------|
| `--lean` | `-S` | Exact match |
| `--quick` | `-S` | Alias for --lean |

## Individual Flags (all still work as overrides)

| Flag | Meaning | Profile where it's already OFF |
|------|---------|-------------------------------|
| `-D` / `--no-docs` | Skip docs | S |
| `-E` / `--no-external` | Skip external review | S |
| `-B` / `--no-browser` | Skip browser testing | S, M |
| `-H` / `--no-how-to` | Skip how-to | S, M |
| `-P` / `--no-ship` | Skip PR creation (work stays local) | None (ship is on for all) |
| `-C` / `--clean` | Enable clean | L (already on) |
| `--adversarial` | Enable adversarial challenge | L (already on) |
| `--research` | Enable research phase | L (already on) |

## Combination Translation

Common old patterns and their size equivalents:

| Old Pattern | Closest Size | Exact? |
|-------------|-------------|--------|
| `-DEBH` | `-S` | Yes (S turns all of these off) |
| `--lean` | `-S` | Yes |
| `--quick` | `-S` | Yes |
| (no flags) | `-M` | Yes |
| `--clean` | `-L` | Approximate (L adds research + adversarial) |
| `-B` | `-M` with browser off | Already M default (off in M) |

## Parsing Priority

1. Size flag (-S, -M, -L) sets the base profile
2. Individual flags override specific toggles
3. If both old combo flags AND a size flag are present, size flag sets the base and old flags override
4. Example: `/target -M -D` = medium profile with docs disabled

## Positive Overrides (opt-in flags)

Some flags add capabilities rather than skip them:

| Flag | Effect | Useful with |
|------|--------|-------------|
| `--docs` | Enable docs | `-S --docs` to add docs to small |
| `--external` | Enable external review | `-S --external` |
| `--clean` / `-C` | Enable clean pass | `-M --clean` |
| `--adversarial` | Enable adversarial | `-M --adversarial` |
| `--research` | Enable research | `-M --research` |
| `--browser` | Enable browser testing | `-M --browser` |
| `--how-to` | Enable how-to guide | `-M --how-to` |
