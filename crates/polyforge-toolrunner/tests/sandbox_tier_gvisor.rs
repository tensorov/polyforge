//! T10: gVisor tier row of the acceptance matrix, in its OWN test process
//! (the selection may pin the process-global tier).
//!
//! Host-consistent by construction: the expected outcome is computed from the
//! T5 prober itself, so the assertions hold on gVisor-capable machines and on
//! plain hosts alike. On a capable host this also proves the recorded tier.

#![cfg(feature = "sandbox-gvisor")]

use polyforge_toolrunner::prober::{select_tier, ProdProbe};
use polyforge_toolrunner::{
    init_executor_with_backend, selected_sandbox_tier, ExecutorKind, SandboxTier,
};

#[test]
fn explicit_gvisor_matches_prober_verdict() {
    let requested = Some(SandboxTier::Gvisor);
    match select_tier(Some(SandboxTier::Gvisor), &ProdProbe) {
        Ok(_) => {
            // Capable host: the runsc prerequisite holds, but the executor
            // still needs a digest-pinned image; without one the fail-closed
            // config validation must name the env var instead of proceeding.
            match std::env::var("POLYFORGE_SANDBOX_IMAGE") {
                Ok(img) if !img.trim().is_empty() => {
                    init_executor_with_backend(ExecutorKind::Sandbox, requested)
                        .expect("gVisor selection with runtime and pinned image");
                    assert_eq!(selected_sandbox_tier(), Some(SandboxTier::Gvisor));
                }
                _ => {
                    let err = init_executor_with_backend(ExecutorKind::Sandbox, requested)
                        .expect_err("no digest-pinned image configured");
                    assert!(
                        err.contains("POLYFORGE_SANDBOX_IMAGE") && err.contains("digest"),
                        "error must name the fix: {err}"
                    );
                }
            }
        }
        Err(e) => {
            let err = init_executor_with_backend(ExecutorKind::Sandbox, requested)
                .expect_err("prober says gVisor is unavailable");
            assert_eq!(
                err,
                e.to_string(),
                "error must name the missing runsc prerequisite"
            );
        }
    }
}
