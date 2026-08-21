//! Append-only SHA-256 Merkle-chained evidence ledger.
//!
//! The ledger is a JSONL file: one JSON object per line, written strictly
//! append-only via `OpenOptions::append(true)`. Every entry carries a
//! `prev_hash` linking it to the canonical JSON of the previous entry (empty
//! for the genesis entry) and a `hash` binding its own identity. Any rewind,
//! reorder, or byte-level tamper breaks the chain and is reported by
//! [`Ledger::verify_chain`] as [`LedgerError::ChainBroken`].
//!
//! Determinism: hashes are pure functions of the entry content fields
//! (`seq`, `prev_hash`, `kind`, `payload`, `tool_version`, `env_fingerprint`,
//! `ts`). The ledger never injects a wallclock; the caller supplies `ts` as a
//! datum. The model's claimed verdict is deliberately NOT part of the hash
//! input — the hash covers the payload, not judgement.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Test-only race-window hook: when nonzero, `Ledger::append` sleeps this many
/// milliseconds between reading the ledger and writing the new entry. The
/// chaos test arms it so that concurrent unlocked appends deterministically
/// read the same head and lose entries; under the exclusive lock the sleep
/// merely serializes the appends and the same test passes.
#[cfg(test)]
static RACE_WINDOW_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Test-only timing scaffolding; sleep(0) ~= no-op, mutants here are
/// equivalent.
///
/// Scoped to #[cfg(test)], outside cargo-mutants' default production scope,
/// and equivalent anyway; no skip registry entry is needed.
#[cfg(test)]
fn race_window_sleep() {
    let race_ms = RACE_WINDOW_MS.load(std::sync::atomic::Ordering::SeqCst);
    if race_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(race_ms));
    }
}

/// A single append-only evidence entry.
///
/// `seq`, `prev_hash` and `hash` are computed by [`Ledger::append`]; the
/// caller supplies the content fields (`kind`, `payload`, `tool_version`,
/// `env_fingerprint`, `ts`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceEntry {
    /// Monotonic sequence number; `0` for the genesis entry.
    pub seq: u64,
    /// SHA-256 hex of the canonical JSON of the previous entry; empty for genesis.
    pub prev_hash: String,
    /// Evidence kind (opaque string for T2; tri-state kinds land in T3).
    pub kind: String,
    /// The datum payload (opaque JSON value).
    pub payload: serde_json::Value,
    /// Tool version that produced this entry (empty if not tool-produced).
    pub tool_version: String,
    /// Environment fingerprint (empty if not tool-produced).
    pub env_fingerprint: String,
    /// Timestamp datum (opaque string; supplied by the caller, never injected).
    pub ts: String,
    /// SHA-256 hex binding this entry's identity.
    pub hash: String,
    /// Canonical hash encoding version. `0` (serde default) marks a legacy v1
    /// entry; fresh entries are `2`. Only version 2 verifies.
    #[serde(default)]
    pub hash_version: u8,
}

impl EvidenceEntry {
    /// Build a content-only entry. `seq`, `prev_hash` and `hash` are filled
    /// in by [`Ledger::append`].
    pub fn new(
        kind: impl Into<String>,
        payload: serde_json::Value,
        tool_version: impl Into<String>,
        env_fingerprint: impl Into<String>,
        ts: impl Into<String>,
    ) -> Self {
        Self {
            seq: 0,
            prev_hash: String::new(),
            kind: kind.into(),
            payload,
            tool_version: tool_version.into(),
            env_fingerprint: env_fingerprint.into(),
            ts: ts.into(),
            hash: String::new(),
            hash_version: 2,
        }
    }
}

/// Identifier of an appended entry (its sequence number).
pub type EntryId = u64;

/// Result of a successful [`Ledger::verify_chain`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainState {
    /// Number of entries in the chain.
    pub entry_count: u64,
    /// SHA-256 hex of the head (last) entry.
    pub head_hash: String,
}

/// Head-anchor persisted alongside the ledger JSONL so that a rewind is
/// detectable. A pure recompute-from-genesis cannot catch trailing truncation (any
/// prefix of a hash chain is internally valid), so `append` records the head hash
/// and entry count in a small sidecar file, updated atomically (write-temp +
/// rename). `verify_chain` recomputes the chain AND checks the recomputed head
/// against this anchor; a mismatch (rewind, or a deleted anchor) is reported as
/// [`LedgerError::ChainBroken`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Anchor {
    entry_count: u64,
    head_hash: String,
}

/// Errors produced by the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedgerError {
    /// Underlying I/O failure.
    Io(String),
    /// A stored line is not valid JSON / not a valid entry.
    Json(String),
    /// The chain is broken at `seq`: `expected` hash did not match `found`.
    ChainBroken {
        seq: u64,
        expected: String,
        found: String,
    },
    /// The ledger file is empty (no genesis entry yet).
    EmptyChain,
    /// The entry's hash encoding version is not supported. Only version 2
    /// verifies; legacy v1 entries stay readable but never verify.
    UnsupportedHashVersion { version: u8 },
}

/// An append-only, Merkle-chained evidence ledger backed by a JSONL file.
#[derive(Debug, Clone)]
pub struct Ledger {
    path: PathBuf,
}

impl Ledger {
    /// Open (creating if needed) the ledger at `path`.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    /// Append `entry` to the ledger, computing `seq`, `prev_hash` and `hash`.
    ///
    /// The caller-supplied `seq`/`prev_hash`/`hash` fields are ignored and
    /// overwritten. Returns the assigned sequence number.
    pub fn append(&mut self, mut entry: EvidenceEntry) -> Result<EntryId, LedgerError> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| LedgerError::Io(e.to_string()))?;
        file.lock_exclusive()
            .map_err(|e| LedgerError::Io(e.to_string()))?;

        let entries = self.read_entries()?;
        #[cfg(test)]
        race_window_sleep();
        let next_seq = entries.len() as u64;
        let prev_hash = match entries.last() {
            Some(prev) => compute_hash(prev),
            None => String::new(),
        };

        entry.seq = next_seq;
        entry.prev_hash = prev_hash;
        entry.hash_version = 2;
        entry.hash = compute_hash(&entry);

        let line = serde_json::to_string(&entry).map_err(|e| LedgerError::Json(e.to_string()))?;
        writeln!(file, "{line}").map_err(|e| LedgerError::Io(e.to_string()))?;
        self.write_anchor(&Anchor {
            entry_count: next_seq + 1,
            head_hash: entry.hash.clone(),
        })?;
        Ok(entry.seq)
    }

    /// Recompute the chain from genesis and report integrity.
    ///
    /// Returns [`LedgerError::ChainBroken`] on any rewind/tamper.
    pub fn verify_chain(&self) -> Result<ChainState, LedgerError> {
        let entries = self.read_entries()?;
        if entries.is_empty() {
            return Err(LedgerError::EmptyChain);
        }
        let mut prev_hash = String::new();
        for (i, entry) in entries.iter().enumerate() {
            if entry.hash_version != 2 {
                return Err(LedgerError::UnsupportedHashVersion {
                    version: entry.hash_version,
                });
            }
            let expected_seq = i as u64;
            if entry.seq != expected_seq {
                return Err(LedgerError::ChainBroken {
                    seq: entry.seq,
                    expected: expected_seq.to_string(),
                    found: entry.seq.to_string(),
                });
            }
            if entry.prev_hash != prev_hash {
                return Err(LedgerError::ChainBroken {
                    seq: entry.seq,
                    expected: prev_hash,
                    found: entry.prev_hash.clone(),
                });
            }
            let recomputed = compute_hash(entry);
            if entry.hash != recomputed {
                return Err(LedgerError::ChainBroken {
                    seq: entry.seq,
                    expected: recomputed,
                    found: entry.hash.clone(),
                });
            }
            prev_hash = compute_hash(entry);
        }
        let head = entries.last().expect("non-empty checked above");
        let recomputed = ChainState {
            entry_count: entries.len() as u64,
            head_hash: head.hash.clone(),
        };
        match self.read_anchor()? {
            Some(anchor) => {
                if anchor.entry_count != recomputed.entry_count
                    || anchor.head_hash != recomputed.head_hash
                {
                    return Err(LedgerError::ChainBroken {
                        seq: recomputed.entry_count.saturating_sub(1),
                        expected: anchor.head_hash,
                        found: recomputed.head_hash,
                    });
                }
            }
            None => {
                // A non-empty ledger must have an anchor; its absence is a tamper.
                return Err(LedgerError::ChainBroken {
                    seq: recomputed.entry_count.saturating_sub(1),
                    expected: "<anchor missing>".to_string(),
                    found: recomputed.head_hash,
                });
            }
        }
        Ok(recomputed)
    }

    /// Read every entry in the ledger (total retention — no pruning).
    pub fn iter_entries(&self) -> Result<Vec<EvidenceEntry>, LedgerError> {
        self.read_entries()
    }

    fn read_entries(&self) -> Result<Vec<EvidenceEntry>, LedgerError> {
        let file = match OpenOptions::new().read(true).open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(LedgerError::Io(e.to_string())),
        };
        let reader = BufReader::new(file);
        let mut entries = Vec::new();
        for line in reader.lines() {
            let line = line.map_err(|e| LedgerError::Io(e.to_string()))?;
            if line.trim().is_empty() {
                continue;
            }
            let entry: EvidenceEntry =
                serde_json::from_str(&line).map_err(|e| LedgerError::Json(e.to_string()))?;
            entries.push(entry);
        }
        Ok(entries)
    }

    fn anchor_path(&self) -> PathBuf {
        let mut s = self.path.as_os_str().to_owned();
        s.push(".anchor");
        PathBuf::from(s)
    }

    fn write_anchor(&self, anchor: &Anchor) -> Result<(), LedgerError> {
        let anchor_path = self.anchor_path();
        let tmp = anchor_path.with_extension("anchor.tmp");
        let json = serde_json::to_string(anchor).map_err(|e| LedgerError::Json(e.to_string()))?;
        std::fs::write(&tmp, json).map_err(|e| LedgerError::Io(e.to_string()))?;
        std::fs::rename(&tmp, &anchor_path).map_err(|e| LedgerError::Io(e.to_string()))?;
        Ok(())
    }

    fn read_anchor(&self) -> Result<Option<Anchor>, LedgerError> {
        let anchor_path = self.anchor_path();
        let content = match std::fs::read_to_string(&anchor_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(LedgerError::Io(e.to_string())),
        };
        let anchor: Anchor =
            serde_json::from_str(&content).map_err(|e| LedgerError::Json(e.to_string()))?;
        Ok(Some(anchor))
    }
}

/// Compute the identity hash of an entry using the version-tagged canonical
/// encoding: `SHA-256(hash_version || len(seq) || seq || len(prev_hash) ||
/// prev_hash || len(kind) || kind || len(payload) || payload ||
/// len(tool_version) || tool_version || len(env_fingerprint) ||
/// env_fingerprint || len(ts) || ts)`. Each `len(x)` is an 8-byte
/// little-endian length prefix; `seq` is encoded as its decimal string. The
/// length prefixes make every field boundary unambiguous, so no two distinct
/// entries can share a hash (the v1 concatenation allowed adjacent-field
/// collisions).
fn compute_hash(entry: &EvidenceEntry) -> String {
    let payload = serde_json::to_string(&entry.payload).unwrap_or_else(|_| "null".to_string());
    let mut input = Vec::with_capacity(64);
    input.push(entry.hash_version);
    for field in [
        entry.seq.to_string(),
        entry.prev_hash.clone(),
        entry.kind.clone(),
        payload,
        entry.tool_version.clone(),
        entry.env_fingerprint.clone(),
        entry.ts.clone(),
    ] {
        input.extend_from_slice(&(field.len() as u64).to_le_bytes());
        input.extend_from_slice(field.as_bytes());
    }
    hex(&Sha256::digest(&input))
}

/// Hex-encode a digest.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("pf-todo2-tests");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{name}-{}.jsonl", std::process::id()))
    }

    fn entry(kind: &str, payload: serde_json::Value) -> EvidenceEntry {
        EvidenceEntry::new(kind, payload, "polyforge-core-test", "env-test", "ts-1")
    }

    #[test]
    fn test_append_increments_seq() {
        let path = tmp_path("seq");
        let mut ledger = Ledger::new(&path);
        let a = ledger.append(entry("kind-a", json!({"x": 1}))).unwrap();
        let b = ledger.append(entry("kind-b", json!({"y": 2}))).unwrap();
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        let entries = ledger.iter_entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[1].seq, 1);
    }

    #[test]
    fn test_genesis_prev_hash_empty() {
        let path = tmp_path("genesis");
        let mut ledger = Ledger::new(&path);
        ledger.append(entry("kind", json!({"x": 1}))).unwrap();
        let entries = ledger.iter_entries().unwrap();
        assert_eq!(entries[0].prev_hash, "");
    }

    #[test]
    fn test_verify_ok_on_valid_chain() {
        let path = tmp_path("verify-ok");
        let mut ledger = Ledger::new(&path);
        ledger.append(entry("kind-a", json!({"x": 1}))).unwrap();
        ledger.append(entry("kind-b", json!({"y": 2}))).unwrap();
        ledger.append(entry("kind-c", json!({"z": 3}))).unwrap();
        let state = ledger.verify_chain().unwrap();
        assert_eq!(state.entry_count, 3);
        assert_eq!(state.head_hash.len(), 64);
    }

    #[test]
    fn test_tamper_detected() {
        let path = tmp_path("tamper");
        let mut ledger = Ledger::new(&path);
        ledger.append(entry("kind-a", json!({"x": 1}))).unwrap();
        ledger.append(entry("kind-b", json!({"y": 2}))).unwrap();
        // verify passes before tamper
        ledger.verify_chain().unwrap();
        // tamper: rewrite the kind value of entry 0 (keeps JSON valid, changes hash)
        let content = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<String> = content.lines().map(String::from).collect();
        lines[0] = lines[0].replacen("kind-a", "kind-Z", 1);
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let err = ledger.verify_chain().unwrap_err();
        assert!(matches!(err, LedgerError::ChainBroken { .. }));
    }

    #[test]
    fn test_rewind_detected() {
        let path = tmp_path("rewind");
        let mut ledger = Ledger::new(&path);
        ledger.append(entry("kind-a", json!({"x": 1}))).unwrap();
        ledger.append(entry("kind-b", json!({"y": 2}))).unwrap();
        ledger.verify_chain().unwrap();
        // truncate: drop the last line (rewind)
        let content = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<&str> = content.lines().collect();
        lines.pop();
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let err = ledger.verify_chain().unwrap_err();
        assert!(matches!(err, LedgerError::ChainBroken { .. }));
    }

    #[test]
    fn test_append_is_append_only() {
        let path = tmp_path("append-only");
        let mut ledger = Ledger::new(&path);
        ledger.append(entry("kind-a", json!({"x": 1}))).unwrap();
        let len1 = std::fs::metadata(&path).unwrap().len();
        ledger.append(entry("kind-b", json!({"y": 2}))).unwrap();
        let len2 = std::fs::metadata(&path).unwrap().len();
        ledger.append(entry("kind-c", json!({"z": 3}))).unwrap();
        let len3 = std::fs::metadata(&path).unwrap().len();
        assert!(len2 > len1, "file must strictly grow");
        assert!(len3 > len2, "file must strictly grow");
    }

    #[test]
    fn test_chaos_concurrent_appends_all_present_and_contiguous() {
        // 8 threads x 25 appends on one path. With the exclusive lock every
        // entry lands, seq is contiguous 0..199, and the chain verifies. The
        // race-window hook makes the unlocked run deterministically lose
        // entries (all threads read the same head before any write).
        const THREADS: usize = 8;
        const APPENDS: usize = 25;
        let path = tmp_path("chaos");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(Ledger::new(&path).anchor_path());
        RACE_WINDOW_MS.store(3, std::sync::atomic::Ordering::SeqCst);
        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                scope.spawn(|| {
                    let mut ledger = Ledger::new(&path);
                    for i in 0..APPENDS {
                        ledger
                            .append(EvidenceEntry::new(
                                "kind",
                                json!({"thread": "t", "i": i}),
                                "polyforge-core-test",
                                "env-test",
                                "ts-1",
                            ))
                            .unwrap();
                    }
                });
            }
        });
        RACE_WINDOW_MS.store(0, std::sync::atomic::Ordering::SeqCst);
        let ledger = Ledger::new(&path);
        let entries = ledger.iter_entries().unwrap();
        assert_eq!(
            entries.len(),
            THREADS * APPENDS,
            "every concurrent append must land; got {} entries",
            entries.len()
        );
        for (i, e) in entries.iter().enumerate() {
            assert_eq!(e.seq, i as u64, "seq must be contiguous at index {i}");
        }
        let state = ledger.verify_chain().unwrap();
        assert_eq!(state.entry_count, (THREADS * APPENDS) as u64);
    }

    #[test]
    fn test_hash_is_sha256_hex_64_chars() {
        let path = tmp_path("hash-format");
        let mut ledger = Ledger::new(&path);
        ledger.append(entry("kind", json!({"x": 1}))).unwrap();
        let entries = ledger.iter_entries().unwrap();
        for e in &entries {
            assert_eq!(e.hash.len(), 64, "hash must be 64 hex chars");
            assert!(e.hash.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn test_reappend_order_determinism() {
        let path_a = tmp_path("det-a");
        let path_b = tmp_path("det-b");
        let mut a = Ledger::new(&path_a);
        let mut b = Ledger::new(&path_b);
        for p in [json!({"x": 1}), json!({"y": 2}), json!({"z": 3})] {
            a.append(entry("kind", p.clone())).unwrap();
            b.append(entry("kind", p)).unwrap();
        }
        let sa = a.verify_chain().unwrap();
        let sb = b.verify_chain().unwrap();
        assert_eq!(
            sa.head_hash, sb.head_hash,
            "same sequence -> same head hash"
        );
    }

    #[test]
    fn test_verify_chain_detects_anchor_head_hash_mismatch() {
        // Mutant guard: `||` -> `&&` in verify_chain. With `&&`, a mismatch on
        // head_hash alone (entry_count still correct) would NOT be reported.
        let path = tmp_path("anchor-head-mismatch");
        let mut ledger = Ledger::new(&path);
        ledger.append(entry("kind-a", json!({"x": 1}))).unwrap();
        // Corrupt only the head_hash; entry_count stays correct.
        ledger
            .write_anchor(&Anchor {
                entry_count: 1,
                head_hash: "0".repeat(64),
            })
            .unwrap();
        let err = ledger.verify_chain().unwrap_err();
        assert!(matches!(err, LedgerError::ChainBroken { .. }));
    }

    #[test]
    fn test_read_entries_reports_io_error_on_bad_path() {
        // Mutant guard: `e.kind() == NotFound` -> `true` in read_entries. A
        // non-NotFound error (ENAMETOOLONG) must surface as Err(Io), not be
        // swallowed as an empty ledger.
        let path = tmp_path(&"x".repeat(300));
        let ledger = Ledger::new(&path);
        let err = ledger.read_entries().unwrap_err();
        assert!(matches!(err, LedgerError::Io(_)));
    }

    #[test]
    fn test_iter_entries_ok_empty_when_file_missing() {
        // Mutant guard: deleting the NotFound arm in read_entries would turn a
        // missing ledger file into Err(Io). A missing file must read as an
        // empty ledger.
        let path = tmp_path("iter-missing");
        let _ = std::fs::remove_file(&path);
        let ledger = Ledger::new(&path);
        assert_eq!(ledger.iter_entries().unwrap(), Vec::<EvidenceEntry>::new());
    }

    #[test]
    fn test_read_anchor_ok_none_when_anchor_missing() {
        // Mutant guards: `== NotFound` -> `false` and `==` -> `!=` in
        // read_anchor. A missing anchor file must yield Ok(None), not Err(Io).
        let path = tmp_path("anchor-missing");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(Ledger::new(&path).anchor_path());
        let ledger = Ledger::new(&path);
        assert_eq!(ledger.read_anchor().unwrap(), None);
    }

    #[test]
    fn test_read_anchor_reports_io_error_on_bad_path() {
        // Mutant guard: `e.kind() == NotFound` -> `true` in read_anchor. A
        // non-NotFound error (ENAMETOOLONG) must surface as Err(Io), not be
        // swallowed as Ok(None).
        let path = tmp_path(&"y".repeat(300));
        let ledger = Ledger::new(&path);
        let err = ledger.read_anchor().unwrap_err();
        assert!(matches!(err, LedgerError::Io(_)));
    }

    /// Build a v2 ledger entry with explicit content fields (hash_version 2).
    fn v2_entry(
        seq: u64,
        prev_hash: &str,
        kind: &str,
        payload: serde_json::Value,
        tool_version: &str,
        env_fingerprint: &str,
        ts: &str,
    ) -> EvidenceEntry {
        EvidenceEntry {
            seq,
            prev_hash: prev_hash.to_string(),
            kind: kind.to_string(),
            payload,
            tool_version: tool_version.to_string(),
            env_fingerprint: env_fingerprint.to_string(),
            ts: ts.to_string(),
            hash: String::new(),
            hash_version: 2,
        }
    }

    #[test]
    fn test_v2_hash_separates_adjacent_field_boundary() {
        // The ADJACENT-field boundary pair (tool_version|env_fingerprint|ts).
        // Under the old v1 concatenation both entries hash to 1460f9d1...: the
        // boundary between `fp="f", ts="c"` and `fp="fc", ts=""` is ambiguous.
        // The v2 length-prefixed encoding must separate them.
        let a = v2_entry(1, "", "a", json!({"x": 1}), "t", "f", "c");
        let b = v2_entry(1, "", "a", json!({"x": 1}), "t", "fc", "");
        assert_ne!(
            compute_hash(&a),
            compute_hash(&b),
            "v2 hash must separate the adjacent-field boundary pair"
        );
    }

    #[test]
    fn test_v2_hash_exact_values() {
        // Exact-hash guards: expected values derived BY HAND from the
        // length-prefixed canonical encoding spec (hash_version byte, then
        // 8-byte LE length prefix + field bytes per field, in order
        // seq|prev_hash|kind|payload|tool_version|env_fingerprint|ts). Any
        // mutant that drops a length prefix or reorders fields changes these.
        let a = v2_entry(1, "", "a", json!({"x": 1}), "t", "f", "c");
        assert_eq!(
            compute_hash(&a),
            "a68440551fe396410d3e2b8a4cfc119a23241dad85b3328f1a8fdb12af374aa2"
        );
        let b = v2_entry(1, "", "a", json!({"x": 1}), "t", "fc", "");
        assert_eq!(
            compute_hash(&b),
            "eae6be3f7958c7f589298cb12de6d9bf45f023ac153ce623b2878cd7649983dd"
        );
        let c = v2_entry(7, "ab", "kind", json!({"z": 2, "a": 1}), "tool", "fp", "ts");
        assert_eq!(
            compute_hash(&c),
            "b0e10536abe9ccf30532a66f87df6da42421eef553b8c868ae19ae842186f241"
        );
    }

    #[test]
    fn test_fresh_entry_hashes_as_v2_and_verifies() {
        // `new()` must set hash_version = 2 explicitly (serde default 0 would
        // silently hash fresh entries as legacy v1), and a fresh chain must
        // verify.
        let path = tmp_path("v2-fresh");
        let mut ledger = Ledger::new(&path);
        let e = EvidenceEntry::new("kind", json!({"x": 1}), "t", "f", "ts");
        assert_eq!(e.hash_version, 2, "new() must set hash_version = 2");
        ledger.append(e).unwrap();
        let state = ledger.verify_chain().unwrap();
        assert_eq!(state.entry_count, 1);
        assert_eq!(state.head_hash.len(), 64);
    }

    #[test]
    fn test_legacy_v1_entry_fails_closed() {
        // A legacy v1-format line (no hash_version key -> serde default 0)
        // stays readable for error reporting but must never verify.
        let path = tmp_path("v1-legacy");
        let ledger = Ledger::new(&path);
        let legacy = r#"{"seq":0,"prev_hash":"","kind":"a","payload":{"x":1},"tool_version":"t","env_fingerprint":"f","ts":"c","hash":"0000000000000000000000000000000000000000000000000000000000000000"}"#;
        std::fs::write(&path, legacy.to_string() + "\n").unwrap();
        let err = ledger.verify_chain().unwrap_err();
        assert!(
            matches!(err, LedgerError::UnsupportedHashVersion { version: 0 }),
            "legacy v1 entry must fail closed with UnsupportedHashVersion, got {err:?}"
        );
    }
}
