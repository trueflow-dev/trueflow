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
    fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))
}

fn read_repo_bytes(relative_path: &str) -> Result<Vec<u8>> {
    let path = repo_root()?.join(relative_path);
    fs::read(&path).with_context(|| format!("failed to read {}", path.display()))
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
fn website_landing_page_has_install_command_above_the_fold() -> Result<()> {
    let html = read_repo_file("website/index.html")?;

    assert_contains(
        &html,
        "<title>Trueflow — Semantic code review that keeps you in flow.</title>",
        "landing page title",
    );
    assert_contains(
        &html,
        "Trueflow is a semantic local code review tool",
        "landing page hero headline",
    );
    assert_contains(
        &html,
        "Use it for content review of existing code or diff review of pull requests and local changes.",
        "landing page content-vs-diff review copy",
    );
    assert_contains(
        &html,
        "curl -fsSL https://trueflow.dev/install.sh | sh",
        "landing page quick install command",
    );
    assert_contains(
        &html,
        "Current binary support: Apple Silicon macOS and Linux x86_64.",
        "landing page supported platform note",
    );
    assert_contains(
        &html,
        "href=\"/install/\"",
        "landing page install details link",
    );
    assert_contains(
        &html,
        "href=\"https://github.com/trueflow-dev/trueflow/blob/main/README.md\"",
        "landing page docs link",
    );
    assert_contains(
        &html,
        "src=\"/assets/tui.png\"",
        "landing page hero screenshot",
    );
    assert_contains(
        &html,
        "README.md#official-language-support",
        "landing page language support matrix link",
    );
    assert_contains(
        &html,
        "Review the semantic units that matter",
        "benefit section content",
    );
    assert_contains(
        &html,
        "Use the workflow that fits your loop",
        "benefit section content",
    );
    assert_contains(
        &html,
        "Keep review state attached to content",
        "benefit section content",
    );

    Ok(())
}

#[test]
fn readme_language_support_matrix_explains_official_vs_fallback_support() -> Result<()> {
    let readme = read_repo_file("README.md")?;

    assert_contains(
        &readme,
        "## Official language support",
        "readme language support section",
    );
    assert_contains(
        &readme,
        "semi-smart text processing",
        "readme fallback explanation",
    );
    assert_contains(
        &readme,
        "| Rust | ✅ | ✅ | ✅ | ✅ |",
        "readme rust support row",
    );
    assert_contains(
        &readme,
        "| Go | ✅ | ✅ | ✅ | ✅ |",
        "readme go support row",
    );
    assert_contains(
        &readme,
        "| C++ | ✅ | ✅ | ✅ | ✅ |",
        "readme cpp support row",
    );
    assert_contains(
        &readme,
        "| Text / Org | 🚧 | ✅ | — | — |",
        "readme text fallback row",
    );

    Ok(())
}

#[test]
fn readme_separates_public_docs_from_operator_infra_docs() -> Result<()> {
    let readme = read_repo_file("README.md")?;
    let infra_readme = read_repo_file("infra/README.md")?;

    assert_contains(
        &readme,
        "infra/README.md",
        "top-level readme operator docs pointer",
    );
    assert_not_contains(
        &readme,
        "## Website infra (`trueflow.dev`)",
        "top-level readme website infra section",
    );
    assert_contains(
        &infra_readme,
        "./scripts/deploy-public-site.sh",
        "infra readme one-shot deploy flow",
    );
    assert_contains(
        &infra_readme,
        "terraform/README.md",
        "infra readme terraform handoff",
    );

    Ok(())
}

#[test]
fn readme_tui_controls_match_current_default_keybinds() -> Result<()> {
    let readme = read_repo_file("README.md")?;

    assert_contains(
        &readme,
        "`l`, Right, Enter, and `C` open the selected item",
        "readme root open controls",
    );
    assert_contains(
        &readme,
        "`h`, Left, and `P` are back/leftward actions",
        "readme root back controls",
    );
    assert_contains(
        &readme,
        "`P`/`C` move to the semantic parent/child",
        "readme parent child controls",
    );
    assert_contains(
        &readme,
        "`c` add a comment",
        "readme comment control",
    );

    Ok(())
}

#[test]
fn readme_moves_internal_development_workflow_to_contributing_doc() -> Result<()> {
    let readme = read_repo_file("README.md")?;
    let contributing = read_repo_file("CONTRIBUTING.md")?;

    assert_contains(
        &readme,
        "CONTRIBUTING.md",
        "top-level readme contributing docs pointer",
    );
    assert_not_contains(
        &readme,
        "nix develop -c just check",
        "top-level readme internal development command block",
    );
    assert_contains(
        &contributing,
        "nix develop -c just check",
        "contributing local gate command",
    );
    assert_contains(
        &contributing,
        "nix develop -c just nix-check-release",
        "contributing release packaging check",
    );
    assert_contains(
        &contributing,
        "The coverage report is written to `trueflow/target/llvm-cov/html/index.html`.",
        "contributing coverage output note",
    );

    Ok(())
}

#[test]
fn website_install_page_explains_script_and_manual_downloads() -> Result<()> {
    let html = read_repo_file("website/install/index.html")?;

    assert_contains(&html, "Install trueflow", "install page title");
    assert_contains(
        &html,
        "curl -fsSL https://trueflow.dev/install.sh | sh",
        "install page quick install command",
    );
    assert_contains(
        &html,
        "curl -fsSL https://trueflow.dev/install.sh | sh -s -- --version v0.1.0",
        "install page pinned version command",
    );
    assert_contains(
        &html,
        "/download/trueflow-v0.1.0-aarch64-apple-darwin.tar.gz",
        "install page macos artifact link",
    );
    assert_contains(
        &html,
        "/download/trueflow-v0.1.0-x86_64-unknown-linux-musl.tar.gz",
        "install page linux artifact link",
    );
    assert_contains(
        &html,
        "/download/trueflow-v0.1.0-SHA256SUMS.txt",
        "install page checksum link",
    );
    assert_contains(
        &html,
        "nix run github:trueflow-dev/trueflow",
        "install page nix run command",
    );
    assert_contains(
        &html,
        "nix profile install github:trueflow-dev/trueflow",
        "install page nix profile install command",
    );
    assert_contains(
        &html,
        "inputs.trueflow.url = \"github:trueflow-dev/trueflow\";",
        "install page nix flake input example",
    );
    assert_contains(
        &html,
        "trueflow.packages.${pkgs.system}.default",
        "install page nix package reference",
    );
    assert_contains(
        &html,
        "path:/path/to/trueflow",
        "install page local nix path example",
    );
    assert_not_contains(
        &html,
        "These URLs go live when the first signed-off binary release is published.",
        "install page stale release note",
    );
    assert_not_contains(
        &html,
        "More targets can be added once the initial packaging and release flow is stable.",
        "install page stale packaging note",
    );

    Ok(())
}

#[test]
fn website_about_page_mentions_jordan_mcqueen_and_personal_site() -> Result<()> {
    let about_html = read_repo_file("website/about/index.html")?;
    let landing_html = read_repo_file("website/index.html")?;
    let install_html = read_repo_file("website/install/index.html")?;
    let edge_router = read_repo_file("infra/terraform/edge-router.js.tftpl")?;

    assert_contains(&about_html, "<title>About Trueflow</title>", "about page title");
    assert_contains(&about_html, "Jordan McQueen", "about page developer name");
    assert_contains(
        &about_html,
        "software engineer based in Tokyo",
        "about page developer bio",
    );
    assert_contains(&about_html, "href=\"https://jm.dev\"", "about page personal site link");
    assert_contains(&landing_html, "href=\"/about/\"", "landing page about link");
    assert_contains(&install_html, "href=\"/about/\"", "install page about link");
    assert_contains(&edge_router, "request.uri === \"/about\"", "about clean path rewrite");
    assert_contains(&edge_router, "request.uri = \"/about/index.html\"", "about index rewrite");

    Ok(())
}

#[test]
fn website_screenshot_asset_matches_repo_screenshot() -> Result<()> {
    let repo_screenshot = read_repo_bytes("tui.png")?;
    let website_screenshot = read_repo_bytes("website/assets/tui.png")?;

    assert_eq!(
        website_screenshot,
        repo_screenshot,
        "expected website/assets/tui.png to match repo-root tui.png",
    );

    Ok(())
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
        "DEFAULT_VERSION=\"v0.1.0\"",
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
fn website_styles_define_install_command_and_skip_link_patterns() -> Result<()> {
    let css = read_repo_file("website/site.css")?;

    assert_contains(&css, "--paper: #f7f5f2;", "website light paper token");
    assert_contains(&css, "--accent: #0e9f86;", "website accent token");
    assert_contains(&css, ".install-card", "install card styles");
    assert_contains(&css, ".command-line", "command block styles");
    assert_contains(&css, "left: -9999px;", "skip link hidden offscreen");
    assert_contains(&css, ".skip-link:focus", "skip link focus reveal styles");
    assert_contains(&css, "@media (max-width: 900px)", "website responsive breakpoint");

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
    assert_contains(&backend, "jm-deploy-state-bucket", "terraform backend bucket");
    assert_contains(&backend, "trueflow/site/terraform.tfstate", "terraform backend key");
    assert_contains(&backend, "us-west-2", "terraform backend region");
    assert_contains(&backend, "encrypt = true", "terraform backend encryption");
    assert_contains(&versions, "required_providers", "terraform provider declaration");
    assert_contains(&versions, "source  = \"hashicorp/aws\"", "aws provider source");
    assert_contains(&main, "data \"aws_route53_zone\" \"site\"", "route53 zone lookup");
    assert_contains(&main, "private_zone = false", "public hosted zone lookup");
    assert_contains(&main, "resource \"aws_s3_bucket\" \"site\"", "site bucket resource");
    assert_contains(&main, "resource \"aws_cloudfront_distribution\" \"site\"", "cloudfront distribution resource");
    assert_contains(&main, "resource \"aws_acm_certificate\" \"site\"", "certificate resource");
    assert_contains(&main, "resource \"aws_cloudfront_origin_access_control\" \"site\"", "oac resource");
    assert_contains(&variables, "default     = \"trueflow.dev\"", "terraform apex domain default");
    assert_contains(&variables, "default     = \"www.trueflow.dev\"", "terraform www domain default");
    assert_contains(&variables, "default     = \"us-east-1\"", "terraform region default");
    assert_contains(&outputs, "output \"site_bucket_name\"", "terraform bucket output");
    assert_contains(&outputs, "output \"site_distribution_id\"", "terraform distribution output");
    assert_contains(&readme, "No secrets are stored in this directory", "infra public safety note");
    assert_contains(&readme, "Tofu and Terraform both understand this HCL", "terraform compatibility note");
    assert_contains(&package_built_release, "trueflow-${VERSION}-${TARGET}.tar.gz", "shared packaging artifact name");
    assert_contains(&package_built_release, "trueflow-${VERSION}-SHA256SUMS.txt", "shared packaging checksum name");
    assert_contains(&package_built_release, ".trueflow/release-artifacts", "shared packaging output root");
    assert_contains(&package_macos_release, "TARGET=\"aarch64-apple-darwin\"", "macos packaging target");
    assert_contains(&package_macos_release, "cargo build --release --locked", "macos packaging build command");
    assert_contains(&package_macos_release, "package-built-release.sh", "macos packaging shared packager handoff");
    assert_contains(&package_linux_release, "TARGET=\"x86_64-unknown-linux-musl\"", "linux packaging target");
    assert_contains(&package_linux_release, "nix build --no-link --print-out-paths .#release", "linux packaging nix build command");
    assert_contains(&deploy_public_site, "tofu init", "one-shot deploy tofu init step");
    assert_contains(&deploy_public_site, "tofu fmt -check", "one-shot deploy tofu fmt step");
    assert_contains(&deploy_public_site, "tofu validate", "one-shot deploy tofu validate step");
    assert_contains(&deploy_public_site, "tofu apply", "one-shot deploy tofu apply step");
    assert_contains(&deploy_public_site, "scripts/deploy-website.sh", "one-shot deploy website step");
    assert_contains(&deploy_public_site, "scripts/package-macos-release.sh", "one-shot deploy package step");
    assert_contains(&deploy_public_site, "scripts/deploy-downloads.sh", "one-shot deploy downloads step");
    assert_contains(&deploy_public_site, "--auto-approve", "one-shot deploy auto approve flag");
    assert_contains(&deploy_public_site_fast, "--skip-infra-apply", "fast deploy skips infra apply");
    assert_contains(&deploy_public_site_fast, "deploy-public-site.sh", "fast deploy wrapper target");
    assert_contains(&deploy_website, "TRUEFLOW_INFRA_CLI", "deploy website infra cli override");
    assert_contains(&deploy_downloads, "TRUEFLOW_INFRA_CLI", "deploy downloads infra cli override");
    assert_contains(&gitignore, "infra/terraform/.terraform/", "terraform plugin dir ignore rule");
    assert_contains(&gitignore, "*.tfstate", "terraform state ignore rule");

    Ok(())
}
