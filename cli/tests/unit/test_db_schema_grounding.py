"""Unit tests for the schema-grounded-discovery additions to db-schema.py.

Covers US2 (.env connection discovery + precedence) and US4 (migration-parser
extension to tables + columns + PRIMARY KEY + FOREIGN KEY). The live-DB path is
not exercised here (no Postgres in CI); the parser and the pure connection-
candidate resolver are the offline-testable core.

db-schema.py is a dash-named standalone script under scripts/codemap/, so it is
loaded via importlib rather than imported as a package module.
"""
from __future__ import annotations

import importlib.util
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]
DB_SCHEMA_PATH = REPO_ROOT / "scripts" / "codemap" / "db-schema.py"


def _load_db_schema():
    spec = importlib.util.spec_from_file_location("db_schema_under_test", DB_SCHEMA_PATH)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


db = _load_db_schema()


# --- US2: connection candidate resolution (.env discovery + precedence) ---


def test_shell_database_url_wins_over_env_local(tmp_path):
    """AC2-FR: an explicit shell DATABASE_URL outranks .env.local."""
    (tmp_path / ".env.local").write_text("DATABASE_URL=postgresql://local/db\n")
    cands = db.gather_connection_candidates(str(tmp_path), {"DATABASE_URL": "postgresql://shell/db"})
    assert cands[0] == ("shell DATABASE_URL", "postgresql://shell/db")
    # .env.local is still present as a lower-precedence candidate.
    assert (".env.local", "postgresql://local/db") in cands
    assert cands.index(("shell DATABASE_URL", "postgresql://shell/db")) < cands.index(
        (".env.local", "postgresql://local/db")
    )


def test_env_local_used_when_no_shell(tmp_path):
    """AC2-HP: with no shell DATABASE_URL, the .env.local value is resolved."""
    (tmp_path / ".env.local").write_text('export DATABASE_URL="postgresql://local/db"\n')
    cands = db.gather_connection_candidates(str(tmp_path), {})
    assert cands[0] == (".env.local", "postgresql://local/db")
    sources = [s for s, _ in cands]
    assert sources.index(".env.local") < sources.index("localhost:54322")


def test_production_env_is_never_a_candidate(tmp_path):
    """AC2-EDGE: .env.production is excluded from auto-discovery."""
    (tmp_path / ".env.production").write_text("DATABASE_URL=postgresql://prod/db\n")
    (tmp_path / ".env.staging").write_text("DATABASE_URL=postgresql://staging/db\n")
    cands = db.gather_connection_candidates(str(tmp_path), {})
    conns = [c for _, c in cands]
    assert "postgresql://prod/db" not in conns
    assert "postgresql://staging/db" not in conns
    # Only the localhost probes remain.
    assert [s for s, _ in cands] == ["localhost:54322", "localhost:5432"]


def test_blank_env_value_is_skipped(tmp_path):
    """Boundary: a present-but-blank connection var falls through."""
    (tmp_path / ".env.local").write_text("DATABASE_URL=\n")
    cands = db.gather_connection_candidates(str(tmp_path), {})
    assert ".env.local" not in [s for s, _ in cands]


def test_connection_alias_honored(tmp_path):
    """A non-DATABASE_URL alias (POSTGRES_URL) is recognized."""
    (tmp_path / ".env").write_text("POSTGRES_URL=postgresql://aliased/db\n")
    cands = db.gather_connection_candidates(str(tmp_path), {})
    assert (".env", "postgresql://aliased/db") in cands


def test_env_file_precedence_local_before_plain(tmp_path):
    (tmp_path / ".env").write_text("DATABASE_URL=postgresql://plain/db\n")
    (tmp_path / ".env.local").write_text("DATABASE_URL=postgresql://local/db\n")
    sources = [s for s, _ in db.gather_connection_candidates(str(tmp_path), {})]
    assert sources.index(".env.local") < sources.index(".env")


def test_env_parser_handles_quotes_and_comments(tmp_path):
    (tmp_path / ".env.local").write_text(
        "# a comment\n"
        "OTHER=ignored\n"
        "export DATABASE_URL='postgresql://quoted/db'\n"
    )
    cands = db.gather_connection_candidates(str(tmp_path), {})
    assert (".env.local", "postgresql://quoted/db") in cands


# --- US4: migration parser extension (tables + columns + PK + FK) ---


def test_tables_columns_and_primary_keys(tmp_path):
    """AC4-HP: table names, columns, and primary keys are parsed."""
    (tmp_path / "001.sql").write_text(
        """
        CREATE TABLE users (
          id uuid PRIMARY KEY,
          email text NOT NULL,
          name text
        );
        CREATE TABLE orders (
          id uuid,
          user_id uuid,
          total numeric(10,2),
          PRIMARY KEY (id),
          FOREIGN KEY (user_id) REFERENCES users (id)
        );
        """
    )
    sd = db.extract_migrations(str(tmp_path))
    assert set(sd.tables["users"]) >= {"id", "email", "name"}
    assert sd.pks["users"] == ["id"]
    assert "total" in sd.tables["orders"]
    assert sd.pks["orders"] == ["id"]
    # FK survives even though numeric(10,2) carries an inner paren.
    assert ("orders", "user_id", "users") in sd.fks


def test_inline_foreign_key_and_pk(tmp_path):
    (tmp_path / "001.sql").write_text(
        """
        CREATE TABLE comments (
          id uuid PRIMARY KEY,
          post_id uuid REFERENCES posts(id)
        );
        """
    )
    sd = db.extract_migrations(str(tmp_path))
    assert sd.pks["comments"] == ["id"]
    assert ("comments", "post_id", "posts") in sd.fks


def test_check_constraint_not_treated_as_column(tmp_path):
    (tmp_path / "001.sql").write_text(
        """
        CREATE TABLE items (
          id uuid PRIMARY KEY,
          name text,
          CONSTRAINT valid_name CHECK (name IS NOT NULL AND length(name) > 0)
        );
        """
    )
    sd = db.extract_migrations(str(tmp_path))
    assert "valid_name" not in sd.tables["items"]
    assert set(sd.tables["items"]) == {"id", "name"}


def test_malformed_ddl_tolerated(tmp_path):
    """AC4-ERR: a truncated CREATE TABLE does not crash the parser."""
    (tmp_path / "001.sql").write_text(
        """
        CREATE TYPE mood AS ENUM ('happy', 'sad');
        CREATE TABLE broken (
          id uuid PRIMARY KEY,
        """
    )
    sd = db.extract_migrations(str(tmp_path))
    assert "mood" in sd.enums  # the valid enum still parsed
    assert "broken" not in sd.tables  # the broken table was skipped


def test_no_create_table_yields_empty_tables(tmp_path):
    """AC4-EDGE: a migrations dir with no CREATE TABLE emits an empty table set."""
    (tmp_path / "001.sql").write_text("CREATE TYPE color AS ENUM ('r', 'g');\n")
    sd = db.extract_migrations(str(tmp_path))
    assert sd.tables == {}
    assert "color" in sd.enums


def test_parsed_source_note(tmp_path):
    (tmp_path / "001.sql").write_text("CREATE TABLE t (id uuid PRIMARY KEY);\n")
    sd = db.extract_migrations(str(tmp_path))
    assert sd.source_note == "migration files"


def test_autodetect_migrations_under_project_root(tmp_path):
    """The directory-positional path resolves supabase/migrations under the
    given root, not the process CWD (how `fno codemap --db-schema` invokes it)."""
    mig = tmp_path / "supabase" / "migrations"
    mig.mkdir(parents=True)
    (mig / "001_init.sql").write_text("CREATE TABLE gadgets (id uuid PRIMARY KEY, label text);\n")
    sd = db.extract_migrations(None, root=str(tmp_path))
    assert "gadgets" in sd.tables
    assert "label" in sd.tables["gadgets"]
    assert sd.source_note == "migration files"


# --- Output formatting ---


def test_markdown_includes_tables_section(tmp_path):
    (tmp_path / "001.sql").write_text("CREATE TABLE widgets (id uuid PRIMARY KEY, label text);\n")
    sd = db.extract_migrations(str(tmp_path))
    md = db.format_markdown(sd)
    assert "### Tables" in md
    assert "widgets" in md
    assert "label" in md


def test_parsed_markdown_carries_no_live_note(tmp_path):
    (tmp_path / "001.sql").write_text("CREATE TABLE t (id uuid PRIMARY KEY);\n")
    md = db.format_markdown(db.extract_migrations(str(tmp_path)))
    assert "Parsed from migration files (no live DB detected)" in md


def test_live_table_rows_normalized_before_filter():
    """A live TABLE_QUERY row is `schema.table`; an unqualified --tables filter
    must still match after schema-qualifier normalization (codex P2 on #509)."""
    rows = [("public.users", "id"), ("public.orders", "total")]
    normalized = db._normalize_table_rows(rows)
    assert ("users", "id") in normalized
    assert ("orders", "total") in normalized
    # An unqualified table filter now matches the normalized bare name.
    assert db.filter_by_tables(normalized, "users") == [("users", "id")]


def test_live_source_note_discloses_live_without_secret():
    """AC2-UI: a live section says 'live' and never carries a connection string."""
    sd = db.SchemaData(
        enums={"mood": "happy, sad"},
        constraints=[],
        triggers=[],
        fks=[],
        tables={},
        pks={},
        source_note="live",
    )
    md = db.format_markdown(sd)
    assert "live" in md.lower()
    assert "no live DB detected" not in md
