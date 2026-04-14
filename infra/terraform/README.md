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

## Local workflow

From the repo root:

```sh
nix develop
./scripts/deploy-public-site.sh
```

That one command will:

- run `tofu init`, `tofu fmt -check`, `tofu validate`, and `tofu apply`
- upload `website/`
- package the Apple Silicon macOS binary artifact
- upload `/download/` artifacts

For the common fast path after infra is already up:

```sh
./scripts/deploy-public-site-fast.sh
```

That keeps the safety checks (`tofu init`, `fmt`, `validate`) but skips
`tofu apply` before uploading website + download artifacts.

If you want to run the steps manually instead:

```sh
cd infra/terraform
tofu init
tofu fmt -check
tofu plan
# inspect the plan carefully
# when ready:
tofu apply
cd ../..
./scripts/deploy-website.sh
```

To package and upload the current Apple Silicon macOS binary:

```sh
./scripts/package-macos-release.sh
./scripts/deploy-downloads.sh .trueflow/release-artifacts/v0.1.0
```

To upload a different artifact directory later:

```sh
./scripts/deploy-downloads.sh path/to/release-artifacts
```

## State

The default workflow uses local state for now. If you later want a remote S3
backend and locking, add that explicitly rather than assuming it exists.
