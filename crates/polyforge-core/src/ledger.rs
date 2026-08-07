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

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
        let entries = self.read_entries()?;
        let next_seq = entries.len() as u64;
        // prev_hash = SHA-256 of the previous entry's canonical JSON (plan contract)
        let prev_hash = match entries.last() {
            Some(prev) => {
                let canonical =
                    serde_json::to_string(prev).map_err(|e| LedgerError::Json(e.to_string()))?;
                hex(&Sha256::digest(canonical.as_bytes()))
            }
            None => String::new(),
        };

        entry.seq = next_seq;
        entry.prev_hash = prev_hash;
        entry.hash = compute_hash(&entry);

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| LedgerError::Io(e.to_string()))?;
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
            prev_hash = hex(&Sha256::digest(
                serde_json::to_string(entry)
                    .map_err(|e| LedgerError::Json(e.to_string()))?
                    .as_bytes(),
            ));
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

/// Compute the identity hash of an entry:
/// `SHA-256(seq || prev_hash || kind || canonical(payload) || tool_version || env_fingerprint || ts)`.
fn compute_hash(entry: &EvidenceEntry) -> String {
    let payload = serde_json::to_string(&entry.payload).unwrap_or_else(|_| "null".to_string());
    let input = format!(
        "{}{}{}{}{}{}{}",
        entry.seq,
        entry.prev_hash,
        entry.kind,
        payload,
        entry.tool_version,
        entry.env_fingerprint,
        entry.ts,
    );
    hex(&Sha256::digest(input.as_bytes()))
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
}
