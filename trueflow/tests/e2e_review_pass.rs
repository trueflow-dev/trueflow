use anyhow::{Context, Result};
use serde_json::Value;

use trueflow_test_support::*;

fn helpers(suffix: &str) -> String {
    (0..6)
        .map(|n| format!("fn helper_{n}() -> u32 {{\n    {n}{suffix}\n}}\n\n"))
        .collect()
}

/// A commit that edits six trivial helpers and adds one shell-out.
fn changed_repo(name: &str) -> Result<(TestRepo, String)> {
    let repo = TestRepo::new(name)?;
    repo.write(
        "trueflow.toml",
        "[scan]\nuse_cache = false\nwrite_cache = false\n",
    )?;
    repo.write("src/lib.rs", &helpers(""))?;
    repo.commit_all("base")?;
    repo.write(
        "src/lib.rs",
        &format!(
            "{}pub fn run_git(dir: &Path) -> io::Result<Output> {{\n    Command::new(\"git\").current_dir(dir).output()\n}}\n",
            helpers(" + 1")
        ),
    )?;
    repo.commit_all("change helpers and shell out")?;
    let revision = run_git_output(&repo.path, &["rev-parse", "HEAD"])?;
    Ok((repo, format!("rev:{}", revision.trim())))
}

#[test]
fn review_focused_pass_reports_a_budgeted_explained_subset() -> Result<()> {
    let (repo, target) = changed_repo("review_focused_pass_json")?;

    // run_git costs 16s and each one-line helper change 18s: 40s holds two cards.
    let output = repo.run(&[
        "review", "--target", &target, "--pass", "focused", "--budget", "40s", "--json",
    ])?;
    let pass: Value = serde_json::from_str(&output).context("focused pass JSON")?;

    assert_eq!(pass["pass"], "focused");
    assert_eq!(pass["source"], "heuristic");
    assert_eq!(pass["budget_seconds"], 40);
    assert_eq!(pass["remainder"], 5);
    let cards = pass["cards"].as_array().context("cards")?;
    assert_eq!(cards.len(), 2);
    assert_eq!(cards[0]["path"], "src/lib.rs");
    assert!(
        cards[0]["content"]
            .as_str()
            .is_some_and(|content| content.contains("pub fn run_git")),
        "the trust-boundary block must lead the pass: {pass}"
    );
    let why = cards[0]["why"].as_array().context("why")?;
    assert!(
        why.iter().any(|reason| reason
            .as_str()
            .is_some_and(|text| text.starts_with("trust boundary"))),
        "{pass}"
    );
    assert_eq!(cards[0]["estimated_seconds"], 16);
    assert_eq!(pass["estimated_seconds"], 34);
    assert!(
        cards
            .iter()
            .all(|card| card["fingerprint"].as_str().is_some()),
        "cards must name their review fingerprint: {pass}"
    );
    Ok(())
}

#[test]
fn review_full_pass_output_is_unchanged_and_budget_needs_a_focused_pass() -> Result<()> {
    let (repo, target) = changed_repo("review_full_pass_json")?;

    let full = json_array(&repo.run(&["review", "--target", &target, "--json"])?)?;
    let explicit =
        json_array(&repo.run(&["review", "--target", &target, "--pass", "full", "--json"])?)?;
    assert_eq!(full, explicit);
    assert_eq!(
        full[0]["blocks"].as_array().context("blocks")?.len(),
        7,
        "a full review lists every changed block"
    );

    let error = repo.run_err(&["review", "--target", &target, "--budget", "40s"])?;
    assert!(error.contains("--pass"), "{error}");
    Ok(())
}
