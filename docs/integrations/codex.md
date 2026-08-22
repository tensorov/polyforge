# PolyForge MCP integration guide: Codex

Register `polyforge-mcp` in the Codex CLI config so agent sessions can append claims to
the tamper-evident PolyForge ledger and query gates through MCP tools. The snippet below
is the same one published in the README section "Connecting the MCP army".

> **Verification caveat**
>
> The codex CLI is absent on the authoring machine, so this guide is server
> handshake-verified only: the TOML block parses and the server it names answers a real
> MCP handshake, but the snippet format itself is unverified against the codex binary.
> The `[mcp_servers.*]` table shape follows the upstream Codex configuration reference:
> https://github.com/openai/codex/blob/main/docs/config.md
> If your codex version differs, trust its docs over this file and re-run the handshake
> check from the appendix.

## What this gives you

Codex gains four MCP tools backed by one append-only Merkle ledger:

| Tool              | Purpose                                                              |
| ----------------- | -------------------------------------------------------------------- |
| `evidence_append` | Append a `ModelClaim` entry (the only kind a model may append)       |
| `evidence_verify` | Run an allowlisted tool to promote the claim to `Verified`           |
| `gate_evaluate`   | Evaluate a task's stage gate against required evidence states        |
| `gate_report`     | Read-only snapshot of tail hash, pass status, bundle SHA-256        |

Models can never self-produce `Verified`: the server rejects every kind except
`ModelClaim`, so promotion always requires an allowlisted tool run or an operator entry.

## Prerequisites

Install the server binary from crates.io:

```sh
cargo install polyforge-mcp
```

Confirm it resolves on your `PATH`:

```sh
command -v polyforge-mcp
```

Building from source works too: `cargo build -p polyforge-mcp`, then point `command` at
the absolute path of `target/debug/polyforge-mcp` (or `target/release/polyforge-mcp`).

## Configuration

Add the following block to `~/.codex/config.toml`:

```toml
[mcp_servers.polyforge]
command = "polyforge-mcp"
env = { PF_MCP_TRANSPORT = "stdio" }
```

Restart codex after editing the file: MCP servers attach at session start.

## Environment variables

Set these in the block's inline `env` table (or export them globally):

| Variable            | Default            | Meaning                                                                                          |
| ------------------- | ------------------ | ------------------------------------------------------------------------------------------------ |
| `PF_MCP_TRANSPORT`  | `stdio`            | `stdio` (what agents use) or `tcp`                                                               |
| `PF_MCP_ADDR`       | `127.0.0.1:18888`  | TCP bind address; non-loopback binds are rejected at startup                                     |
| `PF_MCP_LEDGER`     | `.pf/ledger.jsonl` | Ledger path, resolved relative to the server process working directory                           |
| `PF_MCP_TOKEN`      | unset              | Required when transport is `tcp`; each request must carry it as `_pf_token` or fail with `-32001` |

Agent integrations should keep `PF_MCP_TRANSPORT=stdio`. The TCP mode plus token exists
for remote or multi-client setups.

## Troubleshooting

- **Server not picked up by codex.** Confirm the file path (`~/.codex/config.toml`) and
  that the table name is exactly `mcp_servers.polyforge`; then restart codex.
- **Spawn failure.** `polyforge-mcp` is not on the `PATH` codex uses. Point `command` at
  the absolute binary path, for example `"/home/me/.cargo/bin/polyforge-mcp"`.
- **Ledger lands somewhere unexpected.** `PF_MCP_LEDGER` resolves relative to the spawned
  server's working directory. Use an absolute path when in doubt.
- **Handshake sanity check.** Drive the server by hand:
  ```sh
  printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
    | polyforge-mcp
  ```
  Expect an initialize result naming `rmcp` and a four-entry tools list.

## Verification appendix

Wave: T12 polyforge-phase1-adoption (plan dated 2026-08-21). Proofs captured on the
authoring machine against `target/debug/polyforge-mcp` v0.2.0 built at repo HEAD
`aced71a`, with `PF_MCP_LEDGER` pointed at a fresh `/tmp` ledger.

1. Structural check: the TOML block above was extracted from this file and parsed with
   `python3 tomllib`; shape assertions (`command == "polyforge-mcp"`,
   `env == {"PF_MCP_TRANSPORT": "stdio"}`) passed.
2. Live handshake: the command and env from the parsed block were used to spawn the
   server over raw stdio; `initialize` then `tools/list` JSON-RPC lines were fed on
   stdin. Transcript summary:
   - initialize response result: `protocolVersion "2026-07-28"`, capabilities
     `{"tools": {}}`, serverInfo `{"name": "rmcp", "version": "3.1.1"}`
   - tools/list response: tool names `["evidence_append", "evidence_verify",
     "gate_evaluate", "gate_report"]`
   - assertions: `initialize_result_with_serverInfo=True`,
     `tools_include_evidence_append=True`, exit code 0
3. Full transcripts are preserved in
   `.omo/evidence/task-12-polyforge-phase1-adoption.txt`.

See the caveat box at the top of this guide: the codex CLI itself was absent during
authoring, so the registration format carries no end-to-end proof against that binary.
