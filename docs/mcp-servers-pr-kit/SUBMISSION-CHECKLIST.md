# Submission checklist: modelcontextprotocol/servers PR kit

Target repository: https://github.com/modelcontextprotocol/servers
Prepared 2026-08-22. Upstream conventions were verified by fetching
`README.md` and `CONTRIBUTING.md` from `main` on this date; verbatim copies
are the citation base in `.omo/evidence/task-11-polyforge-phase1-adoption.txt`.

## 0. Route decision (read first)

Upstream CONTRIBUTING.md (fetched 2026-08-22) retires the third-party listing:
the README no longer contains it, wholly new server implementations are not
accepted into the repository, and discoverability moves to the MCP Server
Registry at https://registry.modelcontextprotocol.io/ (quickstart:
https://github.com/modelcontextprotocol/registry/blob/main/docs/modelcontextprotocol-io/quickstart.mdx).

- Primary route: publish `polyforge-mcp` to the MCP Server Registry.
- Fallback route (kept per plan): a listing PR using [ENTRY.md](ENTRY.md),
  valid wherever a curated list still accepts entries. Flag it as such in the
  PR body; do not claim the upstream README still hosts a directory.

## 1. Upstream PR steps (GitHub flow)

1. Fork modelcontextprotocol/servers (no forks were opened while preparing
   this kit; that step happens only at submission time).
2. Create a branch, e.g. `add-polyforge-listing`.
3. Apply the entry from [ENTRY.md](ENTRY.md) at the location maintainers
   designate; confirm placement in the PR description if unclear.
4. Commit with a message describing the addition and open the PR against main.
5. In the PR body: link the working server repository, state what PolyForge
   does in one sentence (reuse the ENTRY.md description), and answer
   maintainer questions promptly.

## 2. Listing quality gates

- [ ] The listing links a WORKING server: https://github.com/tensorov/polyforge
      (public repo, CI green, all four crates published to crates.io at v0.2.0).
- [ ] Description matches our README first line (see the provenance note in
      ENTRY.md).
- [ ] Zero em/en dashes in all submitted prose (house rule).
- [ ] Vendor-neutral wording only, matching the tone of reference entries.

## 3. Working-server proof to attach

- `cargo install polyforge-cli polyforge-mcp` succeeds from crates.io.
- Fresh-ledger smoke passes: `polyforge-cli init`, `append`, `gate`
  (see README Quick start).
- MCP stdio handshake returns four tools: `evidence_append`,
  `evidence_verify`, `gate_evaluate`, `gate_report`.

## 4. CI prerequisite for foreign repos (rustup note)

Any CI example we publish must install a Rust toolchain BEFORE the gate step:
`tensorov/polyforge-action@v1` runs `cargo install polyforge-cli --locked`,
which needs cargo and rustc on PATH. On repos without Rust, add a toolchain
step first:

```yaml
- uses: dtolnay/rust-toolchain@stable
- uses: tensorov/polyforge-action@v1
  with:
    task-id: my-task
    required: verified,validated
```

Source: README section "Python / TypeScript repos", foreign-repo CI note.

## 5. Computer Use (NOTES ONLY, deferred implementation)

Status: deferred. Roadmap item 1.2 second half (docs/ROADMAP.md): Claude
Computer Use integration that records an automatic `model_claim` per tool
call. Do NOT promise Computer Use in any submission text until implemented;
if asked, answer that it is on the roadmap under phase 1.2 and is not part of
the current `polyforge-mcp` surface (four tools listed under README
"Connecting the MCP army").

## 6. Demo assets (paths relative to docs/mcp-servers-pr-kit/)

Reference these exact relative paths inside kit documents; each resolves from
this directory:

- [ ] ../../assets/readme/hero.svg exists
- [ ] ../../assets/readme/hero.gif exists
- [ ] ../../assets/readme/cli-demo.svg exists
- [ ] ../../assets/readme/lifecycle.svg exists

When embedding images in the GitHub PR body itself, use absolute raw URLs
pinned to a tensorov/polyforge commit instead of relative paths (relative
paths do not render outside this repository).

## 7. Pre-submit verification (re-run before opening anything)

```sh
python3 -c "import json; json.load(open('docs/mcp-servers-pr-kit/snippets/opencode.json'))"
python3 -c "import json; json.load(open('docs/mcp-servers-pr-kit/snippets/claude-code.json'))"
python3 -c "import tomllib; tomllib.loads(open('docs/mcp-servers-pr-kit/snippets/codex.toml').read())"
# banned-term scan over docs/mcp-servers-pr-kit/ with the pattern held in the
# task spec: expect zero matches
# em/en dash scan (U+2013, U+2014) over docs/mcp-servers-pr-kit/: expect zero matches
```

House rules honored by this kit: read-only network fetches only, no forks,
PRs, or issues opened during preparation, and only new files under
docs/mcp-servers-pr-kit/.
