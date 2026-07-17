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
pub const TRUEFLOW_PENDING_REVIEW_MARKER: &str = "<!-- trueflow:pending-review -->";
const TRUEFLOW_DELIVERY_MARKER_OPEN: &str = "<!-- trueflow:delivery:";
const TRUEFLOW_DELIVERY_MARKER_CLOSE: &str = "-->";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHubDeliveryMarker {
    CreatePendingReview {
        operation_id: String,
        head_sha: CommitId,
    },
    ReviewThread {
        operation_id: String,
    },
}

pub fn materialize_pending_review_delivery_body(
    body: &str,
    operation_id: &str,
    head_sha: &CommitId,
) -> Result<String> {
    let owned_body;
    let body = if body.contains(TRUEFLOW_PENDING_REVIEW_MARKER) {
        body
    } else {
        owned_body = format!("{body}\n{TRUEFLOW_PENDING_REVIEW_MARKER}");
        &owned_body
    };
    append_trueflow_delivery_marker(
        body,
        &format!(
            "v1 kind=create-pending-review operation={} head={}",
            validated_delivery_operation_id(operation_id)?,
            head_sha
        ),
    )
}

pub fn materialize_review_thread_delivery_body(body: &str, operation_id: &str) -> Result<String> {
    append_trueflow_delivery_marker(
        body,
        &format!(
            "v1 kind=review-thread operation={}",
            validated_delivery_operation_id(operation_id)?
        ),
    )
}

pub fn parse_trueflow_delivery_marker(body: &str) -> Result<Option<GitHubDeliveryMarker>> {
    let mut remaining = body;
    let mut parsed_marker = None;

    while let Some(marker_start) = remaining.find(TRUEFLOW_DELIVERY_MARKER_OPEN) {
        let marker_body = &remaining[marker_start + TRUEFLOW_DELIVERY_MARKER_OPEN.len()..];
        let marker_end = marker_body
            .find(TRUEFLOW_DELIVERY_MARKER_CLOSE)
            .ok_or_else(|| {
                anyhow!("trueflow delivery marker is missing its closing comment delimiter")
            })?;
        let marker = parse_trueflow_delivery_marker_content(marker_body[..marker_end].trim())?;
        if parsed_marker.replace(marker).is_some() {
            return Err(anyhow!(
                "trueflow delivery body contains multiple delivery markers"
            ));
        }
        remaining = &marker_body[marker_end + TRUEFLOW_DELIVERY_MARKER_CLOSE.len()..];
    }

    Ok(parsed_marker)
}

fn append_trueflow_delivery_marker(body: &str, marker_content: &str) -> Result<String> {
    if parse_trueflow_delivery_marker(body)?.is_some() {
        return Err(anyhow!(
            "trueflow delivery body already contains a delivery marker"
        ));
    }
    Ok(format!(
        "{body}\n<!-- trueflow:delivery:{marker_content} -->"
    ))
}

fn validated_delivery_operation_id(operation_id: &str) -> Result<&str> {
    if operation_id.is_empty()
        || !operation_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Err(anyhow!(
            "trueflow delivery operation id must contain only ASCII letters, digits, or hyphens"
        ));
    }
    Ok(operation_id)
}

fn parse_trueflow_delivery_marker_content(content: &str) -> Result<GitHubDeliveryMarker> {
    let fields = content.split_ascii_whitespace().collect::<Vec<_>>();
    let Some(version) = fields.first() else {
        return Err(anyhow!("trueflow delivery marker is empty"));
    };
    if *version != "v1" {
        return Err(anyhow!(
            "unsupported trueflow delivery marker version {version:?}"
        ));
    }
    let Some(kind) = fields.get(1).and_then(|field| field.strip_prefix("kind=")) else {
        return Err(anyhow!("trueflow delivery marker is missing kind"));
    };
    let Some(operation_id) = fields
        .get(2)
        .and_then(|field| field.strip_prefix("operation="))
    else {
        return Err(anyhow!("trueflow delivery marker is missing operation"));
    };
    let operation_id = validated_delivery_operation_id(operation_id)?.to_string();

    match kind {
        "create-pending-review" if fields.len() == 4 => {
            let Some(head_sha) = fields.get(3).and_then(|field| field.strip_prefix("head=")) else {
                return Err(anyhow!(
                    "trueflow pending-review delivery marker is missing head"
                ));
            };
            Ok(GitHubDeliveryMarker::CreatePendingReview {
                operation_id,
                head_sha: CommitId::new(head_sha)
                    .context("trueflow pending-review delivery marker has invalid head")?,
            })
        }
        "review-thread" if fields.len() == 3 => {
            Ok(GitHubDeliveryMarker::ReviewThread { operation_id })
        }
        "create-pending-review" | "review-thread" => Err(anyhow!(
            "trueflow delivery marker has unexpected fields for {kind}"
        )),
        _ => Err(anyhow!("unknown trueflow delivery marker kind {kind:?}")),
    }
}

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedPullRequestReviewThread {
    pub operation_id: String,
    pub thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubPullRequestDeliverySnapshot {
    pub pr: ResolvedPullRequestRef,
    pub head_sha: CommitId,
    pub reviews: Vec<GitHubPullRequestReviewSnapshot>,
    pub threads: Vec<GitHubPullRequestReviewThreadSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubPullRequestReviewSnapshot {
    pub node_id: String,
    pub database_id: Option<u64>,
    pub state: PullRequestReviewState,
    pub head_sha: Option<CommitId>,
    pub body: String,
    pub html_url: String,
    pub viewer_did_author: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubPullRequestReviewThreadSnapshot {
    pub node_id: String,
    pub review_node_id: Option<String>,
    pub path: String,
    pub line: Option<u32>,
    pub side: Option<GitHubCommentSide>,
    pub start_line: Option<u32>,
    pub start_side: Option<GitHubCommentSide>,
    pub comments: Vec<GitHubPullRequestReviewThreadCommentSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubPullRequestReviewThreadCommentSnapshot {
    pub node_id: String,
    pub body: String,
    pub state: GitHubPullRequestReviewCommentState,
    pub review_node_id: Option<String>,
    pub reply_to_node_id: Option<String>,
    pub viewer_did_author: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitHubPullRequestReviewCommentState {
    Pending,
    Submitted,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "UPPERCASE")]
enum PullRequestReviewThreadSubjectType {
    Line,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AddPullRequestReviewThreadInput<'a> {
    pull_request_review_id: &'a str,
    body: &'a str,
    path: &'a str,
    line: u32,
    side: GitHubCommentSide,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_side: Option<GitHubCommentSide>,
    subject_type: PullRequestReviewThreadSubjectType,
    client_mutation_id: &'a str,
}

const ADD_PULL_REQUEST_REVIEW_THREAD_MUTATION: &str = r#"
    mutation AddTrueflowPullRequestReviewThread($input: AddPullRequestReviewThreadInput!) {
        addPullRequestReviewThread(input: $input) {
            clientMutationId
            thread { id }
        }
    }
"#;

fn build_add_pull_request_review_thread_request(
    review_node_id: &str,
    operation_id: &str,
    comment: &GitHubInlineComment,
) -> Result<String> {
    if operation_id.trim().is_empty() {
        return Err(anyhow!(
            "GitHub pull request review thread operation id cannot be blank"
        ));
    }

    match (comment.start_line, comment.start_side) {
        (None, None) => {}
        (Some(start_line), Some(_)) if start_line <= comment.line => {}
        (Some(_), Some(_)) => {
            return Err(anyhow!(
                "GitHub inline comment range starts after its ending line"
            ));
        }
        _ => {
            return Err(anyhow!(
                "GitHub inline comment ranges must specify start_line and start_side together"
            ));
        }
    }

    let input = AddPullRequestReviewThreadInput {
        pull_request_review_id: review_node_id,
        body: &comment.body,
        path: comment.path.as_str(),
        line: comment.line,
        side: comment.side,
        start_line: comment.start_line,
        start_side: comment.start_side,
        subject_type: PullRequestReviewThreadSubjectType::Line,
        client_mutation_id: operation_id,
    };
    serde_json::to_string(&serde_json::json!({
        "query": ADD_PULL_REQUEST_REVIEW_THREAD_MUTATION,
        "variables": { "input": input },
    }))
    .with_context(|| "failed to serialize GitHub pull request review thread request")
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

impl PullRequestReviewState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Commented | Self::Approved | Self::ChangesRequested | Self::Dismissed
        )
    }
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
        operation_id: &str,
    ) -> Result<PostedPullRequestReviewThread>;
    fn update_pending_pull_request_review_body(
        &self,
        _pr: &ResolvedPullRequestRef,
        _review_id: u64,
        _body: &str,
    ) -> Result<PostedPullRequestReview> {
        Err(anyhow!(
            "this GitHub client does not support pending review body updates"
        ))
    }
    fn pull_request_delivery_snapshot(
        &self,
        pr: &ResolvedPullRequestRef,
    ) -> Result<GitHubPullRequestDeliverySnapshot>;
    fn submit_pending_pull_request_review(
        &self,
        pr: &ResolvedPullRequestRef,
        review_id: u64,
    ) -> Result<PostedPullRequestReview>;
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
        let marker = parse_trueflow_delivery_marker(&draft.body)?
            .context("pending review draft did not include a trueflow delivery marker")?;
        let GitHubDeliveryMarker::CreatePendingReview {
            operation_id,
            head_sha: marker_head_sha,
        } = marker
        else {
            return Err(anyhow!(
                "pending review draft included a review-thread delivery marker"
            ));
        };
        if marker_head_sha != *head_sha {
            return Err(anyhow!(
                "pending review delivery marker head {marker_head_sha} did not match requested head {head_sha}"
            ));
        }

        let endpoint = format!("repos/{}/{}/pulls/{}/reviews", pr.owner, pr.repo, pr.number);
        let body = serde_json::to_string(&serde_json::json!({
            "body": draft.body,
            "commit_id": head_sha,
            "comments": draft.comments,
        }))?;
        let response = run_gh_api_with_body(&pr.host, "POST", &endpoint, &body)?;
        parse_created_pending_pull_request_review(&response, &operation_id, head_sha)
    }

    fn add_comment_to_pending_pull_request_review(
        &self,
        pr: &ResolvedPullRequestRef,
        review: &PostedPullRequestReview,
        comment: &GitHubInlineComment,
        operation_id: &str,
    ) -> Result<PostedPullRequestReviewThread> {
        let review_node_id = review.node_id.as_ref().ok_or_else(|| {
            anyhow!(
                "GitHub review {} did not include a GraphQL node id; cannot append comments",
                review.id
            )
        })?;
        let body =
            build_add_pull_request_review_thread_request(review_node_id, operation_id, comment)?;
        let response = run_gh_api_with_body(&pr.host, "POST", "graphql", &body)?;
        parse_add_pull_request_review_thread_response(&response, operation_id)
    }

    fn update_pending_pull_request_review_body(
        &self,
        pr: &ResolvedPullRequestRef,
        review_id: u64,
        body: &str,
    ) -> Result<PostedPullRequestReview> {
        if review_id == 0 {
            return Err(anyhow!(
                "cannot update a pending review with a zero database id"
            ));
        }
        if body.trim().is_empty() {
            return Err(anyhow!("cannot update a pending review with a blank body"));
        }
        let endpoint = format!(
            "repos/{}/{}/pulls/{}/reviews/{review_id}",
            pr.owner, pr.repo, pr.number
        );
        let request = serde_json::to_string(&serde_json::json!({ "body": body }))?;
        let response = run_gh_api_with_body(&pr.host, "PATCH", &endpoint, &request)?;
        let review = parse_posted_pull_request_review(&response)?;
        if review.id != review_id {
            return Err(anyhow!(
                "GitHub updated pending review {} but acknowledged review {}",
                review_id,
                review.id
            ));
        }
        if review.state != PullRequestReviewState::Pending {
            return Err(anyhow!(
                "GitHub review {} was no longer pending after its body update",
                review_id
            ));
        }
        Ok(review)
    }
    fn pull_request_delivery_snapshot(
        &self,
        pr: &ResolvedPullRequestRef,
    ) -> Result<GitHubPullRequestDeliverySnapshot> {
        let number = i32::try_from(pr.number).context("pull request number exceeds GraphQL Int")?;
        let head_sha = fetch_pull_request_delivery_head(pr, number)?;
        let reviews = fetch_pull_request_delivery_reviews(pr, number)?;
        let mut threads = fetch_pull_request_delivery_threads(pr, number)?;
        for thread in &mut threads {
            thread.comments = fetch_pull_request_delivery_thread_comments(pr, &thread.node_id)?;
            thread.review_node_id = Some(pull_request_delivery_thread_review_node_id(
                &thread.node_id,
                &thread.comments,
            )?);
        }

        Ok(GitHubPullRequestDeliverySnapshot {
            pr: pr.clone(),
            head_sha,
            reviews,
            threads,
        })
    }

    fn submit_pending_pull_request_review(
        &self,
        pr: &ResolvedPullRequestRef,
        review_id: u64,
    ) -> Result<PostedPullRequestReview> {
        let endpoint = format!(
            "repos/{}/{}/pulls/{}/reviews/{review_id}/events",
            pr.owner, pr.repo, pr.number
        );
        let body = serde_json::to_string(&serde_json::json!({
            "event": "COMMENT",
        }))?;
        let response = run_gh_api_with_body(&pr.host, "POST", &endpoint, &body)?;
        parse_posted_pull_request_review(&response)
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
    if commits.len() != pull.commit_count {
        return Err(anyhow!(
            "expected {} pull request commits, but GitHub returned {}",
            pull.commit_count,
            commits.len()
        ));
    }
    let final_commit_sha = commits
        .last()
        .map(|commit| commit.sha.as_str())
        .unwrap_or_default();
    if final_commit_sha != pull.head.sha {
        return Err(anyhow!(
            "expected final pull request commit to be head {}, but GitHub returned {}",
            pull.head.sha,
            final_commit_sha
        ));
    }

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
    if gh_api_output_is_not_found(&stdout, &stderr) {
        return Ok(None);
    }

    Err(anyhow!(
        "gh api {endpoint} failed for host {host}: {stdout}{stderr}"
    ))
}

fn gh_api_output_is_not_found(stdout: &str, stderr: &str) -> bool {
    stdout
        .lines()
        .chain(stderr.lines())
        .any(gh_api_line_is_not_found)
}

fn gh_api_line_is_not_found(line: &str) -> bool {
    let line = line.trim();
    line.contains("HTTP 404") || line.contains("404 Not Found")
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

const PULL_REQUEST_DELIVERY_HEAD_QUERY: &str = r#"
    query TrueflowPullRequestDeliveryHead($owner: String!, $repo: String!, $number: Int!) {
        repository(owner: $owner, name: $repo) {
            pullRequest(number: $number) {
                headRefOid
            }
        }
    }
"#;
const PULL_REQUEST_DELIVERY_REVIEWS_QUERY: &str = r#"
    query TrueflowPullRequestDeliveryReviews(
        $owner: String!, $repo: String!, $number: Int!, $after: String
    ) {
        repository(owner: $owner, name: $repo) {
            pullRequest(number: $number) {
                reviews(first: 100, after: $after) {
                    nodes {
                        id
                        fullDatabaseId
                        url
                        body
                        state
                        viewerDidAuthor
                        commit { oid }
                    }
                    pageInfo { hasNextPage endCursor }
                }
            }
        }
    }
"#;
const PULL_REQUEST_DELIVERY_THREADS_QUERY: &str = r#"
    query TrueflowPullRequestDeliveryThreads(
        $owner: String!, $repo: String!, $number: Int!, $after: String
    ) {
        repository(owner: $owner, name: $repo) {
            pullRequest(number: $number) {
                reviewThreads(first: 100, after: $after) {
                    nodes {
                        id
                        path
                        line
                        diffSide
                        startLine
                        startDiffSide
                    }
                    pageInfo { hasNextPage endCursor }
                }
            }
        }
    }
"#;
const PULL_REQUEST_DELIVERY_THREAD_COMMENTS_QUERY: &str = r#"
    query TrueflowPullRequestDeliveryThreadComments($threadId: ID!, $after: String) {
        node(id: $threadId) {
            ... on PullRequestReviewThread {
                comments(first: 100, after: $after) {
                    nodes {
                        id
                        body
                        state
                        viewerDidAuthor
                        pullRequestReview { id }
                        replyTo { id }
                    }
                    pageInfo { hasNextPage endCursor }
                }
            }
        }
    }
"#;

fn fetch_pull_request_delivery_head(pr: &ResolvedPullRequestRef, number: i32) -> Result<CommitId> {
    let raw = run_github_graphql(
        &pr.host,
        PULL_REQUEST_DELIVERY_HEAD_QUERY,
        &serde_json::json!({
            "owner": pr.owner.as_str(),
            "repo": pr.repo.as_str(),
            "number": number,
        }),
    )?;
    parse_pull_request_delivery_head_response(&raw)
}

fn fetch_pull_request_delivery_reviews(
    pr: &ResolvedPullRequestRef,
    number: i32,
) -> Result<Vec<GitHubPullRequestReviewSnapshot>> {
    let mut after = None;
    let mut reviews = Vec::new();
    loop {
        let raw = run_github_graphql(
            &pr.host,
            PULL_REQUEST_DELIVERY_REVIEWS_QUERY,
            &serde_json::json!({
                "owner": pr.owner.as_str(),
                "repo": pr.repo.as_str(),
                "number": number,
                "after": after,
            }),
        )?;
        let (mut page, next) = parse_pull_request_delivery_reviews_page(&raw)?;
        reviews.append(&mut page);
        let Some(next) = next else {
            return Ok(reviews);
        };
        after = Some(next);
    }
}

fn fetch_pull_request_delivery_threads(
    pr: &ResolvedPullRequestRef,
    number: i32,
) -> Result<Vec<GitHubPullRequestReviewThreadSnapshot>> {
    let mut after = None;
    let mut threads = Vec::new();
    loop {
        let raw = run_github_graphql(
            &pr.host,
            PULL_REQUEST_DELIVERY_THREADS_QUERY,
            &serde_json::json!({
                "owner": pr.owner.as_str(),
                "repo": pr.repo.as_str(),
                "number": number,
                "after": after,
            }),
        )?;
        let (mut page, next) = parse_pull_request_delivery_threads_page(&raw)?;
        threads.append(&mut page);
        let Some(next) = next else {
            return Ok(threads);
        };
        after = Some(next);
    }
}

fn fetch_pull_request_delivery_thread_comments(
    pr: &ResolvedPullRequestRef,
    thread_id: &str,
) -> Result<Vec<GitHubPullRequestReviewThreadCommentSnapshot>> {
    let mut after = None;
    let mut comments = Vec::new();
    loop {
        let raw = run_github_graphql(
            &pr.host,
            PULL_REQUEST_DELIVERY_THREAD_COMMENTS_QUERY,
            &serde_json::json!({
                "threadId": thread_id,
                "after": after,
            }),
        )?;
        let (mut page, next) = parse_pull_request_delivery_thread_comments_page(&raw)?;
        comments.append(&mut page);
        let Some(next) = next else {
            return Ok(comments);
        };
        after = Some(next);
    }
}

fn run_github_graphql(host: &str, query: &str, variables: &serde_json::Value) -> Result<String> {
    let body = serde_json::to_string(&serde_json::json!({
        "query": query,
        "variables": variables,
    }))
    .with_context(|| "failed to serialize GitHub GraphQL request")?;
    run_gh_api_with_body(host, "POST", "graphql", &body)
}

#[derive(Deserialize)]
struct PullRequestDeliveryHeadResponse {
    data: Option<PullRequestDeliveryHeadData>,
}

#[derive(Deserialize)]
struct PullRequestDeliveryHeadData {
    repository: Option<PullRequestDeliveryHeadRepository>,
}

#[derive(Deserialize)]
struct PullRequestDeliveryHeadRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<PullRequestDeliveryHead>,
}

#[derive(Deserialize)]
struct PullRequestDeliveryHead {
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
}

#[derive(Deserialize)]
struct PullRequestDeliveryReviewsResponse {
    data: Option<PullRequestDeliveryReviewsData>,
}

#[derive(Deserialize)]
struct PullRequestDeliveryReviewsData {
    repository: Option<PullRequestDeliveryReviewsRepository>,
}

#[derive(Deserialize)]
struct PullRequestDeliveryReviewsRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<PullRequestDeliveryReviewsPullRequest>,
}

#[derive(Deserialize)]
struct PullRequestDeliveryReviewsPullRequest {
    reviews: PullRequestDeliveryConnection<RawPullRequestDeliveryReview>,
}

#[derive(Deserialize)]
struct PullRequestDeliveryThreadsResponse {
    data: Option<PullRequestDeliveryThreadsData>,
}

#[derive(Deserialize)]
struct PullRequestDeliveryThreadsData {
    repository: Option<PullRequestDeliveryThreadsRepository>,
}

#[derive(Deserialize)]
struct PullRequestDeliveryThreadsRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<PullRequestDeliveryThreadsPullRequest>,
}

#[derive(Deserialize)]
struct PullRequestDeliveryThreadsPullRequest {
    #[serde(rename = "reviewThreads")]
    review_threads: PullRequestDeliveryConnection<RawPullRequestDeliveryThread>,
}

#[derive(Deserialize)]
struct PullRequestDeliveryThreadCommentsResponse {
    data: Option<PullRequestDeliveryThreadCommentsData>,
}

#[derive(Deserialize)]
struct PullRequestDeliveryThreadCommentsData {
    node: Option<PullRequestDeliveryThreadCommentsNode>,
}

#[derive(Deserialize)]
struct PullRequestDeliveryThreadCommentsNode {
    comments: PullRequestDeliveryConnection<RawPullRequestDeliveryThreadComment>,
}

#[derive(Deserialize)]
struct PullRequestDeliveryConnection<T> {
    nodes: Vec<T>,
    #[serde(rename = "pageInfo")]
    page_info: PullRequestDeliveryPageInfo,
}

#[derive(Deserialize)]
struct PullRequestDeliveryPageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
struct RawPullRequestDeliveryReview {
    id: String,
    #[serde(rename = "fullDatabaseId")]
    database_id: Option<u64>,
    url: String,
    body: String,
    state: String,
    #[serde(rename = "viewerDidAuthor")]
    viewer_did_author: bool,
    commit: Option<RawPullRequestDeliveryCommit>,
}

#[derive(Deserialize)]
struct RawPullRequestDeliveryCommit {
    oid: String,
}

#[derive(Deserialize)]
struct RawPullRequestDeliveryThread {
    id: String,
    path: String,
    line: Option<u32>,
    #[serde(rename = "diffSide")]
    side: Option<GitHubCommentSide>,
    #[serde(rename = "startLine")]
    start_line: Option<u32>,
    #[serde(rename = "startDiffSide")]
    start_side: Option<GitHubCommentSide>,
}

#[derive(Deserialize)]
struct RawPullRequestDeliveryThreadComment {
    id: String,
    body: String,
    state: String,
    #[serde(rename = "viewerDidAuthor")]
    viewer_did_author: bool,
    #[serde(rename = "pullRequestReview")]
    review: Option<RawPullRequestDeliveryNode>,
    #[serde(rename = "replyTo")]
    reply_to: Option<RawPullRequestDeliveryNode>,
}

#[derive(Deserialize)]
struct RawPullRequestDeliveryNode {
    id: String,
}

fn parse_pull_request_delivery_head_response(raw: &str) -> Result<CommitId> {
    ensure_graphql_response_success(raw)?;
    let response: PullRequestDeliveryHeadResponse = serde_json::from_str(raw)
        .with_context(|| "failed to parse GitHub GraphQL delivery-head response JSON")?;
    let head_ref_oid = response
        .data
        .and_then(|data| data.repository)
        .and_then(|repository| repository.pull_request)
        .map(|pull_request| pull_request.head_ref_oid)
        .context("GitHub GraphQL delivery-head response did not include data.repository.pullRequest.headRefOid")?;
    CommitId::new(&head_ref_oid)
        .context("GitHub GraphQL delivery-head response included an invalid headRefOid")
}

fn parse_pull_request_delivery_reviews_page(
    raw: &str,
) -> Result<(Vec<GitHubPullRequestReviewSnapshot>, Option<String>)> {
    ensure_graphql_response_success(raw)?;
    let response: PullRequestDeliveryReviewsResponse = serde_json::from_str(raw)
        .with_context(|| "failed to parse GitHub GraphQL delivery-reviews response JSON")?;
    let connection = response
        .data
        .and_then(|data| data.repository)
        .and_then(|repository| repository.pull_request)
        .map(|pull_request| pull_request.reviews)
        .context("GitHub GraphQL delivery-reviews response did not include data.repository.pullRequest.reviews")?;
    let next = next_pull_request_delivery_page_cursor(connection.page_info, "reviews")?;
    let reviews = connection
        .nodes
        .into_iter()
        .map(snapshot_pull_request_delivery_review)
        .collect::<Result<Vec<_>>>()?;
    Ok((reviews, next))
}

fn parse_pull_request_delivery_threads_page(
    raw: &str,
) -> Result<(Vec<GitHubPullRequestReviewThreadSnapshot>, Option<String>)> {
    ensure_graphql_response_success(raw)?;
    let response: PullRequestDeliveryThreadsResponse = serde_json::from_str(raw)
        .with_context(|| "failed to parse GitHub GraphQL delivery-threads response JSON")?;
    let connection = response
        .data
        .and_then(|data| data.repository)
        .and_then(|repository| repository.pull_request)
        .map(|pull_request| pull_request.review_threads)
        .context("GitHub GraphQL delivery-threads response did not include data.repository.pullRequest.reviewThreads")?;
    let next = next_pull_request_delivery_page_cursor(connection.page_info, "review threads")?;
    let threads = connection
        .nodes
        .iter()
        .map(snapshot_pull_request_delivery_thread)
        .collect::<Result<Vec<_>>>()?;
    Ok((threads, next))
}

fn parse_pull_request_delivery_thread_comments_page(
    raw: &str,
) -> Result<(
    Vec<GitHubPullRequestReviewThreadCommentSnapshot>,
    Option<String>,
)> {
    ensure_graphql_response_success(raw)?;
    let response: PullRequestDeliveryThreadCommentsResponse = serde_json::from_str(raw)
        .with_context(|| "failed to parse GitHub GraphQL delivery-thread-comments response JSON")?;
    let connection = response
        .data
        .and_then(|data| data.node)
        .map(|thread| thread.comments)
        .context(
            "GitHub GraphQL delivery-thread-comments response did not include data.node.comments",
        )?;
    let next = next_pull_request_delivery_page_cursor(connection.page_info, "thread comments")?;
    let comments = connection
        .nodes
        .into_iter()
        .map(snapshot_pull_request_delivery_thread_comment)
        .collect::<Result<Vec<_>>>()?;
    Ok((comments, next))
}

fn next_pull_request_delivery_page_cursor(
    page_info: PullRequestDeliveryPageInfo,
    connection: &str,
) -> Result<Option<String>> {
    if !page_info.has_next_page {
        return Ok(None);
    }
    page_info
        .end_cursor
        .map(|cursor| cursor.trim().to_string())
        .filter(|cursor| !cursor.is_empty())
        .map(Some)
        .context(format!(
            "GitHub GraphQL {connection} page reported hasNextPage without a nonblank endCursor"
        ))
}

fn snapshot_pull_request_delivery_review(
    review: RawPullRequestDeliveryReview,
) -> Result<GitHubPullRequestReviewSnapshot> {
    Ok(GitHubPullRequestReviewSnapshot {
        node_id: nonblank_pull_request_delivery_value(&review.id, "review id")?,
        database_id: review.database_id,
        state: parse_pull_request_review_state(Some(&review.state)),
        head_sha: review
            .commit
            .map(|commit| {
                CommitId::new(&commit.oid)
                    .context("GitHub GraphQL delivery review included an invalid commit oid")
            })
            .transpose()?,
        body: review.body,
        html_url: nonblank_pull_request_delivery_value(&review.url, "review url")?,
        viewer_did_author: review.viewer_did_author,
    })
}

fn snapshot_pull_request_delivery_thread(
    thread: &RawPullRequestDeliveryThread,
) -> Result<GitHubPullRequestReviewThreadSnapshot> {
    if thread.line.is_some() != thread.side.is_some() {
        return Err(anyhow!(
            "GitHub GraphQL delivery thread included only one of line or diff side"
        ));
    }
    if thread.start_line.is_some() != thread.start_side.is_some() {
        return Err(anyhow!(
            "GitHub GraphQL delivery thread included only one of start line or start diff side"
        ));
    }
    if let (Some(start_line), Some(line)) = (thread.start_line, thread.line)
        && start_line > line
    {
        return Err(anyhow!(
            "GitHub GraphQL delivery thread start line was after its ending line"
        ));
    }

    Ok(GitHubPullRequestReviewThreadSnapshot {
        node_id: nonblank_pull_request_delivery_value(&thread.id, "thread id")?,
        review_node_id: None,
        path: nonblank_pull_request_delivery_value(&thread.path, "thread path")?,
        line: thread.line,
        side: thread.side,
        start_line: thread.start_line,
        start_side: thread.start_side,
        comments: Vec::new(),
    })
}

fn snapshot_pull_request_delivery_thread_comment(
    comment: RawPullRequestDeliveryThreadComment,
) -> Result<GitHubPullRequestReviewThreadCommentSnapshot> {
    Ok(GitHubPullRequestReviewThreadCommentSnapshot {
        node_id: nonblank_pull_request_delivery_value(&comment.id, "thread comment id")?,
        body: comment.body,
        state: parse_pull_request_review_comment_state(&comment.state),
        review_node_id: optional_nonblank_pull_request_delivery_value(
            comment.review.map(|review| review.id),
            "thread comment review id",
        )?,
        reply_to_node_id: optional_nonblank_pull_request_delivery_value(
            comment.reply_to.map(|reply_to| reply_to.id),
            "thread comment reply id",
        )?,
        viewer_did_author: comment.viewer_did_author,
    })
}

fn pull_request_delivery_thread_review_node_id(
    thread_id: &str,
    comments: &[GitHubPullRequestReviewThreadCommentSnapshot],
) -> Result<String> {
    let mut roots = comments
        .iter()
        .filter(|comment| comment.reply_to_node_id.is_none());
    let root = roots.next().ok_or_else(|| {
        anyhow!("GitHub GraphQL delivery thread {thread_id} did not include a root comment")
    })?;
    if roots.next().is_some() {
        return Err(anyhow!(
            "GitHub GraphQL delivery thread {thread_id} included multiple root comments"
        ));
    }
    root.review_node_id.clone().ok_or_else(|| {
        anyhow!(
            "GitHub GraphQL delivery thread {thread_id} root comment did not include a pull request review id"
        )
    })
}

fn nonblank_pull_request_delivery_value(value: &str, field: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(anyhow!(
            "GitHub GraphQL delivery snapshot included a blank {field}"
        ));
    }
    Ok(value.to_string())
}

fn optional_nonblank_pull_request_delivery_value(
    value: Option<String>,
    field: &str,
) -> Result<Option<String>> {
    value
        .map(|value| nonblank_pull_request_delivery_value(&value, field))
        .transpose()
}

fn parse_pull_request_review_comment_state(value: &str) -> GitHubPullRequestReviewCommentState {
    match value {
        "PENDING" => GitHubPullRequestReviewCommentState::Pending,
        "SUBMITTED" => GitHubPullRequestReviewCommentState::Submitted,
        _ => GitHubPullRequestReviewCommentState::Unknown,
    }
}
fn ensure_graphql_response_success(raw: &str) -> Result<()> {
    let response: serde_json::Value = serde_json::from_str(raw)
        .with_context(|| "failed to parse GitHub GraphQL response JSON".to_string())?;
    let Some(errors) = response.get("errors") else {
        return Ok(());
    };
    let Some(errors) = errors.as_array() else {
        return Err(anyhow!(
            "GitHub GraphQL response had non-array errors field"
        ));
    };
    if errors.is_empty() {
        return Ok(());
    }

    let messages = errors
        .iter()
        .map(|error| {
            error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| error.to_string())
        })
        .collect::<Vec<_>>();
    Err(anyhow!(
        "GitHub GraphQL request failed: {}",
        messages.join("; ")
    ))
}

#[derive(Deserialize)]
struct AddPullRequestReviewThreadGraphqlResponse {
    data: Option<AddPullRequestReviewThreadGraphqlData>,
}

#[derive(Deserialize)]
struct AddPullRequestReviewThreadGraphqlData {
    #[serde(rename = "addPullRequestReviewThread")]
    add_pull_request_review_thread: Option<AddPullRequestReviewThreadGraphqlPayload>,
}

#[derive(Deserialize)]
struct AddPullRequestReviewThreadGraphqlPayload {
    #[serde(rename = "clientMutationId")]
    client_mutation_id: Option<String>,
    thread: Option<AddPullRequestReviewThreadGraphqlThread>,
}

#[derive(Deserialize)]
struct AddPullRequestReviewThreadGraphqlThread {
    id: Option<String>,
}

fn parse_add_pull_request_review_thread_response(
    raw: &str,
    operation_id: &str,
) -> Result<PostedPullRequestReviewThread> {
    ensure_graphql_response_success(raw)?;
    let response: AddPullRequestReviewThreadGraphqlResponse = serde_json::from_str(raw)
        .with_context(|| "failed to parse GitHub GraphQL response JSON".to_string())?;
    let payload = response
        .data
        .and_then(|data| data.add_pull_request_review_thread)
        .context("GitHub GraphQL response did not include data.addPullRequestReviewThread")?;
    let returned_operation_id = payload.client_mutation_id.context(
        "GitHub GraphQL response did not include addPullRequestReviewThread.clientMutationId",
    )?;
    if returned_operation_id != operation_id {
        return Err(anyhow!(
            "GitHub GraphQL response addPullRequestReviewThread.clientMutationId did not match the requested operation id"
        ));
    }
    let thread_id = payload
        .thread
        .and_then(|thread| thread.id)
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .context("GitHub GraphQL response did not include addPullRequestReviewThread.thread.id")?;

    Ok(PostedPullRequestReviewThread {
        operation_id: operation_id.to_string(),
        thread_id,
    })
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

pub fn parse_created_pending_pull_request_review(
    raw: &str,
    expected_operation_id: &str,
    expected_head_sha: &CommitId,
) -> Result<PostedPullRequestReview> {
    validated_delivery_operation_id(expected_operation_id)?;
    let review: PullRequestReviewApiResponse = serde_json::from_str(raw)
        .with_context(|| "failed to parse pull request review JSON".to_string())?;
    if review.id == 0 {
        return Err(anyhow!(
            "GitHub created pending review acknowledgement included a zero database id"
        ));
    }
    if review.state.as_deref() != Some("PENDING") {
        return Err(anyhow!(
            "GitHub created review {} was not PENDING",
            review.id
        ));
    }
    if review.html_url.trim().is_empty() {
        return Err(anyhow!(
            "GitHub created pending review acknowledgement included a blank URL"
        ));
    }
    let node_id = review
        .node_id
        .map(|node_id| node_id.trim().to_string())
        .filter(|node_id| !node_id.is_empty())
        .context("GitHub created pending review acknowledgement included a blank node id")?;
    let body = review
        .body
        .context("GitHub created pending review acknowledgement did not include a body")?;
    if !body.contains(TRUEFLOW_PENDING_REVIEW_MARKER) {
        return Err(anyhow!(
            "GitHub created pending review acknowledgement did not include the trueflow pending-review marker"
        ));
    }
    let marker = parse_trueflow_delivery_marker(&body)?.context(
        "GitHub created pending review acknowledgement did not include a delivery marker",
    )?;
    let GitHubDeliveryMarker::CreatePendingReview {
        operation_id,
        head_sha,
    } = marker
    else {
        return Err(anyhow!(
            "GitHub created pending review acknowledgement contained a review-thread delivery marker"
        ));
    };
    if operation_id != expected_operation_id {
        return Err(anyhow!(
            "GitHub created pending review acknowledgement delivery operation id did not match the request"
        ));
    }
    if head_sha != *expected_head_sha {
        return Err(anyhow!(
            "GitHub created pending review acknowledgement delivery head did not match the request"
        ));
    }
    if let Some(commit_id) = review.commit_id {
        let commit_id = CommitId::new(&commit_id)
            .context("GitHub created pending review acknowledgement had an invalid commit id")?;
        if commit_id != *expected_head_sha {
            return Err(anyhow!(
                "GitHub created pending review acknowledgement commit id did not match the request head"
            ));
        }
    }

    Ok(PostedPullRequestReview {
        id: review.id,
        html_url: review.html_url,
        state: PullRequestReviewState::Pending,
        body,
        node_id: Some(node_id),
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
    if host.trim().is_empty() {
        return None;
    }
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
    if segments.next().is_some() {
        return None;
    }
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
    commit_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{
        GH_MAX_PULL_REQUEST_COMMITS, GitHubReviewDraft, GitRemote, PostedPullRequestReview,
        PullRequestCommit, PullRequestMetadata, PullRequestRef, ResolvedPullRequestRef,
        ensure_graphql_response_success, fetch_pull_request_refs, parse_git_remotes_config,
        parse_pull_request_metadata, prepare_pull_request_review_with, resolve_pull_request_ref,
        select_matching_remote,
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
            _operation_id: &str,
        ) -> Result<super::PostedPullRequestReviewThread> {
            Err(anyhow!("not used in tests"))
        }
        fn pull_request_delivery_snapshot(
            &self,
            pr: &ResolvedPullRequestRef,
        ) -> Result<super::GitHubPullRequestDeliverySnapshot> {
            Ok(super::GitHubPullRequestDeliverySnapshot {
                pr: pr.clone(),
                head_sha: self.metadata.head_sha.clone(),
                reviews: Vec::new(),
                threads: Vec::new(),
            })
        }

        fn submit_pending_pull_request_review(
            &self,
            _pr: &ResolvedPullRequestRef,
            _review_id: u64,
        ) -> Result<PostedPullRequestReview> {
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
    fn graphql_response_success_accepts_data_without_errors() {
        ensure_graphql_response_success(r#"{"data":{"ok":true}}"#).unwrap();
    }

    #[test]
    fn graphql_response_success_rejects_errors_array() {
        let err = ensure_graphql_response_success(
            r#"{"data":null,"errors":[{"message":"Field 'line' doesn't exist"},{"message":"bad input"}]}"#,
        )
        .unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("GitHub GraphQL request failed"));
        assert!(rendered.contains("Field 'line' doesn't exist"));
        assert!(rendered.contains("bad input"));
    }

    #[test]
    fn graphql_response_success_rejects_invalid_json() {
        let err = ensure_graphql_response_success("not json").unwrap_err();
        assert!(
            err.to_string()
                .contains("failed to parse GitHub GraphQL response JSON")
        );
    }
    fn inline_comment(
        line: u32,
        side: super::GitHubCommentSide,
        start_line: Option<u32>,
        start_side: Option<super::GitHubCommentSide>,
    ) -> super::GitHubInlineComment {
        super::GitHubInlineComment {
            path: crate::repo_path::RepoPath::new("src/lib.rs").unwrap(),
            line,
            side,
            start_line,
            start_side,
            body: "Please revise this.".to_string(),
        }
    }

    #[test]
    fn add_review_thread_serializes_single_line_input_without_range_start() {
        let request = super::build_add_pull_request_review_thread_request(
            "PRR_node",
            "append-operation-1",
            &inline_comment(11, super::GitHubCommentSide::Right, None, None),
        )
        .unwrap();
        let request: serde_json::Value = serde_json::from_str(&request).unwrap();

        let query = request["query"].as_str().unwrap();
        assert!(query.contains(
            "mutation AddTrueflowPullRequestReviewThread($input: AddPullRequestReviewThreadInput!)"
        ));
        assert!(query.contains("addPullRequestReviewThread(input: $input)"));
        assert!(query.contains("clientMutationId"));
        assert!(query.contains("thread { id }"));
        assert_eq!(
            request["variables"]["input"],
            serde_json::json!({
                "pullRequestReviewId": "PRR_node",
                "body": "Please revise this.",
                "path": "src/lib.rs",
                "line": 11,
                "side": "RIGHT",
                "subjectType": "LINE",
                "clientMutationId": "append-operation-1",
            })
        );
        assert!(request["variables"]["input"].get("startLine").is_none());
        assert!(request["variables"]["input"].get("startSide").is_none());
    }

    #[test]
    fn add_review_thread_serializes_multiline_input_with_paired_start_and_sides() {
        let request = super::build_add_pull_request_review_thread_request(
            "PRR_node",
            "append-operation-2",
            &inline_comment(
                14,
                super::GitHubCommentSide::Left,
                Some(9),
                Some(super::GitHubCommentSide::Right),
            ),
        )
        .unwrap();
        let request: serde_json::Value = serde_json::from_str(&request).unwrap();

        assert_eq!(
            request["variables"]["input"],
            serde_json::json!({
                "pullRequestReviewId": "PRR_node",
                "body": "Please revise this.",
                "path": "src/lib.rs",
                "line": 14,
                "side": "LEFT",
                "startLine": 9,
                "startSide": "RIGHT",
                "subjectType": "LINE",
                "clientMutationId": "append-operation-2",
            })
        );
    }

    #[test]
    fn add_review_thread_rejects_half_and_inverted_ranges_before_request_dispatch() {
        let half_range = super::build_add_pull_request_review_thread_request(
            "PRR_node",
            "append-operation-3",
            &inline_comment(11, super::GitHubCommentSide::Right, Some(9), None),
        )
        .unwrap_err();
        assert!(
            half_range
                .to_string()
                .contains("must specify start_line and start_side together")
        );
        let other_half_range = super::build_add_pull_request_review_thread_request(
            "PRR_node",
            "append-operation-3b",
            &inline_comment(
                11,
                super::GitHubCommentSide::Right,
                None,
                Some(super::GitHubCommentSide::Right),
            ),
        )
        .unwrap_err();
        assert!(
            other_half_range
                .to_string()
                .contains("must specify start_line and start_side together")
        );

        let inverted_range = super::build_add_pull_request_review_thread_request(
            "PRR_node",
            "append-operation-4",
            &inline_comment(
                11,
                super::GitHubCommentSide::Right,
                Some(12),
                Some(super::GitHubCommentSide::Right),
            ),
        )
        .unwrap_err();
        assert!(
            inverted_range
                .to_string()
                .contains("starts after its ending line")
        );
    }

    #[test]
    fn add_review_thread_response_returns_validated_receipt() {
        let receipt = super::parse_add_pull_request_review_thread_response(
            r#"{"data":{"addPullRequestReviewThread":{"clientMutationId":"append-operation-5","thread":{"id":"PRRT_123"}}}}"#,
            "append-operation-5",
        )
        .unwrap();

        assert_eq!(receipt.operation_id, "append-operation-5");
        assert_eq!(receipt.thread_id, "PRRT_123");
    }

    #[test]
    fn add_review_thread_response_rejects_unacknowledged_or_malformed_envelopes() {
        for raw in [
            r#"{"data":null,"errors":[{"message":"invalid input"}]}"#,
            r#"{}"#,
            r#"{"data":{"addPullRequestReviewThread":null}}"#,
            r#"{"data":{"addPullRequestReviewThread":{"clientMutationId":"append-operation-6","thread":null}}}"#,
            r#"{"data":{"addPullRequestReviewThread":{"clientMutationId":"append-operation-6","thread":{"id":"  "}}}}"#,
            r#"{"data":{"addPullRequestReviewThread":{"clientMutationId":"other-operation","thread":{"id":"PRRT_123"}}}}"#,
        ] {
            assert!(
                super::parse_add_pull_request_review_thread_response(raw, "append-operation-6")
                    .is_err(),
                "expected rejected GraphQL envelope: {raw}"
            );
        }
    }
    #[test]
    fn strict_pending_create_receipt_requires_marked_pending_review() {
        let head = CommitId::new("abcdef0123456789").unwrap();
        let body = super::materialize_pending_review_delivery_body(
            "Human-visible review body",
            "create-operation-1",
            &head,
        )
        .unwrap();
        let raw = serde_json::json!({
            "id": 41,
            "html_url": "https://github.com/acme/widgets/pull/7#pullrequestreview-41",
            "state": "PENDING",
            "body": body,
            "node_id": "PRR_41",
            "commit_id": head.as_str(),
        })
        .to_string();

        let receipt =
            super::parse_created_pending_pull_request_review(&raw, "create-operation-1", &head)
                .unwrap();

        assert_eq!(receipt.id, 41);
        assert_eq!(receipt.state, super::PullRequestReviewState::Pending);
        assert_eq!(receipt.node_id.as_deref(), Some("PRR_41"));
    }

    #[test]
    fn strict_pending_create_receipt_rejects_missing_or_mismatched_acknowledgement() {
        let head = CommitId::new("abcdef0123456789").unwrap();
        let marked_body = super::materialize_pending_review_delivery_body(
            "Human-visible review body",
            "create-operation-2",
            &head,
        )
        .unwrap();

        for raw in [
            serde_json::json!({
                "id": 0,
                "html_url": "https://example.test/review",
                "state": "PENDING",
                "body": marked_body,
                "node_id": "PRR_42",
            }),
            serde_json::json!({
                "id": 42,
                "html_url": " ",
                "state": "PENDING",
                "body": marked_body,
                "node_id": "PRR_42",
            }),
            serde_json::json!({
                "id": 42,
                "html_url": "https://example.test/review",
                "state": "COMMENTED",
                "body": marked_body,
                "node_id": "PRR_42",
            }),
            serde_json::json!({
                "id": 42,
                "html_url": "https://example.test/review",
                "state": "PENDING",
                "body": "Human-visible review body",
                "node_id": "PRR_42",
            }),
            serde_json::json!({
                "id": 42,
                "html_url": "https://example.test/review",
                "state": "PENDING",
                "body": marked_body,
                "node_id": "PRR_42",
                "commit_id": "fedcba9876543210",
            }),
        ] {
            assert!(
                super::parse_created_pending_pull_request_review(
                    &raw.to_string(),
                    "create-operation-2",
                    &head,
                )
                .is_err(),
                "expected rejected create acknowledgement: {raw}"
            );
        }
    }

    #[test]
    fn delivery_marker_parser_rejects_ambiguous_or_malformed_marker() {
        let malformed =
            "<!-- trueflow:delivery:v1 kind=create-pending-review operation=create-operation-3 -->";
        assert!(super::parse_trueflow_delivery_marker(malformed).is_err());

        let head = CommitId::new("abcdef0123456789").unwrap();
        let body = super::materialize_review_thread_delivery_body(
            "Human-visible comment",
            "comment-operation-3",
        )
        .unwrap();
        assert!(matches!(
            super::parse_trueflow_delivery_marker(&body).unwrap(),
            Some(super::GitHubDeliveryMarker::ReviewThread { operation_id })
                if operation_id == "comment-operation-3"
        ));

        let duplicated = format!(
            "{}\n{}",
            super::materialize_pending_review_delivery_body("Body", "create-operation-3", &head,)
                .unwrap(),
            super::materialize_review_thread_delivery_body("Comment", "comment-operation-3",)
                .unwrap(),
        );
        assert!(super::parse_trueflow_delivery_marker(&duplicated).is_err());
    }
    #[test]
    fn delivery_snapshot_parses_review_thread_and_every_comment_field() {
        let reviews_raw = serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "reviews": {
                            "nodes": [{
                                "id": "PRR_17",
                                "fullDatabaseId": 17,
                                "url": "https://github.com/acme/widgets/pull/7#pullrequestreview-17",
                                "body": "owned review",
                                "state": "PENDING",
                                "viewerDidAuthor": true,
                                "commit": {"oid": "abcdef0123456789"}
                            }],
                            "pageInfo": {"hasNextPage": true, "endCursor": "review-cursor"}
                        }
                    }
                }
            }
        })
        .to_string();
        let (reviews, next_review_cursor) =
            super::parse_pull_request_delivery_reviews_page(&reviews_raw).unwrap();
        assert_eq!(next_review_cursor.as_deref(), Some("review-cursor"));
        assert_eq!(reviews[0].node_id, "PRR_17");
        assert_eq!(reviews[0].database_id, Some(17));
        assert_eq!(
            reviews[0].head_sha.as_ref().unwrap().as_str(),
            "abcdef0123456789"
        );
        assert!(reviews[0].viewer_did_author);

        let threads_raw = serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "reviewThreads": {
                            "nodes": [{
                                "id": "PRRT_17",
                                "path": "src/lib.rs",
                                "line": 14,
                                "diffSide": "RIGHT",
                                "startLine": 9,
                                "startDiffSide": "LEFT"
                            }],
                            "pageInfo": {"hasNextPage": false, "endCursor": null}
                        }
                    }
                }
            }
        })
        .to_string();
        let (threads, next_thread_cursor) =
            super::parse_pull_request_delivery_threads_page(&threads_raw).unwrap();
        assert_eq!(next_thread_cursor, None);
        assert_eq!(threads[0].path, "src/lib.rs");
        assert_eq!(threads[0].line, Some(14));
        assert_eq!(threads[0].start_line, Some(9));
        assert_eq!(threads[0].side, Some(super::GitHubCommentSide::Right));
        assert_eq!(threads[0].start_side, Some(super::GitHubCommentSide::Left));

        let comments_raw = serde_json::json!({
            "data": {
                "node": {
                    "comments": {
                        "nodes": [
                            {
                                "id": "PRRC_root",
                                "body": "root body",
                                "state": "PENDING",
                                "viewerDidAuthor": true,
                                "pullRequestReview": {"id": "PRR_17"},
                                "replyTo": null
                            },
                            {
                                "id": "PRRC_reply",
                                "body": "reply body",
                                "state": "SUBMITTED",
                                "viewerDidAuthor": false,
                                "pullRequestReview": {"id": "PRR_18"},
                                "replyTo": {"id": "PRRC_root"}
                            }
                        ],
                        "pageInfo": {"hasNextPage": false, "endCursor": null}
                    }
                }
            }
        })
        .to_string();
        let (comments, next_comment_cursor) =
            super::parse_pull_request_delivery_thread_comments_page(&comments_raw).unwrap();
        assert_eq!(next_comment_cursor, None);
        assert_eq!(comments[0].review_node_id.as_deref(), Some("PRR_17"));
        assert_eq!(comments[0].reply_to_node_id, None);
        assert_eq!(
            comments[1].state,
            super::GitHubPullRequestReviewCommentState::Submitted
        );
        assert_eq!(comments[1].reply_to_node_id.as_deref(), Some("PRRC_root"));
        assert_eq!(
            super::pull_request_delivery_thread_review_node_id("PRRT_17", &comments).unwrap(),
            "PRR_17"
        );
    }

    #[test]
    fn delivery_snapshot_rejects_incomplete_or_ambiguous_pages() {
        let missing_cursor = r#"{
            "data": {
                "repository": {
                    "pullRequest": {
                        "reviews": {
                            "nodes": [],
                            "pageInfo": {"hasNextPage": true, "endCursor": null}
                        }
                    }
                }
            }
        }"#;
        assert!(super::parse_pull_request_delivery_reviews_page(missing_cursor).is_err());

        let multiple_roots = vec![
            super::GitHubPullRequestReviewThreadCommentSnapshot {
                node_id: "PRRC_1".to_string(),
                body: String::new(),
                state: super::GitHubPullRequestReviewCommentState::Pending,
                review_node_id: Some("PRR_1".to_string()),
                reply_to_node_id: None,
                viewer_did_author: true,
            },
            super::GitHubPullRequestReviewThreadCommentSnapshot {
                node_id: "PRRC_2".to_string(),
                body: String::new(),
                state: super::GitHubPullRequestReviewCommentState::Pending,
                review_node_id: Some("PRR_1".to_string()),
                reply_to_node_id: None,
                viewer_did_author: true,
            },
        ];
        assert!(
            super::pull_request_delivery_thread_review_node_id("PRRT_1", &multiple_roots).is_err()
        );
    }
    #[test]
    fn delivery_snapshot_requires_a_valid_pull_request_head() {
        let valid = r#"{
            "data": {
                "repository": {
                    "pullRequest": {
                        "headRefOid": "abcdef0123456789"
                    }
                }
            }
        }"#;
        assert_eq!(
            super::parse_pull_request_delivery_head_response(valid)
                .unwrap()
                .as_str(),
            "abcdef0123456789"
        );

        let invalid = r#"{
            "data": {
                "repository": {
                    "pullRequest": {
                        "headRefOid": "not-a-commit"
                    }
                }
            }
        }"#;
        assert!(super::parse_pull_request_delivery_head_response(invalid).is_err());
    }

    #[test]
    fn gh_api_output_not_found_detection_matches_explicit_http_404() {
        assert!(super::gh_api_output_is_not_found(
            "",
            "gh: Not Found (HTTP 404)\n"
        ));
        assert!(super::gh_api_output_is_not_found("404 Not Found\n", ""));
    }

    #[test]
    fn gh_api_output_not_found_detection_ignores_unrelated_404_text() {
        assert!(!super::gh_api_output_is_not_found(
            "",
            "temporary proxy failure on port 4040\n"
        ));
        assert!(!super::gh_api_output_is_not_found(
            "request id: 404-test\n",
            "gh: authentication required\n"
        ));
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
    fn parse_git_remotes_config_rejects_urls_with_extra_path_segments() {
        let remotes = parse_git_remotes_config(
            "remote.bad-https.url https://github.com/jmqd/trueflow/extra.git\nremote.bad-ssh.url git@github.com:jmqd/trueflow/extra.git\n",
        )
        .unwrap();

        assert!(remotes.is_empty());
    }

    #[test]
    fn parse_git_remotes_config_rejects_scp_like_urls_with_empty_host() {
        let remotes =
            parse_git_remotes_config("remote.origin.url git@:jmqd/trueflow.git\n").unwrap();

        assert!(remotes.is_empty());
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
    fn parse_pull_request_metadata_rejects_truncated_commit_list() {
        let requested = ResolvedPullRequestRef {
            host: "github.com".to_string(),
            owner: "jmqd".to_string(),
            repo: "trueflow".to_string(),
            number: 11,
        };
        let pull_json = r#"{
            "title": "Two commits",
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
                "ref": "feature",
                "sha": "2222222222222222222222222222222222222222",
                "repo": null
            }
        }"#;
        let commits_json = r#"[
            {
                "sha": "2222222222222222222222222222222222222222",
                "commit": { "message": "Only returned commit" }
            }
        ]"#;

        let err = parse_pull_request_metadata(&requested, pull_json, commits_json).unwrap_err();
        assert!(
            err.to_string()
                .contains("expected 2 pull request commits, but GitHub returned 1")
        );
    }

    #[test]
    fn parse_pull_request_metadata_rejects_commit_list_missing_head_sha() {
        let requested = ResolvedPullRequestRef {
            host: "github.com".to_string(),
            owner: "jmqd".to_string(),
            repo: "trueflow".to_string(),
            number: 11,
        };
        let pull_json = r#"{
            "title": "Mismatched commits",
            "commits": 1,
            "base": {
                "ref": "main",
                "sha": "1111111111111111111111111111111111111111",
                "repo": {
                    "name": "trueflow",
                    "owner": { "login": "jmqd" }
                }
            },
            "head": {
                "ref": "feature",
                "sha": "2222222222222222222222222222222222222222",
                "repo": null
            }
        }"#;
        let commits_json = r#"[
            {
                "sha": "3333333333333333333333333333333333333333",
                "commit": { "message": "Wrong commit" }
            }
        ]"#;

        let err = parse_pull_request_metadata(&requested, pull_json, commits_json).unwrap_err();
        assert!(err.to_string().contains(
            "expected final pull request commit to be head 2222222222222222222222222222222222222222"
        ));
    }

    #[test]
    fn parse_pull_request_metadata_rejects_commit_list_where_head_is_not_final() {
        let requested = ResolvedPullRequestRef {
            host: "github.com".to_string(),
            owner: "jmqd".to_string(),
            repo: "trueflow".to_string(),
            number: 11,
        };
        let pull_json = r#"{
            "title": "Wrong order",
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
                "ref": "feature",
                "sha": "3333333333333333333333333333333333333333",
                "repo": null
            }
        }"#;
        let commits_json = r#"[
            {
                "sha": "3333333333333333333333333333333333333333",
                "commit": { "message": "Head commit returned first" }
            },
            {
                "sha": "2222222222222222222222222222222222222222",
                "commit": { "message": "Older commit returned last" }
            }
        ]"#;

        let err = parse_pull_request_metadata(&requested, pull_json, commits_json).unwrap_err();
        assert!(
            err.to_string()
                .contains("expected final pull request commit to be head")
        );
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
        let local_file_path = local_repo.join("src/lib.rs");
        fs::create_dir_all(local_file_path.parent().unwrap()).unwrap();
        fs::write(&local_file_path, "pub fn local_only() -> u32 { 99 }\n").unwrap();
        run_git(&local_repo, &["add", "."]);
        run_git(&local_repo, &["commit", "-m", "Local worktree state"]);
        run_git(&local_repo, &["branch", "-M", "local-review"]);
        let local_head_before = run_git_stdout(&local_repo, &["rev-parse", "HEAD"])
            .trim()
            .to_string();

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
            run_git_stdout(&local_repo, &["branch", "--show-current"]).trim(),
            "local-review"
        );
        assert_eq!(
            run_git_stdout(&local_repo, &["rev-parse", "HEAD"]).trim(),
            local_head_before
        );
        assert_eq!(
            fs::read_to_string(&local_file_path).unwrap(),
            "pub fn local_only() -> u32 { 99 }\n"
        );
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
