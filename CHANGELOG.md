# Changelog

All notable changes to PolyForge are documented here. Workspace-wide version bumps; per-crate details below.

## [0.4.0] — unreleased

### polyforge-toolrunner
- **Sandbox tier framework**: capability prober (KVM, container runtime, runsc registration, firecracker binaries) behind a `ProbeSource` seam for hermetic tests; fail-closed tier selection runs before any executor state is written.
- **Mock sandbox executor** (`sandbox-mock` feature): digest-stable mock backend.
- **Container backend** (`sandbox-container` feature): docker/podman exec wrapper; digest-pinned image (`POLYFORGE_SANDBOX_IMAGE`, default `polyforge-sandbox:latest`); read-only checkout mount; PATH-only env forwarding; blank env values fall back to the probed runtime / default image.
- **gVisor backend** (`sandbox-gvisor` feature): runsc route selection (container-runtime vs OCI bundle); binary-first argv construction (`primary_argv`) so zero-fixed-args tools never execute the image default CMD.
- **Firecracker backend** (`sandbox-firecracker` feature): ephemeral microVM per attestation run over the proven vsock exec protocol; connect+handshake retry loop (FC v1.9.1 answers the UDS CONNECT before the guest agent listens); jailer argv builder; `rootfs_sha256` manifest binding verified fail-closed before any VM boot; read-only rootfs drive keeps the binding stable across boots; executor digest composed from declared build inputs (kernel sha256, rootfs build-script sha256, firecracker version).
- **Tier metadata**: attestations carry `fc:` / `gvisor:` / `container:`-prefixed executor digests when a tier is selected; a tier-set run whose digest is unavailable fails closed instead of appending an unidentified `Verified` entry.
- **Fix**: `sha256_of_file` double-hashed (routed the finalized digest through a second sha256); kernel hashes in fc digests recorded before this fix were sha256-of-sha256.
- **Fix**: FC vsock boot race — the stream closed before the guest agent listened; restored the spike-proven retry behavior.

### polyforge-cli
- `--sandbox-backend <container|gvisor|firecracker>` leading-only global flag; requires `--executor sandbox`; an explicit tier without the compiled backend fails closed naming the missing feature.

### polyforge-attest
- Verify the Merkle chain on read; charset-gate subject identifiers.

### Workspace / CI
- `deny.toml` dependency policy gate (bans/licenses/sources) + CI dependency-policy job, with negative-probe evidence retained.
- Mutation kill-test waves across attest / toolrunner / cli / langfuse-bridge: the V-E 17-item survivor map fully killed; cli `main.rs` 287 mutants / 0 missed; langfuse-bridge 37 / 0 missed.
- Firecracker vsock exec spike transcript + self-hosted deployment pack (`Dockerfile.fc-controlplane`, deterministic `rootfs-build.sh`, host-prereqs README).
- Strict rustdoc across the workspace.

[0.4.0]: https://github.com/tensorov/polyforge/compare/v0.3.0...v0.4.0
