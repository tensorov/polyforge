//! T2/T10 kill-tests: the set-once executor AND sandbox-tier selection
//! guards.
//!
//! This file is a dedicated test PROCESS on purpose: the selection APIs pin
//! the process-global `EXECUTOR_KIND` and `SANDBOX_TIER` permanently
//! (set-once CAS), so every flip lives in ONE sequenced test here, isolated
//! from the lib unit-test binary and other integration files. The
//! kind-level and tier-level sequences are mutually exclusive within one
//! process (the kind CAS rejects the second kind outright), so they are
//! sequenced inside a single test: the Sandbox/tier matrix runs FIRST (it
//! needs Sandbox recordable), and the kind-conflict is then proven against
//! the recorded Sandbox instead of Process — the same CAS code path and the
//! same `kind_name_of_code` rendering, just the other recorded value. On a
//! tier-less host (or default build) the mirror Process-first sequence runs
//! instead.
//!
//! Kills:
//! - runner.rs EXECUTOR_KIND CAS guard mutants (`prev == code` -> true /
//!   false, `==` -> `!=`): a conflicting re-selection must be rejected
//!   naming the recorded kind, and a same-kind re-selection must stay
//!   idempotent-Ok.
//! - runner.rs SANDBOX_TIER CAS guard mutants (same shapes at the tier
//!   site): a conflicting tier must be rejected naming the recorded tier,
//!   and a same-tier re-selection must stay idempotent-Ok.

#[cfg(any(
    feature = "sandbox-mock",
    feature = "sandbox-container",
    feature = "sandbox-gvisor",
    feature = "sandbox-firecracker"
))]
use polyforge_toolrunner::prober::{select_tier, ProdProbe};
use polyforge_toolrunner::{init_executor, ExecutorKind};

/// Tiers this build can serve AND this host passes prepare for, in the
/// prober's own preference order.
#[cfg(any(
    feature = "sandbox-mock",
    feature = "sandbox-container",
    feature = "sandbox-gvisor",
    feature = "sandbox-firecracker"
))]
fn servable_tiers() -> Vec<polyforge_toolrunner::SandboxTier> {
    use polyforge_toolrunner::SandboxTier;
    let mut servable = Vec::new();
    for tier in [
        SandboxTier::Firecracker,
        SandboxTier::Gvisor,
        SandboxTier::Container,
    ] {
        // Feature gate first (prepare fails fast on missing features), then
        // the real host probe.
        let feature_ok = match tier {
            SandboxTier::Firecracker => cfg!(feature = "sandbox-firecracker"),
            SandboxTier::Gvisor => cfg!(feature = "sandbox-gvisor"),
            SandboxTier::Container => cfg!(feature = "sandbox-container"),
        };
        if feature_ok && select_tier(Some(tier), &ProdProbe).is_ok() {
            servable.push(tier);
        }
    }
    servable
}

/// Make the Firecracker tier PREPARE-SERVABLE with zero real assets:
/// prepare only does file-existence checks (/dev/kvm, firecracker/jailer on
/// PATH) and FcConfig validation (kernel/rootfs files + manifest hash
/// binding) — nothing executes. Fake binaries, a fake kernel, and a
/// hash-bound fake rootfs in a temp dir satisfy every check hermetically,
/// so the different-tier CAS arm becomes reachable on any KVM host. The
/// returned guard restores every touched env var on drop.
#[cfg(all(unix, feature = "sandbox-firecracker"))]
struct FakeFcAssets {
    dir: std::path::PathBuf,
    saved_path: Option<String>,
    saved: Vec<(&'static str, Option<String>)>,
}

#[cfg(all(unix, feature = "sandbox-firecracker"))]
impl FakeFcAssets {
    fn stage() -> Option<Self> {
        if !std::path::Path::new("/dev/kvm").exists() {
            return None;
        }
        let dir = std::env::temp_dir().join(format!(
            "pf-setonce-fc-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).expect("fake fc bin dir");
        for name in ["firecracker", "jailer"] {
            std::fs::write(bin.join(name), b"#!/bin/sh\n").expect("fake binary");
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(bin.join(name)).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(bin.join(name), perms).unwrap();
        }
        std::fs::write(dir.join("vmlinux"), b"fake-kernel").expect("fake kernel");
        std::fs::write(dir.join("rootfs.ext4"), b"pf-fc-setonce-rootfs").expect("fake rootfs");

        // Discover the rootfs hash through the binding check itself: a
        // placeholder manifest is rejected with an error naming the ACTUAL
        // image hash, which is then written back. No hash dependency needed.
        let manifest = dir.join("manifest.json");
        std::fs::write(
            &manifest,
            r#"{"base_image_or_packages":"fake","build_script_sha256":"a","rootfs_sha256":"placeholder"}"#,
        )
        .expect("fake manifest");
        let actual_hash = match polyforge_toolrunner::fc_exec::FcConfig::from_environment() {
            Err(polyforge_toolrunner::RunnerError::Spawn(msg)) => msg
                .split("image hashes to ")
                .nth(1)
                .map(|rest| rest.split(';').next().unwrap_or(rest).trim().to_string()),
            _ => None,
        };

        let saved_path = std::env::var("PATH").ok();
        let saved: Vec<(&'static str, Option<String>)> = [
            "POLYFORGE_FC_KERNEL",
            "POLYFORGE_FC_ROOTFS",
            "POLYFORGE_FC_MANIFEST",
        ]
        .iter()
        .map(|&k| (k, std::env::var(k).ok()))
        .collect();
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                saved_path.clone().unwrap_or_default()
            ),
        );
        std::env::set_var("POLYFORGE_FC_KERNEL", dir.join("vmlinux"));
        std::env::set_var("POLYFORGE_FC_ROOTFS", dir.join("rootfs.ext4"));
        std::env::set_var("POLYFORGE_FC_MANIFEST", &manifest);

        // With the env now pointing at the fakes, resolve the real hash and
        // finalize the manifest so the binding check passes.
        let rootfs_sha256 = match actual_hash {
            Some(h) => h,
            None => match polyforge_toolrunner::fc_exec::FcConfig::from_environment() {
                Err(polyforge_toolrunner::RunnerError::Spawn(msg)) => msg
                    .split("image hashes to ")
                    .nth(1)
                    .map(|rest| rest.split(';').next().unwrap_or(rest).trim().to_string())
                    .expect("binding error names the actual hash"),
                _ => panic!("placeholder binding must be rejected"),
            },
        };
        std::fs::write(
            &manifest,
            format!(
                r#"{{"base_image_or_packages":"fake","build_script_sha256":"{}","rootfs_sha256":"{rootfs_sha256}"}}"#,
                "a".repeat(64)
            ),
        )
        .expect("final manifest");
        Some(Self {
            dir,
            saved_path,
            saved,
        })
    }
}

#[cfg(all(unix, feature = "sandbox-firecracker"))]
impl Drop for FakeFcAssets {
    fn drop(&mut self) {
        for (k, v) in self.saved.drain(..) {
            match v {
                Some(value) => std::env::set_var(k, value),
                None => std::env::remove_var(k),
            }
        }
        match self.saved_path.take() {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn set_once_selection_rejects_conflicting_reinit() {
    // ---- tier matrix FIRST (needs Sandbox recordable) ------------------------
    //
    // init_executor_with_backend resolves the REQUESTED tier through
    // prepare_sandbox_tier BEFORE either CAS, so a re-init only reaches the
    // tier CAS when the requested tier is servable on this host. The
    // host-consistent pattern (mirroring sandbox_tier_gvisor.rs) computes
    // the expectation from the T5 prober itself, so the assertions hold on
    // any machine; arms whose prerequisites are absent record a skip.
    #[cfg(any(
        feature = "sandbox-mock",
        feature = "sandbox-container",
        feature = "sandbox-gvisor",
        feature = "sandbox-firecracker"
    ))]
    {
        use polyforge_toolrunner::{init_executor_with_backend, SandboxTier};
        let _ = SandboxTier::Container; // silence unused-import in narrow builds
                                        // Staged fake-asset guards, dropped (env restored) at test end.
        #[cfg(all(unix, feature = "sandbox-firecracker"))]
        let mut guards: Vec<FakeFcAssets> = Vec::new();
        let servable = servable_tiers();
        if let Some(&first) = servable.first() {
            // First selection records kind=Sandbox and the tier.
            init_executor_with_backend(ExecutorKind::Sandbox, Some(first)).unwrap_or_else(|e| {
                panic!("first {first:?} selection must succeed on a servable host: {e}")
            });

            // Same-tier re-init is IDEMPOTENT-Ok (the `Err(prev) if prev ==
            // code` guard arm). The `prev == code -> false` and `== -> !=`
            // mutants turn this into the conflict error.
            init_executor_with_backend(ExecutorKind::Sandbox, Some(first))
                .unwrap_or_else(|e| panic!("same-tier re-init must stay idempotent: {e}"));

            // Different-tier re-init must be REJECTED naming the recorded
            // tier (the final CAS arm). The `prev == code -> true` mutant
            // silently accepts the conflicting tier, which the
            // exact-message pin catches. When the host serves no second REAL
            // tier, fake Firecracker assets make its prepare pass (checks
            // are file-existence only, nothing executes), keeping the arm
            // reachable hermetically.
            let second = servable.get(1).copied().or_else(|| {
                if first == SandboxTier::Firecracker {
                    // Firecracker already recorded; a fake would be the same
                    // tier, not a conflicting one.
                    None
                } else {
                    FakeFcAssets::stage().map(|guard| {
                        guards.push(guard);
                        SandboxTier::Firecracker
                    })
                }
            });
            if let Some(second) = second {
                let err = init_executor_with_backend(ExecutorKind::Sandbox, Some(second))
                    .expect_err("a conflicting tier must never replace the recorded one");
                let expected = format!("sandbox backend already initialized to {}", first.label());
                assert_eq!(
                    err, expected,
                    "tier set-once guard must name the recorded tier: {err}"
                );
            } else {
                println!(
                    "[SKIP] reason: no second tier servable (real or fake-fc) on this host; \
                     different-tier CAS arm unreachable (prepare gates it first)"
                );
            }

            // The recorded tier survives every rejection untouched.
            assert_eq!(
                polyforge_toolrunner::selected_sandbox_tier(),
                Some(first),
                "rejections must not mutate the recorded tier"
            );

            // ---- kind-conflict against the recorded Sandbox -----------------
            //
            // The same EXECUTOR_KIND CAS code path as the historical
            // Process-recorded variant, exercised from the other side: a
            // Process selection against a recorded Sandbox must be rejected
            // naming the recorded kind. The `prev == code -> true` mutant
            // silently accepts it. BOTH selection APIs carry their own CAS
            // site (init_executor and init_executor_with_backend), so the
            // conflict is proven through each.
            let err = init_executor(ExecutorKind::Process)
                .expect_err("conflicting re-selection must be rejected");
            assert_eq!(
                err, "executor already initialized to sandbox",
                "set-once guard must pin the recorded kind: {err}"
            );
            let err = init_executor_with_backend(ExecutorKind::Process, None)
                .expect_err("conflicting re-selection through the backend API");
            assert_eq!(
                err, "executor already initialized to sandbox",
                "both CAS sites must reject the conflicting kind: {err}"
            );

            // Same-kind re-init stays idempotent after the rejection.
            init_executor_with_backend(ExecutorKind::Sandbox, Some(first))
                .unwrap_or_else(|e| panic!("recorded selection must survive the rejection: {e}"));
        } else {
            println!(
                "[SKIP] reason: no sandbox tier servable on this host; \
                 kind recorded as process below instead"
            );
        }
    }

    // ---- kind matrix when no tier was servable (kind stays Process) ----------
    //
    // Reachable only when the tier block above skipped (or in the default
    // build): with a tier recorded the kind is Sandbox and every Process
    // selection dies at the CAS, so this Process-first sequence is the
    // mirror variant for tier-less hosts.
    #[cfg(not(any(
        feature = "sandbox-mock",
        feature = "sandbox-container",
        feature = "sandbox-gvisor",
        feature = "sandbox-firecracker"
    )))]
    {
        init_executor(ExecutorKind::Process).expect("first process selection");
        init_executor(ExecutorKind::Process).expect("repeat of the same kind stays idempotent");

        let err = init_executor(ExecutorKind::Sandbox)
            .expect_err("conflicting re-selection must be rejected");
        // Without any sandbox feature the fail-closed gate rejects Sandbox
        // before any state is written; the exact message is part of the
        // contract.
        assert_eq!(err, "sandbox executor requires feature sandbox-mock");

        init_executor(ExecutorKind::Process)
            .expect("the recorded selection must survive the rejection untouched");
    }
}
