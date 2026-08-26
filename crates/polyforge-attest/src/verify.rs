//! Merkle-chain verification for parsed ledger entries.
//!
//! [`verify_chain`] replicates the exact entry-hash computation of
//! `polyforge-core` (`crates/polyforge-core/src/ledger.rs`, `compute_hash`)
//! so a statement is never emitted from a tampered or reordered ledger.
//!
//! The replicated v2 recipe, byte for byte:
//!
//! ```text
//! input  = hash_version (1 raw byte)
//!        ++ len(seq_str) as u64 LE ++ seq_str (decimal ASCII)
//!        ++ len(prev_hash)         ++ prev_hash
//!        ++ len(kind)              ++ kind
//!        ++ len(payload_json)      ++ payload_json   (compact serde_json)
//!        ++ len(tool_version)      ++ tool_version
//!        ++ len(env_fingerprint)   ++ env_fingerprint
//!        ++ len(ts)                ++ ts
//! hash   = lowercase hex( SHA-256(input) )
//! ```
//!
//! Genesis convention: `seq = 0`, `prev_hash = ""`. Only `hash_version == 2`
//! verifies; legacy v1 entries fail closed exactly like core.

use crate::emit::{sha256_hex, Entry, LedgerError};

/// Recomputes an entry's identity hash with core's exact v2 canonical
/// encoding. See the module docs for the byte recipe.
pub fn compute_entry_hash(entry: &Entry) -> String {
    let payload = serde_json::to_string(&entry.payload).unwrap_or_else(|_| "null".to_owned());
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
    sha256_hex(&input)
}

/// Verifies Merkle-chain integrity over `entries` in order:
///
/// 1. `seq` starts at the genesis value `0` and increments by exactly one per
///    entry;
/// 2. `entry[0].prev_hash` is empty (genesis convention) and every later
///    `prev_hash` equals the previous entry's stored `hash`;
/// 3. the recomputed v2 hash equals the stored `hash` for EVERY entry.
///
/// An empty slice is trivially valid (`Ok(())`): there is nothing to tamper
/// with and the emitters define deterministic output for it. Any violation
/// yields [`LedgerError::Integrity`] carrying the offending seq and reason;
/// this function never panics on adversarial content.
pub fn verify_chain(entries: &[Entry]) -> Result<(), LedgerError> {
    let mut prev_hash = String::new();
    for (i, entry) in entries.iter().enumerate() {
        if entry.hash_version != 2 {
            return Err(LedgerError::Integrity {
                msg: format!(
                    "entry at position {i} uses unsupported hash_version {} (only v2 verifies)",
                    entry.hash_version
                ),
            });
        }
        let expected_seq = i as u64;
        if entry.seq != expected_seq {
            return Err(LedgerError::Integrity {
                msg: format!(
                    "seq out of order at position {i}: expected {expected_seq}, found {}",
                    entry.seq
                ),
            });
        }
        if entry.prev_hash != prev_hash {
            return Err(LedgerError::Integrity {
                msg: format!(
                    "broken prev_hash link at seq {}: expected '{}', found '{}'",
                    entry.seq, prev_hash, entry.prev_hash
                ),
            });
        }
        let recomputed = compute_entry_hash(entry);
        if entry.hash != recomputed {
            return Err(LedgerError::Integrity {
                msg: format!(
                    "hash mismatch at seq {}: stored '{}', computed '{}'",
                    entry.seq, entry.hash, recomputed
                ),
            });
        }
        prev_hash = entry.hash.clone();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Builds an Entry mirroring core's hand-derived v2 test vectors.
    fn v2_entry(
        seq: u64,
        prev_hash: &str,
        kind: &str,
        payload: serde_json::Value,
        tool_version: &str,
        env_fingerprint: &str,
        ts: &str,
    ) -> Entry {
        Entry {
            seq,
            prev_hash: prev_hash.to_owned(),
            kind: kind.to_owned(),
            payload,
            hash: String::new(),
            env_fingerprint: env_fingerprint.to_owned(),
            tool_version: tool_version.to_owned(),
            ts: ts.to_owned(),
            hash_version: 2,
        }
    }

    #[test]
    fn replication_matches_core_pinned_vectors() {
        // Exact values pinned in polyforge-core's own tests, derived BY HAND
        // from the length-prefixed canonical encoding spec. If this crate's
        // replication drifts by even one byte, these break.
        let a = v2_entry(1, "", "a", json!({"x": 1}), "t", "f", "c");
        assert_eq!(
            compute_entry_hash(&a),
            "a68440551fe396410d3e2b8a4cfc119a23241dad85b3328f1a8fdb12af374aa2"
        );
        let b = v2_entry(1, "", "a", json!({"x": 1}), "t", "fc", "");
        assert_eq!(
            compute_entry_hash(&b),
            "eae6be3f7958c7f589298cb12de6d9bf45f023ac153ce623b2878cd7649983dd"
        );
        let c = v2_entry(7, "ab", "kind", json!({"z": 2, "a": 1}), "tool", "fp", "ts");
        assert_eq!(
            compute_entry_hash(&c),
            "b0e10536abe9ccf30532a66f87df6da42421eef553b8c868ae19ae842186f241"
        );
    }

    #[test]
    fn empty_slice_is_trivially_valid() {
        assert_eq!(verify_chain(&[]), Ok(()));
    }

    fn chained(n: usize) -> Vec<Entry> {
        let mut out = Vec::new();
        let mut prev = String::new();
        for i in 0..n {
            let mut e = v2_entry(
                i as u64,
                &prev,
                "ModelClaim",
                json!({"task_id": "t", "i": i}),
                "",
                "",
                "ts",
            );
            e.hash = compute_entry_hash(&e);
            prev = e.hash.clone();
            out.push(e);
        }
        out
    }

    #[test]
    fn valid_chain_verifies() {
        assert_eq!(verify_chain(&chained(4)), Ok(()));
    }

    #[test]
    fn wrong_seq_is_rejected() {
        let mut entries = chained(3);
        entries[2].seq = 9;
        let err = verify_chain(&entries).unwrap_err();
        assert!(matches!(err, LedgerError::Integrity { .. }), "{err:?}");
        assert!(err.to_string().contains("seq out of order"));
    }

    #[test]
    fn broken_prev_link_is_rejected() {
        let mut entries = chained(3);
        entries[2].prev_hash = "0".repeat(64);
        let err = verify_chain(&entries).unwrap_err();
        assert!(matches!(err, LedgerError::Integrity { .. }), "{err:?}");
        assert!(err.to_string().contains("prev_hash"));
    }

    #[test]
    fn genesis_prev_hash_must_be_empty() {
        let mut entries = chained(1);
        entries[0].prev_hash = "nonempty".to_owned();
        let err = verify_chain(&entries).unwrap_err();
        assert!(matches!(err, LedgerError::Integrity { .. }), "{err:?}");
    }

    #[test]
    fn tampered_hash_is_rejected() {
        let mut entries = chained(3);
        entries[1].hash = entries[1].hash.replacen('a', "b", 1);
        let err = verify_chain(&entries).unwrap_err();
        assert!(matches!(err, LedgerError::Integrity { .. }), "{err:?}");
        assert!(err.to_string().contains("hash mismatch"));
    }

    #[test]
    fn tampered_payload_is_rejected() {
        let mut entries = chained(2);
        entries[0].payload = json!({"task_id": "t", "i": 999});
        let err = verify_chain(&entries).unwrap_err();
        assert!(matches!(err, LedgerError::Integrity { .. }), "{err:?}");
    }

    #[test]
    fn legacy_v1_fails_closed() {
        let mut entries = chained(1);
        entries[0].hash_version = 0;
        let err = verify_chain(&entries).unwrap_err();
        assert!(matches!(err, LedgerError::Integrity { .. }), "{err:?}");
        assert!(err.to_string().contains("unsupported hash_version"));
    }
}
