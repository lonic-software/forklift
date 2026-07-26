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
#
# Do not treat a clean run of THIS script's argument handling (see the definition-of-done demo in
# the FORK-60 Layer 3 report) as evidence for any of the above — it only proves the script itself
# doesn't crash on bad input or hang on a bad endpoint.

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
  VERIFY_WORKDIR   Scratch directory root (default: a fresh mktemp -d, removed on exit).
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

WORKDIR="${VERIFY_WORKDIR:-$(mktemp -d -t forklift-verify.XXXXXX)}"
trap 'rm -rf "$WORKDIR"' EXIT

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
# ---------------------------------------------------------------------------------------------

log "step 2/4 (checklist #2, #3): small + chunked (>= 8 MiB) round trip"

SRC_DIR="$WORKDIR/src"
FR_DIR="$WORKDIR/franchise"
mkdir -p "$SRC_DIR"

run_in "$SRC_DIR" "$FORKLIFT_BIN" prepare >/dev/null || fail "forklift prepare failed"

printf 'FORK-60 Layer 3 verify run — small file A (%s)\n' "$(date -u +%FT%TZ)" >"$SRC_DIR/small-a.txt"
printf 'FORK-60 Layer 3 verify run — small file B\n' >"$SRC_DIR/small-b.txt"

# 9 MiB of urandom: comfortably above the 8 MiB chunk threshold, with real (non-degenerate,
# non-compressible) content so chunking is exercised for real rather than as an artifact of an
# all-zero or otherwise-compressible fixture.
dd if=/dev/urandom of="$SRC_DIR/big.bin" bs=1M count=9 status=none

run_in "$SRC_DIR" "$FORKLIFT_BIN" load . >/dev/null || fail "forklift load failed"
run_in "$SRC_DIR" "$FORKLIFT_BIN" stack "FORK-60 Layer 3 verify run" >/dev/null || fail "forklift stack failed"
run_in "$SRC_DIR" "$FORKLIFT_BIN" config remote.url "${ENDPOINT}${PREFIX}" >/dev/null || fail "forklift config remote.url failed"
run_in "$SRC_DIR" "$FORKLIFT_BIN" config remote.token "$TOKEN" >/dev/null || fail "forklift config remote.token failed"

log "  lifting (small files promote synchronously; big.bin promotes asynchronously via the verifier)..."
run_in "$SRC_DIR" "$FORKLIFT_BIN" lift ||
  fail "lift failed. If this is the only thing that changed since Layer 2's LocalStack coverage passed, this is exactly the asynchronous/invisible verifier-KMS failure the design memo warns about (§3.1): the staging PUT succeeded, but promotion never completed."

log "  franchising into a second directory and comparing byte-for-byte..."
"$FORKLIFT_BIN" franchise "${ENDPOINT}${PREFIX}" "$FR_DIR" --token "$TOKEN" >/dev/null ||
  fail "franchise failed"

for f in small-a.txt small-b.txt big.bin; do
  cmp -s "$SRC_DIR/$f" "$FR_DIR/$f" ||
    fail "franchised $f differs from the source — byte-for-byte round trip broken"
done

log "  OK — small files and big.bin round-tripped identically"

# ---------------------------------------------------------------------------------------------
# Step 3/4 (checklist step 4) — after a successful lift, staging/ must be empty: commit_lift's
# final batch sweeps it. A non-empty staging/ after this point means either an abandoned session
# from a previous failed run (harmless but worth knowing about) or, more concerning, that the
# sweep itself silently failed.
# ---------------------------------------------------------------------------------------------

log "step 3/4 (checklist #4): staging/ is empty after a successful lift"

staging_key_count="$(aws s3api list-objects-v2 \
  --bucket "$BUCKET" --prefix "staging/" --query 'KeyCount' --output text 2>&1)" ||
  fail "aws s3api list-objects-v2 against bucket $BUCKET failed: $staging_key_count"

[[ "$staging_key_count" == "0" ]] ||
  fail "staging/ is not empty after a successful lift (KeyCount=$staging_key_count) — commit_lift's final sweep should have cleared it"

log "  OK — staging/ is empty"

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
# ---------------------------------------------------------------------------------------------

log "step 4/4: signature-sidecar fetch + explicit control-plane read"

history_json="$(run_in "$SRC_DIR" "$FORKLIFT_BIN" history -n 1 --json)" || fail "forklift history --json failed"
parcel_hash="$(printf '%s' "$history_json" | jq -r '.data.entries[0].parcel // empty' 2>/dev/null)"
[[ -n "$parcel_hash" ]] ||
  fail "could not extract a parcel hash from 'forklift history -n 1 --json': $history_json"

log "  fetching signature sidecar: GET ${PREFIX}/v1/signatures/$parcel_hash"
sig_code="$(curl_to_file "${ENDPOINT}${PREFIX}/v1/signatures/$parcel_hash" "$WORKDIR/sig.bin" \
  -H "Authorization: Bearer $TOKEN")" || fail "signature-sidecar request did not complete"

[[ "$sig_code" == "200" ]] ||
  fail "GET /v1/signatures/$parcel_hash expected 200 (direct bytes, no redirect), got $sig_code"
[[ -s "$WORKDIR/sig.bin" ]] || fail "signature sidecar came back with an empty body"

sig_bytes="$(wc -c <"$WORKDIR/sig.bin" | tr -d ' ')"
log "  OK — signature sidecar fetched directly ($sig_bytes bytes) — binary-response path exercised"

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
