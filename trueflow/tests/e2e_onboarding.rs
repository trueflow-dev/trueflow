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

/// Runs `onboard` against a fake Codex CLI that writes `plan` as its final message.
#[cfg(unix)]
fn generate_with_fake_codex(
    repo: &TestRepo,
    behavior: &str,
    extra: &[&str],
) -> Result<std::process::Output> {
    use std::os::unix::fs::PermissionsExt;
    repo.write(
        "trueflow.toml",
        "[scan]\nuse_cache = false\nwrite_cache = false\n[ai]\nprovider = \"codex_cli\"\nmodel = \"fixture-model\"\n",
    )?;
    let page = candidates(repo)?;
    repo.write(
        ".fixture/plan.json",
        &specification(&page["candidates"][0]["target"]).to_string(),
    )?;
    repo.write(
        ".fixture/bin/codex",
        r#"#!/bin/sh
set -eu
/bin/cat > /dev/null
case "$ONBOARD_BEHAVIOR" in
  hang) exec /bin/sleep 30 ;;
  verbose) /usr/bin/yes '{"type":"item.completed","item":{"type":"command_execution","aggregated_output":"candidate page"}}' | /usr/bin/head -n 40000 ;;
esac
while [ "$#" -gt 0 ]; do
  if [ "$1" = --output-last-message ]; then
    shift
    /bin/cat "$ONBOARD_FIXTURE/plan.json" > "$1"
    exit 0
  fi
  shift
done
exit 24
"#,
    )?;
    let directory = repo.path.join(".fixture");
    std::fs::set_permissions(
        directory.join("bin/codex"),
        std::fs::Permissions::from_mode(0o755),
    )?;
    let path = std::env::join_paths([
        directory.join("bin"),
        "/usr/bin".into(),
        "/bin".into(),
    ])?;
    Ok(std::process::Command::new(env!("CARGO_BIN_EXE_trueflow"))
        .args(["onboard", "--json"])
        .args(extra)
        .current_dir(&repo.path)
        .env_clear()
        .env("HOME", &repo.path)
        .env("PATH", path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("ONBOARD_FIXTURE", &directory)
        .env("ONBOARD_BEHAVIOR", behavior)
        .stdin(std::process::Stdio::null())
        .output()?)
}

#[cfg(unix)]
#[test]
fn onboarding_generation_has_a_configurable_agentic_deadline() -> Result<()> {
    let repo = repository("onboarding_generation_deadline")?;
    let started = std::time::Instant::now();
    let output = generate_with_fake_codex(&repo, "hang", &["--timeout", "1"])?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "a hanging provider generated a plan");
    assert!(stderr.contains("timed out"), "{stderr}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "--timeout 1 must bound a provider that sleeps for 30 seconds"
    );
    assert!(
        String::from_utf8_lossy(&repo.run_raw(&["onboard", "--timeout", "0"])?.stderr)
            .contains("--timeout")
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn onboarding_generation_accepts_a_verbose_agent_event_stream() -> Result<()> {
    let repo = repository("onboarding_generation_events")?;
    let output = generate_with_fake_codex(&repo, "verbose", &[])?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let plan = parse_json(&String::from_utf8(output.stdout)?)?;
    assert_eq!(plan["title"], "State transitions");
    Ok(())
}
