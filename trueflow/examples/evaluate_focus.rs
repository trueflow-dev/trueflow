//! Opt-in synthetic selection comparison, not a human review productivity benchmark.
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use clap::{Parser, ValueEnum};
use serde::Serialize;
use serde_json::{Value, json};
use trueflow::commands::review::{ReviewRequest, collect_review, resolve_review_request};
use trueflow::focus::FocusSpecification;
use trueflow::focus_evidence::{FocusCandidate, FocusInventory};
use trueflow::onboarding::ReviewLocation;
use trueflow::review_order::ReviewOrder;
use trueflow_test_support::TestRepo;

#[derive(Debug, Parser)]
#[command(
    about = "Run live 5m/10m focus selection against a synthetic Git fixture (may incur provider charges)",
    long_about = "Runs a real authenticated CLI provider on synthetic source only, then compares target selection with ordinary ReviewOrder. Fixture concern coverage per estimated minute is NOT proven human-discovered concerns per minute. No live request is made for --help. Authentication remains with your installed provider CLI; no credentials or current project files are copied into the fixture or prompt. Codex uses read-only tools, not an OS-level confidentiality boundary; Claude uses no-tools mode."
)]
struct Args {
    /// Existing trueflow executable; default: sibling of this Cargo-built example
    #[arg(long, value_name = "EXECUTABLE")]
    trueflow: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "codex_cli")]
    provider: Provider,
    /// Provider model name, or auto to use the provider's configured default
    #[arg(long, default_value = "auto", value_parser = model_name)]
    model: String,
    /// Total generation deadline per duration; outer watchdog adds 30s for scanning/exit
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=600))]
    timeout: u64,
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize)]
#[value(rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
enum Provider {
    CodexCli,
    ClaudeCli,
}

fn model_name(value: &str) -> std::result::Result<String, String> {
    if value.trim().is_empty() || value.len() > 200 || value.chars().any(char::is_control) {
        return Err("model must be nonempty, at most 200 bytes, with no control characters".into());
    }
    Ok(value.to_owned())
}

// TestRepo does not remove its directory on drop. Keep this example's fixture ephemeral.
struct Fixture(TestRepo);

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0.path) {
            eprintln!(
                "failed to remove fixture {}: {error}",
                self.0.path.display()
            );
        }
    }
}

struct WorkingDirectory(PathBuf);

impl WorkingDirectory {
    fn enter(path: &Path) -> Result<Self> {
        let original = std::env::current_dir()?;
        std::env::set_current_dir(path)?;
        Ok(Self(original))
    }
}

impl Drop for WorkingDirectory {
    fn drop(&mut self) {
        if let Err(error) = std::env::set_current_dir(&self.0) {
            eprintln!("failed to restore working directory: {error}");
        }
    }
}

#[derive(Serialize)]
struct Concern {
    id: &'static str,
    path: &'static str,
    critical_source: &'static str,
    explanation: &'static str,
}

// Ground truth stays in the harness, never in model-visible fixture files.
const CONCERNS: &[Concern] = &[
    Concern {
        id: "tenant-isolation",
        path: "src/accounts.py",
        critical_source: "db.delete_account(request.json[\"account_id\"])",
        explanation: "Authenticated caller can delete an account without checking tenant ownership.",
    },
    Concern {
        id: "archive-traversal",
        path: "src/imports.py",
        critical_source: "archive.extractall(destination, filter=\"fully_trusted\")",
        explanation: "Uploaded tar members are extracted with path/symlink safety filtering explicitly disabled.",
    },
    Concern {
        id: "webhook-authenticity",
        path: "src/webhooks.py",
        critical_source: "event = json.loads(request.body)",
        explanation: "Payment status is changed from an unsigned request body.",
    },
    Concern {
        id: "retry-double-debit",
        path: "src/transfers.py",
        critical_source: "ledger.debit(account_id, amount)",
        explanation: "A timeout after debit is retried without a durable idempotency key; a later corrective patch does not fix this.",
    },
    Concern {
        id: "query-injection",
        path: "src/search.py",
        critical_source: "cursor.execute(\"SELECT email FROM users WHERE name = '\" + name + \"'\")",
        explanation: "Request-controlled search text is interpolated directly into SQL.",
    },
    Concern {
        id: "export-scope",
        path: "src/exports.py",
        critical_source: "rows = db.all_customers()",
        explanation: "A tenant export writes all customers rather than only the authenticated tenant's records.",
    },
];

fn git(repo: &TestRepo, args: &[&str], date: &str) -> Result<String> {
    let output = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(&repo.path)
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .env("GIT_AUTHOR_NAME", "Synthetic Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Synthetic Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .stdin(Stdio::null())
        .output()
        .context("running fixture Git command")?;
    ensure!(
        output.status.success(),
        "fixture Git command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn fixture(args: &Args) -> Result<Fixture> {
    let fixture = Fixture(TestRepo::new("evaluate_focus")?);
    let repo = &fixture.0;
    fs::create_dir_all(repo.path.join(".trueflow"))?;
    repo.write(".gitignore", ".trueflow/\ntrueflow.toml\n")?;
    // Override scanning/filter choices without moving/copying provider authentication.
    repo.write("trueflow.toml", &toml::to_string(&json!({
        "ai": { "mode": "review_plan", "provider": args.provider, "model": args.model, "cache": false },
        "review": { "only": [], "exclude": [] },
        "scan": { "use_cache": false, "write_cache": false, "ignore_names": ["trueflow.toml"], "ignore_globs": [], "ignore_path_prefixes": [] }
    }))?)?;
    repo.write(
        "src/accounts.py",
        r#"def delete_account(request, db, audit):
    principal = request.authenticated_user()
    if principal is None:
        raise PermissionError("login required")
    if not request.json.get("account_id"):
        raise ValueError("account id required")
    db.delete_account(request.json["account_id"])
    audit.write("delete", principal.id, request.json["account_id"])
    return {"deleted": True}
"#,
    )?;
    repo.write(
        "src/imports.py",
        r#"def import_upload(request, tarfile, destination):
    principal = request.authenticated_user()
    if principal is None:
        raise PermissionError("login required")
    upload = request.files["archive"]
    if upload.size > 8_000_000:
        raise ValueError("archive too large")
    with tarfile.open(fileobj=upload.stream) as archive:
        if len(archive.getmembers()) > 500:
            raise ValueError("too many entries")
        archive.extractall(destination, filter="fully_trusted")
    return {"imported": True}
"#,
    )?;
    repo.write(
        "src/webhooks.py",
        r#"def receive_payment(request, db, json):
    if request.method != "POST":
        raise ValueError("POST required")
    if len(request.body) > 16384:
        raise ValueError("event too large")
    event = json.loads(request.body)
    if event["type"] != "payment.succeeded":
        return {"ignored": True}
    invoice = db.invoice(event["invoice_id"])
    if invoice is None:
        raise ValueError("unknown invoice")
    invoice.mark_paid(event["amount"])
    db.save(invoice)
    return {"accepted": True}
"#,
    )?;
    repo.write(
        "src/search.py",
        r#"def search_users(request, connection):
    principal = request.authenticated_user()
    if principal is None:
        raise PermissionError("login required")
    name = request.query["name"]
    if len(name) > 100:
        raise ValueError("query too long")
    cursor = connection.cursor()
    cursor.execute("SELECT email FROM users WHERE name = '" + name + "'")
    rows = cursor.fetchall()
    return {"emails": [row[0] for row in rows]}
"#,
    )?;
    repo.write(
        "src/exports.py",
        r#"def export_customers(request, db, csv, output):
    principal = request.authenticated_user()
    if principal is None:
        raise PermissionError("login required")
    if not principal.can_export:
        raise PermissionError("export permission required")
    rows = db.all_customers()
    writer = csv.writer(output)
    writer.writerow(["name", "email", "balance"])
    for row in rows:
        writer.writerow([row.name, row.email, row.balance])
    output.flush()
    return {"exported": len(rows), "tenant": principal.tenant_id}
"#,
    )?;
    repo.write("src/transfers.py", &transfer_source(1, false))?;
    write_controls(repo, "label")?;
    commit(
        repo,
        "Initial synthetic service and controls",
        "2025-01-01T12:00:00Z",
    )?;
    repo.write("src/transfers.py", &transfer_source(3, false))?;
    commit(
        repo,
        "PATCH: retry ledger timeouts to recover transient failures",
        "2025-01-02T12:00:00Z",
    )?;
    repo.write("src/transfers.py", &transfer_source(3, true))?;
    write_controls(repo, "display_label")?;
    commit(
        repo,
        "PATCH: reject invalid transfers; rename display labels mechanically",
        "2025-01-03T12:00:00Z",
    )?;
    Ok(fixture)
}

fn transfer_source(attempts: usize, validate: bool) -> String {
    let validation = if validate {
        "    if amount <= 0:\n        raise ValueError(\"positive amount required\")\n"
    } else {
        ""
    };
    format!(
        "def transfer(account_id, destination, amount, ledger):\n{validation}    for attempt in range({attempts}):\n        try:\n            ledger.debit(account_id, amount)\n            ledger.credit(destination, amount)\n            return {{\"transferred\": amount}}\n        except TimeoutError:\n            if attempt == {}:\n                raise\n    raise RuntimeError(\"transfer incomplete\")\n",
        attempts - 1
    )
}

fn write_controls(repo: &TestRepo, label: &str) -> Result<()> {
    let mut tests = String::new();
    let mut mechanical = String::new();
    for index in 0..24 {
        tests.push_str(&format!("def test_label_{index}():\n    {label} = \"item-{index}\"\n    expected = \"item-{index}\"\n    assert {label} == expected\n    assert len({label}) > 0\n    assert {label}.startswith(\"item-\")\n\n"));
        mechanical.push_str(&format!(
            "def label_{index}(value):\n    {label} = str(value)\n    return {label}.strip()\n\n"
        ));
    }
    repo.write("tests/test_labels.py", &tests)?;
    repo.write("src/labels.py", &mechanical)
}

fn commit(repo: &TestRepo, message: &str, date: &str) -> Result<()> {
    git(repo, &["add", "."], date)?;
    git(repo, &["commit", "-q", "-m", message], date)?;
    Ok(())
}

fn inventory_and_order(repo: &TestRepo) -> Result<(FocusInventory, Vec<usize>)> {
    let _cwd = WorkingDirectory::enter(&repo.path)?;
    let config = trueflow::config::load()?;
    let query = resolve_review_request(
        ReviewRequest::AllFiles,
        config.review.resolve_filters(&[], &[]),
        config.scan.resolve_options(),
    )?;
    let review = collect_review(&query)?;
    let inventory = FocusInventory::capture(&review, None)?;
    let mut ordered = Vec::new();
    for id in ReviewOrder::from_tree(&review.tree, &review.unreviewed_block_nodes).iter_node_ids() {
        let node = review.tree.node(id);
        if review.tree.is_container_block(id) {
            continue;
        }
        let block = node
            .block
            .as_ref()
            .context("ordinary review node lacks block")?;
        let index = inventory
            .candidates
            .iter()
            .position(|candidate| {
                candidate.target.path == node.path
                    && candidate.target.start_byte == block.start_byte
                    && candidate.target.end_byte == block.end_byte
            })
            .context("ordinary leaf target missing from focus inventory")?;
        ordered.push(index);
    }
    ensure!(
        ordered.len() == inventory.candidates.len(),
        "baseline and focus candidate sets differ"
    );
    Ok((inventory, ordered))
}

fn reached(candidate: &FocusCandidate, repo: &TestRepo) -> Result<Vec<&'static str>> {
    let mut ids = Vec::new();
    for concern in CONCERNS
        .iter()
        .filter(|concern| candidate.target.path.as_str() == concern.path)
    {
        let source = fs::read_to_string(repo.path.join(concern.path))?;
        let start = source
            .find(concern.critical_source)
            .context("fixture concern anchor missing")?;
        if candidate.target.start_byte <= start
            && candidate.target.end_byte >= start + concern.critical_source.len()
        {
            ids.push(concern.id);
        }
    }
    Ok(ids)
}

fn metrics(indices: &[usize], inventory: &FocusInventory, repo: &TestRepo) -> Result<Value> {
    let mut concerns = BTreeSet::new();
    let mut selections = Vec::new();
    let mut estimated_seconds = 0usize;
    let mut noise_seconds = 0usize;
    let mut noise_count = 0usize;
    for &index in indices {
        let candidate = &inventory.candidates[index];
        let ids = reached(candidate, repo)?;
        concerns.extend(ids.iter().copied());
        let noise = matches!(
            candidate.target.path.as_str(),
            "tests/test_labels.py" | "src/labels.py"
        );
        estimated_seconds += candidate.estimated_seconds;
        if noise {
            noise_seconds += candidate.estimated_seconds;
            noise_count += 1;
        }
        selections.push(json!({"target": candidate.target, "estimated_seconds": candidate.estimated_seconds, "planted_concern_ids_reached": ids, "noise_control": noise}));
    }
    let per_minute =
        (estimated_seconds > 0).then(|| concerns.len() as f64 * 60.0 / estimated_seconds as f64);
    Ok(json!({
        "selected": selections,
        "selected_count": indices.len(),
        "estimated_seconds": estimated_seconds,
        "planted_concern_ids_reached": concerns,
        "planted_concern_count": concerns.len(),
        "fixture_concern_coverage": concerns.len() as f64 / CONCERNS.len() as f64,
        "fixture_concerns_reached_per_estimated_minute": per_minute,
        "noise_selected_count": noise_count,
        "noise_estimated_seconds": noise_seconds
    }))
}

// A strict canonical prefix: never skip an expensive card to cherry-pick later cards.
fn baseline(order: &[usize], inventory: &FocusInventory, budget: usize) -> Vec<usize> {
    let mut remaining = budget;
    order
        .iter()
        .copied()
        .take_while(|&index| {
            let seconds = inventory.candidates[index].estimated_seconds;
            if seconds > remaining {
                return false;
            }
            remaining -= seconds;
            true
        })
        .collect()
}

fn run_focus(
    args: &Args,
    executable: &Path,
    repo: &TestRepo,
    seconds: usize,
    inventory: &FocusInventory,
) -> Result<Value> {
    let duration = format!("{}m", seconds / 60);
    // Regular files avoid pipe deadlocks while the parent enforces its watchdog.
    let stdout_path = repo.path.join(".git/evaluate-focus.stdout");
    let stderr_path = repo.path.join(".git/evaluate-focus.stderr");
    let started = Instant::now();
    let mut child = Command::new(executable)
        .args([
            "focus",
            "--duration",
            &duration,
            "--json",
            "--timeout",
            &args.timeout.to_string(),
        ])
        .current_dir(&repo.path)
        .stdin(Stdio::null())
        .stdout(File::create(&stdout_path)?)
        .stderr(File::create(&stderr_path)?)
        .spawn()
        .context("starting trueflow focus")?;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= Duration::from_secs(args.timeout + 30) {
            timed_out = true;
            child
                .kill()
                .context("killing focus after watchdog deadline")?;
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(50));
    };
    let stdout = fs::read_to_string(stdout_path)?;
    let stderr = fs::read_to_string(stderr_path)?;
    let mut result = json!({"status": "provider_failure", "exit_code": status.code(), "outer_watchdog_expired": timed_out, "wall_seconds": started.elapsed().as_secs_f64(), "stderr": stderr});
    if !status.success() || timed_out {
        result["stdout"] = json!(stdout);
        return Ok(result);
    }
    let parsed = (|| -> Result<(Value, Vec<usize>)> {
        FocusSpecification::from_json(&stdout)?;
        let plan: Value = serde_json::from_str(&stdout)?;
        ensure!(
            plan["duration_seconds"].as_u64() == Some(seconds as u64),
            "provider plan duration differs from request"
        );
        let mut selected = Vec::new();
        for step in plan["steps"].as_array().context("missing focus steps")? {
            let target: ReviewLocation = serde_json::from_value(step["target"].clone())?;
            let index = inventory
                .candidates
                .iter()
                .position(|candidate| candidate.target == target)
                .context("selected target not in initial inventory")?;
            selected.push(index);
        }
        Ok((plan, selected))
    })();
    match parsed {
        Ok((plan, selected)) => {
            result["status"] = json!("success");
            result["provider_plan"] = plan; // Includes genuine reasons, evidence IDs, uncertainty and checks.
            result["metrics"] = metrics(&selected, inventory, repo)?;
        }
        Err(error) => {
            result["status"] = json!("invalid_provider_output");
            result["error"] = json!(format!("{error:#}"));
            result["stdout"] = json!(stdout);
        }
    }
    Ok(result)
}

fn evaluate(args: &Args) -> Result<Value> {
    let executable = match &args.trueflow {
        Some(path) => fs::canonicalize(path).context("resolving --trueflow executable")?,
        None => {
            let current = std::env::current_exe()?;
            let directory = current
                .parent()
                .and_then(Path::parent)
                .context("locating example build directory; pass --trueflow")?;
            fs::canonicalize(directory.join(format!("trueflow{}", std::env::consts::EXE_SUFFIX)))
                .context("trueflow executable not built beside example; build --bin trueflow or pass --trueflow")?
        }
    };
    ensure!(
        executable.is_file(),
        "--trueflow must name an executable file"
    );
    let fixture = fixture(args)?;
    let repo = &fixture.0;
    let (inventory, order) = inventory_and_order(repo)?;
    let available = metrics(&order, &inventory, repo)?;
    ensure!(
        available["planted_concern_count"].as_u64() == Some(CONCERNS.len() as u64),
        "not all planted concern anchors are available for selection"
    );
    let mut runs = Vec::new();
    for seconds in [300, 600] {
        let focused = match run_focus(args, &executable, repo, seconds, &inventory) {
            Ok(result) => result,
            Err(error) => json!({"status": "execution_failure", "error": format!("{error:#}")}),
        };
        let same_budget = metrics(&baseline(&order, &inventory, seconds), &inventory, repo)?;
        let matched_estimate = focused["metrics"]["estimated_seconds"]
            .as_u64()
            .map(|estimate| {
                metrics(
                    &baseline(&order, &inventory, usize::try_from(estimate)?),
                    &inventory,
                    repo,
                )
            })
            .transpose()?;
        runs.push(json!({"requested_duration_seconds": seconds, "focus": focused, "baseline_same_requested_budget": same_budget, "baseline_matched_focus_estimate": matched_estimate}));
    }
    let success = runs.iter().all(|run| run["focus"]["status"] == "success");
    Ok(json!({
        "version": 1,
        "status": if success { "success" } else { "failed" },
        "provider": args.provider,
        "requested_model": args.model,
        "trueflow_executable": executable,
        "generation_timeout_seconds_per_run": args.timeout,
        "fixture_commit": git(repo, &["rev-parse", "HEAD"], "2025-01-03T12:00:00Z")?,
        "inventory_snapshot_hash": inventory.snapshot_hash,
        "inventory_diagnostics": inventory.diagnostics,
        "available": available,
        "planted_concerns": CONCERNS,
        "methodology": {
            "fixture_version": 1,
            "baseline": "Actual ReviewOrder::from_tree, restricted to the identical non-container unreviewed candidate set; strict prefix fitting the reading budget, no heuristic re-ranking or skipping.",
            "estimate": "The exact candidate estimated_seconds supplied by focus; compare both requested budget and actual focus estimate.",
            "concern_reached": "A selected target contains the planted critical source span. This does NOT establish that the model or a human recognized the concern.",
            "noise": "Synthetic label-formatting helpers and label-only tests are designated negative controls; selecting them is not generally evidence that tests are unimportant.",
            "limitations": "One synthetic fixture and one live sample per duration; model output is stochastic, the fixture is not representative, and no exact ranking or productivity claim is asserted. Human-discovered concerns/minute is unmeasured.",
            "privacy": "Only generated fixture source/history enters focus evidence. Ground-truth labels stay outside the fixture. No credentials or actual project source are copied into prompts; installed CLI authentication is reused. Codex read-only mode is not tool-free or an OS confidentiality sandbox."
        },
        "runs": runs
    }))
}

fn main() -> ExitCode {
    let args = Args::parse();
    let report = match evaluate(&args) {
        Ok(report) => report,
        Err(error) => {
            json!({"version": 1, "status": "setup_failure", "error": format!("{error:#}")})
        }
    };
    let success = report["status"] == "success";
    match serde_json::to_string_pretty(&report) {
        Ok(output) => println!("{output}"),
        Err(error) => {
            eprintln!("serializing evaluation report: {error}");
            return ExitCode::FAILURE;
        }
    }
    if success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
