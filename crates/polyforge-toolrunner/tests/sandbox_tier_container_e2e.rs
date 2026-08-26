//! T10: container tier end-to-end - selection, dispatch, and the tier-
//! prefixed executor identity in the persisted attestation payload.
//!
//! Own test process (pins the process-global tier). Skip-clean like the T6
//! mechanics tests: without a container runtime or a locally present image
//! the test prints `[SKIP]` and passes, so plain hosts and CI stay green
//! while an operator-provisioned host gets the full proof.

#![cfg(feature = "sandbox-container")]

use std::process::{Command, Stdio};

use polyforge_core::evidence::{EvidenceEntry, EvidenceKind, EvidenceState};
use polyforge_core::ledger::Ledger;
use polyforge_toolrunner::prober::{select_tier, ProbeSource, ProdProbe};
use polyforge_toolrunner::{
    init_executor_with_backend, lookup, verify_and_append, ExecutorKind, SandboxTier,
};

fn runtime() -> Option<String> {
    if let Ok(rt) = std::env::var("POLYFORGE_SANDBOX_RUNTIME") {
        if !rt.trim().is_empty() {
            return Some(rt.trim().to_string());
        }
    }
    ProdProbe.container_runtime()
}

fn image_present(runtime: &str, image: &str) -> bool {
    Command::new(runtime)
        .args(["image", "inspect", image])
        .stdin(Stdio::null())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn container_tier_attestation_metadata_carries_tier_prefix() {
    let Some(rt) = runtime() else {
        println!("[SKIP] reason: no container runtime (docker/podman) on this host");
        return;
    };
    let image = std::env::var("POLYFORGE_SANDBOX_IMAGE").unwrap_or_default();
    let image = if image.trim().is_empty() {
        "polyforge-sandbox:latest".to_string()
    } else {
        image
    };
    if !image_present(&rt, &image) {
        println!(
            "[SKIP] reason: image {image} not present locally (build or pull it, \
             or point POLYFORGE_SANDBOX_IMAGE at a local image with the toolchain)"
        );
        return;
    }

    init_executor_with_backend(ExecutorKind::Sandbox, Some(SandboxTier::Container))
        .expect("container tier selected");

    let ledger_path = std::env::temp_dir().join(format!(
        "pf-t10-container-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let mut ledger = Ledger::new(&ledger_path);
    let claim_id = ledger
        .append(EvidenceEntry::new_claim("T10", "abc123", "diff-1", "ts-1").to_ledger_entry())
        .expect("claim appended");

    let verified = verify_and_append(
        &mut ledger,
        "T10",
        claim_id,
        &lookup("cargo --version").unwrap(),
        &[],
    )
    .expect("container-backed verification");

    assert_eq!(verified.kind, EvidenceKind::ToolAttestation);
    assert_eq!(verified.state, EvidenceState::Verified);
    let meta = verified
        .eval_metadata
        .as_ref()
        .expect("sandbox attestation must carry executor metadata");
    let digest = meta["executor_digest"].as_str().expect("digest string");
    assert!(
        digest.starts_with("container:") && digest["container:".len()..].len() == 64,
        "executor identity must carry the container tier prefix over the \
         divergent per-tier digest formula, got {digest}"
    );

    let entries = ledger.iter_entries().unwrap();
    assert_eq!(
        entries[1].payload["eval_metadata"]["executor_digest"],
        digest
    );
    ledger.verify_chain().unwrap();
    let _ = std::fs::remove_file(&ledger_path);
}
