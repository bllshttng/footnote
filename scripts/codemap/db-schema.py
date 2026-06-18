#!/usr/bin/env python3
"""Extract database schema details (tables, enums, constraints, triggers, FKs) and output markdown.

Usage:
    python3 db-schema.py [connection_string | project_dir]
    python3 db-schema.py --migrations-dir path/to/migrations
    python3 db-schema.py --schemas attendance,public
    python3 db-schema.py --enums-only

The optional positional argument is interpreted by shape: a string that names
an existing directory is treated as the project root to search (this is how
`fno codemap --db-schema` calls it, passing the analyzed repo); anything else
is treated as a PostgreSQL connection string.

Auto-detects a database connection when none is given, in this precedence:
    1. DATABASE_URL in the shell (explicit operator intent wins)
    2. A connection variable in .env.local / .env.development.local /
       .env.development / .env (dev files only; .env.production and
       .env.staging are deliberately excluded so a planning tool never
       dials production on its own)
    3. postgresql://postgres:postgres@localhost:54322/postgres (Supabase local)
    4. postgresql://postgres:postgres@localhost:5432/postgres (standard Postgres)

The discovered connection string is never echoed (it may carry a password);
only the resulting schema names are written. Falls back to migration-file
parsing when no connection is reachable.
"""

import argparse
import glob
import os
import re
import subprocess
import sys
from collections import namedtuple

# Supabase/system schemas to exclude from output
EXCLUDED_SCHEMAS = frozenset([
    "pg_catalog", "information_schema", "auth", "storage", "extensions",
    "graphql", "graphql_public", "pgbouncer", "pgsodium", "realtime",
    "vault", "_realtime", "supabase_functions", "supabase_migrations",
])

_EXCLUDED_SQL = ", ".join(f"'{s}'" for s in sorted(EXCLUDED_SCHEMAS))

# Connection-variable aliases honored when reading .env files, in priority
# order. Kept short and explicit (Claude's Discretion #2 in the design doc).
CONNECTION_VAR_ALIASES = ("DATABASE_URL", "POSTGRES_URL", "SUPABASE_DB_URL", "DIRECT_URL")

# Dev .env files searched for a connection string, in precedence order.
# .env.production / .env.staging are intentionally absent (Locked Decision #5).
ENV_FILE_PRECEDENCE = (".env.local", ".env.development.local", ".env.development", ".env")

# Extracted schema, threaded through the live and migration paths so the two
# code paths and the markdown formatter share one shape.
#   enums:       {name: "v1, v2, ..."}
#   constraints: [(table, name, definition) | (name, definition)]
#   triggers:    [(table, name, function)]
#   fks:         [(from_table, from_column, to_table)]
#   tables:      {table: [column, ...]}
#   pks:         {table: [pk_column, ...]}
#   source_note: "live" | "migration files" | "Prisma schema" | "Drizzle schema" | None
SchemaData = namedtuple(
    "SchemaData",
    "enums constraints triggers fks tables pks source_note",
)

ENUM_QUERY = """
SELECT n.nspname || '.' || t.typname, string_agg(e.enumlabel, ', ' ORDER BY e.enumsortorder)
FROM pg_enum e
JOIN pg_type t ON e.enumtypid = t.oid
JOIN pg_namespace n ON t.typnamespace = n.oid
WHERE n.nspname NOT IN ('pg_catalog', 'information_schema', 'auth', 'storage',
    'extensions', 'graphql', 'graphql_public', 'pgbouncer', 'pgsodium',
    'realtime', 'vault', '_realtime', 'supabase_functions', 'supabase_migrations')
GROUP BY n.nspname, t.typname ORDER BY n.nspname, t.typname;
"""

CONSTRAINT_QUERY = """
SELECT c.conrelid::regclass::text AS table_name,
       c.conname, pg_get_constraintdef(c.oid)
FROM pg_constraint c
JOIN pg_namespace n ON c.connamespace = n.oid
WHERE c.contype = 'c'
  AND n.nspname NOT IN ('pg_catalog', 'information_schema', 'auth', 'storage',
      'extensions', 'graphql', 'graphql_public', 'pgbouncer', 'pgsodium',
      'realtime', 'vault', '_realtime', 'supabase_functions', 'supabase_migrations')
ORDER BY table_name, c.conname;
"""

TRIGGER_QUERY = """
SELECT tg.tgrelid::regclass::text, tg.tgname, p.proname
FROM pg_trigger tg
JOIN pg_proc p ON tg.tgfoid = p.oid
JOIN pg_class rel ON tg.tgrelid = rel.oid
JOIN pg_namespace n ON rel.relnamespace = n.oid
WHERE NOT tg.tgisinternal
  AND n.nspname NOT IN ('pg_catalog', 'information_schema', 'auth', 'storage', 'extensions')
ORDER BY 1, 2;
"""

FK_QUERY = """
SELECT c.conrelid::regclass::text AS from_table,
       a.attname AS from_column,
       c.confrelid::regclass::text AS to_table
FROM pg_constraint c
JOIN pg_namespace n ON c.connamespace = n.oid
JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = ANY(c.conkey)
WHERE c.contype = 'f'
  AND n.nspname NOT IN ('pg_catalog', 'information_schema', 'auth', 'storage', 'extensions')
ORDER BY from_table, a.attname;
"""

TABLE_QUERY = f"""
SELECT (n.nspname || '.' || c.relname) AS tbl, a.attname
FROM pg_attribute a
JOIN pg_class c ON a.attrelid = c.oid
JOIN pg_namespace n ON c.relnamespace = n.oid
WHERE c.relkind = 'r' AND a.attnum > 0 AND NOT a.attisdropped
  AND n.nspname NOT IN ({_EXCLUDED_SQL})
ORDER BY tbl, a.attnum;
"""

PK_QUERY = f"""
SELECT (n.nspname || '.' || c.relname) AS tbl, a.attname
FROM pg_constraint con
JOIN pg_class c ON con.conrelid = c.oid
JOIN pg_namespace n ON c.relnamespace = n.oid
JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = ANY(con.conkey)
WHERE con.contype = 'p'
  AND n.nspname NOT IN ({_EXCLUDED_SQL})
ORDER BY tbl, a.attnum;
"""


def run_psql(conn_str, query):
    """Run a SQL query via psql and return rows as list of tuples."""
    try:
        result = subprocess.run(
            ["psql", conn_str, "-X", "--no-psqlrc", "-t", "-A", "-F", "\t", "-c", query],
            capture_output=True, text=True, timeout=10,
        )
        if result.returncode != 0:
            if result.stderr.strip():
                print(f"psql error: {result.stderr.strip()}", file=sys.stderr)
            return None
        rows = []
        for line in result.stdout.strip().split("\n"):
            if line.strip():
                rows.append(tuple(line.split("\t")))
        return rows
    except (OSError, subprocess.SubprocessError):
        return None


# --- Connection discovery (.env-aware) ---

def read_env_file(path):
    """Parse a single .env file into a KEY->VALUE dict.

    Minimal, dependency-free: handles `export ` prefixes, surrounding single
    or double quotes, comment lines, and blank lines. Anything it cannot parse
    is skipped rather than raised so a planner never crashes on a stray line.
    """
    values = {}
    try:
        with open(path, encoding="utf-8") as fh:
            for raw in fh:
                line = raw.strip()
                if not line or line.startswith("#"):
                    continue
                if line.startswith("export "):
                    line = line[len("export "):].lstrip()
                if "=" not in line:
                    continue
                key, _, val = line.partition("=")
                key = key.strip()
                val = val.strip()
                if len(val) >= 2 and val[0] == val[-1] and val[0] in ("'", '"'):
                    val = val[1:-1]
                if key:
                    values[key] = val
    except (OSError, UnicodeDecodeError):
        return {}
    return values


def connection_from_env(env_values):
    """Return the first non-blank connection alias from a parsed .env dict."""
    for alias in CONNECTION_VAR_ALIASES:
        val = env_values.get(alias, "").strip()
        if val:
            return val
    return ""


def gather_connection_candidates(search_dir=".", environ=None):
    """Build the ordered list of (source_label, connection_string) candidates.

    Pure resolution (no liveness probe), so precedence and .env handling stay
    unit-testable offline. The shell DATABASE_URL always leads; dev .env files
    follow in precedence order; localhost probes trail. A blank or missing
    value contributes nothing.
    """
    if environ is None:
        environ = os.environ
    candidates = []
    shell_url = environ.get("DATABASE_URL", "").strip()
    if shell_url:
        candidates.append(("shell DATABASE_URL", shell_url))
    for env_name in ENV_FILE_PRECEDENCE:
        path = os.path.join(search_dir, env_name)
        if not os.path.isfile(path):
            continue
        conn = connection_from_env(read_env_file(path))
        if conn:
            candidates.append((env_name, conn))
    candidates.append(("localhost:54322", "postgresql://postgres:postgres@localhost:54322/postgres"))
    candidates.append(("localhost:5432", "postgresql://postgres:postgres@localhost:5432/postgres"))
    return candidates


def detect_connection(search_dir="."):
    """Probe candidate connections in precedence order.

    Returns (connection_string, source_label) for the first that answers
    `SELECT 1`, or (None, None) if every candidate is unreachable. A malformed
    candidate or a missing psql is caught and the next candidate is tried.
    """
    for source, conn in gather_connection_candidates(search_dir):
        try:
            result = subprocess.run(
                ["psql", conn, "-c", "SELECT 1"],
                capture_output=True, text=True, timeout=5,
            )
            if result.returncode == 0:
                return conn, source
        except (OSError, subprocess.SubprocessError):
            continue
    return None, None


def filter_by_schema(rows, schema_filter, table_col=0):
    """Filter rows to only include specified schemas."""
    if not schema_filter:
        return rows
    schemas = {s.strip() for s in schema_filter.split(",")}
    filtered = []
    for row in rows:
        table = row[table_col]
        schema = table.split(".")[0] if "." in table else "public"
        if schema in schemas:
            filtered.append(row)
    return filtered


def filter_by_tables(rows, table_filter, table_col=0):
    """Filter rows to only include specified tables."""
    if not table_filter:
        return rows
    tables = {t.strip() for t in table_filter.split(",")}
    return [r for r in rows if r[table_col] in tables]


def _bare_table(name):
    """Strip a schema qualifier and surrounding quotes from a table name."""
    return name.split(".")[-1].strip('"') if name else name


def _normalize_table_rows(rows):
    """Strip the schema qualifier from the table column (col 0) of live rows.

    TABLE_QUERY / PK_QUERY always return `schema.table` (e.g. `public.users`).
    Normalizing to the bare name BEFORE `filter_by_tables` lets an unqualified
    `--tables users` match; otherwise the exact-string filter compares
    `public.users` against `users` and drops every row.
    """
    return [(_bare_table(row[0]),) + tuple(row[1:]) for row in rows]


def extract_live(conn_str, args):
    """Extract schema info from a live database."""
    enums = {}
    constraints = []
    triggers = []
    fks = []
    tables = {}
    pks = {}

    rows = run_psql(conn_str, ENUM_QUERY)
    if rows:
        filtered = filter_by_schema(rows, args.schemas)
        for row in filtered:
            if len(row) >= 2:
                name = row[0].split(".")[-1] if "." in row[0] else row[0]
                enums[name] = row[1]

    rows = run_psql(conn_str, CONSTRAINT_QUERY)
    if rows:
        for row in rows:
            if len(row) >= 3:
                constraints.append(row)
        constraints = filter_by_schema(constraints, args.schemas)
        constraints = filter_by_tables(constraints, args.tables)

    if not args.enums_only:
        rows = run_psql(conn_str, TRIGGER_QUERY)
        if rows:
            for row in rows:
                if len(row) >= 3:
                    triggers.append(row)
            triggers = filter_by_schema(triggers, args.schemas)
            triggers = filter_by_tables(triggers, args.tables)

        rows = run_psql(conn_str, FK_QUERY)
        if rows:
            for row in rows:
                if len(row) >= 3:
                    fks.append(row)
            fks = filter_by_schema(fks, args.schemas)
            fks = filter_by_tables(fks, args.tables)

        rows = run_psql(conn_str, TABLE_QUERY)
        if rows:
            # Filter by schema while still qualified, then normalize the table
            # name to bare so an unqualified --tables filter matches.
            rows = _normalize_table_rows(filter_by_schema(rows, args.schemas))
            rows = filter_by_tables(rows, args.tables)
            for row in rows:
                if len(row) >= 2:
                    cols = tables.setdefault(row[0], [])
                    if row[1] not in cols:
                        cols.append(row[1])

        rows = run_psql(conn_str, PK_QUERY)
        if rows:
            rows = _normalize_table_rows(filter_by_schema(rows, args.schemas))
            rows = filter_by_tables(rows, args.tables)
            for row in rows:
                if len(row) >= 2:
                    pk_cols = pks.setdefault(row[0], [])
                    if row[1] not in pk_cols:
                        pk_cols.append(row[1])

    return SchemaData(enums, constraints, triggers, fks, tables, pks, "live")


# --- Migration fallback parsing ---

# Items in a CREATE TABLE body that lead with one of these are table-level
# constraints, not column definitions.
CONSTRAINT_LEADERS = frozenset(
    ["PRIMARY", "FOREIGN", "CONSTRAINT", "UNIQUE", "CHECK", "EXCLUDE", "LIKE", "PERIOD"]
)


def _find_create_tables(content):
    """Yield (table_name, body) for each CREATE TABLE with balanced parens.

    A truncated or unbalanced statement (no matching close paren) is skipped,
    so a malformed migration does not abort parsing of the rest of the file.
    """
    for m in re.finditer(
        r"CREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?([A-Za-z0-9_.\"]+)\s*\(",
        content, re.IGNORECASE,
    ):
        depth = 0
        body_chars = []
        i = m.end() - 1  # position of the opening paren
        closed = False
        while i < len(content):
            ch = content[i]
            if ch == "(":
                depth += 1
                if depth == 1:
                    i += 1
                    continue  # skip the outer opening paren
            elif ch == ")":
                depth -= 1
                if depth == 0:
                    closed = True
                    break  # skip the outer closing paren
            body_chars.append(ch)
            i += 1
        if not closed:
            continue
        yield _bare_table(m.group(1)), "".join(body_chars)


def _split_top_level(body):
    """Split a CREATE TABLE body on top-level commas (ignoring nested parens)."""
    items, depth, cur = [], 0, []
    for ch in body:
        if ch == "(":
            depth += 1
            cur.append(ch)
        elif ch == ")":
            depth -= 1
            cur.append(ch)
        elif ch == "," and depth == 0:
            items.append("".join(cur))
            cur = []
        else:
            cur.append(ch)
    tail = "".join(cur)
    if tail.strip():
        items.append(tail)
    return [it.strip() for it in items if it.strip()]


def _parse_table_body(body):
    """Parse a CREATE TABLE body into (columns, pk_columns, fks).

    Scope (Domain Pitfall: SQL regex fragility): table-level and inline
    PRIMARY KEY / FOREIGN KEY ... REFERENCES, plus column identifiers. No
    attempt at full type/default/constraint-expression parsing offline.
    fks is a list of (column, referenced_table) for this table.
    """
    columns, pk_cols, fks = [], [], []
    for item in _split_top_level(body):
        tokens = item.split()
        first = tokens[0] if tokens else ""

        pk_m = re.match(
            r"(?:CONSTRAINT\s+\S+\s+)?PRIMARY\s+KEY\s*\(([^)]*)\)", item, re.IGNORECASE
        )
        if pk_m:
            for col in pk_m.group(1).split(","):
                col = col.strip().strip('"')
                if col and col not in pk_cols:
                    pk_cols.append(col)
            continue

        fk_m = re.match(
            r"(?:CONSTRAINT\s+\S+\s+)?FOREIGN\s+KEY\s*\(([^)]*)\)\s*"
            r"REFERENCES\s+([A-Za-z0-9_.\"]+)",
            item, re.IGNORECASE,
        )
        if fk_m:
            ref = _bare_table(fk_m.group(2))
            for col in fk_m.group(1).split(","):
                col = col.strip().strip('"')
                if col:
                    fks.append((col, ref))
            continue

        # Any other table-level constraint is not a column.
        if first.upper() in CONSTRAINT_LEADERS:
            continue

        col = first.strip('"')
        if not col:
            continue
        if col not in columns:
            columns.append(col)
        if re.search(r"\bPRIMARY\s+KEY\b", item, re.IGNORECASE):
            if col not in pk_cols:
                pk_cols.append(col)
        ref_m = re.search(r"\bREFERENCES\s+([A-Za-z0-9_.\"]+)", item, re.IGNORECASE)
        if ref_m:
            fks.append((col, _bare_table(ref_m.group(1))))
    return columns, pk_cols, fks


def parse_supabase_migrations(migrations_dir):
    """Parse Supabase SQL migration files for schema info.

    Returns (enums, constraints, triggers, tables, pks, fks).
    """
    enums = {}
    constraints = []
    triggers = []
    tables = {}
    pks = {}
    fks = []

    for f in sorted(glob.glob(os.path.join(migrations_dir, "*.sql"))):
        try:
            with open(f) as fh:
                content = fh.read()
        except (OSError, UnicodeDecodeError):
            continue

        # CREATE TYPE ... AS ENUM
        for m in re.finditer(
            r"CREATE\s+TYPE\s+(\S+)\s+AS\s+ENUM\s*\((.*?)\)",
            content, re.DOTALL | re.IGNORECASE,
        ):
            name = m.group(1).split(".")[-1]
            values = [v.strip().strip("'\"") for v in m.group(2).split(",") if v.strip()]
            enums[name] = values

        # ALTER TYPE ... ADD VALUE
        for m in re.finditer(
            r"ALTER\s+TYPE\s+(\S+)\s+ADD\s+VALUE.*?'([^']+)'",
            content, re.IGNORECASE,
        ):
            name = m.group(1).split(".")[-1]
            if name not in enums:
                enums[name] = []
            val = m.group(2)
            if val not in enums[name]:
                enums[name].append(val)

        # CHECK constraints
        for m in re.finditer(
            r"CONSTRAINT\s+(\w+)\s+CHECK\s*\((.*?)\)",
            content, re.DOTALL | re.IGNORECASE,
        ):
            constraints.append((m.group(1), m.group(2).strip()))

        # CREATE TRIGGER
        for m in re.finditer(
            r"CREATE\s+TRIGGER\s+(\w+).*?ON\s+(\S+).*?EXECUTE.*?(\w+)\s*\(",
            content, re.DOTALL | re.IGNORECASE,
        ):
            triggers.append((m.group(2), m.group(1), m.group(3)))

        # CREATE TABLE ... (columns, PK, FK)
        for tname, body in _find_create_tables(content):
            cols, pk_cols, tbl_fks = _parse_table_body(body)
            existing = tables.setdefault(tname, [])
            for c in cols:
                if c not in existing:
                    existing.append(c)
            if pk_cols:
                pk_existing = pks.setdefault(tname, [])
                for c in pk_cols:
                    if c not in pk_existing:
                        pk_existing.append(c)
            for col, ref in tbl_fks:
                entry = (tname, col, ref)
                if entry not in fks:
                    fks.append(entry)

    return enums, constraints, triggers, tables, pks, fks


def parse_prisma_schema(path):
    """Parse Prisma schema.prisma for enum definitions."""
    enums = {}
    try:
        with open(path) as fh:
            content = fh.read()
    except (OSError, UnicodeDecodeError):
        return enums
    for m in re.finditer(r"enum\s+(\w+)\s*\{([^}]+)\}", content):
        name = m.group(1)
        values = [v.strip() for v in m.group(2).split("\n")
                  if v.strip() and not v.strip().startswith("//")]
        enums[name] = values
    return enums


def parse_drizzle_schema(path):
    """Parse Drizzle schema.ts for pgEnum definitions."""
    enums = {}
    try:
        with open(path) as fh:
            content = fh.read()
    except (OSError, UnicodeDecodeError):
        return enums
    for m in re.finditer(r"pgEnum\s*\(\s*['\"](\w+)['\"].*?\[(.*?)\]", content):
        name = m.group(1)
        values = [v.strip().strip("'\"") for v in m.group(2).split(",") if v.strip()]
        enums[name] = values
    return enums


def detect_migrations(root="."):
    """Auto-detect migration file locations under a project root."""
    candidates = [
        ("supabase", os.path.join(root, "supabase", "migrations")),
        ("prisma", os.path.join(root, "prisma", "schema.prisma")),
        ("drizzle", os.path.join(root, "drizzle", "schema.ts")),
    ]
    for kind, pattern in candidates:
        if "*" in pattern:
            matches = glob.glob(pattern)
            if matches:
                return kind, matches[0]
        elif os.path.exists(pattern):
            return kind, pattern
    return None, None


def _supabase_schema(path):
    enums, constraints, triggers, tables, pks, fks = parse_supabase_migrations(path)
    enum_strs = {k: ", ".join(v) if isinstance(v, list) else v for k, v in enums.items()}
    return SchemaData(enum_strs, constraints, triggers, fks, tables, pks, "migration files")


def _prisma_schema(path):
    enums = parse_prisma_schema(path)
    enum_strs = {k: ", ".join(v) for k, v in enums.items()}
    return SchemaData(enum_strs, [], [], [], {}, {}, "Prisma schema")


def _drizzle_schema(path):
    enums = parse_drizzle_schema(path)
    enum_strs = {k: ", ".join(v) for k, v in enums.items()}
    return SchemaData(enum_strs, [], [], [], {}, {}, "Drizzle schema")


def extract_migrations(migrations_dir=None, root="."):
    """Extract schema info from migration files, returning a SchemaData."""
    if migrations_dir:
        if os.path.isdir(migrations_dir):
            if glob.glob(os.path.join(migrations_dir, "*.sql")):
                return _supabase_schema(migrations_dir)
        elif migrations_dir.endswith(".prisma"):
            return _prisma_schema(migrations_dir)
        elif migrations_dir.endswith(".ts"):
            return _drizzle_schema(migrations_dir)
        return SchemaData({}, [], [], [], {}, {}, None)

    kind, path = detect_migrations(root)
    if kind == "supabase":
        return _supabase_schema(path)
    elif kind == "prisma":
        return _prisma_schema(path)
    elif kind == "drizzle":
        return _drizzle_schema(path)
    return SchemaData({}, [], [], [], {}, {}, None)


# --- Output formatting ---

def _has_content(schema):
    return bool(
        schema.enums or schema.constraints or schema.triggers
        or schema.fks or schema.tables
    )


def format_markdown(schema, enums_only=False):
    """Format a SchemaData as markdown."""
    if not _has_content(schema):
        return ""

    lines = ["\n## Database Schema\n"]
    if schema.source_note == "live":
        lines.append("_Source: live database introspection_\n")
    elif schema.source_note:
        lines.append(f"_Parsed from {schema.source_note} (no live DB detected)_\n")

    if schema.enums:
        lines.append("### Enums\n")
        lines.append("| Name | Values |")
        lines.append("|------|--------|")
        for name in sorted(schema.enums.keys()):
            lines.append(f"| {name} | {schema.enums[name]} |")
        lines.append("")

    if enums_only:
        return "\n".join(lines)

    if schema.tables:
        lines.append("### Tables\n")
        lines.append("| Table | Columns | Primary Key |")
        lines.append("|-------|---------|-------------|")
        for name in sorted(schema.tables):
            cols = ", ".join(schema.tables[name])
            pk = ", ".join(schema.pks.get(name, [])) or "-"
            lines.append(f"| {name} | {cols} | {pk} |")
        lines.append("")

    if schema.constraints:
        lines.append("### CHECK Constraints\n")
        lines.append("| Table | Constraint | Definition |")
        lines.append("|-------|-----------|-----------|")
        for row in schema.constraints:
            if len(row) == 3:
                lines.append(f"| {row[0]} | {row[1]} | {row[2]} |")
            elif len(row) == 2:
                lines.append(f"| - | {row[0]} | {row[1]} |")
        lines.append("")

    if schema.triggers:
        lines.append("### Triggers\n")
        lines.append("| Table | Trigger | Function |")
        lines.append("|-------|---------|----------|")
        for row in schema.triggers:
            if len(row) >= 3:
                lines.append(f"| {row[0]} | {row[1]} | {row[2]} |")
        lines.append("")

    if schema.fks:
        lines.append("### Foreign Keys\n")
        lines.append("| From | Column | References |")
        lines.append("|------|--------|-----------|")
        for row in schema.fks:
            if len(row) >= 3:
                lines.append(f"| {row[0]} | {row[1]} | {row[2]} |")
        lines.append("")

    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description="Extract database schema as markdown")
    parser.add_argument("connection", nargs="?",
                        help="PostgreSQL connection string OR project directory to search")
    parser.add_argument("--migrations-dir", help="Path to migration files directory")
    parser.add_argument("--schemas", help="Comma-separated schema filter (e.g., attendance,public)")
    parser.add_argument("--tables", help="Comma-separated table filter")
    parser.add_argument("--enums-only", action="store_true", help="Only output enums section")
    args = parser.parse_args()

    # Discriminate the positional by shape: an existing directory is the
    # project root to search (how `fno codemap --db-schema` invokes us);
    # anything else is an explicit connection string.
    project_root = "."
    explicit_conn = None
    if args.connection:
        if os.path.isdir(args.connection):
            project_root = args.connection
        else:
            explicit_conn = args.connection

    # If --migrations-dir is explicitly provided, use it directly (skip live DB)
    if args.migrations_dir:
        schema = extract_migrations(args.migrations_dir)
        output = format_markdown(schema, args.enums_only)
        if output:
            print(output)
        return

    # Try live DB first.
    conn = explicit_conn
    source = "explicit connection argument" if explicit_conn else None
    if conn is None:
        conn, source = detect_connection(project_root)
    if conn:
        # Disclose the chosen source (never the connection string itself).
        if source:
            print(f"db-schema: connected via {source}", file=sys.stderr)
        schema = extract_live(conn, args)
        if _has_content(schema):
            output = format_markdown(schema, args.enums_only)
            if output:
                print(output)
            return

    # Fall back to auto-detected migration files under the project root.
    schema = extract_migrations(None, project_root)
    if _has_content(schema):
        output = format_markdown(schema, args.enums_only)
        if output:
            print(output)


if __name__ == "__main__":
    main()
