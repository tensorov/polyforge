#![cfg(feature = "sandbox-mock")]
//! T2 kill-tests: the Sandbox selection end-to-end through the PUBLIC API.
//!
//! Dedicated test PROCESS: `init_executor(Sandbox)` permanently pins the
//! set-once `EXECUTOR_KIND` global, and every assertion below depends on
//! that selection being live (public `run()` dispatch and the
//! `verify_and_append` metadata choke point both read it).
//!
//! Mutants killed here:
//! - sandbox_mock.rs `run` -> default/zeroed output: every RunOutput field
//!   is pinned non-default against a real child capture.
//! - sandbox_mock.rs `fresh_workdir` -> empty path: the run fails closed if
//!   no fresh directory can be created, and cleanup is proven by scanning.
//! - runner.rs delete `KIND_SANDBOX` arm in `selected_executor_kind`: with
//!   the arm gone the selection reads as Process, so the public dispatch and
//!   the attestation metadata key both disappear.
//! - runner.rs `executor_digest_for_kind` -> None: same metadata key
//!   disappears from the persisted payload.
//! - sandbox_mock.rs `executor_digest` value mutants: the payload digest is
//!   compared to an independently computed literal.

use std::path::PathBuf;

use polyforge_core::evidence::{EvidenceEntry, EvidenceKind, EvidenceState};
use polyforge_core::ledger::Ledger;
#[cfg(feature = "sandbox-container")]
use polyforge_toolrunner::RunnerError;
use polyforge_toolrunner::{
    executor_digest, init_executor, lookup, run, verify_and_append, ExecutorKind, MOCK_IMAGE_ID,
};

/// sha256("mock-sandbox-image-v1")[..16], computed outside the crate.
const EXPECTED_DIGEST: &str = "7ae02d70fb50d824";

/// Workdirs this test process owns. The name embeds the creating pid, so
/// filtering by our own pid keeps the scan immune to concurrent mock runs
/// in other test binaries.
fn own_workdirs() -> Vec<PathBuf> {
    let prefix = format!("pf-sandbox-mock-{}-", std::process::id());
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                found.push(entry.path());
            }
        }
    }
    found.sort();
    found
}

/// The configured sandbox image ref, mirroring the container backend's own
/// resolution (`POLYFORGE_SANDBOX_IMAGE` or the documented default).
#[cfg(feature = "sandbox-container")]
fn configured_image_ref() -> String {
    match std::env::var(polyforge_toolrunner::POLYFORGE_SANDBOX_IMAGE_ENV) {
        Ok(img) if !img.trim().is_empty() => img.trim().to_string(),
        _ => polyforge_toolrunner::DEFAULT_SANDBOX_IMAGE.to_string(),
    }
}

#[test]
fn sandbox_selection_is_idempotent() {
    init_executor(ExecutorKind::Sandbox).expect("first sandbox selection");
    init_executor(ExecutorKind::Sandbox).expect("repeat of the same kind stays idempotent");
}

#[test]
fn sandbox_pipeline_runs_end_to_end_through_public_api() {
    init_executor(ExecutorKind::Sandbox).expect("sandbox selection");

    assert_eq!(executor_digest(), EXPECTED_DIGEST);
    assert_eq!(MOCK_IMAGE_ID, "mock-sandbox-image-v1");

    let tool = lookup("cargo --version").expect("tool on allowlist");

    // A legacy Sandbox selection routes to the REAL container backend when
    // that feature is compiled in and the host probes a Container tier
    // (same check the runner's container_backend_active caches); the
    // mock-specific pins below apply only to the mock path.
    #[cfg(feature = "sandbox-container")]
    {
        use polyforge_toolrunner::prober::{select_tier, ProdProbe};
        if matches!(
            select_tier(None, &ProdProbe),
            Ok(polyforge_toolrunner::SandboxTier::Container)
        ) {
            let image_ref = configured_image_ref();
            match run(&tool, &[]) {
                Ok(_) => {
                    println!(
                        "[SKIP] reason: container backend active with a resolvable image; \
                         container-path pins live in sandbox_tier_container_e2e.rs"
                    );
                    return;
                }
                Err(RunnerError::Spawn(msg)) => {
                    assert!(
                        msg.contains(&image_ref) && msg.contains("digest"),
                        "fail-closed run must name the unresolvable image and the digest \
                         requirement, got: {msg}"
                    );
                    return;
                }
                Err(e) => panic!("unexpected error from the container backend: {e:?}"),
            }
        }
    }

    let out = run(&tool, &[]).expect("public run must dispatch to the selected backend");

    assert_eq!(out.exit_code, 0);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("cargo"), "real stdout captured: {stdout:?}");
    assert!(!stdout.trim().is_empty());
    assert_eq!(out.stdout_hash.len(), 64);
    assert!(!out.tool_version.is_empty());
    assert!(!out.env_fingerprint.is_empty());
    assert!(out.command.starts_with("cargo"));

    let after = own_workdirs();
    assert!(
        after.is_empty(),
        "fresh workdir must be created for the run and cleaned afterwards; leftovers: {after:?}"
    );

    let ledger_path = std::env::temp_dir().join(format!(
        "pf-t2-pipeline-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let mut ledger = Ledger::new(&ledger_path);
    let claim_id = ledger
        .append(EvidenceEntry::new_claim("T2", "abc123", "diff-1", "ts-1").to_ledger_entry())
        .expect("claim appended");

    let verified = verify_and_append(
        &mut ledger,
        "T2",
        claim_id,
        &lookup("cargo --version").unwrap(),
        &[],
    )
    .expect("sandbox-backed verification");

    assert_eq!(verified.kind, EvidenceKind::ToolAttestation);
    assert_eq!(verified.state, EvidenceState::Verified);
    let meta = verified
        .eval_metadata
        .as_ref()
        .expect("sandbox attestation must carry executor metadata");
    assert_eq!(
        meta["executor_digest"].as_str(),
        Some(EXPECTED_DIGEST),
        "attested digest must be the exact backend identity"
    );

    let entries = ledger.iter_entries().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].kind, "ToolAttestation");
    assert_eq!(
        entries[1].payload["eval_metadata"]["executor_digest"], EXPECTED_DIGEST,
        "persisted payload must carry the executor identity"
    );
    ledger.verify_chain().unwrap();
    let _ = std::fs::remove_file(&ledger_path);
}
