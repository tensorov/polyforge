# Firecracker tier: host prerequisites and deployment pack

T9 deployment pack for the `sandbox-firecracker` execution backend. The
protocol is the PROVEN T8 spike (`spike/spike.sh`, verdict GO on a KVM
host): boot an ephemeral microVM, drive its REST API over a unix socket,
and execute allowlisted tools through the vsock guest agent.

Contents of this directory:

| File                        | Purpose                                                        |
| --------------------------- | -------------------------------------------------------------- |
| `Dockerfile.fc-controlplane`| Privileged control-plane image: firecracker + jailer + python3  |
| `rootfs-build.sh`           | Builds `rootfs.ext4` + `manifest.json` (build-inputs manifest)  |
| `README-host-prereqs.md`    | This document                                                   |
| `spike/spike.sh`            | T8 go/no-go evidence script (do not use for production runs)    |

## 1. Host prerequisites

### KVM

* `/dev/kvm` must exist: `ls -l /dev/kvm`. Check virtualization with
  `grep -Ec '(vmx|svm)' /proc/cpuinfo` (2+ expected).
* Your user needs read/write access, usually via the `kvm` group:
  `sudo usermod -aG kvm "$USER"` then re-login.
* **Nested virtualization is NOT required.** KVM passthrough into a
  container or VM suffices; this is proven by the
  [fadams/firecracker-in-docker](https://github.com/fadams/firecracker-in-docker)
  PoC, which boots real microVMs from inside unprivileged-ish containers by
  passing `/dev/kvm` through. The Ignite precedent (archived) is NOT adopted.

### Running the control plane in Docker

The fadams pattern, which this pack follows:

```sh
docker run -d --name pf-fc-control \
  --device=/dev/kvm --device=/dev/net/tun \
  --cap-add=NET_ADMIN \
  --group-add "$(getent group kvm | cut -d: -f3)" \
  -v /opt/polyforge-fc:/assets:ro \
  polyforge-fc-controlplane
```

Caveats:

* `--device=/dev/kvm` is the only hard requirement for vsock-only guests;
  `--device=/dev/net/tun` plus `CAP_NET_ADMIN` are the ONLY extra caps,
  reserved for optional TAP networking that PolyForge does not use today.
* Non-root works: attach the host kvm gid at runtime with `--group-add`
  (the gid differs per host, so it cannot be baked into the image).
* No ports are published. Every control channel is a unix socket under the
  configured work dir; run the toolrunner inside the same container (or
  share the work dir as a volume) so it can reach those sockets.
* On some hardened hosts, seccomp defaults block KVM ioctls; if boot fails
  with `EPERM` from `/dev/kvm`, add `--security-opt seccomp=unconfined`
  ONLY after reviewing the tradeoff, or run on the host directly.

## 2. Asset pinning strategy

Attestations must name exactly what executed, so both assets are pinned:

**Kernel**: download ONE exact artifact from the Firecracker CI S3 bucket
and record its SHA-256. The T8 spike used:

```sh
curl -fsSLO https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/x86_64/vmlinux-6.1.102
sha256sum vmlinux-6.1.102   # record next to the asset
```

The kernel digest enters the executor digest directly
(`kernel_sha256` in the build-inputs manifest), so replacing the kernel
with any other build changes every subsequent attestation identity.

**Rootfs**: built by `rootfs-build.sh`, which emits `rootfs.ext4` plus
`manifest.json`. The executor digest hashes the DECLARED INPUTS (base
package identity + sha256 of the build script), NOT the raw ext4 bytes:
mkfs.ext4 output embeds UUIDs and superblock timestamps that differ between
rebuilds even from identical inputs, so hashing image bytes would break
cross-build comparability. Same inputs therefore yield one stable `fc:`
digest across rebuilds.

```sh
./rootfs-build.sh /opt/polyforge-fc
# -> /opt/polyforge-fc/rootfs.ext4
# -> /opt/polyforge-fc/manifest.json
```

Host tools needed by the builder: `gcc` (static link), `e2fsprogs`
(`mkfs.ext4 -d`), `curl`, `sha256sum`.

**Firecracker + jailer**: installed by `Dockerfile.fc-controlplane` at a
pinned `FC_VERSION` (default 1.9.1, matching the T8 spike). Jailer mode is
the mandatory production posture (chroot + uid/gid + cgroups); set
`FcConfig::jailer` to enable it. In jailer mode the API socket, logger
target, and vsock socket live inside the jail root
(`<chroot_base>/<run_id>/root/...`) while kernel/rootfs paths are passed
verbatim, so stage those assets where the jailed process can read them.

## 3. Running an attestation through FcExecutor end-to-end

Build any front-end crate with the feature (the CLI forwards it):

```sh
cargo build -p polyforge-cli --features sandbox-firecracker --release
```

Provision the environment:

```sh
export POLYFORGE_FC_KERNEL=/opt/polyforge-fc/vmlinux-6.1.102
export POLYFORGE_FC_ROOTFS=/opt/polyforge-fc/rootfs.ext4
export POLYFORGE_FC_MANIFEST=/opt/polyforge-fc/manifest.json
export POLYFORGE_FC_WORK_DIR=/var/lib/polyforge-fc   # optional, default <tmp>/pf-fc
export PF_TOOL_TIMEOUT_SECS=600                      # optional overall budget
```

Selection smoke test (fail-closed tier resolution, no VM booted yet):

```sh
./target/release/polyforge-cli --executor sandbox --sandbox-backend firecracker init
echo $?   # 0 means the tier resolved: /dev/kvm present, binaries found, assets valid
```

Real attestation run through Rust (the MCP `evidence_verify` path uses the
same runner seam):

```rust
use polyforge_toolrunner::fc_exec::{FcConfig, FcExecutor};
use polyforge_toolrunner::{lookup, run};

let config = FcConfig::from_environment().expect("POLYFORGE_FC_* assets");
let exec = FcExecutor::new(config);
let cargo = lookup("cargo --version").expect("allowlisted");
let out = exec.run(&cargo, &[]).expect("microVM round-trip");
assert_eq!(out.exit_code, 0);
println!("tool_version={} stdout_hash={}", out.tool_version, out.stdout_hash);
```

What happens per call: one ephemeral microVM boots (logger PUT, then
boot-source, drives/rootfs, vsock, InstanceStart over the FC unix socket),
the host completes the vsock `CONNECT 5001` handshake, sends
`{"cmd":"cargo --version"}` followed by the actual command as
newline-delimited JSON on ONE connection, reads
`{"exit":N,"stdout":"..."}` replies, and kills the VM on drop, on error,
or when `PF_TOOL_TIMEOUT_SECS` expires (watchdog SIGKILLs the whole
process group).

The recorded attestation metadata carries the tier-prefixed executor
identity `fc:<sha256 of {kernel_sha256, rootfs_build_inputs, fc_version}>`,
computed per the inputs-not-bytes rule above.

## 4. Tests

* Hermetic unit tests (no VM, no KVM needed):
  `cargo test -p polyforge-toolrunner --features sandbox-firecracker`.
* One opt-in live round-trip exists behind `#[ignore]`; run it ONCE on a
  provisioned KVM host:
  `cargo test -p polyforge-toolrunner --features sandbox-firecracker e2e_live_microvm_echo_roundtrip -- --ignored`
  It prints SKIP-with-reason when prerequisites are absent.
