//! Ledger reader plus deterministic in-toto statement emitters.
//!
//! [`read_ledger`] parses a PolyForge JSONL evidence ledger into [`Entry`]
//! values. [`emit_task_statement`] and [`emit_chain_statement`] turn those
//! entries into in-toto Statement v1 attestations whose bytes are fully
//! deterministic: identical input always produces identical output because
//! every digest is taken over canonical JSON (sorted keys, compact
//! separators) via [`crate::canon::canonical_json`].

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::canon::canonical_json;
use crate::{Statement, Subject};

/// Predicate type URI carried by every statement emitted by this module.
pub const POLYFORGE_EVIDENCE_PREDICATE_V1: &str = "https://polyforge.dev/attestations/evidence/v1";

/// Errors raised while reading a ledger or emitting statements from it.
#[derive(Debug)]
pub enum LedgerError {
    /// A non-empty line failed to parse as a ledger entry; `line` is the
    /// 1-based line number in the file.
    MalformedLine { line: usize },
    /// A semantic integrity problem (for example no entries for a task).
    Integrity { msg: String },
    /// An empty task id was supplied to [`emit_task_statement`].
    EmptyTaskId,
    /// Filesystem failure with the underlying OS message.
    Io(String),
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedLine { line } => {
                write!(f, "malformed ledger line at line {line}")
            }
            Self::Integrity { msg } => write!(f, "ledger integrity error: {msg}"),
            Self::EmptyTaskId => write!(f, "task id must not be empty"),
            Self::Io(msg) => write!(f, "io error: {msg}"),
        }
    }
}

impl std::error::Error for LedgerError {}

/// One append-only ledger entry as stored on disk.
///
/// Mirrors the top-level fields of a `.pf/ledger.jsonl` line that the
/// emitters need; unknown extra fields on the wire are ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Monotonic sequence number of the entry.
    pub seq: u64,
    /// Hash of the previous entry (empty for the first entry).
    #[serde(default)]
    pub prev_hash: String,
    /// Evidence kind, for example `ModelClaim` or `ToolAttestation`.
    pub kind: String,
    /// Opaque payload object; task id, commit sha, state and identity
    /// fields live here.
    pub payload: Value,
    /// Merkle chain hash of this entry (the chain tail is the last hash).
    #[serde(default)]
    pub hash: String,
    /// Environment fingerprint recorded at append time (may be empty).
    #[serde(default)]
    pub env_fingerprint: String,
}

/// Reads a JSONL ledger file into entries.
///
/// Blank lines are skipped. Any other line that fails to parse yields
/// [`LedgerError::MalformedLine`] with its 1-based line number; the reader
/// never panics on malformed content.
pub fn read_ledger(path: &Path) -> Result<Vec<Entry>, LedgerError> {
    let content = std::fs::read_to_string(path).map_err(|e| LedgerError::Io(e.to_string()))?;
    let mut entries = Vec::new();
    for (idx, raw) in content.lines().enumerate() {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let line_no = idx + 1;
        let entry: Entry = serde_json::from_str(trimmed)
            .map_err(|_| LedgerError::MalformedLine { line: line_no })?;
        entries.push(entry);
    }
    Ok(entries)
}

/// Hex-encodes the SHA-256 digest of `data` as 64 lowercase characters.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    let out = hasher.finalize();
    let mut hex = String::with_capacity(out.len() * 2);
    for byte in out {
        use fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn payload_str<'a>(entry: &'a Entry, key: &str) -> Option<&'a str> {
    entry.payload.get(key).and_then(Value::as_str)
}

fn short12(hex: &str) -> String {
    hex.chars().take(12).collect()
}

fn last_non_empty<'a>(
    entries: &[&'a Entry],
    pick: impl Fn(&'a Entry) -> Option<&'a str>,
) -> Option<String> {
    entries
        .iter()
        .rev()
        .find_map(|e| pick(e).filter(|s| !s.is_empty()))
        .map(str::to_owned)
}

/// Emits an in-toto statement attesting one task's evidence subchain.
///
/// Subject name follows `polyforge/task/<task_id>@<first 12 hex chars of
/// commit_sha>` where commit_sha comes from the task's payload fields. The
/// subject digest is the SHA-256 of the gate bundle file when `bundle_path`
/// is given, otherwise the SHA-256 of the concatenated canonical JSON of the
/// task's entries. Identical inputs always produce identical statements.
pub fn emit_task_statement(
    entries: &[Entry],
    task_id: &str,
    bundle_path: Option<&str>,
) -> Result<Statement, LedgerError> {
    if task_id.is_empty() {
        return Err(LedgerError::EmptyTaskId);
    }
    let task_entries: Vec<&Entry> = entries
        .iter()
        .filter(|e| payload_str(e, "task_id") == Some(task_id))
        .collect();
    if task_entries.is_empty() {
        return Err(LedgerError::Integrity {
            msg: format!("no ledger entries for task '{task_id}'"),
        });
    }

    let commit_sha =
        last_non_empty(&task_entries, |e| payload_str(e, "commit_sha")).unwrap_or_default();
    let name = format!("polyforge/task/{task_id}@{}", short12(&commit_sha));

    let digest_hex = match bundle_path {
        Some(path) => {
            let bytes = std::fs::read(path).map_err(|e| LedgerError::Io(e.to_string()))?;
            sha256_hex(&bytes)
        }
        None => {
            let mut subchain = String::new();
            for entry in &task_entries {
                let value = serde_json::to_value(entry).expect("Entry serialization cannot fail");
                subchain.push_str(&canonical_json(&value));
            }
            sha256_hex(subchain.as_bytes())
        }
    };

    let mut kind_counts: BTreeMap<&str, u64> = BTreeMap::new();
    for entry in &task_entries {
        *kind_counts.entry(entry.kind.as_str()).or_insert(0) += 1;
    }
    let final_state =
        last_non_empty(&task_entries, |e| payload_str(e, "state")).unwrap_or_default();

    // Identity fields are record-only metadata: take the last non-null value
    // across the task's entries so later appends win deterministically.
    let identity_str = |key: &str| -> Option<String> {
        task_entries
            .iter()
            .rev()
            .find_map(|e| e.payload.get(key))
            .filter(|v| !v.is_null())
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let eval_metadata = task_entries
        .iter()
        .rev()
        .find_map(|e| e.payload.get("eval_metadata"))
        .filter(|v| !v.is_null())
        .cloned();

    let mut predicate = Map::new();
    predicate.insert(
        "kind_counts".to_owned(),
        kind_counts
            .iter()
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect::<Map<String, Value>>()
            .into(),
    );
    predicate.insert("final_state".to_owned(), json!(final_state));
    predicate.insert(
        "env_fingerprint".to_owned(),
        match last_non_empty(&task_entries, |e| Some(e.env_fingerprint.as_str())) {
            Some(fp) => json!(fp),
            None => Value::Null,
        },
    );
    for key in ["experiment_id", "model_fingerprint", "run_id", "budget"] {
        predicate.insert(
            key.to_owned(),
            match identity_str(key) {
                Some(v) => json!(v),
                None => Value::Null,
            },
        );
    }
    predicate.insert(
        "eval_metadata".to_owned(),
        eval_metadata.unwrap_or(Value::Null),
    );

    Ok(Statement::new(
        vec![Subject::new(name, digest_hex)],
        POLYFORGE_EVIDENCE_PREDICATE_V1,
        Value::Object(predicate),
    ))
}

/// Emits an in-toto statement attesting the whole ledger chain.
///
/// The subject digest is the chain tail hash (the `hash` field of the last
/// entry); for an empty ledger it falls back to the SHA-256 of the empty
/// byte string so the output stays valid and deterministic. The predicate
/// carries the entry count, the tail hash, and the anchor sidecar hash when
/// an `Anchor`-kind entry carries one in its payload (`head_hash`).
pub fn emit_chain_statement(entries: &[Entry]) -> Statement {
    let tail = entries
        .last()
        .map(|e| e.hash.clone())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| sha256_hex(b""));
    let anchor_sidecar_hash = entries
        .iter()
        .rev()
        .find(|e| e.kind == "Anchor")
        .and_then(|e| e.payload.get("head_hash"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    let subject = Subject::new(format!("polyforge/chain@{}", short12(&tail)), tail.clone());
    Statement::new(
        vec![subject],
        POLYFORGE_EVIDENCE_PREDICATE_V1,
        json!({
            "seq_count": entries.len(),
            "tail": tail,
            "anchor_sidecar_hash": anchor_sidecar_hash,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(seq: u64, prev: &str, kind: &str, payload: Value, hash: &str) -> Entry {
        Entry {
            seq,
            prev_hash: prev.to_owned(),
            kind: kind.to_owned(),
            payload,
            hash: hash.to_owned(),
            env_fingerprint: String::new(),
        }
    }

    #[test]
    fn sha256_hex_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn empty_ledger_chain_statement_hashes_empty_input() {
        let s = emit_chain_statement(&[]);
        assert_eq!(
            s.subject[0].digest.get("sha256").map(String::as_str),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
    }

    #[test]
    fn chain_statement_takes_tail_from_last_entry() {
        let entries = vec![
            entry(0, "", "ModelClaim", json!({"task_id": "t"}), "aa11"),
            entry(1, "aa11", "Validation", json!({"task_id": "t"}), "bb22"),
        ];
        let s = emit_chain_statement(&entries);
        let v = serde_json::to_value(&s).expect("serializes");
        assert_eq!(v["predicate"]["tail"], "bb22");
        assert_eq!(v["predicate"]["seq_count"], 2);
        assert!(v["predicate"]["anchor_sidecar_hash"].is_null());
    }

    #[test]
    fn chain_statement_picks_anchor_entry_head_hash() {
        let entries = vec![entry(
            0,
            "",
            "Anchor",
            json!({"head_hash": "cc33", "entry_count": 1}),
            "dd44",
        )];
        let s = emit_chain_statement(&entries);
        let v = serde_json::to_value(&s).expect("serializes");
        assert_eq!(v["predicate"]["anchor_sidecar_hash"], "cc33");
    }

    #[test]
    fn task_statement_errors_without_matching_entries() {
        let entries = vec![entry(0, "", "ModelClaim", json!({"task_id": "a"}), "h")];
        let err = emit_task_statement(&entries, "missing", None).expect_err("must fail");
        assert!(matches!(err, LedgerError::Integrity { .. }));
    }
}
