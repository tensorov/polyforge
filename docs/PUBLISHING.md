# Publishing to crates.io

This checklist documents the steps for publishing PolyForge to crates.io.
It is prep-only: nothing here executes automatically.

> **User-gated.** Publishing is performed only on the user's explicit command.
> Never run `cargo login`, `cargo publish`, or any other crates.io interaction
> on your own initiative.

## Prerequisites

- [ ] Create a crates.io API token at <https://crates.io/settings/tokens>
  (scope: "publish-new" or full access).
- [ ] Authenticate the local toolchain with `cargo login` and paste the token
  when prompted. The token is stored in `~/.cargo/credentials` — never commit
  it, print it, or paste it into any file in this repository.
- [ ] Confirm the workspace builds cleanly first:
      `cargo build --workspace` and `cargo test --workspace`.

## Dry-run verification (per crate)

Verify each crate packages cleanly before anything is uploaded. Run these from
the workspace root:

- [ ] `cargo package -p polyforge-core --allow-dirty`
- [ ] `cargo package -p polyforge-toolrunner --allow-dirty`
- [ ] `cargo package -p polyforge-mcp --allow-dirty`
- [ ] `cargo package -p polyforge-cli --allow-dirty`

For each crate, inspect the generated tarball to confirm exactly the intended
files are shipped and that no unintended files (secrets, local config, target
artifacts) leak into the package:

- [ ] Open the generated `target/package/<crate>-<version>.crate` and list its
      contents: `tar tzf target/package/<crate>-<version>.crate`
- [ ] Verify the `Cargo.toml`, `Cargo.toml.orig`, `Cargo.lock` (if any), `src/`,
      and `README.md` are present.
- [ ] Verify no `.env`, credentials, or private files appear in the listing.
- [ ] Verify the crate's license, edition (2021), and rust-version (1.85)
      metadata are correct.

## Publish order

Crates must be published in dependency order so that each downstream crate
resolves the already-published version of its dependencies:

1. `cargo publish -p polyforge-core`
2. `cargo publish -p polyforge-toolrunner`
3. `cargo publish -p polyforge-mcp`
4. `cargo publish -p polyforge-cli`

Check the crates.io page of each crate after publishing (200 OK, correct
version, expected files) before moving to the next one.

## Post-publish follow-up

- [ ] Add crates.io and docs.rs badges to the README (for example, a
      `docs.rs` badge linking to `https://docs.rs/<crate>` and a crates.io
      badge linking to `https://crates.io/crates/<crate>`).
      **Deferred on purpose** — per plan decision D4 these badges are only
      added after a real publish happens, so the README never links to
      non-existent crates.io pages.
- [ ] The MSRV badge is added separately (see plan todo 5) and is independent
      of this publish gate.
- [ ] Tag the release in git (e.g. `git tag v0.1.0`) and push it.
