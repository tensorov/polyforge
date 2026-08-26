//! Sandbox capability probing and tier selection.
//!
//! Wave 2 of the v0.4.0 hardening plan: before any real isolation backend
//! ships, this module answers one question fail-closed: which sandbox tier
//! can this host actually run? All system access sits behind the
//! [`ProbeSource`] seam so every selection rule is unit-testable with a fake
//! probe fixture, while production uses [`ProdProbe`] against the real host.
//!
//! Selection is evaluated IN ORDER (user decision): Firecracker when KVM is
//! available AND the firecracker/jailer binaries resolve, else gVisor when a
//! container runtime has runsc registered, else a plain container runtime,
//! else an error. An explicit tier request never falls back silently: an
//! unavailable named tier fails closed with [`TierError::TierUnavailable`]
//! naming the missing prerequisite.

use std::env;
use std::fmt;
use std::path::Path;
use std::path::PathBuf;

/// Sandbox isolation tiers, strongest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxTier {
    /// Firecracker microVM: requires `/dev/kvm` plus the firecracker and
    /// jailer binaries.
    Firecracker,
    /// gVisor (runsc) through a registered container runtime.
    Gvisor,
    /// Plain ephemeral container under docker or podman.
    Container,
}

impl SandboxTier {
    /// Short metadata prefix recorded on attestations ("fc", "gvisor",
    /// "container").
    pub fn label(self) -> &'static str {
        match self {
            SandboxTier::Firecracker => "fc",
            SandboxTier::Gvisor => "gvisor",
            SandboxTier::Container => "container",
        }
    }
}

/// System capability seam: production reads the real host, tests inject a
/// fixture. Every method answers one narrow question so selection rules stay
/// pure and exhaustively testable.
pub trait ProbeSource {
    /// True when `/dev/kvm` exists (hardware virtualization for microVMs).
    fn kvm_available(&self) -> bool;
    /// True when BOTH the firecracker and jailer binaries resolve on PATH or
    /// in well-known install locations.
    fn fc_binaries_present(&self) -> bool;
    /// Name of the detected container runtime ("docker" or "podman"), if any.
    fn container_runtime(&self) -> Option<String>;
    /// True when the runsc runtime is registered with the container runtime
    /// (or otherwise reachable).
    fn runsc_registered(&self) -> bool;
}

/// Directories checked in addition to PATH: non-interactive environments
/// often carry a trimmed PATH that misses /usr/local/bin installs.
const WELL_KNOWN_BIN_DIRS: &[&str] = &["/usr/local/bin", "/usr/bin"];

/// Resolve `name` as an executable file in a PATH directory.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    env::split_paths(&path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Resolve `name` on PATH first, then in well-known install locations.
fn find_binary(name: &str) -> Option<PathBuf> {
    if let Some(found) = find_on_path(name) {
        return Some(found);
    }
    WELL_KNOWN_BIN_DIRS
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| candidate.is_file())
}

/// Production probe reading the real host: `/dev/kvm` existence, PATH (plus
/// well-known dirs) lookups, and best-effort `docker info` inspection.
pub struct ProdProbe;

impl ProbeSource for ProdProbe {
    fn kvm_available(&self) -> bool {
        Path::new("/dev/kvm").exists()
    }

    fn fc_binaries_present(&self) -> bool {
        find_binary("firecracker").is_some() && find_binary("jailer").is_some()
    }

    fn container_runtime(&self) -> Option<String> {
        if find_binary("docker").is_some() {
            Some("docker".to_string())
        } else if find_binary("podman").is_some() {
            Some("podman".to_string())
        } else {
            None
        }
    }

    fn runsc_registered(&self) -> bool {
        // Cheap checks first: a runsc binary on PATH or a daemon.json that
        // registers the runtime proves registration without spawning a
        // process. Only then ask the docker daemon itself.
        if find_binary("runsc").is_some() {
            return true;
        }
        if let Ok(daemon_json) = std::fs::read_to_string("/etc/docker/daemon.json") {
            if daemon_json.contains("runsc") {
                return true;
            }
        }
        self.docker_info_lists_runsc()
    }
}

impl ProdProbe {
    /// Ask `docker info` whether a runsc runtime is registered. Best-effort:
    /// any spawn failure, non-zero exit, or unparseable output reads as "not
    /// registered" (fail closed).
    fn docker_info_lists_runsc(&self) -> bool {
        let output = std::process::Command::new("docker")
            .args(["info", "--format", "{{json .Runtimes}}"])
            .output();
        match output {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                stdout.contains("runsc")
            }
            _ => false,
        }
    }
}

/// Tier selection failures. Both variants are actionable: auto-selection
/// reports that no tier exists at all, while a named-tier request reports
/// exactly which prerequisite is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TierError {
    /// Auto-selection found no usable sandbox tier on this host.
    NoTierAvailable,
    /// An explicitly requested tier is unavailable; `missing` names the
    /// prerequisite so operators can act instead of guessing.
    TierUnavailable { tier: SandboxTier, missing: String },
}

impl fmt::Display for TierError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TierError::NoTierAvailable => write!(
                f,
                "no sandbox tier available: need /dev/kvm plus firecracker/jailer \
                 binaries, or a container runtime (docker/podman)"
            ),
            TierError::TierUnavailable { tier, missing } => {
                write!(f, "sandbox tier {} unavailable: {}", tier.label(), missing)
            }
        }
    }
}

impl std::error::Error for TierError {}

/// Select a sandbox tier against `probe`.
///
/// - `Some(tier)`: verify ONLY that tier; unavailable means
///   [`TierError::TierUnavailable`] naming the missing prerequisite. Never
///   falls back to another tier (fail closed).
/// - `None`: auto-select IN ORDER per the user decision: Firecracker when
///   kvm_available && fc_binaries_present, else Gvisor when a container
///   runtime exists && runsc_registered, else Container when a container
///   runtime exists, else [`TierError::NoTierAvailable`].
pub fn select_tier(
    choice: Option<SandboxTier>,
    probe: &dyn ProbeSource,
) -> Result<SandboxTier, TierError> {
    match choice {
        Some(tier) => verify_tier(tier, probe),
        None => auto_select(probe),
    }
}

/// Fail-closed availability check for ONE named tier.
fn verify_tier(tier: SandboxTier, probe: &dyn ProbeSource) -> Result<SandboxTier, TierError> {
    let missing = match tier {
        SandboxTier::Firecracker => {
            if !probe.kvm_available() {
                Some("/dev/kvm is not available (hardware virtualization required)".to_string())
            } else if !probe.fc_binaries_present() {
                Some("firecracker and jailer binaries not found on PATH".to_string())
            } else {
                None
            }
        }
        SandboxTier::Gvisor => {
            if probe.container_runtime().is_none() {
                Some("no container runtime (docker/podman) found on PATH".to_string())
            } else if !probe.runsc_registered() {
                Some("runsc runtime is not registered with the container runtime".to_string())
            } else {
                None
            }
        }
        SandboxTier::Container => {
            if probe.container_runtime().is_none() {
                Some("no container runtime (docker/podman) found on PATH".to_string())
            } else {
                None
            }
        }
    };
    match missing {
        Some(missing) => Err(TierError::TierUnavailable { tier, missing }),
        None => Ok(tier),
    }
}

/// Auto-selection, evaluated IN ORDER per the user decision.
fn auto_select(probe: &dyn ProbeSource) -> Result<SandboxTier, TierError> {
    if probe.kvm_available() && probe.fc_binaries_present() {
        return Ok(SandboxTier::Firecracker);
    }
    if probe.container_runtime().is_some() && probe.runsc_registered() {
        return Ok(SandboxTier::Gvisor);
    }
    if probe.container_runtime().is_some() {
        return Ok(SandboxTier::Container);
    }
    Err(TierError::NoTierAvailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test fixture implementing [`ProbeSource`] with plain fields so every
    /// matrix cell is constructed explicitly.
    #[derive(Default)]
    struct FakeProbe {
        kvm: bool,
        fc: bool,
        runtime: Option<String>,
        runsc: bool,
    }

    impl ProbeSource for FakeProbe {
        fn kvm_available(&self) -> bool {
            self.kvm
        }

        fn fc_binaries_present(&self) -> bool {
            self.fc
        }

        fn container_runtime(&self) -> Option<String> {
            self.runtime.clone()
        }

        fn runsc_registered(&self) -> bool {
            self.runsc
        }
    }

    fn docker_probe(runsc: bool) -> FakeProbe {
        FakeProbe {
            runtime: Some("docker".to_string()),
            runsc,
            ..FakeProbe::default()
        }
    }

    #[test]
    fn auto_prefers_firecracker_when_kvm_and_binaries_present() {
        let probe = FakeProbe {
            kvm: true,
            fc: true,
            ..FakeProbe::default()
        };
        assert_eq!(select_tier(None, &probe), Ok(SandboxTier::Firecracker));
    }

    #[test]
    fn auto_gvisor_happy_when_kvm_absent_docker_runsc() {
        let probe = FakeProbe {
            kvm: false,
            ..docker_probe(true)
        };
        assert_eq!(select_tier(None, &probe), Ok(SandboxTier::Gvisor));
    }

    #[test]
    fn auto_container_when_kvm_absent_docker_without_runsc() {
        let probe = FakeProbe {
            kvm: false,
            ..docker_probe(false)
        };
        assert_eq!(select_tier(None, &probe), Ok(SandboxTier::Container));
    }

    #[test]
    fn auto_fails_closed_when_nothing_available() {
        let probe = FakeProbe::default();
        assert_eq!(select_tier(None, &probe), Err(TierError::NoTierAvailable));
    }

    #[test]
    fn auto_skips_fc_binaries_without_kvm() {
        // Binaries present but no KVM: Firecracker must NOT be selected even
        // though fc_binaries_present alone would suggest capability.
        let probe = FakeProbe {
            kvm: false,
            fc: true,
            ..docker_probe(false)
        };
        assert_eq!(select_tier(None, &probe), Ok(SandboxTier::Container));
    }

    #[test]
    fn auto_skips_kvm_without_fc_binaries() {
        // KVM present but binaries absent: falls through past Firecracker.
        let probe = FakeProbe {
            kvm: true,
            fc: false,
            ..docker_probe(false)
        };
        assert_eq!(select_tier(None, &probe), Ok(SandboxTier::Container));
    }

    #[test]
    fn override_firecracker_without_kvm_names_kvm() {
        let probe = FakeProbe {
            kvm: false,
            fc: true,
            ..docker_probe(true)
        };
        let err = select_tier(Some(SandboxTier::Firecracker), &probe).unwrap_err();
        match err {
            TierError::TierUnavailable { tier, missing } => {
                assert_eq!(tier, SandboxTier::Firecracker);
                assert!(missing.contains("/dev/kvm"), "missing names kvm: {missing}");
            }
            other => panic!("expected TierUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn override_firecracker_with_kvm_but_no_binaries_names_binaries() {
        let probe = FakeProbe {
            kvm: true,
            fc: false,
            ..docker_probe(true)
        };
        let err = select_tier(Some(SandboxTier::Firecracker), &probe).unwrap_err();
        match err {
            TierError::TierUnavailable { tier, missing } => {
                assert_eq!(tier, SandboxTier::Firecracker);
                assert!(
                    missing.contains("firecracker"),
                    "missing names binaries: {missing}"
                );
            }
            other => panic!("expected TierUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn override_gvisor_happy_returns_gvisor() {
        let probe = FakeProbe {
            kvm: true,
            fc: true,
            ..docker_probe(true)
        };
        assert_eq!(
            select_tier(Some(SandboxTier::Gvisor), &probe),
            Ok(SandboxTier::Gvisor)
        );
    }

    #[test]
    fn override_gvisor_without_runsc_names_runsc() {
        let probe = docker_probe(false);
        let err = select_tier(Some(SandboxTier::Gvisor), &probe).unwrap_err();
        match err {
            TierError::TierUnavailable { tier, missing } => {
                assert_eq!(tier, SandboxTier::Gvisor);
                assert!(missing.contains("runsc"), "missing names runsc: {missing}");
            }
            other => panic!("expected TierUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn override_gvisor_without_runtime_names_runtime() {
        let probe = FakeProbe {
            runsc: true,
            ..FakeProbe::default()
        };
        let err = select_tier(Some(SandboxTier::Gvisor), &probe).unwrap_err();
        match err {
            TierError::TierUnavailable { tier, missing } => {
                assert_eq!(tier, SandboxTier::Gvisor);
                assert!(
                    missing.contains("container runtime"),
                    "missing names runtime: {missing}"
                );
            }
            other => panic!("expected TierUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn override_container_happy_even_without_kvm_or_runsc() {
        let probe = docker_probe(false);
        assert_eq!(
            select_tier(Some(SandboxTier::Container), &probe),
            Ok(SandboxTier::Container)
        );
    }

    #[test]
    fn override_container_without_runtime_names_runtime() {
        let probe = FakeProbe::default();
        let err = select_tier(Some(SandboxTier::Container), &probe).unwrap_err();
        match err {
            TierError::TierUnavailable { tier, missing } => {
                assert_eq!(tier, SandboxTier::Container);
                assert!(
                    missing.contains("container runtime"),
                    "missing names runtime: {missing}"
                );
            }
            other => panic!("expected TierUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn override_never_falls_back_to_weaker_tier() {
        // Explicit Firecracker on a host that COULD run containers still
        // fails closed instead of silently downgrading.
        let probe = docker_probe(false);
        assert!(matches!(
            select_tier(Some(SandboxTier::Firecracker), &probe),
            Err(TierError::TierUnavailable { .. })
        ));
    }

    #[test]
    fn labels_match_attestation_metadata_prefixes() {
        assert_eq!(SandboxTier::Firecracker.label(), "fc");
        assert_eq!(SandboxTier::Gvisor.label(), "gvisor");
        assert_eq!(SandboxTier::Container.label(), "container");
    }

    #[test]
    fn error_display_is_actionable() {
        let no_tier = TierError::NoTierAvailable.to_string();
        assert!(no_tier.contains("no sandbox tier available"));
        assert!(no_tier.contains("docker/podman"));

        let unavailable = TierError::TierUnavailable {
            tier: SandboxTier::Firecracker,
            missing: "/dev/kvm is not available".to_string(),
        }
        .to_string();
        assert!(unavailable.contains("sandbox tier fc unavailable"));
        assert!(unavailable.contains("/dev/kvm"));
    }
}
