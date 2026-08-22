# Security Policy

PolyForge is a tamper-evident evidence ledger for AI-driven engineering
workflows. This policy describes which versions receive security fixes and how
to report a vulnerability.

## Supported Versions

| Version | Supported |
| ------- | --------- |
| 0.1.x   | ✅ |

Only the latest minor release of the current series receives security fixes.
Newer minor versions supersede older ones, so always upgrade to the latest
release and report issues against it.

## Reporting a Vulnerability

Please do **not** disclose vulnerabilities publicly before we have had a
chance to address them. Do not open a public issue and do not broadcast the
details on mailing lists or social media.

**Primary path** — use GitHub private vulnerability reporting:

1. Open the repository's **Security** tab.
2. Click **Report a vulnerability**.
3. Provide a clear description, affected versions, and — if available — a
   minimal reproduction.

**Fallback** — if private vulnerability reporting is unavailable for this
repository, open a regular issue instead and mark it as security-related.
Public disclosure is always the last resort, only after a fix is available.

### Response window

We aim to acknowledge reports within **3 business days** and to provide an
initial assessment (reproducibility, severity, target timeline) within
**1 week**.

### Fixes and release notes

Security fixes are released as **patch versions** of the affected series. The
release notes for each fix document the implications for the evidence chain:
the affected chain-tail hash and whether existing ledger entries need to be
re-validated. Always verify ledger integrity after applying a security update.

## Scope

In scope: the `polyforge-*` crates in this repository, the `polyforge-cli`
binary, the MCP server, and the evidence ledger implementation.

Out of scope: user code that embeds PolyForge, third-party dependencies
(please report those to their respective projects), and misconfiguration of a
user's own ledger storage.

## Threat model

This section states what PolyForge protects, what it does not protect, and the
attacker classes it is designed against. It is a living document: update it
whenever the trust boundaries change.

### Assets

- **Ledger integrity.** The append-only Merkle chain is the root of trust.
  Every entry commits to the hash of the previous entry, so tampering with one
  byte of any entry breaks the chain and every subsequent gate or verify fails
  with `LedgerIntegrity`.
- **Gate verdicts.** A gate result is only as good as the chain it was computed
  from. A passing gate must mean the required evidence states were reached on
  an unmodified chain.
- **Attestation provenance.** Each entry records who claimed, which tool
  attested, and (for evaluations) the experiment, run, model fingerprint, and
  budget. Provenance is what makes a verdict auditable.

### Attacker classes

- **Repo-writer.** Anyone who can push to the repository. They can rewrite the
  ledger and re-commit the anchor, so tamper evidence is only meaningful within
  a trusted checkout (see "Not protected" below).
- **MCP network attacker.** Anyone who can reach the MCP TCP endpoint. The TCP
  transport is token-gated and loopback-default, so this class is limited to
  processes on the same host unless the operator changes the bind address.
- **The model itself.** The model is the primary untrusted party. It can only
  append `ModelClaim` entries over MCP; it cannot create `Verified`, `Refuted`,
  or `Validated` entries, and it cannot self-promote.
- **Tool-allowlist bypass.** An attacker who tries to get arbitrary code
  executed through the toolrunner. The toolrunner runs only allowlisted
  binaries (cargo/rustc/gcc) with typed arguments and no shell.

### Trust boundaries

- The ledger file and the checkout it lives in are trusted. The chain proves
  the ledger was not rewritten; it does not prove the checkout is authentic.
- The operator and the toolrunner are trusted to produce attestations. The
  model is not trusted.
- The MCP stdio transport is trusted because it inherits the OS pipe
  permissions of the process that spawned it. The TCP transport is trusted only
  up to the token gate.
- External anchoring (Sigstore, CI attestation on the tail hash) is roadmap
  Phase 3 and is not a trust boundary today.

### Protected vs not protected

| Protected | Not protected |
| --------- | ------------- |
| Append-only Merkle chain: one-byte tamper breaks the chain and fails closed with `LedgerIntegrity` | Trusted checkout: no external anchoring of the tail hash until Phase 3; a repo-writer can rewrite the ledger and re-commit the anchor |
| `promote` gatekeeper: the single promotion path in `polyforge-core`; models over MCP can only append `ModelClaim` | No sandbox: the tool allowlist is not a sandbox; an allowlisted binary runs with the privileges of the process that invoked it |
| State injection: `ToolAttestation`, `Validation`, `EvalAttestation`, and `Discrepancy` are rejected at the MCP server, so a model cannot inject promoted states | Forward-only state machine: promotion cannot be rolled back, so a mistaken or malicious attestation cannot be undone in place |
| Fail-closed gate: a corrupted chain or unmet required state exits non-zero and never fabricates a bundle | |
| Fail-closed TCP token: `PF_MCP_TRANSPORT=tcp` requires `PF_MCP_TOKEN` and defaults to loopback | |

### What the allowlist does and does not guarantee

The tool allowlist guarantees a bounded set of fixed-name binaries, typed arguments without
a shell, a wall-clock timeout, and attribution (environment fingerprint, git state). It does
NOT guarantee that attestations are TRUE: allowlisted tools execute project code
(`conftest.py`, `vite.config.ts`, `eslint.config.js`, `build.rs`) written by the verified
agent. Attestation truth comes from mutation testing, keyed gates, and the operator
Validated stage.
