#!/usr/bin/env bash
# PolyForge composite action entrypoint (C3.1).
#
# Three phases, in order:
#   1. Prefer the pre-built workspace binary ./target/debug/polyforge-cli
#      (the repo's CI builds it before this action runs); fall back to
#      `cargo install polyforge-cli --locked` (crates.io v0.1.0) ONLY when a
#      workspace binary is absent.
#   2. Run `polyforge-cli gate <task-id> --required <req>` with PF_LEDGER and
#      PF_EVIDENCE_DIR set, under an env-strip allowlist (taiki-e/install-action
#      composite pattern - NOT the Docker-based cargo-deny-action pattern).
#   3. Verify the Merkle chain against the committed anchor
#      .pf/ledger.jsonl.anchor: compare the head_hash from `ledger tail`
#      against the anchor head_hash AND the entry_count via `wc -l`
#      (cmd_ledger_tail prints ONLY the head hash, never the count). Fail
#      closed with non-zero exit on either mismatch (anchor tamper).
set -euo pipefail

# BASH_FUNC_ guard: exported bash functions surface as BASH_FUNC_<name>%%
# environment variables and can inject code into any subshell this script (or
# a child shell) starts. Refuse to run when any are present; the polyforge-cli
# invocation below additionally strips the environment via `env -i`.
if env | grep -q '^BASH_FUNC_'; then
  echo "::error::BASH_FUNC_* exported functions present in the environment; refusing to run" >&2
  exit 1
fi

: "${GITHUB_WORKSPACE:?GITHUB_WORKSPACE must be set (repo root)}"
: "${TASK_ID:?TASK_ID must be set}"
: "${LEDGER_PATH:?LEDGER_PATH must be set}"
: "${EVIDENCE_DIR:?EVIDENCE_DIR must be set}"
REQUIRED="${REQUIRED:-verified,validated}"

LEDGER="$GITHUB_WORKSPACE/$LEDGER_PATH"
EVIDENCE="$GITHUB_WORKSPACE/$EVIDENCE_DIR"
ANCHOR="$GITHUB_WORKSPACE/.pf/ledger.jsonl.anchor"

# --- Phase 1: binary selection (workspace binary preferred) ---
WS_BIN="$GITHUB_WORKSPACE/target/debug/polyforge-cli"
if [[ -x "$WS_BIN" ]]; then
  CLI="$WS_BIN"
  echo "using workspace binary: $CLI"
else
  echo "workspace binary $WS_BIN not found; installing polyforge-cli from crates.io"
  cargo install polyforge-cli --locked
  CLI="$(command -v polyforge-cli)"
  echo "using installed binary: $CLI"
fi

# --- Phase 2: gate (env-strip allowlist; env -i also drops BASH_FUNC_*) ---
env -i \
  PATH="$PATH" \
  HOME="$HOME" \
  CARGO_HOME="${CARGO_HOME:-}" \
  RUSTUP_HOME="${RUSTUP_HOME:-}" \
  PF_LEDGER="$LEDGER" \
  PF_EVIDENCE_DIR="$EVIDENCE" \
  "$CLI" gate "$TASK_ID" --required "$REQUIRED"
echo "gate PASSED for task $TASK_ID"

# --- Phase 3: anchor verification (fail-closed on tamper) ---
if [[ ! -f "$ANCHOR" ]]; then
  echo "::error::anchor file missing: $ANCHOR" >&2
  exit 1
fi

# cmd_ledger_tail prints ONLY the head hash - the entry count MUST come from
# wc -l over the ledger file itself.
HEAD_HASH="$(env -i PATH="$PATH" HOME="$HOME" PF_LEDGER="$LEDGER" "$CLI" ledger tail | tr -d '[:space:]')"
ENTRY_COUNT="$(wc -l < "$LEDGER" | tr -d '[:space:]')"

# Parse the fixed anchor shape {"entry_count":N,"head_hash":"<64hex>"}; a
# malformed anchor yields empty values and fails the comparisons below.
ANCHOR_COUNT="$(sed -n 's/.*"entry_count":\([0-9][0-9]*\).*/\1/p' "$ANCHOR" | tr -d '[:space:]')"
ANCHOR_HASH="$(sed -n 's/.*"head_hash":"\([0-9a-f]\{64\}\)".*/\1/p' "$ANCHOR" | tr -d '[:space:]')"

if [[ -z "$ANCHOR_COUNT" || -z "$ANCHOR_HASH" ]]; then
  echo "::error::anchor file malformed: $ANCHOR" >&2
  exit 1
fi
if [[ "$HEAD_HASH" != "$ANCHOR_HASH" ]]; then
  echo "::error::anchor mismatch: ledger head $HEAD_HASH != anchor head $ANCHOR_HASH" >&2
  exit 1
fi
if [[ "$ENTRY_COUNT" != "$ANCHOR_COUNT" ]]; then
  echo "::error::anchor mismatch: ledger entry_count $ENTRY_COUNT != anchor entry_count $ANCHOR_COUNT" >&2
  exit 1
fi
echo "anchor verified ($ENTRY_COUNT entries, head $HEAD_HASH)"
