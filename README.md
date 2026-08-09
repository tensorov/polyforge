# PolyForge

[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![CI](https://github.com/tensorov/polyforge/actions/workflows/ci.yml/badge.svg)](https://github.com/tensorov/polyforge/actions)
[![Rust 1.85](https://img.shields.io/badge/rust-1.85-informational?logo=rust)](https://www.rust-lang.org)

## Demo

[![PolyForge — the gate demo, live](assets/readme/hero.gif)](assets/readme/hero.svg)

PolyForge is a tamper-evident evidence ledger for AI-driven engineering workflows: models
record claims, allowlisted tools attest them, and operators gate on the resulting chain.

## Proof

Everything in this README is covered by the workspace test suite (45 tests across the four
crates) and by the CLI/MCP smoke and end-to-end harnesses.

Run it yourself: `cargo build --workspace && cargo test --workspace` (see [Build from source](#build-from-source)).

## Why "PolyForge"

> **Why "PolyForge"**: *poly* — many agents working together; *forge* — the place where their raw claims are forged into verifiable evidence: through the gate, a model's claim becomes a fact only when a real tool run proves it. (And yes — the name is also a wink: this is the forge that makes forged history impossible.)

## Architecture

Workspace of four crates (edition 2021, rust-version 1.85, Rust toolchain 1.95.0):

| Crate                   | Responsibility                                                                                   |
| ----------------------- | ------------------------------------------------------------------------------------------------ |
| `polyforge-core`        | Evidence model: tri-state entries, promotion rules, the append-only Merkle ledger, and deterministic gate evaluation. |
| `polyforge-toolrunner`  | Allowlisted tool runner: only allowlisted binaries (cargo/rustc/gcc), typed arguments, no shell, per-command environment fingerprint. |
| `polyforge-mcp`         | Model Context Protocol server (rmcp): the interface models use to append claims and query gates. |
| `polyforge-cli`         | Operator CLI: init, append, ledger inspection, and gate execution over a local ledger.           |

The CLI binary is named `polyforge-cli` (the crate name). All examples in this README use
it directly; `alias pf=polyforge-cli` if you prefer the short name.

## Evidence lifecycle

Evidence is tri-state and only ever moves forward:

![Evidence lifecycle: ModelClaimed → Verified → Validated](assets/readme/lifecycle.svg)

- `model_claim` — the model records a claim about its own work. Creates a `ModelClaimed` entry.
- `tool_attestation` — an allowlisted tool run produces a `ToolAttestation` entry that
  promotes the task's `ModelClaimed` entry to `Verified`.
- `validation` — an operator validation produces a `Validation` entry that promotes the
  task's `Verified` entry to `Validated`.

A gate can require `verified` or `validated` (see below).

## Models can never self-produce `Verified`

- The CLI accepts only three kinds: `model_claim`, `tool_attestation`, `validation`.
  `model_claim` can only create a new `ModelClaimed` entry.
- `tool_attestation` does not append a bare entry: it locates the task's latest
  `ModelClaimed` entry and promotes it. With no prior claim the append is rejected
  (models cannot self-promote).
- The MCP `evidence_append` tool accepts `kind=ModelClaim` **only**; `ToolAttestation` and
  `Validation` are rejected at the server — models connected over MCP cannot create
  `Verified` or `Validated` entries at all.
- Promotion is enforced by the single `promote` gatekeeper in `polyforge-core`; a model's
  only path toward `Verified` is an allowlisted tool attestation.

## Build from source

The workspace requires a Rust toolchain of at least version 1.85 (edition 2021; developed
against toolchain 1.95.0). Building the workspace in release mode produces the
`polyforge-cli` binary:

```sh
cargo build --release
```

The binary lands in `target/release/polyforge-cli`. Build and test the whole workspace:

```sh
cargo build --workspace
cargo test --workspace
```

## Quick start

![PolyForge CLI demo](assets/readme/cli-demo.svg)

The sequence below was run verbatim against the real binary, every command exiting 0:

```sh
export PF_LEDGER=/tmp/pf-demo/ledger.jsonl
export PF_EVIDENCE_DIR=/tmp/pf-demo/evidence/
polyforge-cli init
polyforge-cli append model_claim "claim datum" --task demo --commit abc123 --diff d1
polyforge-cli append tool_attestation "ran" --task demo
polyforge-cli ledger tail
polyforge-cli gate demo --required verified
polyforge-cli append validation "operator check" --task demo
polyforge-cli gate demo --required validated
```

What each step does:

- `init` creates the ledger and prints `created ledger at <path>`.
- `append model_claim ...` records a claim by the model; `append tool_attestation ...`
  promotes the task's claim to `Verified`; `append validation ...` promotes it further to
  `Validated`. Each `append` prints `appended entry N`.
- `ledger tail` prints the 64-hex SHA-256 tail hash of the Merkle chain.
- `gate demo --required verified` writes `gate-demo.jsonl` plus `gate-demo.manifest.json`
  and prints `gate PASSED for task demo`, exiting 0.
- On gate failure (required state not reached) the gate exits non-zero and writes no
  bundle.

The `--required` flag takes a comma-list such as `verified,validated`.

Environment variables:

- `PF_LEDGER` — ledger path (default `.omo/ledger.jsonl`).
- `PF_EVIDENCE_DIR` — directory for gate bundles and manifests (default `.omo/evidence/`).

## Running a gate

```sh
polyforge-cli gate <task_id> --required verified,validated
```

- **PASS** (the task's evidence chain satisfies the required state): writes the evidence
  bundle `gate-<task_id>.jsonl` (the task's ledger records, in sequence) plus
  `gate-<task_id>.manifest.json` with `task_id`, `tail_hash`, `passed: true`,
  `bundle_sha256`, and `tool_versions`. The `tail_hash` equals the output of
  `polyforge-cli ledger tail`.
- **FAIL** (required state not reached): exits non-zero and never writes a bundle — at
  most a manifest with `passed: false` and `bundle_sha256: null`. A corrupted chain exits
  non-zero and writes nothing.
- `polyforge-cli gate` is reproducible: a second PASS run produces a byte-identical bundle
  and an identical `bundle_sha256`.

## Connecting the MCP army

`polyforge-mcp` is an rmcp server speaking MCP over stdio by default:

```sh
polyforge-mcp
```

Transport configuration:

- `PF_MCP_TRANSPORT=stdio` (default) — MCP over standard input/output.
- `PF_MCP_TRANSPORT=tcp` — MCP over TCP; bind address via `PF_MCP_ADDR`
  (default `127.0.0.1:18888`).
- `PF_MCP_LEDGER` — ledger path (default `.omo/ledger.jsonl`).

Four tools:

| Tool               | Kind accepted      | Notes                                                            |
| ------------------ | ------------------ | ---------------------------------------------------------------- |
| `evidence_append`  | `ModelClaim` only  | Models can only append claims — never attestations/validations.  |
| `evidence_verify`  | —                  | Runs an allowlisted tool to verify a claim; arbitrary binaries are never executed. |
| `gate_evaluate`    | —                  | Evaluate the gate for a task (read-only).                        |
| `gate_report`      | —                  | Report gate/evidence state (read-only).                          |

## Agent integration

Each agent below registers the same `polyforge-mcp` server over stdio. The server reads
`PF_MCP_TRANSPORT` (default `stdio`), `PF_MCP_ADDR` (default `127.0.0.1:18888`), and
`PF_MCP_LEDGER` (default `.omo/ledger.jsonl`) — see [Connecting the MCP army](#connecting-the-mcp-army).

### OpenCode

`opencode.json` (project root or `~/.config/opencode/`):

```json
{
  "mcp": {
    "polyforge": {
      "type": "local",
      "command": ["polyforge-mcp"],
      "env": {
        "PF_MCP_TRANSPORT": "stdio"
      }
    }
  }
}
```

### Claude Code

```sh
claude mcp add polyforge -- polyforge-mcp
```

Or `.mcp.json` in the project root:

```json
{
  "mcpServers": {
    "polyforge": {
      "command": "polyforge-mcp",
      "args": [],
      "env": {
        "PF_MCP_TRANSPORT": "stdio"
      }
    }
  }
}
```

### Codex

`~/.codex/config.toml`:

```toml
[mcp_servers.polyforge]
command = "polyforge-mcp"
env = { PF_MCP_TRANSPORT = "stdio" }
```

### OpenClaw

`~/.openclaw/config.json` — example, adjust path:

```json
{
  "mcpServers": {
    "polyforge": {
      "command": "polyforge-mcp",
      "args": [],
      "env": {
        "PF_MCP_TRANSPORT": "stdio"
      }
    }
  }
}
```

Make sure `polyforge-mcp` is on `PATH`, or point `command` at the full path to
`target/release/polyforge-mcp`.

## Tamper / rewind guarantee

The ledger is an append-only Merkle chain: every entry commits to the hash of the previous
entry. Rewinding the file or tampering with **one byte** of any entry breaks the chain, and
any subsequent `polyforge-cli gate` or `evidence_verify` fails with `LedgerIntegrity`.
Failed integrity checks never fabricate a bundle or manifest — the failure is surfaced and
nothing is written. This is exercised end-to-end by the army harness (byte-flip in the
tail entry → gate exits non-zero with `ledger integrity broken at seq …`, no bundle, no
manifest).

## Examples

A runnable end-to-end walkthrough of the tri-state lifecycle lives in
[`crates/polyforge-core/examples/ledger_flow.rs`](crates/polyforge-core/examples/ledger_flow.rs):
it creates a temp ledger, appends a `ModelClaim`, applies a tool attestation via `promote`
(→ `Verified`), runs an `evaluate_complete` gate, and cleans up — all with exit 0.

```sh
cargo run -p polyforge-core --example ledger_flow
```

An optional ledger path argument selects the ledger file (a unique temp path is used by
default).

## Performance

Measured on this machine with the release binary and a fresh ledger in `/tmp/pf-bench`:

| Scenario | Value | How to reproduce |
| -------- | ----- | ---------------- |
| 100-task full chain (300 ledger appends) | 0.510 s | `time ( for i in $(seq 1 100); do polyforge-cli append model_claim "bench claim $i" --task task$i --commit c$i --diff d$i >/dev/null; polyforge-cli append tool_attestation "ran" --task task$i >/dev/null; polyforge-cli append validation "op" --task task$i >/dev/null; done )` |
| 100 gate checks over a 300-entry ledger | 0.500 s | `time ( for i in $(seq 1 100); do polyforge-cli gate task$i --required validated >/dev/null; done )` |
| Release binary size | 774664 B | `stat -c%s target/release/polyforge-cli` |
| Full clean rebuild (`cargo clean` + `cargo build --release`) | 34.15 s | `cargo clean && time cargo build --release` |

All numbers were measured on this machine with the release binary; the rebuilt binary is byte-identical to the one measured above.

## Why it matters

PolyForge is a single-format evidence ledger: one append-only Merkle chain, one tri-state
entry model, one deterministic gate. The table below surveys the direct evidence-ledger and
MCP tools found on 2026-08-09, plus three adjacent categories for context. Among the direct
evidence-ledger tools surveyed, PolyForge is the only Rust crate-workspace observed; the
rest are single-language or proprietary. Facts come from each project's public page; where
a source is silent the cell reads "Unknown".

| Project | Category | Stack | Tamper-evident | Gate | Notes | Source (accessed 2026-08-09) |
| ------- | -------- | ----- | -------------- | ---- | ----- | ----------------------------- |
| `agent-gate` (Jott2121/agent-gate) | hash-chained receipts | Python | hash-chained receipts | 5-check gate | fail-closed checklist | https://github.com/Jott2121/agent-gate |
| `AttestMCP` (attestmcp/attestmcp) | MCP proxy | Python | chained hashes | Unknown | Unknown | https://github.com/attestmcp/attestmcp |
| `AGA MCP Server` (attestedintelligence/aga-mcp-server) | policy gate | TypeScript | sealed references | Unknown | Unknown | https://github.com/attestedintelligence/aga-mcp-server |
| `audit-ledger-mcp` (shahidh68) | audit ledger | Unknown | hashed PII + S3 Object Lock | Unknown | Unknown | https://github.com/shahidh68/audit-ledger-mcp |
| `Xiid` | network access control | proprietary | Unknown | Unknown | Unknown | https://xiid.com |
| `Zyvra` | EU AI Act SaaS | Unknown | Unknown | Unknown | Unknown | https://zyvra.tech |
| `Omega` (arXiv 2512.05951) | academic hardware attestation | SEV-SNP / H100 | Unknown | Unknown | Unknown | https://arxiv.org/abs/2512.05951 |
| Observability (LangSmith / Langfuse / AgentOps / Arize Phoenix / Helicone) | observability | various | Unknown | Unknown | adjacent context, not evidence-ledger | https://docs.smith.langchain.com |
| Provenance (in-toto / Sigstore/cosign / Witness / SLSA) | provenance | various | Unknown | Unknown | adjacent context, not evidence-ledger | https://in-toto.io |
| Sandboxes (e2b / Modal / Daytona) | sandboxes | various | Unknown | Unknown | adjacent context, not evidence-ledger | https://e2b.dev |

Taken together: among the direct evidence-ledger tools surveyed on 2026-08-09, PolyForge is
the only Rust crate-workspace combining a single-format Merkle ledger, a deterministic gate,
and an MCP interface in one workspace.

## License

PolyForge is licensed under the [Apache License, Version 2.0](LICENSE). See the
[NOTICE](NOTICE) file for attribution requirements.

## Contributing

Before opening an issue, please read [SECURITY.md](SECURITY.md) for how to report
vulnerabilities. Feature and bug reports use the templates under
[`.github/ISSUE_TEMPLATE/`](.github/ISSUE_TEMPLATE/).
