# PolyForge entry for the modelcontextprotocol/servers directory

Drafted 2026-08-22 against upstream `main`. Verbatim fetched copies of the
upstream `README.md` and `CONTRIBUTING.md` are preserved in
`.omo/evidence/task-11-polyforge-phase1-adoption.txt` (citation base).

## Upstream conventions (verified by fetch)

1. Current guidance: the upstream README no longer carries a third-party
   server listing. CONTRIBUTING.md routes discoverability to the MCP Server
   Registry at https://registry.modelcontextprotocol.io/ and states that new
   server implementations are not accepted into the repository itself.
2. Historical entry format (still visible in the Reference Servers section):
   a hyphen bullet, a bold name linking the server, then " - " and a one-line
   description. Shape: `- **[Name](link)** - One line description.`

The entry below mirrors format (2) so it can be pasted wherever a directory
listing is still accepted, and reused as the description text in a registry
submission (route 1). See SUBMISSION-CHECKLIST.md section 0 for the route
decision.

## Drafted entry

```markdown
- **[PolyForge](https://github.com/tensorov/polyforge)** - Tamper-evident evidence ledger for AI-driven engineering workflows: models record claims, allowlisted tools attest them, and operators gate on the resulting chain.
```

Description provenance: the first descriptive line of our README reads
"PolyForge is a tamper-evident evidence ledger for AI-driven engineering
workflows: models record claims, allowlisted tools attest them, and operators
gate on the resulting chain." The bullet drops the leading subject because the
bold name already names the server, matching upstream entries such as
"Knowledge graph-based persistent memory system."

## Registration snippets

See `snippets/opencode.json`, `snippets/claude-code.json`, and
`snippets/codex.toml`. All three register the same `polyforge-mcp` binary over
stdio (`PF_MCP_TRANSPORT=stdio`) and mirror the "Connecting the MCP army" and
"Agent integration" sections of our README. Claude Code users can alternatively
run: `claude mcp add polyforge -- polyforge-mcp`.

## Demo assets referenced by the kit

Paths are relative to this directory and verified to exist on disk:

- ../../assets/readme/hero.svg (linked demo image)
- ../../assets/readme/hero.gif (animated demo capture)
- ../../assets/readme/cli-demo.svg (CLI quick start capture)
- ../../assets/readme/lifecycle.svg (evidence lifecycle diagram)
