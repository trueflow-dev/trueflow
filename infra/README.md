# Infra

Operational and deployment docs for trueflow live here instead of the public top-level `README.md`.

If you are just using trueflow, start with the repo root `README.md` or <https://trueflow.dev>.

## Scope

This directory is the operator/developer entrypoint for:

- website hosting for `trueflow.dev`
- release/download artifact deployment
- Terraform/OpenTofu infrastructure docs

## Safe website and download publication

From the repo root, use the full orchestrator for every public download release:

```sh
nix develop
./scripts/deploy-public-site.sh
```

It is the only supported download publication path. It packages the macOS
artifact, requires the versioned artifact directory to contain the exact macOS
and Linux archives plus their checksum manifest, then:

1. freezes and validates a private exact snapshot;
2. begins a receipt-bound publication while old and new download sets coexist;
3. switches and invalidates the website using that receipt; and
4. finalizes old-download cleanup only after the website switch.

The Linux archive must be prepared on native Linux before the final publication:

```sh
./scripts/package-linux-release.sh
./scripts/smoke-test-release.sh .trueflow/release-artifacts/vX.Y.Z/trueflow-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz
```

For a supplied macOS binary, keep the same full publication workflow:

```sh
./scripts/deploy-public-site.sh --macos-binary path/to/aarch64-apple-darwin/trueflow
```

The fast path preserves the same release protocol and skips only `tofu apply`:

```sh
./scripts/deploy-public-site-fast.sh
```

If infrastructure is applied manually, finish with the safe full workflow
without a second apply:

```sh
cd infra/terraform
tofu init
tofu fmt -check
tofu plan
# inspect the plan carefully
tofu apply
cd ../..
./scripts/deploy-public-site.sh --skip-infra-apply
```

`./scripts/deploy-website.sh` without a receipt is a standalone website-only
deployment. It takes the same release lock, verifies the website release
against its immutable ledger, and preserves both `download/` and
`.trueflow-release/`; it never cleans downloads.

Snapshots and receipts are private by default under
`.trueflow/release-snapshots/<version>-<nonce>` and
`.trueflow/release-receipts/<version>-<nonce>.receipt` (or their corresponding
`TRUEFLOW_RELEASE_SNAPSHOT_BASE` and `TRUEFLOW_RELEASE_RECEIPT_BASE`
overrides). Keep both for recovery. Abort is permitted only before the
receipt-bound website switch and never cleans downloads. A failure after the
switch leaves the lock for manual, phase-aware recovery. Stale receipt/phase
data or a competing lock fails closed—never delete a lock manually. Normal
unlock uses an ETag-conditional current-key delete marker, never a version-ID
deletion.

Note: on local Darwin right now, the flake-pinned Nix `aws` binary hangs
during startup, so the dev shell intentionally relies on your existing ambient
`aws` instead of shadowing it.
## More detail

- `terraform/README.md` — Terraform/OpenTofu resource model, safety notes, prerequisites, and state/backend details
- `../website/README.md` — static website content/layout
