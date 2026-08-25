//! Canonical JSON writer plus in-toto Statement v1 and DSSE envelope types.
//!
//! Serialization is canonical from day one: object keys are sorted and
//! separators are compact, so identical input always produces identical bytes.
//! Golden fixtures under `tests/fixtures/` pin this behavior byte-for-byte.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub mod canon;
pub mod emit;

pub use canon::canonical_json;
pub use emit::{
    emit_chain_statement, emit_task_statement, read_ledger, sha256_hex, Entry, LedgerError,
    POLYFORGE_EVIDENCE_PREDICATE_V1,
};

/// `_type` value of an in-toto attestation statement, version 1.
pub const IN_TOTO_STATEMENT_V1: &str = "https://in-toto.io/Statement/v1";

/// DSSE `payloadType` for in-toto JSON payloads.
pub const DSSE_PAYLOAD_TYPE_IN_TOTO_JSON: &str = "application/vnd.in-toto+json";

/// One attested software artifact: a name plus a map of algorithm to hex digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    /// Artifact identity, for example `polyforge/task/<task_id>@<commit12>`.
    pub name: String,
    /// Digest set keyed by algorithm, for example `sha256` to a 64-char hex string.
    pub digest: BTreeMap<String, String>,
}

impl Subject {
    /// Builds a subject carrying a single `sha256` digest.
    pub fn new(name: impl Into<String>, sha256_hex: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            digest: BTreeMap::from([("sha256".to_owned(), sha256_hex.into())]),
        }
    }
}

/// An in-toto attestation statement, version 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Statement {
    /// Always [`IN_TOTO_STATEMENT_V1`]; serialized under the `_type` key.
    pub _type: String,
    /// Artifacts this statement is about.
    pub subject: Vec<Subject>,
    /// Predicate type URI, for example `https://polyforge.dev/attestations/evidence/v1`.
    #[serde(rename = "predicateType")]
    pub predicate_type: String,
    /// Opaque predicate payload.
    pub predicate: serde_json::Value,
}

impl Statement {
    /// Builds a statement with the constant `_type` pinned to [`IN_TOTO_STATEMENT_V1`].
    pub fn new(
        subject: Vec<Subject>,
        predicate_type: impl Into<String>,
        predicate: serde_json::Value,
    ) -> Self {
        Self {
            _type: IN_TOTO_STATEMENT_V1.to_owned(),
            subject,
            predicate_type: predicate_type.into(),
            predicate,
        }
    }
}

/// One DSSE signature: the signer key id and the signature bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// Identifier of the signing key.
    pub keyid: String,
    /// Signature bytes, encoded per the surrounding transport convention.
    pub sig: String,
}

/// A DSSE envelope wrapping a base64 payload with its signatures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DsseEnvelope {
    /// Always [`DSSE_PAYLOAD_TYPE_IN_TOTO_JSON`] for statements produced here.
    #[serde(rename = "payloadType")]
    pub payload_type: String,
    /// Base64-encoded payload bytes.
    pub payload: String,
    /// Signatures over the payload.
    pub signatures: Vec<Signature>,
}

impl DsseEnvelope {
    /// Builds an envelope with the constant `payloadType` pinned to
    /// [`DSSE_PAYLOAD_TYPE_IN_TOTO_JSON`].
    pub fn new(payload_b64: impl Into<String>, signatures: Vec<Signature>) -> Self {
        Self {
            payload_type: DSSE_PAYLOAD_TYPE_IN_TOTO_JSON.to_owned(),
            payload: payload_b64.into(),
            signatures,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_spec() {
        assert_eq!(IN_TOTO_STATEMENT_V1, "https://in-toto.io/Statement/v1");
        assert_eq!(
            DSSE_PAYLOAD_TYPE_IN_TOTO_JSON,
            "application/vnd.in-toto+json"
        );
    }

    #[test]
    fn statement_new_pins_constant_type() {
        let s = Statement::new(Vec::new(), "https://example.test", serde_json::json!({}));
        assert_eq!(s._type, IN_TOTO_STATEMENT_V1);
    }

    #[test]
    fn envelope_new_pins_constant_payload_type() {
        let e = DsseEnvelope::new("aGk=", Vec::new());
        assert_eq!(e.payload_type, DSSE_PAYLOAD_TYPE_IN_TOTO_JSON);
    }

    #[test]
    fn subject_new_fills_sha256_digest() {
        let s = Subject::new("polyforge/task/x@abc", "deadbeef");
        assert_eq!(s.digest.get("sha256").map(String::as_str), Some("deadbeef"));
    }

    #[test]
    fn wire_field_names_use_in_toto_casing() {
        let s = Statement::new(
            vec![Subject::new("n", "d")],
            "pt",
            serde_json::json!({ "z": 1 }),
        );
        let v = serde_json::to_value(&s).expect("statement serializes");
        assert!(
            v.get("_type").is_some(),
            "_type key must survive serde rename"
        );
        assert!(v.get("predicateType").is_some());
        assert!(v.get("predicate").is_some());
        assert!(v.get("subject").is_some());

        let e = DsseEnvelope::new(
            "aGk=",
            vec![Signature {
                keyid: "k".into(),
                sig: "s".into(),
            }],
        );
        let v = serde_json::to_value(&e).expect("envelope serializes");
        assert!(v.get("payloadType").is_some());
        assert!(v.get("payload").is_some());
        assert!(v.get("signatures").is_some());
    }
}
