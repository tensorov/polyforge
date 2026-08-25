# polyforge-attest

Canonical JSON writer plus in-toto Statement v1 and DSSE envelope types for
PolyForge attestations, with a thin binary that turns an evidence ledger into
deterministic attestation statements.

The library layer provides:

- `canonical_json`: recursive canonical JSON (sorted object keys, compact separators)
- `Statement` / `Subject` / `DsseEnvelope` / `Signature`: in-toto Statement v1 and DSSE wire types
- `read_ledger`, `emit_task_statement`, `emit_chain_statement`: ledger reader plus deterministic statement emitters

## Binary

`polyforge-attest` reads a PolyForge JSONL evidence ledger and emits in-toto
statements with predicate type `https://polyforge.dev/attestations/evidence/v1`.
It never mutates the ledger.

```text
polyforge-attest task --ledger <path> --task <id> [--bundle <file>]
polyforge-attest chain --ledger <path> [--out <file>]
```

### task

Emits one statement attesting a single task's evidence subchain. The subject is
named `polyforge/task/<task_id>@<first 12 hex chars of commit_sha>`; its sha256
digest covers the gate bundle file when `--bundle` is given, otherwise the
canonical serialization of the task's ledger entries. The predicate carries
kind counts, the final evidence state, the environment fingerprint, and the
record-only identity fields.

```sh
polyforge-attest task --ledger .pf/ledger.jsonl --task bootstrap | python3 -m json.tool
```

### chain

Emits one statement attesting the whole ledger chain. The subject digest is the
chain tail hash; the predicate carries the entry count, the tail hash, and the
anchor sidecar hash when an Anchor entry carries one. Without `--out` the
statement JSON goes to stdout:

```sh
polyforge-attest chain --ledger .pf/ledger.jsonl | python3 -m json.tool
```

With `--out <file>` the command instead writes a DSSE envelope to the file:
the payload is the base64-encoded canonical statement bytes, `payloadType` is
`application/vnd.in-toto+json`, and `signatures` is empty (the envelope is
unsigned; signing happens downstream, for example with cosign):

```sh
polyforge-attest chain --ledger .pf/ledger.jsonl --out /tmp/chain-envelope.json
```

### Exit codes

`0` on success. Usage errors (unknown flag or subcommand, missing arguments,
missing flag values) print to stderr and exit `2`, mirroring `polyforge-cli`
conventions. Ledger read failures and emit errors also exit `2` with an
`error:` message and never panic on malformed input.

## Canonical serialization

Every statement and envelope produced here is serialized canonically: object
keys are sorted recursively and separators are compact, so identical input
always produces identical output bytes. This holds for stdout output, for the
base64 payload inside a DSSE envelope, and across repeated runs of the same
command against the same ledger.
