use crate::repo_path::RepoPath;
use crate::store::CommitId;
use crate::vcs;
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;
use std::process::Command;
use std::str::FromStr;
use url::Url;

const PULL_REQUEST_REFERENCE_HELP: &str =
    "Use pr:11, pr:owner/repo/11, or https://host/owner/repo/pull/11";
const PULL_REQUEST_REF_NAMESPACE: &str = "refs/trueflow/pr";
const GH_MAX_PULL_REQUEST_COMMITS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PullRequestRef {
    Number {
        number: u64,
    },
    Repository {
        owner: String,
        repo: String,
        number: u64,
    },
    HostedRepository {
        host: String,
        owner: String,
        repo: String,
        number: u64,
    },
}

impl PullRequestRef {
    pub fn from_cli(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(anyhow!(
                "pull request reference cannot be empty ({PULL_REQUEST_REFERENCE_HELP})"
            ));
        }

        if let Some(rest) = raw.strip_prefix("pr:") {
            return parse_prefixed_pull_request(rest, raw);
        }

        if raw.starts_with("http://") || raw.starts_with("https://") {
            return parse_pull_request_url(raw);
        }

        Err(anyhow!(
            "Invalid pull request reference '{raw}'. {PULL_REQUEST_REFERENCE_HELP}"
        ))
    }
}

impl fmt::Display for PullRequestRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number { number } => write!(f, "pr:{number}"),
            Self::Repository {
                owner,
                repo,
                number,
            } => write!(f, "pr:{owner}/{repo}/{number}"),
            Self::HostedRepository {
                host,
                owner,
                repo,
                number,
            } => write!(f, "{host}/{owner}/{repo}#{number}"),
        }
    }
}

impl FromStr for PullRequestRef {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::from_cli(raw)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResolvedPullRequestRef {
    pub host: String,
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl fmt::Display for ResolvedPullRequestRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{}/{}#{}",
            self.host, self.owner, self.repo, self.number
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRemote {
    pub name: String,
    pub fetch_url: String,
    pub host: String,
    pub owner: String,
    pub repo: String,
}

impl GitRemote {
    fn matches_pull_request(&self, pr: &ResolvedPullRequestRef) -> bool {
        self.host == pr.host && self.owner == pr.owner && self.repo == pr.repo
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestMetadata {
    pub pr: ResolvedPullRequestRef,
    pub title: String,
    pub base_ref: String,
    pub base_sha: CommitId,
    pub head_ref: String,
    pub head_sha: CommitId,
    pub commits: Vec<PullRequestCommit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestCommit {
    pub sha: CommitId,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedPullRequestReview {
    pub remote: GitRemote,
    pub metadata: PullRequestMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum GitHubCommentSide {
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubInlineComment {
    pub path: RepoPath,
    pub line: u32,
    pub side: GitHubCommentSide,
    pub start_line: Option<u32>,
    pub start_side: Option<GitHubCommentSide>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubReviewDraft {
    pub body: String,
    pub comments: Vec<GitHubInlineComment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedPullRequestReview {
    pub id: u64,
    pub html_url: String,
    pub state: PullRequestReviewState,
    pub body: String,
    pub node_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestReviewState {
    Pending,
    Commented,
    Approved,
    ChangesRequested,
    Dismissed,
    Unknown,
}

pub trait GitHubClient {
    fn resolve_pull_request(&self, pr: &ResolvedPullRequestRef) -> Result<PullRequestMetadata>;
    fn create_pending_pull_request_review(
        &self,
        pr: &ResolvedPullRequestRef,
        head_sha: &CommitId,
        draft: &GitHubReviewDraft,
    ) -> Result<PostedPullRequestReview>;
    fn add_comment_to_pending_pull_request_review(
        &self,
        pr: &ResolvedPullRequestRef,
        review: &PostedPullRequestReview,
        comment: &GitHubInlineComment,
    ) -> Result<()>;
    fn pull_request_review_status(
        &self,
        pr: &ResolvedPullRequestRef,
        review_id: u64,
    ) -> Result<Option<PostedPullRequestReview>>;
}

pub struct GhGitHubClient;

impl GitHubClient for GhGitHubClient {
    fn resolve_pull_request(&self, pr: &ResolvedPullRequestRef) -> Result<PullRequestMetadata> {
        let pull_json = run_gh_api(
            &pr.host,
            &format!("repos/{}/{}/pulls/{}", pr.owner, pr.repo, pr.number),
        )?;
        let commits_json = run_gh_api(
            &pr.host,
            &format!(
                "repos/{}/{}/pulls/{}/commits?per_page={GH_MAX_PULL_REQUEST_COMMITS}",
                pr.owner, pr.repo, pr.number
            ),
        )?;
        parse_pull_request_metadata(pr, &pull_json, &commits_json)
    }

    fn create_pending_pull_request_review(
        &self,
        pr: &ResolvedPullRequestRef,
        head_sha: &CommitId,
        draft: &GitHubReviewDraft,
    ) -> Result<PostedPullRequestReview> {
        let endpoint = format!("repos/{}/{}/pulls/{}/reviews", pr.owner, pr.repo, pr.number);
        let body = serde_json::to_string(&serde_json::json!({
            "body": draft.body,
            "commit_id": head_sha,
            "comments": draft.comments,
        }))?;
        let response = run_gh_api_with_body(&pr.host, "POST", &endpoint, &body)?;
        parse_posted_pull_request_review(&response)
    }

    fn add_comment_to_pending_pull_request_review(
        &self,
        pr: &ResolvedPullRequestRef,
        review: &PostedPullRequestReview,
        comment: &GitHubInlineComment,
    ) -> Result<()> {
        let review_node_id = review.node_id.as_ref().ok_or_else(|| {
            anyhow!(
                "GitHub review {} did not include a GraphQL node id; cannot append comments",
                review.id
            )
        })?;
        let body = serde_json::to_string(&serde_json::json!({
            "query": r#"
                mutation AddTrueflowPullRequestReviewComment(
                    $pullRequestReviewId: ID!,
                    $body: String!,
                    $path: String!,
                    $line: Int!,
                    $side: DiffSide!,
                    $startLine: Int,
                    $startSide: DiffSide
                ) {
                    addPullRequestReviewComment(input: {
                        pullRequestReviewId: $pullRequestReviewId,
                        body: $body,
                        path: $path,
                        line: $line,
                        side: $side,
                        startLine: $startLine,
                        startSide: $startSide
                    }) {
                        comment { id }
                    }
                }
            "#,
            "variables": {
                "pullRequestReviewId": review_node_id,
                "body": comment.body.as_str(),
                "path": comment.path.as_str(),
                "line": comment.line,
                "side": comment.side,
                "startLine": comment.start_line,
                "startSide": comment.start_side,
            }
        }))?;
        run_gh_api_with_body(&pr.host, "POST", "graphql", &body)?;
        Ok(())
    }

    fn pull_request_review_status(
        &self,
        pr: &ResolvedPullRequestRef,
        review_id: u64,
    ) -> Result<Option<PostedPullRequestReview>> {
        let endpoint = format!(
            "repos/{}/{}/pulls/{}/reviews/{review_id}",
            pr.owner, pr.repo, pr.number
        );
        let Some(response) = run_gh_api_optional(&pr.host, &endpoint)? else {
            return Ok(None);
        };
        Ok(Some(parse_posted_pull_request_review(&response)?))
    }
}

pub fn prepare_pull_request_review<C>(
    repo_root: &Path,
    requested: &PullRequestRef,
    client: &C,
) -> Result<PreparedPullRequestReview>
where
    C: GitHubClient,
{
    prepare_pull_request_review_with(repo_root, requested, client, fetch_pull_request_refs)
}

pub fn prepare_pull_request_review_with<C, F>(
    repo_root: &Path,
    requested: &PullRequestRef,
    client: &C,
    mut fetch: F,
) -> Result<PreparedPullRequestReview>
where
    C: GitHubClient,
    F: FnMut(&Path, &str, &PullRequestMetadata) -> Result<()>,
{
    let remotes = load_git_remotes(repo_root)?;
    let resolved = resolve_pull_request_ref(requested, &remotes)?;
    let remote = select_matching_remote(&resolved, &remotes)?;
    let metadata = client.resolve_pull_request(&resolved)?;
    if !remote.matches_pull_request(&metadata.pr) {
        return Err(anyhow!(
            "Current repository remote '{}' points to {}/{}, but PR {} belongs to {}/{}",
            remote.name,
            remote.owner,
            remote.repo,
            metadata.pr.number,
            metadata.pr.owner,
            metadata.pr.repo
        ));
    }
    fetch(repo_root, &remote.name, &metadata)?;
    Ok(PreparedPullRequestReview { remote, metadata })
}

pub fn load_git_remotes(repo_root: &Path) -> Result<Vec<GitRemote>> {
    let output = Command::new("git")
        .args(["config", "--get-regexp", r"^remote\..*\.url$"])
        .current_dir(repo_root)
        .output()
        .with_context(|| format!("failed to execute git config in {}", repo_root.display()))?;

    if !output.status.success() {
        if output.status.code() == Some(1) {
            return Ok(Vec::new());
        }
        return Err(anyhow!(
            "git config failed in {}: {}{}",
            repo_root.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let stdout = String::from_utf8(output.stdout)
        .with_context(|| format!("git config output was not utf8 in {}", repo_root.display()))?;
    parse_git_remotes_config(&stdout)
}

pub fn parse_git_remotes_config(raw: &str) -> Result<Vec<GitRemote>> {
    let mut remotes = Vec::new();

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(split_index) = line.find(char::is_whitespace) else {
            continue;
        };
        let (key, fetch_url) = line.split_at(split_index);
        let Some(remote_name) = parse_remote_config_key(key) else {
            continue;
        };
        let Some((host, owner, repo)) = parse_remote_repo_identity(fetch_url.trim()) else {
            continue;
        };
        remotes.push(GitRemote {
            name: remote_name.to_string(),
            fetch_url: fetch_url.trim().to_string(),
            host,
            owner,
            repo,
        });
    }

    Ok(remotes)
}

pub fn resolve_pull_request_ref(
    requested: &PullRequestRef,
    remotes: &[GitRemote],
) -> Result<ResolvedPullRequestRef> {
    match requested {
        PullRequestRef::HostedRepository {
            host,
            owner,
            repo,
            number,
        } => Ok(ResolvedPullRequestRef {
            host: host.clone(),
            owner: owner.clone(),
            repo: repo.clone(),
            number: *number,
        }),
        PullRequestRef::Repository {
            owner,
            repo,
            number,
        } => {
            let remote = preferred_remote_for_inference(remotes)?;
            Ok(ResolvedPullRequestRef {
                host: remote.host.clone(),
                owner: owner.clone(),
                repo: repo.clone(),
                number: *number,
            })
        }
        PullRequestRef::Number { number } => {
            let remote = preferred_remote_for_inference(remotes)?;
            Ok(ResolvedPullRequestRef {
                host: remote.host.clone(),
                owner: remote.owner.clone(),
                repo: remote.repo.clone(),
                number: *number,
            })
        }
    }
}

pub fn select_matching_remote(
    pr: &ResolvedPullRequestRef,
    remotes: &[GitRemote],
) -> Result<GitRemote> {
    remotes
        .iter()
        .find(|remote| remote.name == "origin" && remote.matches_pull_request(pr))
        .or_else(|| remotes.iter().find(|remote| remote.matches_pull_request(pr)))
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "Current repository does not have a remote for pull request {pr}; expected {}/{}/{}",
                pr.host,
                pr.owner,
                pr.repo
            )
        })
}

pub fn fetch_pull_request_refs(
    repo_root: &Path,
    remote_name: &str,
    metadata: &PullRequestMetadata,
) -> Result<()> {
    let head_refspec = format!(
        "+refs/pull/{}/head:{}",
        metadata.pr.number,
        hidden_ref(&metadata.pr, "head")
    );
    let base_branch_refspec = format!(
        "+refs/heads/{}:{}",
        metadata.base_ref,
        hidden_ref(&metadata.pr, "base-branch")
    );

    run_git_command(
        repo_root,
        [
            "fetch".to_string(),
            "--quiet".to_string(),
            "--no-tags".to_string(),
            remote_name.to_string(),
            head_refspec,
            base_branch_refspec,
        ],
    )?;

    validate_fetched_pull_request(repo_root, metadata)?;
    run_git_command(
        repo_root,
        [
            "update-ref".to_string(),
            hidden_ref(&metadata.pr, "base"),
            metadata.base_sha.to_string(),
        ],
    )?;
    Ok(())
}

pub fn parse_pull_request_metadata(
    requested: &ResolvedPullRequestRef,
    pull_json: &str,
    commits_json: &str,
) -> Result<PullRequestMetadata> {
    let pull: PullRequestApiResponse = serde_json::from_str(pull_json)
        .with_context(|| "failed to parse pull request metadata JSON".to_string())?;
    if pull.commit_count > GH_MAX_PULL_REQUEST_COMMITS {
        return Err(anyhow!(
            "Pull request {} has {} commits; trueflow currently supports up to {GH_MAX_PULL_REQUEST_COMMITS}",
            requested.number,
            pull.commit_count
        ));
    }

    let base_repo = pull.base.repo.ok_or_else(|| {
        anyhow!("pull request metadata did not include a base repository for {requested}")
    })?;
    let pr = ResolvedPullRequestRef {
        host: requested.host.clone(),
        owner: base_repo.owner.login,
        repo: base_repo.name,
        number: requested.number,
    };

    let commits: Vec<PullRequestCommitApiResponse> = serde_json::from_str(commits_json)
        .with_context(|| "failed to parse pull request commits JSON".to_string())?;

    let commits = commits
        .into_iter()
        .map(|commit| {
            let summary = commit
                .commit
                .message
                .lines()
                .next()
                .map(str::trim)
                .unwrap_or_default()
                .to_string();
            Ok(PullRequestCommit {
                sha: CommitId::new(commit.sha)?,
                summary,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(PullRequestMetadata {
        pr,
        title: pull.title.trim().to_string(),
        base_ref: pull.base.ref_name,
        base_sha: CommitId::new(pull.base.sha)?,
        head_ref: pull.head.ref_name,
        head_sha: CommitId::new(pull.head.sha)?,
        commits,
    })
}

fn preferred_remote_for_inference(remotes: &[GitRemote]) -> Result<&GitRemote> {
    remotes
        .iter()
        .find(|remote| remote.name == "origin")
        .or_else(|| remotes.first())
        .ok_or_else(|| {
            anyhow!(
                "Could not infer the current GitHub repository from git remotes. Add a GitHub-style remote URL first."
            )
        })
}

fn hidden_ref(pr: &ResolvedPullRequestRef, suffix: &str) -> String {
    format!("{PULL_REQUEST_REF_NAMESPACE}/{}/{}", pr.number, suffix)
}

fn validate_fetched_pull_request(repo_root: &Path, metadata: &PullRequestMetadata) -> Result<()> {
    let repo = gix::discover(repo_root)
        .with_context(|| format!("git repository required at {}", repo_root.display()))?;

    vcs::resolve_commit_id_in_repo(&repo, metadata.head_sha.as_str()).with_context(|| {
        format!(
            "expected fetched PR head commit {} to be available locally",
            metadata.head_sha
        )
    })?;
    vcs::resolve_commit_id_in_repo(&repo, metadata.base_sha.as_str()).with_context(|| {
        format!(
            "expected fetched PR base commit {} to be available locally",
            metadata.base_sha
        )
    })?;

    for commit in &metadata.commits {
        vcs::resolve_commit_id_in_repo(&repo, commit.sha.as_str()).with_context(|| {
            format!(
                "expected fetched PR commit {} ({}) to be available locally",
                commit.sha, commit.summary
            )
        })?;
    }

    Ok(())
}

fn run_gh_api(host: &str, endpoint: &str) -> Result<String> {
    run_gh_api_with_args(host, ["api", endpoint])
}

fn run_gh_api_optional(host: &str, endpoint: &str) -> Result<Option<String>> {
    let output = Command::new("gh")
        .arg("--hostname")
        .arg(host)
        .arg("api")
        .arg(endpoint)
        .output()
        .with_context(|| format!("failed to execute gh api for host {host}"))?;

    if output.status.success() {
        return Ok(Some(String::from_utf8(output.stdout).with_context(
            || format!("gh api output for {endpoint} was not utf8"),
        )?));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stdout.contains("404") || stderr.contains("404") {
        return Ok(None);
    }

    Err(anyhow!(
        "gh api {endpoint} failed for host {host}: {stdout}{stderr}"
    ))
}

fn run_gh_api_with_body(host: &str, method: &str, endpoint: &str, body: &str) -> Result<String> {
    let mut command = Command::new("gh");
    command
        .arg("--hostname")
        .arg(host)
        .arg("api")
        .arg("--method")
        .arg(method)
        .arg(endpoint)
        .arg("--input")
        .arg("-");
    let mut child = command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to execute gh api for host {host}"))?;
    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().context("failed to open gh stdin")?;
        stdin.write_all(body.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "gh api {endpoint} failed for host {host}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout)
        .with_context(|| format!("gh api output for {endpoint} was not utf8"))
}

fn run_gh_api_with_args<'a>(host: &str, args: impl IntoIterator<Item = &'a str>) -> Result<String> {
    let output = Command::new("gh")
        .arg("--hostname")
        .arg(host)
        .args(args)
        .output()
        .with_context(|| format!("failed to execute gh api for host {host}"))?;

    if !output.status.success() {
        return Err(anyhow!(
            "gh api failed for host {host}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    String::from_utf8(output.stdout).with_context(|| "gh api output was not utf8".to_string())
}

fn parse_posted_pull_request_review(raw: &str) -> Result<PostedPullRequestReview> {
    let review: PullRequestReviewApiResponse = serde_json::from_str(raw)
        .with_context(|| "failed to parse pull request review JSON".to_string())?;
    Ok(PostedPullRequestReview {
        id: review.id,
        html_url: review.html_url,
        state: parse_pull_request_review_state(review.state.as_deref()),
        body: review.body.unwrap_or_default(),
        node_id: review.node_id,
    })
}

fn parse_pull_request_review_state(state: Option<&str>) -> PullRequestReviewState {
    match state {
        Some("PENDING") => PullRequestReviewState::Pending,
        Some("COMMENTED") => PullRequestReviewState::Commented,
        Some("APPROVED") => PullRequestReviewState::Approved,
        Some("CHANGES_REQUESTED") => PullRequestReviewState::ChangesRequested,
        Some("DISMISSED") => PullRequestReviewState::Dismissed,
        Some(_) | None => PullRequestReviewState::Unknown,
    }
}

fn run_git_command(repo_root: &Path, args: impl IntoIterator<Item = String>) -> Result<String> {
    let args = args.into_iter().collect::<Vec<_>>();
    let output = Command::new("git")
        .args(&args)
        .current_dir(repo_root)
        .output()
        .with_context(|| format!("failed to execute git {args:?}"))?;

    if !output.status.success() {
        return Err(anyhow!(
            "git {:?} failed in {}: {}{}",
            args,
            repo_root.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    String::from_utf8(output.stdout).with_context(|| format!("git {args:?} output was not utf8"))
}

fn parse_prefixed_pull_request(rest: &str, raw: &str) -> Result<PullRequestRef> {
    if !rest.contains('/') {
        return Ok(PullRequestRef::Number {
            number: parse_pull_request_number(rest, raw)?,
        });
    }

    let segments = rest.split('/').collect::<Vec<_>>();
    let [owner, repo, number] = segments.as_slice() else {
        return Err(anyhow!(
            "Invalid pull request reference '{raw}'. {PULL_REQUEST_REFERENCE_HELP}"
        ));
    };

    let owner = parse_repo_component(owner, "owner", raw)?;
    let repo = parse_repo_component(repo, "repo", raw)?;
    let number = parse_pull_request_number(number, raw)?;
    Ok(PullRequestRef::Repository {
        owner,
        repo,
        number,
    })
}

fn parse_pull_request_url(raw: &str) -> Result<PullRequestRef> {
    let url = Url::parse(raw).with_context(|| {
        format!("Invalid pull request URL '{raw}'. {PULL_REQUEST_REFERENCE_HELP}")
    })?;

    if !matches!(url.scheme(), "http" | "https") {
        return Err(anyhow!(
            "Invalid pull request URL '{raw}': unsupported scheme '{}'. {PULL_REQUEST_REFERENCE_HELP}",
            url.scheme()
        ));
    }

    let host = url.host_str().context(format!(
        "Invalid pull request URL '{raw}': missing host. {PULL_REQUEST_REFERENCE_HELP}"
    ))?;
    let segments = url
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .unwrap_or_default();
    let [owner, repo, "pull", number] = segments.as_slice() else {
        return Err(anyhow!(
            "Invalid pull request URL '{raw}'. {PULL_REQUEST_REFERENCE_HELP}"
        ));
    };

    Ok(PullRequestRef::HostedRepository {
        host: host.to_string(),
        owner: parse_repo_component(owner, "owner", raw)?,
        repo: parse_repo_component(repo, "repo", raw)?,
        number: parse_pull_request_number(number, raw)?,
    })
}

fn parse_remote_config_key(key: &str) -> Option<&str> {
    let rest = key.strip_prefix("remote.")?;
    let (name, suffix) = rest.rsplit_once('.')?;
    (suffix == "url").then_some(name)
}

fn parse_remote_repo_identity(url: &str) -> Option<(String, String, String)> {
    parse_remote_repo_identity_from_standard_url(url)
        .or_else(|| parse_remote_repo_identity_from_scp_like_url(url))
}

fn parse_remote_repo_identity_from_standard_url(url: &str) -> Option<(String, String, String)> {
    let parsed = Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_string();
    let path = parsed.path().trim_start_matches('/');
    parse_owner_repo_path(path).map(|(owner, repo)| (host, owner, repo))
}

fn parse_remote_repo_identity_from_scp_like_url(url: &str) -> Option<(String, String, String)> {
    let (host_part, path_part) = url.split_once(':')?;
    if host_part.contains('/') {
        return None;
    }
    let host = host_part
        .rsplit_once('@')
        .map_or(host_part, |(_, host)| host);
    parse_owner_repo_path(path_part).map(|(owner, repo)| (host.to_string(), owner, repo))
}

fn parse_owner_repo_path(path: &str) -> Option<(String, String)> {
    let trimmed = path.trim().trim_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let mut segments = trimmed.split('/').filter(|segment| !segment.is_empty());
    let owner = segments.next()?.to_string();
    let repo = segments.next()?.to_string();
    Some((owner, repo))
}

fn parse_repo_component(value: &str, component: &str, raw: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(anyhow!(
            "Invalid pull request reference '{raw}': {component} cannot be empty. {PULL_REQUEST_REFERENCE_HELP}"
        ));
    }
    Ok(value.to_string())
}

fn parse_pull_request_number(value: &str, raw: &str) -> Result<u64> {
    let number: u64 = value.trim().parse().with_context(|| {
        format!(
            "Invalid pull request reference '{raw}': expected numeric pull request number. {PULL_REQUEST_REFERENCE_HELP}"
        )
    })?;
    if number == 0 {
        return Err(anyhow!(
            "Invalid pull request reference '{raw}': pull request number must be greater than zero. {PULL_REQUEST_REFERENCE_HELP}"
        ));
    }
    Ok(number)
}

#[derive(Debug, Deserialize)]
struct PullRequestApiResponse {
    title: String,
    #[serde(rename = "commits")]
    commit_count: usize,
    base: PullRequestBranchApiResponse,
    head: PullRequestBranchApiResponse,
}

#[derive(Debug, Deserialize)]
struct PullRequestBranchApiResponse {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
    repo: Option<PullRequestRepoApiResponse>,
}

#[derive(Debug, Deserialize)]
struct PullRequestRepoApiResponse {
    name: String,
    owner: PullRequestOwnerApiResponse,
}

#[derive(Debug, Deserialize)]
struct PullRequestOwnerApiResponse {
    login: String,
}

#[derive(Debug, Deserialize)]
struct PullRequestCommitApiResponse {
    sha: String,
    commit: PullRequestCommitMessageApiResponse,
}

#[derive(Debug, Deserialize)]
struct PullRequestCommitMessageApiResponse {
    message: String,
}

#[derive(Debug, Deserialize)]
struct PullRequestReviewApiResponse {
    id: u64,
    html_url: String,
    state: Option<String>,
    body: Option<String>,
    node_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{
        GH_MAX_PULL_REQUEST_COMMITS, GitHubReviewDraft, GitRemote, PostedPullRequestReview,
        PullRequestCommit, PullRequestMetadata, PullRequestRef, ResolvedPullRequestRef,
        fetch_pull_request_refs, parse_git_remotes_config, parse_pull_request_metadata,
        prepare_pull_request_review_with, resolve_pull_request_ref, select_matching_remote,
    };
    use crate::store::CommitId;
    use crate::test_git::{run_git, run_git_stdout, temp_git_repo};
    use anyhow::{Result, anyhow};
    use std::fs;

    struct FakeGitHubClient {
        metadata: PullRequestMetadata,
    }

    impl super::GitHubClient for FakeGitHubClient {
        fn resolve_pull_request(
            &self,
            _pr: &ResolvedPullRequestRef,
        ) -> Result<PullRequestMetadata> {
            Ok(self.metadata.clone())
        }

        fn create_pending_pull_request_review(
            &self,
            _pr: &ResolvedPullRequestRef,
            _head_sha: &CommitId,
            _draft: &GitHubReviewDraft,
        ) -> Result<PostedPullRequestReview> {
            Err(anyhow!("not used in tests"))
        }

        fn add_comment_to_pending_pull_request_review(
            &self,
            _pr: &ResolvedPullRequestRef,
            _review: &PostedPullRequestReview,
            _comment: &super::GitHubInlineComment,
        ) -> Result<()> {
            Err(anyhow!("not used in tests"))
        }

        fn pull_request_review_status(
            &self,
            _pr: &ResolvedPullRequestRef,
            _review_id: u64,
        ) -> Result<Option<PostedPullRequestReview>> {
            Err(anyhow!("not used in tests"))
        }
    }

    #[test]
    fn pull_request_ref_parses_short_form() {
        assert_eq!(
            PullRequestRef::from_cli("pr:11").unwrap(),
            PullRequestRef::Number { number: 11 }
        );
    }

    #[test]
    fn pull_request_ref_parses_repo_qualified_form() {
        assert_eq!(
            PullRequestRef::from_cli("pr:jmqd/trueflow/11").unwrap(),
            PullRequestRef::Repository {
                owner: "jmqd".to_string(),
                repo: "trueflow".to_string(),
                number: 11,
            }
        );
    }

    #[test]
    fn pull_request_ref_parses_full_url() {
        assert_eq!(
            PullRequestRef::from_cli("https://github.com/jmqd/trueflow/pull/11").unwrap(),
            PullRequestRef::HostedRepository {
                host: "github.com".to_string(),
                owner: "jmqd".to_string(),
                repo: "trueflow".to_string(),
                number: 11,
            }
        );
    }

    #[test]
    fn pull_request_ref_parses_enterprise_url() {
        assert_eq!(
            PullRequestRef::from_cli("https://github.company.com/jmqd/trueflow/pull/11").unwrap(),
            PullRequestRef::HostedRepository {
                host: "github.company.com".to_string(),
                owner: "jmqd".to_string(),
                repo: "trueflow".to_string(),
                number: 11,
            }
        );
    }

    #[test]
    fn pull_request_ref_rejects_invalid_shape() {
        let err = PullRequestRef::from_cli("pr:jmqd/trueflow").unwrap_err();
        assert!(err.to_string().contains(super::PULL_REQUEST_REFERENCE_HELP));
    }

    #[test]
    fn parse_git_remotes_config_supports_https_and_ssh_urls() {
        let remotes = parse_git_remotes_config(
            "remote.origin.url https://github.com/jmqd/trueflow.git\nremote.upstream.url git@github.company.com:trueflow/trueflow.git\n",
        )
        .unwrap();

        assert_eq!(
            remotes,
            vec![
                GitRemote {
                    name: "origin".to_string(),
                    fetch_url: "https://github.com/jmqd/trueflow.git".to_string(),
                    host: "github.com".to_string(),
                    owner: "jmqd".to_string(),
                    repo: "trueflow".to_string(),
                },
                GitRemote {
                    name: "upstream".to_string(),
                    fetch_url: "git@github.company.com:trueflow/trueflow.git".to_string(),
                    host: "github.company.com".to_string(),
                    owner: "trueflow".to_string(),
                    repo: "trueflow".to_string(),
                },
            ]
        );
    }

    #[test]
    fn resolve_pull_request_ref_uses_origin_for_short_form() {
        let remotes = vec![GitRemote {
            name: "origin".to_string(),
            fetch_url: "git@github.com:jmqd/trueflow.git".to_string(),
            host: "github.com".to_string(),
            owner: "jmqd".to_string(),
            repo: "trueflow".to_string(),
        }];

        let resolved =
            resolve_pull_request_ref(&PullRequestRef::Number { number: 11 }, &remotes).unwrap();
        assert_eq!(
            resolved,
            ResolvedPullRequestRef {
                host: "github.com".to_string(),
                owner: "jmqd".to_string(),
                repo: "trueflow".to_string(),
                number: 11,
            }
        );
    }

    #[test]
    fn select_matching_remote_prefers_origin() {
        let remotes = vec![
            GitRemote {
                name: "mirror".to_string(),
                fetch_url: "git@github.com:jmqd/trueflow.git".to_string(),
                host: "github.com".to_string(),
                owner: "jmqd".to_string(),
                repo: "trueflow".to_string(),
            },
            GitRemote {
                name: "origin".to_string(),
                fetch_url: "https://github.com/jmqd/trueflow.git".to_string(),
                host: "github.com".to_string(),
                owner: "jmqd".to_string(),
                repo: "trueflow".to_string(),
            },
        ];

        let resolved = ResolvedPullRequestRef {
            host: "github.com".to_string(),
            owner: "jmqd".to_string(),
            repo: "trueflow".to_string(),
            number: 11,
        };
        let remote = select_matching_remote(&resolved, &remotes).unwrap();
        assert_eq!(remote.name, "origin");
    }

    #[test]
    fn parse_pull_request_metadata_reads_title_refs_and_commit_summaries() {
        let requested = ResolvedPullRequestRef {
            host: "github.com".to_string(),
            owner: "jmqd".to_string(),
            repo: "trueflow".to_string(),
            number: 11,
        };
        let pull_json = r#"{
            "title": "Add PR review support",
            "commits": 2,
            "base": {
                "ref": "main",
                "sha": "1111111111111111111111111111111111111111",
                "repo": {
                    "name": "trueflow",
                    "owner": { "login": "jmqd" }
                }
            },
            "head": {
                "ref": "feature/pr-review",
                "sha": "3333333333333333333333333333333333333333",
                "repo": null
            }
        }"#;
        let commits_json = r#"[
            {
                "sha": "2222222222222222222222222222222222222222",
                "commit": { "message": "Seed review flow\n\nBody" }
            },
            {
                "sha": "3333333333333333333333333333333333333333",
                "commit": { "message": "Fetch PR refs" }
            }
        ]"#;

        let metadata = parse_pull_request_metadata(&requested, pull_json, commits_json).unwrap();
        assert_eq!(metadata.pr, requested);
        assert_eq!(metadata.title, "Add PR review support");
        assert_eq!(metadata.base_ref, "main");
        assert_eq!(
            metadata.base_sha,
            CommitId::new("1111111111111111111111111111111111111111").unwrap()
        );
        assert_eq!(metadata.head_ref, "feature/pr-review");
        assert_eq!(
            metadata.commits,
            vec![
                PullRequestCommit {
                    sha: CommitId::new("2222222222222222222222222222222222222222").unwrap(),
                    summary: "Seed review flow".to_string(),
                },
                PullRequestCommit {
                    sha: CommitId::new("3333333333333333333333333333333333333333").unwrap(),
                    summary: "Fetch PR refs".to_string(),
                },
            ]
        );
    }

    #[test]
    fn parse_pull_request_metadata_rejects_large_pull_requests() {
        let requested = ResolvedPullRequestRef {
            host: "github.com".to_string(),
            owner: "jmqd".to_string(),
            repo: "trueflow".to_string(),
            number: 11,
        };
        let pull_json = format!(
            r#"{{
                "title": "Big PR",
                "commits": {},
                "base": {{
                    "ref": "main",
                    "sha": "1111111111111111111111111111111111111111",
                    "repo": {{
                        "name": "trueflow",
                        "owner": {{ "login": "jmqd" }}
                    }}
                }},
                "head": {{
                    "ref": "feature",
                    "sha": "2222222222222222222222222222222222222222",
                    "repo": null
                }}
            }}"#,
            GH_MAX_PULL_REQUEST_COMMITS + 1
        );
        let err = parse_pull_request_metadata(&requested, &pull_json, "[]").unwrap_err();
        assert!(err.to_string().contains("currently supports up to"));
    }

    #[test]
    fn prepare_pull_request_review_uses_resolved_remote_and_metadata() {
        let repo_root = temp_git_repo("prepare_pr_review");
        run_git(
            &repo_root,
            &[
                "remote",
                "add",
                "origin",
                "git@github.com:jmqd/trueflow.git",
            ],
        );
        let requested = PullRequestRef::Number { number: 11 };
        let metadata = PullRequestMetadata {
            pr: ResolvedPullRequestRef {
                host: "github.com".to_string(),
                owner: "jmqd".to_string(),
                repo: "trueflow".to_string(),
                number: 11,
            },
            title: "Seed review flow".to_string(),
            base_ref: "main".to_string(),
            base_sha: CommitId::new("1111111111111111111111111111111111111111").unwrap(),
            head_ref: "feature".to_string(),
            head_sha: CommitId::new("2222222222222222222222222222222222222222").unwrap(),
            commits: vec![],
        };
        let client = FakeGitHubClient {
            metadata: metadata.clone(),
        };
        let mut fetch_calls = Vec::new();
        let prepared = prepare_pull_request_review_with(
            &repo_root,
            &requested,
            &client,
            |repo_root, remote_name, metadata| {
                fetch_calls.push((
                    repo_root.to_path_buf(),
                    remote_name.to_string(),
                    metadata.clone(),
                ));
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(prepared.metadata, metadata);
        assert_eq!(prepared.remote.name, "origin");
        assert_eq!(fetch_calls.len(), 1);
        assert_eq!(fetch_calls[0].1, "origin");
    }

    #[test]
    fn fetch_pull_request_refs_fetches_hidden_refs_and_commit_objects() {
        let remote_repo = temp_git_repo("github_pr_fetch_remote");
        let file_path = remote_repo.join("src/lib.rs");
        fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        fs::write(&file_path, "pub fn seed() -> u32 { 1 }\n").unwrap();
        run_git(&remote_repo, &["add", "."]);
        run_git(&remote_repo, &["commit", "-m", "Initial main"]);
        run_git(&remote_repo, &["branch", "-M", "main"]);
        let base_sha = run_git_stdout(&remote_repo, &["rev-parse", "HEAD"])
            .trim()
            .to_string();

        run_git(&remote_repo, &["switch", "-c", "feature/pr-review"]);
        fs::write(&file_path, "pub fn seed() -> u32 { 2 }\n").unwrap();
        run_git(&remote_repo, &["add", "."]);
        run_git(&remote_repo, &["commit", "-m", "Update seed"]);
        let first_pr_sha = run_git_stdout(&remote_repo, &["rev-parse", "HEAD"])
            .trim()
            .to_string();

        fs::write(&file_path, "pub fn seed() -> u32 {\n    3\n}\n").unwrap();
        run_git(&remote_repo, &["add", "."]);
        run_git(&remote_repo, &["commit", "-m", "Expand body"]);
        let head_sha = run_git_stdout(&remote_repo, &["rev-parse", "HEAD"])
            .trim()
            .to_string();
        run_git(
            &remote_repo,
            &["update-ref", "refs/pull/11/head", head_sha.as_str()],
        );
        run_git(&remote_repo, &["switch", "main"]);

        let local_repo = temp_git_repo("github_pr_fetch_local");
        run_git(
            &local_repo,
            &["remote", "add", "origin", remote_repo.to_str().unwrap()],
        );

        let metadata = PullRequestMetadata {
            pr: ResolvedPullRequestRef {
                host: "github.com".to_string(),
                owner: "jmqd".to_string(),
                repo: "trueflow".to_string(),
                number: 11,
            },
            title: "Seed review flow".to_string(),
            base_ref: "main".to_string(),
            base_sha: CommitId::new(&base_sha).unwrap(),
            head_ref: "feature/pr-review".to_string(),
            head_sha: CommitId::new(&head_sha).unwrap(),
            commits: vec![
                PullRequestCommit {
                    sha: CommitId::new(&first_pr_sha).unwrap(),
                    summary: "Update seed".to_string(),
                },
                PullRequestCommit {
                    sha: CommitId::new(&head_sha).unwrap(),
                    summary: "Expand body".to_string(),
                },
            ],
        };

        fetch_pull_request_refs(&local_repo, "origin", &metadata).unwrap();

        assert_eq!(
            run_git_stdout(&local_repo, &["rev-parse", "refs/trueflow/pr/11/head"]).trim(),
            head_sha
        );
        assert_eq!(
            run_git_stdout(&local_repo, &["rev-parse", "refs/trueflow/pr/11/base"]).trim(),
            base_sha
        );
        assert_eq!(
            run_git_stdout(&local_repo, &["rev-parse", first_pr_sha.as_str()]).trim(),
            first_pr_sha
        );
        assert_eq!(
            run_git_stdout(&local_repo, &["rev-parse", head_sha.as_str()]).trim(),
            head_sha
        );
    }
}
