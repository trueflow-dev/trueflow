use anyhow::{Context, Result};
use serde_json::{Value, json};
use trueflow_test_support::{TestRepo, json as parse_json};

fn repository(name: &str) -> Result<TestRepo> {
    let repo = TestRepo::new(name)?;
    repo.write(
        "trueflow.toml",
        "[scan]\nuse_cache = false\nwrite_cache = false\n",
    )?;
    repo.write("src/lib.rs", "pub struct Account { pub balance: u64 }\n\npub fn transfer(a: &mut Account, b: &mut Account, n: u64) {\n    a.balance -= n;\n    b.balance += n;\n}\n")?;
    Ok(repo)
}

fn inventory(repo: &TestRepo) -> Result<Value> {
    parse_json(&repo.run(&["focus", "--candidates", "--path", "src"])?)
}

fn plan(candidate: &Value, seconds: usize) -> Value {
    json!({
        "version": 1,
        "duration_seconds": seconds,
        "title": "Money movement",
        "description": "Inspect account arithmetic, not a security certification.",
        "steps": [{
            "target": candidate["target"],
            "reason": "Check the money movement invariant.",
            "evidence": [candidate["evidence"][0]["id"]],
            "uncertainty": "Caller preconditions have not been established.",
            "checks": ["Can balances underflow or overflow?"]
        }]
    })
}

#[test]
fn focus_resolves_grounded_plans_and_rejects_invented_or_stale_evidence() -> Result<()> {
    let repo = repository("focus_grounding")?;
    let candidates = inventory(&repo)?;
    let candidate = &candidates["candidates"][0];
    let mut proposed = plan(candidate, 300);
    repo.write("plan.json", &proposed.to_string())?;
    let valid = parse_json(&repo.run(&["focus", "--plan", "plan.json", "--json"])?)?;
    assert_eq!(valid["steps"][0]["target"], candidate["target"]);
    assert!(!repo.path.join(".trueflow/reviews.jsonl").exists());
    proposed["steps"][0]["evidence"] = json!(["invented"]);
    repo.write("plan.json", &proposed.to_string())?;
    assert!(
        repo.run_err(&["focus", "--plan", "plan.json", "--json"])?
            .contains("evidence")
    );
    repo.write("plan.json", &plan(candidate, 300).to_string())?;
    repo.write("src/lib.rs", "pub struct Account { pub balance: u128 }\n")?;
    assert!(
        repo.run_err(&["focus", "--plan", "plan.json", "--json"])?
            .contains("stale")
    );
    Ok(())
}

#[test]
fn focus_enforces_requested_budget_before_opening_or_generating() -> Result<()> {
    let repo = repository("focus_duration")?;
    let mut source = String::from("pub fn inspect_me() {\n");
    for n in 0..160 {
        source.push_str(&format!("    consume({n});\n"));
    }
    source.push_str("}\n");
    repo.write("src/lib.rs", &source)?;
    let page = inventory(&repo)?;
    let candidate = &page["candidates"][0];
    assert!(
        candidate["estimated_seconds"]
            .as_u64()
            .context("estimate")?
            > 300
    );
    repo.write("plan.json", &plan(candidate, 300).to_string())?;
    assert!(
        repo.run_err(&["focus", "--plan", "plan.json", "--json"])?
            .contains("budget")
    );
    repo.write("plan.json", &plan(candidate, 600).to_string())?;
    repo.run(&["focus", "--plan", "plan.json", "--json"])?;
    assert!(
        repo.run_err(&["focus", "--plan", "plan.json", "--duration", "5m", "--json"])?
            .contains("duration")
    );
    assert!(repo.run_err(&["focus", "--json"])?.contains("--duration"));
    assert!(
        repo.run_err(&["focus", "--duration", "0m", "--json"])?
            .contains("duration")
    );
    Ok(())
}

#[test]
fn focus_loaded_plan_resumes_without_approving_unselected_content() -> Result<()> {
    let repo = repository("focus_resume")?;
    repo.write("src/other.rs", "pub fn untouched() -> u64 { 42 }\n")?;
    let page = inventory(&repo)?;
    let candidate = &page["candidates"][0];
    repo.write("plan.json", &plan(candidate, 300).to_string())?;
    let onboard = parse_json(&repo.run(&["onboard", "--candidates", "--path", "src"])?)?;
    let same = onboard["candidates"]
        .as_array()
        .context("candidates")?
        .iter()
        .find(|other| other["target"] == candidate["target"])
        .context("same target")?;
    repo.run(&[
        "mark",
        "--fingerprint",
        same["fingerprint"].as_str().context("fingerprint")?,
        "--path",
        "src/lib.rs",
        "--verdict",
        "approved",
        "--quiet",
    ])?;
    repo.run(&["focus", "--plan", "plan.json", "--json"])?;
    let pending = inventory(&repo)?;
    assert!(
        !pending["candidates"]
            .as_array()
            .context("pending")?
            .iter()
            .any(|c| c["target"] == candidate["target"])
    );
    assert!(
        pending["candidates"]
            .as_array()
            .context("pending")?
            .iter()
            .any(|c| c["target"] == page["candidates"][1]["target"])
    );
    Ok(())
}

fn evidence_id<'a>(candidate: &'a Value, kind: &str) -> Result<&'a Value> {
    candidate["evidence"]
        .as_array()
        .context("evidence")?
        .iter()
        .find(|evidence| evidence["kind"] == kind)
        .map(|evidence| &evidence["id"])
        .with_context(|| format!("{kind} evidence"))
}

#[test]
fn focus_plan_reopens_when_unplanned_paths_exceed_the_inventory_cap() -> Result<()> {
    let repo = repository("focus_plan_scope")?;
    let page = inventory(&repo)?;
    repo.write("plan.json", &plan(&page["candidates"][0], 300).to_string())?;
    // More unreviewed leaf blocks outside the plan than one inventory may hold.
    for file in 0..5 {
        let source: String = (0..4001)
            .map(|n| format!("fn generated_{file}_{n}() {{}}\n"))
            .collect();
        repo.write(&format!("generated/part{file}.rs"), &source)?;
    }
    assert!(
        repo.run_err(&["focus", "--candidates"])?
            .contains("narrow the path scope")
    );
    let reopened = parse_json(&repo.run(&["focus", "--plan", "plan.json", "--json"])?)?;
    assert_eq!(
        reopened["steps"][0]["target"],
        page["candidates"][0]["target"]
    );
    Ok(())
}

#[test]
fn focus_plan_survives_comments_and_unrelated_commits() -> Result<()> {
    let repo = repository("focus_plan_mutable_evidence")?;
    repo.commit_all("initial")?;
    let page = inventory(&repo)?;
    let candidate = &page["candidates"][0];
    let mut cited = plan(candidate, 300);
    cited["steps"][0]["evidence"] = json!([
        evidence_id(candidate, "human_review")?,
        evidence_id(candidate, "history_status")?
    ]);
    repo.write("plan.json", &cited.to_string())?;
    repo.run(&["focus", "--plan", "plan.json", "--json"])?;
    let onboard = parse_json(&repo.run(&["onboard", "--candidates", "--path", "src"])?)?;
    let same = onboard["candidates"]
        .as_array()
        .context("candidates")?
        .iter()
        .find(|other| other["target"] == candidate["target"])
        .context("same target")?;
    repo.run(&[
        "mark",
        "--fingerprint",
        same["fingerprint"].as_str().context("fingerprint")?,
        "--path",
        "src/lib.rs",
        "--verdict",
        "comment",
        "--note",
        "Check the underflow before release.",
        "--quiet",
    ])?;
    repo.write("NOTES.txt", "unrelated\n")?;
    repo.commit_all("unrelated change")?;
    let pending = inventory(&repo)?;
    let current = pending["candidates"]
        .as_array()
        .context("pending")?
        .iter()
        .find(|c| c["target"] == candidate["target"])
        .context("commented card stays pending")?;
    assert_ne!(
        current["evidence"], candidate["evidence"],
        "the cited observations must have changed for this regression to apply"
    );
    repo.run(&["focus", "--plan", "plan.json", "--json"])?;
    cited["steps"][0]["evidence"] = json!(["ev-invented"]);
    repo.write("plan.json", &cited.to_string())?;
    assert!(
        repo.run_err(&["focus", "--plan", "plan.json", "--json"])?
            .contains("invented")
    );
    Ok(())
}

#[test]
fn focus_reports_a_path_that_matches_nothing_instead_of_a_budget() -> Result<()> {
    let repo = repository("focus_missing_scope")?;
    for args in [
        &["focus", "--candidates", "--path", "srcc"][..],
        &["focus", "--duration", "60m", "--path", "srcc", "--json"][..],
    ] {
        let error = repo.run_err(args)?;
        assert!(error.contains("srcc"), "{args:?}: {error}");
        assert!(!error.contains("budget"), "{args:?}: {error}");
    }
    Ok(())
}
