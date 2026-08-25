//! Integration tests for the ledger reader and statement emitters.

use std::path::PathBuf;

use polyforge_attest::{
    canonical_json, emit_chain_statement, emit_task_statement, read_ledger, sha256_hex, Entry,
    LedgerError, POLYFORGE_EVIDENCE_PREDICATE_V1,
};
use serde_json::{json, Value};

const FIXTURE_LEDGER: &str = "tests/fixtures/ledger.jsonl";

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
    }
}

#[test]
fn empty_task_id_is_rejected() {
    let err = emit_task_statement(&[], "", None).expect_err("empty task id must fail");
    assert!(matches!(err, LedgerError::EmptyTaskId));
}

#[test]
fn missing_bundle_path_falls_back_to_subchain_digest() {
    let entries = read_ledger(&fixture_path()).expect("fixture parses");
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
fn unicode_task_id_roundtrip() {
    let task_id = "задача-🎉-42";
    let entries = vec![synthetic_entry(
        0,
        "",
        "ModelClaim",
        json!({"task_id": task_id, "commit_sha": "abcdef1234567890", "state": "ModelClaimed"}),
        "h0",
    )];
    let statement = emit_task_statement(&entries, task_id, None).expect("unicode emits");
    assert_eq!(
        statement.subject[0].name,
        format!("polyforge/task/{task_id}@abcdef123456")
    );

    // Roundtrip through JSON keeps the unicode task id intact.
    let text = canonical_json(&serde_json::to_value(&statement).expect("serializes"));
    let back: Value = serde_json::from_str(&text).expect("re-parses");
    assert!(back["subject"][0]["name"]
        .as_str()
        .expect("name is a string")
        .contains(task_id));
}

#[test]
fn two_consecutive_emits_are_byte_equal() {
    let entries = read_ledger(&fixture_path()).expect("fixture parses");
    let first = emit_task_statement(&entries, "bootstrap", None).expect("emits");
    let second = emit_task_statement(&entries, "bootstrap", None).expect("emits");

    let a = canonical_json(&serde_json::to_value(&first).expect("serializes")).into_bytes();
    let b = canonical_json(&serde_json::to_value(&second).expect("serializes")).into_bytes();
    assert_eq!(a, b, "identical input must produce identical bytes");

    let chain_a =
        canonical_json(&serde_json::to_value(emit_chain_statement(&entries)).expect("serializes"))
            .into_bytes();
    let chain_b =
        canonical_json(&serde_json::to_value(emit_chain_statement(&entries)).expect("serializes"))
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
    let entries = read_ledger(&fixture_path()).expect("real ledger copy parses");
    assert_eq!(entries.len(), 3);

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

    let chain = emit_chain_statement(&entries);
    let cv = serde_json::to_value(&chain).expect("serializes");
    assert_eq!(
        cv["predicate"]["tail"], "2a9805b4a337cf2228c7b760a350ac5c5115f8f3d2f8842a0c02b5e9696d24ac",
        "tail is the hash field of the last entry"
    );
    assert_eq!(cv["predicate"]["seq_count"], 3);
    assert!(cv["predicate"]["anchor_sidecar_hash"].is_null());
    assert_eq!(
        chain.subject[0].digest.get("sha256").map(String::as_str),
        Some("2a9805b4a337cf2228c7b760a350ac5c5115f8f3d2f8842a0c02b5e9696d24ac")
    );
}
