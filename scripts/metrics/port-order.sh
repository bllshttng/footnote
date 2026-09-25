#!/usr/bin/env bash
# Port order: rank cli/src/fno modules for the Python-to-Rust port, outside-in.
# Prints the ranked markdown table on stdout. The table and the ranking rule
# live in docs/architecture/rust-python-seam.md (Sequencing -> Port order);
# the counting rule this script implements is stated in words there.
#
# Usage:
#   scripts/metrics/port-order.sh            ranked markdown table on stdout
#   scripts/metrics/port-order.sh --edges    module import edge list, TSV from<TAB>to
#   scripts/metrics/port-order.sh --selftest fixture-tree assertions, PASS or fail
#
# Env:
#   PORT_ORDER_SINCE            git since-spec for churn (default "30 days")
#   PORT_ORDER_TRANSCRIPTS_DIR  use signal source (default ~/.claude/projects)
#   PORT_ORDER_EVENTS           event journal (default ~/.fno/events.jsonl)

set -euo pipefail

SINCE="${PORT_ORDER_SINCE:-30 days}"
TRANSCRIPTS_DIR="${PORT_ORDER_TRANSCRIPTS_DIR:-$HOME/.claude/projects}"
EVENTS="${PORT_ORDER_EVENTS:-$HOME/.fno/events.jsonl}"

# Single files measured as their own row: path under the package, then the
# label the table shows. The Fixes-line parser of the first entry is already
# Rust; what remains is the rest of the file.
EXTRA_FILE_LABEL="pr/closure.py remainder (branch resolution, claim binding, PR context fetch; Fixes parser already Rust)"

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

die() { echo "port-order: $*" >&2; exit 1; }

PKG="cli/src/fno"
ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || ROOT=""

# ----------------------------------------------------------- import scanning

# Read "path<TAB>statement" lines on stdin, write file-level import edges
# "src_module<TAB>path<TAB>dst_module". Absolute fno.A[.B] names A, a bare
# "from fno import X" names X, relative dots climb from the file's own
# subpackage. Unresolvable and intra-module imports drop out.
scan_edges() {
    awk -F'\t' -v pkg="$PKG" '
        {
            path = $1; stmt = $2
            subpath = path
            sub("^" pkg "/", "", subpath)
            n = split(subpath, segs, "/")
            if (n > 1) { src = segs[1]; depth = n - 1 }
            else { src = subpath; sub(/\.py$/, "", src); depth = 0 }
            if (src == "") next
            target = ""
            if (match(stmt, /^[ \t]*from[ \t]+([^ \t]+)[ \t]+import/)) {
                spec = substr(stmt, RSTART, RLENGTH)
                sub(/^[ \t]*from[ \t]+/, "", spec); sub(/[ \t]+import$/, "", spec)
                if (spec ~ /^\./) {
                    tmp = spec; ndots = 0
                    while (substr(tmp, 1, 1) == ".") { ndots++; tmp = substr(tmp, 2) }
                    first = tmp; sub(/[ \t].*$/, "", first); sub(/\..*$/, "", first)
                    keep = depth - (ndots - 1)
                    if (keep < 0) next
                    resolved = ""
                    for (i = 1; i <= keep; i++) resolved = resolved segs[i] "/"
                    if (first != "") resolved = resolved first
                    sub(/\/$/, "", resolved)
                    split(resolved, rp, "/")
                    target = rp[1]
                } else {
                    m = split(spec, sp, ".")
                    if (sp[1] != "fno") next
                    if (m == 1) {
                        line = stmt
                        sub(/^[ \t]*from[ \t]+fno[ \t]+import[ \t]*/, "", line)
                        split(line, np, /[(),\t ]/)
                        target = np[1]
                    } else {
                        target = sp[2]
                    }
                }
            } else if (match(stmt, /^[ \t]*import[ \t]+fno\./)) {
                line = stmt
                sub(/^[ \t]*import[ \t]+fno\./, "", line)
                split(line, ip, /[^A-Za-z0-9_]/)
                target = ip[1]
            }
            if (target == "" || target == src) next
            print src "\t" path "\t" target
        }
    '
}

# Collect file-level edges from the checkout: one grep per import spelling.
compute_edges() {
    local root="$1" out="$2"
    local raw="$workdir/raw.tsv"
    : > "$raw"
    { ( cd "$root" && grep -RnE '^[[:space:]]*from[[:space:]]+(\.+|fno([.[:space:]]|$))' "$PKG" --include='*.py' || true
        cd "$root" && grep -RnE '^[[:space:]]*import[[:space:]]+fno\.' "$PKG" --include='*.py' || true ) } \
        | awk -F: '{ stmt = $0; sub(/^[^:]*:[0-9]+:/, "", stmt); print $1 "\t" stmt }' > "$raw"
    scan_edges < "$raw" > "$out"
    sort -u -o "$out" "$out"
}

# ------------------------------------------------------------------- metrics

lines_for() { # module -> summed .py lines
    local mod="$1"
    find "$ROOT/$PKG" -name '*.py' -type f | while IFS= read -r f; do
        local rel="${f#"$ROOT/$PKG"/}"
        case "$rel" in
            "$mod"/*|"$mod".py) wc -l < "$f" ;;
        esac
    done | awk '{ s += $1 } END { print s + 0 }'
}

fanin_for() { # edges-file module -> distinct importing files, self-imports dropped
    local f="$1" m="$2"
    awk -F'\t' -v m="$m" '$3 == m && $1 != m { f[$2] } END { c = 0; for (k in f) c++; print c + 0 }' "$f"
}

crossing_for() { # edges-file module -> distinct neighbor modules
    local f="$1" m="$2"
    awk -F'\t' -v m="$m" '$1 == m || $3 == m { n[$1]; n[$3] } END { delete n[m]; c = 0; for (k in n) c++; print c + 0 }' "$f"
}

churn_commits_all() { # -> workdir file "module<TAB>commits", one git log pass
    local out="$1"
    git -C "$ROOT" log --since="$SINCE" --name-only --pretty=format:'@c@' origin/main -- "$PKG" 2>/dev/null \
        | awk -v pkg="$PKG" '
            /^@c@$/ { n++; next }
            NF && $0 ~ ("^" pkg "/") { print $0 "\t" n }
            END { }' \
        | awk -F'\t' -v pkg="$PKG" '
            {
                rel = $1; sub("^" pkg "/", "", rel)
                mod = (rel ~ /\//) ? substr(rel, 1, index(rel, "/") - 1) : substr(rel, 1, length(rel) - 3)
                commits[mod ":" $2] = 1
            }
            END { for (k in commits) { split(k, p, ":"); c[p[1]]++ } for (m in c) print m "\t" c[m] }' \
        | sort > "$out" || true
    touch "$out"
}

churn_prs_all() { # -> workdir file "module<TAB>prs", one gh sweep; "-" when gh absent
    local out="$1"
    if ! command -v gh >/dev/null 2>&1; then printf '%s\n' "-ALL-" > "$out"; return 0; fi
    local nums
    nums=$(gh pr list --state open --limit 200 --json number --jq '.[].number' 2>/dev/null || true)
    : > "$out"
    [[ -z "$nums" ]] && { printf '%s\n' "-NONE-" > "$out"; return 0; }
    local tmp="$workdir/prfiles.tsv"
    : > "$tmp"
    printf '%s\n' "$nums" | while IFS= read -r n; do
        gh pr diff "$n" --name-only 2>/dev/null | awk -v n="$n" -v pkg="$PKG" '
            $0 ~ ("^" pkg "/") { rel = $0; sub("^" pkg "/", "", rel)
                mod = (rel ~ /\//) ? substr(rel, 1, index(rel, "/") - 1) : substr(rel, 1, length(rel) - 3)
                print mod "\t" n }' >> "$tmp" || true
    done
    awk -F'\t' '!seen[$0]++ { c[$1]++ } END { for (m in c) print m "\t" c[m] }' "$tmp" | sort > "$out" || true
    touch "$out"
}

use_all() { # -> workdir file "module<TAB>count" from transcripts + event journal
    local out="$1"
    local tmp="$workdir/use_raw.tsv"
    : > "$tmp"
    if [[ -d "$TRANSCRIPTS_DIR" ]]; then
        find "$TRANSCRIPTS_DIR" -type f -name '*.jsonl' -mtime -30 -exec grep -hoE '\bfno [a-z][a-z-]*' {} + 2>/dev/null \
            | awk '{ print $2 "\tTRANSCRIPT" }' >> "$tmp" || true
    fi
    if [[ -f "$EVENTS" ]]; then
        grep -o '"verb":"[a-z_-]*"' "$EVENTS" 2>/dev/null | sed 's/"verb":"//; s/"$//' \
            | awk '{ print $0 "\tEVENT" }' >> "$tmp" || true
    fi
    awk -F'\t' '{ c[$1]++ } END { for (m in c) print m "\t" c[m] }' "$tmp" | sort > "$out" || true
    touch "$out"
}

lookup() { # tsv-file key -> value, "" when absent
    awk -F'\t' -v k="$2" '$1 == k { print $2; exit }' "$1"
}

module_list() {
    find "$ROOT/$PKG" -name '*.py' -type f | while IFS= read -r f; do
        local rel="${f#"$ROOT/$PKG"/}"
        if [[ "$rel" == */* ]]; then printf '%s\n' "${rel%%/*}"; else printf '%s\n' "${rel%.py}"; fi
    done | sort -u
}

lines_all() { # -> workdir file "module<TAB>lines", one pass
    local out="$1"
    find "$ROOT/$PKG" -name '*.py' -type f -print0 2>/dev/null | xargs -0 wc -l 2>/dev/null \
        | awk -v base="$ROOT/$PKG/" -v out="$out" '
            NF >= 2 && index($2, base) == 1 {
                rel = substr($2, length(base) + 1)
                slash = index(rel, "/")
                mod = (slash > 0) ? substr(rel, 1, slash - 1) : rel
                sub(/\.py$/, "", mod)
                l[mod] += $1
            }
            END { for (m in l) print m "\t" l[m] }' | sort > "$out" || true
    touch "$out"
}

# ------------------------------------------------------------------- ranking

emit_table() {
    local edges="$workdir/edges.tsv"
    compute_edges "$ROOT" "$edges"
    local lines_f="$workdir/lines.tsv" churn_f="$workdir/churn.tsv" prs_f="$workdir/prs.tsv" use_f="$workdir/use.tsv"
    lines_all "$lines_f"
    churn_commits_all "$churn_f"
    churn_prs_all "$prs_f"
    use_all "$use_f"

    local rows="$workdir/rows.tsv"
    : > "$rows"

    while IFS= read -r mod; do
        local lines fin prs use cross total leaf churn_show cch
        lines=$(lookup "$lines_f" "$mod"); lines=${lines:-0}
        fin=$(fanin_for "$edges" "$mod")
        cch=$(lookup "$churn_f" "$mod"); cch=${cch:-0}
        prs=$(lookup "$prs_f" "$mod")
        use=$(lookup "$use_f" "$mod"); use=${use:-0}
        cross=$(crossing_for "$edges" "$mod")
        if [[ "$prs" == "-" || "$prs" == "" ]]; then
            total=$cch
            churn_show="$cch/-"
        else
            total=$((cch + prs))
            churn_show="$cch/$prs"
        fi
        if [[ "$fin" == "0" ]]; then leaf=0; else leaf=1; fi
        printf '%d\t%s\t%s\t%d\t%s\t%s\t%d\t%s\n' "$leaf" "$mod" "$lines" "$fin" "$total" "$use" "$cross" "$churn_show" >> "$rows"
    done < <(module_list)

    # extra single-file rows
    local frel="$PKG/pr/closure.py"
    if [[ -f "$ROOT/$frel" ]]; then
        local flines ffin fcch fcross
        flines=$(wc -l < "$ROOT/$frel" | tr -d ' ')
        ffin=$( ( cd "$ROOT" && grep -RlE 'from[[:space:]]+fno\.pr\.closure|from[[:space:]]+\.closure[[:space:]]+import|pr\.closure' "$PKG" --include='*.py' || true ) | sort -u | wc -l | tr -d ' ' )
        fcch=$(git -C "$ROOT" log --since="$SINCE" --name-only --pretty=format: origin/main -- "$frel" 2>/dev/null | grep -c . || true)
        fcross=$ffin
        local fleaf=1
        [[ "$ffin" == "0" ]] && fleaf=0
        printf '%d\t%s\t%s\t%s\t%s\t%s\t%s\t%s/-\n' "$fleaf" "$EXTRA_FILE_LABEL" "$flines" "$ffin" "$fcch" "0" "$fcross" "$fcch" >> "$rows"
    fi

    local sorted="$workdir/sorted.tsv"
    sort -t$'\t' -k1,1 -k4,4n -k5,5n -k6,6n -k2,2 -o "$sorted" "$rows"

    printf '| # | module | lines | fan-in | churn (30d, commits/prs) | use | crossing edges |\n'
    printf '|---|---|---:|---:|---|---:|---:|\n'
    local i=0 leaf mod lines fin total use cross churn
    while IFS=$'\t' read -r leaf mod lines fin total use cross churn; do
        i=$((i + 1))
        local tag=""
        [[ "$leaf" == "0" ]] && tag=" (leaf)"
        printf '| %d | %s%s | %s | %s | %s | %s | %s |\n' "$i" "$mod" "$tag" "$lines" "$fin" "$churn" "$use" "$cross"
    done < "$sorted"
}

# ------------------------------------------------------------------ selftest

check() { # desc expect got
    if [[ "$2" != "$3" ]]; then echo "FAIL: $1: expected [$2], got [$3]"; return 1
    else echo "ok: $1"; return 0; fi
}

selftest() {
    local t fails=0
    t=$(mktemp -d)
    mkdir -p "$t/$PKG/aaa" "$t/$PKG/bbb"
    printf 'from fno.bbb.core import thing\n' > "$t/$PKG/aaa/__init__.py"
    printf 'x = 1\n' > "$t/$PKG/bbb/core.py"
    printf 'from .core import thing\n' > "$t/$PKG/bbb/__init__.py"
    printf 'y = 2\n' > "$t/$PKG/ccc.py"
    printf 'from fno.aaa import helper\n' > "$t/$PKG/ddd.py"

    local saved_root="$ROOT" saved_wd="$workdir"
    ROOT="$t"
    workdir="$t/.wd"; mkdir -p "$workdir"
    local edges="$workdir/edges.tsv"
    compute_edges "$t" "$edges"

        check "edges: aaa->bbb and ddd->aaa only" 2 "$(wc -l < "$edges" | tr -d ' ')" || fails=$((fails + 1))
        check "fan-in aaa (importer ddd.py)" 1 "$(fanin_for "$edges" aaa)" || fails=$((fails + 1))
        check "fan-in bbb (importer aaa, relative self-drop)" 1 "$(fanin_for "$edges" bbb)" || fails=$((fails + 1))
        check "fan-in ccc (leaf)" 0 "$(fanin_for "$edges" ccc)" || fails=$((fails + 1))
        check "crossing degree ccc" 0 "$(crossing_for "$edges" ccc)" || fails=$((fails + 1))
        check "crossing degree bbb" 1 "$(crossing_for "$edges" bbb)" || fails=$((fails + 1))
        check "lines bbb sums its files" 2 "$(lines_for bbb)" || fails=$((fails + 1))

    ROOT="$saved_root"; workdir="$saved_wd"
    rm -rf "$t"
    if [[ "$fails" != "0" ]]; then die "selftest failed"; fi
    # check() failures already printed; a silent pass must stay loud:
    echo "PASS"
}

# ---------------------------------------------------------------------- main

case "${1:-}" in
    --selftest)
        selftest
        ;;
    --edges)
        [[ -d "$ROOT/$PKG" ]] || die "run from a checkout with $PKG"
        edges_out="$workdir/edges.tsv"
        compute_edges "$ROOT" "$edges_out"
        cut -f1,3 "$edges_out" | sort -u
        ;;
    "")
        [[ -d "$ROOT/$PKG" ]] || die "run from a checkout with $PKG"
        emit_table
        ;;
    *) die "unknown flag: $1 (use --edges or --selftest)" ;;
esac
