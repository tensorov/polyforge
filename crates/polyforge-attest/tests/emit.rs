//! Integration tests for the ledger reader and statement emitters.
//!
//! `tests/fixtures/ledger.jsonl` is a verbatim copy of this repository's real
//! `.pf/ledger.jsonl` (4 entries, v2 hash format). Because `read_ledger`
//! verifies Merkle-chain integrity, every fixture-based test doubles as
//! cross-validation that the hash replication in [`polyforge_attest::verify`]
//! is byte-for-byte correct: any drift makes the known-good fixture fail.

use std::path::PathBuf;

use polyforge_attest::{
    canonical_json, compute_entry_hash, emit_chain_statement, emit_task_statement, read_ledger,
    sha256_hex, Entry, LedgerError, POLYFORGE_EVIDENCE_PREDICATE_V1,
};
use serde_json::{json, Value};

const FIXTURE_LEDGER: &str = "tests/fixtures/ledger.jsonl";

/// Tail of the fixture ledger as printed by
/// `polyforge-cli ledger tail` against the live repo (verified manually).
const EXPECTED_TAIL: &str = "5adaf8c94e7bc59fe54d993316284b82fd7e11a3f536e717abebc27dc734bfa6";

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_LEDGER)
}

fn temp_path(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "pf-attest-emit-{tag}-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    p
}

fn synthetic_entry(seq: u64, prev: &str, kind: &str, payload: Value, hash: &str) -> Entry {
    Entry {
        seq,
        prev_hash: prev.to_owned(),
        kind: kind.to_owned(),
        payload,
        hash: hash.to_owned(),
        env_fingerprint: String::new(),
        tool_version: String::new(),
        ts: String::new(),
        hash_version: 2,
    }
}

fn write_lines(tag: &str, lines: &[String]) -> PathBuf {
    let path = temp_path(tag);
    std::fs::write(&path, lines.join("\n") + "\n").expect("writes temp ledger");
    path
}

fn fixture_lines() -> Vec<String> {
    let raw = std::fs::read_to_string(fixture_path()).expect("fixture readable");
    raw.lines().map(str::to_owned).collect()
}

#[test]
fn empty_task_id_is_rejected() {
    let err = emit_task_statement(&[], "", None).expect_err("empty task id must fail");
    assert!(matches!(err, LedgerError::EmptyTaskId));
}

#[test]
fn missing_bundle_path_falls_back_to_subchain_digest() {
    let entries = read_ledger(&fixture_path()).expect("fixture parses and verifies");
    let statement = emit_task_statement(&entries, "bootstrap", None).expect("bootstrap emits");

    let mut subchain = String::new();
    for entry in entries
        .iter()
        .filter(|e| e.payload.get("task_id").and_then(Value::as_str) == Some("bootstrap"))
    {
        let value = serde_json::to_value(entry).expect("entry serializes");
        subchain.push_str(&canonical_json(&value));
    }
    let expected = sha256_hex(subchain.as_bytes());

    assert_eq!(
        statement.subject[0]
            .digest
            .get("sha256")
            .map(String::as_str),
        Some(expected.as_str()),
        "fallback digest must be sha256 over concatenated canonical subchain"
    );
}

#[test]
fn unicode_task_id_is_rejected_by_charset_policy() {
    let task_id = "задача-🎉-42";
    let entries = vec![synthetic_entry(
        0,
        "",
        "ModelClaim",
        json!({"task_id": task_id, "commit_sha": "abcdef1234567890", "state": "ModelClaimed"}),
        "h0",
    )];
    let err =
        emit_task_statement(&entries, task_id, None).expect_err("unicode task id must be rejected");
    match err {
        LedgerError::InvalidIdentifier { field, value } => {
            assert_eq!(field, "task_id");
            assert_eq!(value, task_id);
        }
        other => panic!("expected InvalidIdentifier, got {other:?}"),
    }
}

#[test]
fn two_consecutive_emits_are_byte_equal() {
    let entries = read_ledger(&fixture_path()).expect("fixture parses and verifies");
    let first = emit_task_statement(&entries, "bootstrap", None).expect("emits");
    let second = emit_task_statement(&entries, "bootstrap", None).expect("emits");

    let a = canonical_json(&serde_json::to_value(&first).expect("serializes")).into_bytes();
    let b = canonical_json(&serde_json::to_value(&second).expect("serializes")).into_bytes();
    assert_eq!(a, b, "identical input must produce identical bytes");

    let chain_a = canonical_json(
        &serde_json::to_value(emit_chain_statement(&entries).expect("emits")).expect("serializes"),
    )
    .into_bytes();
    let chain_b = canonical_json(
        &serde_json::to_value(emit_chain_statement(&entries).expect("emits")).expect("serializes"),
    )
    .into_bytes();
    assert_eq!(chain_a, chain_b);
}

#[test]
fn truncated_line_maps_to_malformed_not_panic() {
    let path = temp_path("truncated");
    std::fs::write(
        &path,
        concat!(
            r#"{"seq":0,"prev_hash":"","kind":"ModelClaim","payload":{"task_id":"t"},"hash":"aa"}"#,
            "\n",
            r#"{"seq":1,"prev_hash":"aa","kind":"ToolAttestation","payload":{"task_id""#,
        ),
    )
    .expect("writes temp ledger");
    let result = read_ledger(&path);
    std::fs::remove_file(&path).ok();

    match result {
        Err(LedgerError::MalformedLine { line }) => assert_eq!(line, 2),
        other => panic!("expected MalformedLine at line 2, got {other:?}"),
    }
}

#[test]
fn happy_path_against_repo_fixture() {
    let entries = read_ledger(&fixture_path()).expect("real ledger copy parses AND verifies");
    assert_eq!(entries.len(), 4);

    let task = emit_task_statement(&entries, "bootstrap", None).expect("bootstrap emits");
    assert_eq!(task.predicate_type, POLYFORGE_EVIDENCE_PREDICATE_V1);
    assert_eq!(task._type, "https://in-toto.io/Statement/v1");
    assert_eq!(
        task.subject[0].name, "polyforge/task/bootstrap@bfe611b4b91e",
        "subject pins first 12 hex chars of payload commit_sha"
    );

    let v = serde_json::to_value(&task).expect("serializes");
    assert_eq!(v["predicate"]["final_state"], "Verified");
    assert_eq!(v["predicate"]["kind_counts"]["ModelClaim"], 1);
    assert_eq!(v["predicate"]["kind_counts"]["ToolAttestation"], 1);
    assert_eq!(
        v["predicate"]["env_fingerprint"],
        "a9c7265ab007d6b1215bbcde7b9deb3be405a52240dced729c8fb5f8376cb8f5|nix=none|devbox=none|cargo.lock=4be1c47f02253ab9b4b79e962ac632b8fd0d4fc1fe76df87aa5de45e11295c87"
    );
    assert!(v["predicate"]["experiment_id"].is_null());
    assert!(v["predicate"]["eval_metadata"].is_null());

    let chain = emit_chain_statement(&entries).expect("emits");
    let cv = serde_json::to_value(&chain).expect("serializes");
    assert_eq!(
        cv["predicate"]["tail"], EXPECTED_TAIL,
        "tail is the hash field of the last entry"
    );
    assert_eq!(cv["predicate"]["seq_count"], 4);
    assert!(cv["predicate"]["anchor_sidecar_hash"].is_null());
    assert_eq!(
        chain.subject[0].digest.get("sha256").map(String::as_str),
        Some(EXPECTED_TAIL)
    );
}

// ---------------------------------------------------------------------------
// Cross-validation: the fixture is the REAL repo ledger; read_ledger verifying
// it proves the replicated hash algorithm matches polyforge-core byte for byte
// on production data, not just on synthetic vectors.
// ---------------------------------------------------------------------------

#[test]
fn t5_valid_fixture_reads_ok_and_tail_matches_cli_tail() {
    let entries = read_ledger(&fixture_path())
        .expect("known-good real ledger must verify with no false positive");
    assert_eq!(entries.len(), 4);
    let statement = emit_chain_statement(&entries).expect("chain emits");
    assert_eq!(
        statement.subject[0]
            .digest
            .get("sha256")
            .map(String::as_str),
        Some(EXPECTED_TAIL),
        "subject digest must equal the tail printed by polyforge-cli ledger tail"
    );
    for e in &entries {
        assert_eq!(
            compute_entry_hash(e),
            e.hash,
            "every stored hash recomputes"
        );
    }
}

fn tampered_fixture(tag: &str, mutate: impl Fn(&mut Vec<String>)) -> LedgerError {
    let mut lines = fixture_lines();
    assert_eq!(lines.len(), 4, "fixture shape guard");
    mutate(&mut lines);
    let path = write_lines(tag, &lines);
    let result = read_ledger(&path);
    std::fs::remove_file(&path).ok();
    result.expect_err("tampered ledger must be rejected")
}

#[test]
fn t1_flipped_last_hash_is_rejected_as_integrity() {
    let err = tampered_fixture("t1-flip-hash", |lines| {
        let last = lines.last_mut().expect("non-empty");
        let mut v: Value = serde_json::from_str(last).expect("line parses");
        let hash = v["hash"].as_str().expect("hash field").to_owned();
        let flipped: String = hash
            .chars()
            .enumerate()
            .map(|(i, c)| if i == 0 { '0' } else { c })
            .collect();
        assert_ne!(flipped, hash, "mutation must change the hash");
        v["hash"] = json!(flipped);
        *last = serde_json::to_string(&v).expect("re-serializes");
    });
    assert!(matches!(err, LedgerError::Integrity { .. }), "{err:?}");
    assert!(err.to_string().contains("hash mismatch"), "{err}");
}

#[test]
fn t2_swapped_adjacent_entries_break_seq_order() {
    let err = tampered_fixture("t2-swap", |lines| {
        lines.swap(1, 2);
    });
    assert!(matches!(err, LedgerError::Integrity { .. }), "{err:?}");
    assert!(err.to_string().contains("seq out of order"), "{err}");
}

#[test]
fn t3_modified_prev_hash_of_entry_2_is_rejected() {
    let err = tampered_fixture("t3-prev-hash", |lines| {
        let line = &mut lines[2];
        let mut v: Value = serde_json::from_str(line).expect("line parses");
        v["prev_hash"] = json!("0".repeat(64));
        *line = serde_json::to_string(&v).expect("re-serializes");
    });
    assert!(matches!(err, LedgerError::Integrity { .. }), "{err:?}");
    assert!(err.to_string().contains("prev_hash"), "{err}");
}

#[test]
fn t4_deleted_middle_entry_leaves_seq_gap() {
    let err = tampered_fixture("t4-delete-middle", |lines| {
        lines.remove(1);
    });
    assert!(matches!(err, LedgerError::Integrity { .. }), "{err:?}");
    assert!(err.to_string().contains("seq out of order"), "{err}");
}
