#![cfg(unix)]

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};
use trueflow::repo_path::RepoPath;
use trueflow::store::{
    CommentAnchor, DiffCommentAnchor, DiffCommentAnchorRow, SourceCommentAnchor,
};
use trueflow_test_support::{FeedbackScenario, ReviewRecordOverrides, temp_test_dir};

const OWNER: &str = "trueflow-test-owner";
const REPOSITORY: &str = "trueflow-test-repository";
const PULL_REQUEST: &str = "11";
const PENDING_REVIEW_ID: u64 = 101;
const PENDING_REVIEW_NODE_ID: &str = "PRR_pending_review";
const PENDING_REVIEW_URL: &str =
    "https://github.com/trueflow-test-owner/trueflow-test-repository/pull/11#pullrequestreview-101";
const PENDING_REVIEW_OPERATION: &str = "11111111-1111-4111-8111-111111111111";
const EXISTING_COMMENT_OPERATION: &str = "22222222-2222-4222-8222-222222222222";
const ARGV_CALL_SEPARATOR: u8 = 0x01;
const CONCURRENT_SIGNAL_TIMEOUT: Duration = Duration::from_secs(5);
const CONCURRENT_SIGNAL_POLL_INTERVAL: Duration = Duration::from_millis(10);

type TestResult = Result<()>;

#[derive(Clone, Copy)]
enum AnchorKind {
    SingleRight,
    MultilineLeft,
}

#[derive(Clone, Copy)]
struct ExpectedThreadFields {
    line: u64,
    side: &'static str,
    start_line: Option<u64>,
    start_side: Option<&'static str>,
}

impl AnchorKind {
    const fn expected(self) -> ExpectedThreadFields {
        match self {
            Self::SingleRight => ExpectedThreadFields {
                line: 2,
                side: "RIGHT",
                start_line: None,
                start_side: None,
            },
            Self::MultilineLeft => ExpectedThreadFields {
                line: 3,
                side: "LEFT",
                start_line: Some(2),
                start_side: Some("LEFT"),
            },
        }
    }

    const fn record_id(self) -> &'static str {
        match self {
            Self::SingleRight => "single-line-record",
            Self::MultilineLeft => "multiline-record",
        }
    }

    const fn note(self) -> &'static str {
        match self {
            Self::SingleRight => "single-line feedback",
            Self::MultilineLeft => "multiline feedback",
        }
    }

    const fn fixture_name(self) -> &'static str {
        match self {
            Self::SingleRight => "github_pending_review_single_line",
            Self::MultilineLeft => "github_pending_review_multiline",
        }
    }
}

struct PendingReviewFixture {
    scenario: FeedbackScenario,
    stdin_log: PathBuf,
    argv_log: PathBuf,
    expected: ExpectedThreadFields,
}

struct ConcurrentFeedbackWorkers {
    first: Option<Child>,
    second: Option<Child>,
    release: PathBuf,
}

impl ConcurrentFeedbackWorkers {
    fn wait_for_ready(&mut self, ready: &Path) -> Result<()> {
        let deadline = Instant::now() + CONCURRENT_SIGNAL_TIMEOUT;
        loop {
            if ready.exists() {
                return Ok(());
            }
            ensure_worker_running(&mut self.first, "first feedback worker")?;
            if Instant::now() >= deadline {
                bail!(
                    "timed out waiting for first fake-gh append to signal {}",
                    ready.display()
                );
            }
            sleep(CONCURRENT_SIGNAL_POLL_INTERVAL);
        }
    }

    fn release_and_wait(mut self) -> Result<(Output, Output)> {
        fs::write(&self.release, b"release\n").with_context(|| {
            format!(
                "failed to release first fake-gh append through {}",
                self.release.display()
            )
        })?;
        let first = wait_for_worker(&mut self.first, "first feedback worker")?;
        let second = wait_for_worker(&mut self.second, "second feedback worker")?;
        Ok((first, second))
    }

    fn stop(worker: &mut Option<Child>) {
        if let Some(mut worker) = worker.take() {
            if !matches!(worker.try_wait(), Ok(Some(_))) {
                let _ = worker.kill();
            }
            let _ = worker.wait();
        }
    }
}

impl Drop for ConcurrentFeedbackWorkers {
    fn drop(&mut self) {
        // Any assertion or setup failure must unblock the fake gh before reaping its CLI.
        let _ = fs::write(&self.release, b"release\n");
        Self::stop(&mut self.first);
        Self::stop(&mut self.second);
    }
}

fn wait_for_worker(worker: &mut Option<Child>, description: &str) -> Result<Output> {
    worker
        .take()
        .with_context(|| format!("{description} was not started"))?
        .wait_with_output()
        .with_context(|| format!("failed to wait for {description}"))
}

fn ensure_worker_running(worker: &mut Option<Child>, description: &str) -> Result<()> {
    let exited = worker
        .as_mut()
        .with_context(|| format!("{description} was not started"))?
        .try_wait()
        .with_context(|| format!("failed to inspect {description}"))?
        .is_some();
    if !exited {
        return Ok(());
    }

    let output = wait_for_worker(worker, description)?;
    bail!(
        "{description} exited before synchronization completed: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

impl PendingReviewFixture {
    fn run(&self, response_mode: &str) -> Result<Output> {
        self.command(response_mode)?
            .output()
            .context("failed to run the compiled trueflow feedback CLI")
    }

    fn command(&self, response_mode: &str) -> Result<Command> {
        let fake_bin = self
            .argv_log
            .parent()
            .context("fake gh argv log must have a parent")?;
        let inherited_path =
            env::var_os("PATH").context("PATH must be set for integration tests")?;
        let child_path = format!(
            "{}:{}",
            fake_bin.display(),
            inherited_path.to_string_lossy()
        );

        let mut command = Command::new(env!("CARGO_BIN_EXE_trueflow"));
        command
            .args(["feedback", "--pr", "pr:11"])
            .current_dir(&self.scenario.repo().path)
            .env("PATH", child_path)
            .env("TRUEFLOW_FAKE_GH_STDIN_LOG", &self.stdin_log)
            .env("TRUEFLOW_FAKE_GH_ARGV_LOG", &self.argv_log)
            .env("TRUEFLOW_FAKE_GH_RESPONSE_MODE", response_mode)
            .env("TRUEFLOW_FAKE_GH_BASE", self.base_sha()?)
            .env("TRUEFLOW_FAKE_GH_HEAD", self.head_sha()?)
            .env(
                "TRUEFLOW_FAKE_GH_PENDING_BODY_JSON",
                serde_json::to_string(&self.pending_review_body()?)?,
            );
        Ok(command)
    }

    fn graphql_requests(&self) -> Result<Vec<Value>> {
        let raw = fs::read_to_string(&self.stdin_log).with_context(|| {
            format!(
                "failed to read fake gh stdin log {}",
                self.stdin_log.display()
            )
        })?;
        raw.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).context("fake gh captured invalid request JSON"))
            .collect()
    }

    fn argv_calls(&self) -> Result<Vec<Vec<String>>> {
        let raw = fs::read(&self.argv_log).with_context(|| {
            format!(
                "failed to read fake gh argv log {}",
                self.argv_log.display()
            )
        })?;
        raw.split(|byte| *byte == ARGV_CALL_SEPARATOR)
            .filter(|call| !call.is_empty())
            .map(|call| {
                call.split(|byte| *byte == 0)
                    .filter(|arg| !arg.is_empty())
                    .map(|arg| {
                        String::from_utf8(arg.to_vec())
                            .context("fake gh captured a non-UTF-8 argv value")
                    })
                    .collect()
            })
            .collect()
    }

    fn ledger(&self) -> Result<Value> {
        let path = self
            .scenario
            .repo()
            .path
            .join(".trueflow/github_delivery.json");
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read delivery ledger {}", path.display()))?;
        serde_json::from_str(&raw).context("delivery ledger must remain valid JSON")
    }

    fn base_sha(&self) -> Result<String> {
        git_stdout(&self.scenario.repo().path, &["rev-parse", "HEAD^"])
    }

    fn head_sha(&self) -> Result<String> {
        git_stdout(&self.scenario.repo().path, &["rev-parse", "HEAD"])
    }

    fn pending_review_body(&self) -> Result<String> {
        let head = self.head_sha()?;
        Ok(format!(
            "Pending trueflow feedback\n<!-- trueflow:pending-review -->\n<!-- trueflow:delivery:v1 kind=create-pending-review operation={PENDING_REVIEW_OPERATION} head={head} -->"
        ))
    }
}

#[test]
fn gh_pending_review_append_uses_thread_for_single_line() -> TestResult {
    assert_pending_review_append(AnchorKind::SingleRight)
}

#[test]
fn gh_pending_review_append_uses_thread_for_multiline() -> TestResult {
    assert_pending_review_append(AnchorKind::MultilineLeft)
}

#[test]
fn github_feedback_serializes_concurrent_workers() -> TestResult {
    let fixture = pending_review_fixture(AnchorKind::SingleRight)?;
    let gate_dir = temp_test_dir("github_pending_review_concurrent_gate");
    fs::create_dir_all(&gate_dir).with_context(|| {
        format!(
            "failed to create concurrent fake-gh gate {}",
            gate_dir.display()
        )
    })?;
    let first_append = gate_dir.join("first-append");
    let ready = gate_dir.join("ready");
    let release = gate_dir.join("release");
    let second_append = gate_dir.join("second-append");

    let first =
        concurrent_feedback_command(&fixture, &first_append, &ready, &release, &second_append)?
            .spawn()
            .context("failed to start first concurrent trueflow feedback worker")?;
    let mut workers = ConcurrentFeedbackWorkers {
        first: Some(first),
        second: None,
        release,
    };
    workers.wait_for_ready(&ready)?;

    let second = concurrent_feedback_command(
        &fixture,
        &first_append,
        &ready,
        &workers.release,
        &second_append,
    )?
    .spawn()
    .context("failed to start second concurrent trueflow feedback worker")?;
    workers.second = Some(second);
    assert_second_append_is_absent(&second_append, &mut workers.second)?;

    let (first_output, second_output) = workers.release_and_wait()?;
    assert_success(&first_output)?;
    assert_success(&second_output)?;
    assert_captured_thread_request(&fixture)?;
    assert_concurrent_delivery_ledger(&fixture, AnchorKind::SingleRight.record_id())
}

fn assert_pending_review_append(anchor: AnchorKind) -> TestResult {
    let fixture = pending_review_fixture(anchor)?;
    let output = fixture.run("success")?;
    assert_success(&output)?;
    assert_captured_thread_request(&fixture)?;

    let ledger = fixture.ledger()?;
    let comments = ledger["pending_reviews"][0]["comments"]
        .as_array()
        .context("successful append must preserve a pending-review comment receipt list")?;
    assert_eq!(
        comments.len(),
        2,
        "successful thread append must be acknowledged"
    );
    assert_eq!(
        comments[1]["thread_node_id"].as_str(),
        Some("PRT_created_thread"),
        "the validated thread receipt must be persisted"
    );

    for response_mode in [
        "graphql_error",
        "missing_data",
        "null_thread",
        "blank_thread",
        "wrong_operation",
    ] {
        let fixture = pending_review_fixture(anchor)?;
        let output = fixture.run(response_mode)?;
        assert!(
            !output.status.success(),
            "{response_mode} response was incorrectly accepted: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert_captured_thread_request(&fixture)?;

        let ledger = fixture.ledger()?;
        let active = ledger["active_operations"]
            .as_array()
            .context("malformed append acknowledgement must retain active delivery state")?;
        assert_eq!(
            active.len(),
            1,
            "{response_mode} must leave the dispatched append InFlight"
        );
        assert_eq!(active[0]["status"].as_str(), Some("in_flight"));
        let accepted = ledger["pending_reviews"][0]["comments"]
            .as_array()
            .context("malformed append acknowledgement must preserve pending receipts")?;
        assert_eq!(
            accepted.len(),
            1,
            "{response_mode} must not acknowledge the new thread"
        );
    }

    Ok(())
}

fn concurrent_feedback_command(
    fixture: &PendingReviewFixture,
    first_append: &Path,
    ready: &Path,
    release: &Path,
    second_append: &Path,
) -> Result<Command> {
    let mut command = fixture.command("gated_success")?;
    command
        .env("TRUEFLOW_FAKE_GH_FIRST_APPEND_GATE", first_append)
        .env("TRUEFLOW_FAKE_GH_READY_SIGNAL", ready)
        .env("TRUEFLOW_FAKE_GH_RELEASE_SIGNAL", release)
        .env("TRUEFLOW_FAKE_GH_SECOND_APPEND_GATE", second_append)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command)
}

fn assert_second_append_is_absent(second_append: &Path, worker: &mut Option<Child>) -> TestResult {
    let deadline = Instant::now() + CONCURRENT_SIGNAL_TIMEOUT;
    loop {
        assert!(
            !second_append.exists(),
            "second fake-gh thread append reached {} before the first worker was released",
            second_append.display()
        );
        ensure_worker_running(worker, "second feedback worker")?;
        if Instant::now() >= deadline {
            return Ok(());
        }
        sleep(CONCURRENT_SIGNAL_POLL_INTERVAL);
    }
}

fn assert_concurrent_delivery_ledger(
    fixture: &PendingReviewFixture,
    record_id: &str,
) -> TestResult {
    let ledger = fixture.ledger()?;
    let active = ledger["active_operations"]
        .as_array()
        .context("concurrent delivery must retain an active-operations array")?;
    assert!(
        active.is_empty(),
        "successful workers must not leave an active delivery operation: {active:#?}"
    );
    let pending_reviews = ledger["pending_reviews"]
        .as_array()
        .context("concurrent delivery must retain a pending-review ledger")?;
    assert_eq!(
        pending_reviews.len(),
        1,
        "concurrent workers must retain one pending review"
    );
    let comments = pending_reviews[0]["comments"]
        .as_array()
        .context("concurrent delivery must retain pending-review receipts")?;
    assert_eq!(
        comments.len(),
        2,
        "concurrent workers must preserve the existing receipt and accept the staged record once"
    );

    let existing = comments
        .iter()
        .find(|comment| comment["record_id"].as_str() == Some("already-delivered-record"))
        .context("concurrent delivery lost the pre-existing receipt")?;
    assert_eq!(
        existing["operation_id"].as_str(),
        Some(EXISTING_COMMENT_OPERATION),
        "concurrent delivery must preserve the pre-existing receipt operation"
    );
    let accepted = comments
        .iter()
        .filter(|comment| comment["record_id"].as_str() == Some(record_id))
        .collect::<Vec<_>>();
    assert_eq!(
        accepted.len(),
        1,
        "concurrent workers must accept the staged record exactly once: {comments:#?}"
    );
    assert_eq!(
        accepted[0]["thread_node_id"].as_str(),
        Some("PRT_created_thread"),
        "the accepted staged record must retain its validated thread receipt"
    );
    assert!(
        accepted[0]["operation_id"]
            .as_str()
            .is_some_and(|operation| !operation.trim().is_empty()),
        "the accepted staged record must retain its delivery operation"
    );

    Ok(())
}

fn pending_review_fixture(anchor: AnchorKind) -> Result<PendingReviewFixture> {
    let scenario = FeedbackScenario::new(anchor.fixture_name())?;
    scenario.write(
        "src/lib.rs",
        "pub fn retained() {}\npub fn removed_one() {}\npub fn removed_two() {}\n",
    )?;
    let base_sha = scenario.commit_all("base")?;
    scenario.write(
        "src/lib.rs",
        "pub fn retained() {}\npub fn added_one() {}\npub fn added_two() {}\n",
    )?;
    let head_sha = scenario.commit_all("pull request head")?;

    let mut record = scenario.review_block_with_overrides(
        "src/lib.rs",
        "comment",
        &ReviewRecordOverrides {
            id: Some(anchor.record_id()),
            note: Some(anchor.note()),
            ..ReviewRecordOverrides::default()
        },
    )?;
    record.comment_anchor = Some(match anchor {
        AnchorKind::SingleRight => CommentAnchor::Source(SourceCommentAnchor {
            revision: trueflow::store::CommitId::new(&head_sha)?,
            path: RepoPath::new("src/lib.rs")?,
            // Zero-based, half-open source span for new line 2.
            start_line: 1,
            end_line: 2,
        }),
        AnchorKind::MultilineLeft => CommentAnchor::Diff(DiffCommentAnchor {
            revision: trueflow::store::CommitId::new(&head_sha)?,
            path: RepoPath::new("src/lib.rs")?,
            rows: vec![
                DiffCommentAnchorRow {
                    kind: trueflow::store::CommentAnchorDiffLineKind::Removed,
                    old_line: Some(2),
                    new_line: None,
                },
                DiffCommentAnchorRow {
                    kind: trueflow::store::CommentAnchorDiffLineKind::Removed,
                    old_line: Some(3),
                    new_line: None,
                },
            ],
        }),
    });
    scenario.write_reviews(&[record])?;

    configure_github_shaped_origin(&scenario, &base_sha, &head_sha)?;
    seed_pending_review_ledger(&scenario, &head_sha)?;

    let fake_bin = temp_test_dir("github_pending_review_fake_gh");
    fs::create_dir_all(&fake_bin)
        .with_context(|| format!("failed to create fake gh directory {}", fake_bin.display()))?;
    let stdin_log = fake_bin.join("stdin.jsonl");
    let argv_log = fake_bin.join("argv.bin");
    write_fake_gh(&fake_bin.join("gh"))?;

    Ok(PendingReviewFixture {
        scenario,
        stdin_log,
        argv_log,
        expected: anchor.expected(),
    })
}

fn configure_github_shaped_origin(
    scenario: &FeedbackScenario,
    base_sha: &str,
    head_sha: &str,
) -> Result<()> {
    let bare_remote = temp_test_dir("github_pending_review_git_remote");
    let bare_remote_string = bare_remote.to_string_lossy().to_string();
    run_git(
        &scenario.repo().path,
        &["init", "--bare", bare_remote_string.as_str()],
    )?;
    let bare_url = format!("file://{}", bare_remote.display());
    let base_ref = format!("{base_sha}:refs/heads/main");
    let head_ref = format!("{head_sha}:refs/pull/{PULL_REQUEST}/head");
    run_git(
        &scenario.repo().path,
        &[
            "push",
            bare_url.as_str(),
            base_ref.as_str(),
            head_ref.as_str(),
        ],
    )?;
    run_git(
        &scenario.repo().path,
        &[
            "remote",
            "add",
            "origin",
            &format!("https://github.com/{OWNER}/{REPOSITORY}.git"),
        ],
    )?;
    let rewrite_key = format!("url.{bare_url}.insteadOf");
    run_git(
        &scenario.repo().path,
        &[
            "config",
            "--local",
            rewrite_key.as_str(),
            &format!("https://github.com/{OWNER}/{REPOSITORY}.git"),
        ],
    )
}

fn seed_pending_review_ledger(scenario: &FeedbackScenario, head_sha: &str) -> Result<()> {
    let review_body = format!(
        "Pending trueflow feedback\n<!-- trueflow:pending-review -->\n<!-- trueflow:delivery:v1 kind=create-pending-review operation={PENDING_REVIEW_OPERATION} head={head_sha} -->"
    );
    let ledger = json!({
        "version": 2,
        "active_operations": [],
        "pending_reviews": [{
            "pr": {
                "host": "github.com",
                "owner": OWNER,
                "repo": REPOSITORY,
                "number": 11,
            },
            "head_sha": head_sha,
            "review_id": PENDING_REVIEW_ID,
            "review_node_id": PENDING_REVIEW_NODE_ID,
            "html_url": PENDING_REVIEW_URL,
            "create_operation_id": PENDING_REVIEW_OPERATION,
            "comments": [{
                "record_id": "already-delivered-record",
                "operation_id": EXISTING_COMMENT_OPERATION,
                "thread_node_id": null,
                "comment_node_id": null,
            }],
        }],
        "terminal_reviews": [],
    });
    let ledger_path = scenario.repo().path.join(".trueflow/github_delivery.json");
    fs::write(&ledger_path, serde_json::to_vec_pretty(&ledger)?).with_context(|| {
        format!(
            "failed to seed pending delivery ledger {}",
            ledger_path.display()
        )
    })?;
    // Keep this string materialized above so the fixture's remote snapshot and its durable
    // receipt use the exact same marker grammar.
    let _ = review_body;
    Ok(())
}

fn write_fake_gh(path: &Path) -> Result<()> {
    // Each invocation records argv as NUL-delimited arguments terminated by byte 0x01 and
    // records every JSON stdin body on one line. The shell only routes known endpoints; Rust
    // parses the captured JSON and enforces the documented GraphQL schema below.
    let script = r#"#!/bin/sh
set -eu

{
  for argument in "$@"; do
    printf '%s\000' "$argument"
  done
  printf '\001'
} >> "$TRUEFLOW_FAKE_GH_ARGV_LOG"

if [ "${1:-}" = "--hostname" ]; then
  [ "${2:-}" = "github.com" ] || {
    printf 'fake gh received an unexpected hostname: %s\n' "${2:-}" >&2
    exit 2
  }
  shift 2
fi

[ "${1:-}" = "api" ] || {
  printf 'fake gh expected api command, received: %s\n' "${1:-}" >&2
  exit 2
}

body=""
case " $* " in
  *" --input - "*)
    body="$(cat)"
    printf '%s\n' "$body" >> "$TRUEFLOW_FAKE_GH_STDIN_LOG"
    ;;
esac

case " $* " in
  *" api repos/trueflow-test-owner/trueflow-test-repository/pulls/11/commits?per_page=100 "*)
    printf '[{"sha":"%s","commit":{"message":"pull request head"}}]\n' "$TRUEFLOW_FAKE_GH_HEAD"
    exit 0
    ;;
  *" api repos/trueflow-test-owner/trueflow-test-repository/pulls/11 "*)
    printf '{"title":"test pull request","commits":1,"base":{"ref":"main","sha":"%s","repo":{"name":"trueflow-test-repository","owner":{"login":"trueflow-test-owner"}}},"head":{"ref":"topic","sha":"%s","repo":{"name":"trueflow-test-repository","owner":{"login":"trueflow-test-owner"}}}}\n' "$TRUEFLOW_FAKE_GH_BASE" "$TRUEFLOW_FAKE_GH_HEAD"
    exit 0
    ;;
esac

case "$body" in
  *"TrueflowPullRequestDeliveryHead"*)
    printf '{"data":{"repository":{"pullRequest":{"headRefOid":"%s"}}}}\n' "$TRUEFLOW_FAKE_GH_HEAD"
    ;;
  *"TrueflowPullRequestDeliveryReviews"*)
    printf '{"data":{"repository":{"pullRequest":{"reviews":{"nodes":[{"id":"PRR_pending_review","fullDatabaseId":101,"url":"https://github.com/trueflow-test-owner/trueflow-test-repository/pull/11#pullrequestreview-101","body":%s,"state":"PENDING","viewerDidAuthor":true,"commit":{"oid":"%s"}}],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}\n' "$TRUEFLOW_FAKE_GH_PENDING_BODY_JSON" "$TRUEFLOW_FAKE_GH_HEAD"
    ;;
  *"TrueflowPullRequestDeliveryThreads"*)
    printf '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}\n'
    ;;
  *"AddTrueflowPullRequestReviewThread"*)
    operation_id="$(printf '%s' "$body" | sed -n 's/.*"clientMutationId":"\([^"]*\)".*/\1/p')"
    case "$TRUEFLOW_FAKE_GH_RESPONSE_MODE" in
      success)
        printf '{"data":{"addPullRequestReviewThread":{"clientMutationId":"%s","thread":{"id":"PRT_created_thread"}}}}\n' "$operation_id"
        ;;
      gated_success)
        # stdin was captured above, so this signal proves the append request reached fake gh.
        if mkdir "$TRUEFLOW_FAKE_GH_FIRST_APPEND_GATE" 2>/dev/null; then
          : > "$TRUEFLOW_FAKE_GH_READY_SIGNAL"
          release_waits=0
          while [ ! -e "$TRUEFLOW_FAKE_GH_RELEASE_SIGNAL" ]; do
            release_waits=$((release_waits + 1))
            [ "$release_waits" -lt 1500 ] || {
              printf 'timed out waiting for concurrent fake-gh release\n' >&2
              exit 2
            }
            sleep 0.01
          done
        else
          : > "$TRUEFLOW_FAKE_GH_SECOND_APPEND_GATE"
        fi
        printf '{"data":{"addPullRequestReviewThread":{"clientMutationId":"%s","thread":{"id":"PRT_created_thread"}}}}\n' "$operation_id"
        ;;
      graphql_error)
        printf '{"errors":[{"message":"schema rejected the request"}]}\n'
        ;;
      missing_data)
        printf '{}\n'
        ;;
      null_thread)
        printf '{"data":{"addPullRequestReviewThread":{"clientMutationId":"%s","thread":null}}}\n' "$operation_id"
        ;;
      blank_thread)
        printf '{"data":{"addPullRequestReviewThread":{"clientMutationId":"%s","thread":{"id":"   "}}}}\n' "$operation_id"
        ;;
      wrong_operation)
        printf '{"data":{"addPullRequestReviewThread":{"clientMutationId":"a-different-operation","thread":{"id":"PRT_created_thread"}}}}\n'
        ;;
      *)
        printf 'unknown fake gh response mode: %s\n' "$TRUEFLOW_FAKE_GH_RESPONSE_MODE" >&2
        exit 2
        ;;
    esac
    ;;
  *)
    printf 'fake gh received an unsupported request: %s\n' "$*" >&2
    exit 2
    ;;
esac
"#;

    fs::write(path, script)
        .with_context(|| format!("failed to write fake gh executable {}", path.display()))?;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("failed to make fake gh executable {}", path.display()))
}

fn assert_captured_thread_request(fixture: &PendingReviewFixture) -> TestResult {
    let requests = fixture.graphql_requests()?;
    let request = requests
        .iter()
        .filter(|request| {
            request["query"]
                .as_str()
                .is_some_and(|query| query.contains("AddTrueflowPullRequestReviewThread"))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        request.len(),
        1,
        "expected exactly one captured pending-review thread mutation; captured requests: {requests:#?}"
    );
    let request = request[0];
    let query = request["query"]
        .as_str()
        .context("captured GraphQL request must include a query string")?;
    assert!(
        query.contains(
            "mutation AddTrueflowPullRequestReviewThread($input: AddPullRequestReviewThreadInput!)"
        ),
        "thread request must declare the documented input type: {query}"
    );
    assert!(
        query.contains("addPullRequestReviewThread(input: $input)"),
        "thread request must invoke the documented mutation: {query}"
    );
    assert!(
        !query.contains("addPullRequestReviewComment"),
        "thread append must never use the legacy review-comment mutation: {query}"
    );

    let input = request["variables"]["input"]
        .as_object()
        .context("thread mutation must carry variables.input as an object")?;
    let fields = input.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let mut expected_fields = BTreeSet::from([
        "body",
        "clientMutationId",
        "line",
        "path",
        "pullRequestReviewId",
        "side",
        "subjectType",
    ]);
    if fixture.expected.start_line.is_some() {
        expected_fields.insert("startLine");
        expected_fields.insert("startSide");
    }
    assert_eq!(
        fields, expected_fields,
        "thread input must use exactly the documented schema fields"
    );
    assert_eq!(
        input["pullRequestReviewId"].as_str(),
        Some(PENDING_REVIEW_NODE_ID)
    );
    assert_eq!(input["path"].as_str(), Some("src/lib.rs"));
    assert_eq!(input["line"].as_u64(), Some(fixture.expected.line));
    assert_eq!(input["side"].as_str(), Some(fixture.expected.side));
    assert_eq!(input["subjectType"].as_str(), Some("LINE"));

    let operation_id = input["clientMutationId"]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .context("thread mutation clientMutationId must be nonblank")?;
    let body = input["body"]
        .as_str()
        .context("thread mutation body must be a string")?;
    assert!(
        body.contains("<!-- trueflow:delivery:v1 kind=review-thread operation="),
        "thread body must carry a durable review-thread operation marker: {body:?}"
    );
    assert!(
        body.contains(operation_id),
        "thread marker must correlate with clientMutationId {operation_id}: {body:?}"
    );

    match (fixture.expected.start_line, fixture.expected.start_side) {
        (None, None) => {
            assert!(
                !input.contains_key("startLine") && !input.contains_key("startSide"),
                "single-line thread input must omit both start fields"
            );
        }
        (Some(start_line), Some(start_side)) => {
            assert_eq!(input["startLine"].as_u64(), Some(start_line));
            assert_eq!(input["startSide"].as_str(), Some(start_side));
        }
        _ => bail!("test fixture has an invalid half-range expectation"),
    }

    let calls = fixture.argv_calls()?;
    assert!(
        calls.iter().any(|call| {
            call.windows(5).any(|window| {
                window
                    == [
                        "api".to_string(),
                        "--method".to_string(),
                        "POST".to_string(),
                        "graphql".to_string(),
                        "--input".to_string(),
                    ]
            }) && call.last().is_some_and(|argument| argument == "-")
        }),
        "fake gh must observe a POST graphql child invocation with stdin input; calls: {calls:#?}"
    );

    Ok(())
}

fn assert_success(output: &Output) -> TestResult {
    if output.status.success() {
        return Ok(());
    }
    Err(anyhow!(
        "trueflow feedback failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    ))
}

fn run_git(path: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .with_context(|| format!("failed to execute git {args:?}"))?;
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "git {args:?} failed in {}: {}{}",
        path.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

fn git_stdout(path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .with_context(|| format!("failed to execute git {args:?}"))?;
    if !output.status.success() {
        bail!(
            "git {args:?} failed in {}: {}{}",
            path.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    String::from_utf8(output.stdout)
        .context("git output was not UTF-8")
        .map(|output| output.trim().to_string())
}
