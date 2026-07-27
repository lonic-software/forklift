#!/usr/bin/env bash
#
# verify.sh — real-account deploy-and-verify for infra/aws-serverless (FORK-60, design memo §5,
# "Layer 3"). Mechanizes docs/DEPLOYMENT.md's "Verification checklist" (steps 1-4) against a REAL
# deployed stack — never LocalStack, that's Layer 2's job — plus a signature-sidecar fetch that
# pins the one binary-response path the redirect-based reads never touch.
#
# WHY THIS SCRIPT EXISTS, AND WHAT IT DOES NOT PROVE:
#
# Layer 1 (`tofu test`/`terraform test` with mock_provider, tests/main.tftest.hcl) is free,
# credential-less, and runs on every PR — but it only proves what got *planned*, never what a
# real AWS account actually *enforces*. Layer 2 (LocalStack apply-and-exercise, path-filtered on
# infra/** and the crate) proves the S3-event wiring actually delivers and the verifier actually
# cold-starts — but LocalStack's community image can't even create an API Gateway v2 API, and it
# does not enforce IAM, so a policy regression merges green there and is only caught here. This
# script is the *only* place any of that gets pinned: real IAM enforcement, real arm64 Lambda
# boot, real API Gateway request/response semantics (in particular the one binary response —
# see the signature-sidecar check below), and the presigned-URL / async-promotion signer-role
# interactions under a customer-managed KMS key.
#
# KMS COVERAGE (design memo §3.1, §5) — the reason this script exists at all:
#
# Under a CMK, both Lambda execution roles need the SAME two actions
# (kms:Decrypt, kms:GenerateDataKey), scoped to the key — but the two roles fail in OPPOSITE
# shapes if either grant is missing. The control plane fails LOUDLY and SYNCHRONOUSLY: every
# presigned GET and every offloaded response 403s the moment it's tried. The verifier fails
# SILENTLY and ASYNCHRONOUSLY: the client's staging PUT succeeds, the S3 event fires, the
# verifier's promotion dies inside a Lambda nobody is watching, and the object simply never
# becomes fetchable — the worst failure shape in the whole stack, because nothing surfaces at
# the point of use. A design that only exercised a presigned PUT (as an earlier revision of the
# underlying module did, twice, in two different ways — see the design memo's changelog) would
# never catch either failure. This script deliberately exercises both halves:
#
#   - the >= 8 MiB file in the round trip below is chunked and routes through ASYNC promotion —
#     that's the verifier's half of the grant;
#   - the explicit read checks near the end of this script exercise the control plane's Decrypt
#     grant directly, via a route (the signature sidecar) that isn't a redirect at all.
#
# NOT INDEPENDENT EVIDENCE, and it matters: the round trip's franchise leg (which downloads the
# very files this script just lifted) already goes through `presign_get` as the control plane's
# own signer, so on a CMK-configured stack (see examples/staging/) that leg alone already
# exercises the control plane's Decrypt grant once, as a side effect. The explicit read checks
# below are kept anyway, precisely so the KMS pin does not rest on that incidental shape — but a
# pass on both must NOT be read as two independent confirmations of the control-plane grant. See
# the comment directly above those checks for the same caveat, restated where it applies.
#
# s3:ListBucket COVERAGE (PR #82) — a second instance of the same loud/silent asymmetry, found by
# this script's real-account role, not by a source scan:
#
# S3 answers `403 Forbidden` instead of `404 Not Found` for HeadObject/GetObject on a missing key
# when the caller lacks an unconditional s3:ListBucket grant on the bucket — it refuses to confirm
# non-existence rather than saying so. Both roles need this grant, and the two fail in the SAME
# opposite shapes KMS does: the control plane fails LOUDLY and SYNCHRONOUSLY (every fresh lift
# HEADs a key that correctly doesn't exist yet, and without the grant that comes back as an opaque
# 500 instead of the expected "not there"). The verifier fails SILENTLY and ASYNCHRONOUSLY
# (`verify_and_promote`'s `key_exists` check against the canonical key — absent by definition for
# a genuinely new object — hits the identical 403, and a staged upload that never promotes reads
# from the outside exactly like a hung lift). Like the KMS grant, this one is real-account-only:
# LocalStack does not enforce IAM, so a missing or misconfigured s3:ListBucket grant plans and
# applies clean in Layer 1/2 and is only ever caught here. Unlike KMS, there is no narrower variant
# to consider — the grant cannot be prefix-conditioned at all (s3:prefix is not in the request
# context HeadObject/GetObject authorize against; see docs/DEPLOYMENT.md and
# crates/forklift-aws-lambda/tests/iam_conformance.rs for the full account, including how a
# per-file conformance test now pins it directly, and iam/*.policy.json for the actual grant).
# This script's existing round trip already exercises it end to end: every step below HEADs or
# GETs at least one key that does not yet exist at the point of the call (a fresh object being
# lifted, the canonical key `verify_and_promote` checks before promoting) — there is no separate
# check to add here, only this account of why a clean run is evidence for this grant too, not only
# for KMS.
#
# UNVERIFIED UNTIL A REAL ACCOUNT ACTUALLY RUNS THIS — this script cannot be exercised against a
# real AWS account from this development environment (no AWS account is available here, and none
# was sought — that constraint is by design, not an oversight). What a real run is needed to
# prove, that inspection and local shellcheck/argument-handling checks cannot:
#
#   1. That every step below actually passes against real API Gateway, real IAM enforcement, and
#      a real arm64 Lambda boot — this script's control flow and argument handling can be checked
#      without AWS; whether the *deployed stack* behaves as the design predicts cannot.
#   2. The minimal sufficient KMS action set (N4 in the design memo's §5 "Can't-build entries" —
#      this script proves the *chosen* set works, never that a narrower one would have too).
#   3. Whether 24s of commit_lift's built-in retry/backoff (see forklift-core's remote_utils.rs)
#      is actually enough real-world margin for the verifier to complete a large-object
#      promotion under real S3-event latency — LocalStack's event delivery latency is not
#      representative of production's.
#   4. Timing/cost of the throttling defaults and PITR billing under real, sustained scheduled use.
#   5. Whether the signature-sidecar check (step 4/4) ever actually runs its 200 branch — see its
#      own comment: on any stack that has never had trust established (true of examples/staging/
#      today), it is a documented, standing can't-build, not something a real run has confirmed.
#
# Do not treat a clean run of THIS script's argument handling (see the definition-of-done demo in
# the FORK-60 Layer 3 report) as evidence for any of the above — it only proves the script itself
# doesn't crash on bad input or hang on a bad endpoint.
#
# PR #80 REVIEW — two HIGH findings fixed here, both previously untested because this is the one
# layer that has never been executed against a real stack:
#
#   - Every run used to `prepare` a brand-new root parcel and lift it to the persistent stack's
#     pallet: run 1 succeeded, then every run after diverged from what the stack actually held
#     (remote_utils.rs's divergence check / head.rs's "not a fast-forward"), turning the daily
#     scheduled workflow permanently red after its first success — and indistinguishably from a
#     real verifier-KMS failure, the worst possible false signal. Fixed by franchising the
#     persistent stack FIRST, then stacking new fixture content on top of whatever it already
#     holds, every run (step 2/4's comment has the full reasoning, including why this was chosen
#     over a per-run pallet).
#   - Step 4/4 asserted `GET /v1/signatures/{parcel}` returns 200 unconditionally, but a stack
#     with no trust anchor established never signs anything, so that parcel's signature can only
#     ever 404. Fixed by making the assertion conditional on the handshake already having
#     reported a trust anchor — see step 4/4's comment for why establishing one from inside this
#     script was ruled out rather than attempted.

set -euo pipefail

SCRIPT_NAME="$(basename "$0")"

usage() {
  cat <<'EOF' >&2
Usage: verify.sh <api-endpoint> <bucket-name> <table-name> <auth-token> [warehouse-prefix]

  api-endpoint      The stack's api_endpoint output, e.g.
                     https://abc123.execute-api.us-east-1.amazonaws.com
  bucket-name       The stack's bucket_name output.
  table-name        The stack's table_name output. Accepted for symmetry with the module's
                     outputs (endpoint/bucket/table) even though this script does not query
                     DynamoDB directly today.
  auth-token        The bearer token configured as this stack's auth_token (FORKLIFT_TOKEN).
  warehouse-prefix  Optional. Pass "/warehouses/<id>" when the stack was deployed with a
                     non-null warehouse_id (multi-warehouse mode). Omit entirely for
                     single-warehouse mode (fixed /v1/... routes) — this is what
                     examples/staging/ deploys, so the scheduled workflow never needs it.

Environment:
  FORKLIFT_BIN     Path to the forklift CLI binary (default: "forklift", resolved via PATH).
  VERIFY_WORKDIR   Scratch directory root (default: a fresh mktemp -d, removed on exit). When
                   set, the directory is created if missing (mkdir -p) and is NEVER removed by
                   this script on exit — only a directory this script itself created (the
                   mktemp default) is cleaned up.
  CURL_MAX_TIME    Seconds before any single curl request gives up (default: 20).

Exit codes: 2 = bad invocation or missing prerequisites; 1 = a verification step failed;
0 = every step passed.
EOF
}

log()  { printf '[verify] %s\n' "$*"; }
fail() { printf '[verify] FAIL: %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------------------------
# Preflight: argument count/shape, then required tools. Both are checked before anything that
# could touch the network, so a bad invocation always fails fast with a usage message rather
# than partway through a half-run.
# ---------------------------------------------------------------------------------------------

if [[ $# -lt 4 || $# -gt 5 ]]; then
  usage
  exit 2
fi

ENDPOINT="${1%/}"
BUCKET="$2"
TABLE="$3"
TOKEN="$4"
PREFIX="${5:-}"

if [[ -z "$ENDPOINT" || -z "$BUCKET" || -z "$TABLE" || -z "$TOKEN" ]]; then
  echo "$SCRIPT_NAME: api-endpoint, bucket-name, table-name and auth-token must all be non-empty" >&2
  usage
  exit 2
fi

case "$ENDPOINT" in
  http://* | https://*) ;;
  *)
    echo "$SCRIPT_NAME: api-endpoint must start with http:// or https:// — got: $ENDPOINT" >&2
    exit 2
    ;;
esac

if [[ -n "$PREFIX" && "${PREFIX:0:1}" != "/" ]]; then
  echo "$SCRIPT_NAME: warehouse-prefix must start with / — got: $PREFIX" >&2
  exit 2
fi

FORKLIFT_BIN="${FORKLIFT_BIN:-forklift}"
CURL_MAX_TIME="${CURL_MAX_TIME:-20}"

missing_cmds=()
for c in curl aws jq dd cmp mktemp; do
  command -v "$c" >/dev/null 2>&1 || missing_cmds+=("$c")
done
command -v "$FORKLIFT_BIN" >/dev/null 2>&1 || missing_cmds+=("$FORKLIFT_BIN (set FORKLIFT_BIN to its path if it's not on PATH)")
if ((${#missing_cmds[@]})); then
  printf '%s: missing required command(s): %s\n' "$SCRIPT_NAME" "${missing_cmds[*]}" >&2
  exit 2
fi

# PR #80 review, finding #6: VERIFY_WORKDIR used to be removed on exit unconditionally, even
# though the usage text only ever promised that of the mktemp default — a caller-supplied
# `VERIFY_WORKDIR=~/scratch` was silently deleted too. Only remove what this script itself
# created. A supplied directory is also never created before the first write into it (the
# handshake response, below) — that used to fail with a confusing curl "couldn't write file"
# error instead of a clear one, so it is mkdir -p'd here, up front.
CREATED_WORKDIR=0
if [[ -n "${VERIFY_WORKDIR:-}" ]]; then
  WORKDIR="$VERIFY_WORKDIR"
  mkdir -p "$WORKDIR" || fail "could not create VERIFY_WORKDIR \"$WORKDIR\""
else
  WORKDIR="$(mktemp -d -t forklift-verify.XXXXXX)"
  CREATED_WORKDIR=1
fi

cleanup() {
  if ((CREATED_WORKDIR)); then
    rm -rf "$WORKDIR"
  fi
}
trap cleanup EXIT

log "scratch directory: $WORKDIR"

# ---------------------------------------------------------------------------------------------
# Small helpers.
# ---------------------------------------------------------------------------------------------

# curl_to_file <url> <outfile> [curl-args...] — always bounded by CURL_MAX_TIME/connect-timeout,
# writes the body to <outfile>, and prints the HTTP status code (nothing else) to stdout. A
# non-zero return means curl itself failed (DNS, refused connection, timeout) — never an HTTP
# error status, which this always returns 0 for so the caller can inspect the code itself. This
# is what makes "bad endpoint fails cleanly rather than hangs" true: every call is bounded.
curl_to_file() {
  local url="$1" outfile="$2"
  shift 2
  curl -sS --connect-timeout 5 --max-time "$CURL_MAX_TIME" -o "$outfile" -w '%{http_code}' "$@" "$url"
}

# run_in <dir> <cmd...> — runs a command with cwd set to <dir>, in a subshell, so a failed cd
# short-circuits the command rather than silently running it in the wrong directory.
run_in() {
  local dir="$1"
  shift
  (
    cd "$dir" || exit 1
    "$@"
  )
}

# staging_key_count — prints the number of keys under staging/ in $BUCKET, or fails the run.
#
# Deliberately NOT --query 'KeyCount'. list-objects-v2 is a paginated operation, and the CLI's
# auto-pagination assembles its result from the paginator's result_keys only — for ListObjectsV2
# those are Contents and CommonPrefixes (botocore data/s3/2006-03-01/paginators-1.json), with no
# non_aggregate_keys entry. KeyCount is therefore dropped from the assembled result and --output
# text prints the literal "None" for EVERY response, including a small non-truncated one. Both
# readings were "None", and `(( None > None ))` is false — bash reads the bare word as an unset
# parameter, i.e. 0 — so step 3/4 could not fail, and would not have reddened even if the
# commit_lift sweep it exists to pin were removed outright. length(Contents || `[]`) counts the
# merged pages and is correct across pagination.
#
# stderr is captured separately rather than folded in with 2>&1: a warning printed by an
# otherwise SUCCESSFUL call would be concatenated into the value, and the arithmetic comparison
# at the call site would then abort the run under set -e with a bash syntax error that looks
# nothing like the real cause. The numeric guard below is the same reasoning applied to any
# other unexpected shape — it must fail loudly here, not evaluate to 0 inside (( )) later.
staging_key_count() {
  local out err_file
  err_file="$(mktemp -t forklift-verify-s3err.XXXXXX)"
  if ! out="$(aws s3api list-objects-v2 \
    --bucket "$BUCKET" --prefix "staging/" \
    --query 'length(Contents || `[]`)' --output text 2>"$err_file")"; then
    local err
    err="$(tr '\n' ' ' <"$err_file")"
    rm -f "$err_file"
    fail "aws s3api list-objects-v2 against bucket $BUCKET failed: $err"
  fi
  rm -f "$err_file"
  if [[ ! "$out" =~ ^[0-9]+$ ]]; then
    fail "expected a numeric key count from list-objects-v2 on $BUCKET, got: \"$out\""
  fi
  printf '%s' "$out"
}

# bucket_total_size_bytes — prints the total byte size of every object in $BUCKET (PR #80
# review, finding #3 (MEDIUM), "unbounded growth").
#
# Measured over the WHOLE bucket, deliberately, not just staging/ like staging_key_count above.
# The growth this finding is about lives in objects/ — the canonical, permanent, content-
# addressed namespace (crates/forklift-aws-lambda/src/aws/s3.rs's "Key layout" doc comment) —
# not staging/, which already has its own mandatory lifecycle expiry (main.tf's "expire-staging"
# rule, main.tf:118-129) and is not where anything accumulates run over run. signatures/ is
# canonical-adjacent and permanent too, but tiny; responses/ is ephemeral and usually lifecycle-
# managed (main.tf's optional "expire-responses" rule). A whole-bucket total is the simplest
# measure guaranteed to catch the growth regardless of which prefix it nominally sits under, and
# it is also the number an operator actually pays for.
#
# Every run stages a fresh 9 MiB of /dev/urandom (step 2/4 below) and franchises the whole
# store's history twice. Fresh urandom never deduplicates against anything already in objects/,
# and nothing anywhere — this script, the module, or the crate — ever prunes a canonical object:
# an S3 lifecycle rule on objects/ would make the NEXT franchise fail on a missing object (the
# franchise leg fetches the full history closure), which is a permanently red run AND a
# genuinely corrupted warehouse, not a fix. So the store grows, by design, forever. The point of
# this check is not to stop that growth — it's to make its eventual failure self-naming, rather
# than indistinguishable from a real verifier/KMS failure (this file's header calls that "the
# worst possible false signal").
#
# Same non_aggregate_keys pagination trap as staging_key_count above applies here too: there is
# no top-level aggregate "total bucket size" the CLI can hand back for a paginated listing (only
# Contents/CommonPrefixes survive auto-pagination), so this sums Contents[].Size across the
# merged pages rather than trusting any single-field aggregate. `sum(... || `[]`)` is null-safe
# for an empty bucket (Contents is then absent, the projection is null, `||` substitutes the
# empty-array literal, and sum([]) is 0) — never a bare "None" fed into arithmetic. Same
# stderr-to-a-file and numeric-guard discipline as staging_key_count, for the same reason.
bucket_total_size_bytes() {
  local out err_file
  err_file="$(mktemp -t forklift-verify-s3err.XXXXXX)"
  if ! out="$(aws s3api list-objects-v2 \
    --bucket "$BUCKET" \
    --query 'sum(Contents[].Size || `[]`)' --output text 2>"$err_file")"; then
    local err
    err="$(tr '\n' ' ' <"$err_file")"
    rm -f "$err_file"
    fail "aws s3api list-objects-v2 (whole-bucket size scan) against bucket $BUCKET failed: $err"
  fi
  rm -f "$err_file"
  if [[ ! "$out" =~ ^[0-9]+$ ]]; then
    fail "expected a numeric total size from list-objects-v2 on $BUCKET, got: \"$out\""
  fi
  printf '%s' "$out"
}

# Soft/hard thresholds the size tripwire (below) fires against. Soft is informational: a NOTE
# an operator/log-scraper can key off before it's urgent. Hard aborts the run with a message
# that names itself as retention, not a verifier or stack regression, so nobody has to debug
# this as an application failure. Both are round numbers chosen for a comfortable multi-month
# runway on a DAILY cron (.github/workflows/aws-serverless-verify.yml) at ~9 MiB genuinely new
# bytes added per run (dd of urandom, step 2/4 below) — a tripwire, not a derived capacity plan:
#   - SOFT = 1 GiB  -> roughly 110+ daily runs (~3-4 months) before it's even worth a look
#   - HARD = 4 GiB  -> roughly 450+ daily runs (~15 months) before the run refuses to continue
SOFT_BUCKET_SIZE_BYTES=$((1 * 1024 * 1024 * 1024)) # 1 GiB
HARD_BUCKET_SIZE_BYTES=$((4 * 1024 * 1024 * 1024)) # 4 GiB

# ---------------------------------------------------------------------------------------------
# staging/ baseline (PR #80 review, finding #7) — snapshotted before this run touches anything,
# so step 3/4's post-lift check can be scoped to what THIS run added rather than the whole
# bucket. The CLI has no way to report the lift session id it minted (remote_utils.rs's
# new_lift_session() is internal and never surfaces through LiftReport/--json), so a literal
# "this run's session prefix" isn't obtainable from the outside — a before/after delta on the
# same bucket-wide listing is the mechanism instead, and it is equivalent for the property that
# actually matters: did THIS run's own commit_lift sweep clear everything THIS run staged. One
# aborted run's leftover (from before this run started) no longer wedges every run after it —
# it is carried forward as an unchanging baseline until its own lifecycle-rule expiry, exactly
# the "harmless" the old comment already described, rather than a bucket-wide fatal.
# ---------------------------------------------------------------------------------------------

staging_key_count_before="$(staging_key_count)"

if [[ "$staging_key_count_before" != "0" ]]; then
  log "  NOTE — staging/ already holds $staging_key_count_before key(s) before this run starts \
(most likely a previous aborted run's leftover, e.g. one killed by the job timeout) — harmless, \
and left alone until its own lifecycle-rule expiry. This run's own check (step 3/4) only asserts \
it does not ADD to that count."
fi

# ---------------------------------------------------------------------------------------------
# Size tripwire (PR #80 review, finding #3) — deliberately run before any of the four checklist
# steps below, not after them, so it always executes and reports even when a later step fails
# outright under `set -e` (a bad endpoint, a real verifier/KMS regression, anything). The
# operator needs to know "this is retention, not a real failure" regardless of what else does or
# does not go wrong in the rest of this run. See bucket_total_size_bytes's header comment above
# for what grows, why, and why the fix here is a tripwire rather than an attempt to stop it.
# ---------------------------------------------------------------------------------------------

bucket_size_bytes="$(bucket_total_size_bytes)"

if (( bucket_size_bytes > HARD_BUCKET_SIZE_BYTES )); then
  fail "bucket $BUCKET holds $bucket_size_bytes bytes, over the hard threshold of \
$HARD_BUCKET_SIZE_BYTES bytes (4 GiB). This is RETENTION, not a verifier or stack regression — \
by design (see bucket_total_size_bytes's header comment above), this store never deduplicates \
and nothing ever prunes it. The persistent staging stack is fixture-only and disposable (see \
examples/staging/main.tf's own header); reset it per this README's \"Layer 3 retention & reset\" \
section (tofu destroy / tofu apply from examples/staging/) rather than investigating this as an \
application failure."
fi

if (( bucket_size_bytes > SOFT_BUCKET_SIZE_BYTES )); then
  log "  NOTE — bucket $BUCKET holds $bucket_size_bytes bytes, over the soft threshold of \
$SOFT_BUCKET_SIZE_BYTES bytes (1 GiB) but under the hard threshold of $HARD_BUCKET_SIZE_BYTES \
bytes (4 GiB) — not yet urgent. Expected: this store grows by design and is never pruned (see \
bucket_total_size_bytes's header comment above). Worth planning a reset soon — see this \
README's \"Layer 3 retention & reset\" section."
fi

# ---------------------------------------------------------------------------------------------
# Step 1/4 (checklist step 1) — handshake.
# ---------------------------------------------------------------------------------------------

log "step 1/4 (checklist #1): handshake — GET ${ENDPOINT}${PREFIX}/v1/warehouse"

handshake_code="$(curl_to_file "${ENDPOINT}${PREFIX}/v1/warehouse" "$WORKDIR/handshake.json" \
  -H "Authorization: Bearer $TOKEN")" ||
  fail "handshake request did not complete — network error, DNS failure, or timeout against $ENDPOINT (bounded by CURL_MAX_TIME=${CURL_MAX_TIME}s)"

[[ "$handshake_code" == "200" ]] ||
  fail "handshake expected 200, got $handshake_code. Body: $(cat "$WORKDIR/handshake.json" 2>/dev/null)"

chunking="$(jq -r '.chunking // empty' "$WORKDIR/handshake.json" 2>/dev/null || true)"
[[ "$chunking" == "true" ]] ||
  fail "handshake body is missing \"chunking\": true. Body: $(cat "$WORKDIR/handshake.json")"

log "  OK — 200, chunking: true"

# ---------------------------------------------------------------------------------------------
# Step 2/4 (checklist steps 2 and 3, combined into one lift) — a small round trip plus a chunked
# (>= 8 MiB) round trip. Combined deliberately: both call the identical
# ObjectStore::verify_and_promote (docs/DEPLOYMENT.md "Architecture overview"), one synchronously
# from the control plane and one asynchronously via the verifier's S3-event trigger, so one lift
# exercises both promotion paths at once rather than paying for two round trips against a real
# account on every scheduled run.
#
# This is also THE KMS-VERIFIER HALF: big.bin is well above the 8 MiB chunk threshold, so its
# chunks stage then get asynchronously verified and promoted by the verifier Lambda. If the
# verifier is missing its KMS grants, this is exactly where it shows up — `lift`'s commit_lift
# retries with backoff for about 24s while a blob is "not yet promoted" (forklift-core's
# remote_utils.rs) before giving up, so a real KMS failure here is a slow, real failure, not a
# fast one; that slowness is itself the asynchronous-and-invisible failure mode the design memo
# warns about, made visible by giving it somewhere to surface.
#
# PR #80 review, finding (HIGH) #1 — WHY FRANCHISE-FIRST, NOT A PER-RUN PALLET: this used to
# `prepare` a brand-new, disconnected warehouse and stack straight onto it, then lift — which
# works once (an unborn pallet lifting to an unborn remote pallet) and diverges every run after,
# since the persistent stack's pallet has since moved and this run's root parcel shares no
# ancestry with it. Two shapes were on the table:
#
#   (a) a per-run pallet name (e.g. "fork60-verify-<run id>") — sidesteps divergence by never
#       touching the pallet a previous run used, but accumulates one new pallet in the staging
#       warehouse per scheduled run, forever, with no natural cleanup: nothing in this stack's
#       design ever prunes an abandoned pallet, so this would need its own retention story on
#       top (and did not get designed one).
#   (b) franchise (clone) the persistent stack's pallet FIRST, then load/stack new content on
#       top of whatever it already holds, then lift — the same pallet, every run, exactly the
#       pull-then-push shape a real client actually uses. Chosen: it does not accumulate
#       anything (no new pallet is ever created after the very first run), and `franchise`
#       already handles a never-lifted-to remote gracefully (an "unborn" pallet — verified by
#       reading crates/forklift/src/commands/franchise.rs: it checks out the pallet name with no
#       head and returns cleanly), so the first scheduled run and the thousandth run go through
#       the identical code path. Franchising also adopts the remote's `remote.url`/`remote.token`
#       and trust anchor automatically, which is why the old explicit `config remote.url` /
#       `config remote.token` calls are gone below.
#
# One consequence worth naming: the fixture files below now get OVERWRITTEN each run (not
# freshly created each time) once a persistent stack has been verified more than once — `load`
# detects the change like any modified file would. That is arguably a more representative
# exercise of a long-lived warehouse than always-brand-new files were.
# ---------------------------------------------------------------------------------------------

log "step 2/4 (checklist #2, #3): small + chunked (>= 8 MiB) round trip"

SRC_DIR="$WORKDIR/src"
FR_DIR="$WORKDIR/franchise"

# Cleared, not just created: a caller-supplied VERIFY_WORKDIR is documented as never removed by
# this script, which invites reuse across runs — but `forklift franchise` refuses a non-empty
# target outright ("... is not empty; franchise into a new or empty directory."), so the second
# run into the same VERIFY_WORKDIR would die here. Removing only the two subdirectories this
# script owns keeps that documented contract (the workdir itself, and anything the caller put
# beside these, survives) while making reuse actually work.
rm -rf "$SRC_DIR" "$FR_DIR"

log "  franchising the persistent stack into a scratch working copy (so this run stacks on top \
of whatever it already holds, rather than diverging from a disconnected new root)..."
"$FORKLIFT_BIN" franchise "${ENDPOINT}${PREFIX}" "$SRC_DIR" --token "$TOKEN" >/dev/null ||
  fail "initial franchise (of the persistent stack, before this run's own changes) failed"

printf 'FORK-60 Layer 3 verify run — small file A (%s)\n' "$(date -u +%FT%TZ)" >"$SRC_DIR/small-a.txt"
printf 'FORK-60 Layer 3 verify run — small file B\n' >"$SRC_DIR/small-b.txt"

# 9 MiB of urandom: comfortably above the 8 MiB chunk threshold, with real (non-degenerate,
# non-compressible) content so chunking is exercised for real rather than as an artifact of an
# all-zero or otherwise-compressible fixture.
dd if=/dev/urandom of="$SRC_DIR/big.bin" bs=1M count=9 status=none

run_in "$SRC_DIR" "$FORKLIFT_BIN" load . >/dev/null || fail "forklift load failed"
run_in "$SRC_DIR" "$FORKLIFT_BIN" stack "FORK-60 Layer 3 verify run ($(date -u +%FT%TZ))" >/dev/null ||
  fail "forklift stack failed"

log "  lifting (small files promote synchronously; big.bin promotes asynchronously via the verifier)..."
run_in "$SRC_DIR" "$FORKLIFT_BIN" lift ||
  fail "lift failed. If this is the only thing that changed since Layer 2's LocalStack coverage passed, this is exactly the asynchronous/invisible verifier-KMS failure the design memo warns about (§3.1): the staging PUT succeeded, but promotion never completed."

log "  franchising into a second, independent directory and comparing byte-for-byte..."
"$FORKLIFT_BIN" franchise "${ENDPOINT}${PREFIX}" "$FR_DIR" --token "$TOKEN" >/dev/null ||
  fail "second franchise failed"

for f in small-a.txt small-b.txt big.bin; do
  cmp -s "$SRC_DIR/$f" "$FR_DIR/$f" ||
    fail "franchised $f differs from the source — byte-for-byte round trip broken"
done

log "  OK — small files and big.bin round-tripped identically"

# ---------------------------------------------------------------------------------------------
# Step 3/4 (checklist step 4) — after a successful lift, staging/ must not have grown: commit_
# lift's final batch sweeps everything THIS run staged. See the "staging/ baseline" section
# above (PR #80 review, finding #7) for why this is a before/after delta on the bucket-wide
# count rather than an absolute zero: a leftover from an unrelated aborted run must not wedge
# this or any later run, only a leftover THIS run itself created should.
# ---------------------------------------------------------------------------------------------

log "step 3/4 (checklist #4): staging/ has not grown since before this run's lift"

staging_key_count_after="$(staging_key_count)"

if (( staging_key_count_after > staging_key_count_before )); then
  fail "staging/ grew from $staging_key_count_before to $staging_key_count_after key(s) after this run's lift — this run's own commit_lift sweep should have cleared everything it staged"
fi

log "  OK — staging/ did not grow ($staging_key_count_before -> $staging_key_count_after key(s))"

# ---------------------------------------------------------------------------------------------
# Step 4/4 — signature-sidecar fetch (binary response path) and the explicit control-plane read
# (KMS, control-plane half — see the caveat below).
#
# GET /v1/signatures/{hash} is the one endpoint that returns raw bytes directly through the
# control-plane Lambda instead of a redirect to a presigned S3 URL (docs/DEPLOYMENT.md "Binary
# responses") — every object/bundle fetch this script has exercised so far is a redirect. An
# HTTP API (API Gateway v2) handles that binary Lambda-proxy response transparently; a REST API
# would need explicit Binary Media Types configuration the module deliberately never offers
# (§3.1, §7) — but "transparently" is exactly the kind of claim only a real API Gateway can
# confirm. LocalStack's community image cannot even create the v2 API to test this at all.
#
# It is also, incidentally, a genuinely more DIRECT exercise of the control plane's own
# kms:Decrypt grant than a presigned GET is: get_signature issues its S3 GetObject synchronously,
# inside the control-plane Lambda's own execution context, using its own role's credentials —
# there is no hand-off to a separately-authenticated client request the way a presigned URL's
# eventual GET is.
#
# PR #80 review, finding (HIGH) #2 — WHY THIS IS CONDITIONAL, AND WHY IT IS A CAN'T-BUILD ON A
# STACK WITHOUT A TRUST ANCHOR: this used to assert 200 unconditionally, but a parcel is only
# ever signed once trust is established (stack_utils::resolve_signing_key returns Ok(None), no
# error, while it is not — forklift-core/src/util/stack_utils.rs), and nothing in this script or
# examples/staging/ ever establishes one. So on any such stack (true of examples/staging/ today)
# get_signature has nothing to return and head.rs's signature_get 404s — "The parcel carries no
# signature" — on every run, unconditionally asserting 200 was simply wrong, not flaky.
#
# Two ways to close this were considered and both were ruled out for THIS script, rather than
# silently dropping the check:
#
#   - An alternative unsigned binary-response probe: ruled out with evidence, not assumed. Read
#     against the real S3-backed store (crates/forklift-aws-lambda/src/aws/s3.rs): object_get
#     (`access`) always returns Redirect, never Direct bytes (line ~552); offload_response
#     (the `batch` bundle's own possible direct-bytes path) always uploads and returns a
#     presigned URL, unconditionally (line ~734) — there is no size threshold under which it
#     hands back bytes directly. The signature sidecar is the ONLY 200-with-raw-bytes path this
#     deployment ever has; there is no unsigned substitute to swap in.
#   - Establishing a trust anchor from inside this script (`office keygen` + `office admit`,
#     or the equivalent `office enroll`): ruled out as OUT OF SCOPE for this script, not
#     attempted and silently left broken. `office enroll` is a one-way door for the ENTIRE
#     persistent warehouse ("from then on every parcel stacked in this warehouse must be
#     signed" — its own CLI help text) — flipping that on the real dogfood staging stack is a
#     standing security/product decision, not a shell-script bugfix, and this repo's own
#     convention is that new-invariant decisions like that get a design doc, not a silent side
#     effect of a review-fix PR. It also cannot be made to work per-run without new
#     infrastructure this workflow does not have: enrollment can only happen once per warehouse,
#     so every later run would need to sign as the SAME already-enrolled identity, which means a
#     signing key persisted across runs (a new CI secret) and script-level plumbing to place it
#     correctly in each run's fresh franchise before `stack` — none of which exists today. And
#     because Layer 3 cannot be run for real from this environment (no AWS account, by design —
#     see this file's header), establishing trust for the first time could not be verified here
#     even if it were built.
#
# So: read whether trust is already established from the SAME handshake response step 1/4
# already fetched (`.trust` is `null` until a trust anchor exists — WarehouseInfo, forklift-core/
# src/model/remote.rs), and only assert the 200 when it is. When it is not, this is logged
# loudly as a known, standing can't-build — never a silent pass, never a deletion of the check —
# so the day trust IS deliberately established on this stack (a separate, explicit decision),
# this exact check starts pinning the binary-response path with no further script change needed.
# ---------------------------------------------------------------------------------------------

log "step 4/4: signature-sidecar fetch + explicit control-plane read"

history_json="$(run_in "$SRC_DIR" "$FORKLIFT_BIN" history -n 1 --json)" || fail "forklift history --json failed"
parcel_hash="$(printf '%s' "$history_json" | jq -r '.data.entries[0].parcel // empty' 2>/dev/null)"
[[ -n "$parcel_hash" ]] ||
  fail "could not extract a parcel hash from 'forklift history -n 1 --json': $history_json"

if jq -e '.trust != null' "$WORKDIR/handshake.json" >/dev/null 2>&1; then
  log "  fetching signature sidecar: GET ${PREFIX}/v1/signatures/$parcel_hash"
  sig_code="$(curl_to_file "${ENDPOINT}${PREFIX}/v1/signatures/$parcel_hash" "$WORKDIR/sig.bin" \
    -H "Authorization: Bearer $TOKEN")" || fail "signature-sidecar request did not complete"

  [[ "$sig_code" == "200" ]] ||
    fail "GET /v1/signatures/$parcel_hash expected 200 (direct bytes, no redirect), got $sig_code"
  [[ -s "$WORKDIR/sig.bin" ]] || fail "signature sidecar came back with an empty body"

  sig_bytes="$(wc -c <"$WORKDIR/sig.bin" | tr -d ' ')"
  log "  OK — signature sidecar fetched directly ($sig_bytes bytes) — binary-response path exercised"
else
  log "  SKIP (known can't-build, see this step's header comment) — this stack's handshake \
reports no trust anchor (.trust is null), so no parcel is ever signed and GET /v1/signatures/\
$parcel_hash can only 404. The binary-response path (API Gateway's handling of a raw-bytes \
Lambda-proxy response) remains UNPINNED by this script until trust is deliberately established \
on this stack, out of band, as its own decision."
fi

# NOT INDEPENDENT EVIDENCE (restated from the header comment, where it applies again): step 2's
# franchise leg already fetched this exact parcel's object bytes via a presigned GET signed by
# the control plane, so on a CMK-configured stack that step alone already exercised the control
# plane's kms:Decrypt grant once, as a side effect of proving the round trip byte-for-byte. This
# explicit check is kept anyway so the KMS pin does not rest on that incidental shape — but do
# NOT read this check's pass together with step 2's pass as two independent confirmations of the
# control-plane grant; they are the same grant, on the same role, exercised by two different code
# paths that both happen to be in scope here.
log "  fetching object via presigned redirect (explicit control-plane read; see caveat above): GET ${PREFIX}/v1/objects/$parcel_hash"
read_code="$(curl_to_file "${ENDPOINT}${PREFIX}/v1/objects/$parcel_hash" "$WORKDIR/object.bin" \
  -H "Authorization: Bearer $TOKEN" -L)" || fail "object read request did not complete"

[[ "$read_code" == "200" ]] ||
  fail "GET /v1/objects/$parcel_hash (following the presigned redirect) expected 200, got $read_code"
[[ -s "$WORKDIR/object.bin" ]] || fail "object read came back with an empty body"

log "  OK — explicit control-plane read succeeded"

log "all checks passed"
