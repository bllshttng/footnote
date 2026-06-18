#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

PASS=0
FAIL=0

pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

# --- Test 1: Migration fallback ---

echo "Testing migration fallback..."
TMPDIR=$(mktemp -d)
cat > "$TMPDIR/001_test.sql" << 'SQL'
CREATE TYPE test_status AS ENUM ('active', 'inactive', 'pending');
ALTER TYPE test_status ADD VALUE IF NOT EXISTS 'archived';
CREATE TABLE test_items (
  id uuid PRIMARY KEY,
  name text NOT NULL,
  status test_status NOT NULL,
  CONSTRAINT valid_name CHECK (name IS NOT NULL AND length(name) > 0)
);
CREATE TRIGGER prevent_delete BEFORE DELETE ON test_items
  FOR EACH ROW EXECUTE FUNCTION raise_exception();
SQL

OUTPUT=$(python3 "$SCRIPT_DIR/db-schema.py" --migrations-dir "$TMPDIR" 2>/dev/null)

echo "$OUTPUT" | grep -q "## Database Schema" && pass "Header present" || fail "Missing header"
echo "$OUTPUT" | grep -q "test_status" && pass "Enum parsed" || fail "Enum not parsed"
echo "$OUTPUT" | grep -q "archived" && pass "ALTER TYPE ADD VALUE parsed" || fail "ALTER TYPE ADD VALUE not parsed"
echo "$OUTPUT" | grep -q "valid_name" && pass "Constraint parsed" || fail "Constraint not parsed"
echo "$OUTPUT" | grep -q "prevent_delete" && pass "Trigger parsed" || fail "Trigger not parsed"
echo "$OUTPUT" | grep -q "migration files" && pass "Source note present" || fail "Missing source note"
echo "$OUTPUT" | grep -q "### Tables" && pass "Tables section present" || fail "Missing tables section"
echo "$OUTPUT" | grep -q "id, name, status" && pass "Columns parsed in order" || fail "Columns not parsed"
echo "$OUTPUT" | grep -q "| id, name, status | id |" && pass "Primary key parsed" || fail "Primary key not parsed"

rm -rf "$TMPDIR"

# --- Test 2: Enums-only flag ---

echo ""
echo "Testing --enums-only flag..."
TMPDIR=$(mktemp -d)
cat > "$TMPDIR/001_test.sql" << 'SQL'
CREATE TYPE color AS ENUM ('red', 'blue', 'green');
CREATE TABLE items (
  id uuid PRIMARY KEY,
  CONSTRAINT positive_qty CHECK (qty > 0)
);
SQL

OUTPUT=$(python3 "$SCRIPT_DIR/db-schema.py" --migrations-dir "$TMPDIR" --enums-only 2>/dev/null)

echo "$OUTPUT" | grep -q "color" && pass "Enum in output" || fail "Enum missing"
echo "$OUTPUT" | grep -q "CHECK" && fail "Constraints should be excluded" || pass "Constraints excluded"

rm -rf "$TMPDIR"

# --- Test 3: Graceful fallback (no DB, no migrations) ---

echo ""
echo "Testing graceful fallback..."
OUTPUT=$(python3 "$SCRIPT_DIR/db-schema.py" "postgresql://nobody:nobody@localhost:99999/nope" --migrations-dir /nonexistent 2>/dev/null)
[ -z "$OUTPUT" ] && pass "Empty output when nothing available" || fail "Should produce empty output"

# --- Test 4: Multiple migration files processed in order ---

echo ""
echo "Testing multi-file migration parsing..."
TMPDIR=$(mktemp -d)
cat > "$TMPDIR/001_initial.sql" << 'SQL'
CREATE TYPE role AS ENUM ('admin', 'user');
SQL
cat > "$TMPDIR/002_add_editor.sql" << 'SQL'
ALTER TYPE role ADD VALUE IF NOT EXISTS 'editor';
SQL

OUTPUT=$(python3 "$SCRIPT_DIR/db-schema.py" --migrations-dir "$TMPDIR" 2>/dev/null)

echo "$OUTPUT" | grep -q "admin" && pass "Initial enum values present" || fail "Missing initial values"
echo "$OUTPUT" | grep -q "editor" && pass "Added enum value present" || fail "Missing added value"

rm -rf "$TMPDIR"

# --- Test 5: Live DB (optional - only if local Supabase running) ---

echo ""
echo "Testing live DB (optional)..."
DB_URL="${1:-postgresql://postgres:postgres@localhost:54322/postgres}"
# Capture ONCE: each invocation opens its own connection, so a second
# invocation can transiently fail under load. Gate every content assertion on
# this single capture, and SKIP (don't FAIL) when no live DB answered.
OUTPUT=$(python3 "$SCRIPT_DIR/db-schema.py" "$DB_URL" 2>/dev/null)
if echo "$OUTPUT" | grep -q "## Database Schema"; then
    pass "Live DB extraction works"
    # A live section carries at least one detail subsection. Enums and Tables
    # both trigger the section now, so don't couple to enums specifically
    # (an enum-free DB is legitimate).
    echo "$OUTPUT" | grep -qE "### (Enums|Tables)" && pass "Detail subsection present" || fail "Missing detail subsection"
    echo "$OUTPUT" | grep -q "_Source: live database introspection_" && pass "Live source disclosed" || fail "Missing live source note"
    echo "$OUTPUT" | grep -q "pg_catalog" && fail "Internal schema leaked" || pass "No internal schema leak"
    echo "$OUTPUT" | grep -qE "^.*auth\." && fail "Auth schema leaked" || pass "No auth schema leak"
else
    echo "  SKIP: No live DB at $DB_URL"
fi

# --- Test 6: Foreign keys parsed from migrations (table-level + inline) ---

echo ""
echo "Testing foreign-key parsing..."
TMPDIR=$(mktemp -d)
cat > "$TMPDIR/001_fk.sql" << 'SQL'
CREATE TABLE authors (
  id uuid PRIMARY KEY,
  name text
);
CREATE TABLE books (
  id uuid PRIMARY KEY,
  author_id uuid REFERENCES authors(id),
  editor_id uuid,
  FOREIGN KEY (editor_id) REFERENCES authors (id)
);
SQL

OUTPUT=$(python3 "$SCRIPT_DIR/db-schema.py" --migrations-dir "$TMPDIR" 2>/dev/null)

echo "$OUTPUT" | grep -q "### Foreign Keys" && pass "Foreign Keys section present" || fail "Missing FK section"
echo "$OUTPUT" | grep -q "| books | author_id | authors |" && pass "Inline FK parsed" || fail "Inline FK not parsed"
echo "$OUTPUT" | grep -q "| books | editor_id | authors |" && pass "Table-level FK parsed" || fail "Table-level FK not parsed"

rm -rf "$TMPDIR"

# Note: the directory-positional + .env-discovery + migration-fallback chain
# depends on live connectivity (a reachable localhost DB legitimately wins over
# an unreachable .env candidate), so it is not deterministic in a bash harness.
# Its deterministic pieces - candidate ordering, .env parsing, prod exclusion,
# and migration auto-detection under a project root - are covered by
# cli/tests/unit/test_db_schema_grounding.py.

# --- Summary ---

echo ""
echo "================================"
echo "Results: $PASS passed, $FAIL failed"
echo "================================"

[ "$FAIL" -eq 0 ] && exit 0 || exit 1
