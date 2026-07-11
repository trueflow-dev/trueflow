# Trueflow Terraform / OpenTofu

Terraform-compatible HCL for the public website at `trueflow.dev`.

## Safety

No secrets are stored in this directory.

- domain names are public
- AWS account selection comes from your local AWS environment
- certificate ARNs, access keys, and deploy credentials are **not** checked into the repo
- `.terraform/` and local state files should stay untracked

AWS credentials should come from your local shell, AWS profile, or AWS SSO session.

Tofu and Terraform both understand this HCL. The examples below use OpenTofu (`tofu`).

## Current scope

This configuration manages one CloudFront-backed static website with:

- `trueflow.dev` as the canonical host
- `www.trueflow.dev` redirecting to the apex host
- reuse of the existing public Route53 hosted zone for `trueflow.dev`
- a private S3 bucket behind CloudFront Origin Access Control
- a single bucket/distribution serving:
  - `/`
  - `/about/`
  - `/install/`
  - `/install.sh`
  - `/download/<artifact_name>`

It creates these AWS resources:

- one private S3 bucket
- one CloudFront distribution
- one ACM certificate for `trueflow.dev` + `www.trueflow.dev`
- Route53 alias records for apex + `www`
- Route53 DNS validation records for the ACM certificate
- one CloudFront Function for apex redirect and clean-path rewrites

It does **not** create:

- a new hosted zone
- public S3 website hosting
- EC2, containers, or a Lambda-backed app server
- databases or other persistent app services

## Prerequisites

- OpenTofu or Terraform installed locally
- AWS CLI configured locally
- AWS credentials with permissions for Route53, ACM, CloudFront, S3, and IAM-linked CloudFront/S3 policy changes
- an existing public Route53 hosted zone for `trueflow.dev`

Note: on local Darwin right now, the flake-pinned Nix `aws` binary hangs during
startup, so the repo dev shell intentionally relies on your existing ambient
`aws` instead of shadowing it.

## Safe public release workflow

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
## State

This repo now commits its S3 backend config directly in `backend.tf`.

Backend:

- bucket: `jm-deploy-state-bucket`
- key: `trueflow/site/terraform.tfstate`
- region: `us-west-2`
- encryption: enabled

So on fresh machine, no extra backend config file needed. Run:

```sh
nix develop
tofu -chdir=infra/terraform init
```

If you already had local state and need to migrate it into S3, run:

```sh
nix develop
tofu -chdir=infra/terraform init -migrate-state -force-copy
```

AWS provider for website infra still targets `us-east-1`; backend region is
region of S3 state bucket.
