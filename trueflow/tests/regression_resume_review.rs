use anyhow::Result;
use trueflow::commands::review::{ReviewRequest, ReviewTarget};
use trueflow_test_support::TestRepo;

#[test]
fn test_resume_diff_from_main_logic() -> Result<()> {
    let repo = TestRepo::new("resume_diff_main_logic")?;

    repo.write("src/lib.rs", "pub fn alpha() {}\n")?;
    repo.commit_all("Initial commit")?;
    checkout_branch(&repo, "feature/changes")?;

    repo.write(
        "src/lib.rs",
        r#"
pub fn beta() {
    println!("beta");
}

pub fn gamma() {
    println!("gamma");
}
"#,
    )?;
    repo.commit_all("Add beta and gamma")?;

    let summary = repo.review_summary(
        ReviewRequest::Targets(vec![ReviewTarget::MainDiff]),
        &[],
        &[],
    )?;

    let beta_hash = summary
        .files
        .iter()
        .flat_map(|file| file.blocks.iter())
        .find(|block| block.content.contains("beta"))
        .expect("Should find beta block")
        .hash
        .to_string();
    assert!(
        summary
            .files
            .iter()
            .flat_map(|file| file.blocks.iter())
            .any(|block| block.content.contains("gamma")),
        "Should find gamma block"
    );

    repo.run(&[
        "mark",
        "--fingerprint",
        beta_hash.as_str(),
        "--verdict",
        "approved",
        "--quiet",
    ])?;

    let summary_after = repo.review_summary(
        ReviewRequest::Targets(vec![ReviewTarget::MainDiff]),
        &[],
        &[],
    )?;
    let remaining_blocks = summary_after
        .files
        .iter()
        .flat_map(|file| file.blocks.iter())
        .collect::<Vec<_>>();

    assert!(
        !remaining_blocks
            .iter()
            .any(|block| block.content.contains("beta")),
        "Beta block should be approved and not show up"
    );
    assert!(
        remaining_blocks
            .iter()
            .any(|block| block.content.contains("gamma")),
        "Gamma block should still be unreviewed"
    );

    Ok(())
}

fn checkout_branch(repo: &TestRepo, branch: &str) -> Result<()> {
    repo.git(&["checkout", "-b", branch])
}
