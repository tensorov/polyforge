//! T10 kill-tests: sandbox tier selection through the PUBLIC API.
//!
//! Dedicated test PROCESS: `init_executor_with_backend` pins two process
//! globals (`EXECUTOR_KIND`, `SANDBOX_TIER`) permanently, so every
//! selection-flipping assertion lives in ONE sequenced test inside this own
//! binary, isolated from the lib unit tests and other integration files.
//!
//! Matrix rows covered here (host-consistent rows are pinned exactly; host-
//! dependent rows assert consistency with the T5 prober so they hold on any
//! machine):
//! - firecracker request fails closed BEFORE any state write, naming the
//!   actual missing prerequisite (/dev/kvm, binaries) or the pending backend.
//! - auto honors the prober and records the resolved tier.
//! - explicit container is honored when available and never silently
//!   replaced by a conflicting re-selection.
//! - a tier request on the process executor is rejected at the contract.

#![cfg(feature = "sandbox-container")]

use polyforge_toolrunner::prober::{select_tier, ProdProbe};
use polyforge_toolrunner::{
    init_executor, init_executor_with_backend, selected_sandbox_tier, ExecutorKind, SandboxTier,
};

#[test]
fn sequenced_t10_selection_matrix() {
    // (1) Firecracker: rejected BEFORE any state write, naming the actual
    // prerequisite (/dev/kvm, binaries) or the pending backend; Process
    // stays selectable afterwards, proving nothing was recorded.
    let expected = match select_tier(Some(SandboxTier::Firecracker), &ProdProbe) {
        Err(e) => e.to_string(),
        Ok(_) => "firecracker backend pending T9: microVM executor not implemented".to_string(),
    };
    let err = init_executor_with_backend(ExecutorKind::Sandbox, Some(SandboxTier::Firecracker))
        .expect_err("the firecracker backend does not exist yet");
    assert_eq!(err, expected, "error must name the actual prerequisite");
    assert_eq!(
        selected_sandbox_tier(),
        None,
        "a rejected tier request must leave no recorded tier"
    );

    // (2) A tier request on the process executor is rejected at the
    // contract level.
    let err = init_executor_with_backend(ExecutorKind::Process, Some(SandboxTier::Container))
        .expect_err("a tier only makes sense with the sandbox executor");
    assert_eq!(err, "--sandbox-backend requires --executor sandbox");

    // (3) Auto: whatever the prober answers must be exactly what happens -
    // the resolved tier is recorded when this build can serve it, otherwise
    // the selection fails closed naming the missing backend feature.
    match select_tier(None, &ProdProbe) {
        Ok(tier) => {
            let served = tier == SandboxTier::Container
                || (tier == SandboxTier::Gvisor && cfg!(feature = "sandbox-gvisor"));
            if served {
                init_executor_with_backend(ExecutorKind::Sandbox, None)
                    .expect("auto selection succeeds when a servable tier exists");
                assert_eq!(selected_sandbox_tier(), Some(tier));

                // (4) Repeat auto stays idempotent.
                init_executor_with_backend(ExecutorKind::Sandbox, None)
                    .expect("idempotent re-selection");

                // (5) Explicit same tier is idempotent; a conflicting tier
                // never replaces the first recorded selection.
                if tier == SandboxTier::Container {
                    init_executor_with_backend(ExecutorKind::Sandbox, Some(SandboxTier::Container))
                        .expect("same-tier re-selection is idempotent");
                    let _ = init_executor_with_backend(
                        ExecutorKind::Sandbox,
                        Some(SandboxTier::Gvisor),
                    );
                    assert_eq!(
                        selected_sandbox_tier(),
                        Some(SandboxTier::Container),
                        "a conflicting re-selection must never replace the first tier"
                    );
                }
            } else {
                let err = init_executor_with_backend(ExecutorKind::Sandbox, None)
                    .expect_err("auto-resolved tier without a compiled backend must fail closed");
                assert!(
                    err.contains("requires feature") || err.contains("pending"),
                    "error must name the missing backend: {err}"
                );
            }
        }
        Err(e) => {
            let err = init_executor_with_backend(ExecutorKind::Sandbox, None)
                .expect_err("no tier on this host");
            assert_eq!(err, e.to_string());
        }
    }
}
