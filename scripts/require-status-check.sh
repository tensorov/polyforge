#!/usr/bin/env bash
#
# require-status-check.sh - require the "ci" status check on master.
#
# Applies a required status check on the "ci" job for the master branch of
# tensorov/polyforge via gh api. Rulesets are PREFERRED; branch protection is
# the FALLBACK. RUN BY THE OPERATOR ONLY - never auto-applied in CI.
#
# Usage:
#   bash scripts/require-status-check.sh --dry-run   # report only, no changes
#   bash scripts/require-status-check.sh             # apply (admin token)
#
# Exit codes:
#   0 - success; or dry-run with current protection already blocking direct
#       pushes to master (master PR-only policy satisfied); or an existing
#       ruleset already requires the "ci" check (idempotent)
#   1 - dry-run: current protection would PERMIT direct pushes to master
#       (fail-closed on the master PR-only policy)
#   2 - error (gh/python3 missing, API failure, HTTP 403 scope problem,
#       usage error)
#
# Token scope for real application (the current PAT lacks it - HTTP 403):
#   fine-grained PAT with "Administration: write" on the repository, or a
#   classic PAT with the "repo" scope.

set -euo pipefail

REPO="tensorov/polyforge"
BRANCH="master"
CONTEXT="ci"
RULESET_NAME="master-required-status-checks"

DRY_RUN=0
API_OUT=""
API_ERR=""

usage() {
  cat <<'EOF'
usage: bash scripts/require-status-check.sh [--dry-run]

Require the "ci" status check on master of tensorov/polyforge.
Rulesets are preferred; branch protection is the fallback.
Run by the OPERATOR only - never auto-applied in CI.

  --dry-run   print current repo state, the master PR-only policy verdict and
              the exact payload that WOULD be applied. Makes no mutating API
              calls. Exits 1 if current protection would permit direct pushes
              to master (fail-closed); exits 0 if already satisfied or if the
              current protection blocks direct pushes.

Required token scope for real application:
  fine-grained PAT with "Administration: write" on the repository, or a
  classic PAT with the "repo" scope.
EOF
}

for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown argument: $arg" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if ! command -v python3 >/dev/null 2>&1; then
  echo "error: python3 is required (ruleset condition matching)" >&2
  exit 2
fi

GH_BIN="${GH_BIN:-}"
if [[ -z "$GH_BIN" ]]; then
  if command -v gh >/dev/null 2>&1; then
    GH_BIN="$(command -v gh)"
  elif [[ -x "$HOME/.local/bin/gh" ]]; then
    GH_BIN="$HOME/.local/bin/gh"
  else
    echo "error: gh CLI not found (PATH or ~/.local/bin)" >&2
    exit 2
  fi
fi

# call_api: run gh api, capture stdout/stderr into API_OUT/API_ERR, propagate
# the gh exit code. Failure is NOT fatal here - callers decide how to react.
call_api() {
  local out_file err_file rc
  out_file="$(mktemp)"
  err_file="$(mktemp)"
  if "$GH_BIN" api "$@" >"$out_file" 2>"$err_file"; then
    API_OUT="$(cat "$out_file")"
    API_ERR="$(cat "$err_file")"
  else
    rc=$?
    API_OUT="$(cat "$out_file")"
    API_ERR="$(cat "$err_file")"
  fi
  rm -f "$out_file" "$err_file"
  return "${rc:-0}"
}

SCOPE_HINT='required scope: fine-grained PAT with "Administration: write" on the repository,
             or a classic PAT with the "repo" scope (see scripts/README.md)'

die_403() {
  echo "error: HTTP 403 - Resource not accessible by personal access token" >&2
  echo "  $SCOPE_HINT" >&2
  exit 2
}

RULESET_PAYLOAD="$(cat <<'JSON'
{
  "name": "master-required-status-checks",
  "target": "branch",
  "enforcement": "active",
  "conditions": {
    "ref_name": {
      "include": [
        "refs/heads/master"
      ],
      "exclude": []
    }
  },
  "rules": [
    {
      "type": "required_status_checks",
      "parameters": {
        "strict": true,
        "contexts": [
          "ci"
        ]
      }
    }
  ]
}
JSON
)"

BRANCH_PROTECTION_PAYLOAD="$(cat <<'JSON'
{
  "required_status_checks": {
    "strict": true,
    "checks": [
      {
        "context": "ci",
        "app_id": null
      }
    ]
  },
  "enforce_admins": true,
  "required_pull_request_reviews": null,
  "restrictions": null
}
JSON
)"

# analyze: rulesets JSON + protection JSON (or ABSENT/UNREADABLE) -> JSON verdict
# {"status_covered": bool, "blocks": bool, "lines": [str]}
#   status_covered - an ACTIVE ruleset covering refs/heads/master already
#                    requires status check CONTEXT (idempotence).
#   blocks         - current protection would BLOCK direct pushes to master
#                    (active ruleset with required_pull_request or
#                    required_status_checks; or branch protection with
#                    required_pull_request_reviews or required_status_checks).
analyze() {
  python3 - "$1" "$2" "$CONTEXT" <<'PY'
import json
import sys
import fnmatch

rulesets = json.loads(sys.argv[1])
target = "refs/heads/master"
context = sys.argv[3]

status_covered = False
blocks = False
lines = []

for rs in rulesets:
    if rs.get("target") not in (None, "branch"):
        continue
    cond = rs.get("conditions") or {}
    ref = cond.get("ref_name") or {}
    includes = ref.get("include") or []
    excludes = ref.get("exclude") or []
    covers = any(fnmatch.fnmatch(target, p) for p in includes) and not any(
        fnmatch.fnmatch(target, p) for p in excludes
    )
    rules = rs.get("rules") or []
    types = [r.get("type", "") for r in rules]
    contexts = []
    for r in rules:
        if r.get("type") == "required_status_checks":
            contexts += (r.get("parameters") or {}).get("contexts") or []
    active = rs.get("enforcement") == "active"
    if covers and active:
        if "required_status_checks" in types and context in contexts:
            status_covered = True
        if "required_pull_request" in types or "required_status_checks" in types:
            blocks = True
    lines.append(
        "  - ruleset #%s '%s' enforcement=%s covers_master=%s rules=[%s] contexts=[%s]"
        % (rs.get("id"), rs.get("name"), rs.get("enforcement"), covers,
           ", ".join(types), ", ".join(contexts))
    )

protection = sys.argv[2]
if protection not in ("ABSENT", "UNREADABLE"):
    p = json.loads(protection)
    if (p.get("required_pull_request_reviews") is not None) or (
        p.get("required_status_checks") is not None
    ):
        blocks = True

print(json.dumps({"status_covered": status_covered, "blocks": blocks, "lines": lines}))
PY
}

# ---- gather current repo state (read-only) ----
if call_api "repos/$REPO/rulesets"; then
  RULESETS="$API_OUT"
else
  if [[ "$API_ERR" == *"HTTP 403"* ]]; then
    echo "error: rulesets read failed with HTTP 403" >&2
    die_403
  fi
  echo "error: rulesets read failed: $(echo "$API_ERR" | head -n1)" >&2
  exit 2
fi

PROTECTION_STATE="UNREADABLE"
if call_api "repos/$REPO/branches/$BRANCH/protection"; then
  PROTECTION="$API_OUT"
  PROTECTION_STATE="PRESENT"
elif [[ "$API_ERR" == *"HTTP 403"* ]]; then
  PROTECTION_STATE="UNREADABLE"
elif [[ "$API_ERR" == *"HTTP 404"* ]]; then
  PROTECTION_STATE="ABSENT"
else
  echo "error: branch protection read failed: $(echo "$API_ERR" | head -n1)" >&2
  exit 2
fi

if [[ "$PROTECTION_STATE" == "PRESENT" ]]; then
  VERDICT="$(analyze "$RULESETS" "$PROTECTION")"
else
  VERDICT="$(analyze "$RULESETS" "$PROTECTION_STATE")"
fi
STATUS_COVERED="$(echo "$VERDICT" | python3 -c 'import json,sys; print(json.load(sys.stdin)["status_covered"])')"
BLOCKS="$(echo "$VERDICT" | python3 -c 'import json,sys; print(json.load(sys.stdin)["blocks"])')"
RULESET_LINES="$(echo "$VERDICT" | python3 -c 'import json,sys; print("\n".join(json.load(sys.stdin)["lines"]))')"

if [[ "$STATUS_COVERED" == "True" ]]; then
  echo "already satisfied: a ruleset covering refs/heads/master requires status check '$CONTEXT'"
  echo "(if the ruleset is stale, update or delete it, then re-run)"
  exit 0
fi

# ---- dry-run: report only, no mutations ----
if [[ $DRY_RUN -eq 1 ]]; then
  echo "# polyforge required status check - DRY RUN (no changes made)"
  echo "# repo: $REPO | branch: $BRANCH | status-check context: $CONTEXT"
  echo ""
  echo "== current repo state =="
  if [[ "$RULESETS" == "[]" ]]; then
    echo "rulesets: none (GET /repos/$REPO/rulesets -> [])"
  else
    echo "rulesets:"
    echo "$RULESET_LINES"
  fi
  case "$PROTECTION_STATE" in
    PRESENT)
      echo "branch protection: present (GET /repos/$REPO/branches/$BRANCH/protection)"
      ;;
    ABSENT)
      echo "branch protection: none (HTTP 404)"
      ;;
    UNREADABLE)
      echo "branch protection: NOT readable with the current token (HTTP 403)"
      echo "  $(echo "$API_ERR" | head -n1)"
      echo "  $SCOPE_HINT"
      ;;
  esac
  echo ""
  echo "== master PR-only policy check (fail-closed) =="
  if [[ "$BLOCKS" == "True" ]]; then
    echo "current protection BLOCKS direct pushes to master (PR-only policy satisfied)"
    POLICY_EXIT=0
  else
    echo "current protection would PERMIT direct pushes to master"
    if [[ "$PROTECTION_STATE" == "UNREADABLE" ]]; then
      echo "  (branch protection unreadable with the current token; treated as unblocked)"
    fi
    echo "--> exiting non-zero (fail-closed on the master PR-only policy)"
    POLICY_EXIT=1
  fi
  echo ""
  echo "== what WOULD be applied =="
  echo "preferred: POST /repos/$REPO/rulesets"
  echo "$RULESET_PAYLOAD"
  echo ""
  echo "fallback (only if ruleset creation is unavailable):"
  echo "  PUT /repos/$REPO/branches/$BRANCH/protection"
  echo "$BRANCH_PROTECTION_PAYLOAD"
  exit "$POLICY_EXIT"
fi

# ---- apply mode (operator, admin-scoped token) ----
if [[ "$BLOCKS" == "True" ]]; then
  echo "note: current protection already blocks direct pushes to master"
else
  echo "warning: current protection would permit direct pushes to master"
  echo "         the protection applied below will block them (master PR-only policy)"
fi

echo "creating ruleset '$RULESET_NAME' (preferred)..."
if call_api --method POST "repos/$REPO/rulesets" --input - <<<"$RULESET_PAYLOAD"; then
  echo "ok: ruleset applied (id $(echo "$API_OUT" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("id"))'))"
  exit 0
fi
if [[ "$API_ERR" == *"HTTP 403"* ]]; then
  echo "error: ruleset creation failed with HTTP 403" >&2
  die_403
fi
echo "warning: ruleset creation failed ($(echo "$API_ERR" | head -n1)); falling back to branch protection" >&2

if call_api --method PUT "repos/$REPO/branches/$BRANCH/protection" --input - <<<"$BRANCH_PROTECTION_PAYLOAD"; then
  echo "ok: branch protection applied"
  exit 0
fi
if [[ "$API_ERR" == *"HTTP 403"* ]]; then
  echo "error: branch protection failed with HTTP 403" >&2
  die_403
fi
echo "error: branch protection failed: $(echo "$API_ERR" | head -n1)" >&2
exit 2
