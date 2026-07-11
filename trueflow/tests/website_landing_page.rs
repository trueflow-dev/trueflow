use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

fn repo_root() -> Result<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("crate should live under the repo root")
}

fn read_repo_file(relative_path: &str) -> Result<String> {
    let path = repo_root()?.join(relative_path);
    fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))
}


fn assert_contains(haystack: &str, needle: &str, context: &str) {
    assert!(
        haystack.contains(needle),
        "expected {context} to contain {needle:?}"
    );
}

fn assert_not_contains(haystack: &str, needle: &str, context: &str) {
    assert!(
        !haystack.contains(needle),
        "expected {context} to not contain {needle:?}"
    );
}


#[test]
fn website_install_script_targets_same_domain_and_macos_arm64() -> Result<()> {
    let script = read_repo_file("website/install.sh")?;

    assert_contains(
        &script,
        "BASE_URL=\"${TRUEFLOW_BASE_URL:-https://trueflow.dev}\"",
        "installer base url",
    );
    assert_contains(
        &script,
        "DEFAULT_VERSION=\"v0.1.1\"",
        "installer default version",
    );
    assert_contains(
        &script,
        "aarch64-apple-darwin",
        "installer macos supported target",
    );
    assert_contains(
        &script,
        "x86_64-unknown-linux-musl",
        "installer linux supported target",
    );
    assert_contains(
        &script,
        "trueflow-${VERSION}-${TARGET}.tar.gz",
        "installer artifact naming",
    );
    assert_contains(
        &script,
        "trueflow-${VERSION}-SHA256SUMS.txt",
        "installer checksum naming",
    );
    assert_contains(&script, "--version", "installer pinned version flag");
    assert_contains(&script, "--to", "installer custom install dir flag");
    assert_not_contains(
        &script,
        "current draft support is Apple Silicon macOS only",
        "installer stale draft support message",
    );
    assert_not_contains(
        &script,
        "current support is Apple Silicon macOS only",
        "installer stale single-platform support message",
    );

    Ok(())
}


#[test]
fn infra_terraform_skeleton_is_present_and_public_safe() -> Result<()> {
    let backend = read_repo_file("infra/terraform/backend.tf")?;
    let versions = read_repo_file("infra/terraform/versions.tf")?;
    let main = read_repo_file("infra/terraform/main.tf")?;
    let variables = read_repo_file("infra/terraform/variables.tf")?;
    let outputs = read_repo_file("infra/terraform/outputs.tf")?;
    let readme = read_repo_file("infra/terraform/README.md")?;
    let package_built_release = read_repo_file("scripts/package-built-release.sh")?;
    let package_macos_release = read_repo_file("scripts/package-macos-release.sh")?;
    let package_linux_release = read_repo_file("scripts/package-linux-release.sh")?;
    let deploy_public_site = read_repo_file("scripts/deploy-public-site.sh")?;
    let deploy_public_site_fast = read_repo_file("scripts/deploy-public-site-fast.sh")?;
    let deploy_website = read_repo_file("scripts/deploy-website.sh")?;
    let deploy_downloads = read_repo_file("scripts/deploy-downloads.sh")?;
    let gitignore = read_repo_file(".gitignore")?;

    assert_contains(&backend, "backend \"s3\"", "terraform s3 backend block");
    assert_contains(
        &backend,
        "jm-deploy-state-bucket",
        "terraform backend bucket",
    );
    assert_contains(
        &backend,
        "trueflow/site/terraform.tfstate",
        "terraform backend key",
    );
    assert_contains(&backend, "us-west-2", "terraform backend region");
    assert_contains(&backend, "encrypt = true", "terraform backend encryption");
    assert_contains(
        &versions,
        "required_providers",
        "terraform provider declaration",
    );
    assert_contains(
        &versions,
        "source  = \"hashicorp/aws\"",
        "aws provider source",
    );
    assert_contains(
        &main,
        "data \"aws_route53_zone\" \"site\"",
        "route53 zone lookup",
    );
    assert_contains(&main, "private_zone = false", "public hosted zone lookup");
    assert_contains(
        &main,
        "resource \"aws_s3_bucket\" \"site\"",
        "site bucket resource",
    );
    assert_contains(
        &main,
        "resource \"aws_cloudfront_distribution\" \"site\"",
        "cloudfront distribution resource",
    );
    assert_contains(
        &main,
        "resource \"aws_acm_certificate\" \"site\"",
        "certificate resource",
    );
    assert_contains(
        &main,
        "resource \"aws_cloudfront_origin_access_control\" \"site\"",
        "oac resource",
    );
    assert_contains(
        &variables,
        "default     = \"trueflow.dev\"",
        "terraform apex domain default",
    );
    assert_contains(
        &variables,
        "default     = \"www.trueflow.dev\"",
        "terraform www domain default",
    );
    assert_contains(
        &variables,
        "default     = \"us-east-1\"",
        "terraform region default",
    );
    assert_contains(
        &outputs,
        "output \"site_bucket_name\"",
        "terraform bucket output",
    );
    assert_contains(
        &outputs,
        "output \"site_distribution_id\"",
        "terraform distribution output",
    );
    assert_contains(
        &readme,
        "No secrets are stored in this directory",
        "infra public safety note",
    );
    assert_contains(
        &readme,
        "Tofu and Terraform both understand this HCL",
        "terraform compatibility note",
    );
    assert_contains(
        &package_built_release,
        "trueflow-${VERSION}-${TARGET}.tar.gz",
        "shared packaging artifact name",
    );
    assert_contains(
        &package_built_release,
        "trueflow-${VERSION}-SHA256SUMS.txt",
        "shared packaging checksum name",
    );
    assert_contains(
        &package_built_release,
        ".trueflow/release-artifacts",
        "shared packaging output root",
    );
    assert_contains(
        &package_macos_release,
        "TARGET=\"aarch64-apple-darwin\"",
        "macos packaging target",
    );
    assert_contains(
        &package_macos_release,
        "--binary PATH",
        "macos packaging accepts supplied binary",
    );
    assert_contains(
        &package_macos_release,
        "packaging supplied macOS binary",
        "macos packaging supplied binary path",
    );
    assert_contains(
        &package_macos_release,
        "cargo build --release --locked",
        "macos packaging build command",
    );
    assert_contains(
        &package_macos_release,
        "package-built-release.sh",
        "macos packaging shared packager handoff",
    );
    assert_contains(
        &package_linux_release,
        "TARGET=\"x86_64-unknown-linux-musl\"",
        "linux packaging target",
    );
    assert_contains(
        &package_linux_release,
        "nix build --no-link --print-out-paths .#release",
        "linux packaging nix build command",
    );
    assert_contains(
        &deploy_public_site,
        "tofu init",
        "one-shot deploy tofu init step",
    );
    assert_contains(
        &deploy_public_site,
        "tofu fmt -check",
        "one-shot deploy tofu fmt step",
    );
    assert_contains(
        &deploy_public_site,
        "tofu validate",
        "one-shot deploy tofu validate step",
    );
    assert_contains(
        &deploy_public_site,
        "tofu apply",
        "one-shot deploy tofu apply step",
    );
    assert_contains(
        &deploy_public_site,
        "$SCRIPT_DIR/deploy-website.sh",
        "one-shot deploy website step",
    );
    assert_contains(
        &deploy_public_site,
        "$SCRIPT_DIR/package-macos-release.sh",
        "one-shot deploy package step",
    );
    assert_contains(
        &deploy_public_site,
        "$SCRIPT_DIR/deploy-downloads.sh",
        "one-shot deploy downloads step",
    );
    assert_contains(
        &deploy_public_site,
        "--auto-approve",
        "one-shot deploy auto approve flag",
    );
    assert_contains(
        &deploy_public_site,
        "--macos-binary PATH",
        "one-shot deploy accepts supplied macos binary",
    );
    assert_contains(
        &deploy_public_site,
        "--binary \"$MACOS_BINARY\"",
        "one-shot deploy forwards supplied macos binary",
    );
    assert_contains(
        &deploy_public_site_fast,
        "--skip-infra-apply",
        "fast deploy skips infra apply",
    );
    assert_contains(
        &deploy_public_site_fast,
        "deploy-public-site.sh",
        "fast deploy wrapper target",
    );
    assert_contains(
        &deploy_website,
        "TRUEFLOW_INFRA_CLI",
        "deploy website infra cli override",
    );
    assert_contains(
        &deploy_downloads,
        "TRUEFLOW_INFRA_CLI",
        "deploy downloads infra cli override",
    );
    assert_contains(
        &deploy_downloads,
        "REQUIRED_MACOS_TARGET=\"aarch64-apple-darwin\"",
        "download deploy requires the macOS release target",
    );
    assert_contains(
        &deploy_downloads,
        "REQUIRED_LINUX_TARGET=\"x86_64-unknown-linux-musl\"",
        "download deploy requires the Linux release target",
    );
    assert_contains(
        &deploy_downloads,
        "missing required release artifact",
        "download deploy fails before partial destructive sync",
    );
    assert_contains(
        &gitignore,
        "infra/terraform/.terraform/",
        "terraform plugin dir ignore rule",
    );
    assert_contains(&gitignore, "*.tfstate", "terraform state ignore rule");

    Ok(())
}
