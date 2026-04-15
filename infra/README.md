# Infra

Operational and deployment docs for trueflow live here instead of the public top-level `README.md`.

If you are just using trueflow, start with the repo root `README.md` or <https://trueflow.dev>.

## Scope

This directory is the operator/developer entrypoint for:

- website hosting for `trueflow.dev`
- release/download artifact deployment
- Terraform/OpenTofu infrastructure docs

## Website and download deploy flow

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

Note: on local Darwin right now, the flake-pinned Nix `aws` binary hangs
during startup, so the dev shell intentionally relies on your existing ambient
`aws` instead of shadowing it.

## Release artifact packaging and upload

To package and upload the current Apple Silicon macOS binary separately:

```sh
./scripts/package-macos-release.sh
./scripts/deploy-downloads.sh .trueflow/release-artifacts/v0.1.0
```

To package and smoke-test a native Linux x86_64 musl release on Linux x86_64:

```sh
./scripts/package-linux-release.sh
./scripts/smoke-test-release.sh .trueflow/release-artifacts/v0.1.0/trueflow-v0.1.0-x86_64-unknown-linux-musl.tar.gz
```

That Linux release flow uses `nix build .#release` as the build source of truth.
Website installer and install-page wiring for Linux is a follow-up; for now this
produces local release artifacts that can also be uploaded under `/download/`.

To upload a different artifact directory later:

```sh
./scripts/deploy-downloads.sh path/to/release-artifacts
```

## More detail

- `terraform/README.md` — Terraform/OpenTofu resource model, safety notes, prerequisites, and state/backend details
- `../website/README.md` — static website content/layout
