#![cfg(unix)]

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use trueflow_test_support::{TestRepo, json as parse_json};

const SOURCE: &str = "pub fn debit(balance: &mut u64, amount: u64) { *balance -= amount; }\n\npub fn credit(balance: &mut u64, amount: u64) { *balance += amount; }\n";

fn fixture(provider: &str) -> Result<(TestRepo, Value)> {
    let repo = TestRepo::new("focus_generation")?;
    repo.write("trueflow.toml", &format!("[scan]\nuse_cache = false\nwrite_cache = false\n[ai]\nprovider = \"{provider}_cli\"\nmodel = \"fixture-model\"\n"))?;
    repo.write("src/lib.rs", SOURCE)?;
    let inventory = parse_json(&repo.run_with_env(
        &["focus", "--candidates", "--path", "src"],
        &[("HOME", repo.path.to_str().context("home path")?)],
    )?)?;
    let candidates = inventory["candidates"].as_array().context("candidates")?;
    let steps: Vec<_> = candidates
        .iter()
        .rev()
        .take(2)
        .map(|candidate| {
            json!({
                "target": candidate["target"],
                "reason": "Inspect balance arithmetic at the public boundary.",
                "evidence": [candidate["evidence"][0]["id"]],
                "uncertainty": "Caller preconditions are not established.",
                "checks": ["Can this balance operation overflow or underflow?"]
            })
        })
        .collect();
    assert_eq!(
        steps.len(),
        2,
        "fixture must supply two distinct complete targets"
    );
    let plan = json!({"version": 1, "duration_seconds": 300, "title": "Balance arithmetic", "description": "A focused inspection, not a security certification.", "steps": steps});
    repo.write(".fixture/plan.json", &plan.to_string())?;
    repo.write(
        ".fixture/claude.json",
        &json!({"result": plan.to_string()}).to_string(),
    )?;
    // Only these local executables can satisfy the explicitly configured provider.
    // Capture stdin before signalling readiness so drift occurs after inventory capture.
    let script = r#"#!/bin/sh
set -eu
printf '%s' "$$" > "$FOCUS_FIXTURE/provider.pid"
/bin/cat > "$FOCUS_FIXTURE/prompt"
: > "$FOCUS_FIXTURE/ready"
case "$FOCUS_BEHAVIOR" in
  fail) printf 'fixture provider failed\n' >&2; exit 23 ;;
  hang) exec /bin/sleep 30 ;;
  wait) while [ ! -f "$FOCUS_FIXTURE/release" ]; do /bin/sleep 0.01; done ;;
esac
if [ "$FOCUS_PROVIDER" = claude ]; then
  /bin/cat "$FOCUS_FIXTURE/claude.json"
else
  while [ "$#" -gt 0 ]; do
    if [ "$1" = --output-last-message ]; then
      shift
      /bin/cat "$FOCUS_FIXTURE/plan.json" > "$1"
      exit 0
    fi
    shift
  done
  printf 'missing Codex final-message destination\n' >&2
  exit 24
fi
"#;
    for binary in ["claude", "codex"] {
        repo.write(&format!(".fixture/bin/{binary}"), script)?;
        fs::set_permissions(
            repo.path.join(format!(".fixture/bin/{binary}")),
            fs::Permissions::from_mode(0o755),
        )?;
    }
    Ok((repo, plan))
}

struct Generation {
    child: Child,
    directory: PathBuf,
}

impl Generation {
    fn spawn(repo: &TestRepo, provider: &str, behavior: &str, timeout: &str) -> Result<Self> {
        let directory = repo.path.join(".fixture");
        let path = std::env::join_paths([
            directory.join("bin"),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
        ])?;
        let child = Command::new(env!("CARGO_BIN_EXE_trueflow"))
            .args([
                "focus",
                "--duration",
                "5m",
                "--path",
                "src",
                "--json",
                "--timeout",
                timeout,
            ])
            .current_dir(&repo.path)
            .env_clear()
            .env("HOME", &repo.path)
            .env("PATH", path)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("FOCUS_FIXTURE", &directory)
            .env("FOCUS_PROVIDER", provider)
            .env("FOCUS_BEHAVIOR", behavior)
            .stdin(Stdio::null())
            .stdout(File::create(directory.join("stdout"))?)
            .stderr(File::create(directory.join("stderr"))?)
            .spawn()?;
        Ok(Self { child, directory })
    }

    fn ready(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.directory.join("ready").exists() {
            if let Some(status) = self.child.try_wait()? {
                bail!(
                    "focus exited before provider consumed evidence: {status}: {}",
                    self.stderr()?
                );
            }
            if Instant::now() >= deadline {
                bail!(
                    "provider did not consume evidence before deadline: {}",
                    self.stderr()?
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<(ExitStatus, String, String)> {
        // Bounds a hung test only; callers assert their own timing expectations.
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok((
                    status,
                    fs::read_to_string(self.directory.join("stdout"))?,
                    self.stderr()?,
                ));
            }
            if Instant::now() >= deadline {
                bail!(
                    "focus subprocess exceeded outer cleanup deadline: {}",
                    self.stderr()?
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn stderr(&self) -> Result<String> {
        Ok(fs::read_to_string(self.directory.join("stderr"))?)
    }
}

impl Drop for Generation {
    fn drop(&mut self) {
        // The production launcher creates a provider process group. Kill only the
        // PID recorded by this isolated fake, including any sleep holding a pipe.
        if let Ok(pid) = fs::read_to_string(self.directory.join("provider.pid"))
            && let Ok(pid) = pid.parse::<u32>()
            && pid > 1
        {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{pid}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn records(repo: &TestRepo) -> Result<Vec<u8>> {
    let path = repo.path.join(".trueflow/reviews.jsonl");
    Ok(if path.exists() {
        fs::read(path)?
    } else {
        Vec::new()
    })
}

fn save_response(repo: &TestRepo, response: &str) -> Result<()> {
    repo.write(".fixture/plan.json", response)?;
    repo.write(
        ".fixture/claude.json",
        &json!({"result": response}).to_string(),
    )
}

#[test]
fn focus_generation_uses_supplied_evidence_and_returns_a_portable_validated_plan() -> Result<()> {
    for provider in ["claude", "codex"] {
        let (repo, expected) = fixture(provider)?;
        let mut generation = Generation::spawn(&repo, provider, "valid", "5")?;
        let (status, stdout, stderr) = generation.finish()?;
        assert!(status.success(), "{provider}: {stderr}");
        let plan = parse_json(&stdout)?;
        assert_eq!(
            plan, expected,
            "selection order and source-pinned evidence must survive validation"
        );
        let prompt = fs::read_to_string(repo.path.join(".fixture/prompt"))?;
        let supplied: Vec<Value> = prompt
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|value| value.get("target").is_some())
            .collect();
        for step in plan["steps"].as_array().context("steps")? {
            let candidate = supplied
                .iter()
                .find(|candidate| candidate["target"] == step["target"])
                .context(
                    "selected target must have been supplied to the actual provider process",
                )?;
            assert!(
                candidate["evidence"]
                    .as_array()
                    .context("evidence")?
                    .iter()
                    .any(|evidence| evidence["id"] == step["evidence"][0])
            );
            assert!(
                candidate["excerpt"]
                    .as_str()
                    .context("source excerpt")?
                    .contains("pub fn")
            );
        }
        repo.write("portable.json", &stdout)?;
        repo.run_with_env(
            &["focus", "--plan", "portable.json", "--json"],
            &[("HOME", repo.path.to_str().context("home")?)],
        )?;
        assert!(
            records(&repo)?.is_empty(),
            "generation must never approve code"
        );
        assert_eq!(fs::read_to_string(repo.path.join("src/lib.rs"))?, SOURCE);
    }
    Ok(())
}

#[test]
fn focus_generation_rejects_provider_errors_invalid_json_and_invented_evidence_without_records()
-> Result<()> {
    for provider in ["claude", "codex"] {
        for failure in ["fail", "invalid", "unknown"] {
            let (repo, mut proposed) = fixture(provider)?;
            match failure {
                "invalid" => save_response(&repo, "not a JSON specification")?,
                "unknown" => {
                    proposed["steps"][0]["evidence"] = json!(["invented-evidence"]);
                    save_response(&repo, &proposed.to_string())?;
                }
                _ => {}
            }
            // These providers fail or answer at once; the deadline only bounds a hang.
            let mut generation = Generation::spawn(&repo, provider, failure, "60")?;
            let (status, stdout, stderr) = generation.finish()?;
            assert!(
                !status.success(),
                "{provider}/{failure} unexpectedly generated {stdout}"
            );
            assert!(
                stdout.trim().is_empty(),
                "failed generation leaked a partial plan: {stdout}"
            );
            let diagnostic = match failure {
                "fail" => "fixture provider failed",
                "invalid" => "JSON",
                _ => "evidence",
            };
            assert!(
                stderr.contains(diagnostic),
                "{provider}/{failure}: {stderr}"
            );
            assert!(
                repo.path.join(".fixture/prompt").exists(),
                "must exercise the provider, not fail availability checks"
            );
            assert!(records(&repo)?.is_empty());
        }
    }
    Ok(())
}

#[test]
fn focus_generation_rejects_source_and_approval_drift_after_evidence_capture() -> Result<()> {
    for drift in ["source", "approval"] {
        let (repo, plan) = fixture("claude")?;
        let mut generation = Generation::spawn(&repo, "claude", "wait", "5")?;
        generation.ready()?;
        if drift == "source" {
            repo.write(
                "src/lib.rs",
                &SOURCE.replace("-= amount", "= balance.saturating_sub(amount)"),
            )?;
        } else {
            let page = parse_json(&repo.run_with_env(
                &["onboard", "--candidates", "--path", "src"],
                &[("HOME", repo.path.to_str().context("home")?)],
            )?)?;
            let candidate = page["candidates"]
                .as_array()
                .context("onboard candidates")?
                .iter()
                .find(|candidate| candidate["target"] == plan["steps"][0]["target"])
                .context("approval target")?;
            repo.run_with_env(
                &[
                    "mark",
                    "--fingerprint",
                    candidate["fingerprint"].as_str().context("fingerprint")?,
                    "--path",
                    "src/lib.rs",
                    "--verdict",
                    "approved",
                    "--quiet",
                ],
                &[("HOME", repo.path.to_str().context("home")?)],
            )?;
            assert!(
                !records(&repo)?.is_empty(),
                "concurrent approval must be real persisted state"
            );
        }
        let before = records(&repo)?;
        repo.write(".fixture/release", "")?;
        let (status, stdout, stderr) = generation.finish()?;
        assert!(
            !status.success(),
            "{drift} drift unexpectedly generated {stdout}"
        );
        assert!(stdout.trim().is_empty());
        assert!(
            stderr.contains("changed during focus generation"),
            "{drift}: {stderr}"
        );
        assert_eq!(
            records(&repo)?,
            before,
            "generation must not change concurrent human records"
        );
    }
    Ok(())
}

#[test]
fn focus_generation_hanging_providers_obey_the_cli_deadline() -> Result<()> {
    for provider in ["claude", "codex"] {
        let (repo, _) = fixture(provider)?;
        let mut generation = Generation::spawn(&repo, provider, "hang", "1")?;
        let started = Instant::now();
        let (status, stdout, stderr) = generation.finish()?;
        assert!(
            !status.success(),
            "hanging {provider} unexpectedly generated {stdout}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "--timeout 1 must bound a provider that sleeps for 30 seconds"
        );
        assert!(stderr.contains("timed out"), "{provider}: {stderr}");
        // The total deadline may expire before the provider consumes stdin on a
        // loaded host. If it did start, it must not survive the timeout.
        if let Ok(pid) = fs::read_to_string(generation.directory.join("provider.pid")) {
            let pid: u32 = pid.parse()?;
            let provider_alive = Command::new("/bin/kill")
                .args(["-0", "--", &format!("-{pid}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?
                .success();
            assert!(
                !provider_alive,
                "timeout must reap the provider process group, not just abandon its output"
            );
        }
        assert!(stdout.trim().is_empty());
        assert!(records(&repo)?.is_empty());
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn focus_generation_interrupt_stops_the_provider_process_group() -> Result<()> {
    use std::os::unix::process::ExitStatusExt;

    for (flag, signal) in [("-INT", 2), ("-TERM", 15), ("-HUP", 1)] {
        let (repo, _) = fixture("claude")?;
        let mut generation = Generation::spawn(&repo, "claude", "hang", "30")?;
        generation.ready()?;
        let pid: u32 = fs::read_to_string(generation.directory.join("provider.pid"))?.parse()?;
        Command::new("/bin/kill")
            .args([flag, &generation.child.id().to_string()])
            .status()?;
        let (status, stdout, stderr) = generation.finish()?;
        assert_eq!(
            status.signal(),
            Some(signal),
            "{flag} must still end trueflow with its default action: {status}: {stderr}"
        );
        let provider_alive = Command::new("/bin/kill")
            .args(["-0", "--", &format!("-{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?
            .success();
        assert!(
            !provider_alive,
            "{flag} must stop the provider process group before trueflow exits"
        );
        assert!(stdout.trim().is_empty());
        assert!(records(&repo)?.is_empty());
    }
    Ok(())
}
