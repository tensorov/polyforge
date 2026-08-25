# PolyForge guide: linking Langfuse traces to gate scores

Agents already narrate their work in [Langfuse](https://langfuse.com) traces. PolyForge
records what was proven in its evidence ledger and passes or fails gates on it. This
guide connects the two halves: the trace id travels into the ledger through the append
identity flags, and after a gate runs, the standalone `langfuse-bridge` tool posts a
single score object back to your self-hosted Langfuse instance so the trace carries the
gate outcome.

> This tool is OPTIONAL analytics and is NEVER part of the PolyForge trust contour:
> gates never depend on it.

The sentence above is the contract. A missing bridge, a dead Langfuse, or a wrong URL
can never turn a green gate red or a red gate green. Everything in this guide is
post-gate bookkeeping for humans reading dashboards.

## Prerequisites

You need a built workspace CLI and the bridge binary:

```sh
cargo build --manifest-path tools/langfuse-bridge/Cargo.toml
export PATH="$PWD/target/debug:$PWD/tools/langfuse-bridge/target/debug:$PATH"
```

If you installed the crates with `cargo install`, the freshly built binaries still win
here because the workspace directories come first on `PATH`.

## Step 1: attach the trace id when appending claims

Every `append` kind accepts record-only identity flags. Put your Langfuse trace id into
the `run_id` field with `--run`:

```sh
export PF_LEDGER=/tmp/pf-langfuse-demo/ledger.jsonl
export PF_EVIDENCE_DIR=/tmp/pf-langfuse-demo/evidence/
mkdir -p /tmp/pf-langfuse-demo/evidence
polyforge-cli init
polyforge-cli append model_claim "agent finished task checkout-fix, tests green" \
  --task checkout-fix --commit 3f72225 --diff d12a4f \
  --run lf-trace-demo-001
polyforge-cli append tool_attestation "cargo test -p polyforge-core: 69 passed" --task checkout-fix
```

Alternatively carry the id in structured metadata with `--metadata`, which must be valid
JSON:

```sh
polyforge-cli append model_claim "second claim carrying metadata only" \
  --task checkout-review --commit 3f72225 --diff d12a4f \
  --metadata '{"langfuse_trace_id":"lf-trace-demo-002"}'
polyforge-cli append tool_attestation "cargo test -p polyforge-core: 69 passed" --task checkout-review
```

Both forms survive promotion: when a tool attestation promotes the claim, the identity
fields are copied along, so the trace id stays attached to the task for its whole life.

## Step 2: run the gate and add the trace id to the manifest

A passing gate writes `gate-<task_id>.manifest.json`. Today that manifest records
`task_id`, `tail_hash`, `passed`, `bundle_sha256`, and `tool_versions`, but not the
trace id, so add it before invoking the bridge. Gate both tasks first:

```sh
polyforge-cli gate checkout-fix --required verified
polyforge-cli gate checkout-review --required verified
cat "$PF_EVIDENCE_DIR/gate-checkout-fix.manifest.json"
```

Enrich each manifest with `jq` (any JSON edit works; `jq` keeps this one line):

```sh
jq '. + {run_id: "lf-trace-demo-001"}' \
  "$PF_EVIDENCE_DIR/gate-checkout-fix.manifest.json" \
  > "$PF_EVIDENCE_DIR/gate-checkout-fix.traced.json"
jq '. + {metadata: {langfuse_trace_id: "lf-trace-demo-002"}}' \
  "$PF_EVIDENCE_DIR/gate-checkout-review.manifest.json" \
  > "$PF_EVIDENCE_DIR/gate-checkout-review.traced.json"
```

The bridge resolves the trace id with a fixed priority: the `run_id` field first, then
`metadata.langfuse_trace_id`. If both are present, `run_id` wins.

## Step 3: dry run first

With `--dry-run` the bridge prints the exact payload bytes to stdout and performs no
HTTP request. It does not even read the environment variables, so you can inspect
payloads before configuring anything:

```sh
langfuse-bridge "$PF_EVIDENCE_DIR/gate-checkout-fix.traced.json" --dry-run
```

Expect one line shaped like `{"name":"gate","traceId":"lf-trace-demo-001","value":1}`
and exit code 0. A value of `1` means the gate passed, `0` means it failed.

Running the bridge on the untouched manifest is safe and boring: without either trace
field it prints one warning on stderr and exits 0 having made zero HTTP requests:

```sh
langfuse-bridge "$PF_EVIDENCE_DIR/gate-checkout-fix.manifest.json" --dry-run
echo "exit=$?"
```

Expect `warning: no run_id and no metadata.langfuse_trace_id in ...; skipping Langfuse
post` and `exit=0`.

## Step 4: post the score

Set the three environment variables for your SELF-HOSTED Langfuse and invoke the bridge
on the enriched manifest. The tool performs exactly ONE `POST
{LF_BASE_URL}/api/public/ingestion` with `Authorization: Basic base64(LF_PK:LF_SK)`.
No retries. Plain HTTP only (no TLS), so point `LF_BASE_URL` at your own instance.

| Variable      | Meaning                                                            |
| ------------- | ------------------------------------------------------------------ |
| `LF_BASE_URL` | Base URL of the self-hosted Langfuse, e.g. `http://localhost:3000` |
| `LF_PK`       | Langfuse public key (basic auth user)                              |
| `LF_SK`       | Langfuse secret key (basic auth password)                          |

For authoring, this guide posts against a throwaway capture server instead of a real
Langfuse. Save the tiny stdlib-only responder below and start it on port 18923:

```sh
cat > /tmp/pf-langfuse-demo/capture_server.py <<'EOF'
"""Capture server: answers 200 to any POST and dumps the raw request."""
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

OUT = sys.argv[2]

class Capture(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)
        with open(OUT, "wb") as f:
            f.write(self.requestline.encode() + b"\r\n")
            f.write(str(self.headers).encode())
            f.write(b"\r\n" + body)
        self.send_response(200)
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"ok")

    def log_message(self, *args):
        pass

HTTPServer(("127.0.0.1", int(sys.argv[1])), Capture).serve_forever()
EOF
setsid python3 /tmp/pf-langfuse-demo/capture_server.py 18923 /tmp/pf-langfuse-demo/captured.txt >/dev/null 2>&1 < /dev/null &
sleep 1
```

Then post and inspect what arrived:

```sh
export LF_BASE_URL=http://127.0.0.1:18923
export LF_PK=pk-lf-demo
export LF_SK=sk-lf-demo
langfuse-bridge "$PF_EVIDENCE_DIR/gate-checkout-fix.traced.json"
cat /tmp/pf-langfuse-demo/captured.txt
```

On success the bridge prints `posted gate score for task checkout-fix (trace
lf-trace-demo-001)` and exits 0. The captured request shows the request line
`POST /api/public/ingestion HTTP/1.1`, the basic auth header, and the body
`{"name":"gate","traceId":"lf-trace-demo-001","value":1}`. Stop the capture server when
done:

```sh
pkill -f 'pf-langfuse-demo/capture_serve[r].py'
```

In production the same invocation against `http://your-langfuse-host:3000` creates one
score named `gate` on the trace, visible next to the agent's steps in the Langfuse UI.

## Troubleshooting

- **`error: LF_PK is not set` with exit code 2.** The bridge names each missing
  environment variable and stops before any network traffic. Check for typos: the
  variables are `LF_BASE_URL`, `LF_PK`, and `LF_SK`. This exact failure was reproduced
  during authoring by exporting `LF_PKEY` (wrong name) instead of `LF_PK`:

  ```sh
  export LF_PKEY=pk-lf-demo
  unset LF_PK
  langfuse-bridge "$PF_EVIDENCE_DIR/gate-checkout-fix.traced.json"
  echo "exit=$?"
  unset LF_PKEY
  ```

  Expect `error: LF_PK is not set` and `exit=2`. Fix the name and the same invocation
  succeeds:

  ```sh
  export LF_PK=pk-lf-demo
  langfuse-bridge "$PF_EVIDENCE_DIR/gate-checkout-fix.traced.json"
  ```

- **`warning: no run_id and no metadata.langfuse_trace_id in ...`**. The manifest has
  no trace id. Enrich it as shown in step 2, or accept the skip: nothing is posted and
  the exit code stays 0, because analytics must never fail a pipeline.
- **`error: langfuse bridge: POST ... failed: cannot connect to ...`** with exit code 1.
  `LF_BASE_URL` points at a closed port or unreachable host. The bridge tries once,
  names the full URL, and gives up; there is no retry storm.
- **`error: langfuse bridge: LF_BASE_URL must use plain http (TLS unsupported): ...`**
  with exit code 2. HTTPS URLs are rejected by design; terminate TLS in front of your
  Langfuse and keep `LF_BASE_URL` on the internal http address.
- **`--metadata must be valid JSON`**. The `--metadata` value is parsed eagerly; quote
  the whole object in your shell as shown in step 1.

## Privacy: what crosses the boundary

The score payload is three fields: the name `gate`, a `0` or `1` value, and the trace
id string. Evidence payloads, diffs, bundle bytes, tail hashes, and tool versions never
leave the machine. In the other direction, the agent's trace content (prompts,
generations, tool calls) lives entirely in Langfuse; the PolyForge ledger stores only
hashes plus the id strings an operator chose to attach. The link between the two stores
is exactly one opaque id, so either system can be deleted or exported independently.

## Status: optional analytics

To repeat the contract once more: the bridge is optional analytics and is never
required by gates. Gate evaluation reads the ledger and nothing else. CI workflows that
install `tensorov/polyforge-action` do not need this tool, and removing it changes no
verdict anywhere. Related guides: [Sigstore anchor verification](sigstore.md),
[OpenCode](opencode.md), [Claude Code](claude-code.md), [Codex](codex.md).

## Verification appendix

Wave: T12 polyforge-oss-trust-stack (plan dated 2026-08). Every command block in this
guide was executed verbatim during authoring against the local capture server described
in step 4, with the workspace binaries built at repo HEAD. Results:

1. Build finished clean; both binaries resolved from the prepended `PATH`.
2. Both append forms landed in the ledger and survived promotion through tool
   attestations; both gates printed `gate PASSED` and wrote their bundles.
3. `cat` of the raw manifest confirmed it carries no trace fields, motivating the `jq`
   enrichment step documented above.
4. Dry run printed `{"name":"gate","traceId":"lf-trace-demo-001","value":1}` (exit 0);
   the metadata-enriched variant printed the same shape with
   `lf-trace-demo-002`. The un-enriched manifest produced the skip warning and exit 0.
5. Real POST: the capture server recorded request line
   `POST /api/public/ingestion HTTP/1.1`, header
   `Authorization: Basic cGstbGYtZGVtbzpzay1sZi1kZW1v` (base64 of
   `pk-lf-demo:sk-lf-demo`), and the exact payload bytes from the dry run.
6. QA failure scenario: exporting `LF_PKEY` instead of `LF_PK` failed with
   `error: LF_PK is not set`, exit code 2, zero network traffic; the corrected export
   then posted successfully.

Full transcript: `.omo/evidence/task-12-polyforge-oss-trust-stack.txt`.
