//! T10 acceptance matrix: `--sandbox-backend` end-to-end through the REAL
//! binary. Usage errors must exit 2 pre-spawn on every build; honored-tier
//! rows are feature-gated because the default build compiles no sandbox
//! backend (and must keep rejecting the selection with the legacy message).

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir =
        std::env::temp_dir().join(format!("pf-t10-cli-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir created");
    dir
}

/// Spawn the real CLI with explicit PF_LEDGER/PF_EVIDENCE_DIR under `dir`.
fn pf(dir: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_polyforge-cli"))
        .args(args)
        .current_dir(dir)
        .env("PF_LEDGER", dir.join("ledger.jsonl"))
        .env("PF_EVIDENCE_DIR", dir.join("evidence"))
        .output()
        .expect("failed to spawn polyforge-cli binary")
}

fn exit_code(out: &Output) -> i32 {
    out.status.code().expect("no exit code")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn bogus_backend_value_is_usage_error_exit2() {
    let dir = temp_dir("bogus");
    let out = pf(&dir, &["--sandbox-backend", "bogus", "ledger", "tail"]);
    assert_eq!(exit_code(&out), 2, "stderr: {}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("unknown sandbox backend: bogus"), "{err}");
    assert!(err.contains("usage: pf"), "{err}");
}

#[test]
fn missing_backend_value_is_usage_error_exit2() {
    let dir = temp_dir("missing-value");
    let out = pf(&dir, &["--sandbox-backend"]);
    assert_eq!(exit_code(&out), 2);
    assert!(stderr(&out).contains("usage: pf"));
}

#[test]
fn backend_without_sandbox_executor_is_rejected_exit2() {
    let dir = temp_dir("needs-executor");
    let out = pf(
        &dir,
        &[
            "--sandbox-backend",
            "container",
            "append",
            "model_claim",
            "x",
            "--task",
            "t",
        ],
    );
    assert_eq!(exit_code(&out), 2, "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("--sandbox-backend requires --executor sandbox"));
}

/// Firecracker row: only meaningful once SOME sandbox backend feature is
/// compiled (otherwise the kind-level gate rejects first, which the
/// default-build module pins separately). Fails closed naming the actual
/// missing prerequisite (/dev/kvm, binaries, backend feature, env assets).
#[cfg(any(
    feature = "sandbox-mock",
    feature = "sandbox-container",
    feature = "sandbox-gvisor",
    feature = "sandbox-firecracker"
))]
#[test]
fn firecracker_request_fails_closed_naming_prerequisite() {
    let dir = temp_dir("firecracker");
    let out = pf(
        &dir,
        &[
            "--executor",
            "sandbox",
            "--sandbox-backend",
            "firecracker",
            "append",
            "model_claim",
            "x",
            "--task",
            "t",
        ],
    );
    assert_eq!(exit_code(&out), 2, "stderr: {}", stderr(&out));
    let err = stderr(&out);
    // No-kvm hosts name /dev/kvm; kvm hosts without binaries name them;
    // builds without the backend feature name it; a capable build without
    // provisioned assets names the POLYFORGE_FC_* variables. All fail
    // closed before any spawn.
    let names_prerequisite = err.contains("/dev/kvm")
        || err.contains("binaries")
        || err.contains("requires feature")
        || err.contains("POLYFORGE_FC_");
    assert!(names_prerequisite, "unexpected error: {err}");
}

#[cfg(feature = "sandbox-container")]
mod with_container_backend {
    use super::*;

    use polyforge_toolrunner::prober::{ProbeSource, ProdProbe};

    fn container_available() -> bool {
        ProdProbe.container_runtime().is_some()
    }

    #[test]
    fn explicit_container_tier_is_honored_end_to_end() {
        if !container_available() {
            println!("[SKIP] reason: no container runtime on this host");
            return;
        }
        let dir = temp_dir("container-honored");
        let out = pf(
            &dir,
            &[
                "--executor",
                "sandbox",
                "--sandbox-backend",
                "container",
                "append",
                "model_claim",
                "claim datum",
                "--task",
                "t10",
                "--commit",
                "abc123",
                "--diff",
                "d1",
            ],
        );
        assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    }

    #[test]
    fn auto_tier_selects_per_host_probe_end_to_end() {
        if !container_available() {
            println!("[SKIP] reason: no container runtime on this host");
            return;
        }
        let dir = temp_dir("auto-honored");
        let out = pf(
            &dir,
            &[
                "--executor",
                "sandbox",
                "--sandbox-backend",
                "auto",
                "append",
                "model_claim",
                "claim datum",
                "--task",
                "t10-auto",
                "--commit",
                "abc123",
                "--diff",
                "d1",
            ],
        );
        assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    }
}

#[cfg(feature = "sandbox-gvisor")]
mod with_gvisor_backend {
    use super::*;
    use polyforge_toolrunner::prober::{select_tier, ProdProbe, SandboxTier};

    #[test]
    fn explicit_gvisor_tier_matches_prober_verdict() {
        match select_tier(Some(SandboxTier::Gvisor), &ProdProbe) {
            Ok(_) => match std::env::var("POLYFORGE_SANDBOX_IMAGE") {
                Ok(img) if !img.trim().is_empty() => {
                    let dir = temp_dir("gvisor-honored");
                    let out = pf(
                        &dir,
                        &[
                            "--executor",
                            "sandbox",
                            "--sandbox-backend",
                            "gvisor",
                            "append",
                            "model_claim",
                            "claim datum",
                            "--task",
                            "t10-gvisor",
                        ],
                    );
                    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
                }
                _ => println!(
                    "[SKIP] reason: runsc available but POLYFORGE_SANDBOX_IMAGE \
                     (digest-pinned) unset"
                ),
            },
            Err(e) => {
                let dir = temp_dir("gvisor-unavailable");
                let out = pf(
                    &dir,
                    &[
                        "--executor",
                        "sandbox",
                        "--sandbox-backend",
                        "gvisor",
                        "append",
                        "model_claim",
                        "x",
                        "--task",
                        "t",
                    ],
                );
                assert_eq!(exit_code(&out), 2);
                assert!(stderr(&out).contains(&e.to_string()), "{}", stderr(&out));
            }
        }
    }
}

#[cfg(not(any(
    feature = "sandbox-mock",
    feature = "sandbox-container",
    feature = "sandbox-gvisor"
)))]
mod default_build_legacy {
    use super::*;

    /// Mock stays the default when `--executor sandbox` is given WITHOUT a
    /// tier: in the default build that selection keeps failing with the exact
    /// legacy gate message (byte-identical behavior; the mock build itself is
    /// proven by the toolrunner T2 suite).
    #[test]
    fn sandbox_without_backend_keeps_legacy_gate_message() {
        let dir = temp_dir("legacy-gate");
        let out = pf(
            &dir,
            &[
                "--executor",
                "sandbox",
                "append",
                "model_claim",
                "x",
                "--task",
                "t",
            ],
        );
        assert_eq!(exit_code(&out), 2);
        assert!(stderr(&out).contains("sandbox executor requires feature sandbox-mock"));
    }

    /// An explicit tier in a build without that backend fails closed. With
    /// NO sandbox feature at all the kind-level gate fires first (legacy
    /// message); with some other backend compiled in — mock, gVisor, or
    /// firecracker — the kind gate passes and the error names the missing
    /// container feature instead.
    #[test]
    fn explicit_tier_without_compiled_backend_names_gate() {
        let expected = if cfg!(any(
            feature = "sandbox-mock",
            feature = "sandbox-gvisor",
            feature = "sandbox-firecracker"
        )) {
            "requires feature sandbox-container"
        } else {
            "sandbox executor requires feature sandbox-mock"
        };
        let dir = temp_dir("feature-gate");
        let out = pf(
            &dir,
            &[
                "--executor",
                "sandbox",
                "--sandbox-backend",
                "container",
                "append",
                "model_claim",
                "x",
                "--task",
                "t",
            ],
        );
        assert_eq!(exit_code(&out), 2);
        assert!(
            stderr(&out).contains(expected),
            "expected {expected:?} in: {}",
            stderr(&out)
        );
    }
}
