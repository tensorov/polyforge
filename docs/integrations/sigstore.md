# PolyForge anchor verification guide: Sigstore

Every push to `master` runs a dedicated least-privilege anchoring job that signs the
PolyForge ledger tail hash keylessly with `cosign sign-blob` and uploads the result as
the `anchor-<run_id>` artifact. This guide verifies that artifact end to end against
your own checkout, so you can confirm that the ledger state you are reading existed at
CI time with exactly the tail hash it claims.

## What the anchor proves, and what it does not

The bundle proves one narrow fact: at CI time, a GitHub Actions workflow run in
`tensorov/polyforge` produced a Sigstore signature over the byte string equal to the
ledger tail hash. The certificate identity is pinned to this repository's workflow
origin and to GitHub's OIDC issuer, so a valid verification means the ledger state with
that tail hash existed when that run executed.

It does not prove checkout authenticity. As stated in [SECURITY.md](../../SECURITY.md),
tamper evidence is only meaningful within a trusted checkout: the chain proves the
ledger was not rewritten; it does not prove the checkout itself is authentic. A
repo-writer can rewrite the ledger and re-commit an anchor. The anchoring job makes
silent history rewriting detectable across time (an old signed tail no longer matches a
rewritten chain), but it cannot certify that any given working copy matches the signed
state. That last step is the digest comparison you perform yourself in step 4 below.

## Prerequisites

Install cosign (one line):

```sh
curl -O -L "https://github.com/sigstore/cosign/releases/latest/download/cosign-linux-amd64" && sudo mv cosign-linux-amd64 /usr/local/bin/cosign && sudo chmod +x /usr/local/bin/cosign
```

Authenticate the GitHub CLI and confirm it resolves:

```sh
gh auth status
```

You also need a polyforge checkout of the commit whose CI run you want to verify, with
either `polyforge-cli` on your `PATH` or the workspace built locally (`cargo build -p
polyforge-cli`).

## Step-by-step verification

### 1. Reconstruct the signed blob from your own checkout

The anchoring job signs the tail-hash string itself. Reproduce that byte string from
your checkout and write it to a file:

```sh
cd /path/to/polyforge
polyforge-cli ledger tail > expected-tail.txt
```

Without an installed binary, use the workspace build instead:

```sh
cargo run -p polyforge-cli -- ledger tail > expected-tail.txt
```

This reconstructed file is what CI signed. Verification compares the signature against
it, not against anything inside the artifact.

### 2. Download the anchor artifact

Find the run id of the push you want to verify (`gh run list --workflow ci.yml`), then:

```sh
gh run download <run-id> -n anchor-<run-id> -D /tmp/anchor
find /tmp/anchor -type f | sort
```

The artifact contains two files: `anchor-<run-id>.bundle` (the Sigstore bundle:
signature, Fulcio certificate, Rekor entry) and `tail.txt` (the blob CI signed, for
reference). The download preserves the runner's directory layout, so locate both by
name rather than assuming paths.

### 3. Verify the signature

```sh
BUNDLE=$(find /tmp/anchor -name 'anchor-*.bundle')
cosign verify-blob \
  --bundle "$BUNDLE" \
  --certificate-identity-regexp '^https://github.com/tensorov/polyforge/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  expected-tail.txt
```

Expect `Verified OK` and exit code 0. This is the tlog-backed variant: cosign checks
the Rekor transparency-log inclusion proof online, which establishes the signing time,
so the short-lived Fulcio certificate is validated as of signing time and the command
works at any later date.

For air-gapped environments the same check runs offline against the inclusion proof
embedded in the bundle:

```sh
cosign verify-blob \
  --bundle "$BUNDLE" \
  --certificate-identity-regexp '^https://github.com/tensorov/polyforge/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --offline \
  expected-tail.txt
```

### 4. Compare against your local ledger

Confirm the verified byte string equals the tail your own checkout reports:

```sh
diff <(polyforge-cli ledger tail) expected-tail.txt && echo TAIL-MATCH
sha256sum expected-tail.txt "$(find /tmp/anchor -name tail.txt)"
```

`TAIL-MATCH` plus identical file digests means: the state your checkout's ledger ends
in is exactly the state CI anchored. Any mismatch means your checkout diverged from the
signed run (or was rewritten); treat it as untrusted until explained.

## The tlog-independent variant, and why it expires

Some policies prefer verification without any transparency-log trust. Cosign supports
that by skipping Rekor entirely:

```sh
cosign verify-blob \
  --bundle "$BUNDLE" \
  --certificate-identity-regexp '^https://github.com/tensorov/polyforge/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --insecure-ignore-tlog \
  expected-tail.txt
```

This variant only works within minutes of the CI run. Without the Rekor entry there is
no trusted signing time, so cosign must validate the Fulcio leaf certificate against
the current clock, and that certificate carries a validity window of roughly ten
minutes around signing. After it expires the command fails with `failed to verify leaf
certificate: leaf certificate verification failed`. Use it only immediately after a run
finishes; for everything else use the tlog-backed flow above.

## Policy: unanchored bundles

A bundle without a verifiable Rekor inclusion proof is UNANCHORED. An UNANCHORED
bundle still carries a valid-looking signature over some bytes, but nothing ties its
signing time or uniqueness to the public append-only log, so it cannot serve as
evidence that a ledger state existed at a specific point in time. PolyForge treats
UNANCHORED bundles as no anchor at all. Accepting one is the consumer's explicit
choice: if your process admits them, record that decision in your own policy, because
the default reading of this guide is that only tlog-backed verification counts.

## Troubleshooting

- **`failed to verify leaf certificate: leaf certificate verification failed`.** You ran
  the `--insecure-ignore-tlog` variant after the signing certificate expired. Drop that
  flag and use the tlog-backed command from step 3.
- **`none of the expected identities were found` or identity errors.** The regexp must
  match the workflow origin exactly, including the trailing slash:
  `^https://github.com/tensorov/polyforge/`.
- **Artifact not found.** Anchoring artifacts exist only for pushes to `master` (the
  job is skipped on pull requests), and artifact retention follows the repository
  default. Check `gh run view <run-id>` for a green `anchoring` job first.
- **Unexpected file layout under `/tmp/anchor`.** `gh run download` reproduces the
  runner's nested directories. Always locate files with `find`, never hardcode paths.
- **One corrupted byte fails loudly.** Verification is all-or-nothing: flipping a
  single byte in the bundle makes cosign exit non-zero (in practice with a JSON parse
  error, since the bundle is a signed JSON document). Never repair a bundle by hand;
  re-download it.

## Verification appendix

Wave: T2 polyforge-oss-trust-stack (plan dated 2026-08). Every command block in this
guide was executed verbatim during authoring against the real anchor artifact from CI
run 32843338905, downloaded fresh into `/tmp/anchor-t2`, with cosign v3.1.3 and the
workspace CLI at repo HEAD. Results:

1. Step 1: installed-binary and `cargo run` tails were byte-identical, and both matched
   the CI-signed `tail.txt` byte-for-byte.
2. Step 3: tlog-backed `verify-blob` printed `Verified OK`, exit 0; the `--offline`
   variant also passed, exit 0.
3. Step 4: `diff <(polyforge-cli ledger tail) expected-tail.txt` reported `TAIL-MATCH`;
   `sha256sum` of the reconstructed tail and the CI-signed blob agreed.
4. Diagnostic finding recorded in the guide body: the exact `--insecure-ignore-tlog`
   command that had passed earlier the same day failed with exit 1 once the Fulcio leaf
   validity window closed, while the tlog-backed variant kept passing on the identical
   bundle. This motivated the expiry-window section above.
5. QA failure scenario: a copy of the bundle with byte 100 flipped (`0x7b` to `0x84`)
   failed verification with exit 1 (`invalid character '\u0084' looking for beginning
   of value`).

Full transcript: `.omo/evidence/task-2-polyforge-oss-trust-stack.txt`.
