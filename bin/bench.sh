#!/usr/bin/env bash
# CI benchmark-regression harness (design note DESIGN.html §5.0 milestone D, item T5).
#
# What it does, end to end:
#   1. Builds a small, deterministically-generated warehouse from scratch (no committed
#      corpus, no network) — a few hundred files, a few hundred signed parcels, two
#      diverging pallets. "Deterministic" means the *shape* of the corpus (which files,
#      which commits touch what) is a pure function of the --seed/--files/--parcels
#      knobs, not of wall-clock time or the environment, so numbers are comparable
#      run-to-run on the same machine/job.
#   2. Times the operations the roadmap's axes name: the already-covered hot ops
#      (stocktake, diff, compact/compact --all) plus the missing ones — shift
#      (checkout), a signed stack (commit after "office enroll"), audit (whole signed
#      history), cold vs warm cache (first invocation vs the mean of later ones), peak
#      RSS (`/usr/bin/time -l`/`-v`), and core-count scaling (audit pinned to one core
#      vs unrestricted, Linux-only — `taskset`; there is no worker-count env/flag to
#      pass instead, see forklift-core's fanout_utils/task.rs).
#   3. Prints a report (and writes JSON + Markdown with --out) and, unless --no-gate is
#      given, runs a small number of *ratio-based* and generously-thresholded checks —
#      see "Gating philosophy" below — exiting non-zero if one trips.
#
# CI runners are noisy and heterogeneous. This harness is deliberately not an
# absolute-time trend tracker with a tight ms baseline (that is flake theater on a
# shared runner) — the report is for humans watching trends over time; the *gate* only
# fires on the kind of order-of-magnitude regression a noisy runner cannot hide.
#
# Usage: bin/bench.sh [--seed N] [--files N] [--parcels N] [--runs N] [--commit-runs N]
#                      [--work DIR] [--keep] [--out PREFIX] [--no-gate] [--no-core-scaling]
#
# --out PREFIX writes PREFIX.json and PREFIX.md alongside the stdout report (default:
# bench-results, written to the current directory).
set -euo pipefail

# ── defaults ─────────────────────────────────────────────────────────────────
SEED=1337               # pure arithmetic, not an RNG seed — see gen_content(); kept as a
                         # knob so a maintainer can shift the corpus shape without editing
                         # the script, while the default stays fixed for CI comparability.
N_FILES=240              # tracked files in the corpus (a "few hundred", per T5)
FILES_PER_DIR=20         # -> N_FILES / FILES_PER_DIR directories
N_PARCELS=300             # commits on the main pallet (>= audit's 256-parcel fan-out
                         # threshold, so core-count scaling actually exercises the
                         # parallel path — see audit_utils::PARALLEL_THRESHOLD)
TOUCH_PER_COMMIT=5       # files touched by each synthetic commit
RUNS=5                   # iterations for the fast, read-only/idempotent ops
COMMIT_RUNS=5             # iterations for the signed-stack (commit) measurement
WORK=""
KEEP=0
OUT="bench-results"
GATE=1
CORE_SCALING=1

usage() { sed -n '2,/^set /{ /^set /d; s/^# \{0,1\}//; p; }' "$0"; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --seed)         SEED="$2"; shift 2;;
        --files)        N_FILES="$2"; shift 2;;
        --parcels)      N_PARCELS="$2"; shift 2;;
        --runs)         RUNS="$2"; shift 2;;
        --commit-runs)  COMMIT_RUNS="$2"; shift 2;;
        --work)         WORK="$2"; shift 2;;
        --keep)         KEEP=1; shift;;
        --out)          OUT="$2"; shift 2;;
        --no-gate)      GATE=0; shift;;
        --no-core-scaling) CORE_SCALING=0; shift;;
        -h|--help)      usage 0;;
        *) echo "unknown argument: $1" >&2; usage 2;;
    esac
done

command -v forklift >/dev/null || { echo "error: forklift not on PATH — build/install it first" >&2; exit 1; }
FORKLIFT_VERSION=$(forklift version 2>/dev/null || echo "forklift ?")

# ── portable millisecond clock (identical to bin/benchmark's) ──────────────────
if date +%s%N 2>/dev/null | grep -qE '^[0-9]{16,}$'; then
    _now_ms() { echo $(( $(date +%s%N) / 1000000 )); }
elif command -v gdate >/dev/null 2>&1 && gdate +%s%N 2>/dev/null | grep -qE '^[0-9]{16,}$'; then
    _now_ms() { echo $(( $(gdate +%s%N) / 1000000 )); }
elif command -v python3 >/dev/null 2>&1; then
    _now_ms() { python3 -c 'import time; print(int(time.time()*1000))'; }
elif command -v perl >/dev/null 2>&1; then
    _now_ms() { perl -MTime::HiRes=time -e 'printf "%d\n", time()*1000'; }
else
    echo "error: need one of GNU date, python3, or perl for timing" >&2; exit 1
fi

# ── workspace + identity isolation ──────────────────────────────────────────
# Never touch the maintainer's real global config or keys (same isolation
# crates/forklift/tests/cli.rs's TestWarehouse gives every integration test).
if [ -z "$WORK" ]; then
    WORK=$(mktemp -d "${TMPDIR:-/tmp}/forklift-bench.XXXXXX")
else
    mkdir -p "$WORK"
fi
CORPUS="$WORK/corpus"
HOME_STUB="$WORK/home"
mkdir -p "$CORPUS" "$HOME_STUB"
export FORKLIFT_GLOBAL_CONFIG="$HOME_STUB/global-config.toml"
export FORKLIFT_KEYS_DIR="$HOME_STUB/keys"

cleanup() { [ "$KEEP" -eq 1 ] || rm -rf "$WORK"; }
trap cleanup EXIT

fl() { ( cd "$CORPUS" && forklift "$@" ); }

echo "Forklift bench (T5): a fixed synthetic corpus, CI regression axes"
echo "  work dir: $WORK  (keep=$KEEP)"
echo "  $FORKLIFT_VERSION"
echo "  corpus:   $N_FILES files, $N_PARCELS parcels, seed $SEED"
echo

# ── timers ───────────────────────────────────────────────────────────────────
# time_series <runs> <setup_or_empty> <cmd> — runs <cmd> (with <setup> run first and
# NOT timed, when given) <runs> times in the corpus dir, echoing one ms value per
# line. The caller splits the first line out as "cold" (this process's — and, on a
# freshly-built corpus, often the OS page cache's — first touch) from the mean of the
# rest ("warm"). See the module comment: this *is* forklift's cold-cache story, since
# every invocation is a fresh process with no cross-invocation object-cache warmth of
# its own — only the OS page cache can be warm or cold, and only the first read of a
# given file pays the disk (vs. page-cache) cost.
time_series() {
    local runs="$1" setup="$2" cmd="$3" i t0 t1
    for ((i=0; i<runs; i++)); do
        if [ -n "$setup" ]; then ( cd "$CORPUS" && eval "$setup" ) >/dev/null 2>&1 || true; fi
        t0=$(_now_ms)
        ( cd "$CORPUS" && eval "$cmd" ) >/dev/null 2>&1 || true
        t1=$(_now_ms)
        echo $(( t1 - t0 ))
    done
}

# stats <values...> -> "first mean_rest min mean_all" (ms). mean_rest falls back to
# the single value when runs==1 (no "rest" to average).
stats() {
    awk 'BEGIN{first=-1; n=0; sum=0; sumrest=0; nrest=0; min=""}
         { if (first<0) first=$1; else { sumrest+=$1; nrest++ }
           if (min=="" || $1<min) min=$1
           sum+=$1; n++ }
         END{
           mrest = (nrest>0) ? sumrest/nrest : first
           printf "%d %d %d %d\n", first, mrest, min, (n>0)? sum/n : 0
         }' <<< "$(printf '%s\n' "$@")"
}

fmt_ms() { awk -v ms="$1" 'BEGIN{ if (ms>=1000) printf "%.2fs", ms/1000; else printf "%dms", ms }'; }

# ── RSS (peak resident set size) ────────────────────────────────────────────
# /usr/bin/time -l on macOS (BSD time, "maximum resident set size" in BYTES) or
# -v on Linux (GNU time, "Maximum resident set size (kbytes)" in KB). Both are
# normalized to KB. "n/a" when /usr/bin/time isn't there or the platform's flag
# doesn't produce a parseable line — never fatal, RSS is a report field, not a gate.
measure_rss_kb() {
    local cmd="$1" out
    command -v /usr/bin/time >/dev/null 2>&1 || { echo "n/a"; return; }
    if [ "$(uname -s)" = "Darwin" ]; then
        out=$( cd "$CORPUS" && /usr/bin/time -l sh -c "$cmd" 2>&1 >/dev/null || true )
        awk '/maximum resident set size/{ printf "%d", $1/1024; found=1 } END{ if (!found) print "n/a" }' <<< "$out"
    else
        out=$( cd "$CORPUS" && /usr/bin/time -v sh -c "$cmd" 2>&1 >/dev/null || true )
        awk -F': ' '/Maximum resident set size/{ printf "%d", $2; found=1 } END{ if (!found) print "n/a" }' <<< "$out"
    fi
}

# ── deterministic corpus content (no RNG: pure arithmetic on indices, portable
# across bash/awk implementations — see the header comment) ───────────────────
DIRS=$(( (N_FILES + FILES_PER_DIR - 1) / FILES_PER_DIR ))

file_path() { # index -> relative path
    # Each `local` below is its own statement deliberately: `local a=$1 b=$(f "$a")` is a
    # real bash trap (shellcheck SC2318) — the RHS of a later var in the *same* `local`
    # command can see the enclosing scope's stale value instead of the one just assigned,
    # so a caller with its own same-named local can mask the bug by coincidence (it did,
    # here, until this was split — see the commit that fixed it).
    local idx="$1"
    local d=$(( (idx / FILES_PER_DIR) % DIRS ))
    local f=$(( idx % FILES_PER_DIR ))
    printf 'src/mod%02d/file%03d.txt' "$d" "$f"
}

gen_initial_files() {
    local idx path
    for ((idx=0; idx<N_FILES; idx++)); do
        path=$(file_path "$idx")
        mkdir -p "$CORPUS/$(dirname "$path")"
        {
            printf 'file %d (seed %d)\n' "$idx" "$SEED"
            for ((l=0; l<8; l++)); do
                printf 'line %d: lorem ipsum dolor sit amet, value=%d\n' "$l" $(( (idx * 31 + l * 17 + SEED) % 9973 ))
            done
        } > "$CORPUS/$path"
    done
}

# touch_files <commit_index> <count> <path_offset> — deterministically pick <count>
# files as a function of <commit_index> and append one line to each.
touch_files() {
    local i="$1" count="$2" offset="$3" k idx path
    for ((k=0; k<count; k++)); do
        idx=$(( (offset + i * 31 + k * 17) % N_FILES ))
        path=$(file_path "$idx")
        printf 'rev %d update %d: value=%d\n' "$i" "$k" $(( (i * 13 + k * 7 + SEED) % 9973 )) >> "$CORPUS/$path"
    done
}

# ── build the corpus ─────────────────────────────────────────────────────────
echo "-> building the corpus ($N_FILES files across $DIRS dirs)…"
fl prepare >/dev/null
fl config --global operator.name "Bench Operator" >/dev/null
fl config --global operator.identifier "bench@forklift" >/dev/null

gen_initial_files
fl load . >/dev/null
fl stack "initial import" >/dev/null

echo "-> office enroll (passphraseless — a CI/bench identity, not a human one)…"
fl office enroll >/dev/null

echo "-> stacking $((N_PARCELS - 1)) further signed parcels…"
for ((i=1; i<N_PARCELS; i++)); do
    touch_files "$i" "$TOUCH_PER_COMMIT" 0
    fl load . >/dev/null
    fl stack "parcel $i" >/dev/null
done

echo "-> fresh compact (loose -> packed)…"
t0=$(_now_ms); fl compact >/dev/null; t1=$(_now_ms)
FRESH_COMPACT_MS=$(( t1 - t0 ))

echo "-> building the diverging shift-target pallet…"
fl palletize bench-shift-b >/dev/null
touch_files 0 40 5000
fl load . >/dev/null
fl stack "diverge for shift bench" >/dev/null
fl shift main >/dev/null

# Rows are stored field-delimited (US, 0x1f), like bin/benchmark, so the report can
# render either an aligned text table or the JSON/Markdown artifact from the same data.
US=$(printf '\037')
ROWS=""
add_row() { # name cold_ms mean_warm_ms min_ms note
    ROWS="${ROWS}${1}${US}${2}${US}${3}${US}${4}${US}${5}"$'\n'
}

# ── 1. stocktake (clean tree) — already-hot op, cheap to add ────────────────
echo "-> [1/8] stocktake…"
read -r cold mrest min _ <<< "$(stats $(time_series "$RUNS" "" "forklift stocktake --summary"))"
add_row "stocktake" "$cold" "$mrest" "$min" "clean tree, $N_FILES files"
STOCKTAKE_MEAN=$mrest

# ── 2. diff (dirty tree) ─────────────────────────────────────────────────────
echo "-> [2/8] diff…"
DIRTY_COUNT=30
for ((k=0; k<DIRTY_COUNT; k++)); do
    idx=$(( (k * 41) % N_FILES ))
    path=$(file_path "$idx")
    printf '\n// bench edit\n' >> "$CORPUS/$path"
done
fl load . >/dev/null
read -r cold mrest min _ <<< "$(stats $(time_series "$RUNS" "" "forklift diff --staged"))"
add_row "diff" "$cold" "$mrest" "$min" "$DIRTY_COUNT files dirtied"
DIFF_MEAN=$mrest
fl restore --staged . >/dev/null
fl restore . >/dev/null

# ── 3. compact --all (steady-state repack; D/P3's ~4.4x, see PARALLELIZATION_PLAN) ──
echo "-> [3/8] compact --all (steady-state repack)…"
read -r cold mrest min _ <<< "$(stats $(time_series "$RUNS" "" "forklift compact --all"))"
add_row "compact --all" "$cold" "$mrest" "$min" "steady-state repack; fresh compact was $(fmt_ms "$FRESH_COMPACT_MS")"
REPACK_MEAN=$mrest

# ── 4. shift (checkout) — ping-pong between two diverging pallets ───────────
echo "-> [4/8] shift…"
read -r cold mrest min _ <<< "$(stats $(time_series "$RUNS" "" "forklift shift bench-shift-b; forklift shift main"))"
add_row "shift" "$cold" "$mrest" "$min" "ping-pong main <-> bench-shift-b"
SHIFT_MEAN=$mrest

# ── 5. signed stack (a commit after office enroll — every stack from here is
#    signed; there is no unsigned escape hatch once trust is established) ──
echo "-> [5/8] signed stack…"
SETUP='printf "\n// commit bench\n" >> '"$CORPUS/$(file_path 0)"'; forklift load '"$(file_path 0)"
read -r cold mrest min _ <<< "$(stats $(time_series "$COMMIT_RUNS" "$SETUP" "forklift stack 'bench commit'"))"
add_row "signed stack" "$cold" "$mrest" "$min" "load + signed stack, 1 file"
STACK_MEAN=$mrest

# ── 6. audit (whole signed history, offline) ─────────────────────────────────
echo "-> [6/8] audit…"
read -r cold mrest min _ <<< "$(stats $(time_series "$RUNS" "" "forklift audit main"))"
add_row "audit" "$cold" "$mrest" "$min" "$N_PARCELS reachable parcels (fan-out threshold is 256)"
AUDIT_MEAN=$mrest

# ── 7. RSS on a couple of the heavier ops ────────────────────────────────────
echo "-> [7/8] peak RSS…"
RSS_AUDIT=$(measure_rss_kb "forklift audit main")
RSS_COMPACT_ALL=$(measure_rss_kb "forklift compact --all")
RSS_STOCKTAKE=$(measure_rss_kb "forklift stocktake --summary")

# ── 8. core-count scaling — audit pinned to 1 core vs unrestricted ──────────
# No worker-count env/flag exists (fanout_utils::fanout_map and TaskExecutor both
# call num_cpus::get()/available_parallelism() directly — see PARALLELIZATION_PLAN.md),
# so this pins with `taskset` (Linux; respected because num_cpus reads
# sched_getaffinity there). macOS has no equivalent (no taskset, no affinity API
# num_cpus honors), so this axis is Linux-only — matching the single gating job.
CORE_SCALE_NOTE="linux-only (taskset) — not applicable on this platform"
SERIAL_AUDIT_MEAN=""
PARALLEL_AUDIT_MEAN=""
if [ "$CORE_SCALING" -eq 1 ] && [ "$(uname -s)" = "Linux" ] && command -v taskset >/dev/null 2>&1; then
    echo "-> [8/8] core-count scaling (audit, 1 core vs unrestricted)…"
    read -r _ SERIAL_AUDIT_MEAN _ _ <<< "$(stats $(time_series 3 "" "taskset -c 0 forklift audit main"))"
    read -r _ PARALLEL_AUDIT_MEAN _ _ <<< "$(stats $(time_series 3 "" "forklift audit main"))"
    CORE_SCALE_NOTE="1 core: $(fmt_ms "$SERIAL_AUDIT_MEAN"), unrestricted: $(fmt_ms "$PARALLEL_AUDIT_MEAN")"
else
    echo "-> [8/8] core-count scaling: skipped ($CORE_SCALE_NOTE)"
fi

echo
echo "-> ${N_FILES}-file / $((N_PARCELS + COMMIT_RUNS))-parcel corpus ready; results below."
echo

# ── report ────────────────────────────────────────────────────────────────────
render_table() {
    printf '  %-16s  %-9s  %-9s  %-9s  %s\n' "Operation" "cold" "warm mean" "warm min" "Notes"
    printf '  %-16s  %-9s  %-9s  %-9s  %s\n' "----------------" "---------" "---------" "---------" "----------------------------------"
    printf '%s' "$ROWS" | while IFS="$US" read -r name cold mrest min note; do
        [ -z "$name" ] && continue
        printf '  %-16s  %-9s  %-9s  %-9s  %s\n' "$name" "$(fmt_ms "$cold")" "$(fmt_ms "$mrest")" "$(fmt_ms "$min")" "$note"
    done
}

echo "Results (cold = first invocation, warm = mean of the remaining runs):"
echo
render_table
echo
echo "  Peak RSS (KB):  stocktake=$RSS_STOCKTAKE  audit=$RSS_AUDIT  'compact --all'=$RSS_COMPACT_ALL"
echo "  Core-count scaling (audit): $CORE_SCALE_NOTE"
echo

# ── gating: robust, ratio-based / generously-thresholded checks only ────────
# Philosophy (see the header comment): CI runners are noisy. These checks catch
# order-of-magnitude-class regressions, not normal run-to-run variance:
#   - Same-run ratios between two operations measured in this run (immune to
#     absolute machine speed — a slow runner scales both sides together).
#   - Absolute ceilings so generous they only trip on a pathological regression
#     (e.g. an accidental O(n^2)), never on ordinary noise.
# Anything tighter belongs in the report artifact (tracked over time by a human),
# not the gate — see docs/PARALLELIZATION_PLAN.md's "T5" note.
GATE_FAILURES=0
gate_check() { # description condition_result(0/1)
    if [ "$2" -eq 0 ]; then
        echo "  FAIL: $1"
        GATE_FAILURES=$((GATE_FAILURES + 1))
    else
        echo "  ok:   $1"
    fi
}

if [ "$GATE" -eq 1 ]; then
    echo "Gate checks:"

    # D/P3 established compact --all's steady-state repack as markedly cheaper than a
    # fresh compact (measured ~4.4x on a similarly-sized corpus). 1.5x fresh compact is
    # a generous ceiling — it only trips if the CopyRecord fast path regresses back
    # toward fresh-compact cost, not on ordinary noise.
    ok=$(awk -v r="$REPACK_MEAN" -v f="$FRESH_COMPACT_MS" 'BEGIN{ print (r <= f * 1.5) ? 1 : 0 }')
    gate_check "compact --all (repack, $(fmt_ms "$REPACK_MEAN")) <= 1.5x fresh compact ($(fmt_ms "$FRESH_COMPACT_MS"))" "$ok"

    if [ -n "$SERIAL_AUDIT_MEAN" ] && [ -n "$PARALLEL_AUDIT_MEAN" ]; then
        # audit's fan-out (audit_utils::verify_signatures) must not make things worse:
        # unrestricted must not be dramatically SLOWER than pinned-to-one-core. 2x is
        # generous headroom for a shared/noisy runner; it catches a broken or
        # contended fan-out, not scheduling jitter.
        ok=$(awk -v p="$PARALLEL_AUDIT_MEAN" -v s="$SERIAL_AUDIT_MEAN" 'BEGIN{ print (p <= s * 2.0) ? 1 : 0 }')
        gate_check "audit unrestricted ($(fmt_ms "$PARALLEL_AUDIT_MEAN")) <= 2x audit on 1 core ($(fmt_ms "$SERIAL_AUDIT_MEAN"))" "$ok"
    fi

    # Absolute ceilings: a safety net against a catastrophic (order-of-magnitude)
    # regression on this fixed, small corpus — not a tight ms baseline. Chosen with
    # generous headroom over what a release build does on this corpus size on
    # ordinary CI hardware.
    for pair in "stocktake:$STOCKTAKE_MEAN:5000" "diff:$DIFF_MEAN:5000" "shift:$SHIFT_MEAN:5000" \
                "signed stack:$STACK_MEAN:5000" "audit:$AUDIT_MEAN:10000" "compact --all:$REPACK_MEAN:10000"; do
        name="${pair%%:*}"; rest="${pair#*:}"; val="${rest%%:*}"; ceiling="${rest#*:}"
        ok=$(awk -v v="$val" -v c="$ceiling" 'BEGIN{ print (v <= c) ? 1 : 0 }')
        gate_check "$name warm mean ($(fmt_ms "$val")) <= generous ceiling ($(fmt_ms "$ceiling"))" "$ok"
    done
    echo
fi

# ── JSON + Markdown artifact ──────────────────────────────────────────────────
if [ -n "$OUT" ]; then
    JSON_FILE="${OUT}.json"
    MD_FILE="${OUT}.md"

    {
        printf '{\n'
        printf '  "forklift_version": "%s",\n' "$FORKLIFT_VERSION"
        printf '  "corpus": { "seed": %d, "files": %d, "parcels": %d },\n' "$SEED" "$N_FILES" "$N_PARCELS"
        printf '  "os": "%s",\n' "$(uname -s)"
        printf '  "operations": [\n'
        first=1
        printf '%s' "$ROWS" | while IFS="$US" read -r name cold mrest min note; do
            [ -z "$name" ] && continue
            [ "$first" -eq 1 ] || printf ',\n'
            first=0
            printf '    { "name": "%s", "cold_ms": %s, "warm_mean_ms": %s, "warm_min_ms": %s, "note": "%s" }' \
                "$name" "$cold" "$mrest" "$min" "$note"
        done
        printf '\n  ],\n'
        printf '  "fresh_compact_ms": %d,\n' "$FRESH_COMPACT_MS"
        printf '  "rss_kb": { "stocktake": "%s", "audit": "%s", "compact_all": "%s" },\n' \
            "$RSS_STOCKTAKE" "$RSS_AUDIT" "$RSS_COMPACT_ALL"
        printf '  "core_scaling": { "note": "%s", "serial_audit_mean_ms": %s, "parallel_audit_mean_ms": %s },\n' \
            "$CORE_SCALE_NOTE" "${SERIAL_AUDIT_MEAN:-null}" "${PARALLEL_AUDIT_MEAN:-null}"
        printf '  "gate_failures": %d\n' "$GATE_FAILURES"
        printf '}\n'
    } > "$JSON_FILE"

    {
        echo "# Forklift bench results (T5)"
        echo
        echo "- **Forklift:** $FORKLIFT_VERSION"
        echo "- **Corpus:** $N_FILES files, $N_PARCELS parcels, seed $SEED, OS $(uname -s)"
        echo "- **Method:** cold = first invocation; warm = mean of $((RUNS - 1)) further runs (commit ops: $((COMMIT_RUNS - 1)))"
        echo
        echo "| Operation | cold | warm mean | warm min | Notes |"
        echo "|-----------|------|-----------|----------|-------|"
        printf '%s' "$ROWS" | while IFS="$US" read -r name cold mrest min note; do
            [ -z "$name" ] && continue
            printf '| %s | %s | %s | %s | %s |\n' "$name" "$(fmt_ms "$cold")" "$(fmt_ms "$mrest")" "$(fmt_ms "$min")" "$note"
        done
        echo
        echo "**Peak RSS (KB):** stocktake=$RSS_STOCKTAKE, audit=$RSS_AUDIT, \`compact --all\`=$RSS_COMPACT_ALL"
        echo
        echo "**Core-count scaling (audit):** $CORE_SCALE_NOTE"
        echo
        echo "**Gate failures:** $GATE_FAILURES"
    } > "$MD_FILE"

    echo "-> wrote $JSON_FILE and $MD_FILE"
fi

if [ "$KEEP" -eq 1 ]; then echo "-> work dir kept at $WORK"; fi

if [ "$GATE" -eq 1 ] && [ "$GATE_FAILURES" -gt 0 ]; then
    echo
    echo "bench: $GATE_FAILURES gate check(s) failed" >&2
    exit 1
fi

exit 0
