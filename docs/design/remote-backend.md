# Remote backend design (roadmap 2.1)

Status: DESIGN DRAFT. Nothing here is implemented; this document commits to no code,
no DDL, and no new dependencies. It exists so that the eventual implementation of
[docs/ROADMAP.md](../ROADMAP.md) Phase 2 item 2.1 ("Remote backend: PostgreSQL + S3
(or DynamoDB) instead of a single `ledger.jsonl`") has a reviewed seam to build against.

The invariant that governs every decision below: **the evidence chain is the root of
trust** ([SECURITY.md](../../SECURITY.md), Assets). A remote backend may change where
bytes live; it must not change what those bytes mean.

## Goals and non-goals

### Goals

- G1. Remove the single-file scaling ceiling: many writers, many readers, retention
  measured in years, all without changing the Merkle-chain guarantees.
- G2. Introduce a storage abstraction (`LedgerStore`) such that today's JSONL behavior
  remains the default and its tests remain the specification.
- G3. Preserve byte-for-byte reproducibility of gate bundles regardless of backend.
- G4. Keep fail-closed semantics: any backend that cannot prove chain integrity yields
  an error, never a partial verdict.
- G5. Make the migration path incremental: each phase ships independently useful and
  independently revertible.

### Non-goals

- NG1. Changing `hash_version`, the canonical hash encoding, or the entry JSON shape.
  A v2 entry appended through a future PostgresStore must hash identically to the same
  entry appended through today's `Ledger`.
- NG2. External anchoring (Sigstore, tail-hash publication). That stays roadmap Phase 3;
  this design does not depend on it and does not deliver it.
- NG3. Multi-tenant SaaS isolation models, billing, or hosted operations.
- NG4. Replacing the tri-state promotion rules or the MCP surface. Models keep appending
  `ModelClaim` only; nothing about who may promote changes with the backend.
- NG5. Pruning or compaction of old entries. Total retention is preserved by design.

## Current state: the single-file ledger

The seam subject is `crates/polyforge-core/src/ledger.rs`. Today the whole persistence
layer is one struct, `Ledger`, over one JSONL file plus one sidecar:

- **Append-only JSONL Merkle chain.** Each line is one `EvidenceEntry` with fields
  `seq`, `prev_hash`, `kind`, `payload`, `tool_version`, `env_fingerprint`, `ts`,
  `hash`, `hash_version`. `append` ignores caller-supplied `seq`/`prev_hash`/`hash`,
  assigns `next_seq = number of existing lines`, sets `prev_hash` to the previous
  entry's hash (empty string for genesis), and computes `hash` with the version-tagged,
  length-prefixed canonical encoding (`hash_version = 2`): SHA-256 over the version
  byte followed by 8-byte little-endian length prefixes plus field bytes, in the fixed
  field order above. Length prefixes make field boundaries unambiguous, so no two
  distinct entries collide.
- **Exclusive lock per append.** Every `append` opens the file with
  `OpenOptions::append(true)` and takes an `fs2` `lock_exclusive()` before reading the
  head and writing the line. Concurrent processes serialize; the chaos test (8 threads
  x 25 appends) proves contiguity under contention.
- **Committed anchor sidecar.** After each append, `<ledger>.anchor`
  (`.pf/ledger.jsonl.anchor`) is rewritten atomically (write-temp + rename) with
  `entry_count` and `head_hash`. A pure recompute-from-genesis cannot catch trailing
  truncation (any prefix of a hash chain is internally valid); the anchor makes a
  rewind detectable.
- **Fail-closed `verify_chain`.** Recomputes from genesis and errors on: empty chain
  (`EmptyChain`), unsupported encoding (`UnsupportedHashVersion`; only version 2
  verifies, legacy v1 stays readable but never verifies), sequence gaps, `prev_hash`
  mismatches, recomputed-hash mismatches, anchor head/count mismatches, and a missing
  anchor on a non-empty ledger. Every failure surfaces as `ChainBroken` or a typed
  variant; nothing is ever silently accepted.
- **Reproducible gate bundles.** A passing gate writes `gate-<task_id>.jsonl` plus a
  manifest carrying `tail_hash` and `bundle_sha256`; a second PASS run produces a
  byte-identical bundle and identical digest. Failed integrity checks never fabricate
  a bundle.

What this buys: zero infrastructure, human-readable history, tamper evidence inside a
trusted checkout. What it costs: one writer host effectively owns the file, no remote
readers, no long-horizon durability beyond git, and O(n) full-file reads on every
append and verify.

## Storage trait seam

Design sketch only. The signatures below mirror exactly what `Ledger` does today, so
that extracting the trait is a pure move with no behavior change:

```rust
// DESIGN SKETCH - not an implementation, not a commitment.
// Mirrors crates/polyforge-core/src/ledger.rs behavior 1:1.
pub trait LedgerStore {
    /// Assign seq/prev_hash/hash and persist durably. Returns the assigned EntryId.
    fn append(&mut self, entry: EvidenceEntry) -> Result<EntryId, LedgerError>;

    /// Read every entry, in sequence order. Total retention: no pruning.
    fn iter_entries(&self) -> Result<Vec<EvidenceEntry>, LedgerError>;

    /// Recompute the chain from genesis, check the anchor, report integrity.
    fn verify_chain(&self) -> Result<ChainState, LedgerError>;
}

/// Today's behavior, unchanged: JSONL file + fs2 exclusive lock + atomic anchor sidecar.
pub struct FileStore { /* path */ }

/// Future impls behind the same trait.
pub struct PostgresStore { /* connection config */ }
pub struct S3BundleStore { /* bucket config; bundles only, see phase 3 */ }
```

Hard constraints on the seam, stated explicitly:

1. **Introducing the trait must NOT change `hash_version` or entry serialization.**
   The hash input stays the v2 length-prefixed canonical encoding over the same seven
   content fields; the persisted entry keeps the current serde JSON shape wherever the
   backend stores raw lines. Only the location and transport of bytes change.
2. `FileStore` implements today's behavior unchanged: same lock discipline, same anchor
   sidecar format, same error variants. Existing unit, chaos, and mutation-guard tests
   against `Ledger` must pass against `FileStore` without modification.
3. `Ledger` either becomes a thin facade over `Box<dyn LedgerStore>` (or a generic
   parameter) or is replaced by direct store usage at the CLI/MCP boundary; either way
   callers see identical results for identical inputs.
4. Error taxonomy stays backend-neutral: `Io`, `Json`, `ChainBroken`, `EmptyChain`,
   `UnsupportedHashVersion`. A remote backend maps its failures onto these (plus at
   most one new variant for transport unavailability, see threat-model section);
   it never invents success states.

## Migration path to PostgreSQL + S3

Four phases. Each phase is shippable alone; none requires the next.

### Phase 1: trait seam behind FileStore default

Extract `LedgerStore` as sketched above. CLI, MCP server, and the GitHub Action keep
using `FileStore` by default; configuration plumbing gains a backend selector that has
exactly one legal value. Acceptance: full workspace test suite green with zero test
edits; ledger fixtures produced before the refactor verify after it.

### Phase 2: PostgresStore for entries

Entries land in a relational store. Illustrative shape only (NOT a committed DDL):

- One row per entry; `seq BIGSERIAL`-style monotonic identifier as the primary key
  with a `UNIQUE(seq)` constraint; explicit `prev_hash` and `hash` columns mirroring
  the entry fields; the remaining content fields stored so that the exact v2 hash can
  be recomputed from stored data alone.
- Honest caveat carried forward as an open question: a normalized JSON type for
  `payload` re-serializes values (key order, whitespace) and can break byte-exact
  rehashing. Either the payload column preserves original bytes verbatim, or the row
  additionally carries the exact canonical line. This choice belongs to the
  implementation phase, not this document.
- Appends take the same logical steps as `FileStore::append`: read head, assign seq,
  set prev_hash, compute hash, persist, update anchor - all inside one database
  transaction instead of lock + write + rename.
- Reads map `iter_entries` to an ordered scan by seq.

### Phase 3: S3 for gate bundles, content-addressed by bundle_sha256

Gate bundles and manifests move to object storage keyed by their own digest
(`bundles/<bundle_sha256>`), because bundles are already reproducible and
content-addressed today. Consequences: free deduplication, immutable-by-construction
history (a changed bundle is a different key), and manifests gain a stable pointer
instead of a filesystem path. Optional hardening (object lock / versioning /
write-once policies) is operator configuration, not protocol. The ledger itself does
not move to S3 in this phase; S3 is deliberately unsuitable for the append-with-anchor
transaction of phase 4.

### Phase 4: anchor becomes a database row, written in the same transaction as append

The anchor stops being a sidecar file and becomes a row updated atomically with the
entry insert. This closes the last local-file artifact and makes rewind detection a
database-level property. The open design axis is how concurrent writers serialize.
Three candidate disciplines, none chosen here:

- **Advisory locks** (transaction-scoped advisory lock keyed on the ledger identity):
  closest analogue to today's `fs2` exclusive lock; simple to reason about; serializes
  all appends on one ledger, which matches the chain's inherent sequentiality.
- **Serializable isolation** with retry-on-serialization-failure: optimistic; lets
  verifiers run concurrently with writers; pays in retry storms under heavy append
  load and in subtler reasoning about what a failed transaction saw.
- **Single-writer queue**: one process owns appends (as one host owns the file today);
  everyone else reads replicas. Simplest trust story and easiest audit narrative; adds
  an operational component and a lag window for readers.

Whichever is chosen, the invariant is fixed: an append is visible to `verify_chain`
either completely (entry + anchor) or not at all, and two successful appends can never
observe the same head.

## Threat-model deltas vs local file

[SECURITY.md](../../SECURITY.md) defines three asset classes (ledger integrity, gate
verdicts, attestation provenance) and four attacker classes (repo-writer, MCP network
attacker, the model, tool-allowlist bypass). Moving to a remote backend changes the
trust boundaries; the deltas below must be folded back into SECURITY.md when phase 2
lands.

| Boundary | Local file today | Remote backend delta |
| -------- | ---------------- | -------------------- |
| Network transport | None (filesystem syscalls) | TLS termination, MITM exposure, availability attacks. A truncated or stalled response must look like a failure, never like a shorter valid chain. |
| Credentials | Filesystem permissions | DB credentials grant write power over history. Compromise scope moves from "one checkout" to "every ledger on the cluster"; least-privilege grants and append-constrained roles become part of the trust story. |
| Backup / restore | Git history + working tree | A restored snapshot can rewind the world past the anchor. Restore procedures must re-validate the anchor row against the restored entries and fail closed on mismatch. |
| Writer concurrency | fs2 lock on one host | Database-level serialization (phase 4); a lost-update bug here silently forks the chain. |
| Verifier location | Same host as the file | Verifiers cross the network; see timeout policy below. |

Fail-closed semantics across a network, stated as requirements:

- **Timeout policy.** `verify_chain` over a remote backend must treat connect timeouts,
  query timeouts, and mid-scan disconnects as verification failures. Bounded retries
  are permitted for idempotent reads; after the budget is exhausted the result is an
  error, never a verdict.
- **No partial-trust reads.** A `ChainState` may only be returned from a complete scan
  of all entries plus a consistent anchor read. Streaming a prefix and reporting
  "verified so far" is forbidden; there is no such thing as a partially verified chain.
- **Remote anchor verification.** The anchor row must be read in the same transaction
  or snapshot as the entries it anchors. A missing anchor row on a non-empty ledger
  remains tamper, exactly as the missing sidecar file is today.
- **Error honesty.** Transport unavailability and detected tampering are distinct
  conditions and must be reported distinctly, but both fail closed: neither produces a
  passing gate or a fabricated bundle.

Per SECURITY.md asset classes, local versus remote:

- **Ledger integrity**: preserved in principle (same hashes), but the attacker set
  grows by anyone with DB write access or network position; compensations are
  `UNIQUE(seq)`, append-only grants, audit logging, and the anchor-in-transaction rule.
- **Gate verdicts**: unchanged contract (fail-closed, reproducible bundles), new
  failure mode (backend unavailable means gate fails, which is correct but operationally
  loud).
- **Attestation provenance**: potentially strengthened - a database can attribute each
  append to an authenticated principal, something a shared file cannot do.

## Open questions

Honest unknowns; each needs an answer before its phase ships.

1. **Multi-region anchor consistency.** If entries replicate across regions but the
   anchor row has a home region, what happens during replication lag: can a verifier
   in a lagging region observe a valid-looking chain whose anchor disagrees? Is a
   single-writer-region architecture acceptable, or must anchor reads be
   quorum-consistent?
2. **Offline operation.** CI runners and air-gapped environments cannot reach a remote
   backend. Is falling back to `FileStore` allowed, and if so, how is the inevitable
   fork between offline and online chains prevented or reconciled? Or is offline
   simply unsupported for remote-backed repositories?
3. **Migration of existing ledgers.** How do existing `.pf/ledger.jsonl` files import
   into Postgres with hashes intact (byte-exact payload fidelity, genesis handling)?
   Is there a dual-write window, and what is the rollback story if the import is
   discovered to be lossy after new entries landed remotely?
4. **Cost model.** Total retention grows forever by design. What is the projected
   storage and egress cost per 100-agent fleet per year, and does the DynamoDB
   alternative named in the roadmap change the phase 4 concurrency story enough to
   matter? Who pays: per-repo budgets or central infrastructure?
5. **Key management ownership.** Who owns DB credentials, TLS certificates, and
   rotation? Per-repo service accounts versus a shared cluster identity; rotation
   cadence; and what happens to in-flight gates during a credential rollover.
6. **Payload fidelity versus queryability.** Byte-preserving payload storage protects
   rehashing but forfeits native JSON querying that a dashboard (roadmap 2.2) would
   want. Does the dashboard query a replica with derived columns, or do we accept
   dual representations of the payload?
7. **Writer-of-record identity.** Today an append is anonymous beyond its content
   fields. Should the remote backend require an authenticated principal per append and
   record it outside the hashed fields (so hashes stay stable), and does that
   principal model extend to the model-versus-operator distinction the promotion rules
   already enforce?
