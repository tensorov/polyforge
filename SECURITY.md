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
