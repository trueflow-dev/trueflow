use anyhow::{Context, Result};
use serde_json::Value;

use trueflow_test_support::{TestRepo, first_block_hash, json_array};

fn mark(repo: &TestRepo, hash: &str) -> Result<()> {
    repo.run(&[
        "mark",
        "--fingerprint",
        hash,
        "--verdict",
        "approved",
        "--quiet",
    ])?;
    Ok(())
}

fn is_gap(sub_block: &Value) -> bool {
    sub_block["kind"]
        .as_str()
        .is_some_and(|kind| kind.eq_ignore_ascii_case("gap"))
}

struct GeneratedReviewUnitFixture {
    repo: TestRepo,
    parent_hash: String,
    duplicate_hash: String,
    duplicate_units: Vec<Value>,
    unique_hashes: Vec<String>,
    unique_hash: String,
}

fn mark_at_path(repo: &TestRepo, hash: &str, line: Option<u64>) -> Result<()> {
    let mut args = vec![
        "mark",
        "--fingerprint",
        hash,
        "--verdict",
        "approved",
        "--path",
        "README.md",
    ];
    let line = line.map(|line| line.to_string());
    if let Some(line) = &line {
        args.extend(["--line", line]);
    }
    args.push("--quiet");
    repo.run(&args)?;
    Ok(())
}

fn generated_review_unit_fixture(name: &str) -> Result<GeneratedReviewUnitFixture> {
    let repo = TestRepo::new(name)?;
    repo.write("README.md", "Seed.\n")?;
    repo.commit_all("Seed README")?;

    const DUPLICATE_SENTENCE: &str = "Duplicate generated review unit.\n";
    let mut source = String::new();
    for line in 0..55 {
        if matches!(line, 7 | 47) {
            source.push_str(DUPLICATE_SENTENCE);
        } else {
            source.push_str(&format!("Unique generated review sentence {line}.\n"));
        }
    }
    repo.write("README.md", &source)?;

    let scan_output = repo.run(&["scan", "--json"])?;
    let files = json_array(&scan_output)?;
    let blocks = files
        .first()
        .and_then(|file| file["blocks"].as_array())
        .context("README scan result is missing blocks")?;
    assert_eq!(
        blocks.len(),
        1,
        "fixture must scan to one parent tree block"
    );
    let parent_hash = first_block_hash(&scan_output)?;

    let split_output = repo.run(&["inspect", "--fingerprint", &parent_hash, "--split"])?;
    let sub_blocks = json_array(&split_output)?;
    assert!(
        sub_blocks.len() > 1,
        "fixture must produce generated review units"
    );

    let duplicate_units = sub_blocks
        .iter()
        .filter(|block| block["content"].as_str() == Some(DUPLICATE_SENTENCE))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        duplicate_units.len(),
        2,
        "fixture must contain exactly two duplicate generated review units"
    );
    let duplicate_hash = duplicate_units[0]["hash"]
        .as_str()
        .context("duplicate generated review unit is missing a hash")?
        .to_string();
    assert!(
        duplicate_units
            .iter()
            .all(|block| block["hash"].as_str() == Some(&duplicate_hash)),
        "duplicate generated review units must share a hash"
    );

    let first = &duplicate_units[0];
    let second = &duplicate_units[1];
    assert_ne!(
        first["start_line"], second["start_line"],
        "duplicate generated review units must be on distinct lines"
    );
    assert_ne!(
        (first["start_byte"].clone(), first["end_byte"].clone()),
        (second["start_byte"].clone(), second["end_byte"].clone()),
        "duplicate generated review units must have distinct byte spans"
    );

    let unique_hashes = sub_blocks
        .iter()
        .filter(|block| !is_gap(block))
        .filter_map(|block| block["hash"].as_str())
        .filter(|hash| *hash != duplicate_hash)
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert!(
        !unique_hashes.is_empty(),
        "fixture must provide a unique generated review-unit control"
    );
    let unique_hash = unique_hashes[0].clone();
    assert_eq!(
        sub_blocks
            .iter()
            .filter(|block| block["hash"].as_str() == Some(&unique_hash))
            .count(),
        1,
        "unique generated review-unit control must be globally unique"
    );

    Ok(GeneratedReviewUnitFixture {
        repo,
        parent_hash,
        duplicate_hash,
        duplicate_units,
        unique_hashes,
        unique_hash,
    })
}

fn inspect_split_coverage(repo: &TestRepo, parent_hash: &str) -> Result<Vec<Value>> {
    let output = repo.run(&[
        "inspect",
        "--fingerprint",
        parent_hash,
        "--split",
        "--coverage",
    ])?;
    json_array(&output)
}

fn coverage_for_block<'a>(inspected: &'a [Value], block: &Value) -> Result<&'a Value> {
    let start_byte = block["start_byte"]
        .as_u64()
        .context("generated review unit is missing start_byte")?;
    let end_byte = block["end_byte"]
        .as_u64()
        .context("generated review unit is missing end_byte")?;
    inspected
        .iter()
        .find(|entry| {
            entry["block"]["start_byte"].as_u64() == Some(start_byte)
                && entry["block"]["end_byte"].as_u64() == Some(end_byte)
        })
        .map(|entry| &entry["coverage"])
        .context("inspect coverage output is missing generated review unit")
}

fn review_with_hash<'a>(reviews: &'a [Value], hash: &str) -> Option<&'a Value> {
    reviews
        .iter()
        .find(|review| review["record"]["target"]["hash"].as_str() == Some(hash))
}

fn assert_ambiguous_duplicate_coverage(
    inspected: &[Value],
    fixture: &GeneratedReviewUnitFixture,
) -> Result<()> {
    for duplicate in &fixture.duplicate_units {
        let coverage = coverage_for_block(inspected, duplicate)?;
        let direct = coverage["direct_reviews"]
            .as_array()
            .context("inspect coverage is missing direct reviews")?;
        assert!(
            direct.is_empty(),
            "ambiguous generated review units must receive no direct reviews"
        );
        assert!(
            review_with_hash(direct, &fixture.duplicate_hash).is_none(),
            "ambiguous coarse approval must not become a direct review"
        );
        assert_eq!(
            coverage["checks"]["review"]["direct_latest_verdict"],
            Value::Null,
            "ambiguous coarse approval must not set a direct verdict"
        );
        assert_eq!(
            coverage["checks"]["review"]["direct_identity_count"].as_u64(),
            Some(0),
            "ambiguous coarse approval must not contribute a direct identity"
        );

        let linked = coverage["linked_reviews"]
            .as_array()
            .context("inspect coverage is missing linked reviews")?;
        assert_eq!(
            linked.len(),
            1,
            "ambiguous generated review units must expose only the coarse record as linked"
        );
        let linked_record = review_with_hash(linked, &fixture.duplicate_hash)
            .context("ambiguous coarse approval must remain linked to every candidate")?;
        assert_eq!(
            linked_record["binding_relation"],
            Value::Null,
            "ambiguous coarse approval must have no binding relation"
        );

        let diagnostics = coverage["diagnostics"]
            .as_array()
            .context("inspect coverage is missing diagnostics")?;
        let ambiguity = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.get("AmbiguousRecord").is_some())
            .context("ambiguous coarse approval must expose an ambiguity diagnostic")?;
        let candidates = ambiguity["AmbiguousRecord"]["candidates"]
            .as_array()
            .context("ambiguity diagnostic is missing complete block candidates")?;
        assert_eq!(
            candidates.len(),
            fixture.duplicate_units.len(),
            "ambiguity diagnostic must list every duplicate candidate exactly once"
        );
        for location in &fixture.duplicate_units {
            let candidate = candidates
                .iter()
                .find(|candidate| {
                    candidate["kind"].as_str() == Some("block")
                        && candidate["path"].as_str() == Some("README.md")
                        && candidate["hash"].as_str() == Some(&fixture.duplicate_hash)
                        && candidate["start_byte"] == location["start_byte"]
                        && candidate["end_byte"] == location["end_byte"]
                })
                .context("ambiguity diagnostic is missing a complete duplicate location")?;
            for field in ["start_line", "end_line", "start_byte", "end_byte"] {
                assert_eq!(
                    candidate[field], location[field],
                    "ambiguity diagnostic must retain duplicate {field}"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn test_small_block_approval_marks_the_parent_review_unit() -> Result<()> {
    let repo = TestRepo::new("subblock_implicit")?;
    repo.write("main.rs", "fn seed() {}\n")?;
    repo.commit_all("Seed main")?;
    repo.write("main.rs", "fn main() {\n    part1();\n\n    part2();\n}")?;

    let output = repo.run(&["scan", "--json"])?;
    let parent_hash = first_block_hash(&output)?;

    let output = repo.run(&["inspect", "--fingerprint", &parent_hash, "--split"])?;
    let sub_blocks = json_array(&output)?;
    assert_eq!(sub_blocks.len(), 1);

    let first_hash = sub_blocks
        .first()
        .and_then(|sb| sb["hash"].as_str())
        .context("first sub-block is missing a hash")?;
    assert_eq!(first_hash, parent_hash);

    let output = repo.run(&["review", "--exclude", "Gap", "--exclude", "gap"])?;
    assert!(output.contains("[Unreviewed]"));
    assert!(output.contains(&parent_hash));

    mark(&repo, first_hash)?;

    let output = repo.run(&["review", "--exclude", "Gap", "--exclude", "gap"])?;
    assert!(output.contains("All clear"));

    let output = repo.run(&["check"])?;
    assert!(
        output.trim().is_empty(),
        "Expected check to be silent on stdout"
    );

    Ok(())
}

#[test]
fn test_markdown_implicit_approval() -> Result<()> {
    let repo = TestRepo::new("subblock_markdown")?;
    repo.write("README.md", "# Seed\n")?;
    repo.commit_all("Seed main")?;
    repo.write("README.md", "# Title\n\nPara one.\n\nPara two.\n")?;

    let output = repo.run(&["scan", "--json"])?;
    let parent_hash = first_block_hash(&output)?;

    let output = repo.run(&["inspect", "--fingerprint", &parent_hash, "--split"])?;
    let sub_blocks = json_array(&output)?;

    let output = repo.run(&["review", "--all", "--exclude", "gap"])?;
    assert!(output.contains("[Unreviewed]"));
    assert!(output.contains(&parent_hash));

    for sb in &sub_blocks {
        if is_gap(sb) {
            continue;
        }
        let hash = sb["hash"].as_str().context("sub-block is missing a hash")?;
        mark(&repo, hash)?;
    }

    let output = repo.run(&["review", "--all", "--exclude", "gap"])?;
    assert!(output.contains("All clear"));

    let output = repo.run(&["check"])?;
    assert!(
        output.trim().is_empty(),
        "Expected check to be silent on stdout"
    );

    Ok(())
}

#[test]
fn test_hash_only_approval_does_not_cover_duplicate_generated_review_units() -> Result<()> {
    let fixture = generated_review_unit_fixture("subblock_generated_hash_ambiguity")?;

    for hash in &fixture.unique_hashes {
        mark(&fixture.repo, hash)?;
    }
    mark(&fixture.repo, &fixture.duplicate_hash)?;

    let output = fixture.repo.run(&["review", "--all"])?;
    assert!(
        output.contains("[Unreviewed]") && output.contains(&fixture.parent_hash),
        "a coarse duplicate approval must leave the parent unreviewed: {output}"
    );

    let inspected = inspect_split_coverage(&fixture.repo, &fixture.parent_hash)?;
    assert_ambiguous_duplicate_coverage(&inspected, &fixture)?;

    let unique_coverage = inspected
        .iter()
        .find(|entry| entry["block"]["hash"].as_str() == Some(&fixture.unique_hash))
        .map(|entry| &entry["coverage"])
        .context("inspect coverage output is missing the unique generated review-unit control")?;
    let unique_direct = unique_coverage["direct_reviews"]
        .as_array()
        .context("unique generated review-unit control is missing direct reviews")?;
    let unique_record = review_with_hash(unique_direct, &fixture.unique_hash)
        .context("unique generated review-unit hash-only approval must be direct")?;
    assert_eq!(
        unique_record["binding_relation"].as_str(),
        Some("HashOnly"),
        "unique generated review-unit hash-only approval must retain HashOnly relation"
    );
    assert_eq!(
        unique_coverage["checks"]["review"]["direct_latest_verdict"].as_str(),
        Some("approved"),
        "unique generated review-unit hash-only approval must directly approve"
    );
    assert!(
        unique_coverage["diagnostics"]
            .as_array()
            .context("unique generated review-unit control is missing diagnostics")?
            .is_empty(),
        "unique generated review-unit hash-only approval must not be ambiguous"
    );

    let first_line = fixture.duplicate_units[0]["start_line"]
        .as_u64()
        .context("first duplicate generated review unit is missing start_line")?;
    let second_line = fixture.duplicate_units[1]["start_line"]
        .as_u64()
        .context("second duplicate generated review unit is missing start_line")?;
    mark_at_path(&fixture.repo, &fixture.duplicate_hash, Some(first_line))?;

    let inspected = inspect_split_coverage(&fixture.repo, &fixture.parent_hash)?;
    let first_coverage = coverage_for_block(&inspected, &fixture.duplicate_units[0])?;
    let first_direct = first_coverage["direct_reviews"]
        .as_array()
        .context("first duplicate coverage is missing direct reviews")?;
    let first_exact = review_with_hash(first_direct, &fixture.duplicate_hash)
        .context("exact persisted location approval must directly approve its duplicate")?;
    assert_eq!(
        first_exact["binding_relation"].as_str(),
        Some("Exact"),
        "distinct-line persisted location approval must bind exactly"
    );
    assert_eq!(
        first_coverage["checks"]["review"]["direct_latest_verdict"].as_str(),
        Some("approved"),
        "distinct-line persisted location approval must directly approve its target"
    );
    let second_coverage = coverage_for_block(&inspected, &fixture.duplicate_units[1])?;
    assert!(
        review_with_hash(
            second_coverage["direct_reviews"]
                .as_array()
                .context("second duplicate coverage is missing direct reviews")?,
            &fixture.duplicate_hash,
        )
        .is_none(),
        "first exact duplicate approval must not directly approve its sibling"
    );

    let output = fixture.repo.run(&["review", "--all"])?;
    assert!(
        output.contains("[Unreviewed]") && output.contains(&fixture.parent_hash),
        "one exact duplicate approval must still leave the parent unreviewed: {output}"
    );

    mark_at_path(&fixture.repo, &fixture.duplicate_hash, Some(second_line))?;
    let output = fixture.repo.run(&["review", "--all"])?;
    assert!(
        output.contains("All clear"),
        "all unique and exact duplicate approvals must clear the parent: {output}"
    );

    Ok(())
}
