# PolyForge Roadmap

PolyForge's path to production-ready. Two constraints drive every priority:

1. **Trust first** — attestations must be ungameable and reproducible. A trust layer is only
   as trustworthy as what it can prove: anti-gaming (mutation testing, coverage floors) and
   reproducible environments (Nix/Devbox fingerprints) come before any adoption feature.
2. **Dogfood everything** — PolyForge gates its own development from day one. Using the tool
   on itself is the fastest route to production-ready.

## Guiding principles

- **Trust first**: an agent can delete a failing test and still get a green `cargo test`.
  Mutation testing and coverage floors close that hole; Nix/Devbox fingerprints close the
  "works on my machine" hole.
- **Dogfood everything**: each phase ships through the PolyForge gate itself — the tool
  validates its own PRs, its own ledger chains, its own evidence.
- **Adoption through CI, not ceremony**: the GitHub Action + required status check is the
  single change that turns PolyForge from an interesting CLI into an indispensable part of a
  team's workflow.
- **Human supervision at scale**: an operator TUI exists to keep humans in the loop as
  supervisors, not bottlenecks.

## Phase 0 — Trust hardening + self-gating

The goal: attestations stop being gameable, and PolyForge starts gating its own repository.

| # | Task | Why |
|---|------|-----|
| 0.1 | Mutation testing in the toolrunner allowlist: `cargo-mutants` (Rust), `stryker` (JS) + a coverage floor on the gate | An agent can delete or comment out a test and still pass `cargo test`. Mutation testing breaks the code and checks whether the tests react. Without it, the whole trust system is fiction. |
| 0.2 | **Nix / Devbox fingerprinting**: flake hash / derivation recorded in `ToolAttestation`; lockfile pinning (`uv.lock`, `pnpm-lock`) - **DONE (C2.1)** | Cargo.lock + devbox.lock + Nix store-path identity folded into the per-command environment fingerprint when present (C2.1). Entries recorded before C2.1 carry a different fingerprint; old attestations stay valid history - append-only ledger, do not re-verify or fix old entries. |
| 0.3 | **GitHub Action `polyforge-action` + required status check on the polyforge repo itself** | Dogfooding from day one: every PR to this project runs `polyforge gate`, reads `.pf/ledger.jsonl`, validates the Merkle chain and the committed ledger anchor. This is both adoption item #1 and the self-improvement mechanism. |
| 0.4 | PR comment summary: "`NN tasks verified, M validated, K failed`" | Turns PolyForge into an indispensable part of the workflow, not an interesting toy. |

**Exit criteria:** a polyforge PR cannot merge without a valid chain and a passing gate;
`cargo-mutants` fails in CI when a test is deleted.

## Phase 1 — Adoption

The goal: PolyForge becomes a standard for other teams, not just for us.

| # | Task | Why |
|---|------|-----|
| 1.1 | Publish `tensorov/polyforge-action` to the marketplace + GitLab CI gateway | CI gateway. Agent code lands via PR/MR — the gate must live there. |
| 1.2 | MCP server → ecosystem: PR into `modelcontextprotocol/servers` (featured), with examples and prompts; **Claude Computer Use** — automatic `model_claim` per tool call | The `polyforge-mcp` crate already exists (stdio, ledger, addr); what remains is ecosystem exposure. Computer Use makes attestation seamless. |
| 1.3 | **Python + TypeScript toolrunner**: allowlist `pytest/ruff/mypy/pyright/uv` and `vitest/tsc/eslint/biome` + environment fingerprints (venv, uv.lock, pnpm-lock) | Currently Rust-oriented; most agent pipelines are Python/TS. Direct applicability growth. |
| 1.4 | **TUI "LazyForge"** (Ratatui, like lazygit/k9s): task tree → diffs → confirming tools → one key `[V] Validate` → signature into the ledger | Operator supervision at scale. At thousands of micro-tasks a human will either approve blindly or give up. Key tool for the project's own self-improvement loop. |
| 1.5 | Ready-made prompts / system instructions for Cline, Aider, Cursor: before every `git commit`, the agent must call `evidence_append` | Agents don't write evidence on their own — make it convenient by default. |

**Exit criteria:** a foreign repo with one `.github/workflows/polyforge.yml` gets a gate +
comment; a TUI session validates a hundred tasks in a minute.

## Phase 2 — Scale and observability

The goal: hold up under billions of tokens and make evidence part of observability.

| # | Task | Why |
|---|------|-----|
| 2.1 | **Remote backend**: PostgreSQL + S3 (or DynamoDB) instead of a single `ledger.jsonl` | A single file does not scale. Foundation for everything that follows. |
| 2.2 | **Web dashboard + API**: timeline of all agents, task search, verification-failure rate, verification cost per agent, heatmap of problem areas + REST/gRPC API | CLI is not enough at this volume. The API lets other systems query gate states. |
| 2.3 | **OpenTelemetry exporter** + LangSmith / Helicone / Phoenix integrations | Evidence becomes part of observability, not a parallel universe. |
| 2.4 | **LangGraph / CrewAI / AutoGen middleware**: every node/step → claim + attestation | Direct solution for "hundreds of agents without supervision" — wrap the graph, get attestation automatically. |

**Exit criteria:** a 100-agent pilot with a live dashboard; a LangGraph step lands in the
ledger automatically.

## Phase 3 — Enterprise + ecosystem

| # | Task | Why |
|---|------|-----|
| 3.1 | SLSA / in-toto / Sigstore compatibility | Entry into enterprise and regulated environments. |
| 3.2 | Deep plugins: Cursor, Windsurf, Continue.dev (native, not via MCP shim) | Deeper integration = higher convenience. |
| 3.3 | Web version of the human-in-the-loop validation | People are not always in a terminal. |
| 3.4 | Policy-as-Code for verification rules | Which tests are mandatory for which task types — configurable. |

## Moonshot backlog

- **Verification marketplace** — specialized verifier agents (security, performance, formal
  verification) selling attestations.
- **TEE / hardware attestations** (AWS Nitro, NVIDIA) — trust at the hardware level.

## The self-improvement loop

Phase 0.3 (the GitHub Action on our own repo) and 1.4 (the TUI) are not just features — they
are the mechanism by which PolyForge improves itself: PolyForge gates its own development,
and the operator supervises it through LazyForge instead of CLI arguments. They rank highest
because they pay off twice.