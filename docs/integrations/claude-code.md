# PolyForge MCP integration guide: Claude Code

Register `polyforge-mcp` with Claude Code so coding sessions can append claims to the
tamper-evident PolyForge ledger and query gates through MCP tools. The commands below are
the same ones published in the README section "Connecting the MCP army"; this guide
verifies them against the real `claude` CLI in a sandbox.

## What this gives you

Claude Code gains four MCP tools backed by one append-only Merkle ledger:

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

Building from source works too: `cargo build -p polyforge-mcp`, then register the
absolute path of `target/debug/polyforge-mcp` (or `target/release/polyforge-mcp`)
instead of the bare name.

## Configuration

User scope is the default and registers the server for all projects:

```sh
claude mcp add polyforge -- polyforge-mcp
```

Project scope writes a `.mcp.json` at the project root, which teammates can share
through version control:

```sh
claude mcp add --scope project polyforge -- polyforge-mcp
```

Both forms produce the same server record. The project-scope file looks like this, so you
can also create it by hand:

```json
{
  "mcpServers": {
    "polyforge": {
      "type": "stdio",
      "command": "polyforge-mcp",
      "args": [],
      "env": {}
    }
  }
}
```

To pin environment variables for the server, put them into the `env` object of the JSON
above or extend the add command with `-e KEY=value` flags. Restart Claude Code after
changing registration: MCP servers attach at session start.

## Environment variables

| Variable            | Default            | Meaning                                                                                          |
| ------------------- | ------------------ | ------------------------------------------------------------------------------------------------ |
| `PF_MCP_TRANSPORT`  | `stdio`            | `stdio` (what agents use) or `tcp`                                                               |
| `PF_MCP_ADDR`       | `127.0.0.1:18888`  | TCP bind address; non-loopback binds are rejected at startup                                     |
| `PF_MCP_LEDGER`     | `.pf/ledger.jsonl` | Ledger path, resolved relative to the server process working directory                           |
| `PF_MCP_TOKEN`      | unset              | Required when transport is `tcp`; each request must carry it as `_pf_token` or fail with `-32001` |

Agent integrations should keep `PF_MCP_TRANSPORT=stdio`. The TCP mode plus token exists
for remote or multi-client setups.

## Troubleshooting

- **Server shows as pending.** After `claude mcp list` reports the server, first use in a
  session requires one approval inside the Claude Code app; approve it once and it stays.
- **Health check fails in `claude mcp list`.** The binary name did not resolve. Re-add
  with an absolute path: `claude mcp add polyforge -- /home/me/.cargo/bin/polyforge-mcp`.
- **Ledger lands somewhere unexpected.** `PF_MCP_LEDGER` resolves relative to the spawned
  server's working directory. Use an absolute path when in doubt.
- **Testing without touching your real config.** Point the CLI at a throwaway config dir
  and run from a throwaway project directory:
  ```sh
  export CLAUDE_CONFIG_DIR=$(mktemp -d)
  cd $(mktemp -d)
  claude mcp add --scope project polyforge -- polyforge-mcp
  claude mcp list
  ```
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
authoring machine against the real `claude` CLI, fully sandboxed:
`CLAUDE_CONFIG_DIR=$(mktemp -d)` plus a throwaway project directory, so the real
`~/.claude.json` was never read or written (verified by comparing its stat before and
after; identical).

Transcript summary:

1. `claude mcp add --scope project polyforge -- polyforge-mcp` exited 0 and printed
   `Added stdio MCP server polyforge with command: polyforge-mcp to project config`,
   writing `.mcp.json` inside the sandbox project (content shown above).
2. `claude mcp list` exited 0 and printed:
   `polyforge: polyforge-mcp - Pending approval (run \`claude\` to approve)`
   under `Checking MCP server health...`, proving the registration is visible to the CLI.
3. The server binary itself was handshake-verified separately over raw stdio against
   `target/debug/polyforge-mcp` v0.2.0 built at repo HEAD `aced71a`: initialize returned
   `protocolVersion "2026-07-28"` with serverInfo `{"name": "rmcp", "version": "3.1.1"}`
   and tools/list returned `["evidence_append", "evidence_verify", "gate_evaluate",
   "gate_report"]`; both assertions true, exit code 0.

Full transcripts are preserved in
`.omo/evidence/task-12-polyforge-phase1-adoption.txt`.
