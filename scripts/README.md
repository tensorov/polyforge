# scripts/README.md

## require-status-check.sh

Requires the `ci` status check on the master branch of tensorov/polyforge so
CI runs are mandatory before code lands. Uses `gh api` (rulesets are PREFERRED,
branch protection is the FALLBACK).

The script is RUN BY THE OPERATOR (you). It is never auto-applied in CI: a
required status check is a repository setting, not a workflow step, and its
whole point is that an operator - not a machine - decides when repository
protection changes.

### Usage

```sh
# report-only: prints the current repo state, the policy verdict and the exact
# payload that WOULD be applied; makes no changes. Safe to run any time.
bash scripts/require-status-check.sh --dry-run

# real application (requires an admin-scoped token, see below)
bash scripts/require-status-check.sh
```

### Token requirement (admin scope)

- fine-grained PAT with "Administration: write" on the repository, or
- classic PAT with the `repo` scope.

The current default PAT gets HTTP 403 "Resource not accessible by personal
access token" on branch-protection reads. The script NEVER silently no-ops on
403: it prints the required scope and exits non-zero.

### Current repo state (2026-08-13)

- Rulesets: `[]` - `GET /repos/tensorov/polyforge/rulesets` returns no rulesets.
- Branch protection: HTTP 403 with the current PAT (unreadable; the script
  prints the scope hint and exits non-zero).

### Master PR-only policy

master REQUIRES pull requests (no direct pushes). The `mutations`/`coverage`
push triggers in `.github/workflows/ci.yml` are defense-in-depth, but the
protection that actually blocks direct pushes is branch protection / a ruleset.
The script prints whether the current protection would permit direct pushes to
master and exits non-zero if it would (fail-closed on the PR-only policy).

### Exit codes

| Code | Meaning |
| ---- | ------- |
| 0    | Success: protection applied; or dry-run with current protection already blocking direct pushes to master (PR-only policy satisfied); or an existing ruleset already requires the `ci` check (idempotent). |
| 1    | Dry-run only: current protection would PERMIT direct pushes to master (fail-closed on the master PR-only policy). |
| 2    | Error: gh/python3 missing, API failure, HTTP 403 scope problem, or usage error. |

### How it works

1. Reads `GET /repos/tensorov/polyforge/rulesets`; if an ACTIVE ruleset already
   covers `refs/heads/master` with a `required_status_checks` rule whose
   contexts include `ci`, reports and exits 0 (idempotent).
2. PREFERRED: `POST /repos/tensorov/polyforge/rulesets` - ruleset
   `master-required-status-checks`, `target: branch`, `enforcement: active`,
   conditions on `refs/heads/master` (fnmatch pattern), one rule:
   `required_status_checks` with `strict: true` and `contexts: ["ci"]`.
   The `required_status_checks` rule takes `integration_id` for app-source
   pinning (not `app_id`, which applies only to the branch-protection
   fallback). We omit it, so any GitHub App may report the check.
3. FALLBACK (only if ruleset creation fails or is unavailable):
   `PUT /repos/tensorov/polyforge/branches/master/protection` with
   `required_status_checks: {"strict": true, "checks": [{"context": "ci",
   "app_id": null}]}`, `enforce_admins: true`,
   `required_pull_request_reviews: null`, `restrictions: null`.

The status-check context is the CI job name `ci` (unique across workflows -
only one workflow exists in this repo). `strict: true` requires branches to be
up to date before merging.

### Notes

- python3 is required (ruleset condition fnmatch matching).
- `--dry-run` makes no mutating API calls (GET only).
- Real application with an admin-scoped token is an OPERATOR follow-up; the
  current default PAT cannot apply it. See
  `.omo/plans/roadmap-phase0-trust-hardening.md` todo 8 (C3.3).
