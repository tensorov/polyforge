//! Golden fixture tests: decode committed canonical JSON into typed structs and
//! re-serialize through the canonical writer. The re-serialized bytes must match
//! the fixture bytes exactly, so T4 emitters can rely on byte-stable output.

use std::fs;

use polyforge_attest::{canonical_json, DsseEnvelope, Statement};

const STATEMENT_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/statement.json"
);
const ENVELOPE_FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/envelope.json");

fn read_fixture(path: &str) -> String {
    let raw = fs::read_to_string(path).expect("fixture must exist");
    assert!(
        !raw.starts_with('\n') && !raw.ends_with('\n'),
        "golden fixtures are single-line canonical JSON without surrounding whitespace"
    );
    raw
}

#[test]
fn statement_fixture_roundtrips_byte_for_byte() {
    let raw = read_fixture(STATEMENT_FIXTURE);
    let statement: Statement = serde_json::from_str(&raw).expect("fixture decodes into Statement");
    let value = serde_json::to_value(&statement).expect("Statement serializes to Value");
    let canonical = canonical_json(&value);
    assert_eq!(canonical, raw, "re-serialized statement must equal golden bytes");
}

#[test]
fn envelope_fixture_roundtrips_byte_for_byte() {
    let raw = read_fixture(ENVELOPE_FIXTURE);
    let envelope: DsseEnvelope = serde_json::from_str(&raw).expect("fixture decodes into DsseEnvelope");
    let value = serde_json::to_value(&envelope).expect("DsseEnvelope serializes to Value");
    let canonical = canonical_json(&value);
    assert_eq!(canonical, raw, "re-serialized envelope must equal golden bytes");
}

#[test]
fn mutated_statement_fixture_is_detected() {
    let raw = read_fixture(STATEMENT_FIXTURE);
    let marker = "Statement/v1";
    let pos = raw.find(marker).expect("marker present in golden statement");
    let offset = pos + "Statement/".len();
    assert_eq!(raw.as_bytes()[offset], b'v', "mutation target must actually change");

    let mut bytes = raw.clone().into_bytes();
    bytes[offset] = b'w';
    let mutated = String::from_utf8(bytes).expect("mutation stays utf8");

    let parsed: serde_json::Value =
        serde_json::from_str(&mutated).expect("mutated fixture still parses as JSON");
    let canonical = canonical_json(&parsed);
    assert_ne!(
        canonical, raw,
        "single-byte mutation must be caught by the canonical round-trip"
    );
}

#[test]
fn mutated_envelope_fixture_is_detected() {
    let raw = read_fixture(ENVELOPE_FIXTURE);
    let marker = "pf-test-key-01";
    let pos = raw.find(marker).expect("marker present in golden envelope");
    assert_eq!(raw.as_bytes()[pos], b'p', "mutation target must actually change");

    let mut bytes = raw.clone().into_bytes();
    bytes[pos] = b'q';
    let mutated = String::from_utf8(bytes).expect("mutation stays utf8");

    let parsed: serde_json::Value =
        serde_json::from_str(&mutated).expect("mutated fixture still parses as JSON");
    let canonical = canonical_json(&parsed);
    assert_ne!(
        canonical, raw,
        "single-byte mutation must be caught by the canonical round-trip"
    );
}
