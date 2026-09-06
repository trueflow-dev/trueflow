use anyhow::{Context, Result};
use serde_json::{Value, json};
use trueflow_test_support::{TestRepo, json as parse_json};

fn repository(name: &str) -> Result<TestRepo> {
    let repo = TestRepo::new(name)?;
    repo.write("src/lib.rs", "pub struct State { pub count: usize }\n\npub fn advance(state: &mut State) { state.count += 1; }\n")?;
    Ok(repo)
}

fn candidates(repo: &TestRepo) -> Result<Value> {
    parse_json(&repo.run(&["onboard", "--candidates"])?)
}

fn specification(target: &Value) -> Value {
    json!({
        "version": 1,
        "title": "State transitions",
        "description": "Review the central state and its transitions.",
        "steps": [{"target": target, "description": "Check the state invariant."}]
    })
}

fn save_plan(repo: &TestRepo, plan: &Value) -> Result<()> {
    repo.write(".trueflow/onboarding.json", &serde_json::to_string(plan)?)
}

#[test]
fn candidates_exclude_approved_content_and_include_changed_content() -> Result<()> {
    let repo = repository("onboarding_reviewed_content")?;
    let before = candidates(&repo)?;
    let first = &before["candidates"][0];
    let hash = first["fingerprint"].as_str().context("fingerprint")?;
    let path = first["target"]["path"].as_str().context("path")?;
    repo.run(&[
        "mark",
        "--fingerprint",
        hash,
        "--path",
        path,
        "--verdict",
        "approved",
        "--quiet",
    ])?;
    let after = candidates(&repo)?;
    assert!(
        !after["candidates"]
            .as_array()
            .context("candidates")?
            .iter()
            .any(|candidate| candidate["target"] == first["target"])
    );
    repo.write("src/lib.rs", "pub struct State { pub count: u64 }\n\npub fn advance(state: &mut State) { state.count += 1; }\n")?;
    let changed = candidates(&repo)?;
    assert!(
        changed["candidates"]
            .as_array()
            .context("candidates")?
            .iter()
            .any(|candidate| candidate["target"]["start_line"] == first["target"]["start_line"])
    );
    Ok(())
}

#[test]
fn portable_plan_rejects_stale_source_and_duplicate_targets() -> Result<()> {
    let repo = repository("onboarding_stale_plan")?;
    let page = candidates(&repo)?;
    let plan = specification(&page["candidates"][0]["target"]);
    save_plan(&repo, &plan)?;
    repo.run(&["onboard", "--plan", ".trueflow/onboarding.json", "--json"])?;
    let mut duplicate = plan.clone();
    duplicate["steps"]
        .as_array_mut()
        .context("steps")?
        .push(plan["steps"][0].clone());
    save_plan(&repo, &duplicate)?;
    assert!(
        repo.run_err(&["onboard", "--plan", ".trueflow/onboarding.json", "--json"])?
            .contains("duplicate")
    );
    save_plan(&repo, &plan)?;
    repo.write("src/lib.rs", "pub struct State { pub count: u64 }\n")?;
    assert!(
        repo.run_err(&["onboard", "--plan", ".trueflow/onboarding.json", "--json"])?
            .contains("stale")
    );
    Ok(())
}

#[test]
fn portable_plan_enforces_card_and_reading_budgets() -> Result<()> {
    let repo = repository("onboarding_budgets")?;
    let page = candidates(&repo)?;
    let mut plan = specification(&page["candidates"][0]["target"]);
    plan["steps"] = Value::Array(vec![plan["steps"][0].clone(); 65]);
    save_plan(&repo, &plan)?;
    assert!(
        repo.run_err(&["onboard", "--plan", ".trueflow/onboarding.json", "--json"])?
            .contains("64")
    );
    let mut source = String::from("pub fn long_operation() {\n");
    for index in 0..450 {
        source.push_str(&format!("    let _value_{index} = {index};\n"));
    }
    source.push_str("}\n");
    repo.write("src/lib.rs", &source)?;
    let large = candidates(&repo)?;
    let plan = specification(&large["candidates"][0]["target"]);
    save_plan(&repo, &plan)?;
    assert!(
        repo.run_err(&["onboard", "--plan", ".trueflow/onboarding.json", "--json"])?
            .contains("reading budget")
    );
    Ok(())
}

#[test]
fn candidate_pagination_and_subdirectory_queries_are_repository_scoped() -> Result<()> {
    let repo = repository("onboarding_pagination")?;
    repo.write("other.rs", "pub enum Outcome { Done, Pending }\n")?;
    let all = candidates(&repo)?;
    let mut paged = Vec::new();
    let total = all["total_candidates"].as_u64().context("total")?;
    for offset in 0..total {
        let page = parse_json(&repo.run(&[
            "onboard",
            "--candidates",
            "--limit",
            "1",
            "--offset",
            &offset.to_string(),
        ])?)?;
        paged.push(page["candidates"][0].clone());
    }
    assert_eq!(Value::Array(paged), all["candidates"]);
    let nested = parse_json(&repo.run_in(&["onboard", "--candidates"], &repo.path.join("src"))?)?;
    assert_eq!(nested, all);
    Ok(())
}
