This tool is OPTIONAL analytics and is NEVER part of the PolyForge trust contour: gates never depend on it.

# langfuse-bridge

Standalone binary that reads one PolyForge gate manifest (the `gate-<task_id>.manifest.json` written by `polyforge-cli gate`) and posts a single score object to a SELF-HOSTED Langfuse instance, so agent traces can carry gate outcomes.

## Build

```sh
cargo build --manifest-path tools/langfuse-bridge/Cargo.toml
```

## Usage

```sh
langfuse-bridge <gate-manifest.json> [--dry-run]
```

Environment variables:

| Variable      | Meaning                                              |
| ------------- | ---------------------------------------------------- |
| `LF_BASE_URL` | Base URL of the self-hosted Langfuse, e.g. `http://localhost:3000` |
| `LF_PK`       | Langfuse public key (basic auth)                     |
| `LF_SK`       | Langfuse secret key (basic auth)                     |

Trace id resolution: `run_id` field first, then `metadata.langfuse_trace_id`, else one warning line on stderr and exit 0 with zero HTTP posts.

With `--dry-run` the exact payload bytes are printed to stdout and nothing is posted.

Payload shape: `{"name":"gate","value":passed?1:0,"traceId":"..."}`.

The tool performs exactly ONE `POST {LF_BASE_URL}/api/public/ingestion` with header `Authorization: Basic base64(LF_PK:LF_SK)`. No retries. Plain HTTP only (no TLS); point `LF_BASE_URL` at your self-hosted instance.

## Tests

Mock-server based; no real Langfuse needed.

```sh
cargo test --manifest-path tools/langfuse-bridge/Cargo.toml
```
