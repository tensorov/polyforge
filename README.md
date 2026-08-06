# PolyForge

PolyForge is a tamper-evident evidence ledger for AI-driven engineering workflows: models
record claims, allowlisted tools attest them, and operators gate on the resulting chain.

Everything in this README is covered by the workspace test suite (45 tests across the four
crates) and by the CLI/MCP smoke and end-to-end harnesses.

## Architecture

Workspace of four crates (edition 2021, rust-version 1.85, Rust toolchain 1.95.0):

| Crate          | Responsibility                                                                           |
| -------------- | ---------------------------------------------------------------------------------------- |
| `pf-core`      | Evidence model: tri-state entries, promotion rules, the append-only Merkle ledger, and deterministic gate evaluation. |
| `pf-toolrunner`| Allowlisted tool runner: only allowlisted binaries (cargo/rustc/gcc), typed arguments, no shell, per-command environment fingerprint. |
| `pf-mcp`       | Model Context Protocol server (rmcp): the interface models use to append claims and query gates. |
| `pf-cli`       | Operator CLI: init, append, ledger inspection, and gate execution over a local ledger.   |

The CLI binary is named `pf-cli` (the crate name). All examples below use it directly;
`alias pf=pf-cli` if you prefer the short name.

Build and test the workspace:

```sh
cargo build --workspace
cargo test --workspace
```

## Evidence lifecycle

Evidence is tri-state and only ever moves forward:

```
 ModelClaimed ──► Verified ──► Validated
      │               │             │
      │ (model)       │ (tool)      │ (operator)
      │ appends       │ attests     │ validates
      └───────────────┴─────────────┘
```

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
- Promotion is enforced by the single `promote` gatekeeper in `pf-core`; a model's only
  path toward `Verified` is an allowlisted tool attestation.

## Running a gate

```sh
pf-cli gate <task_id> [--required verified|validated]
```

- **PASS** (the task's evidence chain satisfies the required state): writes the evidence
  bundle `gate-<task_id>.jsonl` (the task's ledger records, in sequence) plus
  `gate-<task_id>.manifest.json` with `task_id`, `tail_hash`, `passed: true`,
  `bundle_sha256`, and `tool_versions`. The `tail_hash` equals the output of
  `pf-cli ledger tail`.
- **FAIL** (required state not reached): exits non-zero and never writes a bundle — at
  most a manifest with `passed: false` and `bundle_sha256: null`. A corrupted chain exits
  non-zero and writes nothing.
- `pf-cli gate` is reproducible: a second PASS run produces a byte-identical bundle and an
  identical `bundle_sha256`.

Example (against a throwaway ledger):

```sh
export PF_LEDGER=/tmp/pf-demo/ledger.jsonl
export PF_EVIDENCE_DIR=/tmp/pf-demo/evidence/
pf-cli init
pf-cli append model_claim "claim datum" --task demo --commit abc123 --diff d1
pf-cli append tool_attestation "ran" --task demo
pf-cli ledger tail
pf-cli gate demo --required verified
pf-cli append validation "operator check" --task demo
pf-cli gate demo --required validated
```

Environment variables: `PF_LEDGER` (default `.omo/ledger.jsonl`) and
`PF_EVIDENCE_DIR` (default `.omo/evidence/`).

## Connecting the MCP army

`pf-mcp` is an rmcp server speaking MCP over stdio by default:

```sh
pf-mcp
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

## Tamper / rewind guarantee

The ledger is an append-only Merkle chain: every entry commits to the hash of the previous
entry. Rewinding the file or tampering with **one byte** of any entry breaks the chain, and
any subsequent `pf-cli gate` or `evidence_verify` fails with `LedgerIntegrity`. Failed
integrity checks never fabricate a bundle or manifest — the failure is surfaced and
nothing is written. This is exercised end-to-end by the army harness (byte-flip in the
tail entry → gate exits non-zero with `ledger integrity broken at seq …`, no bundle, no
manifest).
