import pathlib

p = pathlib.Path("tests/migrations/test_county_stats_data_api_grants.py")
t = p.read_text(encoding="utf-8")

# Finding 3: sys is unused (F401 is per-file-ignored under tests/, so ruff misses it).
t = t.replace("import pathlib\nimport re\nimport sys\n", "import pathlib\n")

old_fixture = '''@pytest.fixture(scope="module")
def migration_code(migration_sql: str) -> str:
    """Migration text with line comments and string bodies blanked.

    An assertion about "the SQL does not mention X" must not be satisfiable or
    defeatable by a comment, so strip both before matching.
    """
    without_line_comments = re.sub(r"--[^\\n]*", "", migration_sql)
    without_block_comments = re.sub(r"/\\*.*?\\*/", "", without_line_comments, flags=re.S)
    return re.sub(r"'[^']*'", "''", without_block_comments)
'''

new_fixture = '''def _blank_noncode(sql: str) -> str:
    """Blank comments and string bodies to spaces, preserving every newline.

    Line shape is load-bearing here: the assertions below scan
    ``splitlines()`` and require that a GRANT and its privilege sit on the
    same physical line. So each removed character becomes a space and each
    newline survives.

    A regex cannot do this. ``re.sub(r"/\\*.*?\\*/", "", sql, flags=re.S)``
    deletes the newlines inside the span, so a block comment spanning two
    lines merges the statements around it onto one line, and a line-scoped
    assertion then passes over a statement it never inspected. The same
    applies to blanking a multi-line string body. Stripping line comments
    before string bodies has a second failure in the other direction: the
    ``--`` inside ``'range 1--2'`` eats the rest of that line and leaves an
    unbalanced quote that misaligns every later string. Both produce a false
    PASS, which is the one outcome this fixture exists to prevent.
    """
    out: list[str] = []
    i, n = 0, len(sql)
    while i < n:
        if sql.startswith("--", i):
            while i < n and sql[i] != "\\n":
                out.append(" ")
                i += 1
        elif sql.startswith("/*", i):
            end = sql.find("*/", i + 2)
            end = n if end == -1 else end + 2
            out.extend("\\n" if sql[j] == "\\n" else " " for j in range(i, end))
            i = end
        elif sql[i] == "'":
            out.append("'")
            i += 1
            while i < n and sql[i] != "'":
                out.append("\\n" if sql[i] == "\\n" else " ")
                i += 1
            if i < n and sql.startswith("''", i):
                # Escaped quote inside the literal: still inside the string.
                out.append("  ")
                i += 2
                continue
            if i < n:
                out.append("'")
                i += 1
        else:
            out.append(sql[i])
            i += 1
    return "".join(out)


@pytest.fixture(scope="module")
def migration_code(migration_sql: str) -> str:
    """Migration text with comments and string bodies blanked.

    An assertion about "the SQL does not mention X" must not be satisfiable or
    defeatable by a comment, so blank both before matching.
    """
    return _blank_noncode(migration_sql)
'''

assert old_fixture in t, "fixture block not found verbatim"
t = t.replace(old_fixture, new_fixture)

# Finding 4: the deployment contract this message cites was deleted in this PR.
t = t.replace(
    'f"{table}: expected a wholesale REVOKE ALL. An enumerated privilege list is "\n'
    '        f"refused by the deployment contract and also forgets REFERENCES/TRIGGER."',
    'f"{table}: expected a wholesale REVOKE ALL. An enumerated privilege list "\n'
    '        f"forgets REFERENCES and TRIGGER."',
)

# The sidecar note in the module docstring is fine, but "re" is still needed? check later.
p.write_text(t, encoding="utf-8")
print("patched")
print("re still used:", "re." in t)
