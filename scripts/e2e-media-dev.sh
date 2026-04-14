#!/usr/bin/env bash
# Phase 16.1 headless E2E smoke test against roz-api-dev.fly.dev.
#
# Verifies, end-to-end, that:
#  1. The CLI can authenticate with the dev gRPC endpoint.
#  2. AnalyzeMedia RPC streams back text/usage/done chunks.
#  3. Inline-bytes upload works (CLI → 12 MiB tonic cap → handler → fetcher
#     (skipped for inline) → Gemini via PAIG-or-direct → SSE → client).
#  4. SSRF rejection propagates: file_uri to a blocked IP surfaces as
#     FailedPrecondition.
#  5. Bad mime propagates: application/json is rejected client-side.
#
# Required env:
#   ROZ_API_URL   defaults to https://roz-api-dev.fly.dev
#   ROZ_API_KEY   dev API key (CLI Bearer auth)
#
# Usage:
#   scripts/e2e-media-dev.sh
#   ROZ_API_URL=https://staging.example.com scripts/e2e-media-dev.sh

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ROZ_API_URL="${ROZ_API_URL:-https://roz-api-dev.fly.dev}"
FIXTURE_PNG="${ROOT}/scripts/e2e-fixtures/gradient-16x16.png"
FIXTURE_MP4="${ROOT}/scripts/e2e-fixtures/tiny-1s-128x128.mp4"
FIXTURE_WAV="${ROOT}/scripts/e2e-fixtures/tiny-1s-440hz.wav"
# back-compat alias (earlier tests only referenced image)
FIXTURE="$FIXTURE_PNG"
PASS=0
FAIL=0

if [ -z "${ROZ_API_KEY:-}" ]; then
  echo "ERROR: ROZ_API_KEY is not set. Export a dev API key before running." >&2
  exit 1
fi
for f in "$FIXTURE_PNG" "$FIXTURE_MP4" "$FIXTURE_WAV"; do
  if [ ! -f "$f" ]; then
    echo "ERROR: fixture not found: $f" >&2
    exit 1
  fi
done

export ROZ_API_URL
export ROZ_API_KEY

BIN="${ROZ_BIN:-target/release/roz}"
if [ ! -x "$BIN" ]; then
  BIN="target/debug/roz"
fi
if [ ! -x "$BIN" ]; then
  echo "Building roz (release)..."
  cargo build --release --bin roz >/dev/null
  BIN="target/release/roz"
fi

banner() { printf '\n== %s ==\n' "$1"; }

pass() { printf '  PASS: %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  FAIL: %s\n' "$1"; FAIL=$((FAIL + 1)); }

# ---------------------------------------------------------------------------
# 1. Happy path — inline PNG → streamed analysis
# ---------------------------------------------------------------------------
banner "1. Inline PNG happy path"
OUT=$(mktemp)
trap 'rm -f "$OUT"' EXIT
set +e
"$BIN" media analyze "$FIXTURE" \
  --prompt "Describe the colors in this 16x16 image in one sentence." \
  --mime image/png \
  --json \
  > "$OUT" 2>&1
RC=$?
set -e

if [ $RC -eq 0 ]; then pass "exit code 0"; else fail "exit code $RC (output below)"; cat "$OUT"; fi

if grep -q '"type":"text_delta"' "$OUT" || grep -q '"type": "text_delta"' "$OUT"; then
  pass "received text_delta chunks"
else
  fail "no text_delta chunks in output"; head -20 "$OUT"
fi

if grep -q '"type":"usage"' "$OUT" || grep -q '"type": "usage"' "$OUT"; then
  pass "received usage chunk"
else
  fail "no usage chunk"
fi

if grep -q '"type":"done"' "$OUT" || grep -q '"type": "done"' "$OUT"; then
  pass "received done chunk"
else
  fail "no done chunk — stream did not terminate cleanly"
fi

# Extract concatenated text and assert non-empty.
TEXT=$(grep -o '"text":"[^"]*"' "$OUT" | sed 's/"text":"//; s/"$//' | tr -d '\n')
if [ -n "$TEXT" ] && [ ${#TEXT} -gt 10 ]; then
  pass "non-trivial text response (${#TEXT} chars)"
  printf '    sample: %.120s...\n' "$TEXT"
else
  fail "text response empty or too short"
fi

# ---------------------------------------------------------------------------
# 1b. Video happy path — 1s 128x128 H.264 test pattern
# ---------------------------------------------------------------------------
banner "1b. Inline video/mp4 happy path"
set +e
"$BIN" media analyze "$FIXTURE_MP4" \
  --prompt "In one short sentence, what kind of test pattern is in this video?" \
  --mime video/mp4 \
  --json \
  > "$OUT" 2>&1
RC=$?
set -e

if [ $RC -eq 0 ]; then pass "video exit code 0"; else fail "video exit code $RC"; cat "$OUT"; fi
if grep -q '"type":"text_delta"' "$OUT" || grep -q '"type": "text_delta"' "$OUT"; then
  pass "video: text_delta chunks received"
else
  fail "video: no text_delta chunks"; head -10 "$OUT"
fi
if grep -q '"type":"done"' "$OUT" || grep -q '"type": "done"' "$OUT"; then
  pass "video: done chunk received"
else
  fail "video: no done chunk"
fi
VTEXT=$(grep -o '"text":"[^"]*"' "$OUT" | sed 's/"text":"//; s/"$//' | tr -d '\n')
if [ -n "$VTEXT" ] && [ ${#VTEXT} -gt 10 ]; then
  pass "video: non-trivial response (${#VTEXT} chars)"
  printf '    sample: %.120s...\n' "$VTEXT"
else
  fail "video: response empty or too short"
fi

# ---------------------------------------------------------------------------
# 1c. Audio happy path — 1s 440Hz sine tone, 16kHz mono
# ---------------------------------------------------------------------------
banner "1c. Inline audio/wav happy path"
set +e
"$BIN" media analyze "$FIXTURE_WAV" \
  --prompt "In one sentence, describe what you hear (tone, pitch, duration)." \
  --mime audio/wav \
  --json \
  > "$OUT" 2>&1
RC=$?
set -e

if [ $RC -eq 0 ]; then pass "audio exit code 0"; else fail "audio exit code $RC"; cat "$OUT"; fi
if grep -q '"type":"text_delta"' "$OUT" || grep -q '"type": "text_delta"' "$OUT"; then
  pass "audio: text_delta chunks received"
else
  fail "audio: no text_delta chunks"; head -10 "$OUT"
fi
if grep -q '"type":"done"' "$OUT" || grep -q '"type": "done"' "$OUT"; then
  pass "audio: done chunk received"
else
  fail "audio: no done chunk"
fi
ATEXT=$(grep -o '"text":"[^"]*"' "$OUT" | sed 's/"text":"//; s/"$//' | tr -d '\n')
if [ -n "$ATEXT" ] && [ ${#ATEXT} -gt 10 ]; then
  pass "audio: non-trivial response (${#ATEXT} chars)"
  printf '    sample: %.120s...\n' "$ATEXT"
else
  fail "audio: response empty or too short"
fi

# ---------------------------------------------------------------------------
# 2. SSRF rejection — file_uri pointing at AWS IMDS
# ---------------------------------------------------------------------------
banner "2. SSRF rejection (AWS IMDS)"
set +e
"$BIN" media analyze "https://169.254.169.254/latest/meta-data/iam" \
  --prompt "describe" \
  --mime image/png \
  --json \
  > "$OUT" 2>&1
RC=$?
set -e

if [ $RC -eq 3 ]; then
  pass "exit code 3 (gRPC Status returned)"
else
  fail "expected exit 3, got $RC"; cat "$OUT"
fi

if grep -qi 'FailedPrecondition' "$OUT"; then
  pass "FailedPrecondition surfaced to client"
else
  fail "expected FailedPrecondition in output"; head -10 "$OUT"
fi

if grep -qi 'blocked' "$OUT"; then
  pass "message identifies block reason"
else
  fail "expected 'blocked' in message"
fi

# ---------------------------------------------------------------------------
# 3. Bad mime — client-side rejection
# ---------------------------------------------------------------------------
banner "3. Client-side mime rejection"
set +e
"$BIN" media analyze "$FIXTURE" \
  --prompt "x" \
  --mime application/json \
  --json \
  > "$OUT" 2>&1
RC=$?
set -e

# Client-side validation returns anyhow bail → exit 1
if [ $RC -eq 1 ]; then
  pass "exit code 1 (client-side rejection)"
else
  fail "expected exit 1, got $RC"; head -10 "$OUT"
fi

if grep -qi 'unsupported mime' "$OUT"; then
  pass "rejection message mentions unsupported mime"
else
  fail "expected 'unsupported mime' in error"
fi

# ---------------------------------------------------------------------------
# 4. Unknown model_hint — server-side InvalidArgument
# ---------------------------------------------------------------------------
banner "4. Unknown model_hint rejection"
set +e
"$BIN" media analyze "$FIXTURE" \
  --prompt "x" \
  --mime image/png \
  --model-hint "claude-opus" \
  --json \
  > "$OUT" 2>&1
RC=$?
set -e

if [ $RC -eq 3 ]; then
  pass "exit code 3 (server returned Status)"
else
  fail "expected exit 3, got $RC"; head -10 "$OUT"
fi

if grep -qi 'InvalidArgument' "$OUT"; then
  pass "InvalidArgument surfaced"
else
  fail "expected InvalidArgument in output"
fi

if grep -qi 'model_hint not supported' "$OUT"; then
  pass "message identifies unknown hint"
else
  fail "expected 'model_hint not supported' in message"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
banner "Summary"
printf '  %d passed, %d failed\n' "$PASS" "$FAIL"
if [ $FAIL -gt 0 ]; then
  exit 1
fi
